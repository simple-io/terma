//! session_id — 会话与 Profile 的稳定 ID 类型
//!
//! SessionId 使用 u64 单调递增，保证即便关闭某个 Session 后，
//! 已分配的 ID 也不会被复用，避免 Tab 切换时指向错误的 Session。

use std::fmt;
use serde::{Deserialize, Serialize};

/// 终端会话 ID（稳定、不可复用）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u64);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl SessionId {
    pub fn to_slint_string(self) -> String {
        self.0.to_string()
    }
}

/// Profile ID（用于在 ProfileManager 中唯一标识一个终端配置）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProfileId(pub String);

impl ProfileId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 单调递增 ID 生成器
#[derive(Debug, Default)]
pub struct IdGenerator {
    next: u64,
}

impl IdGenerator {
    pub fn next_session_id(&mut self) -> SessionId {
        self.next += 1;
        SessionId(self.next)
    }
}
