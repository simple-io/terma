//! 快捷键管理窗口（ShortcutsWindow）相关逻辑：懒创建、草稿录制、保存应用，
//! 以及主窗口 KeyBinding 命中后的动作分发。
//!
//! 作为 `App`（controller）的子模块，直接复用 config / event_tx / keymap / main_window。
//! 编辑采用「草稿 + 保存」模式：录制/恢复默认仅改内存草稿并回填窗口列表，
//! 点击「保存」才落盘、重建生效中的 keymap 并注入主窗口 KeyBinding。

use super::*;
use crate::app::keymap::{KeyMap, display_for_parts, parts_from_key_event};
use crate::config::shortcut::{ShortcutAction, default_shortcuts};

impl App {
    // 打开快捷键管理窗口
    pub(super) fn on_open_shortcuts(&mut self) {
        self.open_shortcuts_window();
    }

    // 关闭快捷键管理窗口（隐藏而非销毁）
    pub(super) fn on_close_shortcuts(&self) {
        if let Some(w) = self.shortcuts_window.as_ref() {
            w.hide().ok();
        }
    }

    // KeyBinding 命中：按动作 id 分发到对应行为。
    pub(super) fn on_shortcut_activated(&mut self, id: &str) {
        let Some(action) = ShortcutAction::from_id(id) else {
            return;
        };
        match action {
            ShortcutAction::NewTab => self.on_create_default_session(),
            ShortcutAction::CloseTab => {
                if let Some(sid) = self.sessions.active_id() {
                    self.on_close_session(sid);
                }
            }
        }
    }

    // 保存：草稿落盘 + 重建生效 keymap + 注入主窗口 KeyBinding。
    pub(super) fn on_save_shortcuts(&mut self) {
        self.recording_action = None;
        let draft = self.shortcut_draft.clone();
        self.config.update_shortcuts(draft);
        self.keymap = KeyMap::from_config(self.config.shortcuts());
        self.apply_shortcuts();
        if let Some(w) = self.shortcuts_window.as_ref() {
            w.set_recording_id("".into());
            w.set_message("已保存".into());
        }
    }

    // 恢复默认：仅重置草稿并回填列表（保存后才生效）。
    pub(super) fn on_reset_shortcuts(&mut self) {
        self.shortcut_draft = default_shortcuts();
        self.recording_action = None;
        self.render_shortcut_draft();
        if let Some(w) = self.shortcuts_window.as_ref() {
            w.set_recording_id("".into());
            w.set_message("已恢复默认（保存后生效）".into());
        }
    }

    // 开始录制某动作的快捷键。
    pub(super) fn on_start_record_shortcut(&mut self, id: &SharedString) {
        let Some(action) = ShortcutAction::from_id(id.as_str()) else {
            return;
        };
        self.recording_action = Some(action);
        if let Some(w) = self.shortcuts_window.as_ref() {
            w.set_recording_id(id.clone());
            w.set_message("请按下新快捷键（需含 ⌘/Ctrl 或 Alt/Option），Esc 取消".into());
        }
    }

    // 取消录制。
    pub(super) fn on_cancel_record_shortcut(&mut self) {
        self.recording_action = None;
        if let Some(w) = self.shortcuts_window.as_ref() {
            w.set_recording_id("".into());
            w.set_message("".into());
        }
    }

    // 录制到一次按键：Esc 取消；否则解析校验，成功即写入草稿并退出录制态，
    // 失败则回填错误提示并保持录制态（等待用户重按）。
    pub(super) fn on_record_shortcut_key(&mut self, text: &SharedString, mods: KeyboardModifiers) {
        // Esc 取消录制
        if text.as_str() == "\u{001b}" {
            self.on_cancel_record_shortcut();
            return;
        }
        let Some(action) = self.recording_action else {
            return;
        };

        match parts_from_key_event(text.as_str(), mods) {
            Ok(parts) => {
                if let Some(cfg) = self
                    .shortcut_draft
                    .iter_mut()
                    .find(|c| c.action == action.id())
                {
                    cfg.keys = parts.clone();
                }
                self.recording_action = None;
                self.render_shortcut_draft();
                if let Some(w) = self.shortcuts_window.as_ref() {
                    w.set_recording_id("".into());
                    w.set_message(
                        format!("已设置：{}（保存后生效）", display_for_parts(&parts)).into(),
                    );
                }
            }
            Err(msg) => {
                if let Some(w) = self.shortcuts_window.as_ref() {
                    w.set_message(msg.into());
                }
            }
        }
    }

    // 打开快捷键管理窗口（懒创建）。每次打开都以当前已保存配置重置草稿，
    // 丢弃上一次未保存的编辑。
    fn open_shortcuts_window(&mut self) {
        self.shortcut_draft = self.config.shortcuts().to_vec();
        self.recording_action = None;

        if let Some(w) = self.shortcuts_window.as_ref() {
            self.render_shortcut_draft();
            w.set_recording_id("".into());
            w.set_message("".into());
            w.show().ok();
            return;
        }

        let window = ShortcutsWindow::new().expect("failed to create ShortcutsWindow");
        window.set_entries(
            Rc::new(VecModel::from(
                KeyMap::from_config(&self.shortcut_draft).ui_entries(),
            ))
            .into(),
        );

        // 关闭按钮 → 隐藏窗口
        let command_tx = self.event_tx.clone();
        window.on_close_requested(move || {
            command_tx.send(WindowEvent::CloseShortcuts).unwrap();
        });

        // 保存
        let command_tx = self.event_tx.clone();
        window.on_save(move || {
            command_tx.send(WindowEvent::SaveShortcuts).unwrap();
        });

        // 恢复默认
        let command_tx = self.event_tx.clone();
        window.on_reset_all(move || {
            command_tx.send(WindowEvent::ResetShortcuts).unwrap();
        });

        // 开始录制
        let command_tx = self.event_tx.clone();
        window.on_start_record(move |id| {
            command_tx
                .send(WindowEvent::StartRecordShortcut(id))
                .unwrap();
        });

        // 取消录制
        let command_tx = self.event_tx.clone();
        window.on_cancel_record(move || {
            command_tx.send(WindowEvent::CancelRecordShortcut).unwrap();
        });

        // 录制按键
        let command_tx = self.event_tx.clone();
        window.on_record_key(move |text, mods| {
            command_tx
                .send(WindowEvent::RecordShortcutKey(text, mods))
                .unwrap();
        });

        // 系统关闭请求 → 隐藏而非销毁
        window
            .window()
            .on_close_requested(move || slint::CloseRequestResponse::HideWindow);

        window.show().expect("failed to show shortcuts window");
        self.shortcuts_window = Some(window);
    }

    // 由当前草稿重建列表并回填窗口（录制/恢复默认后调用）。
    fn render_shortcut_draft(&self) {
        if let Some(w) = self.shortcuts_window.as_ref() {
            let entries = KeyMap::from_config(&self.shortcut_draft).ui_entries();
            w.set_entries(Rc::new(VecModel::from(entries)).into());
        }
    }
}
