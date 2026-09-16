//! render_model — 把 TerminalState 的 ScreenBuffer 转为渲染用的 Cell 渲染项列表
//!
//! 策略：**逐格（逐字符）绝对定位**，每个非空白、非续列占位的 Cell 生成一个渲染项。
//!   - 完全默认的空白 Cell 不生成渲染项（让终端背景透出）
//!   - 带非默认背景的空格仍生成渲染项（绘制背景色块）
//!   - 宽字符（width=2）的续列占位格被跳过，不单独渲染
//!   - 每个渲染项记录行列（网格绝对定位）、单字符文本、显示宽度（1/2）、fg/bg RGB、样式标志
//!
//! 逐格定位保证每个字符的中心/左缘都精确落在 char-w 的整数倍网格上，不依赖字体
//! advance 宽度是否恰好等于 char-w，从而消除「单行越长、误差累积越大」的对齐漂移。
//!
//! 增量渲染支持：
//!   - `build_row_runs` 仅处理单行，供控制器按行缓存、仅重建脏行
//!   - `build_runs` 遍历所有行（全量重建，用于回退或初始化）

use crate::terminal::screen::ScreenBuffer;
use crate::terminal::cell::{Cell, char_width};
use crate::terminal::color::Color;
use crate::terminal::state::{TerminalState};

/// 一个渲染项：单个字符单元格（非空白、非续列占位）
#[derive(Debug, Clone)]
pub struct RunRender {
    // 文本内容（单个字符）
    pub text: String,
    // 行列用于绝对定位
    pub row: i32,
    // 列用于绝对定位
    pub col: i32,
    // 该字符占用的显示宽度（1 或 2，CJK 等宽字符为 2），供 Slint 侧按 len * char-w 计算格子宽度
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

/// 一个 Cell 是否完全默认（空格 + 默认前景背景 + 无样式）：不生成渲染项
#[inline]
fn is_blank(c: &Cell) -> bool {
    c.is_blank()
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

/// 由单行构建渲染项列表（增量渲染用，仅处理指定行）。
///
/// 逐格绝对定位：每个非空白、非续列占位的 Cell 生成一个渲染项，
/// `col` 为该格的网格列号，`len` 为该字符显示宽度（1/2）。
pub fn build_row_runs(row: &Vec<Cell>, row_idx: usize) -> Vec<RunRender> {
    let mut runs = Vec::new();
    for (col, cell) in row.iter().enumerate() {
        // 续列占位：本身不渲染，宽字符首格已占用 2 列
        if cell.continuation {
            continue;
        }
        // 完全默认空白：透出终端背景，不生成渲染项
        if is_blank(cell) {
            continue;
        }
        let width = char_width(cell.character);
        // 零宽/组合字符不占列，跳过
        if width == 0 {
            continue;
        }
        let (fg, bg) = cell.resolved_colors();
        let has_bg = cell.background != Color::Default || cell.inverse;
        runs.push(RunRender {
            text: cell.character.to_string(),
            row: row_idx as i32,
            col: col as i32,
            len: width as i32,
            fg,
            bg,
            has_bg,
            bold: cell.bold,
            italic: cell.italic,
            underline: cell.underline,
        });
    }
    runs
}
