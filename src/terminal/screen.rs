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
use std::ops::{Deref, DerefMut};

use crate::terminal::cell::{Cell, char_width};

/// 终端行：包含 Cell 列表和软换行标志。
///
/// `wrapped` 标志用于区分「硬换行」（程序显式输出 \n 产生的独立行）
/// 和「软换行」（因列宽不足自动折行的延续行），是 reflow 合并/拆分的依据。
///
/// 通过 `Deref<Target=Vec<Cell>>` / `DerefMut` 保持与现有代码的兼容：
/// `row[col]`、`row.len()`、`row.drain()` 等操作透明委托到内部 `cells`。
#[derive(Debug, Clone)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// 软换行标志：true 表示该行是因列宽不足而自动折行的延续行
    pub wrapped: bool,
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

/// 终端本质就是个 行(rows) × 列(cols) 的二维 Cell 网格，每个字符占用其中一个格子
/// 底层的程序 CLI(bash、zsh) 和 TUL(vim、top) 和 终端屏幕，都会维护这个网格，所有的操作都是按照行和列的寻址，双方网格尺寸和寻址逻辑保持一致
/// 大致的数据链路：
///     1，键盘输入
///     2，前置处理，特殊键处理，ANSI转义等
///     3，写入 PTY writer-> CLI|TUL -> PTY reader  （PTY 类似一个管道）
///     4，读取 PTY 输出，支持 ANSI 解析，识别并执行：co、osc、csi 等控制字符
///     5，更新终端屏幕，渲染 UI 界面
/// 注意，宽字符的处理比较特殊，它会占用两个网格，所有对宽字符的操作都按照双格寻址和操作，比如：
///     1，删除宽字符，bash 发送 osc 序列删除两个网格，终端屏幕也要删除两个网格
///     2，渲染宽字符，UI会占用两个网格，但实际只占一个存储位置，所以渲染时需要特殊处理，业界主流主发是增加占位符
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

    /// 调整尺寸（含 Reflow 内容重排）：
    ///   - 缩小列宽：超出内容自动换行（软换行），宽字符不截断
    ///   - 放大列宽：合并软换行行，保留硬换行
    ///   - scrollback 历史行参与重排
    ///
    /// 参数
    ///  - new_rows: 新行数
    ///  - new_cols: 新列数
    ///  - cursor_row: 光标行
    ///  - cursor_col: 光标列
    /// 返回重排后的光标位置 `(new_row, new_col)`
    pub fn resize(&mut self, new_rows: usize, new_cols: usize, cursor_row: usize, cursor_col: usize) -> (usize, usize) {
        let old_rows = self.rows;
        let old_cols = self.cols;

        // 列宽未变：仅调整行数维度，跳过内容重排
        if new_cols == old_cols {
            return self.resize_rows_only(new_rows, old_rows, cursor_row);
        }

        // 合并 scrollback + grid 为统一逻辑行序列
        let sb_count = self.scrollback.len();
        let mut lines: Vec<Row> = Vec::with_capacity(sb_count + self.grid.len());
        for row in self.scrollback.drain(..) {
            lines.push(row);
        }
        for row in self.grid.drain(..=cursor_row) {
            lines.push(row);
        }

        // 裁剪尾部空行：避免空白行参与 reflow 产生多余的输出行
        // while lines.len() > 1 {
        //     let last = lines.last().unwrap();
        //     let has_content = last.iter().any(|c| {
        //         !c.continuation
        //             && (c.character != ' '
        //                 || c.foreground != crate::terminal::color::Color::Default
        //                 || c.background != crate::terminal::color::Color::Default
        //                 || c.bold
        //                 || c.italic
        //                 || c.underline
        //                 || c.inverse)
        //     });
        //     if !has_content && !last.wrapped {
        //         lines.pop();
        //     } else {
        //         break;
        //     }
        // }

        // 光标在逻辑行序列中的位置（scrollback 行在前，grid 行在后）
        let cursor_log_row = (sb_count + cursor_row).min(lines.len().saturating_sub(1));

        // ── 按列宽变化方向执行 reflow ──
        let (mut new_lines, new_cursor) = if new_cols < old_cols {
            // 列宽变窄：逐行按新列宽拆分，宽字符不截断
            self.reflow_narrowing(&lines, new_cols, cursor_log_row, cursor_col)
        } else {
            // 列宽变宽：合并软换行行，保留硬换行
            self.reflow_widening(&lines, new_cols, cursor_log_row, cursor_col)
        };

        // ── 拆分回 scrollback + grid ──
        // 保留总量上限 = scrollback_limit + new_rows
        // 超出部分从最旧行（前端）丢弃，剩余前部归 scrollback，末尾归 grid
        let total = new_lines.len();
        let max_keep = self.scrollback_limit + new_rows;
        let discard = total.saturating_sub(max_keep);

        // 丢弃最旧的行
        for row in new_lines.drain(..discard) {
            let _ = row;
        }
        // 剩余行中，前部归 scrollback，末尾 new_rows 行归 grid
        let remaining = new_lines.len();
        let target_sb = remaining.saturating_sub(new_rows);
        for row in new_lines.drain(..target_sb) {
            self.push_scrollback(row);
        }
        self.grid = new_lines;
        self.grid.resize(new_rows, new_row(new_cols));

        self.rows = new_rows;
        self.cols = new_cols;
        self.mark_all_rows_dirty();
        new_cursor
    }

    /// 仅行数变化时的简化路径（列宽不变，无需内容重排）
    fn resize_rows_only(&mut self, new_rows: usize, _old_rows: usize, cursor_row: usize, ) -> (usize, usize) {
        // if new_rows > self.grid.len() {
        //     self.grid.resize(new_rows, new_row(self.cols));
        // } else if new_rows < self.grid.len() {
        if new_rows < self.grid.len() {
            // 如果缩小窗口高度，则把顶部行移动到 scrollback
            let drop = self.grid.len() - new_rows;
            if self.scrollback_limit > 0 {
                let drained: Vec<Row> = self.grid.drain(0..drop).collect();
                for row in drained {
                    self.push_scrollback(row);
                }
            } else {
                self.grid.drain(0..drop);
            }
        }
        // 调整屏幕行数与窗口一致
        self.grid.resize(new_rows, new_row(self.cols));
        self.rows = new_rows;
        let cr = cursor_row.min(new_rows.saturating_sub(1));
        self.mark_all_rows_dirty();
        (cr, 0)
    }

    /// 缩小列宽 reflow：逐行按新列宽拆分，宽字符不截断。
    /// 提取内容 cell 时跳过 continuation 续列占位格（重排时宽字符自动重新生成续列）。
    /// 参数
    ///     - lines: 逻辑行序列
    ///     - new_cols: 新列数
    ///     - cursor_log_row: 光标行
    ///     - cursor_col: 光标列
    fn reflow_narrowing(&mut self, lines: &[Row], new_cols: usize, cursor_log_row: usize, cursor_col: usize, ) -> (Vec<Row>, (usize, usize)) {
        let mut new_lines: Vec<Row> = Vec::new();
        let mut new_cursor = (0usize, 0usize);
        let mut out_row: usize = 0;

        for (log_idx, line) in lines.iter().enumerate() {
            let cells: Vec<Cell> = line
                .iter()
                .filter(|c| !c.continuation)
                .copied()
                .collect();

            let line_start_row = out_row;

            if cells.is_empty() {
                let mut row = new_row(new_cols);
                row.wrapped = line.wrapped;
                if log_idx == cursor_log_row {
                    new_cursor = (out_row, 0);
                }
                new_lines.push(row);
                out_row += 1;
                continue;
            }

            // 按 new_cols 分行放置内容 cell（
            let mut chunks: Vec<Vec<Cell>> = Vec::new();
            let mut current: Vec<Cell> = Vec::new();
            let mut current_cols: usize = 0;

            // 按照新列宽，对内容 cell 重新拆分行数，宽字符不截断）
            for cell in &cells {
                let w = char_width(cell.character);
                if current_cols + w > new_cols && !current.is_empty() {
                    chunks.push(current);
                    current = Vec::new();
                    current_cols = 0;
                }
                current.push(*cell);
                current_cols += w;
            }
            if !current.is_empty() {
                chunks.push(current);
            }

            for (ci, chunk) in chunks.iter().enumerate() {
                let mut row = new_row(new_cols);
                let mut col_pos: usize = 0;
                for cell in chunk.iter() {
                    row.cells[col_pos] = *cell;
                    let w = char_width(cell.character);
                    if w == 2 && col_pos + 1 < new_cols {
                        row.cells[col_pos + 1] = Cell::continuation_cell();
                    }
                    col_pos += w;
                }
                // 非末片标记为软换行（后续还有内容），末片继承原行 wrapped 状态
                row.wrapped = if ci + 1 < chunks.len() { true } else { line.wrapped };
                new_lines.push(row);
                out_row += 1;
            }

            // 光标追踪：在 chunk 循环结束后统一计算
            if log_idx == cursor_log_row {
                let total_cells: usize = cells.iter()
                    .map(|c| char_width(c.character).max(1))
                    .sum();
                let col = cursor_col.min(total_cells.saturating_sub(1));
                if new_cols > 0 {
                    new_cursor = (
                        line_start_row + col / new_cols,
                        col % new_cols,
                    );
                }
            }
        }

        let cursor = (new_cursor.0.min(new_lines.len().saturating_sub(1)), new_cursor.1);
        (new_lines, cursor)
    }

    /// 放大列宽 reflow：合并软换行行组，剥离旧列宽填充的尾部空格。
    /// 硬换行（wrapped=false）分隔的行组不被合并。
    /// 参数
    ///     - lines: 逻辑行序列
    ///     - new_cols: 新列数
    ///     - cursor_log_row: 光标行
    ///     - cursor_col: 光标列
    fn reflow_widening(&mut self, lines: &[Row], new_cols: usize, cursor_log_row: usize, cursor_col: usize, ) -> (Vec<Row>, (usize, usize)) {
        let mut new_lines: Vec<Row> = Vec::new();
        let mut new_cursor = (0usize, 0usize);
        let mut out_row: usize = 0;
        let mut i: usize = 0;

        while i < lines.len() {
            // 收集连续 wrapped 行组（第一行自身可以是 hard break，后续行 wrapped=true 表示它是上一行的延续）
            let group_start = i;
            i += 1;
            while i < lines.len() && lines[i - 1].wrapped {
                i += 1;
            }
            let group_end = i; // exclusive

            // 合并组内所有行的内容 cell，剥离每行尾部填充空格
            let mut merged: Vec<Cell> = Vec::new();
            let mut cursor_set = false;
            let mut row_offset: usize = 0; // 组内行在 merged 中的起始偏移

            for j in group_start..group_end {
                let row = &lines[j];
                let cells: Vec<Cell> = row
                    .iter()
                    .filter(|c| !c.continuation)
                    .copied()
                    .collect();
                let content_len = cells
                    .iter()
                    .rposition(|c| c.character != ' ' || c.continuation)
                    .map(|p| p + 1)
                    .unwrap_or(0);

                // 光标追踪：光标在此行上且尚未定位
                if j == cursor_log_row && !cursor_set {
                    let col = if row.wrapped {
                        cursor_col.min(content_len)
                    } else {
                        cursor_col.min(content_len)
                    };
                    new_cursor = (out_row, row_offset + col);
                    cursor_set = true;
                }

                if j < group_end - 1 {
                    // 非末行：剥离尾部空格
                    merged.extend_from_slice(&cells[..content_len]);
                    row_offset += content_len;
                } else {
                    // 末行：保留完整内容（含尾部空格）
                    merged.extend_from_slice(&cells);
                }
            }

            // 按 new_cols 分行放置合并后的内容
            if merged.is_empty() {
                let mut row = new_row(new_cols);
                row.wrapped = lines[group_start].wrapped;
                if !cursor_set && cursor_log_row >= group_start && cursor_log_row < group_end {
                    new_cursor = (out_row, 0);
                }
                new_lines.push(row);
                out_row += 1;
            } else {
                let mut pos: usize = 0;
                let total_chunks = (merged.len() + new_cols - 1) / new_cols.max(1);
                let mut chunk_idx: usize = 0;
                while pos < merged.len() {
                    let end = (pos + new_cols).min(merged.len());
                    let mut row = new_row(new_cols);
                    let mut col_pos: usize = 0;
                    for cell in merged[pos..end].iter() {
                        if col_pos >= new_cols { break; }
                        row.cells[col_pos] = *cell;
                        let w = char_width(cell.character).max(1);
                        if w == 2 && col_pos + 1 < new_cols {
                            row.cells[col_pos + 1] = Cell::continuation_cell();
                        }
                        col_pos += w;
                    }
                    // 非末片标记为软换行，末片继承组首行 wrapped 状态
                    row.wrapped = if chunk_idx + 1 < total_chunks {
                        true
                    } else {
                        lines[group_start].wrapped
                    };
                    chunk_idx += 1;

                    // 光标追踪：光标在 merged 中的绝对偏移落在当前片内
                    if !cursor_set
                        && cursor_log_row >= group_start
                        && cursor_log_row < group_end
                    {
                        let abs_col = new_cursor.1; // widening 中暂存为绝对偏移
                        if abs_col >= pos && abs_col < end {
                            new_cursor = (out_row, abs_col - pos);
                            cursor_set = true;
                        } else if pos >= end.saturating_sub(1) {
                            // 已到最后一片，光标定位到末尾
                            new_cursor = (out_row, (end - pos).saturating_sub(1));
                            cursor_set = true;
                        }
                    }

                    new_lines.push(row);
                    out_row += 1;
                    pos = end;
                }
            }
        }

        // 后处理：如果光标落在 continuation 格上，回退到同字符的首格
        let (cr, mut cc) = new_cursor;
        if cr < new_lines.len() {
            let row = &new_lines[cr];
            if cc < row.cells.len() && row.cells[cc].continuation && cc > 0 {
                cc -= 1;
            }
        }

        (new_lines, (cr, cc))
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
            for cell in self.grid[row].iter_mut() {
                *cell = Cell::default();
            }
            self.mark_row_dirty(row);
        }
    }

    /// 擦除行，删除从 col（光标）到 row（本行）末尾字符
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
    ///
    /// 实现：取出滚动区域 → 丢弃顶部 n 行（滚出）→ 末尾补 n 空行 → 放回原位。
    /// 关键：必须保持 `self.grid.len() == self.rows` 不变，否则网格行数会随滚动无限增长。
    pub fn scroll_up_region(&mut self, top: usize, bottom: usize, n: usize) {
        if bottom < top || self.rows == 0 {
            return;
        }
        // 根据屏幕大小，计算最大的滚动范围
        let region_len = bottom - top + 1;
        // 计算滚动行数，屏幕大小和指定行数取小值，防御设计避免越界，最大滚动行数不能超过屏幕行数
        let n = n.min(region_len);
        if n == 0 {
            return;
        }

        // 取出整个滚动区域，操作后放回，避免索引漂移
        let mut region: Vec<Row> = self.grid.drain(top..=bottom).collect();
        // 从区域顶部滚出 n 行
        let drained: Vec<Row> = region.drain(0..n).collect();
        // 仅整屏滚动（top==0）且有 scrollback 时，滚出的行进入历史记录；
        // TUI（vim/top）、局部滚动区域（top != 0）滚出的行直接丢弃。
        if top == 0 && self.scrollback_limit > 0 {
            for r in drained {
                self.push_scrollback(r);
            }
        }
        // 区域末尾补 n 空行，模拟终端向上滚动后底部出现新空行
        for _ in 0..n {
            region.push(new_row(self.cols));
        }
        // 放回原位，保持 grid 总行数不变
        self.grid.splice(top..top, region);
        // 滚动区域内所有行内容变化，全部标脏
        for r in top..=bottom {
            self.mark_row_dirty(r);
        }
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

    /// 获取并清除脏行索引列表（返回自上次取出后被修改的行号）。
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
            let lead_ok = i > 0 && !row[i - 1].continuation && char_width(row[i - 1].character) == 2;
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
    Row {
        cells: vec![Cell::default(); cols],
        wrapped: false,
    }
}

