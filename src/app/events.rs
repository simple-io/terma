//! events — 跨线程事件类型（Phase 2+ 使用）
//!
//! Phase 1 占位。Phase 2 接入 PTY Output Thread 后，
//! PTY 线程通过 channel 把 TerminalEvent 发送到 UI 线程，
//! UI 线程据此更新 ScreenBuffer 并触发重渲染。

#[derive(Debug, Clone)]
pub enum TerminalEvent {
    /// PTY 有数据到达（Phase 2 由 vte 解析后转为 state 更新，而非直接传字节）
    Data(SessionId, Vec<u8>),
    /// 终端状态已更新，需要重绘
    Dirty(SessionId),
    /// 子进程退出
    ChildExited(SessionId, Option<i32>),
}

#[allow(unused_imports)]
use crate::session::session_id::SessionId;
