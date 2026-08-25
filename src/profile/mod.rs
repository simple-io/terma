//! profile — 终端配置文件管理
//!
//! Phase 6：从 <config_dir>/terma/profiles.toml 加载持久化配置；
//!           文件不存在时生成平台默认配置并落盘。支持 set_default_profile + 保存。

pub mod loader;
pub mod manager;
pub mod profile;

pub use manager::ProfileManager;
pub use profile::Profile;
