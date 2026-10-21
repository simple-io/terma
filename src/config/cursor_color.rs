//! cursor_color — 光标颜色（全局外观项）
//!
//! 光标色已从配色方案 `ColorScheme` 中独立出来：它是全局外观设置
//! （与光标样式 `cursor_style` 同级），不再随单个配色方案变化。
//! 持久化时以 `#RRGGBB` 字符串序列化，与配色方案色值格式一致。

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::color_scheme::{parse_hex, to_hex};

/// 默认光标色（沿用原 "Default" 内置方案的光标色）。
pub const DEFAULT_CURSOR: (u8, u8, u8) = (255, 255, 255);

/// 光标颜色配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorColor {
    /// 光标 RGB 颜色。
    pub color: (u8, u8, u8),
}

impl Default for CursorColor {
    fn default() -> Self {
        Self { color: DEFAULT_CURSOR }
    }
}

impl Serialize for CursorColor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&to_hex(self.color))
    }
}

impl<'de> Deserialize<'de> for CursorColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self {
            color: parse_hex(&s).unwrap_or(DEFAULT_CURSOR),
        })
    }
}
