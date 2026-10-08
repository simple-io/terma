//! pty — PTY 能力封装层
//!
//! 职责：
//!   - 基于 `portable-pty` 创建 PTY 并 spawn 子进程（args / cwd / env / UTF-8 locale）
//!   - 处理 master/slave 端，提供 reader（输出）与 writer（输入）句柄
//!   - 在独立 Output Thread 阻塞读 PTY，把原始字节经 `on_output` 回调上抛
//!   - 提供 write / resize / is_child_exited / shutdown 接口，统一封装 PTY 能力
//!
//! 线程模型：
//!   - PTY Output Thread 拥有 reader，绝不接触 Slint UI
//!   - 读到的原始字节经回调交由上层会话做 vte 解析、脏标记与 UI 唤醒
//!   - 关闭流程：kill child → wait → reader 收 EOF → join Output Thread

mod pty;

pub use pty::Pty;