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
fn default_initial_rows() -> u32 {
    36
}
fn default_initial_cols() -> u32 {
    120
}
fn default_cursor_style() -> String {
    "block".into()
}
fn default_cursor_blink() -> bool {
    true
}
fn default_color_scheme() -> String {
    "Default".into()
}
fn default_scrollback_limit() -> usize {
    10000
}

/// 会话内容缓存数（scrollback 历史行数）下限：低于此值历史几乎无实用意义，
/// 且 `ScreenBuffer` 把 `scrollback_limit == 0` 视为「不记录历史」，故下限必须 > 0。
pub const MIN_SCROLLBACK_LIMIT: usize = 100;
/// 会话内容缓存数上限：每行按列数存放 Cell，过大的上限会造成明显的内存占用。
pub const MAX_SCROLLBACK_LIMIT: usize = 100000;

/// 把缓存行数钳制到 [MIN_SCROLLBACK_LIMIT, MAX_SCROLLBACK_LIMIT]。
pub fn clamp_scrollback_limit(limit: usize) -> usize {
    limit.clamp(MIN_SCROLLBACK_LIMIT, MAX_SCROLLBACK_LIMIT)
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

    // 终端初始尺寸：行数
    #[serde(default = "default_initial_rows")]
    pub initial_rows: u32,

    // 终端初始尺寸：列数
    #[serde(default = "default_initial_cols")]
    pub initial_cols: u32,

    // 终端光标样式
    #[serde(default = "default_cursor_style")]
    pub cursor_style: String,

    // 终端光标颜色（全局外观项，独立于配色方案）
    #[serde(default)]
    pub cursor_color: CursorColor,

    // 终端光标闪烁
    #[serde(default = "default_cursor_blink")]
    pub cursor_blink: bool,

    // 终端配色方案
    #[serde(default = "default_color_scheme")]
    pub color_scheme: String,

    // 配色方案列表
    #[serde(default)]
    pub color_schemes: Vec<ColorScheme>,

    // 会话内容缓存数：终端 scrollback 历史行数上限
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
            initial_rows: default_initial_rows(),
            initial_cols: default_initial_cols(),
            cursor_style: default_cursor_style(),
            cursor_color: CursorColor::default(),
            cursor_blink: default_cursor_blink(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_limit_is_clamped_to_allowed_range() {
        assert_eq!(clamp_scrollback_limit(0), MIN_SCROLLBACK_LIMIT);
        assert_eq!(
            clamp_scrollback_limit(MIN_SCROLLBACK_LIMIT - 1),
            MIN_SCROLLBACK_LIMIT
        );
        assert_eq!(
            clamp_scrollback_limit(MAX_SCROLLBACK_LIMIT + 1),
            MAX_SCROLLBACK_LIMIT
        );
        assert_eq!(clamp_scrollback_limit(5000), 5000);
    }

    #[test]
    fn default_scrollback_limit_within_allowed_range() {
        let d = default_scrollback_limit();
        assert_eq!(clamp_scrollback_limit(d), d);
    }
}
