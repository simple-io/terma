//! session — TerminalSession 会话编排与终端状态协作
//!
//! TerminalSession 负责「会话生命周期编排」与「终端状态协作」
//!   - `Pty` 拥有 master/writer/child 与 Output Thread，阻塞读 PTY 后把每段原始字节
//!     经 `on_output` 回调交回本会话。
//!   - 本会话在 `on_output` 闭包里逐字节 feed 给 vte::Parser 解析进 TerminalState；
//!     解析完成后若本会话为激活会话，才经唤醒通道通知 UI 线程渲染。
//!   - UI 线程只读取 `Arc<Mutex<TerminalState>>` 里的脏行做增量渲染，不再执行
//!     任何 vte 解析，解析与渲染彻底解耦。
//!
//! 脏标记（Dirty Flag）：
//!   - 解析回调每解析一段输出调用 `mark_dirty`，置 `TerminalState.dirty = true`；
//!     纯光标移动（不触碰 grid）也能被捕获，保证光标位置实时刷新。
//!   - `ScreenBuffer` 内部另有逐行 dirty_rows，供渲染层只重建脏行。
//!   - resize / 屏幕切换（1049）同样置脏。
//!
//! 事件触发通知（仅激活会话）：
//!   - 解析回调经 `ActiveSession`（AtomicU64）判断本会话是否激活，仅激活会话
//!     发送 `WindowEvent::Pty(TerminalEvent::Data)` 唤醒渲染分发线程，再由
//!     `invoke_from_event_loop` 调度 UI 线程渲染。非激活会话静默解析、不唤醒 UI。
//!
//! 所有权与线程模型：
//!   - TerminalSession 拥有：`Pty`（封装 master/writer/child/output_thread）与
//!     `Arc<Mutex<TerminalState>>`（与 Output Thread 共享）。
//!   - vte parser / active_tracker / event_tx 经 `on_output` 闭包 move 进 Output Thread，
//!     线程绝不接触 Slint / SessionManager。
//!   - 关闭 Tab → Drop：Pty::shutdown（kill child → wait → join thread → 释放资源）。
//!
//! 跨线程边界：
//!   PTY Output Thread ──(on_output → vte 解析)→ Arc<Mutex<TerminalState>>
//!   PTY Output Thread ──(active? event_tx)──> 渲染分发线程 ──(invoke_from_event_loop)──> UI Thread
//!   PTY Output Thread ──(on_exit: EOF/错误 → ChildExited(id))──> UI Thread 关闭对应会话

use std::cell::Cell;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use crate::app::controller::CellRun;
use crate::app::events::{TerminalEvent, WindowEvent};
use crate::config::Profile;
use crate::pty::Pty;
use crate::renderer::RenderCells;
use crate::session::active::ActiveSession;
use crate::session::session_id::{ProfileId, SessionId};
use crate::terminal::TerminalState;
use anyhow::Result;

/// 提交滚动偏移后应触发的渲染动作，由 App 据此决定重绘方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAction {
    /// 滚动位置未变化，无需重绘
    None,
    /// 从滚动模式回到跟随模式，需全量重建
    FollowMode,
    /// 滚动模式内偏移变化，仅重建视口
    Viewport,
}

/// 一帧渲染的光标信息：跟随模式显示真实光标，滚动模式隐藏（光标在视口外）。
pub struct CursorRender {
    /// 光标行（网格坐标）
    pub row: i32,
    /// 光标列（网格坐标）
    pub col: i32,
    /// 是否可见
    pub visible: bool,
}

/// 一帧渲染的滚动条信息（无 scrollback 时为 None，App 不更新滚动条）。
pub struct ScrollRender {
    /// 滚动条位置比例：0 = 底部（最新），1 = 顶部（最老历史）
    pub position: f32,
    /// 滑块大小比例：可见行数 / 总行数（scrollback + 可见行）
    pub viewport: f32,
}

/// 会话渲染一帧的完整输出，由 App 写入 Slint model 与窗口属性。
/// 计算全部在会话侧完成，App 不再触碰 TerminalState，从根本上规避重入加锁死锁。
pub struct RenderOutput {
    /// 单元格渲染项列表
    pub cells: Vec<CellRun>,
    /// 光标
    pub cursor: CursorRender,
    /// 滚动条（None = 无历史）
    pub scroll: Option<ScrollRender>,
}

