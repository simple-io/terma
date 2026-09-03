//! controller — App
//!
//! 桥接 Slint MainWindow 与 SessionManager / ProfileManager。
//!
//! 设计要点：
//!   - `App` 用 `Rc` 包装，`SessionManager` / `last_size` 用 `RefCell` 提供内部可变性，
//!     Slint 的 `'static` 回调闭包通过 `Weak<App>` 回访业务状态。
//!   - PTY 输出在独立 Output Thread 中读取并完成 vte 解析；
//!     仅激活会话经 `SyncSender(1)` 唤醒通道通知渲染分发线程，由后者
//!     `invoke_from_event_loop` 调度 UI 线程渲染，实现低延迟事件触发式渲染，
//!     彻底移除渲染轮询定时器。Slint 对象绝不跨线程，PTY 线程不接触 Slint。
//!   - 非激活会话的输出在后台静默解析、不唤醒 UI 线程，避免浪费 UI 线程资源。
//!   - 渲染采用脏标记（Dirty）+ 脏行标记（Dirty Rows）+ 按行缓存（Per-Row Cache）：
//!     UI 线程锁 `Arc<Mutex<TerminalState>>` 快照脏行，仅对脏行调用 build_row_runs，
//!     未变更行复用上一帧的缓存结果，大幅降低 CPU 占用（尤其对少量字符输出场景）。
//!   - Tab 切换、窗口 resize、新建/关闭会话等交互事件立即强制全量渲染。
//!   - Tab 切换、窗口 resize、新建/关闭会话等交互事件立即触发渲染。
//!   - 键盘：TerminalView 的 FocusScope 接 key-pressed → encode_key（完整键盘映射）→ PTY。
//!   - 尺寸：独立的低频率定时器轮询 window().size()，变化时同步 PTY + ScreenBuffer。
//!
//! 回调（全部在 Slint UI 线程执行）：
//!   on_new_tab        → 用默认 Profile 创建 Session
//!   on_close_tab(id)  → 关闭对应 Session
//!   on_select_tab(id) → 切换激活 Session（立即强制全量渲染）
//!   on_select_profile(pid) → 用指定 Profile 创建 Session
//!   on_key_pressed(text, mods) → 转发到激活 Session 的 PTY

use slint::{ComponentHandle, VecModel};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use crate::profile::{Profile, ProfileManager};
use crate::renderer::{RunRender, build_row_runs};
use crate::session::SessionManager;
use crate::session::session::TerminalSessionLike;
use crate::session::session_id::{ProfileId, SessionId};
use crate::terminal::Cell as TerminalCell;

mod ui_generated {
    slint::include_modules!();
}

pub use ui_generated::{CellRun, MainWindow, ProfileInfo, TabInfo};

/// 窗口 resize 轮询间隔（仅检测尺寸变化，与渲染解耦；PTY 输出采用事件驱动，不再定时驱动渲染）
const RESIZE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
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

