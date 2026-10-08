//! controller — App
//!
//! 桥接 Slint MainWindow 与 SessionManager / ProfileManager。
//!
//! 设计要点：
//!   - `App` 用 `Rc` 包装，`SessionManager` / `last_size` 用 `RefCell` 提供内部可变性，
//!     Slint 的 `'static` 回调闭包通过 `Weak<App>` 回访业务状态。
//!     彻底移除渲染轮询定时器。Slint 对象绝不跨线程，PTY 线程不接触 Slint。
//!   - 非激活会话的输出在后台静默解析、不唤醒 UI 线程，避免浪费 UI 线程资源。
//!   - 渲染采用脏标记（Dirty）+ 脏行标记（Dirty Rows）+ 按行缓存（Per-Row Cache）：
//!     UI 线程锁 `Arc<Mutex<TerminalState>>` 快照脏行，仅对脏行调用 build_row_runs，
//!     未变更行复用上一帧的缓存结果，大幅降低 CPU 占用（尤其对少量字符输出场景）。
//!   - Tab 切换、窗口 resize、新建/关闭会话等交互事件立即强制全量渲染。
//!   - Tab 切换、窗口 resize、新建/关闭会话等交互事件立即触发渲染。
//!   - 键盘：TerminalView 的 FocusScope 接 key-pressed → encode_key（完整键盘映射）→ PTY。
//!   - 尺寸：窗口 width/height 属性变更（changed 回调）即时触发，同步 PTY + ScreenBuffer。
//!
//! 回调（全部在 Slint UI 线程执行）：
//!   on_new_tab        → 用默认 Profile 创建 Session
//!   on_close_tab(id)  → 关闭对应 Session
//!   on_select_tab(id) → 切换激活 Session（立即强制全量渲染）
//!   on_select_profile(pid) → 用指定 Profile 创建 Session
//!   on_key_pressed(text, mods) → 转发到激活 Session 的 PTY
//!   on_resized()          → 窗口尺寸变化即时同步网格 + PTY + 重绘

