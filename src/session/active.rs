//! active — 共享的「当前激活会话」追踪器
//!
//! 用于跨线程广播当前激活会话：SessionManager（UI 线程）在切换 / 新建 / 关闭
//! Tab 时更新；每个会话的 Output Thread（解析线程）在解析到新输出后据此判断
//! 是否需要唤醒 UI 线程做渲染。
//!
//! 线程模型：
//!   - 用 AtomicU64 存储激活会话 id，无锁读，任何线程可安全访问。
//!   - 只有激活会话产生输出时才发送唤醒信号，非激活会话静默解析、不唤醒 UI，
//!     从而避免非活跃会话的输出浪费 UI 线程资源。
//!   - id 由 IdGenerator 单调递增且不复用，SessionId(0) 保留为「无激活会话」。

use std::sync::atomic::{AtomicU64, Ordering};

use crate::session::session_id::SessionId;

/// 共享激活会话追踪器，跨线程 `Arc<ActiveSession>` 共享。
#[derive(Debug, Default)]
pub struct ActiveSession {
    active: AtomicU64,
}

impl ActiveSession {
    pub fn new() -> Self {
        Self {
            active: AtomicU64::new(0),
        }
    }

    /// UI 线程更新当前激活会话（None 表示无激活会话）。
    pub fn set(&self, id: Option<SessionId>) {
        self.active.store(id.map(|i| i.0).unwrap_or(0), Ordering::Relaxed);
    }

    pub fn get(&self) -> Option<SessionId> {
        let id = self.active.load(Ordering::Relaxed);
        if id == 0 {
            None
        } else {
            Some(SessionId(id))
        }
    }
    
    /// 解析线程查询：给定会话当前是否为激活会话。
    pub fn is_active(&self, id: SessionId) -> bool {
        id.0 != 0 && self.active.load(Ordering::Relaxed) == id.0
    }
}
