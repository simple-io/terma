//! shortcut — 快捷键数据定义（纯数据，不依赖 Slint）
//!
//! 设计要点：
//!   - [`ShortcutAction`] 是「可绑定动作」的注册表：每个动作有稳定的字符串 id、
//!     展示名与平台无关的默认按键。新增动作 = 在此枚举增加一个变体并在
//!     [`ShortcutAction::all`] 中登记，其余（配置读写、UI 列表、KeyBinding 生成）
//!     全部按注册表自动展开，无需散落的 if/match。
//!   - 默认按键统一用 Slint 的 `Control` 修饰符表达：Slint 在 macOS 上把物理
//!     ⌘(Command) 重映射为 `control`、在 Windows/Linux 上 `control` 即 Ctrl，
//!     因此同一份 `["Control", "t"]` 在 macOS 表现为 ⌘T、在其他平台表现为 Ctrl+T，
//!     跨平台兼容由 Slint 的修饰符重映射 + `Keys` 的原生展示自动完成。
//!   - 按键以「Slint parts」形式（如 `["Control", "t"]`，主键小写）持久化到 config.toml，
//!     与 `Keys::from_parts` / `Keys::to_parts` 直接对接。

use serde::{Deserialize, Serialize};

/// 一个可绑定快捷键的应用动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShortcutAction {
    /// 用默认启动项新建会话。
    NewTab,
    /// 关闭当前激活会话。
    CloseTab,
}

impl ShortcutAction {
    /// 全部已注册动作（顺序即 UI 列表与默认配置顺序）。
    pub fn all() -> [ShortcutAction; 2] {
        [ShortcutAction::NewTab, ShortcutAction::CloseTab]
    }

    /// 稳定字符串 id（持久化到配置、跨 Rust/Slint 传递）。
    pub fn id(&self) -> &'static str {
        match self {
            ShortcutAction::NewTab => "new_tab",
            ShortcutAction::CloseTab => "close_tab",
        }
    }

    /// 由 id 反查动作。
    pub fn from_id(id: &str) -> Option<Self> {
        Self::all().into_iter().find(|a| a.id() == id)
    }

    /// 展示名（快捷键管理窗口列表用）。
    pub fn display_name(&self) -> &'static str {
        match self {
            ShortcutAction::NewTab => "新建会话",
            ShortcutAction::CloseTab => "关闭当前会话",
        }
    }

    /// 平台无关的默认按键（Slint parts 语法）。
    /// `Control` 在 macOS 对应 ⌘、在 Windows/Linux 对应 Ctrl。
    /// 注意：主键必须小写——Slint `Keys::from_parts` 对字面量键要求小写
    /// （大写会返回 NotLowercase 错误），展示时由 Slint 自动转为原生大写符号。
    pub fn default_parts(&self) -> Vec<String> {
        match self {
            ShortcutAction::NewTab => vec!["Control".into(), "t".into()],
            ShortcutAction::CloseTab => vec!["Control".into(), "w".into()],
        }
    }
}

/// 单条快捷键配置：动作 id + Slint parts 形式的按键。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShortcutConfig {
    /// 动作 id（对应 [`ShortcutAction::id`]）。
    pub action: String,
    /// 按键 parts（如 `["Control", "T"]`）。空数组表示「未绑定」。
    pub keys: Vec<String>,
}

impl ShortcutConfig {
    pub fn new(action: impl Into<String>, keys: Vec<String>) -> Self {
        Self {
            action: action.into(),
            keys,
        }
    }
}

/// 生成全部动作的默认快捷键配置。
pub fn default_shortcuts() -> Vec<ShortcutConfig> {
    ShortcutAction::all()
        .iter()
        .map(|a| ShortcutConfig::new(a.id(), a.default_parts()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        for action in ShortcutAction::all() {
            assert_eq!(ShortcutAction::from_id(action.id()), Some(action));
        }
        assert_eq!(ShortcutAction::from_id("nope"), None);
    }

    #[test]
    fn defaults_cover_all_actions() {
        let defaults = default_shortcuts();
        assert_eq!(defaults.len(), ShortcutAction::all().len());
        for action in ShortcutAction::all() {
            assert!(defaults.iter().any(|d| d.action == action.id()));
        }
    }
}
