//! controller — App
//!
//! 桥接 Slint MainWindow 与 SessionManager / ProfileManager。
//!
//! 设计要点：
//!   - `App` 用 `Rc` 包装，`SessionManager` / `last_size` 用 `RefCell` 提供内部可变性，
//!     Slint 的 `'static` 回调闭包通过 `Weak<App>` 回访业务状态。
//!     彻底移除渲染轮询定时器。Slint 对象绝不跨线程，PTY 线程不接触 Slint。
//!   - 非激活会话的输出在后台静默解析、不唤醒 UI 线程，避免浪费 UI 线程资源。
//!   - 渲染采用脏标记（Dirty）+ 脏行标记（Dirty Rows）+ 按行缓存（Per-Row Cache）+
//!     会话私有 cells model 增量更新：每个会话持有独立的 `VecModel<CellRun>`，
//!     仅对脏行做 set_row_data/insert/remove 级别的增量更新，未变更行零转换零触碰；
//!     切换 Tab 仅换挂 model（零拷贝），彻底移除每帧全量转换 + 全量 set_vec 的开销。
//!     Tab 切换、窗口 resize、新建/关闭会话等交互事件立即强制全量渲染。
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
use crate::config::ColorScheme;
use crate::session::session_id::{ProfileId, SessionId};
use crate::session::{RenderOutput, ScrollAction, SessionManager};
use slint::language::KeyboardModifiers;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

mod ui_generated {
    slint::include_modules!();
}

// 设置窗口（SettingsWindow）相关逻辑：懒创建、回调绑定、配置/Profile CRUD 与字体搜索
mod settings;

pub use ui_generated::{
    CellRun, ColorSchemeEntry, FontEntry, GeneralSettingsInfo, MainWindow, ProfileEditInfo,
    ProfileInfo, SaveAsValidation, SchemeColors, SettingsWindow, TabInfo,
};

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

