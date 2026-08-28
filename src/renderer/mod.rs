//! renderer — Terminal 渲染层
//!
//! 把 TerminalState/ScreenBuffer 映射为 Slint Model，供 TerminalView 渲染。
//! 配合脏标记机制，仅在 TerminalState 变更时触发 build_runs，
//! 空闲状态下完全跳过重建，大幅降低 CPU 占用。

pub mod render_model;

pub use render_model::{build_runs, build_row_runs, RunRender};
