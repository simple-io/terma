use crate::config::color_scheme::ColorScheme;
use crate::config::cursor_color::CursorColor;
use crate::config::fonts;
use serde::{Deserialize, Serialize};

fn default_font_family() -> String {
    fonts::DEFAULT_FONT_FAMILY.into()
}
fn default_font_size() -> u32 {
    12
}
fn default_cursor_style() -> String {
    "block".into()
}
fn default_color_scheme() -> String {
    "Default".into()
}
fn default_scrollback_limit() -> usize {
    10000
}

/// 通用配置（应用级别）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    // 默认启动项
    #[serde(default)]
    pub default_profile: String,

    // 终端字体类型
    #[serde(default = "default_font_family")]
    pub font_family: String,

    // 终端字体大小
    #[serde(default = "default_font_size")]
    pub font_size: u32,

    // 终端光标样式
    #[serde(default = "default_cursor_style")]
    pub cursor_style: String,

    // 终端光标颜色（全局外观项，独立于配色方案）
    #[serde(default)]
    pub cursor_color: CursorColor,

    // 终端配色方案
    #[serde(default = "default_color_scheme")]
    pub color_scheme: String,

    // 配色方案列表
    #[serde(default)]
    pub color_schemes: Vec<ColorScheme>,

    // 终端滚动缓冲区行数限制
    #[serde(default = "default_scrollback_limit")]
    pub scrollback_limit: usize,

    // 终端 bell 声音
    #[serde(default)]
    pub bell_sound: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            default_profile: String::new(),
            font_family: default_font_family(),
            font_size: default_font_size(),
            cursor_style: default_cursor_style(),
            cursor_color: CursorColor::default(),
            color_scheme: default_color_scheme(),
            color_schemes: Vec::new(),
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
