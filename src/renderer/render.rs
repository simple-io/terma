//! render — RenderCells：每个会话独立的渲染计算、按行缓存与 Slint model 增量更新单元
//!
//! 职责：把 `ScreenBuffer`（grid + scrollback）转换为 Slint 的 `CellRun` 并**直接增量写入**
//! 本会话私有的 `VecModel<CellRun>`。每个 `TerminalSession` 持有一个 `RenderCells`，
//! 独立计算、独立缓存、独立 model，会话之间互不干扰。
//!
//! ⚡ 增量更新策略（替代旧的「每帧全量转换 + 全量 set_vec」）：
//!   - **跟随模式**（offset == 0）：`follow_cache` 按行缓存 `CellRun`（缓存即最终渲染项，
//!     不再缓存中间态 RunRender）。脏行重建后仅对该行在扁平 model 中的区段做
//!     `set_row_data` / `insert` / `remove`，未变更行的渲染项**零转换零触碰**；
//!   - **滚动模式**（offset > 0）：以 `(offset, sb_len, sb_total)` 为视口 key 做快照缓存。
//!     key 不变（视口冻结，仅 grid 脏行变化）时只增量更新落入视口的脏行；
//!     key 变化（滚动 / 历史增长）时才全量重建视口（单次 set_vec）。
//!     滚动期间持续把脏行同步进 follow_cache，回到跟随模式免重算；
//!   - **全量路径**（resize / 首帧 / 模式切换 / 脏行过半）：单次 `set_vec` 重建，
//!     避免海量逐项通知反而更慢。
//!
//! 线程与所有权约束：
//!   - `VecModel` 非 `Send`，`RenderCells` 随 `TerminalSession` 仅在 UI 线程创建与访问，
//!     所有 model 操作都发生在 `render_frame`（UI 线程）内，绝不跨线程共享；
//!   - App（UI 线程）通过 `model()` 取出 `Rc<VecModel>` 挂载到窗口 `cells` property，
//!     切换 Tab 即换挂 model —— 内容零拷贝、零重算。

use crate::app::controller::CellRun;
use crate::config::ColorScheme;
use crate::terminal::screen::ScreenBuffer;
use slint::{Model, VecModel};
use std::cell::{Cell, RefCell};
use std::env::var;
use std::rc::Rc;

/// model 当前呈现内容的模式（用于判定增量更新的前置条件是否成立）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Present {
    /// 跟随模式：model 内容 == follow_cache 各行按序拼接
    Follow,
    /// 滚动模式：model 内容为视口快照，key 标识快照对应的滚动状态。
    /// 任一 key 分量变化都意味着视口内容整体改变，需全量重建：
    ///   - offset：滚动位置；
    ///   - sb_len：物理历史行数（历史被清空 / 裁剪时变化）；
    ///   - sb_total：逻辑累计历史行数（历史满后仍持续增长，可捕捉
    ///     「物理长度不变但内容整体上移一行」的边界情形）。
    Viewport {
        offset: usize,
        sb_len: usize,
        sb_total: usize,
    },
}


/// 会话独立的渲染单元：按行缓存渲染项 + 会话私有 Slint model 的增量维护。
pub struct RenderCells {
    /// 本会话私有的 cells model（挂载到窗口由 App 负责）
    model: Rc<VecModel<CellRun>>,
    /// 跟随模式按行缓存（下标 = 网格行号），滚动期间也持续同步，切回跟随免重算
    follow_cache: RefCell<Vec<Vec<CellRun>>>,
    /// 滚动模式视口快照中各视口行的渲染项数量（用于定位行段在扁平 model 中的偏移）
    viewport_lens: RefCell<Vec<usize>>,
    /// model 当前呈现模式
    present: Cell<Present>,
    /// 强制下次全量重建（resize 等缓存失效场景）
    force_full: Cell<bool>,
    /// 配色方案（颜色解析用）
    color_scheme: RefCell<ColorScheme>,
}

impl Default for RenderCells {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderCells {
    pub fn new() -> Self {
        Self {
            model: Rc::new(VecModel::default()),
            follow_cache: RefCell::new(Vec::new()),
            viewport_lens: RefCell::new(Vec::new()),
            present: Cell::new(Present::Follow),
            force_full: Cell::new(true),
            color_scheme: RefCell::new(ColorScheme::default()),
        }
    }

