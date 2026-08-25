//! loader — Profile TOML 持久化
//!
//! 配置文件位置：`<config_dir>/terma/profiles.toml`
//!   - macOS:  ~/Library/Application Support/terma/profiles.toml
//!   - Linux:  ~/.config/terma/profiles.toml
//!   - Windows: %APPDATA%\terma\profiles.toml
//!
//! 文件不存在时由 ProfileManager 生成平台默认配置并落盘，便于用户编辑。

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::profile::Profile;
use crate::session::session_id::ProfileId;

/// TOML 文件结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileFile {
    pub default_profile: String,
    #[serde(default)]
    pub profiles: Vec<ProfileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileEntry {
    pub id: String,
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_scheme: Option<String>,
}

/// 返回配置文件路径（确保父目录存在）。
pub fn config_path() -> Result<PathBuf> {
    let cfg = dirs::config_dir().context("无法定位配置目录")?;
    let dir = cfg.join("terma");
    std::fs::create_dir_all(&dir).ok();
    Ok(dir.join("profiles.toml"))
}

/// 从磁盘加载配置；文件不存在或解析失败时返回 None。
pub fn load() -> Option<ProfileFile> {
    let path = config_path().ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}

/// 把 (profiles, default_id) 写入磁盘。
pub fn save(profiles: &[Profile], default_id: &ProfileId) -> Result<()> {
    let file = ProfileFile {
        default_profile: default_id.0.clone(),
        profiles: profiles.iter().map(profile_to_entry).collect(),
    };
    let path = config_path()?;
    let s = toml::to_string_pretty(&file).context("序列化 profiles.toml 失败")?;
    std::fs::write(&path, s).with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(())
}

fn profile_to_entry(p: &Profile) -> ProfileEntry {
    ProfileEntry {
        id: p.id.0.clone(),
        name: p.name.clone(),
        command: p.command.clone(),
        args: p.args.clone(),
        working_directory: p.working_directory.clone(),
        icon: p.icon.clone(),
        color_scheme: p.color_scheme.clone(),
    }
}

pub fn entry_to_profile(e: &ProfileEntry) -> Profile {
    let mut p = Profile::new(e.id.clone(), e.name.clone(), e.command.clone());
    p.args = e.args.clone();
    p.working_directory = e.working_directory.clone();
    p.icon = e.icon.clone();
    p.color_scheme = e.color_scheme.clone();
    p
}
