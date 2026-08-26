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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveScreen {
    Primary,
    Alternate,
}

/// 终端默认前景/背景色（渲染层也会用到）
pub const DEFAULT_FG: (u8, u8, u8) = (230, 230, 230);
pub const DEFAULT_BG: (u8, u8, u8) = (12, 12, 12);

pub struct TerminalState {
    // 终端尺寸
    pub rows: usize,
    pub cols: usize,
    // 光标位置
    pub cursor: Cursor,
    // 延迟换行标志：光标停在行末，下一个 print 触发换行
    pending_wrap: bool,

    primary: ScreenBuffer,
    alternate: ScreenBuffer,
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
}

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
        }
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
        if self.cursor.col + 1 >= self.cols {
            // 到达行末：设置延迟换行，光标停在最后一列
            self.pending_wrap = true;
        } else {
            self.cursor.col += 1;
        }
    }

    /// 换行：光标下移；若超出滚动区底部则在区域内向上滚动
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
}

impl Perform for TerminalState {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

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

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, c: char) {
        // 把参数扁平化为 u16 序列（同时兼容 ; 与 : 分隔，用于 SGR/定位）
        let p: Vec<u16> = params.iter().flat_map(|g| g.iter().copied()).collect();
        let private = intermediates.contains(&b'?');

        macro_rules! param {
            ($i:expr, $def:expr) => {
                p.get($i).copied().unwrap_or($def)
            };
        }

        match c {
            // 光标移动
            'A' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.row = self.cursor.row.saturating_sub(n);
                self.pending_wrap = false;
            }
            'B' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.row = (self.cursor.row + n).min(self.rows.saturating_sub(1));
                self.pending_wrap = false;
            }
            'C' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.col = (self.cursor.col + n).min(self.cols.saturating_sub(1));
                self.pending_wrap = false;
            }
            'D' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.col = self.cursor.col.saturating_sub(n);
                self.pending_wrap = false;
            }
            'E' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.row = (self.cursor.row + n).min(self.rows.saturating_sub(1));
                self.cursor.col = 0;
                self.pending_wrap = false;
            }
            'F' => {
                let n = param!(0, 1).max(1) as usize;
                self.cursor.row = self.cursor.row.saturating_sub(n);
                self.cursor.col = 0;
                self.pending_wrap = false;
            }
            'G' | '`' => {
                let col1 = param!(0, 1).max(1);
                self.set_cursor_col(col1 as usize - 1);
            }
            'd' => {
                let row1 = param!(0, 1).max(1);
                self.set_cursor_row(row1 as usize - 1);
            }
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

            // 滚动
            'S' => {
                let n = param!(0, 1).max(1) as usize;
                let (top, bottom) = (self.scroll_top, self.scroll_bottom);
                self.screen_mut().scroll_up_region(top, bottom, n);
            }
            'T' => {
                let n = param!(0, 1).max(1) as usize;
                let (top, bottom) = (self.scroll_top, self.scroll_bottom);
                self.screen_mut().scroll_down_region(top, bottom, n);
            }

            // Scroll region（DECSTBM）
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
                }
            }

            // 私有模式
            'h' if private => self.set_private_mode(&p, true),
            'l' if private => self.set_private_mode(&p, false),

            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'M' => self.reverse_index(), // RI
            b'c' => {
                // RIS：完全复位
                *self = TerminalState::new(self.rows, self.cols);
            }
            b'7' => {
                // DECSC 保存光标
                self.saved_cursor = Some(self.cursor);
            }
            b'8' => {
                // DECRC 恢复光标
                if let Some(c) = self.saved_cursor {
                    self.cursor = c;
                    self.clamp_cursor();
                }
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0/2 设置窗口标题等：Phase 3 暂忽略
    }
}

impl TerminalState {
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
                            self.cursor = Cursor::new(0, 0);
                            self.pending_wrap = false;
                        }
                    } else if self.active == ActiveScreen::Alternate {
                        self.active = ActiveScreen::Primary;
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
