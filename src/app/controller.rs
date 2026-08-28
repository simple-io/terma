//! controller — App
//!
//! 桥接 Slint MainWindow 与 SessionManager / ProfileManager。
//!
//! 设计要点：
//!   - `App` 用 `Rc` 包装，`SessionManager` / `last_size` 用 `RefCell` 提供内部可变性，
//!     Slint 的 `'static` 回调闭包通过 `Weak<App>` 回访业务状态。
//!   - PTY 输出在独立 Output Thread 中读取 → mpsc channel → 原子标志通知 UI 线程：
//!     Output Thread 写入 channel 后设置 AtomicBool 标志，UI 线程定时器检测标志
//!     后立即 pump + render，实现低延迟的事件触发式渲染。
//!     Slint 对象绝不跨线程，PTY 线程不接触 Slint。
//!   - 渲染采用脏行标记（Dirty Rows）+ 按行缓存（Per-Row Cache）的增量渲染机制：
//!     ScreenBuffer 追踪哪些行被修改，渲染层仅对脏行调用 build_row_runs，
//!     未变更行复用上一帧的缓存结果，大幅降低 CPU 占用（尤其对少量字符输出场景）。
//!   - Tab 切换、窗口 resize、新建/关闭会话等交互事件立即触发渲染。
//!   - 键盘：TerminalView 的 FocusScope 接 key-pressed → encode_key（完整键盘映射）→ PTY。
//!   - 尺寸：定时器内同步检查窗口 resize，变化时同步 PTY + ScreenBuffer。
//!
//! 回调（全部在 Slint UI 线程执行）：
//!   on_new_tab        → 用默认 Profile 创建 Session
//!   on_close_tab(id)  → 关闭对应 Session
//!   on_select_tab(id) → 切换激活 Session（立即 pump + render）
//!   on_select_profile(pid) → 用指定 Profile 创建 Session
//!   on_key_pressed(text, mods) → 转发到激活 Session 的 PTY

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use slint::{ComponentHandle, VecModel};

use crate::profile::{Profile, ProfileManager};
use crate::renderer::{build_row_runs, RunRender};
use crate::session::SessionManager;
use crate::session::session::TerminalSessionLike;
use crate::session::session_id::{ProfileId, SessionId};

mod ui_generated {
    slint::include_modules!();
}

pub use ui_generated::{CellRun, MainWindow, ProfileInfo, TabInfo};

/// UI 刷新定时器间隔（检查原子标志 + resize + 按需渲染）
const TICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
/// 等宽字体度量（与 terminal_view.slint 中 char-w/char-h 保持一致）
const CHAR_W: i32 = 8;
const CHAR_H: i32 = 16;
/// Tab 栏 + 分隔线高度（逻辑像素）
const CHROME_H: i32 = 37;
/// 默认终端网格（启动目标尺寸，也作为无窗口尺寸可参考时的回退值）
const DEFAULT_ROWS: u16 = 36;
const DEFAULT_COLS: u16 = 120;
/// 窗口水平方向 chrome（终端区域左右留白，逻辑像素）
const H_PADDING: i32 = 16;
/// 窗口垂直方向底部留白（顶部 Tab 栏高度见 CHROME_H，逻辑像素）
const V_PADDING: i32 = 8;
/// 终端网格尺寸上限（防止异常窗口尺寸下分配过大缓冲）
const MAX_COLS: i32 = 400;
const MAX_ROWS: i32 = 200;

/// 按行渲染缓存：每行维护独立的 RunRender 列表，
/// 仅脏行触发 build_row_runs 重建，未变更行直接复用缓存。
struct RowRenderCache {
    /// 每行的 RunRender 缓存（key = 行号）
    rows: HashMap<usize, Vec<RunRender>>,
    /// 上一次同步给 Slint 的 CellRun 列表（用于比较是否需要 set_vec）
    prev_cells: Vec<CellRun>,
}

impl RowRenderCache {
    fn new() -> Self {
        Self {
            rows: HashMap::new(),
            prev_cells: Vec::new(),
        }
    }

