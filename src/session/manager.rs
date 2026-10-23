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
//!   - SessionManager 仅在 UI 线程访问
//!   - 每个 TerminalSession 内部有自己的 Output Thread（含 vte 解析），与 manager 解耦
//!   - active_tracker（Arc<ActiveSession>）在切换/新建/关闭时由 manager 更新，
//!     解析线程据此判断是否唤醒 UI 渲染（仅激活会话唤醒）

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::SyncSender;

use crate::app::audio::Bell;
use crate::app::events::WindowEvent;
use crate::config::{ColorScheme, Profile};
use crate::session::active::ActiveSession;
use crate::session::session::TerminalSession;
use crate::session::session_id::{IdGenerator, SessionId};
use anyhow::Result;

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

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
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

    // 创建新会话，并设定会激活状态，成功就返回会话ID，否则返回错误
    pub fn create_session(
        &mut self,
        profile: &Profile,
        rows: u16,
        cols: u16,
        color_scheme: ColorScheme,
        scrollback_limit: usize,
        event_tx: SyncSender<WindowEvent>,
        bell: Bell,
    ) -> Result<SessionId> {
        let id = self.id_gen.next_session_id();
        let title = format!("{} {}", profile.name, self.sessions.len() + 1);
        let session = TerminalSession::new(
            id,
            profile,
            title,
            rows,
            cols,
            color_scheme,
            scrollback_limit,
            event_tx,
            self.active_tracker.clone(),
            bell,
        )?;
        self.sessions.insert(id, session);
        self.order.push(id);
        self.active_tracker.set(Some(id));
        Ok(id)
    }

    // 关闭指定会话，被关闭的会话离开作用域，触发自动关闭底层 PTY，如果关闭激活会话，则切换相邻会话，返回激活会话 id
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

    // 获取会话列表（按 Tab 顺序）
    pub fn sessions_in_order(&self) -> Vec<&TerminalSession> {
        self.order
            .iter()
            .filter_map(|id| self.sessions.get(id))
            .collect()
    }

    // 返回激活会话只读引用。
    pub fn active_session(&self) -> Option<&TerminalSession> {
        self.active_tracker
            .get()
            .and_then(|id| self.sessions.get(&id))
    }

    // 返回激活会话可变引用。
    pub fn active_session_mut(&mut self) -> Option<&mut TerminalSession> {
        self.active_tracker
            .get()
            .and_then(|id| self.sessions.get_mut(&id))
    }
    
    // 返回激活会话 ID（只读，供渲染）。
    pub fn active_id(&self) -> Option<SessionId> {
        self.active_tracker.get()
    }

    // 切换激活会话
    pub fn set_active_session(&mut self, id: SessionId) -> bool {
        if self.sessions.contains_key(&id) {
            self.active_tracker.set(Some(id));
            true
        } else {
            false
        }
    }

    // 会话是否存在（供关闭流程幂等守卫，避免重复关闭已移除的会话）
    pub fn contains(&self, id: SessionId) -> bool {
        self.sessions.contains_key(&id)
    }

    // 会话列表是否为空
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    // 会话数量
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    // 向激活会话的 PTY 写入用户输入。
    pub fn write_to_active(&mut self, data: &[u8]) -> std::io::Result<()> {
        if let Some(id) = self.active_tracker.get() {
            if let Some(s) = self.sessions.get_mut(&id) {
                return s.write(data);
            }
        }
        Ok(())
    }

    // 调整所有会话的 PTY 尺寸 + ScreenBuffer。
    pub fn resize_all(&mut self, rows: u16, cols: u16) {
        for s in self.sessions.values_mut() {
            s.resize(rows, cols);
        }
    }

    // 把新配色方案应用到所有会话（配色切换时调用）。
    pub fn apply_color_scheme(&mut self, scheme: ColorScheme) {
        for s in self.sessions.values_mut() {
            s.set_color_scheme(scheme.clone());
        }
    }

    // 把新的会话内容缓存数应用到所有已存在的会话（设置保存时调用），
    // 新建会话由 `create_session` 按配置传入，两者共同保证配置即时生效。
    pub fn apply_scrollback_limit(&mut self, limit: usize) {
        for s in self.sessions.values_mut() {
            s.set_scrollback_limit(limit);
        }
    }
}
