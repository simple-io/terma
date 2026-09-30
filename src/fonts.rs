//! fonts — 内置字体元信息与系统字体枚举
//!
//! 内置字体的数据内嵌由 Slint 编译器完成，在 src/ui/main_window.slint 中的
//! import 语句让 Slint 以 FileData 形式把字体静态编译进二进制，并在窗口
//! 初始化时注册到字体系统。本模块仅提供：
//!   - 默认字体 family 常量（JetBrains Mono）
//!   - 内置字体 family 列表（手动维护，在 main_window.slint 中导入）
//!   - 操作系统已安装字体 family 列表（font-kit，进程内缓存）

use std::sync::OnceLock;
use font_kit::source::SystemSource;

/// 内置字体，在 main_window.slint 中导入
static BUNDLED_FONT_FAMILIES: &[&str] = &[
    "JetBrains Mono",
    "Cascadia Code",
];

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_families_default_first() {
        let families = bundled_families();
        assert_eq!(families.first().map(String::as_str), Some(DEFAULT_FONT_FAMILY));
        assert!(families.len() >= 2, "应至少包含 JetBrains Mono 与 Cascadia Code");
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
        assert_eq!(bundled.first().map(String::as_str), Some(DEFAULT_FONT_FAMILY));
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
}
