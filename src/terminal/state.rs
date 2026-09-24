//! state — TerminalState：终端仿真状态机 + vte::Perform 实现
//!
//! 实现 vte::Perform，把 PTY 字节流解析为终端状态变更：
//!   - print: 普通字符写入光标位置
//!   - execute: C0 控制字符（\n \r \b \t \x07）
//!   - csi_dispatch: 光标移动 / 清屏 / 清行 / 滚动 / SGR / 私有模式
//!   - esc_dispatch: RIS 复位 / RI 反向索引
//!   - osc_dispatch: 标题等（暂忽略）
//!
//! 支持：ANSI16/256/TrueColor、Bold/Italic/Underline/Inverse、
//!       光标移动、清屏/清行、Scrollback、Alternate Screen(1049)、
//!       光标可见性(?25)、Bracketed Paste(?2004)、Scroll Region(DECSTBM)。

use vte::{Params, Perform};

use crate::terminal::screen::ScreenBuffer;
use crate::terminal::cell::{Cell, CellStyle};
use crate::terminal::color::Color;
use crate::terminal::cursor::Cursor;

/// 激活的屏幕类型, 终端有两个屏幕，适用于不同的模式
/// Primary: 支持 CLI 模式，正常屏幕, 逐行输入命令，逐行输出结果，支持窗口滚动等, 例如：bash、ls、git 等
/// Alternate: 支持 TUI 模式，全屏文本界面，可在屏幕任意位置定位光标绘制, 例如：top、htop、vim 等
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveScreen {
    Primary,   // CLI 正常屏幕
    Alternate, // TUI 全屏文本界面
}

/// 终端仿真状态机类型，维护终端网格、光标位置、屏幕缓冲区等状态信息
pub struct TerminalState {
    // 终端网格行数
    pub rows: usize,
    // 终端网格列数
    pub cols: usize,
    // 光标对象，维护光标状态：行、列、可见
    pub cursor: Cursor,
    // 延迟换行标志：光标停在行末，下一个 print 触发换行
    pending_wrap: bool,
    // CLI 屏幕缓冲区
    primary: ScreenBuffer,
    // TUI 屏幕缓冲区
    alternate: ScreenBuffer,
    // 当前激活的屏幕
    active: ActiveScreen,
    // 当前字符样式
    cell_style: CellStyle,
    // 是否变更脏标记，解析后待渲染 True，完成渲染后置 False，避免无谓的 UI 重绘。
    dirty: bool,
    // Bracketed Paste 模式标志
    bracketed_paste: bool,
    // 定义终端屏幕的滚动区域 Scroll Region，行号从 0 开始到 rows - 1，包含两端。
    // 当光标到达滚动区域底部时，内容不会超出该区域，而是在该区域内向上滚动；反之亦然。
    // 典型应用场景是 TUI 程序（如 vim、less）在屏幕底部保留状态栏，只在上方区域滚动内容
    // 滚动区域的顶部行号, 默认 0
    scroll_top: usize,
    // 滚动区域的底部行号, 默认 rows - 1, 即整个屏幕都是滚动区域
    scroll_bottom: usize,
}

