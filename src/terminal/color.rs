//! color — 终端颜色
//!
//! 支持 ANSI 16 色 / 256 色 / TrueColor。
//! `Color::Default` 表示使用终端默认前景/背景（由配色方案 `ColorScheme` 决定）。
//!
//! 颜色解析（Default / Index）由当前配色方案 `ColorScheme` 驱动：
//!   - 前景 `Default` → `scheme.foreground`；背景 `Default` → `scheme.background`；
//!   - `Index(0..=15)` → `scheme.ansi[i]`；`Index(16..)` → 标准 256 色立方体/灰度。

use slint::Brush;

use crate::config::ColorScheme;

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

    /// 作为前景解析为 RGB（`Default` → 配色方案前景）。
    pub fn fg_rgb(&self, s: &ColorScheme) -> (u8, u8, u8) {
        match self {
            Color::Default => s.foreground,
            Color::Index(i) => index_rgb(s, *i),
            Color::Rgb(r, g, b) => (*r, *g, *b),
        }
    }

    /// 作为背景解析为 RGB（`Default` → 配色方案背景）。
    pub fn bg_rgb(&self, s: &ColorScheme) -> (u8, u8, u8) {
        match self {
            Color::Default => s.background,
            Color::Index(i) => index_rgb(s, *i),
            Color::Rgb(r, g, b) => (*r, *g, *b),
        }
    }

    /// 作为前景解析为 Brush。
    pub fn fg_brush(&self, s: &ColorScheme) -> Brush {
        let (r, g, b) = self.fg_rgb(s);
        Brush::SolidColor(slint::Color::from_rgb_u8(r, g, b))
    }

    /// 作为背景解析为 Brush。
    pub fn bg_brush(&self, s: &ColorScheme) -> Brush {
        let (r, g, b) = self.bg_rgb(s);
        Brush::SolidColor(slint::Color::from_rgb_u8(r, g, b))
    }
}

/// ANSI 索引 → RGB：0..=15 走配色方案调色板，16.. 走标准 256 色。
fn index_rgb(s: &ColorScheme, i: u8) -> (u8, u8, u8) {
    if (i as usize) < 16 {
        s.ansi[i as usize]
    } else {
        ansi256_to_rgb(i)
    }
}

/// 256 色调色板 → RGB
fn ansi256_to_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        // 标准 16 色（xterm 风格，默认方案与 XTERM_16 一致）
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
        // 6x6x6 立方体（16..=231），color_id = 16 + R*36 + G*6 + B
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
