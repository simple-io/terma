//! fonts — 内置字体元信息与系统字体枚举
//!
//! 内置字体的数据内嵌由 Slint 编译器完成，在 src/ui/main_window.slint 中的
//! import 语句让 Slint 以 FileData 形式把字体静态编译进二进制，并在窗口
//! 初始化时注册到字体系统。本模块仅提供：
//!   - 默认字体 family 常量（JetBrains Mono）
//!   - 内置字体 family 列表（手动维护，在 main_window.slint 中导入）
//!   - 操作系统已安装字体 family 列表（font-kit，进程内缓存）

use font_kit::family_name::FamilyName;
use font_kit::handle::Handle;
use font_kit::properties::Properties;
use font_kit::source::SystemSource;
use std::sync::Arc;
use std::sync::OnceLock;

/// 内置字体，在 main_window.slint 中导入
static BUNDLED_FONT_FAMILIES: &[&str] = &["JetBrains Mono", "Cascadia Code"];

/// 项目默认字体：未选择其他字体时 UI 使用该值（需与 fonts/ 内嵌字体一致）
pub const DEFAULT_FONT_FAMILY: &str = "JetBrains Mono";

/// 内置字体 family 列表
pub fn bundled_families() -> Vec<String> {
    BUNDLED_FONT_FAMILIES
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
}

/// 操作系统已安装字体 family 列表（不区分大小写去重、按字母排序，进程内缓存）。
fn system_families_uncached() -> Vec<String> {
    let mut families = SystemSource::new().all_families().unwrap_or_default();
    families.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    families.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    families
}

pub fn system_families() -> Vec<String> {
    static CACHE: OnceLock<Vec<String>> = OnceLock::new();
    CACHE.get_or_init(system_families_uncached).clone()
}

/// 字体下拉框候选：内置列表, 系统列表
pub fn candidate_families() -> (Vec<String>, Vec<String>) {
    let bundled = bundled_families();
    let mut system = system_families();
    system.retain(|s| !bundled.iter().any(|b| b.eq_ignore_ascii_case(s)));
    (bundled, system)
}

/// 字体下拉框模糊匹配：关键词是否命中 family（部分匹配、不区分大小写）。
/// 空关键词或仅空白视为匹配全部（恢复完整列表）。
pub fn fuzzy_match(family: &str, keyword: &str) -> bool {
    let kw = keyword.trim().to_lowercase();
    kw.is_empty() || family.to_lowercase().contains(&kw)
}

// ─── 字体度量 ─────────────────────────────────────────────────────────────
//
// 终端网格的字符宽高由「真实字体度量」推导，而非固定经验值：
//   - char_w = 单字符 advance 宽度（等宽字体每字符同宽，取 '0' 为代表）
//   - char_h = 行高 = (ascent + |descent| + line_gap) / units_per_em * 字号
// 内置字体直接加载内嵌字节，保证与 Slint 实际渲染字体一致；系统字体走 SystemSource。

/// 内嵌内置字体字节（与 main_window.slint 中 import 的字体一一对应）。
/// 度量只需 Regular 字重：等宽字体的 advance 与行高在字重间一致。
const JETBRAINS_MONO_REGULAR: &[u8] = include_bytes!("../../fonts/JetBrainsMono-Regular.ttf");
const CASCADIA_CODE: &[u8] = include_bytes!("../../fonts/CascadiaCode.ttf");

/// 等宽字符度量（逻辑像素）。
#[derive(Debug, Clone, Copy)]
pub struct FontMetrics {
    /// 单字符 advance 宽度（逻辑像素）
    pub char_w: f32,
    /// 行高（逻辑像素）
    pub char_h: f32,
}

/// 加载指定 family 的字体用于度量：内置字体读内嵌字节，系统字体走 SystemSource。
fn load_font_for_measure(family: &str) -> Option<font_kit::font::Font> {
    let bytes: &[u8] = if family.eq_ignore_ascii_case("JetBrains Mono") {
        JETBRAINS_MONO_REGULAR
    } else if family.eq_ignore_ascii_case("Cascadia Code") {
        CASCADIA_CODE
    } else {
        // 系统字体，按 family 名匹配
        return SystemSource::new()
            .select_best_match(&[FamilyName::Title(family.to_string())], &Properties::new())
            .ok()
            .and_then(|h| h.load().ok());
    };
    Handle::from_memory(Arc::new(bytes.to_vec()), 0).load().ok()
}

