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

pub mod buffer;
pub mod cell;
pub mod color;
pub mod cursor;
pub mod parser;
pub mod state;
pub mod control;

pub use buffer::{Row, ScreenBuffer};
pub use cell::{Cell, CellStyle, char_width};
pub use color::Color;
pub use cursor::Cursor;
pub use state::{TerminalState};
