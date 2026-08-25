//! renderer — Terminal 渲染层
//!
//! 把 TerminalState/ScreenBuffer 映射为 Slint Model，供 TerminalView 渲染。
//! Phase 4 起逐步优化 dirty-row / frame-batching；当前阶段每帧整体重建 Run 列表。

pub mod render_model;

pub use render_model::{build_runs, RunRender};
