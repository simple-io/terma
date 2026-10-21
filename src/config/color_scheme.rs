//! color_scheme — 配色方案（内置 + 自定义）
//!
//! 参考 Ghostty / iTerm2 / Windows Terminal 等成熟终端的经典配色。
//! 统一为一个 `ColorScheme` 类型：直接持有解析后的 RGB，供渲染层解析
//! ANSI 索引与默认色；配置持久化时以 `#RRGGBB` 字符串序列化。
//!
//! 每个方案统一描述：前景 / 背景 / 选区 + 16 色 ANSI 调色板（标准 8 色 + 亮 8 色）。
//! 光标色已独立到 [`crate::config::cursor_color::CursorColor`]（全局外观项）。

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 终端默认前景色（无配色方案时的兜底）
pub const DEFAULT_FG: (u8, u8, u8) = (195, 195, 195);
/// 终端默认背景色（无配色方案时的兜底）
pub const DEFAULT_BG: (u8, u8, u8) = (0, 0, 0);
/// 默认选区色（`Default` 实现兜底）
pub const DEFAULT_SELECTION: (u8, u8, u8) = (63, 63, 70);

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

/// 单个配色方案：前景 / 背景 / 选区 + 16 色 ANSI 调色板（RGB 已解析）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorScheme {
    /// 配色方案名称。
    pub name: String,
    /// 前景色。
    pub foreground: (u8, u8, u8),
    /// 背景色。
    pub background: (u8, u8, u8),
    /// 选区色。
    pub selection: (u8, u8, u8),
    /// 16 色 ANSI 调色板（下标 0..=15，缺失项回退 xterm 默认）。
    pub ansi: [(u8, u8, u8); 16],
}

impl Default for ColorScheme {
    fn default() -> Self {
        Self {
            name: String::new(),
            foreground: DEFAULT_FG,
            background: DEFAULT_BG,
            selection: DEFAULT_SELECTION,
            ansi: XTERM_16,
        }
    }
}

impl ColorScheme {
    /// 按名称取方案：自定义优先，内置兜底，最终回退首个内置方案。
    pub fn new_from_schemes(name: &str, schemes: &[ColorScheme]) -> ColorScheme {
        schemes
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            .cloned()
            .or_else(|| {
                builtins()
                    .into_iter()
                    .find(|s| s.name.eq_ignore_ascii_case(name))
            })
            .unwrap_or_else(|| schemes[0].clone())
    }

    /// 从磁盘 wire 表示解析：非法/缺失色值逐项回退默认，绝不 panic。
    fn from_wire(w: ColorSchemeWire) -> Self {
        let mut ansi = XTERM_16;
        for (i, s) in w.palette.iter().take(16).enumerate() {
            if let Some(c) = parse_hex(s) {
                ansi[i] = c;
            }
        }
        ColorScheme {
            name: w.name,
            foreground: parse_hex(&w.foreground).unwrap_or(DEFAULT_FG),
            background: parse_hex(&w.background).unwrap_or(DEFAULT_BG),
            // 与原 resolve() 行为一致：选区缺失回退默认前景色。
            selection: parse_hex(&w.selection).unwrap_or(DEFAULT_FG),
            ansi,
        }
    }
}

// ─── 序列化：色值以 `#RRGGBB` 字符串持久化，与历史 config.toml 格式保持一致 ───

/// 磁盘 wire 表示（字符串色值），仅用于 (反)序列化。
#[derive(Serialize, Deserialize)]
struct ColorSchemeWire {
    name: String,
    #[serde(default)]
    foreground: String,
    #[serde(default)]
    background: String,
    #[serde(default)]
    selection: String,
    #[serde(default)]
    palette: Vec<String>,
}

