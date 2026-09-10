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

use crate::terminal::buffer::ScreenBuffer;
use crate::terminal::cell::Cell;
use crate::terminal::color::{Color, DEFAULT_BG, DEFAULT_FG};
use crate::terminal::state::{TerminalState};

/// 一个渲染运行：同行连续相同样式的字符段
#[derive(Debug, Clone)]
pub struct RunRender {
    // 文本内容
    pub text: String,
    // 行列用于绝对定位
    pub row: i32,
    // 列用于绝对定位
    pub col: i32,
    // 文本长度（单元格宽度，供 Slint 侧按 len * char-w 计算 Run 渲染宽度）
    //
    // 【技术债 - 宽字符】当前 `len` 按 `text.chars().count()`（字符数）计算，
    // 未按显示宽度区分：CJK 等宽字符占 2 列却计作 1，导致渲染时宽度减半、与后续列重叠。
    // 待 TerminalState 支持宽字符（见 state.rs put_char 注释）后，此处应改为
    // 累加每个字符的显示宽度（`unicode-width`），并跳过续列占位 Cell。
    pub len: i32,
    // 前景色
    pub fg: (u8, u8, u8),
    // 背景色
    pub bg: (u8, u8, u8),
    // 是否显式设置了背景色
    pub has_bg: bool,
    // 样式标志
    // 是否粗体、斜体、下划线
    pub bold: bool,
    // 是否斜体
    pub italic: bool,
    // 是否下划线
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

/// 构建「滚动视图」的 Run 列表：根据 `offset`（从底部向上滚动的行数）
/// 把 scrollback 历史行与 grid 当前可见行拼接，RunRender 的行号为视口行号（0..rows）。
///
/// 拼接规则：视口第 i 行（0-based）的内容来源
///   - i < offset              → scrollback[scrollback.len() - offset + i]（历史行）
///   - i >= offset             → grid[i - offset]（当前屏行）
pub fn build_viewport_runs(screen: &ScreenBuffer, offset: usize) -> Vec<RunRender> {
    let rows = screen.rows;
    let scrollback = &screen.scrollback;
    let grid = &screen.grid;
    // 防御性 clamp：正常情况下调用方已保证 offset <= scrollback.len()
    let offset = offset.min(scrollback.len());

    let mut runs = Vec::new();
    for viewport_row in 0..rows {
        let source = if viewport_row < offset {
            &scrollback[scrollback.len() - offset + viewport_row]
        } else {
            &grid[viewport_row - offset]
        };
        runs.extend(build_row_runs(source, viewport_row));
    }
    runs
}

/// 由单行构建 Run 列表（增量渲染用，仅处理指定行）
pub fn build_row_runs(row: &Vec<Cell>, row_idx: usize) -> Vec<RunRender> {
    let mut runs = Vec::new();
    let mut c = 0;
    while c < row.len() {
        let cell = &row[c];
        if is_blank(cell) {
            c += 1;
            continue;
        }
        let (fg, bg) = cell.resolved_colors();
        let has_bg = cell.background != Color::Default || cell.inverse;
        let start = c;
        let mut text = String::new();
        text.push(cell.character);
        c += 1;
        // 样式指纹
        let style_key = StyleKey::from_cell(cell);
        // 合并后续相同样式（且非完全默认空白）的 Cell
        while c < row.len() {
            let nc = &row[c];
            if is_blank(nc) {
                break;
            }
            if StyleKey::from_cell(nc) != style_key {
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
            bold: style_key.bold,
            italic: style_key.italic,
            underline: style_key.underline,
        });
    }
    runs
}
