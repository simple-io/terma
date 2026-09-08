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

use crate::terminal::buffer::ScreenBuffer;
use crate::terminal::cell::{CellStyle, Cell};
use crate::terminal::color::Color;
use crate::terminal::cursor::Cursor;


/// 激活的屏幕类型, 终端有两个屏幕，适用于不同的模式
/// Primary: 支持 CLI 模式，正常屏幕, 逐行输入命令，逐行输出结果，支持窗口滚动等, 例如：bash、ls、git 等
/// Alternate: 支持 TUI 模式，全屏文本界面，可在屏幕任意位置定位光标绘制, 例如：top、htop、vim 等
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveScreen {
    Primary,        // CLI 正常屏幕
    Alternate,      // TUI 全屏文本界面
}

/// 终端仿真状态机类型，维护终端网格、光标位置、屏幕缓冲区等状态信息
pub struct TerminalState {
    // 终端网格行数
    pub rows: usize,
    // 终端网格列数
    pub cols: usize,
    // 光标对象
    pub cursor: Cursor,
    // 延迟换行标志：光标停在行末，下一个 print 触发换行
    pending_wrap: bool,
    // CLI 屏幕缓冲区
    primary: ScreenBuffer,
    // TUI 屏幕缓冲区
    alternate: ScreenBuffer,
    // 当前激活的屏幕
    active: ActiveScreen,
    // 当前样式
    pub current_style: CellStyle,

    // Scroll region（0-based，含两端），默认整屏
    scroll_top: usize,
    scroll_bottom: usize,

    // 1049 切换前的光标保存
    saved_cursor: Option<Cursor>,
    // 终端状态
    pub cursor_visible: bool,
    // Bracketed Paste 模式标志
    pub bracketed_paste: bool,
    // 脏标记用于区分「有变更」与「完全无变更」，解析后待渲染 True，完成渲染后置 False，避免无谓的 UI 重绘。
    dirty: bool,
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
            current_style: CellStyle::default(),
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            saved_cursor: None,
            cursor_visible: true,
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
    /// 使用 `&self`（通过 ScreenBuffer 的 RefCell 内部可变性），
    /// 使渲染层可在持有不可变 state 引用时调用。
    pub fn take_dirty_rows(&self) -> Vec<usize> {
        self.screen().take_dirty_rows()
    }