    /// 本会话私有的 cells model（App 在切换/新建会话时挂载到窗口）。
    pub fn model(&self) -> Rc<VecModel<CellRun>> {
        self.model.clone()
    }

    /// 缓存失效（resize 等尺寸变化时调用）：清空按行缓存并强制下次全量重建。
    pub fn clear(&self) {
        self.follow_cache.borrow_mut().clear();
        self.viewport_lens.borrow_mut().clear();
        self.force_full.set(true);
    }

    /// 跟随模式渲染：仅重建脏行并**增量写入 model**，未变更行零转换零触碰。
    ///
    /// 全量路径条件（单次 set_vec，避免海量逐项通知）：
    ///   - 缓存失效（首帧 / resize 后长度不匹配）或 force_full；
    ///   - model 当前呈现的不是跟随内容（从滚动模式切回）；
    ///   - 脏行数 ≥ 总行数一半。
    pub fn render_follow(&self, screen: &ScreenBuffer, mut dirty_rows: Vec<usize>) {
        let visible = screen.visible_rows();
        let scheme = self.color_scheme.borrow();
        dirty_rows.sort_unstable();
        dirty_rows.dedup();

        // 缓存对齐，如果窗口尺寸发生了变化，重置缓存
        let mut cache = self.follow_cache.borrow_mut();
        let cache_reset = cache.len() != visible.len();
        if cache_reset {
            cache.clear();
            cache.resize_with(visible.len(), Vec::new);
            dirty_rows = (0..visible.len()).collect();
        }

        // 全量路径条件：缓存失效、非跟随内容、脏行数过多
        let full = self.force_full.get()
            || cache_reset
            || self.present.get() != Present::Follow
            || dirty_rows.len() * 2 >= visible.len().max(1);

        // 全量路径：重建所有脏行并 set_vec
        if full {
            for idx in &dirty_rows {
                if let Some(row) = visible.get(*idx) {
                    cache[*idx] = row.to_cell_run(*idx, &scheme);
                }
            }
            let flat: Vec<CellRun> = cache.iter().flatten().cloned().collect();
            self.model.set_vec(flat);
            self.present.set(Present::Follow);
            self.force_full.set(false);
            return;
        }

        // 增量路径：按行号升序遍历脏行，维护前缀偏移定位行段
        for idx in dirty_rows {
            let Some(row) = visible.get(idx) else {
                continue;
            };
            // 前缀 = 该行之前所有行的渲染项数量之和（升序遍历，增量推进）
            let prefix: usize = cache[..idx].iter().map(|r| r.len()).sum();
            let new_runs = row.to_cell_run(idx, &scheme);
            self.apply_row_segment(prefix, &cache[idx], &new_runs);
            cache[idx] = new_runs;
        }
    }

