//! parser — vte::Parser 封装
//!
//! TerminalState 自己实现 Perform，Session 直接持有 Parser 与 State，
//! drain 时把字节逐个 feed 给 parser.advance(&mut state, byte)。

pub use vte::Parser;

// 保留模块以对应设计文档；当前无额外封装。