// UI 线程独占的 App 弱引用：供渲染分发线程的 `invoke_from_event_loop` 回调回访 App。
//
// App 用 `Rc` 管理（非 Send），无法跨线程捕获；此处借助 thread_local 让回调在 UI 线程上
// 安全 upgrade 到 App，避免把非 Send 的 `Weak<App>` 移入其他线程。
thread_local! {
    static APP_WEAK: RefCell<Option<Weak<App>>> = const { RefCell::new(None) };
}

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
    // 窗口 resize 轮询定时器（仅检测尺寸变化，与渲染解耦）
    resize_timer: slint::Timer,
    // 按行渲染缓存（增量渲染核心）
    row_cache: RefCell<RowRenderCache>,
    // PTY 输出唤醒通道发送端：新建会话时 clone 给对应 Output Thread
    wake_tx: SyncSender<()>,
    // PTY 输出唤醒通道接收端：渲染分发线程经此通道接收唤醒信号
    wake_rx: Arc<Mutex<Receiver<()>>>,
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

        // 唤醒通道，使用容量为 1 的同步队列，避免 PTY 高频输出，通道仅作为「有数据待消费」信号传递，不传输数据数据
        let (wake_tx, wake_rx) = mpsc::sync_channel::<()>(1);

        let app = Rc::new(Self {
            main_window,
            sessions: RefCell::new(SessionManager::new()),
            profiles: ProfileManager::new(),
            tabs_model,
            profiles_model,
            cells_model,
            last_size: Cell::new((0, 0)),
            resize_timer: slint::Timer::default(),
            row_cache: RefCell::new(RowRenderCache::new()),
            wake_tx,
            wake_rx: Arc::new(Mutex::new(wake_rx)),
        });

        // 加载配置文件
        app.populate_profiles();
        // 启动异步渲染线程
        app.async_render_ui();
        // 先按目标终端网格设置窗口初始物理尺寸，使首个会话能以正确网格创建。
        app.apply_initial_window_size();
        // 创建默认会话（内部 create_session_for_profile 已 sync_tabs 并渲染）
        app.create_default_session();
        // 设置 UI 回调函数
        app.bind_callbacks();
        // 启动窗口 resize 定时器
        app.start_resize_timer();

        let weak = Rc::downgrade(&app);
        app.main_window.window().on_close_requested(move || {
            let _ = weak.upgrade();
            slint::CloseRequestResponse::HideWindow
        });

        app.main_window.show().expect("failed to show window");
        slint::run_event_loop().expect("event loop error");
    }

    fn async_render_ui(self: &Rc<Self>) {
        // 在 UI 线程中注册 thread_local 的 App 弱引用，以便在供渲染分发线程回访
        APP_WEAK.with(|slot| *slot.borrow_mut() = Some(Rc::downgrade(self)));
        // 获取管道的接收端
        let wake_rx_mux = self.wake_rx.clone();
        // 启动异步线程
        let _ = std::thread::Builder::new()
            .name("terma-render-wake".into())
            .spawn(move || {
                let wake_rx = wake_rx_mux.lock().unwrap();
                while wake_rx.recv().is_ok() {
                    let _ = slint::invoke_from_event_loop(|| {
                        APP_WEAK.with(|slot| {
                            if let Some(app) = slot.borrow().as_ref().and_then(|w| w.upgrade()) {
                                app.render();
                            }
                        });
                    });
                }
            });
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

    // 同步 Tab 列表 + 激活态标题，切/关/建 Tab 后激活会话可能变化，并强制渲染激活会话。
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
                self.main_window
                    .set_active_profile_name(s.profile_name().into());
            }
            None => {
                self.main_window.set_active_title("".into());
                self.main_window.set_active_profile_name("".into());
            }
        }
        drop(sessions);
        self.force_full_render();
    }

    // 会话变化（建/切/关 Tab）后强制全量重建激活会话整屏，将激活会话状态标为全脏，再走统一 render 路径。
    fn force_full_render(&self) {
        if let Some(state) = self.sessions.borrow().active_state() {
            state.lock().unwrap().mark_all_dirty();
        }
        self.render();
    }

    // 增量渲染：仅对脏行调用 build_row_runs，未变更行复用缓存。
    // 利用 TerminalState 的 dirty 标记 + ScreenBuffer 的 dirty_rows 追踪 +
    // RowRenderCache 按行缓存，将 O(全屏行数) 的 build_runs 降低到 O(脏行数)。
    //
    // 解析已在 Output Thread 完成，这里只读状态并渲染：
    // 锁状态快照脏行与光标 → 克隆脏行释放锁 → 锁外重建 runs → 更新 Slint Model。
    fn render(&self) {
        let state = match self.sessions.borrow().active_state() {
            Some(s) => s,
            None => {
                // 无激活会话 → 清空
                let mut cache = self.row_cache.borrow_mut();
                if !cache.prev_cells.is_empty() {
                    cache.clear();
                    self.cells_model.set_vec(Vec::new());
                }
                self.main_window.set_cursor_visible(false);
                return;
            }
        };

        // 锁状态快照：取脏行 + 光标信息，克隆脏行后立即释放锁，
        // 避免在持锁期间执行 build_row_runs 阻塞解析线程。
        let mut guard = state.lock().unwrap();
        if !guard.is_dirty() {
            // 无变更（可能是残余唤醒），直接跳过
            return;
        }
        let dirty_rows = guard.take_dirty_rows();
        let cursor_row = guard.cursor.row as i32;
        let cursor_col = guard.cursor.col as i32;
        let cursor_visible = guard.cursor_visible;
        let visible = guard.screen().visible_rows();
        let num_rows = visible.len();
        let snapshots: Vec<(usize, Vec<TerminalCell>)> = dirty_rows
            .iter()
            .filter(|&&r| r < num_rows)
            .map(|&r| (r, visible[r].clone()))
            .collect();
        guard.clear_dirty();
        drop(guard);

        // 对脏行重建 runs（锁外），未变更行复用缓存
        let mut cache = self.row_cache.borrow_mut();
        for (row_idx, row) in snapshots {
            cache.rows.insert(row_idx, build_row_runs(&row, row_idx));
        }

        // 组装所有行的 runs → CellRun
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
                fg: slint::Brush::SolidColor(slint::Color::from_rgb_u8(r.fg.0, r.fg.1, r.fg.2)),
                bg: slint::Brush::SolidColor(slint::Color::from_rgb_u8(r.bg.0, r.bg.1, r.bg.2)),
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
        self.main_window.set_cursor_row(cursor_row);
        self.main_window.set_cursor_col(cursor_col);
        self.main_window.set_cursor_visible(cursor_visible);
    }

    // 窗口 resize 轮询：仅检测尺寸变化并同步 PTY；变化会置脏标记，随后按需渲染。
    fn start_resize_timer(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.resize_timer
            .start(slint::TimerMode::Repeated, RESIZE_INTERVAL, move || {
                if let Some(app) = weak.upgrade() {
                    app.maybe_resize();
                }
            });
    }

    // UI 回调绑定
    fn bind_callbacks(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.main_window.on_new_tab(move || {
            if let Some(app) = weak.upgrade() {
                // create_default_session 内部已 sync_tabs（含强制渲染）
                app.create_default_session();
            }
        });

        let weak = Rc::downgrade(self);
        self.main_window.on_close_tab(move |id_str| {
            if let Some(app) = weak.upgrade() {
                if let Some(id) = parse_session_id(&id_str) {
                    app.close_session(id);
                }
                // 关闭后激活会话可能切换，清空按行缓存再强制渲染新激活会话
                app.row_cache.borrow_mut().clear();
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
                    // sync_tabs 末尾强制全量渲染，立即显示新会话最新状态（无白屏）
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

        if let Err(e) = self.sessions.borrow_mut().create_session(
            profile,
            title,
            rows,
            cols,
            self.wake_tx.clone(),
        ) {
            eprintln!("[terma] create session failed: {e:#}");
        }
        // 新建会话后清空按行缓存（新会话的行内容与旧会话无关），
        // sync_tabs 末尾强制渲染，立即显示新会话（空屏 + prompt）。
        self.row_cache.borrow_mut().clear();
        self.sync_tabs();
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
    // resize 会标记 dirty_rows，此处立即按需渲染，确保窗口缩放无延迟。
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
            self.render();
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
        '\u{0008}' => return vec![0x7f],           // Backspace → DEL
        '\u{0009}' => return vec![0x09],           // Tab
        '\u{000a}' => return vec![0x0d],           // Return → CR
        '\u{001b}' => return vec![0x1b],           // Escape
        '\u{007f}' => return b"\x1b[3~".to_vec(),  // Delete
        '\u{F700}' => return b"\x1b[A".to_vec(),   // Up
        '\u{F701}' => return b"\x1b[B".to_vec(),   // Down
        '\u{F702}' => return b"\x1b[D".to_vec(),   // Left
        '\u{F703}' => return b"\x1b[C".to_vec(),   // Right
        '\u{F729}' => return b"\x1b[H".to_vec(),   // Home
        '\u{F72B}' => return b"\x1b[F".to_vec(),   // End
        '\u{F72C}' => return b"\x1b[5~".to_vec(),  // PageUp
        '\u{F72D}' => return b"\x1b[6~".to_vec(),  // PageDown
        '\u{F727}' => return b"\x1b[2~".to_vec(),  // Insert
        '\u{F704}' => return b"\x1b[11~".to_vec(), // F1
        '\u{F705}' => return b"\x1b[12~".to_vec(), // F2
        '\u{F706}' => return b"\x1b[13~".to_vec(), // F3
        '\u{F707}' => return b"\x1b[14~".to_vec(), // F4
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