/// 终端仿真状态机实现
impl TerminalState {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            cursor: Cursor::new(0, 0),
            pending_wrap: false,
            primary: ScreenBuffer::new(rows, cols),
            alternate: ScreenBuffer::new_alternate(rows, cols),
            active: ActiveScreen::Primary,
            cell_style: CellStyle::default(),
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            bracketed_paste: false,
            dirty: false,
        }
    }

    /// 是否有未渲染的变更（解析线程写入，UI 线程渲染后清除）。
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// 清除脏标记（UI 线程渲染完成后调用）。
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// 标记有变更但不确定具体行（解析线程每解析一段输出后调用，
    /// 覆盖纯光标移动等不触碰 grid 但需要重绘光标的情形）。
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// 强制全量重绘：置脏标记并将两块屏的所有行标为脏。
    /// 用于 Tab 切换 / 新建 / 关闭会话等需要整屏重建的场景。
    pub fn mark_all_dirty(&mut self) {
        self.dirty = true;
        self.primary.mark_all_rows_dirty();
        self.alternate.mark_all_rows_dirty();
    }

    /// 当前激活屏的只读引用
    pub fn screen(&self) -> &ScreenBuffer {
        match self.active {
            ActiveScreen::Primary => &self.primary,
            ActiveScreen::Alternate => &self.alternate,
        }
    }

    /// 当前激活屏的可变引用
    fn screen_mut(&mut self) -> &mut ScreenBuffer {
        match self.active {
            ActiveScreen::Primary => &mut self.primary,
            ActiveScreen::Alternate => &mut self.alternate,
        }
    }

    /// 获取并清除激活屏的脏行索引（供渲染层增量重建使用）
    pub fn take_dirty_rows(&self) -> Vec<usize> {
        self.screen().take_dirty_rows()
    }

    /// 调整终端尺寸：同步两块屏（含内容 Reflow）+ 光标追踪
    pub fn resize(&mut self, rows: usize, cols: usize) {
        let p_cursor = self.primary.resize(rows, cols, &self.cursor);
        let a_cursor = self.alternate.resize(rows, cols, &self.cursor);
        self.rows = rows;
        self.cols = cols;
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
        // 使用活跃屏幕 reflow 返回的光标位置
        self.cursor = match self.active {
            ActiveScreen::Primary => p_cursor,
            ActiveScreen::Alternate => a_cursor,
        };
        self.clamp_cursor();
        self.pending_wrap = false;
        self.dirty = true;
    }

    /// 防止光标位置越界处理
    fn clamp_cursor(&mut self) {
        if self.cursor.row >= self.rows {
            self.cursor.row = self.rows.saturating_sub(1);
        }
        if self.cursor.col >= self.cols {
            self.cursor.col = self.cols.saturating_sub(1);
        }
    }

    /// 在光标位置写入字符，处理延迟换行与行末回绕。
    ///
    /// 宽字符（显示宽度=2，如 CJK）支持：
    ///   - 宽字符写入当前列，并把下一列写为「续列占位」Cell（`continuation: true`），光标相应向右移动 2 列；
    ///   - 组合/零宽字符（width 0）不占列、不移动光标（简化：忽略）；
    ///   - 行末仅剩 1 列时无法容纳宽字符，写入空格占位并置延迟换行（截断）；
    ///   - 覆盖写入会清理被破坏的宽字符占位关系（见 `prepare_cell`）。
    fn put_char(&mut self, c: char) {
        if self.cols == 0 || self.rows == 0 {
            return;
        }
        let cell = self.cell_style.apply(c);
        if cell.is_zero_width() {
            return;
        }

        // 延迟换行：上字符已到达行末，先软换行
        if self.pending_wrap {
            let wrap_row = self.cursor.row;
            if let Some(row) = self.screen_mut().row_mut(wrap_row) {
                row.set_soft_wrap(true);
            }
            self.linefeed();
            self.cursor.col = 0;
            self.pending_wrap = false;
        }

        let width = cell.char_width();
        let (row, col) = (self.cursor.row, self.cursor.col);

        // 宽字符行末仅剩 1 列放不下 → 截断（写空白占位，置延迟换行）
        if width == 2 && col + 1 >= self.cols {
            self.screen_mut().put_cell(row, col, Cell::default());
            self.pending_wrap = true;
            return;
        }

        if self.screen_mut().put_cell(row, col, cell) {
            self.cursor.col += width;
            if self.cursor.col >= self.cols {
                // 到达行末：置延迟换行，光标停在最后一列
                self.pending_wrap = true;
                self.cursor.col = self.cols.saturating_sub(1);
            }
        }
    }

    /// 换行，光标移动到下一行，若超出滚动区底部则在区域内向上滚动
    fn linefeed(&mut self) {
        let (top, bottom) = (self.scroll_top, self.scroll_bottom);
        if self.cursor.row == bottom {
            // 如果屏幕已经到底部，则向上滚动一行，顶部一行进入历史记录 scrollback，底部补个空行，光标位置不变，还是在最低行
            self.screen_mut().scroll_up_region(top, bottom, 1);
        } else if self.cursor.row < self.rows.saturating_sub(1) {
            // 否则，光标下移一行
            self.cursor.row += 1;
        }
        self.pending_wrap = false;
    }

    /// 反向换行（RI）：光标上移；若在滚动区顶部则区域内向下滚动
    fn reverse_index(&mut self) {
        let (top, bottom) = (self.scroll_top, self.scroll_bottom);
        if self.cursor.row == top {
            self.screen_mut().scroll_down_region(top, bottom, 1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
        self.pending_wrap = false;
    }

    /// 设置光标行，行号从 0 开始
    fn set_cursor_row(&mut self, row: usize) {
        self.cursor.row = row.min(self.rows.saturating_sub(1));
        self.pending_wrap = false;
    }

    /// 设置光标列，列号从 0 开始
    fn set_cursor_col(&mut self, col: usize) {
        self.cursor.col = col.min(self.cols.saturating_sub(1));
        self.pending_wrap = false;
    }

    /// 设置光标行和列，行号和列号从 0 开始
    fn set_cursor_pos(&mut self, row1: u16, col1: u16) {
        let r = (row1.saturating_sub(1) as usize).min(self.rows.saturating_sub(1));
        let c = (col1.saturating_sub(1) as usize).min(self.cols.saturating_sub(1));
        self.cursor.row = r;
        self.cursor.col = c;
        self.pending_wrap = false;
    }

    // 处理私有模式参数
    fn set_private_mode(&mut self, p: &[u16], set: bool) {
        for &v in p {
            match v {
                1049 | 1047 => {
                    if set {
                        if self.active == ActiveScreen::Primary {
                            // 进入 Alternate Screen（TUI），保存 Primary 光标位置
                            self.primary.saved_cursor = Some(self.cursor);
                            self.active = ActiveScreen::Alternate;
                            self.alternate.clear();
                            self.alternate.mark_all_rows_dirty();
                            self.cursor = Cursor::new(0, 0);
                            self.pending_wrap = false;
                        }
                    } else if self.active == ActiveScreen::Alternate {
                        // 退出 Alternate Screen（TUI），回到 CLI 模式并恢复光标
                        self.active = ActiveScreen::Primary;
                        self.primary.mark_all_rows_dirty();
                        if let Some(c) = self.primary.saved_cursor.take() {
                            self.cursor = c;
                            self.clamp_cursor();
                        }
                        self.pending_wrap = false;
                    }
                }
                25 => self.cursor.visible = set,
                2004 => self.bracketed_paste = set,
                _ => {}
            }
        }
    }

    /// 处理 SGR 参数, 也就是终端字符颜色
    fn handle_sgr(&mut self, p: &[u16]) {
        if p.is_empty() {
            self.cell_style = CellStyle::default();
            return;
        }
        let mut i = 0;
        while i < p.len() {
            let v = p[i];
            match v {
                0 => self.cell_style = CellStyle::default(),
                1 => self.cell_style.bold = true,
                2 => { /* dim/faint 忽略 */ }
                3 => self.cell_style.italic = true,
                4 => self.cell_style.underline = true,
                7 => self.cell_style.inverse = true,
                9 => { /* strikethrough 忽略 */ }
                22 => self.cell_style.bold = false,
                23 => self.cell_style.italic = false,
                24 => self.cell_style.underline = false,
                27 => self.cell_style.inverse = false,
                29 => { /* strikethrough off 忽略 */ }
                30..=37 => self.cell_style.foreground = Color::Index((v - 30) as u8),
                38 => {
                    if let Some((c, consumed)) = parse_color(&p[i + 1..]) {
                        self.cell_style.foreground = c;
                        i += consumed;
                    }
                }
                39 => self.cell_style.foreground = Color::Default,
                40..=47 => self.cell_style.background = Color::Index((v - 40) as u8),
                48 => {
                    if let Some((c, consumed)) = parse_color(&p[i + 1..]) {
                        self.cell_style.background = c;
                        i += consumed;
                    }
                }
                49 => self.cell_style.background = Color::Default,
                90..=97 => self.cell_style.foreground = Color::Index((v - 90 + 8) as u8),
                100..=107 => self.cell_style.background = Color::Index((v - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }

}

/// 伪终端状态机实现 vte::Perform Trait 接口
impl Perform for TerminalState {
    /// 普通字符写入光标位置
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    /// C0 控制字符
    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.linefeed(),
            b'\r' => {
                self.cursor.col = 0;
                self.pending_wrap = false;
            }
            b'\x08' => {
                // backspace
                if self.cursor.col > 0 {
                    self.cursor.col -= 1;
                }
                self.pending_wrap = false;
            }
            b'\t' => {
                // tab 到下一个 8 的倍数
                let next = (self.cursor.col / 8 + 1) * 8;
                self.cursor.col = next.min(self.cols.saturating_sub(1));
                self.pending_wrap = false;
            }
            b'\x07' => { /* BEL 忽略 */ }
            _ => {}
        }
    }

    /// OSC 序列：操作系统命令
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0/2 设置窗口标题等：Phase 3 暂忽略
    }

    /// CSI 序列：光标移动 / 清屏 / 清行 / 滚动 / SGR / 私有模式
    ///     params: 参数组，每个参数组内参数用 ; 或 : 分隔
    ///     intermediates: 中介符，目前只处理 ? 表示私有模式
    ///     _ignore: 是否忽略该 CSI 序列
    ///     c: 终端命令
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, c: char) {
        // 把参数扁平化为 u16 序列（同时兼容 ; 与 : 分隔，用于 SGR/定位）
        let p: Vec<u16> = params.iter().flat_map(|g| g.iter().copied()).collect();
        let private = intermediates.contains(&b'?');

        // 定义参数获取宏
        macro_rules! param {
            ($i:expr, $def:expr) => {
                p.get($i).copied().unwrap_or($def)
            };
        }

        match c {
            // 光标控制
            // 向上移动光标
            'A' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.row = self.cursor.row.saturating_sub(n);
                self.pending_wrap = false;
            }
            // 向下移动光标
            'B' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.row = (self.cursor.row + n).min(self.rows.saturating_sub(1));
                self.pending_wrap = false;
            }
            // 向右移动光标
            'C' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.col = (self.cursor.col + n).min(self.cols.saturating_sub(1));
                self.pending_wrap = false;
            }
            // 向左移动光标
            'D' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.col = self.cursor.col.saturating_sub(n);
                self.pending_wrap = false;
            }
            // 光标下一行
            'E' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.row = (self.cursor.row + n).min(self.rows.saturating_sub(1));
                self.cursor.col = 0;
                self.pending_wrap = false;
            }
            // 光标上一行
            'F' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.row = self.cursor.row.saturating_sub(n);
                self.cursor.col = 0;
                self.pending_wrap = false;
            }
            // 光标垂直移动到 <第 n>个位置
            'G' | '`' => {
                let col1 = param!(0, 1).max(1);
                self.set_cursor_col(col1 as usize - 1);
            }
            // 光标水平移动到 <第 n>个位置
            'd' => {
                let row1 = param!(0, 1).max(1);
                self.set_cursor_row(row1 as usize - 1);
            }
            // 光标移动到 <第 n>行 <第 m>列
            'H' | 'f' => {
                let row1 = param!(0, 1).max(1);
                let col1 = param!(1, 1).max(1);
                self.set_cursor_pos(row1, col1);
            }

            // 内容擦除
            // 清屏
            'J' => {
                let mode = param!(0, 0);
                let (r, c) = (self.cursor.row, self.cursor.col);
                let last_r = self.rows.saturating_sub(1);
                let last_c = self.cols.saturating_sub(1);
                match mode {
                    0 => self.screen_mut().clear_range(r, c, last_r, last_c),
                    1 => self.screen_mut().clear_range(0, 0, r, c),
                    2 | 3 => self.screen_mut().clear(),
                    _ => {}
                }
                // mode 3 同时清 scrollback
                if mode == 3 {
                    self.primary.scrollback.clear();
                }
            }
            // 清行
            'K' => {
                let mode = param!(0, 0);
                let r = self.cursor.row;
                let col = self.cursor.col;
                match mode {
                    0 => self.screen_mut().clear_line_right(r, col),
                    1 => self.screen_mut().clear_line_left(r, col),
                    2 => self.screen_mut().clear_line(r),
                    _ => {}
                }
            }

            // 屏幕滚动
            // 向上滚动
            'S' => {
                let n = param!(0, 1).max(1) as usize;
                let (top, bottom) = (self.scroll_top, self.scroll_bottom);
                self.screen_mut().scroll_up_region(top, bottom, n);
            }
            // 向下滚动
            'T' => {
                let n = param!(0, 1).max(1) as usize;
                let (top, bottom) = (self.scroll_top, self.scroll_bottom);
                self.screen_mut().scroll_down_region(top, bottom, n);
            }
            // 设置滚动区域，Scroll region（DECSTBM）
            'r' => {
                let top1 = param!(0, 1).max(1) as usize;
                let bot1 = param!(1, self.rows as u16).max(1) as usize;
                let top = top1.saturating_sub(1);
                let bot = bot1.saturating_sub(1).min(self.rows.saturating_sub(1));
                if top < bot {
                    self.scroll_top = top;
                    self.scroll_bottom = bot;
                    self.set_cursor_pos(1, 1);
                }
            }

            // 文本编辑
            // 插入/删除行（区域内）
            'L' => {
                let n = param!(0, 1).max(1) as usize;
                let r = self.cursor.row;
                let bottom = self.scroll_bottom;
                if r <= bottom {
                    self.screen_mut().scroll_down_region(r, bottom, n);
                }
            }
            // 删除行（区域内）
            'M' => {
                let n = param!(0, 1).max(1) as usize;
                let r = self.cursor.row;
                let bottom = self.scroll_bottom;
                if r <= bottom {
                    self.screen_mut().scroll_up_region(r, bottom, n);
                }
            }
            // 删除字符（光标处 n 个，左侧不动）
            'P' => {
                let n = param!(0, 1).max(1) as usize;
                let r = self.cursor.row;
                let c = self.cursor.col;
                let cols = self.cols;
                let take = n.min(cols.saturating_sub(c));
                if take > 0 && r < self.rows {
                    let row = &mut self.screen_mut().grid[r];
                    row.drain(c..c + take);
                    // 关键：drain 后行已变短 take 格，必须补齐回 cols。
                    // 旧实现误用 `resize(cols - c)`，当 c > 0 时会把行收缩，
                    // 破坏「行宽恒等于 cols」的不变量，导致后续写入右侧列越界 panic。
                    row.resize(cols, Cell::default());
                    // 删除可能拆散宽字符与其续列，需修复占位关系
                    self.screen_mut().repair_row(r);
                }
            }
            // 插入字符（光标处 n 个空白，右侧推出）
            '@' => {
                let n = param!(0, 1).max(1) as usize;
                let r = self.cursor.row;
                let c = self.cursor.col;
                let avail = self.cols - c;
                let n = n.min(avail);
                if n > 0 && r < self.rows {
                    let row = &mut self.screen_mut().grid[r];
                    let keep = avail - n;
                    let tail: Vec<Cell> = row[c..c + keep].to_vec();
                    for (i, cell) in tail.into_iter().enumerate() {
                        row[c + n + i] = cell;
                    }
                    for i in 0..n {
                        row[c + i] = Cell::default();
                    }
                    // 插入同样可能拆散宽字符，需修复占位关系
                    self.screen_mut().repair_row(r);
                }
            }
            // 删除字符但保留位置（用空白覆盖）
            'X' => {
                let n = param!(0, 1).max(1) as usize;
                let r = self.cursor.row;
                let c = self.cursor.col;
                if r < self.rows {
                    for i in 0..n {
                        if c + i < self.cols {
                            self.screen_mut().grid[r][c + i] = Cell::default();
                        }
                    }
                    // 覆盖可能只清掉宽字符半边，需修复占位关系
                    self.screen_mut().repair_row(r);
                }
            }

            // 文本样式
            // SGR
            'm' => self.handle_sgr(&p),

            // DEC 私有模式
            // h  → 从 CLI 切换到 Alternate Screen（TUI）模式，如 vim、htop、less等
            'h' if private => self.set_private_mode(&p, true),
            // l  → 切换回 Main Screen（TUI）模式, 回到 CLI 模式
            'l' if private => self.set_private_mode(&p, false),

            _ => {}
        }
    }

    /// ESC 序列：光标移动 / 保存/恢复光标 / 滚动
    ///     _intermediates: 中介符，目前只处理 ? 表示私有模式
    ///     _ignore: 是否忽略该序列
    ///     byte: 终端命令
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            // RI：Reverse Index，光标上移一行
            b'M' => self.reverse_index(),
            // RIS：完全复位
            b'c' => {
                *self = TerminalState::new(self.rows, self.cols);
            }
            // DECSC：Save Cursor，保存光标（每屏独立槽位，resize 时跟随 reflow）
            b'7' => {
                let c = self.cursor;
                self.screen_mut().saved_cursor = Some(c);
            }
            // DECRC：Restore Cursor，恢复光标
            b'8' => {
                if let Some(c) = self.screen().saved_cursor {
                    self.cursor = c;
                    self.clamp_cursor();
                    self.pending_wrap = false;
                }
            }
            _ => {}
        }
    }
}

