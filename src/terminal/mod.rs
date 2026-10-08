//! terminal — 终端仿真核心
//!
//! 数据流：
//!   PTY Output → bytes → vte::Parser → TerminalState(impl Perform) → ScreenBuffer → Renderer → Slint
//!
//! 子模块：
//!   - cell     单元格与当前样式
//!   - color    ANSI16/256/TrueColor
//!   - cursor   光标
//!   - buffer   ScreenBuffer（Primary/Alternate + Scrollback）
//!   - state    TerminalState + vte::Perform 实现
//!   - parser   vte::Parser 封装

pub mod cell;
pub mod color;
pub mod control;
pub mod cursor;
pub mod parser;
pub mod row;
pub mod screen;
pub mod state;

pub use cell::{Cell, CellStyle};
pub use color::Color;
pub use cursor::Cursor;
pub use row::Row;
pub use screen::ScreenBuffer;
pub use state::TerminalState;
