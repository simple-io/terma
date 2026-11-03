//! keymap — 快捷键运行时映射（桥接「配置数据」与「Slint Keys / KeyBinding」）
//!
//! 职责：
//!   - 把持久化的 [`ShortcutConfig`]（parts 形式）解析为 Slint 的 [`Keys`]，供主窗口
//!     的 `KeyBinding`（for 循环动态生成）使用；
//!   - 在 Rust 侧镜像 Slint 私有的 `Keys::matches` 逻辑（[`MatchData`]），让终端输入
//!     回调能判断「这次按键是否命中某个已配置快捷键」——命中则拒绝（让事件冒泡到
//!     `KeyBinding` 触发 `activated`），未命中则照常写入 PTY；
//!   - 为快捷键管理窗口提供展示数据（动作名 + 平台原生按键串）；
//!   - 录制新按键时，把 `(text, modifiers)` 事件还原为 parts（[`parts_from_key_event`]）。
//!
//! 跨平台：按键统一以 Slint parts 存储（如 `["Control","t"]`）。Slint 在 macOS 上把物理
//! ⌘(Command) 重映射为逻辑 `control`、在 Windows/Linux 上 `control` 即 Ctrl；`Keys` 的
//! Display 亦按平台输出原生符号（⌘T / Ctrl+T）。因此同一份配置自动适配各系统。

use crate::app::controller::{ShortcutBinding, ShortcutEntry};
use crate::config::shortcut::{ShortcutAction, ShortcutConfig};
use slint::language::KeyboardModifiers;
use slint::{Keys, SharedString};

/// 从 [`Keys::to_parts`] 派生的匹配数据。
///
/// Slint 的 `Keys::matches` 是 `pub(crate)`，无法直接调用；这里以 `to_parts()`（公开、
/// 且与 Slint 解析器同源）为唯一真相重建等价匹配逻辑，避免手工维护修饰符/大小写规则。
#[derive(Debug, Clone, PartialEq, Eq)]
struct MatchData {
    /// 已小写化的主键（与 `Keys` 内部存储一致）。
    key: String,
    /// 需要按下的修饰符。
    mods: KeyboardModifiers,
    /// 匹配时忽略 Shift 状态（布局相关的可 Shift 键）。
    ignore_shift: bool,
    /// 匹配时忽略 Alt 状态。
    ignore_alt: bool,
}

impl MatchData {
    /// 由 `Keys` 派生；空 `Keys`（无主键）返回 `None`，表示永不匹配。
    fn from_keys(keys: &Keys) -> Option<Self> {
        let mut mods = KeyboardModifiers::default();
        let mut ignore_shift = false;
        let mut ignore_alt = false;
        let mut key: Option<String> = None;
        for part in keys.to_parts() {
            match part {
                "Control" => mods.control = true,
                "Alt" => mods.alt = true,
                "Shift" => mods.shift = true,
                "Meta" => mods.meta = true,
                "Shift?" => ignore_shift = true,
                "Alt?" => ignore_alt = true,
                other => key = Some(other.to_lowercase()),
            }
        }
        Some(Self {
            key: key?,
            mods,
            ignore_shift,
            ignore_alt,
        })
    }

    /// 镜像 `slint::Keys::matches`：对事件文本做小写化后逐字符比较，并按 ignore_*
    /// 规则放宽对应修饰符，其余修饰符需完全一致。
    fn matches(&self, text: &str, mods: KeyboardModifiers) -> bool {
        let mut expected = self.mods;
        if self.ignore_shift {
            expected.shift = mods.shift;
        }
        if self.ignore_alt {
            expected.alt = mods.alt;
        }
        if mods != expected {
            return false;
        }
        let event_key: String = text.chars().flat_map(|c| c.to_lowercase()).collect();
        event_key == self.key
    }
}

/// 单条绑定的运行时表示。
struct Binding {
    action: ShortcutAction,
    /// 供主窗口 `KeyBinding.keys` 使用。
    keys: Keys,
    /// Rust 侧门禁匹配用（空绑定为 `None`）。
    match_data: Option<MatchData>,
}

/// 快捷键运行时映射表：按 [`ShortcutAction::all`] 注册顺序持有全部动作的绑定。
pub struct KeyMap {
    bindings: Vec<Binding>,
}

impl KeyMap {
    /// 从配置构建（配置层已 normalize，此处对缺失动作再兜底为平台默认）。
    pub fn from_config(configs: &[ShortcutConfig]) -> Self {
        let bindings = ShortcutAction::all()
            .iter()
            .map(|action| {
                let parts = configs
                    .iter()
                    .find(|c| c.action == action.id())
                    .map(|c| c.keys.clone())
                    .unwrap_or_else(|| action.default_parts());
                Self::build(*action, parts)
            })
            .collect();
        Self { bindings }
    }

