//! app — 应用控制器层
//!
//! 职责：
//!   - 持有 SessionManager 与 ProfileManager
//!   - 作为 Slint MainWindow 与业务层之间的桥梁
//!   - 处理 UI 回调（new-tab / close-tab / select-tab / select-profile）
//!   - 把 SessionManager 状态同步到 Slint Model
//!
//! 分层边界：
//!   UI 层（Slint） ⇄ App ⇄ SessionManager / ProfileManager
//!   PTY / vte / TerminalState 严禁出现在 UI 层。

pub mod controller;
pub mod events;

pub use controller::App;
