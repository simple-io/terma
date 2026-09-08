use crate::app::controller::CellRun;
use crate::renderer::{RunRender, build_row_runs};
use crate::terminal::Row;
use std::cell::RefCell;
use std::collections::HashMap;

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

    /// 清空全部缓存（resize / Tab 切换 / 新建会话时调用，避免残留旧尺寸或旧会话的行）。
    pub fn clear(&self) {
        self.rows.borrow_mut().clear();
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
