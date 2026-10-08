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

use crate::app::events::{TerminalEvent, WindowEvent};
use crate::config::Profile;
use crate::pty::Pty;
use crate::session::active::ActiveSession;
use crate::session::session_id::{ProfileId, SessionId};
use crate::terminal::TerminalState;
use anyhow::Result;

/// 单个会话（Tab）独立的滚动视图状态。
///
/// 滚动状态必须按 Tab 各自维护：滚动偏移量、滚轮累加器、冻结锚点等互不影响，
/// 切换 Tab 时各自恢复、各自冻结，避免 A 标签页的滚动位置串扰到 B 标签页。
/// 仅 UI 线程访问，故用 `Cell` 通过 `&self` 提供内部可变性。
#[derive(Debug, Clone, Copy, Default)]
struct ScrollState {
    /// 滚动偏移量：从底部向上滚动的行数（0 = 跟随最新输出）
    offset: usize,
    /// 上次设置 offset 时 scrollback 的长度，用于滚动状态下冻结视图
    anchor_len: usize,
    /// 滚轮像素累加器：累积满一个字符行高（CHAR_H）才触发一次整行滚动
    acc: f32,
}

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
    // 状态状态机，解析回调写状态，UI 线程读状态渲染。
    state: Arc<Mutex<TerminalState>>,
    // 本会话独立的滚动视图状态（仅 UI 线程访问，Cell 提供 &self 内部可变性）
    scroll: Cell<ScrollState>,
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
            scroll: Cell::new(ScrollState::default()),
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
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.pty.resize(rows, cols);
        self.state
            .lock()
            .unwrap()
            .resize(rows as usize, cols as usize);
    }

    // 终端状态的共享句柄（供渲染层只读 + 解析回调写）。
    pub fn state(&self) -> &Arc<Mutex<TerminalState>> {
        &self.state
    }

    // ─── 滚动视图状态（本会话独立维护，仅 UI 线程访问） ───

    // 读取本会话当前的 scrollback 历史长度。
    pub fn scrollback_len(&self) -> usize {
        self.state.lock().unwrap().screen().scrollback.len()
    }

    // 冻结滚动视图并返回本会话最终生效的滚动偏移量。
    // 滚动状态下（offset > 0）scrollback 增长时，把偏移量随底部同步下移，
    // 保持用户正在查看的历史内容在视口中不动；跟随模式（offset == 0）直接返回 0。
    // 同时刷新锚点长度为当前 scrollback 长度，供下一次增长量计算。
    // 注：渲染路径已持有 state 锁，故 sb_len 由调用方传入，避免重入死锁。
    pub fn settle_scroll(&self, sb_len: usize) -> usize {
        let mut st = self.scroll.get();
        let current = st.offset;
        let offset = if current > 0 {
            let grew = sb_len.saturating_sub(st.anchor_len);
            (current + grew).min(sb_len)
        } else {
            0
        };
        st.offset = offset;
        st.anchor_len = sb_len;
        self.scroll.set(st);
        offset
    }

    // 提交新的滚动偏移量，更新偏移与锚点，返回应触发的渲染动作。
    //  offset == 0，跟随模式，查看最新数据；offset > 0，滚动模式，查看历史数据。
    // 滚动位置未变则返回 ScrollAction::None，避免无谓重绘。
    pub fn commit_scroll(&self, offset: usize, sb_len: usize) -> ScrollAction {
        let offset = offset.min(sb_len);
        let mut st = self.scroll.get();
        let prev = st.offset;
        if offset == prev {
            return ScrollAction::None;
        }
        st.offset = offset;
        st.anchor_len = sb_len;
        self.scroll.set(st);
        if prev > 0 && offset == 0 {
            // 从滚动模式回到了跟随模式，需全量重建。
            ScrollAction::FollowMode
        } else {
            // 滚动模式内偏移变化，仅需重建视口。
            ScrollAction::Viewport
        }
    }

    // 滚轮像素累加换算：正 = 向上（进入历史），负 = 向下（回到最新）。
    // 累积满一个字符行高（char_h）才换算为整行滚动，未满一行的余量留在本会话累加器中。
    // 返回需要提交的新目标偏移量；不足一行返回 None。
    pub fn scroll_by(&self, dy: f32, char_h: f32, sb_len: usize) -> Option<usize> {
        let mut st = self.scroll.get();
        let mut acc = st.acc + dy;
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
        st.acc = acc;
        self.scroll.set(st);
        if lines == 0 {
            None
        } else {
            // 当前滚动位置加上要滚动的行数，clamp 确保 offset 落在 [0, sb_len]
            Some((st.offset as i64 + lines).clamp(0, sb_len as i64) as usize)
        }
    }

    // 滚动条比例跳转：ratio ∈ [0,1]（0 = 底部，1 = 顶部），换算为目标偏移量。
    pub fn scroll_to_offset(&self, ratio: f32, sb_len: usize) -> usize {
        (ratio.clamp(0.0, 1.0) * sb_len as f32).round() as usize
    }

    // 滚动条位置比例 = offset / sb_len。0 = 底部（最新），1 = 顶部（最老历史）。
    pub fn scroll_ratio(&self, sb_len: usize) -> f32 {
        if sb_len == 0 {
            return 0.0;
        }
        self.scroll.get().offset as f32 / sb_len as f32
    }

    // 滑块大小比例 = 可见行数 / 总行数（scrollback + 可见行），历史越长滑块越短。max(1) 防止除零。
    pub fn viewport_ratio(&self, sb_len: usize, rows: usize) -> f32 {
        let rows = rows.max(1);
        rows as f32 / (sb_len + rows) as f32
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
