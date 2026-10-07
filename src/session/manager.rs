//! manager — SessionManager
//!
//! 职责：
//!   - 管理所有 TerminalSession 的生命周期
//!   - 维护当前激活的 Session（含共享的 ActiveSession 追踪器，供解析线程查询）
//!   - 提供 create / close / switch / resize / write / 状态访问 接口
//!
//! 设计要点：
//!   - 使用 HashMap<SessionId, TerminalSession> 保证 ID 稳定且 O(1) 查找
//!   - 不复用已关闭的 ID（IdGenerator 单调递增）
//!   - 切换 Tab 仅切换 active_session_id，不销毁/重建底层 Session
//!   - 关闭 Tab 时 remove session → Drop 自动 shutdown PTY/child/thread
//!
//! 线程模型：
//!   - SessionManager 仅在 UI 线程访问（被 App 的 RefCell 包裹）
//!   - 每个 TerminalSession 内部有自己的 Output Thread（含 vte 解析），与 manager 解耦
//!   - active_tracker（Arc<ActiveSession>）在切换/新建/关闭时由 manager 更新，
//!     解析线程据此判断是否唤醒 UI 渲染（仅激活会话唤醒）

use std::collections::HashMap;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use crate::app::events::WindowEvent;
use crate::config::Profile;
use crate::session::active::ActiveSession;
use crate::session::session::{TerminalSession, TerminalSessionLike};
use crate::session::session_id::{IdGenerator, ProfileId, SessionId};
use crate::terminal::TerminalState;

pub struct SessionManager {
    // 会话映射表
    sessions: HashMap<SessionId, TerminalSession>,
    // 插入顺序，用于 UI 按创建顺序展示 Tab
    order: Vec<SessionId>,
    // 共享的激活会话追踪器（跨线程），切换/新建/关闭时同步更新
    active_tracker: Arc<ActiveSession>,
    // id 生成器，确保不重复
    id_gen: IdGenerator,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            order: Vec::new(),
            active_tracker: Arc::new(ActiveSession::new()),
            id_gen: IdGenerator::default(),
        }
    }

    /// 用指定 Profile 创建一个真实终端会话并设为激活。
    /// 返回新 Session 的 ID；PTY 创建失败时返回 Err。
    ///
    /// `rows` / `cols` 为初始终端网格，调用方需根据当前主窗口实际尺寸计算，
    /// 使新会话与既有会话、渲染层保持尺寸一致。
    /// `wake_tx` 是渲染唤醒通道，透传给 Output Thread 用于事件触发式渲染。
    pub fn create_session(&mut self, profile: &Profile, title: String, rows: u16, cols: u16, event_tx: SyncSender<WindowEvent>) -> Result<SessionId> {
        let id = self.id_gen.next_session_id();
        let session = TerminalSession::new(
            id,
            profile,
            title,
            rows,
            cols,
            event_tx,
            self.active_tracker.clone(),
        )?;
        self.sessions.insert(id, session);
        self.order.push(id);
        self.active_tracker.set(Some(id));
        Ok(id)
    }

    /// 关闭指定会话。remove 触发 TerminalSession::Drop → shutdown PTY/child/thread。
    /// 如果关闭的是激活会话，则自动切到相邻会话。
    pub fn close_session(&mut self, id: SessionId) -> Option<SessionId> {
        if self.sessions.remove(&id).is_none() {
            return self.active_tracker.get();
        }
        self.order.retain(|s| *s != id);

        if self.active_tracker.get() == Some(id) {
            self.active_tracker.set(self.order.first().copied());
        }
        self.active_tracker.get()
    }

    /// 返回激活会话引用（只读，供渲染）。
    pub fn active_session(&self) -> Option<&TerminalSession> {
        self.active_tracker.get().and_then(|id| self.sessions.get(&id))
    }

    /// 返回激活会话 ID（只读，供渲染）。
    pub fn active_id(&self) -> Option<SessionId> {
        self.active_tracker.get()
    }

    /// 切换激活会话。不重建任何底层资源。同步更新共享追踪器，
    /// 使新激活会话的解析线程开始唤醒 UI，原会话解析线程停止唤醒。
    pub fn set_active_session(&mut self, id: SessionId) -> bool {
        if self.sessions.contains_key(&id) {
            self.active_tracker.set(Some(id));
            true
        } else {
            false
        }
    }

    /// 按 Tab 顺序返回所有会话引用（用于 UI 渲染）。
    pub fn sessions_in_order(&self) -> Vec<&TerminalSession> {
        self.order
            .iter()
            .filter_map(|id| self.sessions.get(id))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// 向激活会话的 PTY 写入用户输入。
    pub fn write_to_active(&mut self, data: &[u8]) -> std::io::Result<()> {
        if let Some(id) = self.active_tracker.get() {
            if let Some(s) = self.sessions.get_mut(&id) {
                return s.write(data);
            }
        }
        Ok(())
    }

    /// 激活会话的终端状态共享句柄（只读，供渲染）。
    pub fn active_state(&self) -> Option<Arc<Mutex<TerminalState>>> {
        self.active_tracker
            .get()
            .and_then(|id| self.sessions.get(&id))
            .map(|s| s.state().clone())
    }

    /// 调整所有会话的 PTY 尺寸 + ScreenBuffer。
    ///
    /// 窗口只有一个 TerminalView，所有 Tab 共享同一网格尺寸，因此窗口尺寸变化时
    /// 必须同步全部会话，避免切换 Tab 时出现尺寸不同步 / 渲染错位。
    pub fn resize_all(&mut self, rows: u16, cols: u16) {
        for s in self.sessions.values_mut() {
            s.resize(rows, cols);
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}