use crate::app::events::{TerminalEvent, WindowEvent};
use crate::config::ConfigManager;
use crate::config::fonts;
use crate::renderer::buffer::RenderCache;
use crate::renderer::{build_viewport_runs, runs_to_cell_runs};
use crate::session::SessionManager;
use crate::session::session::TerminalSessionLike;
use crate::session::session_id::{ProfileId, SessionId};
use crate::terminal::TerminalState;
use slint::language::KeyboardModifiers;
use slint::{ComponentHandle, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

mod ui_generated {
    slint::include_modules!();
}

// 设置窗口（SettingsWindow）相关逻辑：懒创建、回调绑定、配置/Profile CRUD 与字体搜索
mod settings;

pub use ui_generated::{
    CellRun, FontEntry, GeneralSettingsInfo, MainWindow, ProfileEditInfo, ProfileInfo,
    SettingsWindow, TabInfo,
};

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
/// Tab 最大宽度（逻辑像素）：空间充足时标签保持的宽度上限，不再拉伸
const TAB_W_MAX: i32 = 160;
/// Tab 最小下限（逻辑像素）：窗口极窄时均分收缩的最终下限，
/// 保证标题区与 × 按钮仍可辨识（常规下限 80 不再截断收缩，见 update_tab_width）
const TAB_W_MIN: i32 = 40;
/// Tab 栏右侧固定占位（逻辑像素）：+ 按钮(30) + ▼ 按钮(30) + 布局 spacing(2×3)
const BABAR_RESERVED_W: i32 = 66;
/// 相邻标签之间的间距（逻辑像素，与 tab_bar.slint 的 spacing 一致）
const TAB_GAP: i32 = 2;

// UI 线程独占的 App 弱引用：供渲染分发线程的 `invoke_from_event_loop` 回调回访 App。
thread_local! {
    static WEAK_APP: RefCell<Option<Weak<RefCell<App>>>> = const { RefCell::new(None) };
}

/// 单个会话（Tab）独立的滚动视图状态。
///
/// 滚动状态必须按 Tab 各自维护：滚动偏移量、滚轮累加器、冻结锚点等互不影响，
/// 切换 Tab 时各自恢复、各自冻结，避免 A 标签页的滚动位置串扰到 B 标签页。
#[derive(Debug, Clone, Copy, Default)]
pub struct ScrollState {
    /// 滚动偏移量：从底部向上滚动的行数（0 = 跟随最新输出）
    pub offset: usize,
    /// 上次设置 offset 时 scrollback 的长度，用于滚动状态下冻结视图
    pub anchor_len: usize,
    /// 滚轮像素累加器：累积满一个字符行高（CHAR_H）才触发一次整行滚动
    pub acc: f32,
}

/// 应用控制器：持有窗口、会话管理器、Profile 管理器与 UI Model。
pub struct App {
    main_window: MainWindow,
    sessions: SessionManager,
    // 配置管理器
    config: ConfigManager,
    // 设置窗口
    settings_window: Option<SettingsWindow>,
    // 标签页模型
    tabs_model: Rc<VecModel<TabInfo>>,
    // Profile 模型
    profiles_model: Rc<VecModel<ProfileInfo>>,
    // 终端 Cell 渲染模型（Run 列表）
    cells_model: Rc<VecModel<CellRun>>,
    // 上次同步给 PTY 的 (rows, cols)，用于检测窗口尺寸变化
    last_size: Cell<(u16, u16)>,
    // 渲染缓存：按行缓存渲染项，脏行重建、未变更行复用
    render_cache: RenderCache,
    // 每个会话（Tab）独立的滚动视图状态，以 SessionId 索引，互不影响
    scroll_states: RefCell<HashMap<SessionId, ScrollState>>,
    // 字体下拉框完整候选列表缓存（内置 + 系统字体），供搜索过滤时复用，
    font_entries: RefCell<Vec<FontEntry>>,
    // 命令通道发送端
    event_tx: SyncSender<WindowEvent>,
    // 命令通道接收端
    event_rx: Arc<Mutex<Receiver<WindowEvent>>>,
}

impl App {
    pub fn run() {
        let tabs_model = Rc::new(VecModel::default());
        let profiles_model = Rc::new(VecModel::default());
        let cells_model = Rc::new(VecModel::default());

        // 窗口对象
        let main_window = MainWindow::new().expect("failed to create MainWindow");
        main_window.set_tabs(tabs_model.clone().into());
        main_window.set_profiles(profiles_model.clone().into());
        main_window.set_cells(cells_model.clone().into());

        // 事件管道
        let (event_tx, event_rx) = mpsc::sync_channel(32);

        let app = Rc::new(RefCell::new(Self {
            main_window,
            sessions: SessionManager::new(),
            config: ConfigManager::new(),
            settings_window: None,
            tabs_model,
            profiles_model,
            cells_model,
            last_size: Cell::new((0, 0)),
            render_cache: RenderCache::new(),
            scroll_states: RefCell::new(HashMap::new()),
            font_entries: RefCell::new(Vec::new()),
            event_tx,
            event_rx: Arc::new(Mutex::new(event_rx)),
        }));

        // 初始化
        app.borrow().app_init();
        // 启动异步事件线程
        Self::async_recv_event(&app);
        // 启动默认会话
        app.borrow_mut().on_create_default_session();

        // 显示主窗口
        app.borrow()
            .main_window
            .show()
            .expect("failed to show window");
        slint::run_event_loop().expect("event loop error");
    }

    // 应用初始化
    fn app_init(&self) {
        // 应用配置
        self.apply_profiles();
        // 应用字体
        self.apply_font();
        // 应用初始窗口尺寸
        self.apply_initial_window_size();
        // 设置 UI 回调函数
        self.bind_callbacks();
    }

    // 设置终端字体, 配置为空时回退默认字体（JetBrains Mono）。
    fn apply_font(&self) {
        let mut family = self.config.general().font_family.clone();
        if family.is_empty() {
            family = fonts::DEFAULT_FONT_FAMILY.to_string();
        }
        self.main_window.set_font_family(family.into());
    }

    // 设置终端配置
    fn apply_profiles(&self) {
        let items: Vec<ProfileInfo> = self
            .config
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

    // 设置终端窗口初始尺寸
    fn apply_initial_window_size(&self) {
        let logical = slint::LogicalSize::new(
            (DEFAULT_COLS as i32 * CHAR_W + H_PADDING) as f32,
            (DEFAULT_ROWS as i32 * CHAR_H + CHROME_H + V_PADDING) as f32,
        );
        self.main_window
            .window()
            .set_size(slint::WindowSize::Logical(logical));
    }

    // 同步 Tab 列表 + 激活态标题，切/关/建 Tab 后激活会话可能变化，并强制渲染激活会话。
    fn sync_tabs(&self) {
        // let sessions = self.sessions.borrow();
        let tabs: Vec<TabInfo> = self
            .sessions
            .sessions_in_order()
            .iter()
            .map(|s| TabInfo {
                id: s.id().to_slint_string().into(),
                title: s.title().into(),
                active: Some(s.id()) == self.sessions.active_id(),
                profile_name: s.profile_name().into(),
            })
            .collect();
        self.tabs_model.set_vec(tabs);

        match self.sessions.active_session() {
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
        // drop(sessions);
        // 标签数变化后重新计算标签宽度
        self.update_tab_width();
        self.force_full_render();
    }

    /// 标签宽度计算并注入（逻辑像素）：
    ///   单标签宽度 = clamp((窗口宽 − 固定占位 − 间距) / n, 40, 240)
    /// - 空间充足：保持上限 240，右侧由弹性占位保留空白，不拉伸填满窗口
    /// - 空间不足：均分可用宽度**持续收缩**（可低于常规下限 80），直至硬性下限 40，
    ///   保证窗口始终可以继续缩小（放不下的部分由 TabBar 的 clip 容器裁剪）
    /// 宽度计算放在 Rust 侧而非 Slint 绑定，避免锚定窗口宽度形成布局依赖环。
    fn update_tab_width(&self) {
        let n = self.sessions.len();
        if n == 0 {
            return;
        }
        let win = self.main_window.window();
        let scale = win.scale_factor();
        if scale <= 0.0 {
            return;
        }
        let win_w = win.size().to_logical(scale).width as i32;
        if win_w <= 0 {
            return;
        }
        let avail = (win_w - BABAR_RESERVED_W - TAB_GAP * (n as i32 - 1)).max(0);
        let w = (avail / n as i32).clamp(TAB_W_MIN, TAB_W_MAX) as f32;
        // 值未变化时跳过注入，避免无谓的布局重算
        if (self.main_window.get_tab_width() - w).abs() >= 1.0 {
            self.main_window.set_tab_width(w);
        }
    }

    // 会话变化（建/切/关 Tab）后强制全量重建激活会话整屏，将激活会话状态标为全脏，再走统一 render 路径。
    fn force_full_render(&self) {
        if let Some(state) = self.sessions.active_state() {
            state.lock().unwrap().mark_all_dirty();
        }
        self.render();
    }

    // 脏行渲染，利用 TerminalState 的 dirty 标记有脏行时才触发渲染
    fn render(&self) {
        let state = match self.sessions.active_state() {
            Some(s) => s,
            None => {
                self.clear_view();
                return;
            }
        };

        let mut guard = state.lock().unwrap();
        if !guard.is_dirty() {
            // 无变更（可能是残余唤醒），直接跳过
            return;
        }
        self.render_locked(&guard);
        // 渲染完成后清除脏标记：否则 dirty 永远为 true，is_dirty 守卫形同虚设
        guard.clear_dirty();
    }

    // 滚动渲染入口，滚动偏移变化时强制重绘（此时终端状态可能不脏，不能走 is_dirty 守卫）。
    fn render_viewport(&self) {
        let state = match self.sessions.active_state() {
            Some(s) => s,
            None => {
                self.clear_view();
                return;
            }
        };
        let mut guard = state.lock().unwrap();
        self.render_locked(&guard);
        guard.clear_dirty();
    }

    // 无激活会话时清空视图。
    fn clear_view(&self) {
        self.render_cache.clear();
        self.cells_model.set_vec(Vec::new());
        self.main_window.set_cursor_visible(false);
    }

    // 实际执行渲染
    // 1，scroll_offset 决定走「跟随模式（增量缓存）」还是「滚动模式（视口拼接）」。
    // 2，增量渲染，只渲染新增的内容
    // 3，全量渲染，全量渲染屏幕的所有内容
    fn render_locked(&self, guard: &std::sync::MutexGuard<'_, TerminalState>) {
        let screen = guard.screen();
        let sb_len = screen.scrollback.len();

        // 冻结内容，滚动状态下新历史行到达时，把偏移量随底部下移，保持用户正在查看的内容不动。
        // 偏移量与锚点按激活会话独立维护，切换 Tab 各自恢复、互不串扰。
        let offset = self.settle_scroll(sb_len);

        if offset == 0 {
            // 跟随模式：仅重建脏行，未变更行复用缓存
            let dirty_rows = guard.take_dirty_rows();
            if dirty_rows.len() > 0 {
                self.render_cache.update(dirty_rows, screen.visible_rows());
                self.cells_model
                    .set_vec(self.render_cache.build_cell_runs());
            }
        } else {
            // 滚动模式：全量重建视口（scrollback 历史 + 当前屏拼接），丢弃脏行标记
            let _ = guard.take_dirty_rows();
            self.cells_model
                .set_vec(runs_to_cell_runs(&build_viewport_runs(screen, offset)));
        };

        // 光标：滚动状态下隐藏（实时光标位于视口之外）
        if offset == 0 {
            self.main_window.set_cursor_row(guard.cursor.row as i32);
            self.main_window.set_cursor_col(guard.cursor.col as i32);
            self.main_window.set_cursor_visible(guard.cursor.visible);
        } else {
            self.main_window.set_cursor_visible(false);
        }

        // 滚动条状态（无历史时隐藏滚动条）
        self.main_window.set_has_scrollback(sb_len > 0);
        if sb_len > 0 {
            // 滚动条的位置 = offset / sb_len。0 = 底部（最新），1 = 顶部（最老历史）
            self.main_window
                .set_scroll_ratio(offset as f32 / sb_len as f32);
            // 滑块大小比例 = 可见行数 / 总行数（scrollback + 可见行），控制滚动条滑块的视觉长度——历史越长，滑块越短。max(1) 防止除零
            let rows = screen.rows.max(1);
            self.main_window
                .set_viewport_ratio(rows as f32 / (sb_len + rows) as f32);
        }
    }

    // 冻结滚动视图并返回激活会话最终生效的滚动偏移量。
    // 滚动状态下（offset > 0）scrollback 增长时，把偏移量随底部同步下移，
    // 保持用户正在查看的历史内容在视口中不动；跟随模式（offset == 0）直接返回 0。
    // 同时刷新锚点长度为当前 scrollback 长度，供下一次增长量计算。
    fn settle_scroll(&self, sb_len: usize) -> usize {
        let id = match self.sessions.active_id() {
            Some(id) => id,
            None => return 0,
        };
        let mut states = self.scroll_states.borrow_mut();
        let st = states.entry(id).or_default();
        let current = st.offset;
        let offset = if current > 0 {
            let grew = sb_len.saturating_sub(st.anchor_len);
            (current + grew).min(sb_len)
        } else {
            0
        };
        st.offset = offset;
        st.anchor_len = sb_len;
        offset
    }

    // 设置新的滚动偏移量(scroll_offset)，更新偏移与锚点，并按需重绘
    //  scroll_offset == 0，跟随模式，查看最新数据
    //  scroll_offset >  0，滚动模式，查看历史数据
    fn commit_scroll(&self, offset: usize, sb_len: usize) {
        let id = match self.sessions.active_id() {
            Some(id) => id,
            None => return,
        };
        let offset = offset.min(sb_len);
        // 更新激活会话的滚动状态；滚动位置未变则直接跳过，避免无谓重绘。
        let (prev, changed) = {
            let mut states = self.scroll_states.borrow_mut();
            let st = states.entry(id).or_default();
            let prev = st.offset;
            if offset == prev {
                (prev, false)
            } else {
                st.offset = offset;
                st.anchor_len = sb_len;
                (prev, true)
            }
        };
        if !changed {
            return;
        }
        if prev > 0 && offset == 0 {
            // 从滚动模式回到了跟随模式，全量重建。
            self.force_full_render();
        } else {
            // 滚动模式，滚动位置变了，仅重建视口
            self.render_viewport();
        }
    }

    // 读取当前激活会话的 scrollback 长度（无会话时为 0）。
    fn scrollback_len(&self) -> usize {
        self.sessions
            .active_state()
            .map(|s| s.lock().unwrap().screen().scrollback.len())
            .unwrap_or(0)
    }

    // UI 回调绑定
    fn bind_callbacks(&self) {
        // 关闭主窗口时，只是隐藏界面
        self.main_window
            .window()
            .on_close_requested(move || slint::CloseRequestResponse::HideWindow);

        // 新建 Tab
        let command_tx = self.event_tx.clone();
        self.main_window.on_new_tab(move || {
            command_tx.send(WindowEvent::NewTab).unwrap();
        });

        // 关闭 Tab
        let command_tx = self.event_tx.clone();
        self.main_window.on_close_tab(move |id_str| {
            if let Some(id) = parse_session_id(&id_str) {
                command_tx.send(WindowEvent::CloseTab(id)).unwrap();
            }
        });

        // 切换 Tab
        let command_tx = self.event_tx.clone();
        self.main_window.on_select_tab(move |id_str| {
            if let Some(id) = parse_session_id(&id_str) {
                command_tx.send(WindowEvent::SelectTab(id)).unwrap();
            }
        });

        // 创建新会话
        let command_tx = self.event_tx.clone();
        self.main_window.on_select_profile(move |pid_str| {
            let pid = ProfileId::new(pid_str.to_string());
            command_tx.send(WindowEvent::NewTabForProfile(pid)).unwrap();
        });

        // 窗口尺寸变化
        let command_tx = self.event_tx.clone();
        self.main_window.on_resized(move || {
            command_tx.send(WindowEvent::Resize).unwrap();
        });

        // 键盘回调，处理键盘输入转发到激活会话的 PTY
        let command_tx = self.event_tx.clone();
        self.main_window.on_key_pressed(move |text, mods| {
            command_tx.send(WindowEvent::KeyInput(text, mods)).unwrap();
            true
        });

        // IME 提交回调
        let command_tx = self.event_tx.clone();
        self.main_window.on_ime_commit(move |text| {
            command_tx.send(WindowEvent::ImeCommit(text)).unwrap();
        });

        // 滚动：鼠标滚轮
        let command_tx = self.event_tx.clone();
        self.main_window.on_scroll_delta(move |dy| {
            command_tx.send(WindowEvent::ScrollDelta(dy)).unwrap();
        });

        // 滚动：滚动条拖动/点击
        let command_tx = self.event_tx.clone();
        self.main_window.on_scroll_to(move |ratio| {
            command_tx.send(WindowEvent::ScrollTo(ratio)).unwrap();
        });

        // 打开设置窗口
        let command_tx = self.event_tx.clone();
        self.main_window.on_open_settings(move || {
            command_tx.send(WindowEvent::OpenSettings).unwrap();
        });
    }

    // 响应窗口回调事件
    fn on_event(&mut self, event: WindowEvent) {
        match event {
            WindowEvent::Pty(TerminalEvent::Data) => self.render(),
            // 新建 Tab
            WindowEvent::NewTab => self.on_create_default_session(),
            // 新建 Tab 会话
            WindowEvent::NewTabForProfile(pid) => self.on_create_session_for_profile(&pid),
            // 关闭 Tab 会话
            WindowEvent::CloseTab(sid) => self.on_close_session(sid),
            // 切换 Tab 会话
            WindowEvent::SelectTab(sid) => self.on_select_tab(sid),
            // 窗口尺寸变化
            WindowEvent::Resize => self.on_resized(),
            // 鼠标滚轮滚动
            WindowEvent::ScrollDelta(dy) => self.on_scroll_offset_by(dy),
            // 鼠标拖转滑块
            WindowEvent::ScrollTo(ratio) => self.on_scroll_offset_to(ratio),
            // 键盘输入
            WindowEvent::KeyInput(text, mods) => self.on_key_input(text, mods),
            // IME 提交
            WindowEvent::ImeCommit(text) => self.on_ime_commit(text),

            // 打开设置窗口
            WindowEvent::OpenSettings => self.on_open_settings(),
            // 关闭设置窗口
            WindowEvent::CloseSettings => self.on_close_settings(),
            // 保存设置
            WindowEvent::SaveSettings(settings) => self.on_save_settings(settings),
            // 保存配置文件
            WindowEvent::SaveProfile(profile) => self.on_save_profile(profile),
            // 添加配置文件
            WindowEvent::AddProfile() => self.on_add_profile(),
            // 删除配置文件
            WindowEvent::DelProfile(id) => self.on_delete_profile(id),
            // 模糊搜索字体
            WindowEvent::FilterFonts(keyword, current) => self.on_filter_fonts(&keyword, &current),
        }
    }

    // 创建默认会话
    fn on_create_default_session(&mut self) {
        let default_id = self.config.default_profile_id().clone();
        self.on_create_session_for_profile(&default_id);
    }

    // 根据 Profile 创建会话
    fn on_create_session_for_profile(&mut self, profile_id: &ProfileId) {
        let profile = match self.config.get_profile(profile_id) {
            Some(p) => p.clone(),
            None => return,
        };

        let (rows, cols) = self.current_grid();
        self.last_size.set((rows, cols));
        let title = format!("{} {}", profile.name, self.sessions.len() + 1);

        match self
            .sessions
            .create_session(&profile, title, rows, cols, self.event_tx.clone())
        {
            Ok(id) => {
                // 为新建会话登记独立的滚动状态（每 Tab 互不影响）
                self.scroll_states
                    .borrow_mut()
                    .insert(id, ScrollState::default());
            }
            Err(e) => eprintln!("[terma] create session failed: {e:#}"),
        }
        self.render_cache.clear();
        self.sync_tabs();
    }

    // 根据 id 关闭会话
    fn on_close_session(&mut self, id: SessionId) {
        // 关闭会话
        self.sessions.close_session(id);
        self.scroll_states.borrow_mut().remove(&id);

        // 清理缓存
        self.render_cache.clear();
        // 同步标签
        self.sync_tabs();
        // 关闭的是最后一个会话，则退出整个应用
        if self.sessions.is_empty() {
            let _ = slint::quit_event_loop();
        }
    }

    // 根据 id 切换会话
    fn on_select_tab(&mut self, id: SessionId) {
        // 设置活动会话
        self.sessions.set_active_session(id);
        // 清空缓存
        self.render_cache.clear();
        // 刷新标签 UI
        self.sync_tabs();
    }

    // 窗口尺寸变化回调处理：由 `changed width/height` 即时触发。
    // 根据窗口尺寸重新计算 cols/rows，变化时同步所有会话的 PTY + ScreenBuffer。
    fn on_resized(&mut self) {
        let (rows, cols) = self.current_grid();

        let last = self.last_size.get();
        if last != (rows, cols) {
            // 窗口尺寸变，联动调整标签宽度
            self.update_tab_width();
            self.last_size.set((rows, cols));
            self.sessions.resize_all(rows, cols);
            // 清空渲染缓存，避免复用旧尺寸的缓存数据。
            self.render_cache.clear();
            // 强制全量重绘终端
            self.force_full_render();
        }
    }

    // 滚轮滚动触发执行，按行数滚动：正 = 向上（进入历史），负 = 向下（回到最新）。
    // 滚动像素级累加，累积满一个字符行高（CHAR_H）触发一次整行滚动。
    fn on_scroll_offset_by(&self, dy: f32) {
        let sb_len = self.scrollback_len();
        let id = match self.sessions.active_id() {
            Some(id) => id,
            None => return,
        };

        // 滚轮像素累加器按会话独立维护：累积满一个字符行高才换算为整行滚动，
        // 未满一行的余量留在本会话的累加器中，切换 Tab 后互不影响。
        let target = {
            let mut states = self.scroll_states.borrow_mut();
            let st = states.entry(id).or_default();
            let mut acc = st.acc + dy;
            let mut lines: i64 = 0;
            while acc >= CHAR_H as f32 {
                // 向上滚动
                acc -= CHAR_H as f32;
                lines += 1;
            }
            while acc <= -(CHAR_H as f32) {
                // 向下滚动
                acc += CHAR_H as f32;
                lines -= 1;
            }
            st.acc = acc;
            if lines == 0 {
                None
            } else {
                // 当前滚动位置加上要滚动的行数，得到新目标行位置，使用 clamp 确保 offset 在 [0, sb_len] 范围内
                Some((st.offset as i64 + lines).clamp(0, sb_len as i64) as usize)
            }
        };

        // 如果滚动超过一个行高度，就重绘窗口
        if let Some(offset) = target {
            self.commit_scroll(offset, sb_len);
        }
    }

    // 滚动条跳转：ratio ∈ [0,1]（0 = 底部，1 = 顶部）。
    fn on_scroll_offset_to(&self, ratio: f32) {
        let sb_len = self.scrollback_len();
        let offset = (ratio.clamp(0.0, 1.0) * sb_len as f32).round() as usize;
        self.commit_scroll(offset, sb_len);
    }

    // 键盘输入
    fn on_key_input(&mut self, text: SharedString, mods: KeyboardModifiers) {
        let input_data = encode_key(&text, mods);
        if !input_data.is_empty() {
            if let Err(e) = self.sessions.write_to_active(&input_data) {
                eprintln!("[terma] key input write error: {e}");
            }
        }
    }

    // IME 回调提交文本, 输入法完成选词后提交的完整字符串（如中文）, 直接转为 UTF-8 字节写入激活会话的 PTY。
    fn on_ime_commit(&mut self, text: SharedString) {
        let ime_data = text.as_bytes().to_vec();
        if !ime_data.is_empty() {
            if let Err(e) = self.sessions.write_to_active(&ime_data) {
                eprintln!("[terma] ime commit write error: {e}");
            }
        }
    }

    // 获取当前窗终端网格行列数
    fn current_grid(&self) -> (u16, u16) {
        let win = self.main_window.window();
        let scale = win.scale_factor();
        if scale <= 0.0 {
            return (DEFAULT_ROWS, DEFAULT_COLS);
        }
        let logical = win.size().to_logical(scale);
        let term_w = logical.width as i32 - H_PADDING;
        let term_h = logical.height as i32 - CHROME_H - V_PADDING;
        if term_w <= 0 || term_h <= 0 {
            return (DEFAULT_ROWS, DEFAULT_COLS);
        }
        let cols = (term_w / CHAR_W).clamp(1, MAX_COLS) as u16;
        let rows = (term_h / CHAR_H).clamp(1, MAX_ROWS) as u16;
        (rows, cols)
    }

    // 异步接收命令
    pub fn async_recv_event(app: &Rc<RefCell<App>>) {
        let weak_app = Rc::downgrade(app);
        WEAK_APP.with(|ptr| *ptr.borrow_mut() = Some(weak_app));
        // 获取管道的接收端
        let command_rx = app.borrow().event_rx.clone();
        // 启动事件异步线程
        let _ = std::thread::Builder::new()
            .name("terma_event".into())
            .spawn(move || {
                let rx = command_rx.lock().unwrap();
                loop {
                    if let Ok(event) = rx.recv() {
                        let _ = slint::invoke_from_event_loop(|| {
                            WEAK_APP.with(|ptr| {
                                if let Some(app) = ptr.borrow().as_ref().unwrap().upgrade() {
                                    app.borrow_mut().on_event(event)
                                }
                            });
                        });
                    }
                }
            });
    }
}

/// 把 Slint 传回的 id 字符串解析回 SessionId(u64)。
fn parse_session_id(s: &SharedString) -> Option<SessionId> {
    s.as_str().parse::<u64>().ok().map(SessionId)
}

/// 把键盘事件编码为写入 PTY 的字节序列
fn encode_key(text: &SharedString, mods: KeyboardModifiers) -> Vec<u8> {
    let s = text.as_str();
    let c = match s.chars().next() {
        Some(c) => c,
        None => return Vec::new(),
    };

    // 特殊特殊键处理，键盘输入部分特殊字符与终端标准输入不一致，两者需要转换，例如
    //    键盘输入：按下退格键 Backspace 输入的是 BS(0x08)，期望删除光标前面一个字符
    //    终端期望：现代终端标准删除光标前面一个字符，期望控制符是 DEL(0x7f)
    //    问题出现了两者对不上，需要有一个显示的转换，把 BS(0x08) 转换为 DEL(0x7f)
    // 出现这种问题的原因有很多，历史遗留、键盘布局调整、终端控制字符约定等
    match c {
        '\u{0008}' => return vec![0x7f],           // Backspace → DEL
        '\u{0009}' => return vec![0x09],           // Tab 快速返回
        '\u{000a}' => return vec![0x0d],           // Return → CR
        '\u{001b}' => return vec![0x1b],           // Escape 快速返回
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

    // 去除 MacOS NSEvent 机制导致的无意义控制字符，跳过不处理
    if c.is_control() && !s.chars().any(|ch| ch >= '\u{20}') {
        return Vec::new();
    }

    // 输入ASCII 控制字符（Ctrl + 字母）
    if mods.meta {
        if c.is_ascii_alphabetic() {
            return vec![(c as u8) & 0x1f];
        }
        return vec![];
    }

    // 普通可见字符 → utf8 字节
    s.as_bytes().to_vec()
}
