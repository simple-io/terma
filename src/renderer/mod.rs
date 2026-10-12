//! renderer — Terminal 渲染层
//!
//! 把 TerminalState/ScreenBuffer 映射为 Slint Model，供 TerminalView 渲染。
//! 每个会话持有独立的 RenderCells（render.rs）与会话私有的 `VecModel<CellRun>`：
//!   - 跟随模式按脏行增量更新 model（set_row_data/insert/remove），未变更行零开销；
//!   - 滚动模式以视口 key 做快照缓存，key 不变时仅增量更新落入视口的脏行；
//!   - 全量重建仅发生在 resize / 首帧 / 模式切换 / 大面积变更场景（单次 set_vec）。
//! 配合脏标记机制，空闲状态完全跳过渲染，大幅降低 CPU 与 UI 更新开销。

pub mod render;

pub use render::RenderCells;