/// 终端会话
pub struct TerminalSession {
    // 会话 id
    id: SessionId,
    // 会话标题
    title: String,
    // 会话启动项 id
    profile_id: ProfileId,
    // 会话启动项名称
    profile_name: String,
    // PTY 对象
    pty: Pty,
    // 状态状态机，Pty 线程和 UI 线程共同持有，PTY 解析回调写状态，UI 线程读状态渲染。
    state: Arc<Mutex<TerminalState>>,
    // 滚动条偏移量, 从底部向上滚动的行数（0 = 在最底部，越大越靠近最老历史）
    scroll_offset: Cell<usize>,
    // 滚动条锚点，上次同步 offset 时的历史长度
    scroll_anchor: Cell<usize>,
    // 滚动条累加器，累积满一个字符行高触发一次整行滚动
    scroll_acc: f32,
    // 会话独立渲染单元：按行缓存 + 独立计算
    render_cells: RenderCells,
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl TerminalSession {
    /// 创建终端会话
    pub fn new(
        id: SessionId,
        profile: &Profile,
        title: String,
        rows: u16,
        cols: u16,
        event_tx: SyncSender<WindowEvent>,
        active_tracker: Arc<ActiveSession>,
    ) -> Result<Self> {
        // 终端状态对象，用于解析回调写 和 UI 线程读。
        let state = Arc::new(Mutex::new(TerminalState::new(rows as usize, cols as usize)));
        let state_for_thread = state.clone();

        // 输出处理回调：move 进 Pty 的 Output Thread，逐字节 vte 解析进 state。
        let mut parser = vte::Parser::new();
        let event_tx_output = event_tx.clone();
        let on_output = move |bytes: &[u8]| {
            {
                let state = &mut *state_for_thread.lock().unwrap();
                for &b in bytes {
                    parser.advance(state, b);
                }
                // 标记有变更，供 UI 线程判定是否渲染
                state.mark_dirty();
            }
            // 仅激活会话唤醒 UI 线程渲染；非激活会话静默保持状态最新
            if active_tracker.is_active(id) {
                let _ = event_tx_output.send(WindowEvent::Pty(TerminalEvent::Data));
            }
        };

        // PTY 输出结束（EOF / 不可恢复读取错误）回调，通知 UI 线程关闭本会话。
        let event_tx_exit = event_tx.clone();
        let on_exit = move || {
            let _ = event_tx_exit.send(WindowEvent::Pty(TerminalEvent::ChildExited(id)));
        };

        // 创建 PTY
        let pty = Pty::spawn(
            rows,
            cols,
            profile,
            format!("pty-output-{}", id.0),
            on_output,
            on_exit,
        )?;

        Ok(Self {
            id,
            profile_id: profile.id.clone(),
            title,
            profile_name: profile.name.clone(),
            pty,
            state,
            scroll_offset: Cell::new(0),
            scroll_anchor: Cell::new(0),
            scroll_acc: 0.0,
            render_cells: RenderCells::new(),
        })
    }

    // 获取会话 id
    pub fn id(&self) -> SessionId {
        self.id
    }

    // 获取会话标题
    pub fn title(&self) -> &str {
        &self.title
    }

    // 获取会话启动项 id
    fn profile_id(&self) -> &ProfileId {
        &self.profile_id
    }

    // 获取会话启动项名称
    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    // 向 PTY 写入用户输入（键盘）
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.pty.write(data)
    }

