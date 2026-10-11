//! render — RenderCells：每个会话独立的渲染计算与缓存单元
//!
//! 职责：把 `ScreenBuffer`（grid + scrollback）转换为 Slint 的 `CellRun` 列表。
//! 每个 `TerminalSession` 持有一个 `RenderCells`，**独立计算、独立缓存**，会话之间
//! 互不干扰。切换 Tab 时无需清空缓存，可直接复用各自按行缓存做增量重建。
//!
//! 两种渲染路径（由滚动偏移 offset 决定）：
//!   - 跟随模式（offset == 0）：仅重建脏行（dirty_rows），未变更行复用按行缓存；
//!   - 滚动模式（offset > 0）：全量拼接 scrollback 历史 + grid 当前屏为视口，不依赖缓存。

use crate::app::controller::CellRun;
use crate::renderer::render_model::{RunRender, build_row_runs, build_viewport_runs};
use crate::terminal::screen::ScreenBuffer;
use std::cell::RefCell;

/// 把 `RunRender` 列表转换为 Slint 的 `CellRun` 列表（供 TerminalView 渲染）。
/// 跟随模式与滚动模式两条路径复用，避免重复的映射逻辑。
fn runs_to_cell_runs(runs: &[RunRender]) -> Vec<CellRun> {
    runs.iter()
        .map(|r| CellRun {
            text: r.text.clone().into(),
            row: r.row,
            col: r.col,
            len: r.len,
            fg: slint::Brush::SolidColor(slint::Color::from_rgb_u8(r.fg.0, r.fg.1, r.fg.2)),
            bg: slint::Brush::SolidColor(slint::Color::from_rgb_u8(r.bg.0, r.bg.1, r.bg.2)),
            has_bg: r.has_bg,
            bold: r.bold,
            italic: r.italic,
            underline: r.underline,
        })
        .collect()
}

/// 会话独立的渲染单元：按行缓存渲染项（`Vec<Vec<RunRender>>`，下标 = 视口行号），
/// 仅脏行触发 `build_row_runs` 重建，未变更行直接复用缓存。
///
/// 每个会话一个实例，缓存与计算彼此隔离；用 `RefCell` 提供内部可变性，
/// 使渲染方法可在持有 `state` 锁的路径中以 `&self` 调用，不引入额外锁。
pub struct RenderCells {
    /// 每行的渲染项缓存，下标即行号（与 `ScreenBuffer::visible_rows()` 对齐）
    rows: RefCell<Vec<Vec<RunRender>>>,
}

impl RenderCells {
    pub fn new() -> Self {
        Self {
            rows: RefCell::new(Vec::new()),
        }
    }

    /// 清空缓存（resize 等尺寸变化时调用，避免复用旧尺寸或旧内容的行）。
    pub fn clear(&self) {
        self.rows.borrow_mut().clear();
    }

    /// 跟随模式渲染：仅重建 `dirty_rows` 指定的脏行，未变更行复用缓存，
    /// 返回按行号顺序拼接的全量 `CellRun` 列表。
    pub fn render_follow(&self, screen: &ScreenBuffer, dirty_rows: Vec<usize>) -> Vec<CellRun> {
        let visible = screen.visible_rows();
        {
            let mut rows = self.rows.borrow_mut();
            // 行数变化（resize）时重建索引结构；正常情况下调用方已 clear，此处兜底保证下标对齐
            if rows.len() != visible.len() {
                rows.clear();
                rows.resize_with(visible.len(), Vec::new);
            }
            for idx in dirty_rows {
                if let Some(row) = visible.get(idx) {
                    rows[idx] = build_row_runs(row, idx);
                }
            }
        }
        self.flatten()
    }

    /// 滚动模式渲染：根据 `offset`（从底部向上滚动的行数）全量拼接 scrollback 历史
    /// 与 grid 当前屏为视口，返回视口 `CellRun` 列表。滚动为临时状态，不写入缓存。
    pub fn render_viewport(&self, screen: &ScreenBuffer, offset: usize) -> Vec<CellRun> {
        runs_to_cell_runs(&build_viewport_runs(screen, offset))
    }

    /// 把按行缓存拼接为扁平的 `CellRun` 列表（按行号顺序，未缓存行视为空行不产生渲染项）。
    fn flatten(&self) -> Vec<CellRun> {
        let mut cells: Vec<CellRun> = Vec::new();
        for runs in self.rows.borrow().iter() {
            cells.extend(runs_to_cell_runs(runs));
        }
        cells
    }
}
