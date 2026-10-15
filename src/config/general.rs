use crate::config::fonts;
use serde::{Deserialize, Serialize};

fn default_font_family() -> String {
    fonts::DEFAULT_FONT_FAMILY.into()
}
fn default_font_size() -> u32 {
    12
}
fn default_bg_color() -> String {
    "#000000".into()
}
fn default_cursor_style() -> String {
    "block".into()
}
fn default_color_scheme() -> String {
    "default".into()
}
fn default_scrollback_limit() -> usize {
    10000
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
    #[serde(default = "default_scrollback_limit")]
    pub scrollback_limit: usize,
    #[serde(default)]
    pub bell_sound: bool,
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
            scrollback_limit: default_scrollback_limit(),
            bell_sound: false,
        }
    }
}

impl GeneralConfig {
    pub fn new() -> Self {
        Self::default()
    }
}
