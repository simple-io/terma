//! cell — 终端单元格

use crate::terminal::color::Color;

/// 一个字符单元：字符 + 前景/背景色 + 文本样式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub character: char,
    pub foreground: Color,
    pub background: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
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
        }
    }
}

impl Cell {
    /// 应用 inverse 后用于渲染的实际颜色对
    pub fn resolved_colors(
        &self,
        default_fg: (u8, u8, u8),
        default_bg: (u8, u8, u8),
    ) -> ((u8, u8, u8), (u8, u8, u8)) {
        if self.inverse {
            // 反相：前景取背景色，背景取前景色
            let fg = crate::terminal::color::bg_rgb(self.background, default_fg);
            let bg = crate::terminal::color::fg_rgb(self.foreground, default_bg);
            (fg, bg)
        } else {
            let fg = crate::terminal::color::fg_rgb(self.foreground, default_fg);
            let bg = crate::terminal::color::bg_rgb(self.background, default_bg);
            (fg, bg)
        }
    }
}

/// 当前写入样式（SGR 状态）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub foreground: Color,
    pub background: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
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
        }
    }
}
