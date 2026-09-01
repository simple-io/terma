//! session — TerminalSession 抽象与真实 PTY 实现
//!
//! TerminalSession 持有 vte::Parser + TerminalState。
//!   - Output Thread 把 PTY 字节送回 UI 线程（mpsc）
//!   - UI 线程 drain 后用 parser.advance(&mut state, byte) 解析，更新 TerminalState/ScreenBuffer
//!   - 渲染层读取 state 生成 Cell 网格
//!
//! 脏标记（Dirty Flag）：
//!   - pump_output 解析到数据后置 dirty = true
//!   - App 层仅在 dirty 时触发 build_runs + UI 更新，避免无效全量重绘
//!   - resize / 屏幕切换（1049）同样置 dirty
//!
//! 事件触发通知：
//!   - Output Thread 写入 mpsc channel 后经 SyncSender(1) 唤醒通道通知渲染分发线程
//!   - 分发线程通过 slint::invoke_from_event_loop 通知 UI 线程立即 pump + render
//!   - 彻底移除渲染轮询定时器，空闲状态下零渲染 CPU 占用
//!
//! 所有权与线程模型：
//!   - TerminalSession 拥有：master、writer、child、receiver、parser、state。
//!   - parser 与 state 仅 UI 线程访问（SessionManager 在 RefCell 中）。
//!   - reader move 进 Output Thread，线程绝不接触 Slint / state。
//!   - 关闭 Tab → Drop：kill child → wait → join thread → 释放资源。
//!
//! 跨线程边界：
//!   PTY Output Thread ──(mpsc::Sender<Vec<u8>>)──> UI Thread (receiver → parser → state)
//!   PTY Output Thread ──(SyncSender<()> 唤醒)──> 渲染分发线程 ──(invoke_from_event_loop)──> UI Thread

use std::io::Read;
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use portable_pty::ChildKiller;

use crate::profile::Profile;
use crate::session::session_id::{ProfileId, SessionId};
use crate::terminal::TerminalState;

/// 一个终端会话对外暴露的最小接口（与具体实现解耦，便于测试与替换）。
pub trait TerminalSessionLike {
    fn id(&self) -> SessionId;
    fn profile_id(&self) -> &ProfileId;
    fn title(&self) -> &str;
    fn profile_name(&self) -> &str;
}

/// 终端会话，一个会话包括：PTY + 一个子进程 + 一个 vte Parser + 一个 TerminalState(含 ScreenBuffer)。
pub struct TerminalSession {
    // 会话 id
    id: SessionId,
    // 会话标题
    title: String,
    // 会话所属 profile id
    profile_id: ProfileId,
    // 会话所属 profile 名称
    profile_name: String,

    // PTY 主端：resize 用。take_writer / try_clone_reader 之后仍可调用 resize。
    master: Box<dyn portable_pty::MasterPty + Send>,
    // PTY 写入端：转发用户键盘输入。仅 UI 线程访问（&mut）。
    writer: Box<dyn std::io::Write + Send>,
    // PTY 读取端：读取Shell结果显示到窗口，UI 线程 try_recv drain。
    receiver: mpsc::Receiver<Vec<u8>>,
    // 子进程句柄：关闭时 kill + wait。
    child: Box<dyn portable_pty::Child + Send + Sync>,
    // Output Thread 句柄：关闭时 join。
    output_thread: Option<JoinHandle<()>>,
    // VT 解析器（UI 线程独占）
    parser: vte::Parser,
    // 终端状态（含 ScreenBuffer，UI 线程独占）
    state: TerminalState,
    /// 脏标记：pump_output 解析到新数据、resize、屏幕切换后置 true。
    /// App 层据此决定是否触发 build_runs + UI 刷新，避免无变更时的无效重绘。
    dirty: bool,
}

