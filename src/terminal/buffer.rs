//! buffer — ScreenBuffer：二维 Cell 网格 + 滚动历史
//!
//! 设计：
//!   - Primary Screen 拥有 scrollback（行滚出顶部时存入历史）
//!   - Alternate Screen 不带 scrollback（vim/top 等全屏程序退出后恢复 Primary）
//!   - 滚动区域（scroll region）默认为整屏，CSI r 可设置（本阶段实现整屏滚动）

use crate::terminal::cell::Cell;

pub type Row = Vec<Cell>;

/// 一块屏幕缓冲：rows × cols 的 Cell 网格
#[derive(Debug, Clone)]
pub struct ScreenBuffer {
    pub rows: usize,
    pub cols: usize,
    /// 行数据，grid[0] 为顶行
    pub grid: Vec<Row>,
    /// 滚动历史（仅 Primary 使用）
    pub scrollback: Vec<Row>,
    /// scrollback 最大行数
    pub scrollback_limit: usize,
}

impl ScreenBuffer {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            grid: new_grid(rows, cols),
            scrollback: Vec::new(),
            scrollback_limit: 10_000,
        }
    }

    /// 创建一个无 scrollback 的缓冲（Alternate Screen 用）
    pub fn new_alternate(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            grid: new_grid(rows, cols),
            scrollback: Vec::new(),
            scrollback_limit: 0,
        }
    }

    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.grid[row][col]
    }

    pub fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        &mut self.grid[row][col]
    }

    /// 调整尺寸：保留左上对齐内容，新增 cell 用默认填充。
    pub fn resize(&mut self, rows: usize, cols: usize) {
        // 调整每行列数
        for row in &mut self.grid {
            if cols > row.len() {
                row.resize(cols, Cell::default());
            } else {
                row.truncate(cols);
            }
        }
        self.cols = cols;
        // 调整行数
        if rows > self.grid.len() {
            self.grid.resize(rows, new_row(cols));
        } else if rows < self.grid.len() {
            // 行数变少：把多出的顶部行压入 scrollback（仅 Primary）
            let drop = self.grid.len() - rows;
            if self.scrollback_limit > 0 {
                let drained: Vec<Row> = self.grid.drain(0..drop).collect();
                for r in drained {
                    self.push_scrollback(r);
                }
            } else {
                self.grid.drain(0..drop);
            }
            self.grid.resize(rows, new_row(cols));
        }
        self.rows = rows;
    }

    /// 整屏清空为默认 Cell
    pub fn clear(&mut self) {
        for row in &mut self.grid {
            for cell in row.iter_mut() {
                *cell = Cell::default();
            }
        }
    }

    /// 清除从 (start_row,start_col) 到 (end_row,end_col) 含端点的区域
    pub fn clear_range(&mut self, start_row: usize, start_col: usize, end_row: usize, end_col: usize) {
        for r in start_row..=end_row.min(self.rows.saturating_sub(1)) {
            let c_start = if r == start_row { start_col } else { 0 };
            let c_end = if r == end_row {
                end_col.min(self.cols.saturating_sub(1))
            } else {
                self.cols.saturating_sub(1)
            };
            for c in c_start..=c_end {
                self.grid[r][c] = Cell::default();
            }
        }
    }

    /// 清除整行
    pub fn clear_line(&mut self, row: usize) {
        if row < self.rows {
            for cell in &mut self.grid[row] {
                *cell = Cell::default();
            }
        }
    }

    /// 清除行内从 col 到行尾
    pub fn clear_line_right(&mut self, row: usize, col: usize) {
        if row < self.rows {
            for c in col..self.cols {
                self.grid[row][c] = Cell::default();
            }
        }
    }

    /// 清除行内从行首到 col
    pub fn clear_line_left(&mut self, row: usize, col: usize) {
        if row < self.rows {
            for c in 0..=col.min(self.cols.saturating_sub(1)) {
                self.grid[row][c] = Cell::default();
            }
        }
    }

    /// 向上滚动 n 行：顶部 n 行进入 scrollback，底部补 n 空行
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll_up_region(0, self.rows.saturating_sub(1), n);
    }

    /// 向下滚动 n 行：底部 n 行丢弃，顶部补 n 空行
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_down_region(0, self.rows.saturating_sub(1), n);
    }

    /// 在 [top, bottom] 区域内向上滚动 n 行。
    /// 仅当 top==0 且有 scrollback 时，滚出的行存入历史；否则丢弃。
    pub fn scroll_up_region(&mut self, top: usize, bottom: usize, n: usize) {
        if bottom < top || self.rows == 0 {
            return;
        }
        let region_len = bottom - top + 1;
        let n = n.min(region_len);
        if n == 0 {
            return;
        }
        // 取出整个区域，操作后放回，避免索引漂移
        let mut region: Vec<Row> = self.grid.drain(top..=bottom).collect();
        let drained: Vec<Row> = region.drain(0..n).collect();
        if top == 0 && self.scrollback_limit > 0 {
            for r in drained {
                self.push_scrollback(r);
            }
        }
        for _ in 0..n {
            region.push(new_row(self.cols));
        }
        self.grid.splice(top..top, region);
    }

    /// 区域向下滚动
    pub fn scroll_down_region(&mut self, top: usize, bottom: usize, n: usize) {
        if bottom < top || self.rows == 0 {
            return;
        }
        let region_len = bottom - top + 1;
        let n = n.min(region_len);
        if n == 0 {
            return;
        }
        let mut region: Vec<Row> = self.grid.drain(top..=bottom).collect();
        region.truncate(region_len - n); // 丢弃末尾 n 行
        for _ in 0..n {
            region.insert(0, new_row(self.cols));
        }
        self.grid.splice(top..top, region);
    }

    fn push_scrollback(&mut self, row: Row) {
        self.scrollback.push(row);
        if self.scrollback.len() > self.scrollback_limit {
            let excess = self.scrollback.len() - self.scrollback_limit;
            self.scrollback.drain(0..excess);
        }
    }

    /// 取可见行（grid），用于渲染
    pub fn visible_rows(&self) -> &[Row] {
        &self.grid
    }
}

fn new_grid(rows: usize, cols: usize) -> Vec<Row> {
    (0..rows).map(|_| new_row(cols)).collect()
}

fn new_row(cols: usize) -> Row {
    vec![Cell::default(); cols]
}
