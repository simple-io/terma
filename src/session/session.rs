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
    // 滚动条偏移量, 从底部向上滚动的行数（0 = 在最底部，1 = 最最顶部）
    scroll_offset: usize,
    // 滚动条锚点，上次设置 offset 时 scrollback 的长度，用于滚动状态下冻结视图
    scroll_anchor: usize,
    // 滚动条累加器，累积满一个字符行高（CHAR_H）才触发一次整行滚动
    scroll_acc: f32,
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
            scroll_offset: 0,
            scroll_anchor: 0,
            scroll_acc: 0.0,
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

    // 获取滚动偏移量
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
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

    // 读取本会话当前的 scrollback 历史长度。
    pub fn scrollback_len(&self) -> usize {
        self.state.lock().unwrap().screen().scrollback.len()
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
        let offset = (self.scroll_offset as i64 + lines).clamp(0, sb_len as i64) as usize;
        self.scroll_update(offset)
    }

    // 鼠标点击滚动条跳转触发，根据滚动条比例跳转到目标偏移量
    pub fn scroll_offset_to(&mut self, ratio: f32) -> ScrollAction {
        let sb_len = self.scrollback_len();
        let offset = (ratio.clamp(0.0, 1.0) * sb_len as f32).round() as usize;
        self.scroll_update(offset)
    }

    // 更新滚动偏移量，返回应触发的渲染动作
    // 滚动状态下(offset > 0)且同时有内容输出, 把偏移量同步调整，保证正在查看的历史内容在视口中不动
    // 跟随模式(offset == 0)，不管是否有内容输出，强制渲染全量内容
    fn scroll_update(&mut self, new_offset: usize) -> ScrollAction {
        let old_offset = self.scroll_offset;
        if old_offset == new_offset {
            return ScrollAction::None;
        }

        // 计算新的偏移量
        let sb_len = self.scrollback_len();
        let new_offset = if new_offset > 0 {
            // 滚动模式下且同时有内容输出，偏移量要累加变化的行数，确保正在查看的历史内容窗口冻结
            let grew = sb_len.saturating_sub(self.scroll_anchor);
            (new_offset + grew).min(sb_len)
        } else {
            0
        };

        // 设置偏移量
        self.scroll_offset = new_offset;
        self.scroll_anchor = sb_len;

        if new_offset == 0 {
            // 跟随模式
            ScrollAction::FollowMode
        } else {
            // 滚动模式
            ScrollAction::Viewport
        }
    }

    // 滚动条位置，即 offset / sb_len（0 = 底部（最新），1 = 顶部（最老历史))。
    // 注：sb_len 由调用方传入。渲染路径已持有 state 锁，若在此内部再 lock 会重入死锁。
    pub fn scroll_position(&self, sb_len: usize) -> f32 {
        if sb_len == 0 {
            return 0.0;
        }
        self.scroll_offset as f32 / sb_len as f32
    }

    // 滑块大小比例 = 可见行数 / 总行数（scrollback + 可见行），历史越长滑块越短。max(1) 防止除零。
    // 注：sb_len / rows 由调用方传入，避免在持锁的渲染路径中重入加锁导致死锁。
    pub fn scroll_ratio(&self, sb_len: usize, rows: usize) -> f32 {
        let rows = rows.max(1) as f32;
        rows / (sb_len as f32 + rows)
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
