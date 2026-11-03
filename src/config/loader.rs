//! loader — 统一配置文件持久化（config.toml）
//!
//! 配置文件位置：`<config_dir>/terma/config.toml`
//!   - macOS:  ~/Library/Application Support/terma/config.toml
//!   - Linux:  ~/.config/terma/config.toml
//!   - Windows: %APPDATA%\terma\config.toml
//!
//! 文件不存在时由 ProfileManager 生成平台默认配置并落盘。
//! 若检测到旧版 profiles.toml，自动迁移为 config.toml 后删除旧文件。

use std::path::PathBuf;

use crate::config;
use crate::config::shortcut;
use anyhow;
use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;

// 完整配置文件结构（config.toml）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    // 通用配置
    #[serde(default)]
    pub general: config::GeneralConfig,
    // 启动项配置
    #[serde(default)]
    pub profiles: Vec<config::Profile>,
    // 快捷键配置
    #[serde(default = "shortcut::default_shortcuts")]
    pub shortcuts: Vec<shortcut::ShortcutConfig>,
}

// 从磁盘加载配置。
pub fn load() -> Option<ConfigFile> {
    // 尝试加载 config.toml
    if let Some(config) = load_config() {
        return Some(config);
    }
    None
}

fn load_config() -> Option<ConfigFile> {
    let path = config_path().ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let config: ConfigFile = toml::from_str(&content).ok()?;
    Some(config)
}

// 返回配置文件路径（确保父目录存在）
pub fn config_path() -> anyhow::Result<PathBuf> {
    let cfg = dirs::config_dir().context("无法定位配置目录")?;
    let dir = cfg.join("terma");
    std::fs::create_dir_all(&dir).ok();
    Ok(dir.join("config.toml"))
}

// 把完整配置写入磁盘
pub fn save(config: &ConfigFile) -> anyhow::Result<()> {
    let path = config_path()?;
    let s = toml::to_string_pretty(config).context("序列化 config.toml 失败")?;
    std::fs::write(&path, s).with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(())
}