    /// 清除所有缓存（Tab 切换 / 新建会话时调用）
    fn clear(&mut self) {
        self.rows.clear();
        self.prev_cells.clear();
    }
}

/// 应用控制器：持有窗口、会话管理器、Profile 管理器与 UI Model。
pub struct App {
    main_window: MainWindow,
    sessions: RefCell<SessionManager>,
    profiles: ProfileManager,
    tabs_model: Rc<VecModel<TabInfo>>,
    profiles_model: Rc<VecModel<ProfileInfo>>,
    // 终端 Cell 渲染模型（Run 列表）
    cells_model: Rc<VecModel<CellRun>>,
    // 上次同步给 PTY 的 (rows, cols)，用于检测窗口尺寸变化
    last_size: Cell<(u16, u16)>,
    // 主循环定时器（PTY 数据检查 + resize + 按需渲染）
    _tick_timer: slint::Timer,
    // 按行渲染缓存（增量渲染核心）
    row_cache: RefCell<RowRenderCache>,
    // PTY 数据到达标志：Output Thread 写入 channel 后置 true，
    // UI 线程 tick 检测后立即 pump + render。空闲时 tick 仅做廉价原子检查。
    data_flag: Arc<AtomicBool>,
}

impl App {
    pub fn run() {
        let main_window = MainWindow::new().expect("failed to create MainWindow");

        let tabs_model = Rc::new(VecModel::default());
        let profiles_model = Rc::new(VecModel::default());
        let cells_model = Rc::new(VecModel::default());

        main_window.set_tabs(tabs_model.clone().into());
        main_window.set_profiles(profiles_model.clone().into());
        main_window.set_cells(cells_model.clone().into());

        // let data_flag = Arc::new(AtomicBool::new(false));

        let app = Rc::new(Self {
            main_window,
            sessions: RefCell::new(SessionManager::new()),
            profiles: ProfileManager::new(),
            tabs_model,
            profiles_model,
            cells_model,
            last_size: Cell::new((0, 0)),
            _tick_timer: slint::Timer::default(),
            row_cache: RefCell::new(RowRenderCache::new()),
            data_flag: Arc::new(AtomicBool::new(false))
        });

        // 加载配置文件
        app.populate_profiles();
        // 先按目标终端网格设置窗口初始物理尺寸，使首个会话能以正确网格创建。
        app.apply_initial_window_size();
        // 创建默认会话
        app.create_default_session();
        app.sync_tabs();
        app.bind_callbacks();
        app.start_tick_timer();

        let weak = Rc::downgrade(&app);
        app.main_window.window().on_close_requested(move || {
            let _ = weak.upgrade();
            slint::CloseRequestResponse::HideWindow
        });

        app.main_window.show().expect("failed to show window");
        slint::run_event_loop().expect("event loop error");
    }

    // ─── UI 数据同步 ───
    fn populate_profiles(&self) {
        let items: Vec<ProfileInfo> = self
            .profiles
            .profiles()
            .iter()
            .map(|p| ProfileInfo {
                id: p.id.0.as_str().into(),
                name: p.name.as_str().into(),
                command: p.command.as_str().into(),
            })
            .collect();
        self.profiles_model.set_vec(items);
    }

    // 同步 Tab 列表 + 激活态标题（不触发渲染，渲染由调用方按需处理）。
    fn sync_tabs(&self) {
        let sessions = self.sessions.borrow();
        let tabs: Vec<TabInfo> = sessions
            .sessions_in_order()
            .iter()
            .map(|s| TabInfo {
                id: s.id().to_slint_string().into(),
                title: s.title().into(),
                active: Some(s.id()) == sessions.active_id(),
                profile_name: s.profile_name().into(),
            })
            .collect();
        self.tabs_model.set_vec(tabs);

        match sessions.active_session() {
            Some(s) => {
                self.main_window.set_active_title(s.title().into());
                self.main_window.set_active_profile_name(s.profile_name().into());
            }
            None => {
                self.main_window.set_active_title("".into());
                self.main_window.set_active_profile_name("".into());
            }
        }
    }

    // ─── 增量渲染核心 ───

