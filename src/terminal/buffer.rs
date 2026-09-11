//! buffer — ScreenBuffer：二维 Cell 网格 + 滚动历史
//!
//! 设计：
//!   - Primary Screen 拥有 scrollback（行滚出顶部时存入历史）
//!   - Alternate Screen 不带 scrollback（vim/top 等全屏程序退出后恢复 Primary）
//!   - 滚动区域（scroll region）默认为整屏，CSI r 可设置（本阶段实现整屏滚动）
//!
//! 脏行追踪（Dirty Row Tracking）：
//!   - 使用 RefCell<Vec<usize>> 记录被修改的行索引，支持通过 &self 调用 take_dirty_rows
//!   - 所有修改 grid 的方法自动标记对应行为脏
//!   - 渲染层据此仅重建脏行的 Run 列表，未变更行复用缓存

use std::cell::RefCell;

use crate::terminal::cell::{Cell, char_width};

pub type Row = Vec<Cell>;

/// 一块屏幕缓冲：rows × cols 的 Cell 网格
#[derive(Debug)]
pub struct ScreenBuffer {
    // 行
    pub rows: usize,
    // 列
    pub cols: usize,
    /// 数据缓冲区，是个二维数组，以行和列的模式，包含所有数据
    pub grid: Vec<Row>,
    /// 滚动历史（仅 Primary 使用）
    pub scrollback: Vec<Row>,
    /// scrollback 最大行数
    pub scrollback_limit: usize,
    /// 脏行索引：记录自上次 take 后被修改的行号（RefCell 支持通过 &self 取出行）
    dirty_rows: RefCell<Vec<usize>>,
}

impl Clone for ScreenBuffer {
    fn clone(&self) -> Self {
        Self {
            rows: self.rows,
            cols: self.cols,
            grid: self.grid.clone(),
            scrollback: self.scrollback.clone(),
            scrollback_limit: self.scrollback_limit,
            dirty_rows: RefCell::new(self.dirty_rows.borrow().clone()),
        }
    }
}

