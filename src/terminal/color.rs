//! color — 终端颜色
//!
//! 支持 ANSI 16 色 / 256 色 / TrueColor。
//! `Color::Default` 表示使用终端默认前景/背景（由调色板 `Palette` 决定）。
//!
//! 颜色解析（Default / Index）由当前配色方案的 `Palette` 驱动：
//!   - 前景 `Default` → `palette.foreground`；背景 `Default` → `palette.background`；
//!   - `Index(0..=15)` → `palette.ansi[i]`；`Index(16..)` → 标准 256 色立方体/灰度。

use slint::Brush;

/// 终端默认前景色（无配色方案时的兜底）
pub const DEFAULT_FG: (u8, u8, u8) = (195, 195, 195);
/// 终端默认背景色（无配色方案时的兜底）
pub const DEFAULT_BG: (u8, u8, u8) = (0, 0, 0);

/// 标准 xterm 16 色（下标 0..=15），作为内置 "Default" 方案与回退基准。
pub const XTERM_16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// 解析后的配色方案调色板（渲染层使用，Copy 便于随 Renderer 持有）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// 默认前景色
    pub foreground: (u8, u8, u8),
    /// 默认背景色
    pub background: (u8, u8, u8),
    /// 光标色
    pub cursor: (u8, u8, u8),
    /// 选区色
    pub selection: (u8, u8, u8),
    /// 16 色 ANSI 调色板（下标 0..=15）
    pub ansi: [(u8, u8, u8); 16],
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            foreground: DEFAULT_FG,
            background: DEFAULT_BG,
            cursor: (255, 255, 255),
            selection: (63, 63, 70),
            ansi: XTERM_16,
        }
    }
}

impl Palette {
    /// ANSI 索引 → RGB：0..=15 走调色板，16.. 走标准 256 色。
    pub fn ansi_rgb(&self, i: u8) -> (u8, u8, u8) {
        if (i as usize) < 16 {
            self.ansi[i as usize]
        } else {
            ansi256_to_rgb(i)
        }
    }
}

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

    /// 作为前景解析为 RGB（`Default` → 调色板前景）。
    pub fn fg_rgb(&self, p: &Palette) -> (u8, u8, u8) {
        match self {
            Color::Default => p.foreground,
            Color::Index(i) => p.ansi_rgb(*i),
            Color::Rgb(r, g, b) => (*r, *g, *b),
        }
    }

    /// 作为背景解析为 RGB（`Default` → 调色板背景）。
    pub fn bg_rgb(&self, p: &Palette) -> (u8, u8, u8) {
        match self {
            Color::Default => p.background,
            Color::Index(i) => p.ansi_rgb(*i),
            Color::Rgb(r, g, b) => (*r, *g, *b),
        }
    }

    /// 作为前景解析为 Brush。
    pub fn fg_brush(&self, p: &Palette) -> Brush {
        let (r, g, b) = self.fg_rgb(p);
        Brush::SolidColor(slint::Color::from_rgb_u8(r, g, b))
    }

    /// 作为背景解析为 Brush。
    pub fn bg_brush(&self, p: &Palette) -> Brush {
        let (r, g, b) = self.bg_rgb(p);
        Brush::SolidColor(slint::Color::from_rgb_u8(r, g, b))
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