    // 按需渲染：仅当激活会话有脏标记时才执行增量渲染。
    fn render_if_dirty(&self) {
        if !self.sessions.borrow().is_active_dirty() {
            return;
        }
        self.render();
    }

    // 增量渲染：仅对脏行调用 build_row_runs，未变更行复用缓存。
    // 利用 ScreenBuffer 的 dirty_rows 追踪 + RowRenderCache 按行缓存，
    // 将 O(全屏行数) 的 build_runs 降低到 O(脏行数)。
    fn render(&self) {
        let sessions = self.sessions.borrow();

        // 无激活会话 → 清空
        let state = match sessions.active_state() {
            Some(s) => s,
            None => {
                drop(sessions);
                let mut cache = self.row_cache.borrow_mut();
                if !cache.prev_cells.is_empty() {
                    cache.clear();
                    self.cells_model.set_vec(Vec::new());
                }
                self.main_window.set_cursor_visible(false);
                return;
            }
        };

        // 通过 RefCell 内部可变性获取脏行（&self，无需 &mut）
        let dirty_rows = state.take_dirty_rows();
        let cursor_row = state.cursor.row as i32;
        let cursor_col = state.cursor.col as i32;
        let cursor_visible = state.cursor_visible;

        // 对脏行重建 runs，未变更行复用缓存
        let mut cache = self.row_cache.borrow_mut();
        let visible = state.screen().visible_rows();
        for &row_idx in &dirty_rows {
            if row_idx < visible.len() {
                let new_runs = build_row_runs(&visible[row_idx], row_idx);
                cache.rows.insert(row_idx, new_runs);
            }
        }

        // 组装所有行的 runs → CellRun
        let num_rows = visible.len();
        let mut all_runs: Vec<RunRender> = Vec::new();
        for r in 0..num_rows {
            if let Some(row_runs) = cache.rows.get(&r) {
                all_runs.extend(row_runs.iter().cloned());
            }
        }

        let cells: Vec<CellRun> = all_runs
            .into_iter()
            .map(|r| CellRun {
                text: r.text.into(),
                row: r.row,
                col: r.col,
                len: r.len,
                fg: slint::Brush::SolidColor(slint::Color::from_rgb_u8(
                    r.fg.0, r.fg.1, r.fg.2,
                )),
                bg: slint::Brush::SolidColor(slint::Color::from_rgb_u8(
                    r.bg.0, r.bg.1, r.bg.2,
                )),
                has_bg: r.has_bg,
                bold: r.bold,
                italic: r.italic,
                underline: r.underline,
            })
            .collect();

        // 仅在内容变更时更新 Slint Model（避免无效 set_vec 开销）
        if cache.prev_cells != cells {
            self.cells_model.set_vec(cells.clone());
            cache.prev_cells = cells;
        }

        // 光标属性始终更新（开销极低）
        drop(cache);
        drop(sessions);
        self.main_window.set_cursor_row(cursor_row);
        self.main_window.set_cursor_col(cursor_col);
        self.main_window.set_cursor_visible(cursor_visible);

        self.sessions.borrow_mut().clear_active_dirty();
    }

    // ─── 定时器：原子标志检测 + resize + 按需渲染 ───

    fn start_tick_timer(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self._tick_timer.start(slint::TimerMode::Repeated, TICK_INTERVAL, move || {
            if let Some(app) = weak.upgrade() {
                app.tick();
            }
        });
    }

    /// 定时器回调：检测 PTY 数据标志 + resize + 按需渲染。
    /// 空闲状态（data_flag=false）下仅做廉价原子检查 + resize，
    /// 完全不触发 build_row_runs / set_vec，CPU 占用趋近于零。
    fn tick(&self) {
        // 检测 PTY 数据到达标志（原子操作，开销极低）
        let has_data = self.data_flag.swap(false, Ordering::Relaxed);

        // resize 检查（每次 tick 都执行，开销极低）
        self.maybe_resize();

        if has_data {
            // PTY 有新数据 → drain channel + 解析 + 按需渲染
            self.sessions.borrow_mut().pump_active();
            self.render_if_dirty();
        }
    }

