//! cell — 终端单元格

use crate::terminal::color;
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
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: ' ',
            foreground: Color::Default,
            background: Color::Default,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
            continuation: false,
        }
    }
}

impl Cell {
    /// 默认的续列占位单元格
    pub fn continuation_cell() -> Self {
        Self {
            continuation: true,
            ..Default::default()
        }
    }
    /// 应用 inverse 后用于渲染的实际颜色对
    pub fn resolved_colors(&self) -> ((u8, u8, u8), (u8, u8, u8)) {
        if self.inverse {
            // 取反：前景取背景色，背景取前景色
            (color::bg_rgb(self.background), color::fg_rgb(self.foreground))
        } else {
            (color::fg_rgb(self.foreground), color::bg_rgb(self.background))
        }
    }
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
        }
    }
}
