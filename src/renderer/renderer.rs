//! renderer — 二维 cells 渲染器（外层为行，内层为该行的 CellRun）
//!
//! 职责：把 `ScreenBuffer`（grid + scrollback）转换为 Slint 的二维 `CellRun` 模型
//! （`VecModel<ModelRc<CellRun>>`）并**直接增量写入**本会话私有的 model。每个
//! `TerminalSession` 持有一个 `Renderer`，独立计算、独立缓存、独立 model，会话之间互不干扰。
//!
//! 与旧的一维 `RenderCells` 相比，二维模型把「行」作为一等公民：外层 model 的每个元素
//! 就是一行的内层 `VecModel<CellRun>`。行与行彻底解耦，脏行更新只需就地改动对应行的内层
//! model，无需再维护扁平 model 的前缀偏移（prefix）与各视口行长度（viewport_lens）。
//!
//! ⚡ 增量更新策略（语义与 RenderCells 保持一致）：
//!   - **跟随模式**（offset == 0）：`follow_cache` 按行缓存 `CellRun`。脏行重建后仅对该行
//!     的内层 model 做 `set_row_data` / `insert` / `remove`，未变更行零转换零触碰；
//!   - **滚动模式**（offset > 0）：以 `(offset, sb_len, sb_total)` 为视口 key 做快照缓存。
//!     key 不变（视口冻结，仅 grid 脏行变化）时只增量更新落入视口的脏行；key 变化
//!     （滚动 / 历史增长）时才全量重建视口；滚动期间持续把脏行同步进 follow_cache，
//!     回到跟随模式免重算；
//!   - **全量路径**（resize / 首帧 / 模式切换 / 脏行过半）：重建所有行的内层 model，
//!     行数变化时单次 `set_vec` 重建外层 model。
//!
//! 线程与所有权约束：
//!   - `VecModel` 非 `Send`，`Renderer` 随 `TerminalSession` 仅在 UI 线程创建与访问，
//!     所有 model 操作都发生在 `render_frame`（UI 线程）内，绝不跨线程共享；
//!   - App（UI 线程）通过 `model()` 取出 `Rc<VecModel<ModelRc<CellRun>>>` 挂载到窗口
//!     `cells` property，切换 Tab 即换挂 model —— 内容零拷贝、零重算。