impl Serialize for ColorScheme {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = ColorSchemeWire {
            name: self.name.clone(),
            foreground: to_hex(self.foreground),
            background: to_hex(self.background),
            selection: to_hex(self.selection),
            palette: self.ansi.iter().map(|&c| to_hex(c)).collect(),
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ColorScheme {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ColorSchemeWire::deserialize(deserializer)?;
        Ok(ColorScheme::from_wire(wire))
    }
}

/// 解析 `#RRGGBB` 十六进制颜色为 RGB，非法返回 None。
/// 允许省略 `#`（编辑器手动输入 `ff0000` 亦可用）。
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

/// RGB → `#RRGGBB`（编辑器回填规范色值用）。
pub fn to_hex(c: (u8, u8, u8)) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// 内置配色方案（顺序即设置界面展示顺序，首项为默认）。
pub fn builtins() -> Vec<ColorScheme> {
    vec![
        scheme(
            "Default",
            "#c3c3c3", "#000000", "#3f3f46",
            &["#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5",
              "#7f7f7f", "#ff0000", "#00ff00", "#ffff00", "#5c5cff", "#ff00ff", "#00ffff", "#ffffff"],
        ),
        scheme(
            "Solarized Dark",
            "#839496", "#002b36", "#073642",
            &["#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
              "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3"],
        ),
        scheme(
            "Dracula",
            "#f8f8f2", "#282a36", "#44475a",
            &["#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
              "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff"],
        ),
        scheme(
            "Gruvbox Dark",
            "#ebdbb2", "#282828", "#3c3836",
            &["#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#5b4f8d", "#689d6a", "#a89984",
              "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2"],
        ),
        scheme(
            "Nord",
            "#d8dee9", "#2e3440", "#3b4252",
            &["#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
              "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4"],
        ),
        scheme(
            "One Dark",
            "#abb2bf", "#282c34", "#3e4451",
            &["#282c34", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
              "#545862", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#ffffff"],
        ),
        scheme(
            "Tokyo Night",
            "#c0caf5", "#1a1b26", "#33467c",
            &["#15161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6",
              "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5"],
        ),
        scheme(
            "Catppuccin Mocha",
            "#cdd6f4", "#1e1e2e", "#585b70",
            &["#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
              "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8"],
        ),
        scheme(
            "Solarized Light",
            "#657b83", "#fdf6e3", "#eee8d5",
            &["#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
              "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3"],
        ),
        scheme(
            "GitHub Light",
            "#24292f", "#ffffff", "#b6e3ff",
            &["#000000", "#cf222e", "#116329", "#4d2d00", "#0969da", "#8250df", "#1b7c83", "#6e7781",
              "#57606a", "#a40e26", "#1a7f37", "#633c01", "#218bff", "#a475f9", "#3192aa", "#8c959f"],
        ),
    ]
}

/// 内置 + 自定义合并后的完整方案列表（内置在前，自定义在后）。
pub fn all_schemes(customs: &[ColorScheme]) -> Vec<ColorScheme> {
    let mut v = builtins();
    v.extend(customs.iter().cloned());
    v
}

/// 便捷构造一个内置方案（palette 为 16 个 `#RRGGBB`）。
fn scheme(
    name: &str,
    foreground: &str,
    background: &str,
    selection: &str,
    palette: &[&str; 16],
) -> ColorScheme {
    let mut ansi = XTERM_16;
    for (i, s) in palette.iter().enumerate() {
        if let Some(c) = parse_hex(s) {
            ansi[i] = c;
        }
    }
    ColorScheme {
        name: name.into(),
        foreground: parse_hex(foreground).unwrap_or(DEFAULT_FG),
        background: parse_hex(background).unwrap_or(DEFAULT_BG),
        selection: parse_hex(selection).unwrap_or(DEFAULT_SELECTION),
        ansi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_non_empty_and_default_first() {
        let b = builtins();
        assert!(!b.is_empty());
        assert_eq!(b[0].name, "Default");
        // 每个内置方案的 16 色调色板均能无损解析
        for s in &b {
            assert_eq!(s.ansi.len(), 16, "方案 {} 调色板应 16 色", s.name);
            assert_ne!(s.background, (1, 1, 1), "方案 {} 背景色应有效", s.name);
        }
    }

    #[test]
    fn parse_hex_roundtrip() {
        assert_eq!(parse_hex("#1a2b3c"), Some((0x1a, 0x2b, 0x3c)));
        assert_eq!(parse_hex("1a2b3c"), Some((0x1a, 0x2b, 0x3c)));
        assert_eq!(parse_hex("#12345"), None);
        assert_eq!(parse_hex("#zzzzzz"), None);
        assert_eq!(to_hex((0x1a, 0x2b, 0x3c)), "#1a2b3c");
    }

    #[test]
    fn serde_roundtrip_preserves_hex() {
        let s = builtins()[0].clone();
        let t = toml::to_string_pretty(&s).unwrap();
        let back: ColorScheme = toml::from_str(&t).unwrap();
        assert_eq!(s, back);
    }
}
