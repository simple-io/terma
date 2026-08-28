//! render_model — 把 TerminalState 的 ScreenBuffer 转为渲染用的 Run 列表
//!
//! 策略：把同行连续相同样式（前景/背景/粗体/斜体/下划线/反相）的 Cell 合并为一个 Run。
//!   - 完全默认的空白 Cell 不生成 Run（让终端背景透出）
//!   - 带非默认背景的空格仍生成 Run（绘制背景色块）
//!   - 每个 Run 记录起始行列、文本、解析后的 fg/bg RGB、样式标志
//!
//! 渲染层用 Run 的 col/row 做绝对定位（等宽字体），保证列对齐。
//!
//! 增量渲染支持：
//!   - `build_row_runs` 仅处理单行，供控制器按行缓存、仅重建脏行
//!   - `build_runs` 遍历所有行（全量重建，用于回退或初始化）

use crate::terminal::cell::Cell;
use crate::terminal::color::Color;
use crate::terminal::state::{TerminalState, DEFAULT_BG, DEFAULT_FG};

/// 一个渲染运行：同行连续相同样式的字符段
#[derive(Debug, Clone)]
pub struct RunRender {
    pub text: String,
    pub row: i32,
    pub col: i32,
    pub len: i32,
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub has_bg: bool,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// 样式指纹：用于判定相邻 Cell 能否合并为同一 Run
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StyleKey {
    fg: Color,
    bg: Color,
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl StyleKey {
    fn from_cell(c: &Cell) -> Self {
        Self {
            fg: c.foreground,
            bg: c.background,
            bold: c.bold,
            italic: c.italic,
            underline: c.underline,
            inverse: c.inverse,
        }
    }
}

/// 一个 Cell 是否完全默认（空格 + 默认前景背景 + 无样式）：不生成 Run
fn is_blank(c: &Cell) -> bool {
    c.character == ' '
        && c.foreground == Color::Default
        && c.background == Color::Default
        && !c.bold
        && !c.italic
        && !c.underline
        && !c.inverse
}

/// 由 TerminalState 构建当前可见屏的 Run 列表（全量遍历所有行）
pub fn build_runs(state: &TerminalState) -> Vec<RunRender> {
    let screen = state.screen();
    let rows = screen.visible_rows();
    let mut runs = Vec::new();

    for (r, row) in rows.iter().enumerate() {
        runs.extend(build_row_runs(row, r));
    }
    runs
}

/// 由单行构建 Run 列表（增量渲染用，仅处理指定行）
pub fn build_row_runs(row: &[crate::terminal::cell::Cell], row_idx: usize) -> Vec<RunRender> {
    let mut runs = Vec::new();
    let mut c = 0;
    while c < row.len() {
        let cell = &row[c];
        if is_blank(cell) {
            c += 1;
            continue;
        }
        let key = StyleKey::from_cell(cell);
        let (fg, bg) = cell.resolved_colors(DEFAULT_FG, DEFAULT_BG);
        let has_bg = cell.background != Color::Default || cell.inverse;
        let start = c;
        let mut text = String::new();
        text.push(cell.character);
        c += 1;
        // 合并后续相同样式（且非完全默认空白）的 Cell
        while c < row.len() {
            let nc = &row[c];
            if is_blank(nc) {
                break;
            }
            if StyleKey::from_cell(nc) != key {
                break;
            }
            text.push(nc.character);
            c += 1;
        }
        let len = text.chars().count() as i32;
        runs.push(RunRender {
            text,
            row: row_idx as i32,
            col: start as i32,
            len,
            fg,
            bg,
            has_bg,
            bold: key.bold,
            italic: key.italic,
            underline: key.underline,
        });
    }
    runs
}
