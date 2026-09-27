//! ScreenBuffer 逻辑屏幕缓冲区，是个二维 Cell 网格 + 滚动历史
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

use crate::terminal::Cursor;
use crate::terminal::cell::{Cell};
use crate::terminal::color::Color;
use crate::terminal::row::Row;
use std::cell::RefCell;

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
    /// DECSC（ESC 7）保存的光标位置，DECRC（ESC 8）恢复到 prompt 起点再整行重绘
    pub saved_cursor: Option<Cursor>,
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
            saved_cursor: self.saved_cursor,
            dirty_rows: RefCell::new(self.dirty_rows.borrow().clone()),
        }
    }
}

impl ScreenBuffer {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            grid: (0..rows).map(|_| Row::new_row(cols)).collect(),
            scrollback: Vec::new(),
            scrollback_limit: rows * 5,
            saved_cursor: None,
            dirty_rows: RefCell::new(Vec::new()),
        }
    }

    /// 创建一个无 scrollback 的缓冲（Alternate Screen 用）
    pub fn new_alternate(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            grid: (0..rows).map(|_| Row::new_row(cols)).collect(),
            scrollback: Vec::new(),
            scrollback_limit: 0,
            saved_cursor: None,
            dirty_rows: RefCell::new(Vec::new()),
        }
    }

    /// 获取只读行数据
    pub fn row(&self, row: usize) -> Option<&Row> {
        if row >= self.grid.len() {
            return None;
        }
        Some(&self.grid[row])
    }

    /// 获取可变引用行数据
    pub fn row_mut(&mut self, row: usize) -> Option<&mut Row> {
        if row >= self.grid.len() {
            return None;
        }
        Some(&mut self.grid[row])
    }

    /// 写入一个 Cell 到指定网格位置，自动处理宽字符占两个格子的情况
    pub fn put_cell(&mut self, row: usize, col: usize, cell: Cell) -> bool {
        if let Some(row_mut) = self.row_mut(row) {
            if row_mut.put_cell(col, cell) {
                self.mark_row_dirty(row);
                return true;
            }
        }
        false
    }

    /// 获取只读 Cell 对象
    pub fn get_cell(&self, row: usize, col: usize) -> Option<&Cell> {
        if let Some(row) = self.row(row) {
            return row.get_cell(col);
        }
        None
    }

    /// 获取可变引用 Cell 对象
    pub fn get_cell_mut(&mut self, row: usize, col: usize) -> Option<&mut Cell> {
        if let Some(row_mut) = self.row_mut(row) {
            return row_mut.get_cell_mut(col);
        }
        None
    }

    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.grid[row][col]
    }

    pub fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        self.mark_row_dirty(row);
        &mut self.grid[row][col]
    }

    /// 调整屏幕尺寸，重排内容并计算光标新位置。
    /// 任一维度变化都走 reflow：按软换行链合并逻辑行 → 按新列宽重新折行。
    pub fn resize(&mut self, new_rows: usize, new_cols: usize, cursor: &Cursor) -> Cursor {
        if self.rows == new_rows && self.cols == new_cols {
            return *cursor;
        }
        self.resize_reflow(new_rows, new_cols, cursor)
    }

    /// 内容重排（缩小与放大列宽、行数变化共用）：
    ///   1. 合并 scrollback + grid 为统一物理行序列；
    ///   2. 按 soft_wrap 链把物理行聚成逻辑行，用「内容索引」追踪活动/暂存光标；
    ///   3. 每个逻辑行按新列宽重新折行，并把内容索引映射回 (行, 列)；
    ///   4. 底部 `new_rows` 行为 grid，其余进 scrollback（超出上限丢弃）。
    pub fn resize_reflow(&mut self, new_rows: usize, new_cols: usize, cursor: &Cursor) -> Cursor {
        let old_rows = self.rows;
        let sb_count = self.scrollback.len();
        let saved = self.saved_cursor;

        // 1. 合并为统一的物理行序列，网格底部的空白填充不参与重排，否则会被当成空行
        let content_bottom = (0..old_rows)
            .rev()
            .find(|&r| !self.grid[r].get_content_cells().is_empty())
            .unwrap_or(0)
            .max(cursor.row)
            .max(saved.map_or(0, |c| c.row));
        let mut physical: Vec<Row> = Vec::with_capacity(sb_count + content_bottom + 1);
        physical.append(&mut self.scrollback);
        drop(self.grid.split_off(content_bottom + 1)); // 尾部空行直接丢弃
        physical.append(&mut self.grid);

        // 2. 光标在物理序列中的行号
        let active_row = sb_count + cursor.row;
        let saved_row = saved.map(|c| sb_count + c.row);

        // 3. 逐逻辑行重排
        let mut out: Vec<Row> = Vec::new();
        let mut new_active: Option<Cursor> = None;
        let mut new_saved: Option<Cursor> = None;
        let mut i = 0;
        while i < physical.len() {
            // 合并逻辑行（soft_wrap）的内容，并定位光标内容索引
            let mut content: Vec<Cell> = Vec::new();
            let mut active_idx = None;
            let mut saved_idx = None;
            loop {
                let row = &physical[i];
                if active_row == i {
                    active_idx = Some(content.len() + cursor_content_index(row, cursor.col));
                }
                if saved_row == Some(i) {
                    let sc = saved.expect("saved cursor present");
                    saved_idx = Some(content.len() + cursor_content_index(row, sc.col));
                }
                content.extend(row.get_content_cells());
                i += 1;
                if !row.is_soft_wrap() || i >= physical.len() {
                    break;
                }
            }

            // 修剪逻辑行末尾的「填充空格」：缩小列宽时，硬换行行末尾的空白空格
            // 只是填充，若不裁剪会被拆成一行只有空格的「多余行」，还会把上一行
            // 内容误标为软换行。带样式（如背景色）的空格是可见内容，不裁剪。
            while content.last().map_or(false, is_padding_space) {
                content.pop();
            }
            active_idx = active_idx.map(|i| i.min(content.len()));
            saved_idx = saved_idx.map(|i| i.min(content.len()));

            // 重排内容，根据新列宽重新折行
            let (rows, pos) = Row::reflow_content(&content, new_cols);
            let base = out.len();
            if let Some(ci) = active_idx {
                let (r, c) = pos[ci];
                new_active = Some(Cursor::new(base + r, c));
            }
            if let Some(ci) = saved_idx {
                let (r, c) = pos[ci];
                new_saved = Some(Cursor::new(base + r, c));
            }
            out.extend(rows);
        }

        // 4. 拆回 scrollback + grid
        let grid_start = out.len().saturating_sub(new_rows);
        let mut sb: Vec<Row> = out.drain(..grid_start).collect();
        if sb.len() > 0 {
            let excess = sb.len().saturating_sub(self.scrollback_limit);
            sb.drain(..excess);
            self.scrollback = sb;
        }
        self.grid = out;
        self.grid.resize(new_rows, Row::new_row(new_cols));

        // 5. 更新尺寸与光标，光标行从物理序列坐标换算回屏幕坐标
        self.rows = new_rows;
        self.cols = new_cols;
        let remap = |c: Cursor| Cursor::new(
            c.row.saturating_sub(grid_start).min(new_rows.saturating_sub(1)),
            c.col.min(new_cols.saturating_sub(1)),
        );
        self.saved_cursor = new_saved.map(remap);
        self.mark_all_rows_dirty();
        new_active.map(remap).unwrap_or_else(|| {
            Cursor::new(
                cursor.row.min(new_rows.saturating_sub(1)),
                cursor.col.min(new_cols.saturating_sub(1)),
            )
        })
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
        let scroll_len = region_len.min(n);
        if scroll_len == 0 {
            return;
        }

        // 取出整个滚动区域，操作后放回，避免索引漂移
        let mut region: Vec<Row> = self.grid.drain(top..=bottom).collect();
        // 从区域顶部滚出 n 行
        let drained: Vec<Row> = region.drain(0..scroll_len).collect();
        // 仅整屏滚动（top==0）且有 scrollback 时，滚出的行进入历史；TUI（vim/top）、局部滚动直接丢弃。
        if top == 0 {
            for r in drained {
                self.push_scrollback(r);
            }
        }
        // 区域末尾补 n 空行，模拟终端向上滚动后底部出现新空行
        for _ in 0..scroll_len {
            region.push(Row::new_row(self.cols));
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
            region.insert(0, Row::new_row(self.cols));
        }
        self.grid.splice(top..top, region);
        // 滚动区域内所有行内容变化
        for r in top..=bottom {
            self.mark_row_dirty(r);
        }
    }

    fn push_scrollback(&mut self, row: Row) {
        if self.scrollback_limit > 0 {
            self.scrollback.push(row);
            if self.scrollback.len() > self.scrollback_limit {
                let excess = self.scrollback.len() - self.scrollback_limit;
                self.scrollback.drain(0..excess);
            }
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
            let lead_ok =
                i > 0 && !row[i - 1].continuation && row[i - 1].is_wide_char();
            if !lead_ok {
                row[i] = Cell::default();
            }
        } else if row[i].is_wide_char() {
            // 宽字符首格：后一个必须是续列占位
            let cont_ok = i + 1 < cols && row[i + 1].continuation;
            if !cont_ok {
                row[i] = Cell::default();
            }
        }
        i += 1;
    }
}

/// 返回光标列 `col` 在行「内容序列」中的索引，与 `cells_content` 的过滤规则一一对应
///   - 光标在宽字符续列占位格上 → 归到其首格（宽字符不可从中拆开）；
///   - 光标在尾部零宽/空白填充 cell 上 → 归到内容末尾。
fn cursor_content_index(line: &Row, col: usize) -> usize {
    let cells = line.get_cells();
    let idx = col.min(cells.len().saturating_sub(1));
    let mut ci = 0usize;
    for (i, cell) in cells.iter().enumerate() {
        if i == idx {
            // 光标在续列占位格 → 归到左侧首格
            return if cell.continuation { ci.saturating_sub(1) } else { ci };
        }
        if cell.is_content() {
            // 可显示内容格
            ci += 1;
        }
    }
    ci
}

/// 判断一个 cell 是否为「可裁剪的尾部填充空格」：默认样式的空格（`' '`）。
/// 带样式（前景/背景色、粗斜下划取反）的空格是可见内容，不参与尾部裁剪。
fn is_padding_space(c: &Cell) -> bool {
    c.character == ' '
        && c.foreground == Color::Default
        && c.background == Color::Default
        && !c.bold
        && !c.italic
        && !c.underline
        && !c.inverse
}