    fn build(action: ShortcutAction, parts: Vec<String>) -> Binding {
        let keys = Keys::from_parts(parts.iter().map(|s| s.as_str())).unwrap_or_default();
        let match_data = MatchData::from_keys(&keys);
        Binding {
            action,
            keys,
            match_data,
        }
    }

    /// 事件是否命中某个已配置动作。命中返回该动作。
    /// 门禁据此让按键冒泡到 `KeyBinding`（而非写入 PTY）。
    pub fn matches(&self, text: &str, mods: KeyboardModifiers) -> Option<ShortcutAction> {
        self.bindings
            .iter()
            .find(|b| {
                b.match_data
                    .as_ref()
                    .is_some_and(|m| m.matches(text, mods))
            })
            .map(|b| b.action)
    }

    /// 主窗口 `KeyBinding` 模型（id + keys）。
    pub fn ui_bindings(&self) -> Vec<ShortcutBinding> {
        self.bindings
            .iter()
            .map(|b| ShortcutBinding {
                id: SharedString::from(b.action.id()),
                keys: b.keys.clone(),
            })
            .collect()
    }

    /// 管理窗口列表（id + 展示名 + 平台原生按键串）。
    pub fn ui_entries(&self) -> Vec<ShortcutEntry> {
        self.bindings
            .iter()
            .map(|b| ShortcutEntry {
                id: SharedString::from(b.action.id()),
                name: SharedString::from(b.action.display_name()),
                keys_text: SharedString::from(b.keys.to_string()),
            })
            .collect()
    }

    /// 导出为配置（按注册顺序，keys 取 `Keys` 规范化后的 parts）。
    pub fn to_config(&self) -> Vec<ShortcutConfig> {
        self.bindings
            .iter()
            .map(|b| {
                ShortcutConfig::new(
                    b.action.id(),
                    b.keys.to_parts().map(|s| s.to_string()).collect(),
                )
            })
            .collect()
    }
}

/// 由动作 parts 直接生成展示串（管理窗口草稿渲染用，无需完整 KeyMap）。
pub fn display_for_parts(parts: &[String]) -> String {
    match Keys::from_parts(parts.iter().map(|s| s.as_str())) {
        Ok(keys) => keys.to_string(),
        Err(_) => parts.join(" + "),
    }
}

/// 把一次按键事件 `(text, modifiers)` 还原为可持久化的 parts（录制新快捷键用）。
///
/// 约束与失败情形：
///   - 必须包含 `Control`/`Alt`/`Meta` 之一：终端输入框仅在带这些修饰符（或私有区特殊键）
///     时才让被拒绝的按键冒泡到 `KeyBinding`，纯字符快捷键会被输入框吞掉而无法生效；
///   - 主键支持字母/数字/符号（单字面量）与常见特殊键（方向键、F1–F12、Home/End 等）；
///   - 其余无法识别的输入返回中文错误提示。
///
/// 成功时返回 `Keys::to_parts` 的规范化结果，保证与 `from_parts` 精确往返。
pub fn parts_from_key_event(text: &str, mods: KeyboardModifiers) -> Result<Vec<String>, String> {
    if !(mods.control || mods.alt || mods.meta) {
        return Err("快捷键需包含 ⌘/Ctrl 或 Alt/Option 修饰键".into());
    }

    let key_part = key_part_from_text(text)?;

    // 修饰符顺序与 Slint `@keys` / to_parts 一致：Meta, Control, Alt, Shift。
    let mut parts: Vec<String> = Vec::new();
    if mods.meta {
        parts.push("Meta".into());
    }
    if mods.control {
        parts.push("Control".into());
    }
    if mods.alt {
        parts.push("Alt".into());
    }
    if mods.shift {
        parts.push("Shift".into());
    }
    parts.push(key_part);

    // 交由 Slint 解析器校验（如 Shift 与可 Shift 键冲突等），并取其规范化 parts。
    let keys = Keys::from_parts(parts.iter().map(|s| s.as_str()))
        .map_err(|e| format!("无法绑定该按键：{e}"))?;
    Ok(keys.to_parts().map(|s| s.to_string()).collect())
}