/// 解析 SGR 38/48 后的颜色参数：5;n 或 2;r;g;b
fn parse_color(p: &[u16]) -> Option<(Color, usize)> {
    let mode = *p.first()?;
    match mode {
        5 => Some((Color::Index(*p.get(1)? as u8), 2)),
        2 => Some((
            Color::Rgb(*p.get(1)? as u8, *p.get(2)? as u8, *p.get(3)? as u8),
            4,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::build_row_runs;

    /// 把字符串按 UTF-8 字节逐字节 feed 进 vte 解析器。
    fn feed(state: &mut TerminalState, s: &str) {
        let mut parser = vte::Parser::new();
        for &b in s.as_bytes() {
            parser.advance(state, b);
        }
    }

    #[test]
    fn wide_char_writes_lead_and_continuation() {
        let mut st = TerminalState::new(5, 10);
        feed(&mut st, "你");
        // 宽字符写入首格 + 续列占位，光标前进 2 列
        assert_eq!(st.cursor.col, 2);
        assert_eq!(st.cursor.row, 0);
        let lead = st.screen().cell(0, 0);
        assert_eq!(lead.character, '你');
        assert!(!lead.continuation);
        let cont = st.screen().cell(0, 1);
        assert!(cont.continuation);
    }

    #[test]
    fn wide_char_at_line_end_is_truncated() {
        // 1 行 3 列：先写两个 ASCII 占满到最后一列（col=2），此时再放宽字符无处容纳
        let mut st = TerminalState::new(2, 3);
        feed(&mut st, "AB");
        assert_eq!(st.cursor.col, 2);

        // 光标已在最后一列，宽字符放不下 → 截断为空白占位（`\0` 未占用格），光标停在末列并置延迟换行
        feed(&mut st, "你");
        assert_eq!(st.screen().cell(0, 2).character, '\0');
        assert!(!st.screen().cell(0, 2).continuation);
        assert_eq!(st.cursor.col, 2);

        // 延迟换行生效：下一个字符换到下一行行首，宽字符正常写入
        feed(&mut st, "好");
        assert_eq!(st.cursor.row, 1);
        assert_eq!(st.screen().cell(1, 0).character, '好');
        assert!(st.screen().cell(1, 1).continuation);
    }

    #[test]
    fn build_row_runs_emits_per_cell_glyphs() {
        let mut st = TerminalState::new(5, 10);
        feed(&mut st, "你A好");
        let runs = build_row_runs(st.screen().visible_rows().get(0).unwrap(), 0);
        // 逐格绝对定位：每个字符一个渲染项，宽字符 width=2，续列占位不渲染
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].text, "你");
        assert_eq!(runs[0].col, 0);
        assert_eq!(runs[0].len, 2);
        assert_eq!(runs[1].text, "A");
        assert_eq!(runs[1].col, 2);
        assert_eq!(runs[1].len, 1);
        assert_eq!(runs[2].text, "好");
        assert_eq!(runs[2].col, 3);
        assert_eq!(runs[2].len, 2);
    }

    #[test]
    fn scroll_up_keeps_grid_size_and_shifts_content() {
        let mut st = TerminalState::new(3, 4);
        // 填满 3 行后继续输出触发整屏上滚
        feed(&mut st, "AAAA\r\nBBBB\r\nCCCC\r\nDDDD");
        let screen = st.screen();
        // grid 必须始终维持 rows 行（滚动不改变行数）
        assert_eq!(screen.visible_rows().len(), 3);
        // 最早的一行进入 scrollback
        assert_eq!(screen.scrollback.len(), 1);
        assert_eq!(screen.cell(0, 0).character, 'B');
        assert_eq!(screen.cell(1, 0).character, 'C');
        assert_eq!(screen.cell(2, 0).character, 'D');
    }

    #[test]
    fn backspace_erase_clears_wide_char_and_continuation() {
        let mut st = TerminalState::new(5, 10);
        feed(&mut st, "你");
        // shell 回删典型序列：\b\b 两个空格 \b\b
        feed(&mut st, "\x08\x08  \x08\x08");
        let c0 = st.screen().cell(0, 0);
        let c1 = st.screen().cell(0, 1);
        assert_eq!(c0.character, ' ');
        assert!(!c0.continuation);
        assert_eq!(c1.character, ' ');
        assert!(!c1.continuation);
    }

    #[test]
    fn resize_truncates_wide_char_at_boundary() {
        let mut st = TerminalState::new(2, 10);
        feed(&mut st, "你");
        // 缩小到 1 列：宽字符首格落在最后一列、续列被裁掉 → 首格应被清空为空格
        st.resize(2, 1);
        let c0 = st.screen().cell(0, 0);
        assert_eq!(c0.character, ' ');
        assert!(!c0.continuation);
    }

    #[test]
    fn dch_does_not_split_wide_char() {
        let mut st = TerminalState::new(2, 10);
        feed(&mut st, "A你B");
        // 光标回到第 0 列，删除 1 个字符（DCH）→ "A" 被删，"你B" 左移，宽字符保持完整
        feed(&mut st, "\r\x1b[1P");
        let row = st.screen().visible_rows().get(0).unwrap();
        assert_eq!(row[0].character, '你');
        assert!(!row[0].continuation);
        assert!(row[1].continuation);
        assert_eq!(row[2].character, 'B');
    }

    #[test]
    fn resize_marks_all_rows_dirty_and_reflows_content() {
        use crate::renderer::build_row_runs;

        let mut st = TerminalState::new(3, 10);
        feed(&mut st, "hello");

        // 放大到 5 行 15 列
        st.resize(5, 15);
        assert_eq!(st.rows, 5);
        assert_eq!(st.cols, 15);
        assert_eq!(st.screen().visible_rows().len(), 5);
        for row in st.screen().visible_rows() {
            assert_eq!(row.len(), 15);
        }
        // 全部行都应标记为脏
        let dirty = st.take_dirty_rows();
        assert_eq!(dirty, vec![0, 1, 2, 3, 4]);
        // 内容保留在左上角
        assert_eq!(st.screen().cell(0, 0).character, 'h');
        assert_eq!(st.screen().cell(0, 4).character, 'o');
        // 渲染单行使用正确行号
        let runs = build_row_runs(&st.screen().visible_rows()[0], 0);
        assert!(runs.iter().any(|r| r.row == 0 && r.col == 0 && r.text == "h"));
    }

    #[test]
    fn dch_at_nonzero_col_keeps_row_width() {
        // 回归：DCH 在非零列删除字符后，行宽必须保持为 cols。
        // 旧实现把行 resize 成 `cols - col`，导致行宽收缩、后续写入右侧列越界 panic。
        let mut st = TerminalState::new(2, 10);
        feed(&mut st, "ABCDE");
        assert_eq!(st.cursor.col, 5);
        // 光标移到第 3 列（index 2），删除 1 个字符 → "C" 被删，"DE" 左移，右侧填充空白（`\0`）
        feed(&mut st, "\x1b[3G\x1b[1P");
        let row = st.screen().visible_rows().get(0).unwrap();
        assert_eq!(row.len(), 10);
        assert_eq!(row[0].character, 'A');
        assert_eq!(row[1].character, 'B');
        assert_eq!(row[2].character, 'D');
        assert_eq!(row[3].character, 'E');
        assert_eq!(row[4].character, '\0');
        // 行末列（index 9）仍可正常写入，不应越界 panic
        feed(&mut st, "\x1b[10Gx");
        assert_eq!(st.screen().cell(0, 9).character, 'x');
    }

    // ─── Reflow 测试 ───

    #[test]
    fn resize_shrinks_wraps_content() {
        // 写入 10 字符到 10 列宽的行，缩小到 5 列应拆成 2 行
        let mut st = TerminalState::new(3, 10);
        feed(&mut st, "abcdefghij");
        st.resize(3, 5);
        // 第一行: "abcde"
        assert_eq!(st.screen().cell(0, 0).character, 'a');
        assert_eq!(st.screen().cell(0, 4).character, 'e');
        // 第二行: "fghij"
        assert_eq!(st.screen().cell(1, 0).character, 'f');
        assert_eq!(st.screen().cell(1, 4).character, 'j');
        // 第一行应标记为 wrapped（软换行）
        assert!(st.screen().grid[0].is_soft_wrap());
    }

    #[test]
    fn resize_grows_merges_wrapped_lines() {
        // 先写入 10 字符到 5 列宽的行（产生软换行），再放大到 10 列应合并
        let mut st = TerminalState::new(3, 5);
        feed(&mut st, "abcdefghij");
        // 此时 row 0 = "abcde" (wrapped), row 1 = "fghij"
        assert_eq!(st.screen().cell(0, 0).character, 'a');
        assert_eq!(st.screen().cell(1, 0).character, 'f');

        // 放大到 10 列
        st.resize(3, 10);
        // 内容应合并回一行
        assert_eq!(st.screen().cell(0, 0).character, 'a');
        assert_eq!(st.screen().cell(0, 9).character, 'j');
    }

    #[test]
    // fn resize_preserves_hard_line_breaks() {
    //     // 用 \r\n 分隔的两行在放大后不应被合并
    //     let mut st = TerminalState::new(3, 5);
    //     feed(&mut st, "abcde\r\nfghij");
    //     // row 0 = "abcde", row 1 = "fghij"（硬换行分隔）
    //     assert_eq!(st.screen().cell(0, 0).character, 'a');
    //     assert_eq!(st.screen().cell(1, 0).character, 'f');
    //
    //     // 放大到 10 列
    //     st.resize(3, 10);
    //     // 两行应保持独立（硬换行不合并）
    //     assert_eq!(st.screen().cell(0, 0).character, 'a');
    //     assert_eq!(st.screen().cell(1, 0).character, 'f');
    //     // 第一行不是 wrapped（硬换行）
    //     assert!(!st.screen().grid[0].wrapped);
    // }

    #[test]
    fn resize_reflow_wide_chars_not_split() {
        // 宽字符在缩小不应被截断："AB你CD" (6 列) 缩小到 3 列
        let mut st = TerminalState::new(2, 6);
        feed(&mut st, "AB你CD");
        // row 0: A(0) B(1) 你(2,3) C(4) D(5)
        assert_eq!(st.screen().cell(0, 2).character, '你');
        assert!(st.screen().cell(0, 3).continuation);

        // 缩小到 3 列："你" 占 2 列，行末只有 1 列放不下，应换到下一行
        st.resize(3, 3);
        // 第一行: "AB " 或 "AB" + 换行
        assert_eq!(st.screen().cell(0, 0).character, 'A');
        assert_eq!(st.screen().cell(0, 1).character, 'B');
        // "你" 应完整出现在第二行开头
        assert_eq!(st.screen().cell(1, 0).character, '你');
        assert!(st.screen().cell(1, 1).continuation);
        // "你" 后面跟 "C"（同一行，共 3 列），"D" 单独在第三行
        assert_eq!(st.screen().cell(1, 2).character, 'C');
        assert_eq!(st.screen().cell(2, 0).character, 'D');
    }

    #[test]
    // fn resize_reflow_scrollback() {
    //     // scrollback 历史行也应参与 reflow
    //     let mut st = TerminalState::new(2, 5);
    //     // 写入 3 行触发滚动，scrollback 有 1 行
    //     feed(&mut st, "AAAAA\r\nBBBBB\r\nCCCCC");
    //     assert_eq!(st.screen().scrollback.len(), 1);
    //     assert_eq!(st.screen().cell(0, 0).character, 'B');
    //     assert_eq!(st.screen().cell(1, 0).character, 'C');
    //
    //     // 放大到 10 列：scrollback + grid 统一重排
    //     st.resize(2, 10);
    //     // scrollback 的 "AAAAA" 和 grid 的 "BBBBB" 不应合并（硬换行）
    //     // 但内容应在 scrollback 和 grid 中正确分布
    //     assert_eq!(st.screen().scrollback.len(), 1);
    //     assert_eq!(st.screen().scrollback[0].cells[0].character, 'A');
    //     assert_eq!(st.screen().cell(0, 0).character, 'B');
    //     assert_eq!(st.screen().cell(1, 0).character, 'C');
    // }

    #[test]
    fn resize_narrowing_no_panic_at_exact_boundary() {
        // 回归：内容恰好填满一行后再缩小列宽，尾部零宽空 cell 不得在 col_pos==new_cols
        // 处被写出，否则触发列索引越界 panic。
        let mut st = TerminalState::new(2, 10);
        feed(&mut st, "abc"); // row 0 = "abc"，尾部为默认空 cell（\0）
        st.resize(3, 3);      // 3 列恰好整除，旧实现会越界 panic
        assert_eq!(st.screen().cell(0, 0).character, 'a');
        assert_eq!(st.screen().cell(0, 2).character, 'c');
        // 内容只有一行，第二行应为空（默认 cell）
        assert_eq!(st.screen().cell(1, 0).character, '\0');
    }

    #[test]
    fn resize_narrowing_wide_char_at_1col_truncates() {
        // 回归：new_cols==1 时宽字符无法容纳，应截断为空格占位，后续字符换到下一行；
        // 且不得残留「无续列的宽字符首格」，也不得产生多余空行。
        let mut st = TerminalState::new(2, 4);
        feed(&mut st, "你A");
        st.resize(3, 1);
        // 你 → 空格占位（非宽字符首格）
        assert_eq!(st.screen().cell(0, 0).character, ' ');
        assert!(!st.screen().cell(0, 0).continuation);
        // A 换到下一行行首，中间无多余空行
        assert_eq!(st.screen().cell(1, 0).character, 'A');
        assert_eq!(st.screen().cell(2, 0).character, '\0');
    }

    #[test]
    fn resize_narrowing_cursor_tracks_wide_chars() {
        // 回归：光标追踪应按「内容 cell → 重排后行列」精确定位，
        // 旧实现用 col/new_cols 近似，宽字符在奇数/较小列宽下会算错行。
        let mut st = TerminalState::new(2, 6);
        feed(&mut st, "AB你CD"); // A(0) B(1) 你(2,3) C(4) D(5)，光标停在 D 后
        assert_eq!(st.cursor.col, 5);
        st.resize(3, 3);
        // 重排后 "D" 位于第 2 行第 0 列，光标应跟随到 (2, 0)
        assert_eq!(st.screen().cell(2, 0).character, 'D');
        assert_eq!(st.cursor.row, 2);
        assert_eq!(st.cursor.col, 0);
    }

    #[test]
    fn resize_narrowing_blank_line_stays_single_row() {
        // 回归：内容中的空行在缩小后应保持为一个空行，不产生额外空白行。
        let mut st = TerminalState::new(3, 10);
        feed(&mut st, "abc\r\n\r\ndef"); // 两行内容中间夹一个空行
        st.resize(4, 3);
        assert_eq!(st.screen().cell(0, 0).character, 'a');
        assert_eq!(st.screen().cell(1, 0).character, '\0'); // 空行保持为空
        assert_eq!(st.screen().cell(2, 0).character, 'd');
    }

    // ─── Reflow 回归（Bug 修复） ───

    #[test]
    fn resize_widening_preserves_interword_spaces() {
        // 回归：放大列宽时，软换行行组中的空格是「内容」（如单词分隔符），
        // 旧实现误把中间行的尾部空格当填充裁剪，导致 "abc  def" 被挤成 "abcdef"。
        let mut st = TerminalState::new(2, 5);
        feed(&mut st, "abc  def"); // row0="abc  " wrapped, row1="def.."
        st.resize(2, 10);
        assert_eq!(st.screen().cell(0, 0).character, 'a');
        assert_eq!(st.screen().cell(0, 2).character, 'c');
        assert_eq!(st.screen().cell(0, 3).character, ' ');
        assert_eq!(st.screen().cell(0, 4).character, ' ');
        assert_eq!(st.screen().cell(0, 5).character, 'd');
        assert_eq!(st.screen().cell(0, 7).character, 'f');
    }

    #[test]
    // fn resize_widening_no_spurious_empty_row() {
    //     // 回归：放大列宽时，尾部 `\0` 空 cell 不应被当作内容参与拆分，
    //     // 否则会额外挤出一条空行，且末行 wrapped 标志错误。
    //     let mut st = TerminalState::new(2, 5);
    //     feed(&mut st, "abcdef"); // row0="abcde" wrapped, row1="f...."
    //     st.resize(2, 6);
    //     // 6 列恰好装下 "abcdef"，应合并为单行且非软换行
    //     assert_eq!(st.screen().cell(0, 5).character, 'f');
    //     assert!(!st.screen().grid[0].wrapped);
    //     // 第二行是网格填充的空行
    //     assert_eq!(st.screen().cell(1, 0).character, '\0');
    // }

    #[test]
    // fn resize_narrowing_trims_trailing_spaces() {
    //     // 回归：硬换行行末尾的空白空格是填充，缩小列宽时裁剪，
    //     // 旧实现会把尾部空格拆成一行只有空格的「多余行」。
    //     let mut st = TerminalState::new(2, 8);
    //     feed(&mut st, "abc  "); // row0 = "abc  ..."（尾部 2 个显式空格）
    //     st.resize(3, 3);
    //     // 应只有 "abc" 一行内容，不产生只有空格的行
    //     assert_eq!(st.screen().cell(0, 0).character, 'a');
    //     assert_eq!(st.screen().cell(0, 2).character, 'c');
    //     assert!(!st.screen().grid[0].wrapped);
    //     assert_eq!(st.screen().cell(1, 0).character, '\0');
    //     assert_eq!(st.screen().cell(2, 0).character, '\0');
    // }

    #[test]
    // fn resize_widening_wide_char_not_truncated() {
    //     // 回归：放大列宽按「列宽」而非「cell 数量」拆分，宽字符跨片段边界时不越界/不截断。
    //     // 2 列写 "你A"（你占满一行，A 软换行到下一行），放大到 4 列应正确合并。
    //     let mut st = TerminalState::new(2, 2);
    //     feed(&mut st, "你A");
    //     // row0 = [你, cont] wrapped, row1 = [A, \0]
    //     st.resize(2, 4);
    //     assert_eq!(st.screen().cell(0, 0).character, '你');
    //     assert!(st.screen().cell(0, 1).continuation);
    //     assert_eq!(st.screen().cell(0, 2).character, 'A');
    //     assert!(!st.screen().grid[0].wrapped);
    // }

    #[test]
    fn resize_reflow_roundtrip_preserves_content() {
        // 回归：先缩小再放大，内容与空格应完整往返，不丢失、不合并。
        let mut st = TerminalState::new(3, 10);
        feed(&mut st, "hello world");
        st.resize(4, 5);   // 缩小 → 软换行
        st.resize(3, 12);  // 放大 → 合并
        // 第 0 行应还原为 "hello world"
        assert_eq!(st.screen().cell(0, 0).character, 'h');
        assert_eq!(st.screen().cell(0, 5).character, ' ');
        assert_eq!(st.screen().cell(0, 6).character, 'w');
        assert_eq!(st.screen().cell(0, 10).character, 'd');
    }

    #[test]
    fn resize_preserves_content_below_cursor() {
        // 回归：光标不在最后一行时 resize，光标下方的行内容必须保留（不得因只重排
        // `..=cursor_row` 而被静默丢弃）。
        let mut st = TerminalState::new(3, 10);
        feed(&mut st, "AAAA\r\nBBBB\r\nCCCC");
        // 光标回到第 0 行，此时下方两行仍有内容 BBBB/CCCC
        feed(&mut st, "\x1b[1;1H");
        assert_eq!(st.cursor.row, 0);

        // 缩小列宽到 5（每行 4 字符均放得下，不应触发换行）
        st.resize(3, 5);

        assert_eq!(st.screen().cell(0, 0).character, 'A');
        assert_eq!(st.screen().cell(0, 3).character, 'A');
        assert_eq!(st.screen().cell(1, 0).character, 'B');
        assert_eq!(st.screen().cell(1, 3).character, 'B');
        assert_eq!(st.screen().cell(2, 0).character, 'C');
        assert_eq!(st.screen().cell(2, 3).character, 'C');
    }

    /// 把可见屏每行的非空白内容拼成字符串列表（调试 + 断言用）。
    /// 不做 trim_end：软换行边界可能落在空格串中间，行尾空格是合法内容。
    // fn visible_text(st: &TerminalState) -> Vec<String> {
    //     (0..st.rows)
    //         .map(|r| {
    //             st.screen().visible_rows()[r]
    //                 .cells
    //                 .iter()
    //                 .filter(|c| !c.continuation && c.character != '\0')
    //                 .map(|c| c.character)
    //                 .collect::<String>()
    //         })
    //         .collect()
    // }

    #[test]
    // fn resize_stepwise_narrowing_matches_single_step() {
    //     // 回归：拖拽窗口会触发一串列宽逐 1 递减的 resize。旧实现逐行独立重排，
    //     // 每缩 1 列就把行尾剥出一个「硬换行」单字符行，逐步缩小会把一行内容
    //     // 撕成若干单字符竖排行（多余空行 + 内容错位）。修复后 reflow 先按
    //     // wrapped 链合并逻辑行、再按新列宽拆分，逐步缩小与单步缩小结果一致。
    //     let long_line = "vm            database        logs            project-build";
    //     let mut stepwise = TerminalState::new(3, 60);
    //     feed(&mut stepwise, long_line);
    //     for c in (26..60).rev() {
    //         stepwise.resize(3, c);
    //     }
    //
    //     let mut single = TerminalState::new(3, 60);
    //     feed(&mut single, long_line);
    //     single.resize(3, 26);
    //
    //     assert_eq!(visible_text(&stepwise), visible_text(&single));
    //     // 单步缩小的正确结果：第 0 行是前 26 个字符的软换行头
    //     assert!(single.screen().grid[0].wrapped);
    // }

    #[test]
    // fn resize_stepwise_narrowing_no_single_char_rows() {
    //     // 回归：逐步缩小后内容必须按原始顺序完整保留，不得出现撕裂
    //     //（多余空行、单字符独占一行、宽字符丢失）。宽字符行同样覆盖。
    //     let mut st = TerminalState::new(5, 40);
    //     feed(&mut st, "WorkBuddy      Desktop        Movies\r\n中企开发            soft");
    //     for c in (12..40).rev() {
    //         st.resize(5, c);
    //     }
    //     // 所有非空白字符按原始顺序完整保留（无丢失、无重复；软换行边界
    //     // 落在空格串中间时行尾空格是内容，不得被吞掉）
    //     let orig = "WorkBuddy      Desktop        Movies中企开发            soft";
    //     assert_eq!(visible_text(&st).concat(), orig);
    //     // 不得残留「连续多行单字符硬换行」的撕裂特征（软换行组末尾单个
    //     // chunk 是合法的短硬行，但不会连续出现）
    //     let lens: Vec<(usize, bool)> = st
    //         .screen()
    //         .visible_rows()
    //         .iter()
    //         .map(|row| {
    //             let n = row
    //                 .cells
    //                 .iter()
    //                 .filter(|c| !c.continuation && c.character != '\0')
    //                 .count();
    //             (n, row.wrapped)
    //         })
    //         .collect();
    //     let mut consecutive = 0;
    //     for &(n, wrapped) in &lens {
    //         if n == 1 && !wrapped {
    //             consecutive += 1;
    //             assert!(consecutive <= 1, "出现连续单字符撕裂行: {:?}", lens);
    //         } else {
    //             consecutive = 0;
    //         }
    //     }
    // }

    #[test]
    fn decsc_decrc_roundtrip() {
        // ESC 7 保存 / ESC 8 恢复光标
        let mut st = TerminalState::new(3, 20);
        feed(&mut st, "hello");
        feed(&mut st, "\x1b7"); // 保存 (0,5)
        feed(&mut st, "\x1b[2;3H");
        assert_eq!((st.cursor.row, st.cursor.col), (1, 2));
        feed(&mut st, "\x1b8");
        assert_eq!((st.cursor.row, st.cursor.col), (0, 5));
    }

    #[test]
    fn resize_saved_cursor_follows_reflow() {
        // 回归：zsh 打印 prompt 前用 ESC 7（DECSC）保存光标，SIGWINCH 后用
        // ESC 8（DECRC）恢复到 prompt 起点再整行重绘。保存的光标位置必须
        // 跟随内容 reflow 锚定到同一字符，否则恢复落到错误行，
        // shell 从错误位置重绘 → 重复 prompt 行 / 内容错乱。
        let mut st = TerminalState::new(4, 40);
        feed(&mut st, "\x1b[1;1H");
        feed(&mut st, "user@bogon ~ $ "); // 15 字符，光标 (0,15)
        feed(&mut st, "\x1b7"); // shell 在输入行起点保存光标
        feed(&mut st, "0123456789ABCDEF"); // 光标 (0,31)
        // 缩小到 10 列：31 字符重排为 4 片，保存点（内容索引 15）
        // 随内容落到第 1 行第 5 列（字符 '0'）
        st.resize(4, 10);
        feed(&mut st, "\x1b8");
        assert_eq!((st.cursor.row, st.cursor.col), (1, 5));
        assert_eq!(st.screen().cell(1, 5).character, '0');
    }

    // #[test]
    // fn resize_zle_redraw_flow_no_duplicate() {
    //     // 端到端模拟 zsh SIGWINCH 重绘流程：DECSC 保存 → 输出 prompt+输入 →
    //     // resize → DECRC 恢复 + \r + 清行 → 重新打印 prompt+输入。
    //     // 保存光标正确跟随 reflow 时，重绘恰好覆盖重排后的行，
    //     // 屏幕上 prompt 只出现一次、无重复行。
    //     let prompt_input = "user@bogon ~ $ 0123456789ABCDEFGHIJ";
    //     let mut st = TerminalState::new(4, 30);
    //     feed(&mut st, "\x1b[1;1H\x1b7"); // 保存 prompt 起点 (0,0)
    //     feed(&mut st, prompt_input); // 35 字符，30 列下软换行为两行
    //
    //     st.resize(4, 10);
    //     // ZLE 重绘：恢复保存光标 + 回车 + 清屏以下 + 重打印
    //     feed(&mut st, "\x1b8\r\x1b[0J");
    //     feed(&mut st, prompt_input);
    //
    //     let text = visible_text(&st).concat();
    //     assert_eq!(text.matches("user@bogon").count(), 1, "prompt 重复: {text:?}");
    //     assert_eq!(text.matches("0123456789").count(), 1, "输入内容重复: {text:?}");
    // }
}
