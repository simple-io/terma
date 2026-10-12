//! color — 终端颜色
//!
//! 支持 ANSI 16 色 / 256 色 / TrueColor。
//! `Color::Default` 表示使用终端默认前景/背景（由渲染层决定）。

use slint::Brush;

/// 终端默认前景色（渲染层也会用到）
pub const DEFAULT_FG: (u8, u8, u8) = (230, 230, 230);
/// 终端默认背景色（渲染层也会用到）
pub const DEFAULT_BG: (u8, u8, u8) = (12, 12, 12);

/// 一个 Cell 的颜色，通常表示一个可见字符的颜色
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// 终端默认色（前景或背景，由上下文决定）
    Default,
    /// ANSI 16/256 色索引
    Index(u8),
    /// TrueColor RGB
    Rgb(u8, u8, u8),
}

impl Default for Color {
    fn default() -> Self {
        Color::Default
    }
}

impl Color {
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Color::Rgb(r, g, b)
    }

    // 转换为 RGB
    pub fn to_rgb(&self) -> (u8, u8, u8) {
        match self {
            Color::Default => DEFAULT_FG,
            Color::Index(i) => ansi256_to_rgb(*i),
            Color::Rgb(r, g, b) => (*r, *g, *b),
        }
    }

    // 转换为 Brush
    pub fn to_brush(&self) -> Brush {
        let rgb = self.to_rgb();
        Brush::SolidColor(slint::Color::from_rgb_u8(rgb.0, rgb.1, rgb.2))
    }
}

// /// 前景色 → RGB（Default 用 default_fg）
// pub fn fg_rgb(color: Color) -> (u8, u8, u8) {
//     match color {
//         Color::Default => DEFAULT_FG,
//         Color::Index(i) => ansi256_to_rgb(i),
//         Color::Rgb(r, g, b) => (r, g, b),
//     }
// }
//
// /// 背景色 → RGB（Default 用 default_bg）
// pub fn bg_rgb(color: Color) -> (u8, u8, u8) {
//     match color {
//         Color::Default => DEFAULT_BG,
//         Color::Index(i) => ansi256_to_rgb(i),
//         Color::Rgb(r, g, b) => (r, g, b),
//     }
// }

/// 256 色调色板 → RGB
fn ansi256_to_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        // 标准 16 色（xterm 风格）
        0 => (0, 0, 0),
        1 => (205, 0, 0),
        2 => (0, 205, 0),
        3 => (205, 205, 0),
        4 => (0, 0, 238),
        5 => (205, 0, 205),
        6 => (0, 205, 205),
        7 => (229, 229, 229),
        8 => (127, 127, 127),
        9 => (255, 0, 0),
        10 => (0, 255, 0),
        11 => (255, 255, 0),
        12 => (92, 92, 255),
        13 => (255, 0, 255),
        14 => (0, 255, 255),
        15 => (255, 255, 255),
        // 6x6x6 立方体（16..=231）
        16..=231 => {
            let i = i - 16;
            let r = (i / 36) % 6;
            let g = (i / 6) % 6;
            let b = i % 6;
            (level(r), level(g), level(b))
        }
        // 灰度（232..=255）
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// 6 立方体单通道值映射
fn level(v: u8) -> u8 {
    if v == 0 { 0 } else { 55 + v * 40 }
}
