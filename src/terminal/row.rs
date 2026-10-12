use crate::terminal::{Cell, Color};
use crate::terminal::cell::cells_content;
use std::ops::{Deref, DerefMut};
use crate::app::controller::CellRun;

/// 终端行，表示终端中的一行数据，包含 Cell 字符列表和软换行标志
#[derive(Debug, Clone, Default)]
pub struct Row {
    cells: Vec<Cell>,
    /// 软换行标志：true 表示该行是因列宽不足而自动折行的延续行
    soft_wrap: bool,
}

impl Deref for Row {
    type Target = Vec<Cell>;
    fn deref(&self) -> &Vec<Cell> {
        &self.cells
    }
}

impl DerefMut for Row {
    fn deref_mut(&mut self) -> &mut Vec<Cell> {
        &mut self.cells
    }
}

impl Row {
    /// 创建一个空行，包含指定列数的 Cell
    pub fn new_row(cols: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
            soft_wrap: false,
        }
    }

    /// 创建多行内容，把内容 cell 按 `cols` 列宽拆分成若干行。
    ///   - 宽字符（宽度 2）不截断，放不下时整体换到下一行；
    ///   - 当 `cols` 小于宽字符宽度时，宽字符以空格占位（截断）；
    ///   - 因宽度不足而自动折行的续行，软换行标志为 true。
    pub fn new_from_cells_row(cells: &Vec<Cell>, cols: usize) -> Vec<Self> {
        Self::reflow_content(&cells_content(cells), cols).0
    }

    /// 把「内容 cell」按 `cols` 列宽折行（`content` 须已过滤掉占位/零宽/空白）。
    ///
    /// 返回：
    ///   - 折行后的行列表；
    ///   - 每个内容索引 (0..=len) 落点的 `(行偏移, 列)`，供 reflow 时光标追踪。
    pub fn reflow_content(content: &[Cell], cols: usize) -> (Vec<Self>, Vec<(usize, usize)>) {
        let mut rows: Vec<Self> = Vec::new();
        let mut pos: Vec<(usize, usize)> = Vec::with_capacity(content.len() + 1);
        let mut cur = Self::new_row(cols);
        let mut col = 0usize;

        for &cell in content {
            let w = cell.char_width();
            if w == 0 {
                continue;
            }
            // 宽字符放不下整行时截断为空格占位（宽度计 1）
            let w_eff = w.min(cols).max(1);
            if col + w_eff > cols && col > 0 {
                cur.set_soft_wrap(true);
                rows.push(cur);
                cur = Self::new_row(cols);
                col = 0;
            }
            pos.push((rows.len(), col));
            if w > cols {
                cur.put_cell(col, Cell::new(' '));
                col += 1;
            } else {
                cur.put_cell(col, cell);
                col += w;
            }
        }
        // 内容末尾
        pos.push((rows.len(), col));

        if col > 0 {
            cur.set_soft_wrap(false);
            rows.push(cur);
        } else if rows.is_empty() {
            // 空逻辑行仍保留一行空行
            rows.push(cur);
        }
        (rows, pos)
    }

    /// 设置指定列 Cell，可能会破坏宽字符占位关系，有三种情况需要处理（其实 CLI 工作正常无需处理，更多是防御性策略）
    ///
    ///   1. 目标格是续列占位 → 其左侧宽字符首格失去续列，需一并清除；
    ///   2. 目标格是宽字符首格 → 其右侧续列失去主格，需一并清除；
    ///   3. 写入宽字符且右侧相邻格是宽字符首格 → 右侧首格的续列将成为孤儿，需清除。
    pub fn put_cell(&mut self, col: usize, cell: Cell) -> bool {
        let len = self.cells.len();
        if col >= len || cell.is_zero_width() {
            return false;
        }
        // 1. 目标格是续列占位 → 其左侧宽字符首格失去续列，需一并清除
        if self.cells[col].continuation && col > 0 {
            self.cells[col - 1] = Cell::default();
        }
        // 2. 目标格是宽字符首格 → 其右侧续列失去主格，需一并清除
        if self.cells[col].is_wide_char() && col + 1 < len {
            self.cells[col + 1] = Cell::default();
        }
        // 3. 写入宽字符且右侧相邻格是宽字符首格 → 其续列（col+2）将成为孤儿，需清除
        if cell.is_wide_char() && col + 2 < len && self.cells[col + 1].is_wide_char() {
            self.cells[col + 2] = Cell::default();
        }

        if cell.is_wide_char() {
            // 宽字符占 2 列：末列放不下时截断为空格占位，避免越界
            if col + 1 >= len {
                self.cells[col] = Cell::new(' ');
            } else {
                self.cells[col] = cell;
                self.cells[col + 1] = Cell::continuation_cell();
            }
        } else {
            self.cells[col] = cell;
        }
        true
    }

    // 把内容行转换为 CellRun 列表
    pub fn to_cell_run(&self, row_idx: usize) -> Vec<CellRun> {
        let mut cell_runs: Vec<CellRun> = Vec::new();
        for (col, cell) in self.cells.iter().enumerate() {
            // 非内容字符，跳过
            if !cell.is_content() {
                continue;
            }
            let (fg, bg) = cell.resolved_colors_brush();
            cell_runs.push(CellRun {
                text: cell.character.to_string().into(),
                row: row_idx as i32,
                col: col as i32,
                len: cell.char_width() as i32,
                fg,
                bg,
                has_bg: cell.background != Color::Default || cell.inverse,
                bold: cell.bold,
                italic: cell.italic,
                underline: cell.underline,
            });
        }
        cell_runs
    }

    /// 获取指定列的 Cell
    pub fn get_cell(&self, col: usize) -> Option<&Cell> {
        self.cells.get(col)
    }

    /// 获取指定列的 Cell（可变引用）
    pub fn get_cell_mut(&mut self, col: usize) -> Option<&mut Cell> {
        self.cells.get_mut(col)
    }

    /// 获取所有 Cell
    pub fn get_cells(&self) -> &Vec<Cell> {
        &self.cells
    }

    /// 获取所有 Cell（内容），也就是排除零宽字符和占位符
    pub fn get_content_cells(&self) -> Vec<Cell> {
        cells_content(&self.cells)
    }

    /// 获取 Cell 列表长度
    pub fn get_cells_len(&self) -> usize {
        self.cells.len()
    }

    /// 获取 Cell 列表内容长度
    pub fn get_content_len(&self) -> usize {
        cells_content(&self.cells)
            .iter()
            .map(|c| c.char_width())
            .sum()
    }

    /// 设置软换行标志
    pub fn set_soft_wrap(&mut self, soft_wrap: bool) {
        self.soft_wrap = soft_wrap;
    }

    /// 获取软换行标志
    pub fn is_soft_wrap(&self) -> bool {
        self.soft_wrap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_from_cells_row() {
        // 根据列宽拆分行，会有余量
        let cells = vec![
            Cell::new('a'),
            Cell::new('b'),
            Cell::new('c'),
            Cell::new('d'),
        ];
        let rows = Row::new_from_cells_row(&cells, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get_content_cells(), vec![cells[0], cells[1]]);
        assert_eq!(rows[0].is_soft_wrap(), true);
        assert_eq!(rows[1].get_content_cells(), vec![cells[2], cells[3]]);
        assert_eq!(rows[1].is_soft_wrap(), false);
    }

    #[test]
    fn new_from_cells_row_width() {
        // 根据列宽拆分行，会有余量
        let cells = vec![
            Cell::new('a'),
            Cell::new('b'),
            Cell::new('你'),
            Cell::new('d'),
            Cell::new('e'),
            Cell::new('f'),
        ];
        let rows = Row::new_from_cells_row(&cells, 3);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].is_soft_wrap(), true);
        assert_eq!(rows[1].is_soft_wrap(), true);
        assert_eq!(rows[2].is_soft_wrap(), false);

        assert_eq!(rows[0].cells[0], Cell::new('a'));
        assert_eq!(rows[0].cells[1], Cell::new('b'));
        assert_eq!(rows[0].cells[2], Cell::new('\x00'));
        assert_eq!(rows[1].cells[1], Cell::continuation_cell());
        assert_eq!(rows[1].cells[1].is_continuation(), true);
    }

    #[test]
    fn new_from_cells_row_margin() {
        // 根据列宽拆分行，会有余量
        let cells = vec![
            Cell::new('a'),
            Cell::new('b'),
            Cell::new('c'),
            Cell::new('d'),
            Cell::new('e'),
            Cell::new('f'),
            Cell::new('g'),
            Cell::new('h'),
            Cell::new('i'),
            Cell::new('j'),
        ];
        let rows = Row::new_from_cells_row(&cells, 4);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0].get_content_cells(),
            vec![cells[0], cells[1], cells[2], cells[3]]
        );
        assert_eq!(
            rows[1].get_content_cells(),
            vec![cells[4], cells[5], cells[6], cells[7]]
        );
    }
}