    /// 滚动模式渲染：以 `(offset, sb_len, sb_total)` 为视口 key 的快照缓存。
    ///
    ///   - key 不变（视口冻结，仅 grid 脏行变化）：只增量更新落入视口的脏 grid 行
    ///     （视口行 = grid 行 + offset），scrollback 行不变零开销；
    ///   - key 变化（滚动 / 历史增长补偿）：全量重建视口（单次 set_vec）。
    ///
    /// 无论走哪条路径，都先把脏行同步进 follow_cache（保持其与 grid 最新内容一致），
    /// 回到跟随模式时直接从缓存全量 set_vec，无需重算任何行。
    pub fn render_viewport(&self, screen: &ScreenBuffer, offset: usize, mut dirty_rows: Vec<usize>) {
        let rows = screen.rows;
        let scheme = self.color_scheme.borrow();
        let sb_len = screen.scrollback.len();
        let sb_total = screen.scrollback_total;
        // 防御性 clamp：与旧 build_viewport_runs 一致，offset 不越过物理历史行数
        let offset = offset.min(sb_len);
        dirty_rows.sort_unstable();
        dirty_rows.dedup();

        // 1) 同步跟随缓存（脏行重建，行号 = 网格行号）
        {
            let mut cache = self.follow_cache.borrow_mut();
            if cache.len() != rows {
                cache.clear();
                cache.resize_with(rows, Vec::new);
            }
            for &g in &dirty_rows {
                if let Some(row) = screen.grid.get(g) {
                    cache[g] = row.to_cell_run(g, &scheme);
                }
            }
        }

        let key = Present::Viewport {
            offset,
            sb_len,
            sb_total,
        };
        let lens_len = self.viewport_lens.borrow().len();
        let incremental = !self.force_full.get()
            && self.present.get() == key
            && lens_len == rows;

        if incremental {
            // 2) 增量：仅更新落入视口的脏 grid 行（视口行 v = g + offset < rows）
            let mut lens = self.viewport_lens.borrow_mut();
            let mut prefix = 0usize;
            let mut cursor = 0usize; // 已累计过长度的视口行游标
            for &g in &dirty_rows {
                let v = g + offset;
                if v >= rows {
                    continue; // 该 grid 行不在当前视口内
                }
                let Some(source) = screen.grid.get(g) else {
                    continue;
                };
                for r in cursor..v {
                    prefix += lens[r];
                }
                cursor = v;
                let new_runs = source.to_cell_run(v, &scheme);
                let old_len = lens[v];
                // 逐项应用（无需旧内容做逐项比较——整段以新内容为准）
                let common = old_len.min(new_runs.len());
                for i in 0..common {
                    self.model.set_row_data(prefix + i, new_runs[i].clone());
                }
                if new_runs.len() > old_len {
                    for i in common..new_runs.len() {
                        self.model.insert(prefix + i, new_runs[i].clone());
                    }
                } else if new_runs.len() < old_len {
                    for _ in new_runs.len()..old_len {
                        self.model.remove(prefix + new_runs.len());
                    }
                }
                lens[v] = new_runs.len();
                prefix += new_runs.len();
            }
            return;
        }

        // 3) 全量重建视口：scrollback 历史 + grid 当前屏拼接为视口
        let mut flat: Vec<CellRun> = Vec::new();
        let mut lens = vec![0usize; rows];
        for v in 0..rows {
            let source: &crate::terminal::row::Row = if v < offset {
                &screen.scrollback[sb_len - offset + v]
            } else {
                &screen.grid[v - offset]
            };
            let runs = source.to_cell_run(v, &scheme);
            lens[v] = runs.len();
            flat.extend(runs);
        }
        self.model.set_vec(flat);
        *self.viewport_lens.borrow_mut() = lens;
        self.present.set(key);
        self.force_full.set(false);
    }