    // UI 回调绑定
    fn bind_callbacks(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.main_window.on_new_tab(move || {
            if let Some(app) = weak.upgrade() {
                app.create_default_session();
                app.sync_tabs();
            }
        });

        let weak = Rc::downgrade(self);
        self.main_window.on_close_tab(move |id_str| {
            if let Some(app) = weak.upgrade() {
                if let Some(id) = parse_session_id(&id_str) {
                    app.close_session(id);
                }
                app.sync_tabs();
            }
        });

        let weak = Rc::downgrade(self);
        self.main_window.on_select_tab(move |id_str| {
            if let Some(app) = weak.upgrade() {
                if let Some(id) = parse_session_id(&id_str) {
                    app.sessions.borrow_mut().set_active_session(id);
                    // 切换 Tab 时清空按行缓存（新会话的行内容与旧会话无关）
                    app.row_cache.borrow_mut().clear();
                    // 立即 drain 新激活会话的待处理 PTY 数据并渲染
                    app.sessions.borrow_mut().pump_active();
                    app.render_if_dirty();
                    app.sync_tabs();
                }
            }
        });

        let weak = Rc::downgrade(self);
        self.main_window.on_select_profile(move |pid_str| {
            if let Some(app) = weak.upgrade() {
                let pid = ProfileId::new(pid_str.to_string());
                if let Some(profile) = app.profiles.get_profile(&pid) {
                    app.create_session_for_profile(profile);
                    app.sync_tabs();
                }
            }
        });

        // 键盘：转发到激活会话的 PTY
        let weak = Rc::downgrade(self);
        self.main_window.on_key_pressed(move |text, mods| {
            if let Some(app) = weak.upgrade() {
                let data = encode_key(&text, mods.control, mods.alt, mods.shift);
                if !data.is_empty() {
                    if let Err(e) = app.sessions.borrow_mut().write_to_active(&data) {
                        eprintln!("[terma] pty write error: {e}");
                    }
                }
            }
            true
        });
    }

    /// ─── 业务操作 ───
    // 创建默认会话
    fn create_default_session(self: &Rc<Self>) {
        let profile = self.profiles.default_profile().clone();
        self.create_session_for_profile(&profile);
    }

    // 根据 Profile 创建会话
    fn create_session_for_profile(self: &Rc<Self>, profile: &Profile) {
        // 用当前主窗口实际尺寸计算初始终端网格，避免新建会话回到固定 24×80。
        let (rows, cols) = self.current_grid();
        let next_index = self.sessions.borrow().len() + 1;
        let title = format!("{} {}", profile.name, next_index);

        // 创建 PTY 输出通知回调：Output Thread 写入 channel 后调用，
        // 设置原子标志通知 UI 线程有新数据到达。
        let data_flag = self.data_flag.clone();
        let notify_tx: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            data_flag.store(true, Ordering::Release);
        });

        if let Err(e) = self
            .sessions
            .borrow_mut()
            .create_session(profile, title, rows, cols, notify_tx)
        {
            eprintln!("[terma] create session failed: {e:#}");
        }
        // 新建会话后清空按行缓存并强制渲染（新会话 dirty=false，但需要清空旧内容）
        self.row_cache.borrow_mut().clear();
        self.sync_tabs();
        self.render();
    }

    // 根据 id 关闭会话
    fn close_session(&self, id: SessionId) {
        self.sessions.borrow_mut().close_session(id);
    }

    /// 按目标终端网格设置主窗口初始尺寸（物理像素），供启动时创建首个会话使用。
    fn apply_initial_window_size(&self) {
        let win = self.main_window.window();
        let logical = grid_to_logical_size(DEFAULT_ROWS, DEFAULT_COLS);
        win.set_size(slint::WindowSize::Logical(logical));
    }

    /// 当前主窗口对应的终端网格（rows, cols）。
    fn current_grid(&self) -> (u16, u16) {
        compute_grid(self.main_window.window()).unwrap_or((DEFAULT_ROWS, DEFAULT_COLS))
    }

    // 根据窗口尺寸计算 cols/rows，变化时同步所有会话的 PTY + ScreenBuffer。
    // resize 会标记 dirty_rows，下一次 tick 检测到 has_data 时自动渲染。
    fn maybe_resize(&self) {
        let Some((rows, cols)) = compute_grid(self.main_window.window()) else {
            return;
        };

        let last = self.last_size.get();
        if last != (rows, cols) {
            self.last_size.set((rows, cols));
            self.sessions.borrow_mut().resize_all(rows, cols);
            // resize 后清空按行缓存（所有行内容因重排而变化）
            self.row_cache.borrow_mut().clear();
            // resize 标记了 dirty，立即渲染确保窗口缩放无延迟
            self.render_if_dirty();
        }
    }
}

