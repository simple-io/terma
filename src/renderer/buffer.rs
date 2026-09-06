use crate::app::controller::CellRun;
use crate::renderer::{RunRender, build_row_runs};
use crate::terminal::Row;
use std::cell::RefCell;
use std::collections::HashMap;

/// 按行渲染缓存：每行维护独立的 RunRender 列表，
/// 仅脏行触发 build_row_runs 重建，未变更行直接复用缓存。
pub struct RenderCache {
    /// 每行的 RunRender 缓存（key = 行号）
    rows: RefCell<HashMap<usize, Vec<RunRender>>>,
}

impl RenderCache {
    pub fn new() -> Self {
        Self {
            rows: RefCell::new(HashMap::new()),
        }
    }

    pub fn update(&self, dirty_rows: Vec<usize>, visible: &Vec<Row>) {
        for row_idx in dirty_rows {
            if let Some(row) = visible.get(row_idx) {
                self.rows
                    .borrow_mut()
                    .insert(row_idx, build_row_runs(row, row_idx));
            }
        }
    }

    // 构建所有行的 CellRun 列表
    pub fn build_cell_runs(&self) -> Vec<CellRun> {
        let mut cell_all: Vec<CellRun> = Vec::new();
        let all_runs = self.rows.borrow();
        for i in 0..all_runs.len() {
            let cells: Vec<CellRun> = all_runs
                .get(&i)
                .unwrap()
                .iter()
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
                .collect();
            cell_all.extend(cells)
        }
        cell_all
    }
}