use crate::app::controller::CellRun;
use crate::config::ColorScheme;
use crate::terminal::screen::ScreenBuffer;
use slint::{Model, ModelRc, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// model 当前呈现内容的模式（用于判定增量更新的前置条件是否成立）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Present {
    /// 跟随模式：外层 model 内容 == follow_cache 各行
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

/// 会话独立的渲染单元：按行缓存渲染项 + 会话私有二维 Slint model 的增量维护。
pub struct Renderer {
    // 本会话私有的二维 cells model（外层为行，内层为该行的 CellRun），挂载到窗口由 App 负责
    model: Rc<VecModel<ModelRc<CellRun>>>,
    // 每行常驻的内层 model（下标 = 视口行号），就地增量更新，行与行互不影响
    rows: RefCell<Vec<Rc<VecModel<CellRun>>>>,
    // 跟随模式按行缓存（下标 = 网格行号），滚动期间也持续同步，切回跟随免重算
    follow_cache: RefCell<Vec<Vec<CellRun>>>,
    // model 当前呈现模式
    present: Cell<Present>,
    // 强制下次全量重建（resize 等缓存失效场景）
    force_full: Cell<bool>,
    // 当前配色方案（颜色解析用；切换方案时置空缓存强制全量重算）
    color_scheme: RefCell<ColorScheme>,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            model: Rc::new(VecModel::default()),
            rows: RefCell::new(Vec::new()),
            follow_cache: RefCell::new(Vec::new()),
            present: Cell::new(Present::Follow),
            force_full: Cell::new(true),
            color_scheme: RefCell::new(ColorScheme::default()),
        }
    }

    /// 切换配色方案：更新后清空缓存并强制下次全量重建（颜色需整体重算）。
    pub fn set_color_scheme(&self, scheme: ColorScheme) {
        *self.color_scheme.borrow_mut() = scheme;
        self.clear();
    }

    /// 本会话私有的二维 cells model（App 在切换/新建会话时挂载到窗口）。
    pub fn model(&self) -> Rc<VecModel<ModelRc<CellRun>>> {
        self.model.clone()
    }

    /// 缓存失效（resize 等尺寸变化时调用）：清空按行缓存并强制下次全量重建。
    pub fn clear(&self) {
        self.follow_cache.borrow_mut().clear();
        self.force_full.set(true);
    }

    // 跟随模式渲染：仅重建脏行并**增量写入内层 model**，未变更行零转换零触碰。
    //
    // 全量路径条件（避免海量逐项通知）：
    //   - 缓存失效（首帧 / resize 后长度不匹配）或 force_full；
    //   - model 当前呈现的不是跟随内容（从滚动模式切回）；
    //   - 脏行数 ≥ 总行数一半。
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

        // 全量路径：重建所有脏行缓存，并按缓存重建各行内层 model
        if full {
            for idx in &dirty_rows {
                if let Some(row) = visible.get(*idx) {
                    cache[*idx] = row.to_cell_run(*idx, &scheme);
                }
            }
            self.rebuild_rows(&cache);
            self.present.set(Present::Follow);
            self.force_full.set(false);
            return;
        }

        // 增量路径：仅重建脏行的内层 model（外层 model 零触碰，行数未变）
        let rows = self.rows.borrow();
        for idx in dirty_rows {
            let Some(row) = visible.get(idx) else {
                continue;
            };
            let new_runs = row.to_cell_run(idx, &scheme);
            if let Some(inner) = rows.get(idx) {
                apply_row(inner, &new_runs);
            }
            cache[idx] = new_runs;
        }
    }

    // 滚动模式渲染：以 `(offset, sb_len, sb_total)` 为视口 key 的快照缓存。
    //
    //   - key 不变（视口冻结，仅 grid 脏行变化）：只增量更新落入视口的脏 grid 行
    //     （视口行 = grid 行 + offset），scrollback 行不变零开销；
    //   - key 变化（滚动 / 历史增长补偿）：全量重建视口。
    //
    // 无论走哪条路径，都先把脏行同步进 follow_cache（保持其与 grid 最新内容一致），
    // 回到跟随模式时直接从缓存全量重建，无需重算任何行。
    pub fn render_viewport(&self, screen: &ScreenBuffer, offset: usize, mut dirty_rows: Vec<usize>) {
        let rows_count = screen.rows;
        let scheme = self.color_scheme.borrow();
        let sb_len = screen.scrollback.len();
        let sb_total = screen.scrollback_total;
        // 防御性 clamp：offset 不越过物理历史行数
        let offset = offset.min(sb_len);
        dirty_rows.sort_unstable();
        dirty_rows.dedup();

        // 1) 同步跟随缓存（脏行重建，行号 = 网格行号）
        {
            let mut cache = self.follow_cache.borrow_mut();
            if cache.len() != rows_count {
                cache.clear();
                cache.resize_with(rows_count, Vec::new);
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
        let incremental = !self.force_full.get()
            && self.present.get() == key
            && self.rows.borrow().len() == rows_count;

        if incremental {
            // 2) 增量：仅更新落入视口的脏 grid 行（视口行 v = g + offset < rows）
            let rows = self.rows.borrow();
            for &g in &dirty_rows {
                let v = g + offset;
                if v >= rows_count {
                    continue; // 该 grid 行不在当前视口内
                }
                let Some(source) = screen.grid.get(g) else {
                    continue;
                };
                let new_runs = source.to_cell_run(v, &scheme);
                if let Some(inner) = rows.get(v) {
                    apply_row(inner, &new_runs);
                }
            }
            return;
        }

        // 3) 全量重建视口：scrollback 历史 + grid 当前屏拼接为视口
        let mut viewport_rows: Vec<Vec<CellRun>> = Vec::with_capacity(rows_count);
        for v in 0..rows_count {
            let source: &crate::terminal::row::Row = if v < offset {
                &screen.scrollback[sb_len - offset + v]
            } else {
                &screen.grid[v - offset]
            };
            viewport_rows.push(source.to_cell_run(v, &scheme));
        }
        self.rebuild_rows(&viewport_rows);
        self.present.set(key);
        self.force_full.set(false);
    }

    /// 全量重建：按给定各行渲染项，确保外层 model 有 `rows.len()` 行，逐行就地更新内层 model；
    /// 仅当行数变化时才对外层 model 做一次 `set_vec`（行内层 model 引用保持稳定）。
    fn rebuild_rows(&self, rows_content: &[Vec<CellRun>]) {
        let n = rows_content.len();
        let mut rows = self.rows.borrow_mut();
        // 调整常驻内层 model 数量到 n（复用已有的，尾部新增 / 截断）
        rows.resize_with(n, || Rc::new(VecModel::default()));
        // 逐行填充内容（就地 diff，未变更项不触发通知）
        for v in 0..n {
            apply_row(&rows[v], &rows_content[v]);
        }
        // 行数变化才重建外层 model：新建/截断内层 model 后需重新挂载
        if self.model.row_count() != n {
            let outer: Vec<ModelRc<CellRun>> = rows.iter().map(|r| r.clone().into()).collect();
            self.model.set_vec(outer);
        }
    }
}

/// 对一行内层 model 应用新内容 `new`：
///   - 前 common 项：值有变化才 `set_row_data`（触发单行 RowChanged）；
///   - 变多：尾部逐项 `insert`；
///   - 变少：尾部逐项 `remove`。
fn apply_row(inner: &VecModel<CellRun>, new: &[CellRun]) {
    let old_len = inner.row_count();
    let new_len = new.len();
    let common = old_len.min(new_len);
    for i in 0..common {
        if let Some(old) = inner.row_data(i) {
            if !same_run(&old, &new[i]) {
                inner.set_row_data(i, new[i].clone());
            }
        }
    }
    if new_len > old_len {
        for i in old_len..new_len {
            inner.insert(i, new[i].clone());
        }
    } else if new_len < old_len {
        for _ in new_len..old_len {
            inner.remove(new_len);
        }
    }
}

/// 两个渲染项是否完全一致（逐字段比较，值未变则跳过 model 通知）。
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