/// 由终端网格（rows/cols）计算窗口逻辑尺寸（与 `compute_grid` 互为逆运算）。
fn grid_to_logical_size(rows: u16, cols: u16) -> slint::LogicalSize {
    slint::LogicalSize::new(
        (cols as i32 * CHAR_W + H_PADDING) as f32,
        (rows as i32 * CHAR_H + CHROME_H + V_PADDING) as f32,
    )
}

/// 由窗口当前物理尺寸推导终端网格（rows, cols）。
fn compute_grid(win: &slint::Window) -> Option<(u16, u16)> {
    let phys = win.size();
    let scale = win.scale_factor();
    if scale <= 0.0 {
        return None;
    }
    let lw = (phys.width as f64 / scale as f64) as i32;
    let lh = (phys.height as f64 / scale as f64) as i32;
    let term_w = lw - H_PADDING;
    let term_h = lh - CHROME_H - V_PADDING;
    if term_w <= 0 || term_h <= 0 {
        return None;
    }
    let cols = (term_w / CHAR_W).clamp(1, MAX_COLS) as u16;
    let rows = (term_h / CHAR_H).clamp(1, MAX_ROWS) as u16;
    Some((rows, cols))
}

/// 把 Slint 传回的 id 字符串解析回 SessionId(u64)。
fn parse_session_id(s: &slint::SharedString) -> Option<SessionId> {
    s.as_str().parse::<u64>().ok().map(SessionId)
}

/// 把键盘事件编码为写入 PTY 的字节序列
fn encode_key(text: &slint::SharedString, control: bool, _alt: bool, _shift: bool) -> Vec<u8> {
    let s = text.as_str();
    let c = match s.chars().next() {
        Some(c) => c,
        None => return Vec::new(),
    };

    // 特殊键
    match c {
        '\u{0008}' => return vec![0x7f],                          // Backspace → DEL
        '\u{0009}' => return vec![0x09],                          // Tab
        '\u{000a}' => return vec![0x0d],                          // Return → CR
        '\u{001b}' => return vec![0x1b],                          // Escape
        '\u{007f}' => return b"\x1b[3~".to_vec(),                 // Delete
        '\u{F700}' => return b"\x1b[A".to_vec(),                  // Up
        '\u{F701}' => return b"\x1b[B".to_vec(),                  // Down
        '\u{F702}' => return b"\x1b[D".to_vec(),                  // Left
        '\u{F703}' => return b"\x1b[C".to_vec(),                  // Right
        '\u{F729}' => return b"\x1b[H".to_vec(),                  // Home
        '\u{F72B}' => return b"\x1b[F".to_vec(),                  // End
        '\u{F72C}' => return b"\x1b[5~".to_vec(),                 // PageUp
        '\u{F72D}' => return b"\x1b[6~".to_vec(),                 // PageDown
        '\u{F727}' => return b"\x1b[2~".to_vec(),                 // Insert
        '\u{F704}' => return b"\x1b[11~".to_vec(),                // F1
        '\u{F705}' => return b"\x1b[12~".to_vec(),                // F2
        '\u{F706}' => return b"\x1b[13~".to_vec(),                // F3
        '\u{F707}' => return b"\x1b[14~".to_vec(),                // F4
        _ => {}
    }

    // Ctrl + 字母 → 控制码
    if control && c.is_ascii_alphabetic() {
        let lc = c.to_ascii_lowercase();
        return vec![(lc as u8) & 0x1f];
    }

    // 普通可见字符 → utf8 字节
    s.as_bytes().to_vec()
}
