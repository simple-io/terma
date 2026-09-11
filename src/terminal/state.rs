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
use crate::terminal::cell::{Cell, CellStyle, char_width};
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
    pub current_style: CellStyle,

    /**
    定义终端屏幕的滚动区域 Scroll Region，行号从 0 开始，包含两端。
    当光标到达滚动区域底部时，内容不会超出该区域，而是在该区域内向上滚动；反之亦然。
    典型应用场景是 TUI 程序（如 vim、less）在屏幕底部保留状态栏，只在上方区域滚动内容
    */
    // 滚动区域的顶部行号, 默认 0
    scroll_top: usize,
    // 滚动区域的底部行号, 默认 rows - 1, 即整个屏幕都是滚动区域
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
        let width = char_width(c);
        if width == 0 {
            // 组合/零宽字符：简化处理，直接忽略（不占列、不移动光标）
            return;
        }
        if self.pending_wrap {
            self.cursor.col = 0;
            self.linefeed();
            self.pending_wrap = false;
        }
        let (row, col) = (self.cursor.row, self.cursor.col);
        // 防御性校验：以「实际 grid 尺寸」为准，而非仅依赖 self.rows/self.cols 字段。
        // 一旦某些操作或 resize 边界处理导致字段与 grid 尺寸不一致，此处兜底返回，
        // 避免出现「len 是 X 却访问索引 Y」的索引越界 panic。
        let screen = self.screen();
        if row >= screen.grid.len() {
            return;
        }
        if col >= screen.grid[row].len() {
            return;
        }
        // 覆盖写入前，先清理该位置周围可能被破坏的宽字符占位关系
        self.prepare_cell(row, col, width);

        if width == 1 {
            // width == 1：普通字符
            let cell = self.current_style.apply(c);
            self.screen_mut().grid[row][col] = cell;
            self.screen_mut().mark_row_dirty(row);
            if col + 1 >= self.cols {
                // 到达行末：设置延迟换行，光标停在最后一列
                self.pending_wrap = true;
            } else {
                self.cursor.col += 1;
            }
        } else if width == 2 {
            // width == 2：宽字符
            if col + 1 >= self.cols {
                // 行末仅剩 1 列，放不下宽字符：填充空格占位并置延迟换行（宽字符被截断，不写入）
                self.screen_mut().grid[row][col] = Cell::default();
                self.screen_mut().mark_row_dirty(row);
                self.pending_wrap = true;
                return;
            }
            let cell = self.current_style.apply(c);
            // 写入宽字符
            self.screen_mut().grid[row][col] = cell;
            // 续列占位：本身不渲染，仅占住第二列
            self.screen_mut().grid[row][col + 1] = Cell::continuation_cell();
            self.screen_mut().mark_row_dirty(row);
            if col + 2 >= self.cols {
                self.pending_wrap = true;
                self.cursor.col = self.cols - 1;
            } else {
                self.cursor.col += 2;
            }
        }
    }

    /// 在写入 (row, col) 前，清理可能被本次写入破坏的宽字符占位关系：
    ///   1. 目标格是续列占位 → 其左侧宽字符首格失去续列，需一并清除；
    ///   2. 目标格是宽字符首格 → 其右侧续列失去主格，需一并清除；
    ///   3. 写入宽字符且右侧相邻格是宽字符首格 → 右侧首格的续列将成为孤儿，需清除。
    fn prepare_cell(&mut self, row: usize, col: usize, width: usize) {
        let screen = self.screen_mut();
        let g = &mut screen.grid[row];
        let cols = g.len();

        if col < cols {
            // 插入模式，清理目标格左侧的宽字符占位关系
            if g[col].continuation && col > 0 {
                g[col - 1] = Cell::default();
            }
            if !g[col].continuation && char_width(g[col].character) == 2 && col + 1 < cols {
                g[col + 1] = Cell::default();
            }
        }
        if width == 2
            && col + 1 < cols
            && !g[col + 1].continuation
            && char_width(g[col + 1].character) == 2
            && col + 2 < cols
        {
            g[col + 2] = Cell::default();
        }
    }

    /// 换行：光标下移，若超出滚动区底部则在区域内向上滚动
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
                    // 插入同样可能拆散宽字符，需修复占位关系
                    self.screen_mut().repair_row(r);
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
                    // 覆盖可能只清掉宽字符半边，需修复占位关系
                    self.screen_mut().repair_row(r);
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

        // 光标已在最后一列，宽字符放不下 → 截断为空格，光标停在末列并置延迟换行
        feed(&mut st, "你");
        assert_eq!(st.screen().cell(0, 2).character, ' ');
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
    fn dch_at_nonzero_col_keeps_row_width() {
        // 回归：DCH 在非零列删除字符后，行宽必须保持为 cols。
        // 旧实现把行 resize 成 `cols - col`，导致行宽收缩、后续写入右侧列越界 panic。
        let mut st = TerminalState::new(2, 10);
        feed(&mut st, "ABCDE");
        assert_eq!(st.cursor.col, 5);
        // 光标移到第 3 列（index 2），删除 1 个字符 → "C" 被删，"DE" 左移，右侧补空格
        feed(&mut st, "\x1b[3G\x1b[1P");
        let row = st.screen().visible_rows().get(0).unwrap();
        assert_eq!(row.len(), 10);
        assert_eq!(row[0].character, 'A');
        assert_eq!(row[1].character, 'B');
        assert_eq!(row[2].character, 'D');
        assert_eq!(row[3].character, 'E');
        assert_eq!(row[4].character, ' ');
        // 行末列（index 9）仍可正常写入，不应越界 panic
        feed(&mut st, "\x1b[10Gx");
        assert_eq!(st.screen().cell(0, 9).character, 'x');
    }
}
