//! renderer — Terminal 渲染层
//!
//! 把 TerminalState/ScreenBuffer 映射为 Slint Model，供 TerminalView 渲染。
//! 每个会话持有独立的 RenderCells（render.rs），独立计算与按行缓存；
//! 配合脏标记机制，仅在 TerminalState 变更时重建脏行，空闲状态完全跳过，
//! 大幅降低 CPU 占用。

pub mod render;
pub mod render_model;

pub use render::RenderCells;
pub use render_model::{RunRender, build_row_runs, build_runs, build_viewport_runs};