impl TerminalSession {
    /// 创建一个真实终端会话：openpty → spawn shell → take writer/reader → 启动 Output Thread。
    ///
    /// `rows` / `cols` 为初始终端网格，由调用方根据当前主窗口实际尺寸计算得出，
    /// 确保 PTY 与 TerminalState 渲染层尺寸一致。
    /// `wake_tx` 是渲染唤醒通道：Output Thread 每次写入 channel 后 `try_send(())`，
    /// 由渲染分发线程经 `invoke_from_event_loop` 通知 UI 线程立即 pump + 渲染。
    pub fn new(id: SessionId, profile: &Profile, title: String, rows: u16, cols: u16, wake_tx: SyncSender<()>, ) -> Result<Self> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openPty failed")?;

        // 构造子进程命令
        let mut cmd = portable_pty::CommandBuilder::new(&profile.command);
        for arg in &profile.args {
            cmd.arg(arg);
        }
        if let Some(cwd) = &profile.working_directory {
            cmd.cwd(cwd);
        }

        // 启动 pty 的 shell
        let child = pair.slave.spawn_command(cmd).context("spawn_command failed")?;

        // 关键：丢弃 slave 端，否则 master 的 reader 永远收不到 EOF（子进程退出时）。
        drop(pair.slave);

        // 获取 pty 的 writer 和 reader
        let writer = pair.master.take_writer().context("take_writer failed")?;
        let reader = pair.master.try_clone_reader().context("try_clone_reader failed")?;

        // Output thread：独立线程阻塞读 PTY，把字节块经 channel 送回 UI 线程。
        // 线程只持有 reader + sender + wake_tx，不接触 Slint / SessionManager。
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let handle = thread::Builder::new()
            .name(format!("pty-output-{}", id.0))
            .spawn(move || {
                let mut reader = reader;
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        // EOF：子进程已关闭
                        Ok(0) => break,
                        // 正常读取字节块
                        Ok(n) => {
                            // 把数据写入管道，如果异常则退出
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                            // 通知 UI 线程有数据待消费（SyncSender(1) 自动合并连续唤醒）
                            let _ = wake_tx.try_send(());
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(std::time::Duration::from_millis(5));
                            continue;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .context("spawn output thread failed")?;

        Ok(Self {
            id,
            profile_id: profile.id.clone(),
            title,
            profile_name: profile.name.clone(),
            master: pair.master,
            writer,
            child,
            output_thread: Some(handle),
            receiver: rx,
            parser: vte::Parser::new(),
            state: TerminalState::new(rows as usize, cols as usize),
            dirty: false,
        })
    }

    /// 向 PTY 写入用户输入（键盘）。
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()
    }

    /// 调整 PTY 尺寸（rows/cols）并同步 ScreenBuffer。
    /// 尺寸变更会导致 ScreenBuffer 内容重排，标记 dirty 触发重绘。
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.master.resize(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.state.resize(rows as usize, cols as usize);
        self.dirty = true;
    }

    /// 非阻塞 drain 当前会话所有待处理输出，并经 vte::Parser 解析进 TerminalState。
    /// 返回是否解析到了新数据（用于上层脏标记判定）。
    /// UI 线程在 Timer / 事件回调中调用。
    pub fn pump_output(&mut self) -> bool {
        let mut processed = false;
        while let Ok(chunk) = self.receiver.try_recv() {
            for &b in &chunk {
                self.parser.advance(&mut self.state, b);
            }
            processed = true;
        }
        if processed {
            self.dirty = true;
        }
        processed
    }

    /// 当前会话是否有未渲染的变更。
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// 清除脏标记（在 UI 完成渲染后调用）。
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// 只读访问终端状态（供渲染层构建 Cell 网格）。
    pub fn state(&self) -> &TerminalState {
        &self.state
    }

    /// 检查子进程是否已退出（非阻塞）。
    pub fn is_child_exited(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(_) => true,
        }
    }

    /// 关闭会话：kill 子进程 → 等待退出 → reader 收 EOF 退出 → join 输出线程。
    fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.output_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl TerminalSessionLike for TerminalSession {
    fn id(&self) -> SessionId {
        self.id
    }
    fn profile_id(&self) -> &ProfileId {
        &self.profile_id
    }
    fn title(&self) -> &str {
        &self.title
    }
    fn profile_name(&self) -> &str {
        &self.profile_name
    }
}