/// 事件文本 → parts 主键（特殊键用 Slint 名称，可打印字符用小写字面量）。
fn key_part_from_text(text: &str) -> Result<String, String> {    let mut chars = text.chars();
    let c = chars.next().ok_or_else(|| "无法识别的按键".to_string())?;

    let name = match c {
        '\u{0008}' => "Backspace",
        '\u{0009}' => "Tab",
        '\u{000a}' => "Return",
        '\u{001b}' => "Escape",
        '\u{007f}' => "Delete",
        '\u{0020}' => "Space",
        '\u{F700}' => "UpArrow",
        '\u{F701}' => "DownArrow",
        '\u{F702}' => "LeftArrow",
        '\u{F703}' => "RightArrow",
        '\u{F704}' => "F1",
        '\u{F705}' => "F2",
        '\u{F706}' => "F3",
        '\u{F707}' => "F4",
        '\u{F708}' => "F5",
        '\u{F709}' => "F6",
        '\u{F70A}' => "F7",
        '\u{F70B}' => "F8",
        '\u{F70C}' => "F9",
        '\u{F70D}' => "F10",
        '\u{F70E}' => "F11",
        '\u{F70F}' => "F12",
        '\u{F727}' => "Insert",
        '\u{F729}' => "Home",
        '\u{F72B}' => "End",
        '\u{F72C}' => "PageUp",
        '\u{F72D}' => "PageDown",
        _ => {
            // 多字符（如输入法提交）不支持
            if chars.next().is_some() {
                return Err("无法识别的按键".into());
            }
            // 其余控制字符 / 未覆盖的私有区特殊键不支持
            if c.is_control() || ('\u{f700}'..='\u{f7ff}').contains(&c) {
                return Err("暂不支持该按键".into());
            }
            // 可打印单字符 → 小写字面量
            return Ok(c.to_lowercase().collect());
        }
    };
    Ok(name.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(control: bool, shift: bool, alt: bool, meta: bool) -> KeyboardModifiers {
        let mut m = KeyboardModifiers::default();
        m.control = control;
        m.shift = shift;
        m.alt = alt;
        m.meta = meta;
        m
    }

    #[test]
    fn defaults_match_control_t_and_w() {
        let km = KeyMap::from_config(&crate::config::shortcut::default_shortcuts());
        // ⌘T / Ctrl+T → 新建会话；⌘W / Ctrl+W → 关闭会话
        assert_eq!(km.matches("t", mods(true, false, false, false)), Some(ShortcutAction::NewTab));
        // 大写文本（CapsLock）仍命中：matches 会对事件文本做小写化
        assert_eq!(km.matches("T", mods(true, false, false, false)), Some(ShortcutAction::NewTab));
        // 额外按住 Shift 是不同组合，不应命中 ⌘T
        assert_eq!(km.matches("t", mods(true, true, false, false)), None);
        assert_eq!(km.matches("w", mods(true, false, false, false)), Some(ShortcutAction::CloseTab));
    }

    #[test]
    fn non_shortcut_keys_do_not_match() {
        let km = KeyMap::from_config(&crate::config::shortcut::default_shortcuts());
        // Ctrl+C 是终端控制字符，不应被当作应用快捷键（否则会吞掉终端输入）
        assert_eq!(km.matches("c", mods(true, false, false, false)), None);
        // 无修饰符的普通字符不匹配
        assert_eq!(km.matches("t", mods(false, false, false, false)), None);
    }

    #[test]
    fn config_round_trips_through_keymap() {
        let cfg = crate::config::shortcut::default_shortcuts();
        let km = KeyMap::from_config(&cfg);
        let exported = km.to_config();
        // 规范化后应与原配置等价
        let km2 = KeyMap::from_config(&exported);
        assert_eq!(
            km2.matches("t", mods(true, false, false, false)),
            Some(ShortcutAction::NewTab)
        );
    }

    #[test]
    fn recording_requires_modifier() {
        // 无修饰符 → 拒绝（纯字符会被终端输入框吞掉，无法冒泡到 KeyBinding）
        assert!(parts_from_key_event("t", mods(false, false, false, false)).is_err());
        // 带 Control 的字母 → 成功，规范化为 ["Control","t"]
        let parts = parts_from_key_event("t", mods(true, false, false, false)).unwrap();
        assert_eq!(parts, vec!["Control".to_string(), "t".to_string()]);
    }

    #[test]
    fn recording_maps_special_keys_to_names() {
        // F5（私有区 \u{F708}）：解析成功，规范化 parts 存原始键字符，
        // 但展示串按平台还原为名称（含 "F5"）。
        let parts = parts_from_key_event("\u{F708}", mods(true, false, false, false)).unwrap();
        assert!(Keys::from_parts(parts.iter().map(|s| s.as_str())).is_ok());
        assert!(display_for_parts(&parts).contains("F5"));
        // 方向键 Up（\u{F700}）：展示串含 "UpArrow"
        let parts = parts_from_key_event("\u{F700}", mods(false, false, true, false)).unwrap();
        assert!(display_for_parts(&parts).contains("UpArrow"));
    }

    #[test]
    fn recorded_special_key_matches_event() {
        // 录制 F5 后，同样的事件应被 KeyMap 命中
        let parts = parts_from_key_event("\u{F708}", mods(true, false, false, false)).unwrap();
        let cfg = vec![ShortcutConfig::new(ShortcutAction::NewTab.id(), parts)];
        let km = KeyMap::from_config(&cfg);
        assert_eq!(
            km.matches("\u{F708}", mods(true, false, false, false)),
            Some(ShortcutAction::NewTab)
        );
    }
}