/// 计算等宽字体在指定字号（逻辑像素）下的真实字符宽度与行高。
///
/// 加载失败时依次回退：内置默认字体 → 经验值（0.6em 宽 / 1.4em 高）。
pub fn measure_monospace(family: &str, size_px: u32) -> FontMetrics {
    let size = size_px.max(1) as f32;
    let font = load_font_for_measure(family).or_else(|| load_font_for_measure(DEFAULT_FONT_FAMILY));
    let Some(font) = font else {
        return FontMetrics {
            char_w: size * 0.6,
            char_h: size * 1.4,
        };
    };

    let metrics = font.metrics();
    let em = metrics.units_per_em.max(1) as f32;

    // 计算字符宽度，等宽字体任意 ASCII 字符 advance 相同，取 '0' 作为代表；极端情况回退 0.5em。
    let advance = font
        .glyph_for_char('0')
        .and_then(|g| font.advance(g).ok())
        .map(|v| v.x())
        .unwrap_or(em * 0.5);
    let char_w = advance / em * size;

    // 计算行高，行高 = ascent + |descent| + line_gap（descent 在 font-kit 中为负值）。
    let line_height = (metrics.ascent + metrics.descent.abs() + metrics.line_gap) / em * size;
    // 行高兜底不小于 1.1em，避免异常字体导致行间重叠。
    let char_h = line_height.max(size * 1.1);

    FontMetrics { char_w, char_h }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_families_default_first() {
        let families = bundled_families();
        assert_eq!(
            families.first().map(String::as_str),
            Some(DEFAULT_FONT_FAMILY)
        );
        assert!(
            families.len() >= 2,
            "应至少包含 JetBrains Mono 与 Cascadia Code"
        );
    }

    #[test]
    fn system_families_non_empty() {
        let families = system_families();
        assert!(!families.is_empty(), "系统字体枚举不应为空");
        // 排序 + 去重不变式
        let sorted = {
            let mut v = families.clone();
            v.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
            v
        };
        assert_eq!(families, sorted);
    }

    #[test]
    fn candidates_no_overlap_between_bundled_and_system() {
        let (bundled, system) = candidate_families();
        assert_eq!(
            bundled.first().map(String::as_str),
            Some(DEFAULT_FONT_FAMILY)
        );
        for b in &bundled {
            assert!(
                !system.iter().any(|s| s.eq_ignore_ascii_case(b)),
                "系统列表不应与内置字体重复：{b}"
            );
        }
    }

    #[test]
    fn fuzzy_match_case_and_partial() {
        // 部分匹配 + 不区分大小写
        assert!(fuzzy_match("JetBrains Mono", "jet"));
        assert!(fuzzy_match("JetBrains Mono", "JETBRAINS"));
        assert!(fuzzy_match("JetBrains Mono", "brains mo"));
        assert!(fuzzy_match("JetBrains Mono", "mono"));
        assert!(!fuzzy_match("JetBrains Mono", "vscode"));
        // 顺序敏感（子串语义）：颠倒顺序不匹配
        assert!(!fuzzy_match("JetBrains Mono", "mono jetbrains"));
        // 空关键词 / 仅空白 = 匹配全部
        assert!(fuzzy_match("Any Font", ""));
        assert!(fuzzy_match("Any Font", "   "));
        // 首尾空白容忍
        assert!(fuzzy_match("Cascadia Code", "  cascadia  "));
    }

    #[test]
    fn measure_monospace_positive_and_scales_with_size() {
        let m12 = measure_monospace(DEFAULT_FONT_FAMILY, 12);
        let m24 = measure_monospace(DEFAULT_FONT_FAMILY, 24);
        // 宽高均为正，且随字号线性增长
        assert!(m12.char_w > 0.0 && m12.char_h > 0.0);
        assert!((m24.char_w - m12.char_w * 2.0).abs() < 0.01);
        assert!((m24.char_h - m12.char_h * 2.0).abs() < 0.01);
        // 行高应不小于字号（含 ascent/descent）
        assert!(m12.char_h >= 12.0);
    }
}