impl ScreenBuffer {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            grid: new_grid(rows, cols),
            scrollback: Vec::new(),
            scrollback_limit: rows * 5,
            dirty_rows: RefCell::new(Vec::new()),
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
            dirty_rows: RefCell::new(Vec::new()),
        }
    }

    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.grid[row][col]
    }

    pub fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        self.mark_row_dirty(row);
        &mut self.grid[row][col]
    }

    /// 调整尺寸：保留左上对齐内容，新增 cell 用默认填充。
    pub fn resize(&mut self, rows: usize, cols: usize) {
        // 调整每行列数
        for row in &mut self.grid {
            if cols > row.len() {
                row.resize(cols, Cell::default());
            } else if cols < row.len() {
                row.truncate(cols);
                // 裁剪可能把宽字符首格与其续列截断，修复边界占位关系
                fix_wide_invariants(row);
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
        // resize 后所有行内容变化，标记全部为脏
        self.mark_all_rows_dirty();
    }

    /// 整屏清空为默认 Cell
    pub fn clear(&mut self) {
        for row in self.grid.iter_mut() {
            for cell in row.iter_mut() {
                *cell = Cell::default();
            }
        }
        // clear 修改了所有行
        self.mark_all_rows_dirty();
    }

    /// 清除从 (start_row,start_col) 到 (end_row,end_col) 含端点的区域
    pub fn clear_range(
        &mut self,
        start_row: usize,
        start_col: usize,
        end_row: usize,
        end_col: usize,
    ) {
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
            self.mark_row_dirty(r);
        }
    }

    /// 清除整行
    pub fn clear_line(&mut self, row: usize) {
        if row < self.rows {
            for cell in &mut self.grid[row] {
                *cell = Cell::default();
            }
            self.mark_row_dirty(row);
        }
    }

    /// 清除行内从 col 到行尾
    pub fn clear_line_right(&mut self, row: usize, col: usize) {
        if row < self.rows {
            for c in col..self.cols {
                self.grid[row][c] = Cell::default();
            }
            // 清除可能只清掉宽字符的半边，修复占位关系
            fix_wide_invariants(&mut self.grid[row]);
            self.mark_row_dirty(row);
        }
    }

    /// 清除行内从行首到 col
    pub fn clear_line_left(&mut self, row: usize, col: usize) {
        if row < self.rows {
            for c in 0..=col.min(self.cols.saturating_sub(1)) {
                self.grid[row][c] = Cell::default();
            }
            // 清除可能只清掉宽字符的半边，修复占位关系
            fix_wide_invariants(&mut self.grid[row]);
            self.mark_row_dirty(row);
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

    /// 滚动屏幕，在 [top, bottom] 区域内向上滚动 n 行。
    /// 仅当 top==0 且有 scrollback 时，滚出的行存入历史；否则丢弃。
    pub fn scroll_up_region(&mut self, top: usize, bottom: usize, n: usize) {
        if bottom < top || self.rows == 0 {
            return;
        }
        // 根据屏幕大小，计算最大的滚动范围
        let region_len = bottom - top + 1;
        // 计算滚动行数，屏幕大小和指定行数行数取小值，防御设计避免越界，最大滚动行数不能超过屏幕行数
        let n = n.min(region_len);
        if n == 0 {
            return;
        }

        // 从屏幕区域顶部弹出滚出的行数，此时屏幕区域的行数会缩短
        let drained: Vec<Row> = self.grid.drain(top..top + n).collect();
        // 屏幕向上滚动后，在末尾补充对应对应数量的空行，这模拟了终端向上滚动后底部出现新空行的行为
        for i in 0..(bottom + 1 - n) {
            self.grid.push(new_row(self.cols));
        }
        // 把从屏幕区域顶部弹出的行，放入历史记录 scrollback
        // 注意：而所有 TUI（vim、top）、局部滚动区域（top != 0，如 CSI 设置的滚动区域）等都丢弃，不进入历史记录
        if top == 0 && self.scrollback_limit > 0 {
            for r in drained {
                self.push_scrollback(r);
            }
        }
        // 由于滚动区域内所有行内容变化，所以把每一行都标记为脏
        for r in top..=bottom {
            self.mark_row_dirty(r);
        }

        // // 只有当滚动区域从第 0 行开始（即整屏滚动，而非局部滚动区域）且 scrollback_limit > 0 时，才将滚出的行存入历史。
        // // 而所有 TUI（vim、top）、局部滚动区域（top != 0，如 CSI 设置的滚动区域）等都丢弃，不进入历史记录
        // if top == 0 && self.scrollback_limit > 0 {
        //     for r in drained {
        //         self.push_scrollback(r);
        //     }
        // }
        //
        // // 取出整个屏幕区域，操作后放回，避免索引漂移
        // let mut region: Vec<Row> = self.grid.drain(top..=bottom).collect();
        // // 从 region 取出需要滚出的行数，此时 region 中剩下的就是需要上移的行
        // let drained: Vec<Row> = region.drain(0..n).collect();
        // // 只有当滚动区域从第 0 行开始（即整屏滚动，而非局部滚动区域）且 scrollback_limit > 0 时，才将滚出的行存入历史。
        // // 而所有 TUI（vim、top）、局部滚动区域（top != 0，如 CSI 设置的滚动区域）等都丢弃，不进入历史记录
        // if top == 0 && self.scrollback_limit > 0 {
        //     for r in drained {
        //         self.push_scrollback(r);
        //     }
        // }
        // // 在 region 末尾补充 n 个全新的空行，这模拟了终端向上滚动后底部出现新空行的行为
        // for _ in 0..n {
        //     region.push(new_row(self.cols));
        // }
        // // 把处理好的内容放回 grid 中
        // self.grid.splice(top..top, region);
        // // 由于滚动区域内所有行内容变化，所以把每一行都标记为脏
        // for r in top..=bottom {
        //     self.mark_row_dirty(r);
        // }
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
        // 滚动区域内所有行内容变化
        for r in top..=bottom {
            self.mark_row_dirty(r);
        }
    }

    fn push_scrollback(&mut self, row: Row) {
        self.scrollback.push(row);
        if self.scrollback.len() > self.scrollback_limit {
            let excess = self.scrollback.len() - self.scrollback_limit;
            self.scrollback.drain(0..excess);
        }
    }

    /// 取可见行（grid），用于渲染
    pub fn visible_rows(&self) -> &Vec<Row> {
        &self.grid
    }

    // ─── 脏行追踪（通过 &self 操作 RefCell，支持在不可变借用上下文中调用） ───

    /// 获取并清除脏行索引列表（返回自上次取出后被修改的行号）。
    /// 使用 `&self`（而非 `&mut self`），使渲染层可在持有不可变 state 引用时调用。
    pub fn take_dirty_rows(&self) -> Vec<usize> {
        std::mem::take(&mut *self.dirty_rows.borrow_mut())
    }

    /// 标记所有行为脏（用于强制全量重绘：resize、屏幕切换等）
    pub fn mark_all_rows_dirty(&self) {
        let mut rows = self.dirty_rows.borrow_mut();
        rows.clear();
        for i in 0..self.rows {
            rows.push(i);
        }
    }

    /// 标记指定行为脏（用于直接修改 grid 的场景）
    pub fn mark_row_dirty(&self, row: usize) {
        let mut rows = self.dirty_rows.borrow_mut();
        if !rows.contains(&row) {
            rows.push(row);
        }
    }

    /// 修复指定行的宽字符占位关系并标记为脏。
    /// 用于 ICH/DCH/ECH 等可能拆散宽字符与其续列占位的操作之后。
    pub fn repair_row(&mut self, row: usize) {
        if row < self.rows {
            fix_wide_invariants(&mut self.grid[row]);
            self.mark_row_dirty(row);
        }
    }
}

/// 修复一行内的宽字符占位关系，保证不变量：
///   - 续列占位格（continuation）左侧必须是宽字符首格，否则清空该占位格；
///   - 宽字符首格右侧必须有续列占位格（且未越界），否则把该首格清空为空格。
///
/// 该函数幂等，可在 resize 裁剪、ICH/DCH/ECH、部分清行等操作后安全调用。
fn fix_wide_invariants(row: &mut Row) {
    let cols = row.len();
    let mut i = 0;
    while i < cols {
        if row[i].continuation {
            // 续列占位：前一个必须是宽字符首格
            let lead_ok = i > 0
                && !row[i - 1].continuation
                && char_width(row[i - 1].character) == 2;
            if !lead_ok {
                row[i] = Cell::default();
            }
        } else if char_width(row[i].character) == 2 {
            // 宽字符首格：后一个必须是续列占位
            let cont_ok = i + 1 < cols && row[i + 1].continuation;
            if !cont_ok {
                row[i] = Cell::default();
            }
        }
        i += 1;
    }
}

fn new_grid(rows: usize, cols: usize) -> Vec<Row> {
    (0..rows).map(|_| new_row(cols)).collect()
}

fn new_row(cols: usize) -> Row {
    vec![Cell::default(); cols]
}