    /// 对 model 中 `[start, start+old_len)` 区段应用新内容 `[new_runs]`：
    ///   - 前 common 项：值有变化才 `set_row_data`（触发单行 RowChanged）；
    ///   - 变多：尾部逐项 `insert`；
    ///   - 变少：尾部逐项 `remove`。
    /// 后续行段的索引随 insert/remove 自动平移，由调用方按升序遍历并维护前缀偏移。
    fn apply_row_segment(&self, start: usize, old: &[CellRun], new: &[CellRun]) {
        let common = old.len().min(new.len());
        for i in 0..common {
            if !same_run(&old[i], &new[i]) {
                self.model.set_row_data(start + i, new[i].clone());
            }
        }
        if new.len() > old.len() {
            for i in common..new.len() {
                self.model.insert(start + i, new[i].clone());
            }
        } else if new.len() < old.len() {
            for _ in new.len()..old.len() {
                self.model.remove(start + new.len());
            }
        }
    }
}


// 两个渲染项是否完全一致（逐字段比较，值未变则跳过 model 通知）。
fn same_run(a: &CellRun, b: &CellRun) -> bool {
    a.row == b.row
        && a.col == b.col
        && a.len == b.len
        && a.text == b.text
        && a.fg == b.fg
        && a.bg == b.bg
        && a.has_bg == b.has_bg
        && a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::state::TerminalState;

    /// 把字符串按 UTF-8 字节逐字节 feed 进 vte 解析器。
    fn feed(state: &mut TerminalState, s: &str) {
        let mut parser = vte::Parser::new();
        for &b in s.as_bytes() {
            parser.advance(state, b);
        }
    }

    /// 读取 model 全部渲染项为 (row, col, text) 三元组，便于断言。
    fn snapshot(model: &VecModel<CellRun>) -> Vec<(i32, i32, String)> {
        (0..model.row_count())
            .map(|i| {
                let r = model.row_data(i).unwrap();
                (r.row, r.col, r.text.as_str().to_string())
            })
            .collect()
    }

    #[test]
    fn follow_first_render_is_full() {
        let mut st = TerminalState::new(4, 10, Default::default(), 1000);
        feed(&mut st, "AAAA\r\nBBBB\r\nCCCC");
        let rc = RenderCells::new();
        rc.render_follow(st.screen(), st.take_dirty_rows());
        let snap = snapshot(&rc.model());
        assert_eq!(snap.len(), 12);
        assert_eq!(snap[0], (0, 0, "A".into()));
        assert_eq!(snap[11], (2, 3, "C".into()));
    }

    #[test]
    fn follow_incremental_updates_only_dirty_row() {
        let mut st = TerminalState::new(4, 10, Default::default(), 1000);
        feed(&mut st, "AAAA\r\nBBBB\r\nCCCC");
        let rc = RenderCells::new();
        rc.render_follow(st.screen(), st.take_dirty_rows());

        // 覆写 row1 为 XXXX（少量脏行 → 增量路径）
        feed(&mut st, "\x1b[2;1HXXXX");
        rc.render_follow(st.screen(), st.take_dirty_rows());

        let snap = snapshot(&rc.model());
        // 总项数不变（同行等长替换），row1 内容更新，其余行不受影响
        assert_eq!(snap.len(), 12);
        assert_eq!(snap[0], (0, 0, "A".into()));
        for i in 0..4 {
            assert_eq!(snap[4 + i], (1, i as i32, "X".into()));
        }
        assert_eq!(snap[8], (2, 0, "C".into()));
    }

    #[test]
    fn follow_row_shrink_removes_items() {
        let mut st = TerminalState::new(2, 10, Default::default(), 1000);
        feed(&mut st, "AAAA\r\nBBBB");
        let rc = RenderCells::new();
        rc.render_follow(st.screen(), st.take_dirty_rows());
        assert_eq!(rc.model().row_count(), 8);

        // row0 清行后只写 2 字符 → 该行区段收缩（remove）
        feed(&mut st, "\x1b[1;1H\x1b[2KCC");
        rc.render_follow(st.screen(), st.take_dirty_rows());

        let snap = snapshot(&rc.model());
        assert_eq!(snap.len(), 6); // CC + BBBB
        assert_eq!(snap[0], (0, 0, "C".into()));
        assert_eq!(snap[1], (0, 1, "C".into()));
        assert_eq!(snap[2], (1, 0, "B".into()));
        assert_eq!(snap[5], (1, 3, "B".into()));
    }

    #[test]
    fn follow_row_grow_inserts_items() {
        let mut st = TerminalState::new(2, 10, Default::default(), 1000);
        feed(&mut st, "AA\r\nBBBB");
        let rc = RenderCells::new();
        rc.render_follow(st.screen(), st.take_dirty_rows());
        assert_eq!(rc.model().row_count(), 6);

        // row0 追加 3 字符 → 该行区段增长（insert），后续行整体后移
        feed(&mut st, "\x1b[1;1HAADEF");
        rc.render_follow(st.screen(), st.take_dirty_rows());

        let snap = snapshot(&rc.model());
        assert_eq!(snap.len(), 9); // AADEF + BBBB
        assert_eq!(snap[4], (0, 4, "F".into()));
        assert_eq!(snap[5], (1, 0, "B".into()));
        assert_eq!(snap[8], (1, 3, "B".into()));
    }

    #[test]
    fn viewport_full_rebuild_on_key_change() {
        // 2 行屏，产生 2 行 scrollback：grid=CCCC/DDDD，scrollback=AAAA/BBBB
        let mut st = TerminalState::new(2, 10, Default::default(), 1000);
        feed(&mut st, "AAAA\r\nBBBB\r\nCCCC\r\nDDDD");
        assert_eq!(st.screen().scrollback.len(), 2);
        let rc = RenderCells::new();

        // offset=1：视口 = scrollback[BBBB] + grid[0][CCCC]
        rc.render_viewport(st.screen(), 1, vec![]);
        let snap = snapshot(&rc.model());
        assert_eq!(snap.len(), 8);
        assert_eq!(snap[0], (0, 0, "B".into()));
        assert_eq!(snap[4], (1, 0, "C".into()));

        // offset=2：视口 = scrollback[AAAA] + scrollback[BBBB]（key 变化 → 全量重建）
        rc.render_viewport(st.screen(), 2, vec![]);
        let snap = snapshot(&rc.model());
        assert_eq!(snap.len(), 8);
        assert_eq!(snap[0], (0, 0, "A".into()));
        assert_eq!(snap[4], (1, 0, "B".into()));
    }

    #[test]
    fn viewport_incremental_updates_dirty_grid_row() {
        // 3 行屏：scrollback=[AAAA]，grid=BBBB/CCCC/DDDD
        let mut st = TerminalState::new(3, 10, Default::default(), 1000);
        feed(&mut st, "AAAA\r\nBBBB\r\nCCCC\r\nDDDD");
        let rc = RenderCells::new();
        rc.render_follow(st.screen(), st.take_dirty_rows());

        // offset=1：视口 = AAAA/BBBB/CCCC
        rc.render_viewport(st.screen(), 1, vec![]);
        let snap = snapshot(&rc.model());
        assert_eq!(snap[0], (0, 0, "A".into()));
        assert_eq!(snap[8], (2, 0, "C".into()));

        // key 不变，grid row1 改为 ZZZZ → 视口行 2（= grid 1 + offset 1）增量更新
        feed(&mut st, "\x1b[2;1HZZZZ");
        rc.render_viewport(st.screen(), 1, st.take_dirty_rows());
        let snap = snapshot(&rc.model());
        assert_eq!(snap.len(), 12);
        assert_eq!(snap[0], (0, 0, "A".into())); // scrollback 行不动
        assert_eq!(snap[8], (2, 0, "Z".into())); // 视口行 2 已更新
        assert_eq!(snap[11], (2, 3, "Z".into()));
    }

    #[test]
    fn viewport_return_to_follow_uses_fresh_cache() {
        // 3 行屏：scrollback=[AAAA]，grid=BBBB/CCCC/DDDD
        let mut st = TerminalState::new(3, 10, Default::default(), 1000);
        feed(&mut st, "AAAA\r\nBBBB\r\nCCCC\r\nDDDD");
        let rc = RenderCells::new();
        rc.render_follow(st.screen(), st.take_dirty_rows());

        // 进入滚动模式，期间 grid row1 变更（脏行同步进跟随缓存）
        rc.render_viewport(st.screen(), 1, vec![]);
        feed(&mut st, "\x1b[2;1HZZZZ");
        rc.render_viewport(st.screen(), 1, st.take_dirty_rows());

        // 回到跟随模式：缓存已同步（ZZZZ），无需重算，直接全量 set_vec
        rc.render_follow(st.screen(), vec![]);
        let snap = snapshot(&rc.model());
        assert_eq!(snap.len(), 12);
        assert_eq!(snap[0], (0, 0, "B".into()));
        assert_eq!(snap[4], (1, 0, "Z".into()));
        assert_eq!(snap[8], (2, 0, "D".into()));
    }

    #[test]
    fn clear_forces_full_rebuild_after_resize() {
        let mut st = TerminalState::new(4, 10, Default::default(), 1000);
        feed(&mut st, "AAAA\r\nBBBB\r\nCCCC");
        let rc = RenderCells::new();
        rc.render_follow(st.screen(), st.take_dirty_rows());

        // resize 后缓存失效（clear），即使脏行为空也必须全量重建
        st.resize(2, 10);
        rc.clear();
        rc.render_follow(st.screen(), st.take_dirty_rows());
        let snap = snapshot(&rc.model());
        // 缩行后 grid = BBBB/CCCC（AAAA 滚入 scrollback）
        assert_eq!(snap.len(), 8);
        assert_eq!(snap[0], (0, 0, "B".into()));
        assert_eq!(snap[4], (1, 0, "C".into()));
    }

    #[test]
    fn follow_many_dirty_rows_falls_back_to_full() {
        let mut st = TerminalState::new(3, 10, Default::default(), 1000);
        feed(&mut st, "AAAA\r\nBBBB\r\nCCCC");
        let rc = RenderCells::new();
        rc.render_follow(st.screen(), st.take_dirty_rows());

        // 全部行重写（脏行过半 → 全量路径），结果必须与逐行增量一致
        feed(&mut st, "\x1b[1;1HXXXX\x1b[2;1HYYYY\x1b[3;1HZZZZ");
        rc.render_follow(st.screen(), st.take_dirty_rows());
        let snap = snapshot(&rc.model());
        assert_eq!(snap.len(), 12);
        assert_eq!(snap[0], (0, 0, "X".into()));
        assert_eq!(snap[4], (1, 0, "Y".into()));
        assert_eq!(snap[8], (2, 0, "Z".into()));
    }
}