/// 应用控制器：持有窗口、会话管理器、Profile 管理器与 UI Model。
pub struct App {
    main_window: MainWindow,
    // 标签页模型
    tabs_model: Rc<VecModel<TabInfo>>,
    // Profile 模型
    profiles_model: Rc<VecModel<ProfileInfo>>,
    // 终端 Cell 渲染模型，二维数组：外层为行，内层为该行 CellRun
    cells_model: Rc<VecModel<ModelRc<CellRun>>>,
    // 会话切换/新建/关闭时重新挂载对应会话的 model，内容零拷贝零重算。
    attached_cells: Cell<Option<SessionId>>,
    // 会话管理器
    sessions: SessionManager,
    // 配置管理器
    config: ConfigManager,
    // 设置窗口
    settings_window: Option<SettingsWindow>,
    // 上次同步给 PTY 的 (rows, cols)，用于检测窗口尺寸变化
    last_size: Cell<(u16, u16)>,
    // 终端字体宽度
    char_w: Cell<f32>,
    // 终端字体高度
    char_h: Cell<f32>,
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
        let cells_model: Rc<VecModel<ModelRc<CellRun>>> = Rc::new(VecModel::default());

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
            attached_cells: Cell::new(None),
            last_size: Cell::new((0, 0)),
            char_w: Cell::new(8.0),
            char_h: Cell::new(16.0),
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
        // 应用配色方案（窗口背景/光标色）
        self.apply_window_colors();
        // 应用初始窗口尺寸
        self.apply_initial_window_size();
        // 设置 UI 回调函数
        self.bind_callbacks();
    }

    // 终端字体设置, 配置为空时回退默认字体（JetBrains Mono）。
    fn apply_font(&self) {
        let general = self.config.general();
        let mut family = general.font_family.clone();
        if family.is_empty() {
            family = fonts::DEFAULT_FONT_FAMILY.to_string();
        }
        let size = general.font_size.clamp(1, 100);
        let metrics = fonts::measure_monospace(&family, size);

        self.char_w.set(metrics.char_w);
        self.char_h.set(metrics.char_h);
        self.main_window.set_font_family(family.into());
        self.main_window.set_font_size(size as f32);
        self.main_window.set_char_w(metrics.char_w);
        self.main_window.set_char_h(metrics.char_h);
    }

    // 当前生效配色方案（供窗口颜色注入与会话创建使用）。
    fn active_color_scheme(&self) -> ColorScheme {
        self.config.active_color_scheme()
    }

    // 把当前配色方案的背景色 + 全局光标色注入主窗口（终端区背景 + 光标色）。
    // 选区色当前仅存于方案数据中（尚无选区渲染，预留）。
    fn apply_window_colors(&self) {
        let p = self.active_color_scheme();
        self.main_window.set_bg_color(rgb_to_color(p.background));
        let cursor = self.config.general().cursor_color.color;
        self.main_window
            .set_cursor_color(slint::Color::from_argb_u8(
                0x44, cursor.0, cursor.1, cursor.2,
            ));
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
        let char_w = self.char_w.get();
        let char_h = self.char_h.get();
        let logical = slint::LogicalSize::new(
            DEFAULT_COLS as f32 * char_w + H_PADDING as f32,
            DEFAULT_ROWS as f32 * char_h + CHROME_H as f32 + V_PADDING as f32,
        );
        self.main_window
            .window()
            .set_size(slint::WindowSize::Logical(logical));
    }

    // 同步 Tab 列表 + 激活态标题，切/关/建 Tab 后激活会话可能变化，并强制渲染激活会话。
    fn sync_tabs(&self) {
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
        // 标签数变化后重新计算标签宽度
        self.update_tab_width();
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

    // 强制渲染激活会话一帧（Tab 切换 / 新建 / 关闭会话 / 回到跟随模式等交互）：
    // 忽略脏标记守卫立即刷新，复用目标会话的独立缓存做增量重建，无需清空任何全局缓存。
    fn render_active_force(&self) {
        self.render_active(true);
    }

    // PTY 输出触发：依赖 state 脏标记守卫，无变更则跳过渲染。
    fn render(&self) {
        self.render_active(false);
    }

    // 滚动视口触发：滚动偏移变化时强制重绘（此时终端状态可能不脏，不能走 is_dirty 守卫）。
    fn render_viewport(&self) {
        self.render_active(true);
    }

    // 渲染激活会话的口，把「计算」下沉到会话的 Renderer 对象，App 只负责把挂在激活对象
    fn render_active(&self, force: bool) {
        let Some(session) = self.sessions.active_session() else {
            self.clear_view();
            return;
        };

        // 首次渲染 / 会话切换，把窗口 cells 挂载到该会话私有的 model。
        if self.attached_cells.get() != Some(session.id()) {
            self.main_window.set_cells(session.cells_model().into());
            self.attached_cells.set(Some(session.id()));
        }

        // 会话内部一次性加锁 state，完成滚动冻结、终端渲染、光标与滚动条计算，
        if let Some(output) = session.render_frame(force) {
            // 光标（滚动模式下 output.cursor.visible 已置 false）
            self.main_window.set_cursor_row(output.cursor.row);
            self.main_window.set_cursor_col(output.cursor.col);
            self.main_window.set_cursor_visible(output.cursor.visible);
            // 滚动条：仅在有 scrollback 时更新（无历史时保持现状，与原逻辑一致）
            if let Some(scroll) = output.scroll {
                self.main_window.set_has_scrollback(true);
                // 滚动条位置 = offset / sb_len。0 = 底部（最新），1 = 顶部（最老历史）
                self.main_window.set_scroll_ratio(scroll.position);
                // 滑块大小比例 = 可见行数 / 总行数，历史越长滑块越短
                self.main_window.set_viewport_ratio(scroll.viewport);
            }
        }
    }

    // 无激活会话时清空视图：挂载占位空 model 并隐藏光标。
    fn clear_view(&self) {
        if self.attached_cells.get().is_some() {
            self.main_window.set_cells(self.cells_model.clone().into());
            self.attached_cells.set(None);
        }
        self.main_window.set_cursor_visible(false);
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
            // PTY 输出数据
            WindowEvent::Pty(TerminalEvent::Data) => self.render(),
            // PTY 输出结束（子进程退出 EOF 或不可恢复读取错误）
            WindowEvent::Pty(TerminalEvent::ChildExited(sid)) => self.on_close_session(sid),
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
            // 配色方案选中（回填编辑器与预览）
            WindowEvent::SchemeSelected(name) => self.on_scheme_selected(name),
            // 配色草稿变化（解析回填预览色板）
            WindowEvent::SchemeColorsChanged(colors) => self.on_scheme_colors_changed(colors),
            // 另存为自定义配色方案
            WindowEvent::SaveCustomScheme(colors) => self.on_save_custom_scheme(colors),
            // 删除自定义配色方案
            WindowEvent::DeleteCustomScheme(name) => self.on_delete_custom_scheme(name),
            // 光标色草稿变化（解析回填光标色预览）
            WindowEvent::CursorColorChanged(hex) => self.on_cursor_color_changed(hex),
            // 「另存为」名称校验
            WindowEvent::CheckSchemeName(name) => self.on_check_scheme_name(name),
        }
    }

    // 创建默认会话
    fn on_create_default_session(&mut self) {
        let default_id = self.config.default_profile_id().clone();
        self.on_create_session_for_profile(&default_id);
    }

    // 根据 Profile 创建会话，并把新会话设置为激活状态，重新渲染标签和终端
    fn on_create_session_for_profile(&mut self, profile_id: &ProfileId) {
        let Some(profile) = self.config.get_profile(profile_id) else {
            return;
        };

        let (rows, cols) = self.current_grid();
        let event_tx = self.event_tx.clone();
        let color_scheme = self.active_color_scheme();
        if self
            .sessions
            .create_session(&profile, rows, cols, color_scheme, event_tx)
            .is_ok()
        {
            self.sync_tabs();
            self.render_active_force();
        }
    }

    // 根据 id 关闭会话
    fn on_close_session(&mut self, id: SessionId) {
        // 幂等守卫，会话已不存在（可能已被主动关闭，或 PTY 退出事件重复到达），
        if !self.sessions.contains(id) {
            return;
        }
        let active_id = self.sessions.active_id();
        // 关闭会话
        self.sessions.close_session(id);
        // 同步标签
        self.sync_tabs();
        // 如果关闭的是激活终端，则切换到相邻会话并复用其独立缓存重新渲染
        if active_id == Some(id) {
            self.render_active_force();
        }
        // 关闭的是最后一个会话，则退出整个应用
        if self.sessions.is_empty() {
            let _ = slint::quit_event_loop();
        }
    }

    // 根据 id 切换会话
    fn on_select_tab(&mut self, id: SessionId) {
        if self.sessions.active_id() != Some(id) {
            // 设置激活会话
            self.sessions.set_active_session(id);
            // 刷新标签
            self.sync_tabs();
            // 渲染激活终端
            self.render_active_force();
        }
    }

    // 窗口尺寸变化处理，重新计算窗口尺寸，同步所有会话的 PTY
    fn on_resized(&mut self) {
        let (rows, cols) = self.current_grid();

        let last = self.last_size.get();
        if last != (rows, cols) {
            // 窗口尺寸变，联动调整标签宽度
            self.update_tab_width();
            self.last_size.set((rows, cols));
            // resize_all 内部同步各会话 PTY + ScreenBuffer，并清空各自缓存、全量标脏
            self.sessions.resize_all(rows, cols);
            // 强制重绘激活会话（缓存已随 resize 清空，本次全量重建）
            self.render_active_force();
        }
    }

    // 滚轮滚动触发，按行数滚动：正 = 向上（进入历史），负 = 向下（回到最新）。
    fn on_scroll_offset_by(&mut self, dy: f32) {
        let Some(session) = self.sessions.active_session_mut() else {
            return;
        };
        // 设置滚动条偏移量
        let action = session.scroll_offset_by(dy, self.char_h.get());
        // 依据返回动作选择渲染方式
        match action {
            ScrollAction::None => {}
            ScrollAction::FollowMode => self.render_active_force(),
            ScrollAction::Viewport => self.render_viewport(),
        }
    }

    // 滚动条跳转触发：ratio ∈ [0,1]（0 = 底部，1 = 顶部）。
    fn on_scroll_offset_to(&mut self, ratio: f32) {
        let Some(session) = self.sessions.active_session_mut() else {
            return;
        };
        // 设置滚动条偏移量
        let action = session.scroll_offset_to(ratio);
        // 依据返回动作选择渲染方式
        match action {
            ScrollAction::None => {}
            ScrollAction::FollowMode => self.render_active_force(),
            ScrollAction::Viewport => self.render_viewport(),
        }
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
        let char_w = self.char_w.get().max(1.0);
        let char_h = self.char_h.get().max(1.0);
        let cols = (term_w as f32 / char_w).floor().clamp(1.0, MAX_COLS as f32) as u16;
        let rows = (term_h as f32 / char_h).floor().clamp(1.0, MAX_ROWS as f32) as u16;
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

/// RGB 元组 → Slint Color。
pub(super) fn rgb_to_color(rgb: (u8, u8, u8)) -> slint::Color {
    slint::Color::from_rgb_u8(rgb.0, rgb.1, rgb.2)
}

/// RGB 元组 → Slint Brush（不透明）。
pub(super) fn rgb_to_brush(rgb: (u8, u8, u8)) -> slint::Brush {
    slint::Brush::SolidColor(rgb_to_color(rgb))
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
