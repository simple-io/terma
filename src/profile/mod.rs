//! profile — 终端配置文件管理
//!
//! 从 <config_dir>/terma/config.toml 加载持久化配置（通用配置 + Profile 列表）；
//! 文件不存在时生成平台默认配置并落盘。支持 Profile CRUD + 通用配置更新 + 保存。
//! 兼容旧版 profiles.toml，自动迁移为 config.toml。

pub mod loader;
pub mod manager;
pub mod profile;

pub use loader::GeneralConfig;
pub use manager::ProfileManager;
pub use profile::Profile;
