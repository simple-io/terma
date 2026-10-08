//! session — TerminalSession 抽象与真实 PTY 实现
//!
//! TerminalSession 持有 PTY 资源，并把 vte 解析下沉到 Output Thread：
//!   - Output Thread（解析线程）阻塞读 PTY，逐字节 feed 给 vte::Parser 解析进
//!     TerminalState；解析完成后若本会话为激活会话，才经唤醒通道通知 UI 线程渲染。
//!   - UI 线程只读取 `Arc<Mutex<TerminalState>>` 里的脏行做增量渲染，不再执行
//!     任何 vte 解析，解析与渲染彻底解耦。
//!
//! 脏标记（Dirty Flag）：
//!   - 解析线程每解析一段输出调用 `mark_dirty`，置 `TerminalState.dirty = true`；
//!     纯光标移动（不触碰 grid）也能被捕获，保证光标位置实时刷新。
//!   - `ScreenBuffer` 内部另有逐行 dirty_rows，供渲染层只重建脏行。
//!   - resize / 屏幕切换（1049）同样置脏。
//!
//! 事件触发通知（仅激活会话）：
//!   - 解析线程经 `ActiveSession`（AtomicU64）判断本会话是否激活，仅激活会话
//!     `wake_tx.try_send(())` 唤醒渲染分发线程，再由 `invoke_from_event_loop`
//!     调度 UI 线程渲染。非激活会话静默解析、不唤醒 UI，避免浪费 UI 线程资源。
//!
//! 所有权与线程模型：
//!   - TerminalSession 拥有：master、writer、child、output_thread 与
//!     `Arc<Mutex<TerminalState>>`（与解析线程共享）。
//!   - reader / parser / active_tracker / wake_tx move 进 Output Thread，
//!     线程绝不接触 Slint / SessionManager。
//!   - 关闭 Tab → Drop：kill child → wait → join thread → 释放资源。
//!
//! 跨线程边界：
//!   PTY Output Thread ──(vte 解析)→ Arc<Mutex<TerminalState>>
//!   PTY Output Thread ──(active? wake_tx)──> 渲染分发线程 ──(invoke_from_event_loop)──> UI Thread

use std::io::Read;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::app::events::{TerminalEvent, WindowEvent};
use crate::config::Profile;
use crate::session::active::ActiveSession;
use crate::session::session_id::{ProfileId, SessionId};
use crate::terminal::TerminalState;
use anyhow::{Context, Result};
use portable_pty::ChildKiller;

