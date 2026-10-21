//! cell — 终端单元格

use slint::Brush;
use crate::config::ColorScheme;
use crate::terminal::color::Color;

/// 单个字符单元：字符 + 前景/背景色 + 文本样式（加粗、斜体、下划线）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    // 字符
    pub character: char,
    // 前景色
    pub foreground: Color,
    // 背景色
    pub background: Color,
    // 是否粗体
    pub bold: bool,
    // 是否斜体
    pub italic: bool,
    // 是否下划线
    pub underline: bool,
    // 是否颜色取反
    pub inverse: bool,
    // 宽字符（显示宽度=2）的续列占位：本身不渲染可见字符，
    pub continuation: bool,
    // 显示宽度：0/1/2
    width: usize,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: '\x00',
            foreground: Color::Default,
            background: Color::Default,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
            continuation: false,
            width: 0,
        }
    }
}

impl Cell {
    /// 创建一个默认样式的字符单元
    pub fn new(character: char) -> Self {
        Self {
            character,
            width: char_width(character),
            ..Default::default()
        }
    }

    /// 默认的续列占位单元格
    pub fn continuation_cell() -> Self {
        Self {
            continuation: true,
            ..Default::default()
        }
    }

    /// 应用 inverse 后用于渲染的实际颜色对（按配色方案解析 Default/Index）。
    pub fn resolved_colors(&self, s: &ColorScheme) -> ((u8, u8, u8), (u8, u8, u8)) {
        if self.inverse {
            // 颜色取反：前景取背景色，背景取前景色
            (self.background.bg_rgb(s), self.foreground.fg_rgb(s))
        } else {
            // 正常：前景取前景色，背景取背景色
            (self.foreground.fg_rgb(s), self.background.bg_rgb(s))
        }
    }

    /// 根据 inverse 设置区分后渲染的颜色（按配色方案解析 Default/Index）。
    pub fn resolved_colors_brush(&self, s: &ColorScheme) -> (Brush, Brush) {
        if self.inverse {
            // 颜色取反：前景取背景色，背景取前景色
            (self.background.bg_brush(s), self.foreground.fg_brush(s))
        } else {
            // 正常：前景取前景色，背景取背景色
            (self.foreground.fg_brush(s), self.background.bg_brush(s))
        }
    }

    /// 字符的显示宽度 0/1/2
    pub fn char_width(&self) -> usize {
        self.width
    }

    /// 是否为零宽字符（显示宽度=0）
    pub fn is_zero_width(&self) -> bool {
        self.width == 0
    }

    /// 是否为普通字符（显示宽度=1）
    pub fn is_normal_char(&self) -> bool {
        self.width == 1
    }
    /// 是否为宽字符（显示宽度=2）
    pub fn is_wide_char(&self) -> bool {
        self.width == 2
    }

    /// 是否为续列占位格
    pub fn is_continuation(&self) -> bool {
        self.continuation
    }

    /// 是否为「内容格」：非续列占位、非零宽空、非空白填充。
    pub fn is_content(&self) -> bool {
        !self.continuation && !self.is_zero_width() && !self.is_blank()
    }

    /// 是否为「空白填充格」：默认样式的空格，渲染时不产生可见内容。
    /// 与续列占位、零宽空 cell（`\0`）一样，reflow 时可作为尾部填充裁剪，
    /// 但「有样式（如背景色）的空格」不属于填充，必须保留。
    pub fn is_blank(&self) -> bool {
        self.character == '\x00'
            && self.foreground == Color::Default
            && self.background == Color::Default
            && !self.bold
            && !self.italic
            && !self.underline
            && !self.inverse
    }
}

/// 计算多个字符单元的显示宽度总和。
pub fn cells_width(cells: &Vec<Cell>) -> usize {
    cells
        .iter()
        .filter(|c| !c.continuation && !c.is_zero_width() && !c.is_blank())
        .map(|c| c.char_width())
        .sum()
}

/// 过滤掉续列占位、零宽空 cell（`\0`），提取有内容的 cell 集合
pub fn cells_content(cells: &Vec<Cell>) -> Vec<Cell> {
    cells
        .iter()
        .filter(|c| !c.continuation && !c.is_zero_width() && !c.is_blank())
        .copied()
        .collect()
}

/// 返回单个字符的显示宽度（0/1/2）。
///   - 0：组合字符、零宽字符（不占列）
///   - 1：普通半角字符
///   - 2：CJK 等 East Asian Wide / Fullwidth 字符
pub fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// 当前写入样式（SGR 状态）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    // 前景色
    pub foreground: Color,
    // 背景色
    pub background: Color,
    // 是否粗体
    pub bold: bool,
    // 是否斜体
    pub italic: bool,
    // 是否下划线
    pub underline: bool,
    // 是否颜色取反
    pub inverse: bool,
}

impl CellStyle {
    /// 把当前样式写入一个 Cell 的字符
    pub fn apply(&self, c: char) -> Cell {
        Cell {
            character: c,
            foreground: self.foreground,
            background: self.background,
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            inverse: self.inverse,
            continuation: false,
            width: char_width(c),
        }
    }
}
