//! config — 终端配置文件管理
//!
//! 从 <config_dir>/terma/config.toml 加载持久化配置（通用配置 + Profile 列表）；
//! 文件不存在时生成平台默认配置并落盘。支持 Profile CRUD + 通用配置更新 + 保存。

pub mod color_scheme;
pub mod cursor_color;
pub mod fonts;
pub mod general;
pub mod loader;
pub mod manager;
pub mod profile;
pub mod shortcut;

pub use color_scheme::ColorScheme;
pub use cursor_color::CursorColor;
pub use general::{GeneralConfig, MAX_SCROLLBACK_LIMIT, MIN_SCROLLBACK_LIMIT};
pub use manager::ConfigManager;
pub use profile::Profile;
pub use shortcut::{ShortcutAction, ShortcutConfig};