/// 终端会话，一个会话包括：PTY + 一个子进程 + 一个 Output Thread（内含 vte 解析）。
/// `TerminalState` 与解析线程通过 `Arc<Mutex<>>` 共享，UI 线程只读并渲染。
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
    // 子进程句柄：关闭时 kill + wait。
    child: Box<dyn portable_pty::Child + Send + Sync>,
    // Output Thread 句柄：关闭时 join。
    output_thread: Option<JoinHandle<()>>,
    // 终端状态（含 ScreenBuffer）：与 Output Thread 共享。
    // UI 线程读脏行渲染 / resize，解析线程写状态，二者经 Mutex 串行化。
    // 状态状态机
    state: Arc<Mutex<TerminalState>>,
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl TerminalSession {
    /// 创建一个真实终端会话：openpty → spawn shell → take writer/reader → 启动 Output Thread。
    ///
    /// `rows` / `cols` 为初始终端网格，由调用方根据当前主窗口实际尺寸计算得出，
    /// 确保 PTY 与 TerminalState 渲染层尺寸一致。
    /// `wake_tx` 是渲染唤醒通道：Output Thread 仅在本会话激活时 `try_send(())`，
    /// 由渲染分发线程经 `invoke_from_event_loop` 通知 UI 线程渲染。
    /// `active_tracker` 是共享的激活会话追踪器，供解析线程判断是否唤醒。
    pub fn new(
        id: SessionId,
        profile: &Profile,
        title: String,
        rows: u16,
        cols: u16,
        event_tx: SyncSender<WindowEvent>,
        active_tracker: Arc<ActiveSession>,
    ) -> Result<Self> {
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

        // 注入 Profile 自定义环境变量
        for (key, value) in &profile.env_vars {
            cmd.env(key, value);
        }

        // 确保子 shell 使用 UTF-8 locale，否则中文（及多字节字符）会乱码。
        if let Some(locale) = detect_utf8_locale() {
            if std::env::var_os("LANG").filter(|v| !v.is_empty()).is_none() {
                cmd.env("LANG", &locale);
            }
            if std::env::var_os("LC_CTYPE")
                .filter(|v| !v.is_empty())
                .is_none()
            {
                cmd.env("LC_CTYPE", &locale);
            }
        }

        // 启动 pty 的 shell
        let child = pair
            .slave
            .spawn_command(cmd)
            .context("spawn_command failed")?;

        // 关键：丢弃 slave 端，否则 master 的 reader 永远收不到 EOF（子进程退出时）。
        drop(pair.slave);

        // 获取 pty 的 writer 和 reader
        let writer = pair.master.take_writer().context("take_writer failed")?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("try_clone_reader failed")?;

        // 终端状态对象，用于解析线程写 和 UI 线程读。
        let state = Arc::new(Mutex::new(TerminalState::new(rows as usize, cols as usize)));
        let state_for_thread = state.clone();

        // 独立线程响应 pty 输出，逐字节 vte 解析进 state
        let handle = thread::Builder::new()
            .name(format!("pty-output-{}", id.0))
            .spawn(move || {
                let mut parser = vte::Parser::new();
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        // EOF：子进程已关闭
                        Ok(0) => break,
                        // 正常读取字节块：后台解析，不再占用 UI 线程
                        Ok(n) => {
                            {
                                let state = &mut *state_for_thread.lock().unwrap();
                                for &b in &buf[..n] {
                                    parser.advance(state, b);
                                }
                                // 标记有变更（含纯光标移动），供 UI 线程判定是否渲染
                                state.mark_dirty();
                            }
                            // 仅激活会话唤醒 UI 线程渲染；非激活会话静默保持状态最新
                            if active_tracker.is_active(id) {
                                let _ = event_tx.send(WindowEvent::Pty(TerminalEvent::Data));
                            }
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
            state,
        })
    }

    pub fn id(&self) -> SessionId {
        self.id
    }
    fn profile_id(&self) -> &ProfileId {
        &self.profile_id
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    /// 向 PTY 写入用户输入（键盘）。
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()
    }

    /// 调整 PTY 尺寸（rows/cols）并同步 ScreenBuffer。
    /// 尺寸变更会导致 ScreenBuffer 内容重排，`TerminalState::resize` 内部已置脏。
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.master.resize(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.state
            .lock()
            .unwrap()
            .resize(rows as usize, cols as usize);
    }

    /// 终端状态的共享句柄（供渲染层只读 + 解析线程写）。
    pub fn state(&self) -> &Arc<Mutex<TerminalState>> {
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

/// 检测系统可用的 UTF-8 locale 字符串（如 "zh_CN.UTF-8"）。
///
/// 优先级：
/// 1. macOS：读取 `defaults read .GlobalPreferences AppleLocale`，拼接 `.UTF-8`
/// 2. 兜底：`en_US.UTF-8`（几乎所有 Unix 系统都安装了该 locale）
///
/// 在 IDE（如 RustRover）中启动时，父进程往往没有 `LANG`/`LC_CTYPE`，
/// Terminal.app 会自动注入 locale 而 IDE 不会，导致子 shell 回退到 C locale。
fn detect_utf8_locale() -> Option<String> {
    // macOS：从系统偏好读取 AppleLocale
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("defaults")
            .args(["read", ".GlobalPreferences", "AppleLocale"])
            .output()
        {
            if output.status.success() {
                let s = String::from_utf8_lossy(&output.stdout);
                let locale = s.trim();
                if !locale.is_empty() {
                    return Some(format!("{}.UTF-8", locale));
                }
            }
        }
    }

    // 兜底：en_US.UTF-8 几乎在所有系统上可用
    Some("en_US.UTF-8".to_string())
}
