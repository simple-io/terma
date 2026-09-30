//! loader — 统一配置文件持久化（config.toml）
//!
//! 配置文件位置：`<config_dir>/terma/config.toml`
//!   - macOS:  ~/Library/Application Support/terma/config.toml
//!   - Linux:  ~/.config/terma/config.toml
//!   - Windows: %APPDATA%\terma\config.toml
//!
//! 文件不存在时由 ProfileManager 生成平台默认配置并落盘。
//! 若检测到旧版 profiles.toml，自动迁移为 config.toml 后删除旧文件。

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::profile::Profile;
use crate::session::session_id::ProfileId;

// ─── 默认值函数 ───

fn default_font_family() -> String { crate::fonts::DEFAULT_FONT_FAMILY.into() }
fn default_font_size() -> u32 { 13 }
fn default_bg_color() -> String { "#000000".into() }
fn default_cursor_style() -> String { "block".into() }
fn default_color_scheme() -> String { "default".into() }
fn default_scrollback_limit() -> usize { 10000 }

// ─── 配置文件结构 ───

/// 完整配置文件结构（config.toml）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub profiles: Vec<ProfileEntry>,
}

/// 通用配置（应用级别）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    #[serde(default)]
    pub default_profile: String,
    #[serde(default = "default_font_family")]
    pub font_family: String,
    #[serde(default = "default_font_size")]
    pub font_size: u32,
    #[serde(default = "default_bg_color")]
    pub background_color: String,
    #[serde(default = "default_cursor_style")]
    pub cursor_style: String,
    #[serde(default = "default_color_scheme")]
    pub color_scheme: String,
    #[serde(default)]
    pub bell_sound: bool,
    #[serde(default = "default_scrollback_limit")]
    pub scrollback_limit: usize,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            default_profile: String::new(),
            font_family: default_font_family(),
            font_size: default_font_size(),
            background_color: default_bg_color(),
            cursor_style: default_cursor_style(),
            color_scheme: default_color_scheme(),
            bell_sound: false,
            scrollback_limit: default_scrollback_limit(),
        }
    }
}

/// Profile 条目（TOML 序列化用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileEntry {
    pub id: String,
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_vars: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_scheme: Option<String>,
}

// ─── 路径 ───

/// 返回配置文件路径（确保父目录存在）。
pub fn config_path() -> Result<PathBuf> {
    let cfg = dirs::config_dir().context("无法定位配置目录")?;
    let dir = cfg.join("terma");
    std::fs::create_dir_all(&dir).ok();
    Ok(dir.join("config.toml"))
}

/// 旧版配置文件路径（profiles.toml），用于迁移检测。
fn legacy_path() -> Result<PathBuf> {
    let cfg = dirs::config_dir().context("无法定位配置目录")?;
    Ok(cfg.join("terma").join("profiles.toml"))
}

// ─── 加载 ───

/// 从磁盘加载配置。
///
/// 优先读取 config.toml；若不存在则尝试迁移旧版 profiles.toml；
/// 两者都不存在时返回 None，由调用方生成默认配置。
pub fn load() -> Option<ConfigFile> {
    // 1. 尝试加载 config.toml
    if let Some(config) = load_config() {
        return Some(config);
    }

    // 2. 尝试从旧版 profiles.toml 迁移
    if let Some(config) = try_migrate_legacy() {
        return Some(config);
    }

    None
}

fn load_config() -> Option<ConfigFile> {
    let path = config_path().ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let mut config: ConfigFile = toml::from_str(&content).ok()?;

    // 迁移：旧版默认字体 Monaco → 新版默认字体（JetBrains Mono）。
    // Monaco 是旧版的隐式默认值（用户几乎不会手输），统一迁移到新默认。
    if config.general.font_family == "Monaco" {
        config.general.font_family = default_font_family();
    }

    Some(config)
}

/// 检测旧版 profiles.toml 并迁移为 config.toml。
fn try_migrate_legacy() -> Option<ConfigFile> {
    let legacy = legacy_path().ok()?;
    if !legacy.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&legacy).ok()?;

    // 旧版结构：default_profile 在顶层，profiles 为数组
    #[derive(Deserialize)]
    struct LegacyFile {
        default_profile: String,
        #[serde(default)]
        profiles: Vec<ProfileEntry>,
    }

    let legacy_file: LegacyFile = toml::from_str(&content).ok()?;
    let config = ConfigFile {
        general: GeneralConfig {
            default_profile: legacy_file.default_profile,
            ..Default::default()
        },
        profiles: legacy_file.profiles,
    };

    // 保存新格式并删除旧文件
    if save(&config).is_ok() {
        let _ = std::fs::remove_file(&legacy);
    }

    Some(config)
}

// ─── 保存 ───

/// 把完整配置写入磁盘。
pub fn save(config: &ConfigFile) -> Result<()> {
    let path = config_path()?;
    let s = toml::to_string_pretty(config).context("序列化 config.toml 失败")?;
    std::fs::write(&path, s).with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(())
}

// ─── 转换函数 ───

pub fn profile_to_entry(p: &Profile) -> ProfileEntry {
    ProfileEntry {
        id: p.id.0.clone(),
        name: p.name.clone(),
        command: p.command.clone(),
        args: p.args.clone(),
        working_directory: p.working_directory.clone(),
        env_vars: p.env_vars.clone(),
        icon: p.icon.clone(),
        color_scheme: p.color_scheme.clone(),
    }
}

pub fn entry_to_profile(e: &ProfileEntry) -> Profile {
    let mut p = Profile::new(e.id.clone(), e.name.clone(), e.command.clone());
    p.args = e.args.clone();
    p.working_directory = e.working_directory.clone();
    p.env_vars = e.env_vars.clone();
    p.icon = e.icon.clone();
    p.color_scheme = e.color_scheme.clone();
    p
}

// ─── env_vars 文本格式转换 ───

/// 把 env_vars 文本（KEY=VALUE 每行一对）解析为 BTreeMap。
pub fn parse_env_vars_text(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    map
}

/// 把 BTreeMap 格式化为 env_vars 文本（KEY=VALUE 每行一对）。
pub fn format_env_vars_text(map: &BTreeMap<String, String>) -> String {
    map.iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("\n")
}
