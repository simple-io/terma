//! cursor — 终端光标

/// 表示光标位置和可见性，有两个坐标轴
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// 表示行坐标轴，光标在水平方向的位置
    pub row: usize,
    /// 表示列坐标轴，光标在垂直方向的位置
    pub col: usize,
    /// 是否可见
    pub visible: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            visible: true,
        }
    }
}

impl Cursor {
    pub fn new(row: usize, col: usize) -> Self {
        Self {
            row,
            col,
            visible: true,
        }
    }
}
