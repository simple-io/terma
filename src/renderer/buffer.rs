use crate::app::controller::CellRun;
use crate::renderer::{RunRender, build_row_runs};
use std::cell::RefCell;
use crate::terminal::row::Row;

/// 把 RunRender 列表转换为 Slint 的 CellRun 列表（供 TerminalView 渲染）。
/// 供普通增量渲染与滚动视口渲染两条路径复用，避免重复的映射逻辑。
pub fn runs_to_cell_runs(runs: &[RunRender]) -> Vec<CellRun> {
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

/// 按行渲染缓存：每行维护独立的渲染项列表（`Vec<Vec<RunRender>>`，下标 = 行号），
/// 仅脏行触发 build_row_runs 重建，未变更行直接复用缓存。
pub struct RenderCache {
    /// 每行的渲染项缓存，下标即行号（与 `ScreenBuffer::visible_rows()` 对齐）
    rows: RefCell<Vec<Vec<RunRender>>>,
}

impl RenderCache {
    pub fn new() -> Self {
        Self {
            rows: RefCell::new(Vec::new()),
        }
    }

    /// 清空全部缓存（resize / Tab 切换 / 新建会话时调用，避免残留旧尺寸或旧会话的行）。
    pub fn clear(&self) {
        self.rows.borrow_mut().clear();
    }

    pub fn update(&self, dirty_rows: Vec<usize>, visible: &[Row]) {
        let mut rows = self.rows.borrow_mut();
        // 行数变化（resize）时重建索引结构；正常情况下调用方已 clear，此处兜底保证下标对齐
        if rows.len() != visible.len() {
            rows.clear();
            rows.resize_with(visible.len(), Vec::new);
        }
        for row_idx in dirty_rows {
            if let Some(row) = visible.get(row_idx) {
                rows[row_idx] = build_row_runs(row, row_idx);
            }
        }
    }

    /// 构建所有行的 CellRun 列表（按行号顺序拼接，未缓存行视为空行，不产生渲染项）
    pub fn build_cell_runs(&self) -> Vec<CellRun> {
        let mut cell_all: Vec<CellRun> = Vec::new();
        for runs in self.rows.borrow().iter() {
            cell_all.extend(runs_to_cell_runs(runs));
        }
        cell_all
    }
}