    /// 调整终端尺寸：同步两块屏 + 光标 clamp
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.primary.resize(rows, cols);
        self.alternate.resize(rows, cols);
        self.rows = rows;
        self.cols = cols;
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
        if self.cursor.row >= rows {
            self.cursor.row = rows.saturating_sub(1);
        }
        if self.cursor.col >= cols {
            self.cursor.col = cols.saturating_sub(1);
        }
        self.pending_wrap = false;
        self.dirty = true;
    }

    // ─── 内部辅助 ───
    fn clamp_cursor(&mut self) {
        if self.cursor.row >= self.rows {
            self.cursor.row = self.rows.saturating_sub(1);
        }
        if self.cursor.col >= self.cols {
            self.cursor.col = self.cols.saturating_sub(1);
        }
    }

    /// 在光标位置写入字符，处理延迟换行与行末回绕
    fn put_char(&mut self, c: char) {
        if self.cols == 0 || self.rows == 0 {
            return;
        }
        if self.pending_wrap {
            self.cursor.col = 0;
            self.linefeed();
            self.pending_wrap = false;
        }
        let cell = self.current_style.apply(c);
        let (row, col) = (self.cursor.row, self.cursor.col);
        self.screen_mut().grid[row][col] = cell;
        self.screen_mut().mark_row_dirty(row);
        if self.cursor.col + 1 >= self.cols {
            // 到达行末：设置延迟换行，光标停在最后一列
            self.pending_wrap = true;
        } else {
            self.cursor.col += 1;
        }
    }

    /// 换行：光标下移，若超出滚动区底部则在区域内向上滚动
    fn linefeed(&mut self) {
        let (top, bottom) = (self.scroll_top, self.scroll_bottom);
        if self.cursor.row == bottom {
            self.screen_mut().scroll_up_region(top, bottom, 1);
        } else if self.cursor.row < self.rows.saturating_sub(1) {
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

    fn set_cursor_row(&mut self, row: usize) {
        self.cursor.row = row.min(self.rows.saturating_sub(1));
        self.pending_wrap = false;
    }

    fn set_cursor_col(&mut self, col: usize) {
        self.cursor.col = col.min(self.cols.saturating_sub(1));
        self.pending_wrap = false;
    }

    /// 1-based 位置定位（0/缺省视为 1）
    fn set_cursor_pos(&mut self, row1: u16, col1: u16) {
        let r = (row1.saturating_sub(1) as usize).min(self.rows.saturating_sub(1));
        let c = (col1.saturating_sub(1) as usize).min(self.cols.saturating_sub(1));
        self.cursor.row = r;
        self.cursor.col = c;
        self.pending_wrap = false;
    }

    // 处理 SGR 参数, 也就是终端字符颜色
    fn handle_sgr(&mut self, p: &[u16]) {
        if p.is_empty() {
            self.current_style = CellStyle::default();
            return;
        }
        let mut i = 0;
        while i < p.len() {
            let v = p[i];
            match v {
                0 => self.current_style = CellStyle::default(),
                1 => self.current_style.bold = true,
                2 => { /* dim/faint 忽略 */ }
                3 => self.current_style.italic = true,
                4 => self.current_style.underline = true,
                7 => self.current_style.inverse = true,
                9 => { /* strikethrough 忽略 */ }
                22 => self.current_style.bold = false,
                23 => self.current_style.italic = false,
                24 => self.current_style.underline = false,
                27 => self.current_style.inverse = false,
                29 => { /* strikethrough off 忽略 */ }
                30..=37 => self.current_style.foreground = Color::Index((v - 30) as u8),
                38 => {
                    if let Some((c, consumed)) = parse_color(&p[i + 1..]) {
                        self.current_style.foreground = c;
                        i += consumed;
                    }
                }
                39 => self.current_style.foreground = Color::Default,
                40..=47 => self.current_style.background = Color::Index((v - 40) as u8),
                48 => {
                    if let Some((c, consumed)) = parse_color(&p[i + 1..]) {
                        self.current_style.background = c;
                        i += consumed;
                    }
                }
                49 => self.current_style.background = Color::Default,
                90..=97 => self.current_style.foreground = Color::Index((v - 90 + 8) as u8),
                100..=107 => self.current_style.background = Color::Index((v - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }

    // 处理私有模式参数
    fn set_private_mode(&mut self, p: &[u16], set: bool) {
        for &v in p {
            match v {
                1049 | 1047 => {
                    // Alternate Screen
                    if set {
                        if self.active == ActiveScreen::Primary {
                            self.saved_cursor = Some(self.cursor);
                            self.active = ActiveScreen::Alternate;
                            self.alternate.clear();
                            self.alternate.mark_all_rows_dirty();
                            self.cursor = Cursor::new(0, 0);
                            self.pending_wrap = false;
                        }
                    } else if self.active == ActiveScreen::Alternate {
                        self.active = ActiveScreen::Primary;
                        self.primary.mark_all_rows_dirty();
                        if let Some(c) = self.saved_cursor.take() {
                            self.cursor = c;
                            self.clamp_cursor();
                        }
                        self.pending_wrap = false;
                    }
                }
                25 => self.cursor_visible = set,
                2004 => self.bracketed_paste = set,
                _ => {}
            }
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
            // 向上移动光标
            'A' => {
                // 向上移动光标
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
            // 向左移动光标
            }
            // 向左移动光标
            'D' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.col = self.cursor.col.saturating_sub(n);
                self.pending_wrap = false;
            // 向下移动光标并置行首
            }
            // 光标下一行
            'E' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.row = (self.cursor.row + n).min(self.rows.saturating_sub(1));
                self.cursor.col = 0;
            // 向上移动光标并置行首
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

            // SGR
            'm' => self.handle_sgr(&p),

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
                let take = n.min(self.cols.saturating_sub(c));
                let avail = self.cols - c;
                if take > 0 && r < self.rows {
                    let row = &mut self.screen_mut().grid[r];
                    row.drain(c..c + take);
                    row.resize(avail, Cell::default());
                    self.screen_mut().mark_row_dirty(r);
                }
            }
            // 插入字符（光标处 n 个空格，右侧推出）
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
                    self.screen_mut().mark_row_dirty(r);
                }
            }
            // 删除字符但保留位置（用空格覆盖）
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
                    self.screen_mut().mark_row_dirty(r);
                }
            }

            // 私有模式
            'h' if private => self.set_private_mode(&p, true),
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
            // DECSC：Save Cursor，保存光标
            b'7' => {
                self.saved_cursor = Some(self.cursor);
            }
            // DECRC：Restore Cursor，恢复光标
            b'8' => {
                if let Some(c) = self.saved_cursor {
                    self.cursor = c;
                    self.clamp_cursor();
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
