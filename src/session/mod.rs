//! session — 会话管理
//!
//! Phase 1：每个 Tab 对应一个 mock TerminalSession（仅持有 id / title / config name）。
//! Phase 2+ 将把 mock 替换为真实 TerminalSession：
//!   - portable-pty 创建 PTY
//!   - vte::Parser 解析输出
//!   - TerminalState / ScreenBuffer 维护终端状态
//!
//! 核心架构约束（后续阶段必须遵守）：
//!   One Tab == One TerminalSession == One PTY + Child + Parser + State + ScreenBuffer
//!   多个 TerminalSession 完全隔离，严禁共享上述任何资源。

pub mod active;
pub mod manager;
pub mod session;
pub mod session_id;

pub use active::ActiveSession;
pub use manager::SessionManager;
pub use session::{CursorRender, RenderOutput, ScrollAction, ScrollRender, TerminalSession};
