//! controller — App
//!
//! 桥接 Slint MainWindow 与 SessionManager / ProfileManager。
//!
//! 设计要点：
//!   - `App` 用 `Rc` 包装，`SessionManager` / `last_size` 用 `RefCell` 提供内部可变性，
//!     Slint 的 `'static` 回调闭包通过 `Weak<App>` 回访业务状态。
//!   - PTY 输出在独立 Output Thread 中读取并完成 vte use slint :: private_unstable_api :: re_exports as sp;     `invoke_from_event_loop` 调度 UI 线程渲染，实现低延迟事件触发式渲染，
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
use crate::renderer::{RunRender, build_viewport_runs, runs_to_cell_runs};
use crate::session::SessionManager;
use crate::session::session::TerminalSessionLike;
use crate::session::session_id::{ProfileId, SessionId};
use crate::terminal::Cell as TerminalCell;
use crate::terminal::TerminalState;

mod ui_generated {
    slint::include_modules!();
}

use crate::renderer::buffer::RenderCache;
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
    // 渲染缓存：用于存储上一帧的渲染结果，避免重复渲染
    render_cache: RenderCache,
    // PTY 输出唤醒通道发送端：新建会话时 clone 给对应 Output Thread
    wake_tx: SyncSender<()>,
    // PTY 输出唤醒通道接收端：渲染分发线程经此通道接收唤醒信号
    wake_rx: Arc<Mutex<Receiver<()>>>,
    // 滚动偏移量：从底部向上滚动的行数（0 = 跟随最新输出）
    scroll_offset: Cell<usize>,
    // 上次设置 scroll_offset 时 scrollback 的长度，用于滚动状态下冻结视图：
    // 新历史行到达时把偏移量同步下移，保持用户正在查看的内容不动。
    anchor_len: Cell<usize>,
    // 滚轮像素累加器：累积满一个字符行高（CHAR_H）才触发一次整行滚动
    scroll_acc: Cell<f32>,
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
            render_cache: RenderCache::new(),
            wake_tx,
            wake_rx: Arc::new(Mutex::new(wake_rx)),
            scroll_offset: Cell::new(0),
            anchor_len: Cell::new(0),
            scroll_acc: Cell::new(0.0),
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
    // RenderCache 按行缓存，将 O(全屏行数) 的 build_runs 降低到 O(脏行数)。
    //
    // 解析已在 Output Thread 完成，这里只读状态并渲染：
    // 锁状态快照脏行与光标 → 克隆脏行释放锁 → 锁外重建 runs → 更新 Slint Model。
    //
    // 该入口受脏标记守护（PTY 输出 / resize / Tab 切换触发）；滚动偏移变化走 render_viewport。
    fn render(&self) {
        let state = match self.sessions.borrow().active_state() {
            Some(s) => s,
            None => {
                self.clear_view();
                return;
            }
        };

        let guard = state.lock().unwrap();
        if !guard.is_dirty() {
            // 无变更（可能是残余唤醒），直接跳过
            return;
        }
        self.render_locked(&guard);
    }

    // 滚动渲染入口：滚动偏移变化时强制重绘（此时终端状态可能不脏，不能走 is_dirty 守卫）。
    fn render_viewport(&self) {
        let state = match self.sessions.borrow().active_state() {
            Some(s) => s,
            None => {
                self.clear_view();
                return;
            }
        };
        let guard = state.lock().unwrap();
        self.render_locked(&guard);
    }

    // 无激活会话时清空视图与滚动状态。
    fn clear_view(&self) {
        self.row_cache.borrow_mut().clear();
        self.render_cache.clear();
        self.cells_model.set_vec(Vec::new());
        self.main_window.set_cursor_visible(false);
        self.scroll_offset.set(0);
        self.anchor_len.set(0);
    }

    // 实际渲染：根据 scroll_offset 决定走「跟随模式（增量缓存）」还是「滚动模式（视口拼接）」。
    fn render_locked(&self, guard: &std::sync::MutexGuard<'_, TerminalState>) {
        let screen = guard.screen();
        let sb_len = screen.scrollback.len();

        // 冻结内容，滚动状态下新历史行到达时，把偏移量随底部下移，保持用户正在查看的内容不动。
        let offset = {
            let current_offset = self.scroll_offset.get();
            if current_offset > 0 {
                let grew = sb_len.saturating_sub(self.anchor_len.get());
                (current_offset + grew).min(sb_len)
            } else {
                0
            }
        };
        self.scroll_offset.set(offset);
        self.anchor_len.set(sb_len);

        let cells = if offset == 0 {
            // 跟随模式：仅重建脏行，未变更行复用缓存
            self.render_cache
                .update(guard.take_dirty_rows(), screen.visible_rows());
            self.render_cache.build_cell_runs()
        } else {
            // 滚动模式：全量重建视口（scrollback 历史 + 当前屏拼接），丢弃脏行标记
            let _ = guard.take_dirty_rows();
            runs_to_cell_runs(&build_viewport_runs(screen, offset))
        };
        self.cells_model.set_vec(cells);

        // 光标：滚动状态下隐藏（实时光标位于视口之外）
        if offset == 0 {
            self.main_window.set_cursor_row(guard.cursor.row as i32);
            self.main_window.set_cursor_col(guard.cursor.col as i32);
            self.main_window.set_cursor_visible(guard.cursor_visible);
        } else {
            self.main_window.set_cursor_visible(false);
        }

        // 滚动条状态（无历史时隐藏滚动条）
        self.main_window.set_has_scrollback(sb_len > 0);
        if sb_len > 0 {
            // 滚动条的位置 = offset / sb_len。0 = 底部（最新），1 = 顶部（最老历史）
            self.main_window.set_scroll_ratio(offset as f32 / sb_len as f32);
            // 滑块大小比例 = 可见行数 / 总行数（scrollback + 可见行），控制滚动条滑块的视觉长度——历史越长，滑块越短。max(1) 防止除零
            let rows = screen.rows.max(1);
            self.main_window.set_viewport_ratio(rows as f32 / (sb_len + rows) as f32);
        }
    }

    // 滚轮滚动触发执行，按行数滚动：正 = 向上（进入历史），负 = 向下（回到最新）。
    // 滚动像素级累加，累积满一个字符行高（CHAR_H）触发一次整行滚动。
    fn scroll_offset_by(&self, dy: f32) {
        let mut acc = self.scroll_acc.get() + dy;
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

        self.scroll_acc.set(acc);
        if lines != 0 {
            // 如果滚动超过一个行高度，就重绘窗口
            let sb_len = self.scrollback_len();
            let cur = self.scroll_offset.get() as i64;
            // 当前滚动位置加上要滚动的行数，得到新目标行位置，使用 clamp 确保 offset 在 [0, sb_len] 范围内，
            let offset = (cur + lines).clamp(0, sb_len as i64) as usize;
            self.commit_scroll(offset, sb_len);
        }
    }

    // 滚动条跳转：ratio ∈ [0,1]（0 = 底部，1 = 顶部）。
    fn scroll_offset_to(&self, ratio: f32) {
        let sb_len = self.scrollback_len();
        let offset = (ratio.clamp(0.0, 1.0) * sb_len as f32).round() as usize;
        self.commit_scroll(offset, sb_len);
    }

    // 设置新的滚动偏移量(scroll_offset)，更新偏移与锚点，并按需重绘
    //  scroll_offset == 0，跟随模式，查看最新数据
    //  scroll_offset >  0，滚动模式，查看历史数据
    fn commit_scroll(&self, offset: usize, sb_len: usize) {
        let offset = offset.min(sb_len);
        let prev = self.scroll_offset.get();
        if offset == prev {
            // 滚动位置未变，直接跳过
            return;
        }
        self.scroll_offset.set(offset);
        self.anchor_len.set(sb_len);
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
            .borrow()
            .active_state()
            .map(|s| s.lock().unwrap().screen().scrollback.len())
            .unwrap_or(0)
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
        // 新建 Tab
        let weak = Rc::downgrade(self);
        self.main_window.on_new_tab(move || {
            if let Some(app) = weak.upgrade() {
                // create_default_session 内部已 sync_tabs（含强制渲染）
                app.create_default_session();
            }
        });

        // 关闭 Tab
        let weak = Rc::downgrade(self);
        self.main_window.on_close_tab(move |id_str| {
            if let Some(app) = weak.upgrade() {
                if let Some(id) = parse_session_id(&id_str) {
                    app.close_session(id);
                }
                // 关闭后激活会话可能切换，清空按行缓存再强制渲染新激活会话
                app.row_cache.borrow_mut().clear();
                app.render_cache.clear();
                app.sync_tabs();
                // 关闭的是最后一个会话（管理器已无会话）→ 退出整个应用
                if app.sessions.borrow().is_empty() {
                    let _ = slint::quit_event_loop();
                }
            }
        });

        // 切换 Tab
        let weak = Rc::downgrade(self);
        self.main_window.on_select_tab(move |id_str| {
            if let Some(app) = weak.upgrade() {
                if let Some(id) = parse_session_id(&id_str) {
                    app.sessions.borrow_mut().set_active_session(id);
                    // 切换 Tab 时清空按行缓存（新会话的行内容与旧会话无关）
                    app.row_cache.borrow_mut().clear();
                    app.render_cache.clear();
                    // sync_tabs 末尾强制全量渲染，立即显示新会话最新状态（无白屏）
                    app.sync_tabs();
                }
            }
        });

        // 选择 Profile 新建会话
        let weak = Rc::downgrade(self);
        self.main_window.on_select_profile(move |pid_str| {
            if let Some(app) = weak.upgrade() {
                let pid = ProfileId::new(pid_str.to_string());
                if let Some(profile) = app.profiles.get_profile(&pid) {
                    app.create_session_for_profile(profile);
                }
            }
        });

        // 键盘回调，处理键盘输入转发到激活会话的 PTY
        let weak = Rc::downgrade(self);
        self.main_window.on_key_pressed(move |text, mods| {
            if let Some(app) = weak.upgrade() {
                let data = encode_key(&text, mods.meta, mods.control, mods.alt, mods.shift);
                if !data.is_empty() {
                    if let Err(e) = app.sessions.borrow_mut().write_to_active(&data) {
                        eprintln!("[terma] pty write error: {e}");
                    }
                }
            }
            true
        });

        // IME 提交回调：输入法完成选词后提交的完整字符串（如中文），
        // 直接转为 UTF-8 字节写入激活会话的 PTY。
        //
        // 与 on_key_pressed 的区别：IME 提交走 TextInput 的 edited 回调，
        // 提交文本已是完整字符串，无需经过 encode_key 的特殊键映射，
        // 直接按 UTF-8 原样写入，避免误触 Backspace/Delete 等控制符转换。
        let weak = Rc::downgrade(self);
        self.main_window.on_ime_commit(move |text| {
            if let Some(app) = weak.upgrade() {
                let data = text.as_bytes().to_vec();
                if !data.is_empty() {
                    if let Err(e) = app.sessions.borrow_mut().write_to_active(&data) {
                        eprintln!("[terma] ime pty write error: {e}");
                    }
                }
            }
        });

        // 滚动：鼠标滚轮
        let weak = Rc::downgrade(self);
        self.main_window.on_scroll_delta(move |dy| {
            if let Some(app) = weak.upgrade() {
                app.scroll_offset_by(dy);
            }
        });

        // 滚动：滚动条拖动/点击
        let weak = Rc::downgrade(self);
        self.main_window.on_scroll_to(move |ratio| {
            if let Some(app) = weak.upgrade() {
                app.scroll_offset_to(ratio);
            }
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
        self.render_cache.clear();
        self.sync_tabs();
    }

    // 根据 id 关闭会话
    fn close_session(&self, id: SessionId) {
        self.sessions.borrow_mut().close_session(id);
    }

    /// 按目标终端网格设置主窗口初始尺寸（物理像素），供启动时创建首个会话使用。
    fn apply_initial_window_size(&self) {
        let logical = slint::LogicalSize::new(
            (DEFAULT_COLS as i32 * CHAR_W + H_PADDING) as f32,
            (DEFAULT_ROWS as i32 * CHAR_H + CHROME_H + V_PADDING) as f32,
        );
        self.main_window
            .window()
            .set_size(slint::WindowSize::Logical(logical));
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
            self.render_cache.clear();
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
    let scale = win.scale_factor();
    if scale <= 0.0 {
        return None;
    }
    let logical = win.size().to_logical(scale);
    let term_w = logical.width as i32 - H_PADDING;
    let term_h = logical.height as i32 - CHROME_H - V_PADDING;
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
fn encode_key(text: &slint::SharedString, meta: bool, control: bool, alt: bool, shift: bool) -> Vec<u8> {
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
    if meta {
        if c.is_ascii_alphabetic() {
            return vec![(c as u8) & 0x1f];
        }
        return vec![];
    }

    // 普通可见字符 → utf8 字节
    s.as_bytes().to_vec()
}
