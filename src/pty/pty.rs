//! pty — PTY 能力封装层
//!
//! `Pty` 统一封装与 PTY 直接相关的全部逻辑，供上层会话（TerminalSession）编排：
//!   - openpty 创建主/从端，按 Profile 构建子进程命令（args / cwd / env / UTF-8 locale）
//!   - 在 slave 端 spawn 子进程，随后丢弃 slave（否则 master reader 收不到 EOF）
//!   - take_writer / try_clone_reader 获取读写句柄
//!   - 启动 Output Thread 阻塞读 PTY，把每段原始字节回调给上层（`on_output`）
//!   - 提供 write / resize / is_child_exited / shutdown 接口
//!
//! 线程模型：
//!   - Output Thread 拥有 reader，绝不接触 Slint / SessionManager；
//!     读到的原始字节经 `on_output` 回调交由上层做 vte 解析与渲染唤醒。
//!   - 关闭流程：kill child → wait child → reader 收 EOF 退出 → join Output Thread。
//!   - 自然退出通知：Output Thread 因 EOF（`Ok(0)`）或不可恢复错误退出时，经 `on_exit`
//!     回调通知上层关闭会话；主动 `shutdown` 会置位 `closing` 抑制该通知，避免重复关闭。
//!
//! 说明：vte 解析、TerminalState 维护、脏标记与 UI 唤醒属于「终端状态协作」职责，
//! 由上层会话通过 `on_output` 闭包实现，`Pty` 只负责 PTY 本身。

use std::io::{ErrorKind, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use crate::config::Profile;
use anyhow::{Context, Result};
use portable_pty::ChildKiller;

/// PTY 封装：一个 PTY 主端 + 一个子进程 + 一个读取输出的 Output Thread。
pub struct Pty {
    // PTY 主端：resize 用。take_writer / try_clone_reader 之后仍可调用 resize。
    master: Box<dyn portable_pty::MasterPty + Send>,
    // PTY 写入端：转发用户键盘输入。仅 UI 线程访问（&mut）。
    writer: Box<dyn std::io::Write + Send>,
    // 子进程句柄：关闭时 kill + wait。
    child: Box<dyn portable_pty::Child + Send + Sync>,
    // Output Thread 句柄：关闭时 join。
    output_thread: Option<JoinHandle<()>>,
    // 主动关闭标记：非主动关闭时触发的会话退出通知，避免重复关闭。
    closing: Arc<AtomicBool>,
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Pty {
    /// 创建 PTY 并启动子进程与 Output Thread：
    /// openpty → spawn shell → take writer/reader → 启动读取线程。
    ///
    /// `rows` / `cols` 为初始终端网格，由调用方根据当前主窗口实际尺寸计算得出，
    /// 确保 PTY 与渲染层尺寸一致。
    /// `thread_name` 为 Output Thread 名称（便于诊断）。
    /// `on_output` 在 Output Thread 中被调用，接收每段从 PTY 读到的原始字节，
    /// 由上层完成 vte 解析、脏标记与（激活会话的）UI 唤醒。
    /// `on_exit` 在 Output Thread 因 EOF（`Ok(0)`）或不可恢复读取错误（`Err(_)`）
    /// 退出读循环时被调用一次，用于通知上层关闭对应会话；若退出源于上层主动
    /// `shutdown`（已置位 `closing`），则不会调用，避免与主动关闭重复。
    pub fn spawn<F, G>(
        rows: u16,
        cols: u16,
        profile: &Profile,
        thread_name: String,
        mut on_output: F,
        on_exit: G,
    ) -> Result<Self>
    where
        F: FnMut(&[u8]) + Send + 'static,
        G: FnOnce() + Send + 'static,
    {
        // 构造子进程命令
        let mut cmd = portable_pty::CommandBuilder::new(&profile.command);
        for arg in &profile.args {
            cmd.arg(arg);
        }
        // 切换工作目录
        if let Some(cwd) = &profile.working_directory {
            cmd.cwd(cwd);
        }
        // 注入自定义环境变量
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

        // 创建 PTY 主从端对
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openPty failed")?;

        // 启动 pty 的 shell
        let child = pair
            .slave
            .spawn_command(cmd)
            .context("spawn_command failed")?;

        // 关键：丢弃 slave 端，否则 master 的 reader 永远收不到 EOF（子进程退出时）。
        drop(pair.slave);

        // 获取 pty 的 writer
        let writer = pair.master.take_writer().context("take_writer failed")?;
        // 获取 pty 的 reader
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("try_clone_reader failed")?;

        // 主动关闭标记
        let closing = Arc::new(AtomicBool::new(false));
        let closing_thread = closing.clone();

        // 独立线程响应 pty 输出，把原始字节回调给上层解析
        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        // EOF：子进程已退出或会话结束，跳出读循环触发退出通知
                        Ok(0) => break,
                        // 正常读取字节块
                        Ok(n) => on_output(&buf[..n]),
                        // 非阻塞读取（EAGAIN/EWOULDBLOCK），退避后继续轮询
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(std::time::Duration::from_millis(5));
                            continue;
                        }
                        // 被信号中断（EINTR），数据流完好，立即重试
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        // 不可恢复读取错误，视为会话结束，跳出读循环触发退出通知
                        Err(_) => break,
                    }
                }
                // 如果不是主动关闭（没有设置标志位），而是出现 EOF 或不可恢复错误退出，则触发关闭通知
                if !closing_thread.load(Ordering::Acquire) {
                    on_exit();
                }
            })
            .context("spawn output thread failed")?;

        Ok(Self {
            master: pair.master,
            writer,
            child,
            output_thread: Some(handle),
            closing,
        })
    }

    /// 向 PTY 写入用户输入（键盘）。
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()
    }

    /// 调整 PTY 尺寸（rows/cols）。终端 ScreenBuffer 的同步由上层负责。
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.master.resize(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// 检查子进程是否已退出（非阻塞）。
    pub fn child_exited(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(_) => true,
        }
    }

    // 关闭 PTY：kill 子进程 → 等待退出 → reader 收 EOF 退出 → join 输出线程。
    pub fn shutdown(&mut self) {
        // 设置主动关闭标志，防止 Output Thread 重复触发会话退出通知
        self.closing.store(true, Ordering::Release);
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
