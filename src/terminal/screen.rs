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
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// 软换行标志：true 表示该行是因列宽不足而自动折行的延续行
    pub wrapped: bool,
}

impl Row {
    pub fn new_row(cols: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
            wrapped: false,
        }
    }
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
    /// DECSC（ESC 7）保存的光标位置，DECRC（ESC 8）恢复到 prompt 起点再整行重绘
    pub saved_cursor: Option<(usize, usize)>,
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
            grid: new_grid(rows, cols),
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
            grid: new_grid(rows, cols),
            scrollback: Vec::new(),
            scrollback_limit: 0,
            saved_cursor: None,
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

        // 合并 scrollback + grid 为统一逻辑行序列（必须抽取整屏所有行参与重排，
        // 否则光标下方的行会被静默丢弃，导致 resize 时「丢失字符」）。
        let sb_count = self.scrollback.len();
        let mut lines: Vec<Row> = Vec::with_capacity(sb_count + self.grid.len());
        for row in self.scrollback.drain(..) {
            lines.push(row);
        }
        for row in self.grid.drain(..) {
            lines.push(row);
        }

        // 裁剪尾部空填充行：grid 底部总有 rows 高的空填充，且纯空/纯空格行参与
        // reflow 只会产生多余的空白输出行、把真实内容误推入 scrollback。软换行行
        // （wrapped=true）是上一行的延续，不属于「尾部空行」，不裁剪。
        while lines.len() > 1 {
            let last = lines.last().unwrap();
            if row_is_effectively_empty(last) && !last.wrapped {
                lines.pop();
            } else {
                break;
            }
        }

        // ── 锚点：活动光标 + DECSC 保存的光标，二者都必须跟随内容重排 ──
        // zsh 等 line editor 打印 prompt 前发 ESC 7 保存光标，SIGWINCH 后发
        // ESC 8 恢复到 prompt 起点再整行重绘；若保存位置不跟随 reflow，
        // 恢复会落到错误行 → 重复 prompt 行 / 内容错乱。
        let mut anchors: Vec<(usize, usize)> = Vec::with_capacity(2);
        let cursor_log_row = (sb_count + cursor_row).min(lines.len().saturating_sub(1));
        anchors.push((cursor_log_row, cursor_col));
        if let Some((sr, sc)) = self.saved_cursor {
            let sr = sr.min(self.rows.saturating_sub(1));
            anchors.push(((sb_count + sr).min(lines.len().saturating_sub(1)), sc));
        }

        // ── 内容重排（缩小/放大共用：按软换行链合并逻辑行后重新拆分） ──
        let (mut new_lines, new_anchors) = self.reflow(&lines, new_cols, &anchors);
        let mut new_cursor = new_anchors[0];

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

        // 光标（含保存光标）从「逻辑行坐标」换算到 grid 坐标：
        // 逻辑行号 − 转入 scrollback 的行数，并夹紧到新网格范围。
        let clamp_row =
            |r: usize| r.saturating_sub(target_sb).min(new_rows.saturating_sub(1));
        new_cursor = (
            clamp_row(new_cursor.0),
            new_cursor.1.min(new_cols.saturating_sub(1)),
        );
        if new_anchors.len() > 1 {
            let (ar, ac) = new_anchors[1];
            self.saved_cursor = Some((clamp_row(ar), ac.min(new_cols.saturating_sub(1))));
        }

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

    /// 内容重排（缩小与放大列宽共用）：按软换行链分组 → 合并逻辑行 → 按新列宽重新拆分。
    ///
    /// 关键不变量：`wrapped=true` 的行是上一行的软换行延续，必须先合并回逻辑行
    /// 再重新拆分。若像旧实现那样逐行独立重排，拖拽缩放（列宽逐 1 递减的一串
    /// resize）会把行尾逐步「剥」出硬换行单字符行，最终把一行内容撕成
    /// 若干单字符竖排行（多余空行 + 内容错位/丢失）。
    ///
    /// 提取内容 cell 时跳过两类格子：续列占位（`continuation`，重排时自动重建）、
    /// 零宽空 cell（`\0`，填充）。显式空格是内容：
    ///   - 软换行组：尾部空格保留（如单词分隔符，放大后不得被挤压合并）；
    ///   - 硬换行组：尾部空白空格视为填充，裁剪（避免拆出只有空格的多余行）。
    /// 宽字符不截断；仅在整行都放不下（`new_cols == 1`）时按终端惯例截断为空白占位。
    ///
    /// 参数
    ///     - lines: 逻辑行序列
    ///     - new_cols: 新列数
    ///     - anchors: 需要跟随内容重排的光标锚点列表 `[(逻辑行号, 列)]`，
    ///       含活动光标与 DECSC 保存的光标；返回值与其一一对应。
    fn reflow(
        &mut self,
        lines: &[Row],
        new_cols: usize,
        anchors: &[(usize, usize)],
    ) -> (Vec<Row>, Vec<(usize, usize)>) {
        let mut new_lines: Vec<Row> = Vec::new();
        let mut new_anchors = vec![(0usize, 0usize); anchors.len()];
        let mut anchor_set = vec![false; anchors.len()];
        let mut out_row: usize = 0;
        let mut i: usize = 0;

        while i < lines.len() {
            // 收集连续 wrapped 行组：首行可以是硬换行，后续行 wrapped=true 表示
            // 它是上一行的延续。
            let group_start = i;
            i += 1;
            while i < lines.len() && lines[i - 1].wrapped {
                i += 1;
            }
            let group_end = i; // exclusive

            // 组的软换行状态由「组内最后一行」决定（它决定是否延续到下一组），
            // 而非首行（首行的 wrapped 仅表示它软换行到组内下一行）。
            let group_wrapped = lines[group_end - 1].wrapped;

            // 合并组内所有行的内容 cell：跳过续列占位与 `\0`，保留空格。
            let mut merged: Vec<Cell> = Vec::new();
            let mut anchor_abs: Vec<Option<usize>> = vec![None; anchors.len()];

            for j in group_start..group_end {
                let row = &lines[j];
                let cells = content_cells(row);
                for (k, &(arow, acol)) in anchors.iter().enumerate() {
                    if j == arow && !anchor_set[k] {
                        let idx = cursor_content_index(row, acol).min(cells.len());
                        anchor_abs[k] = Some(merged.len() + idx);
                    }
                }
                merged.extend_from_slice(&cells);
            }

            // 硬换行组末尾的空白空格是填充，裁剪掉；软换行组的空格是内容，保留。
            if !group_wrapped {
                while merged.last().map_or(false, |c| c.is_blank()) {
                    merged.pop();
                }
            }

            // 按 new_cols 分行放置合并后的内容
            if merged.is_empty() {
                for (k, abs) in anchor_abs.iter().enumerate() {
                    if abs.is_some() && !anchor_set[k] {
                        new_anchors[k] = (out_row, 0);
                        anchor_set[k] = true;
                    }
                }
                new_lines.push(new_row(new_cols));
                out_row += 1;
            } else {
                // 按列宽拆分（而非按 cell 数量），宽字符不截断、不越界。
                let chunks = split_content_cells(&merged, new_cols);
                let total = chunks.len();
                let mut offset = 0usize;

                for (ci, chunk) in chunks.iter().enumerate() {
                    let mut row = write_chunk_into_row(chunk, new_cols);
                    // 非末片标记为软换行，末片继承组的软换行状态。
                    row.wrapped = if ci + 1 < total {
                        true
                    } else {
                        group_wrapped
                    };

                    // 锚点追踪：绝对 cell 偏移落在当前片内 → 映射为片内列；
                    // 锚点在内容末尾之后 → 定位到末片内容末尾。
                    for (k, abs) in anchor_abs.iter().enumerate() {
                        if anchor_set[k] {
                            continue;
                        }
                        if let Some(abs) = *abs {
                            if abs >= offset && abs < offset + chunk.len() {
                                let mut col = 0usize;
                                for (m, &cell) in chunk.iter().enumerate() {
                                    if m == abs - offset {
                                        break;
                                    }
                                    col += char_width(cell.character);
                                }
                                new_anchors[k] = (out_row, col);
                                anchor_set[k] = true;
                            } else if ci + 1 == total {
                                let mut col = 0usize;
                                for &cell in chunk.iter() {
                                    col += char_width(cell.character);
                                }
                                new_anchors[k] =
                                    (out_row, col.min(new_cols.saturating_sub(1)));
                                anchor_set[k] = true;
                            }
                        }
                    }

                    new_lines.push(row);
                    out_row += 1;
                    offset += chunk.len();
                }
            }
        }

        // 后处理：如果锚点落在 continuation 格上，回退到同字符的首格。
        for a in new_anchors.iter_mut() {
            let (cr, mut cc) = *a;
            if cr < new_lines.len() {
                let row = &new_lines[cr];
                if cc < row.cells.len() && row.cells[cc].continuation && cc > 0 {
                    cc -= 1;
                }
            }
            *a = (cr, cc);
        }

        (new_lines, new_anchors)
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

/// 返回旧行 `line` 中第 `cursor_col` 个格子对应的「内容 cell 索引」。
///
/// 「内容 cell」指非续列、非零宽（非默认 `\0`）的格子，与 reflow 提取的
/// 内容序列一一对应。规则：
///   - 光标落在宽字符续列占位格上 → 归到其首格（宽字符不可「从中拆开」）；
///   - 光标落在尾部零宽空 cell 上 → 归到内容末尾。
fn cursor_content_index(line: &Row, cursor_col: usize) -> usize {
    let idx = cursor_col.min(line.cells.len().saturating_sub(1));
    let mut ci = 0usize;
    for (i, cell) in line.cells.iter().enumerate() {
        if cell.continuation {
            if i == idx {
                return ci.saturating_sub(1);
            }
            continue;
        }
        if char_width(cell.character) == 0 {
            if i == idx {
                return ci;
            }
            continue;
        }
        if i == idx {
            return ci;
        }
        ci += 1;
    }
    ci
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

/// 提取一行中的「内容 cell」：跳过续列占位格与零宽空 cell（`\0`），保留空格。
///
/// 空格（宽度 1）是有意义的内容（如单词间的分隔），必须保留；
/// `\0`（默认空 cell，宽度 0）与 `continuation` 是填充/占位，reflow 时丢弃。
fn content_cells(row: &Row) -> Vec<Cell> {
    row.iter()
        .filter(|c| !c.continuation && char_width(c.character) > 0)
        .copied()
        .collect()
}

/// 一行是否「无可见内容」：所有内容 cell 都是默认样式空格（`is_blank`）。
///
/// grid 底部总有 `rows` 高的空填充行（全 `\0`），且用户也可能输出只有空格的行；
/// 这类行参与 reflow 只会产生多余的空白输出行、把真实内容误推入 scrollback，
/// 因此 resize 时作为「尾部空填充行」统一裁剪。有样式（如背景色）的空格不算空。
fn row_is_effectively_empty(row: &Row) -> bool {
    content_cells(row).iter().all(|c| c.is_blank())
}

/// 把内容 cell 按 `max_cols` 列宽拆分成若干片段（chunk）。
///   - 宽字符（宽度 2）不截断：放不下时整体换到下一片；
///   - `max_cols == 1` 时单个宽字符也放不下，按终端惯例截断为空格占位（保留原样式）。
fn split_content_cells(cells: &[Cell], max_cols: usize) -> Vec<Vec<Cell>> {
    let row_cap = max_cols.max(1);
    let mut chunks: Vec<Vec<Cell>> = Vec::new();
    let mut current: Vec<Cell> = Vec::new();
    let mut current_cols = 0usize;

    for &cell in cells {
        let w = char_width(cell.character);
        if w > row_cap {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                current_cols = 0;
            }
            let mut space = cell;
            space.character = ' ';
            chunks.push(vec![space]);
            continue;
        }
        if current_cols + w > row_cap {
            chunks.push(std::mem::take(&mut current));
            current_cols = 0;
        }
        current.push(cell);
        current_cols += w;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// 把一片内容 cell 写入一个新建的空行，重建宽字符的续列占位关系。
fn write_chunk_into_row(chunk: &[Cell], new_cols: usize) -> Row {
    let mut row = new_row(new_cols);
    let mut col_pos = 0usize;
    for &cell in chunk.iter() {
        if col_pos >= new_cols {
            break;
        }
        row.cells[col_pos] = cell;
        let w = char_width(cell.character);
        if w == 2 && col_pos + 1 < new_cols {
            row.cells[col_pos + 1] = Cell::continuation_cell();
        }
        col_pos += w;
    }
    row
}

