//! controller — App
//!
//! 桥接 Slint MainWindow 与 SessionManager / ProfileManager。
//!
//! 设计要点：
//!   - `App` 用 `Rc` 包装，`SessionManager` / `last_size` 用 `RefCell` 提供内部可变性，
//!     Slint 的 `'static` 回调闭包通过 `Weak<App>` 回访业务状态。
//!   - PTY 输出在独立 Output Thread 中读取 → mpsc channel → UI 线程 `slint::Timer`
//!     定时 pump：vte::Parser 解析进 TerminalState → build_runs 生成 Cell 网格 → 刷新 UI。
//!     Slint 对象绝不跨线程，PTY 线程不接触 Slint。
//!   - 键盘：TerminalView 的 FocusScope 接 key-pressed → encode_key（完整键盘映射）→ PTY。
//!   - 尺寸：Timer 轮询 window().size()，变化时同步 PTY + ScreenBuffer。
//!
//! 回调（全部在 Slint UI 线程执行）：
//!   on_new_tab        → 用默认 Profile 创建 Session
//!   on_close_tab(id)  → 关闭对应 Session
//!   on_select_tab(id) → 切换激活 Session（不重建底层资源）
//!   on_select_profile(pid) → 用指定 Profile 创建 Session
//!   on_key_pressed(text, mods) → 转发到激活 Session 的 PTY

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use slint::{ComponentHandle, VecModel};

use crate::profile::{Profile, ProfileManager};
use crate::renderer::build_runs;
use crate::session::SessionManager;
use crate::session::session::TerminalSessionLike;
use crate::session::session_id::{ProfileId, SessionId};

mod ui_generated {
    slint::include_modules!();
}

pub use ui_generated::{CellRun, MainWindow, ProfileInfo, TabInfo};

/// UI 刷新轮询间隔（PTY pump + 渲染 + 尺寸检查）
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
    // 主循环定时器（持有以保活）
    _tick_timer: slint::Timer,
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

        let app = Rc::new(Self {
            main_window,
            sessions: RefCell::new(SessionManager::new()),
            profiles: ProfileManager::new(),
            tabs_model,
            profiles_model,
            cells_model,
            last_size: Cell::new((0, 0)),
            _tick_timer: slint::Timer::default(),
        });

        app.populate_profiles();
        // 先按目标终端网格设置窗口初始物理尺寸，使首个会话能以正确网格创建。
        app.apply_initial_window_size();
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

    // 同步 Tab 列表 + 激活态标题 + 终端渲染。
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
        drop(sessions);
        self.render();
    }

    // 把激活会话的 TerminalState 渲染为 Cell 网格 + 光标。
    fn render(&self) {
        let sessions = self.sessions.borrow();
        match sessions.active_state() {
            Some(state) => {
                let runs = build_runs(state);
                let cells: Vec<CellRun> = runs
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
                self.cells_model.set_vec(cells);
                self.main_window.set_cursor_row(state.cursor.row as i32);
                self.main_window.set_cursor_col(state.cursor.col as i32);
                self.main_window.set_cursor_visible(state.cursor_visible);
            }
            None => {
                self.cells_model.set_vec(Vec::new());
                self.main_window.set_cursor_visible(false);
            }
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

    // 根据 id 创建会话
    fn create_session_for_profile(self: &Rc<Self>, profile: &Profile) {
        // 用当前主窗口实际尺寸计算初始终端网格，避免新建会话回到固定 24×80。
        let (rows, cols) = self.current_grid();
        let next_index = self.sessions.borrow().len() + 1;
        let title = format!("{} {}", profile.name, next_index);
        if let Err(e) = self
            .sessions
            .borrow_mut()
            .create_session(profile, title, rows, cols)
        {
            eprintln!("[terma] create session failed: {e:#}");
        }
    }

    // 根据 id 关闭会话
    fn close_session(&self, id: SessionId) {
        self.sessions.borrow_mut().close_session(id);
    }

    // ─── 主循环：尺寸同步 + PTY pump + 渲染 ───
    fn start_tick_timer(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self._tick_timer.start(slint::TimerMode::Repeated, TICK_INTERVAL, move || {
                if let Some(app) = weak.upgrade() {
                    app.tick();
                }
            });
    }

    fn tick(&self) {
        self.maybe_resize();
        self.sessions.borrow_mut().pump_active();
        self.render();
    }

    /// 按目标终端网格设置主窗口初始尺寸（物理像素），供启动时创建首个会话使用。
    ///
    /// 使用 `WindowSize::Logical` 传入逻辑尺寸：CHAR_W / CHAR_H 均为逻辑像素度量，
    /// 平台会在窗口创建时按真实 scale_factor 换算为物理尺寸，与 `compute_grid`
    /// 的反向推导精确对应。
    fn apply_initial_window_size(&self) {
        let win = self.main_window.window();
        let logical = grid_to_logical_size(DEFAULT_ROWS, DEFAULT_COLS);
        win.set_size(slint::WindowSize::Logical(logical));
    }

    /// 当前主窗口对应的终端网格（rows, cols）。
    /// 窗口尺寸不可用（未显示 / scale 无效 / 尺寸过小）时回退到默认网格。
    fn current_grid(&self) -> (u16, u16) {
        compute_grid(self.main_window.window()).unwrap_or((DEFAULT_ROWS, DEFAULT_COLS))
    }

    // 根据窗口尺寸计算 cols/rows，变化时同步所有会话的 PTY + ScreenBuffer。
    fn maybe_resize(&self) {
        let Some((rows, cols)) = compute_grid(self.main_window.window()) else {
            return;
        };

        let last = self.last_size.get();
        if last != (rows, cols) {
            self.last_size.set((rows, cols));
            self.sessions.borrow_mut().resize_all(rows, cols);
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
/// scale 无效或终端区域非正时返回 None（交由调用方回退）。
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

/// 把键盘事件编码为写入 PTY 的字节序列（Phase 7 完整键盘映射）。
///
/// Slint 特殊键在 e.text 中以专用 Unicode 码点传递：
///   Backspace=\u{8}  Tab=\u{9}  Return=\u{a}  Escape=\u{1b}
///   Delete=\u{7f}  Up=\u{F700}  Down=\u{F701}  Left=\u{F702}  Right=\u{F703}
///   Home=\u{F729}  End=\u{F72B}  PageUp=\u{F72C}  PageDown=\u{F72D}  Insert=\u{F727}
///   F1=\u{F704}  F2=\u{F705}  F3=\u{F706}  F4=\u{F707}
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