    // 调整终端尺寸，先同步 PTY 尺寸，再重排 ScreenBuffer。
    // 尺寸变化后本会话按行缓存失效：清空缓存并全量标脏，确保下次渲染重建所有行
    // （仅增高时 resize_row 只标脏新增行，若不清缓存会丢失已有行内容）。
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.pty.resize(rows, cols);
        {
            let mut guard = self.state.lock().unwrap();
            guard.resize(rows as usize, cols as usize);
            guard.mark_all_dirty();
        }
        self.render_cells.clear();
    }

    // 读取本会话当前的 scrollback 历史长度。
    pub fn scrollback_len(&self) -> usize {
        self.state.lock().unwrap().screen().scrollback.len()
    }

    // 一次加锁读取 scrollback 的物理长度与逻辑累计长度（避免在持锁路径中重入加锁死锁）。
    //   - 物理长度：当前实际保留的历史行数，用于钳制 offset（无法滚进已被丢弃的行）；
    //   - 逻辑累计长度：含已丢弃行的总历史行数，用于计算锚点补偿增量 grew。
    fn scrollback_metrics(&self) -> (usize, usize) {
        let guard = self.state.lock().unwrap();
        let screen = guard.screen();
        (screen.scrollback.len(), screen.scrollback_total)
    }

    // 鼠标滚动滑轮触发，根据滚轮像素换算行数，正 = 向上（进入历史），负 = 向下（回到最新）。
    // 累积满一个字符行高（char_h）才换算为整行滚动，未满一行的余量留在本会话累加器中。
    pub fn scroll_offset_by(&mut self, dy: f32, char_h: f32) -> ScrollAction {
        let mut acc = self.scroll_acc + dy;
        let mut lines: i64 = 0;
        while acc >= char_h {
            // 向上滚动
            acc -= char_h;
            lines += 1;
        }
        while acc <= -char_h {
            // 向下滚动
            acc += char_h;
            lines -= 1;
        }
        self.scroll_acc = acc;

        if lines == 0 {
            return ScrollAction::None;
        }
        let sb_len = self.scrollback_len();
        let offset = (self.scroll_offset.get() as i64 + lines).clamp(0, sb_len as i64) as usize;
        self.scroll_update(offset)
    }

    // 鼠标点击滚动条跳转触发，根据滚动条比例跳转到目标偏移量
    pub fn scroll_offset_to(&mut self, ratio: f32) -> ScrollAction {
        let sb_len = self.scrollback_len();
        let offset = (ratio.clamp(0.0, 1.0) * sb_len as f32).round() as usize;
        self.scroll_update(offset)
    }

    // 更新滚动偏移量，target 为调用方按当前 offset 加减行数算出的目标偏移，范围 0 ～ sb_len
    fn scroll_update(&mut self, target: usize) -> ScrollAction {
        let old_offset = self.scroll_offset.get();
        let (sb_len, sb_total) = self.scrollback_metrics();

        // 滚动模式下补偿期间新到达的历史行：按逻辑累计长度计算，
        // 兼容 scrollback 满后物理长度不再增长、旧行被丢弃的情形。
        let grew = if old_offset > 0 {
            sb_total.saturating_sub(self.scroll_anchor.get())
        } else {
            0
        };
        let new_offset = if target > 0 {
            // 滚动模式：累加补偿后钳制到真实（物理）历史行数
            (target + grew).min(sb_len)
        } else {
            0
        };

        // 设置偏移量并刷新锚点为当前逻辑累计历史长度
        self.scroll_offset.set(new_offset);
        self.scroll_anchor.set(sb_total);

        if old_offset == new_offset {
            // 滚动位置未变，无需重绘
            return ScrollAction::None;
        }
        if new_offset == 0 {
            // 从滚动模式回到跟随模式，全量重建
            ScrollAction::FollowMode
        } else {
            // 滚动模式内偏移变化，仅重建视口
            ScrollAction::Viewport
        }
    }

    // 期间 PTY 可能新输出，所以渲染时重新计算滚动条偏移量，补偿 grew 增量，确保在滚动模式下窗口不下移。
    //   - sb_len：物理历史行数，作为 offset 上限（无法滚进已被丢弃的旧行）；
    //   - sb_total：逻辑累计历史行数，作为锚点补偿基准。历史满后 sb_len 不再增长，
    //     但每来一行新历史，锚定内容「距底部行数」就 +1，故按 sb_total 计算 grew 才能冻结视口。
    pub fn scroll_settle(&self, sb_len: usize, sb_total: usize) -> usize {
        let current = self.scroll_offset.get();
        let offset = if current > 0 {
            let grew = sb_total.saturating_sub(self.scroll_anchor.get());
            // offset 上限为真实 scrollback 行数：无法滚进已被丢弃的历史行
            (current + grew).min(sb_len)
        } else {
            0
        };
        self.scroll_offset.set(offset);
        self.scroll_anchor.set(sb_total);
        offset
    }

    // 滚动条位置，即 offset / sb_len（0 = 底部（最新），1 = 顶部（最老历史))。
    // 注：sb_len 由调用方传入。渲染路径已持有 state 锁，若在此内部再 lock 会重入死锁。
    pub fn scroll_position(&self, sb_len: usize) -> f32 {
        if sb_len == 0 {
            return 0.0;
        }
        self.scroll_offset.get() as f32 / sb_len as f32
    }

    // 滑块大小比例 = 可见行数 / 总行数（scrollback + 可见行），历史越长滑块越短。max(1) 防止除零。
    // 注：sb_len / rows 由调用方传入，避免在持锁的渲染路径中重入加锁导致死锁。
    pub fn scroll_ratio(&self, sb_len: usize, rows: usize) -> f32 {
        let rows = rows.max(1) as f32;
        rows / (sb_len as f32 + rows)
    }

    /// 计算本会话当前一帧的完整渲染结果（cells + 光标 + 滚动条）。
    ///
    /// 内部一次性加锁 `state`，完成滚动冻结、脏行增量 / 视口拼接、光标与滚动条计算，
    /// 返回后由 App 直接写入 Slint。**渲染计算全部下沉至此，App 不再触碰 TerminalState**，
    /// 从根本上规避「渲染路径重入 state 锁」的死锁。计算完毕清除脏标记。
    ///
    /// - `force == false`：仅当 state 有脏标记时才计算，否则返回 `None`（PTY 输出场景）；
    /// - `force == true`：忽略脏标记守卫强制渲染一帧（Tab 切换 / 滚动 / 新建关闭等交互）。
    pub fn render_frame(&self, force: bool) -> Option<RenderOutput> {
        let mut guard = self.state.lock().unwrap();
        if !force && !guard.is_dirty() {
            // 无脏标记，跳过渲染
            return None;
        }

        let (cells, cursor, scroll) = {
            let screen = guard.screen();
            let sb_len = screen.scrollback.len();
            let sb_total = screen.scrollback_total;
            // 冻结滚动视口，得到本帧滚动偏移（补偿渲染间隙新到达的历史行）
            let offset = self.scroll_settle(sb_len, sb_total);

            let cells = if offset == 0 {
                // 跟随模式：仅重建脏行，未变更行复用本会话缓存
                let dirty = guard.take_dirty_rows();
                self.render_cells.render_follow(screen, dirty)
            } else {
                // 滚动模式：全量拼接视口显示历史。不清空 dirty_rows，让其累积，
                // 回到跟随模式时增量重建期间变更的行，缓存始终跟随 grid 最新内容。
                self.render_cells.render_viewport(screen, offset)
            };

            // 光标：跟随模式显示真实光标，滚动模式光标在视口外，隐藏
            let cursor = if offset == 0 {
                CursorRender {
                    row: guard.cursor.row as i32,
                    col: guard.cursor.col as i32,
                    visible: guard.cursor.visible,
                }
            } else {
                CursorRender {
                    row: 0,
                    col: 0,
                    visible: false,
                }
            };

            // 滚动条：有历史才计算（sb_len / rows 直接从 guard 读，避免重入加锁）
            let scroll = if sb_len > 0 {
                let rows = guard.rows;
                Some(ScrollRender {
                    position: self.scroll_position(sb_len),
                    viewport: self.scroll_ratio(sb_len, rows),
                })
            } else {
                None
            };
            (cells, cursor, scroll)
        };

        guard.clear_dirty();
        Some(RenderOutput {
            cells,
            cursor,
            scroll,
        })
    }

    // 检查子进程是否已退出（非阻塞）。
    pub fn child_exited(&mut self) -> bool {
        self.pty.child_exited()
    }

    // 关闭会话，委托 `Pty::shutdown`（kill 子进程 → 等待退出 → reader 收 EOF → join 输出线程）。
    fn shutdown(&mut self) {
        self.pty.shutdown();
    }
}
