//! 设置窗口（SettingsWindow）相关逻辑：懒创建、回调绑定、
//! 通用配置 / Profile 的 CRUD 与字体模糊搜索。
//!
//! 作为 `App`（controller）的子模块，直接复用主控制器的
//! config / event_tx / main_window / sessions 等状态；
//! 保存设置后回写主窗口（字体、背景色、Profile 下拉、强制重绘）。

use super::*;
use crate::config::profile::{format_env_vars_text, parse_env_vars_text};
use crate::config::{GeneralConfig, Profile};

impl App {
    // 打开设置窗口
    pub(super) fn on_open_settings(&mut self) {
        self.open_settings_window();
    }

    // 关闭设置窗口
    pub(super) fn on_close_settings(&self) {
        if let Some(w) = self.settings_window.as_ref() {
            w.hide().ok();
        }
    }

    // 保存通用配置并实时生效
    pub(super) fn on_save_settings(&self, info: GeneralSettingsInfo) {
        let config = GeneralConfig {
            default_profile: info.default_profile_id.to_string(),
            font_family: info.font_family.to_string(),
            font_size: info.font_size as u32,
            background_color: info.background_color.to_string(),
            cursor_style: info.cursor_style.to_string(),
            color_scheme: info.color_scheme.to_string(),
            bell_sound: info.bell_sound,
            scrollback_limit: info.scrollback_limit as usize,
        };
        self.config.borrow_mut().update_general(config);

        // 即时生效：字体 + 背景色
        self.apply_font();
        let bg = self.config.borrow().general().background_color.clone();
        if let Some(color) = parse_color(&bg) {
            self.main_window.set_bg_color(color);
        }

        // 刷新主窗口 Profile 下拉菜单（默认 Profile 可能变化）
        self.apply_profiles();
        // 强制全量重绘
        self.force_full_render();
    }

    // 保存 Profile 修改
    pub(super) fn on_save_profile(&self, info: ProfileEditInfo) {
        let profile = Profile {
            id: ProfileId::new(info.id.to_string()),
            name: info.name.to_string(),
            command: info.command.to_string(),
            args: info
                .args
                .split_whitespace()
                .map(|s| s.to_string())
                .collect(),
            working_directory: if info.working_directory.is_empty() {
                None
            } else {
                Some(std::path::PathBuf::from(info.working_directory.to_string()))
            },
            env_vars: parse_env_vars_text(&info.env_vars),
            icon: None,
            color_scheme: None,
        };
        self.config.borrow_mut().update_profile(profile);
    }

    // 添加新 Profile
    pub(super) fn on_add_profile(&mut self) {
        // 新建 profile
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let id = format!("profile-{}", ts);
        let profile = Profile::new(id, "New Profile", "/bin/sh");
        let idx = self.config.borrow_mut().add_profile(profile);

        // 选中新添加的 Profile
        if let Some(w) = self.settings_window.as_mut() {
            w.set_selected_index(idx as i32);
        }
    }

    // 删除 Profile
    pub(super) fn on_delete_profile(&self, id: SharedString) {
        let pid = ProfileId::new(id.to_string());
        self.config.borrow_mut().delete_profile(&pid);
        // self.refresh_settings_ui();
        // self.populate_profiles();
    }

    // 按关键词模糊过滤字体候选列表并回填设置窗口（部分匹配、不区分大小写）。
    // 空关键词（或仅空白）= 恢复完整列表，同时回填当前字体索引（下拉打开
    // 时键盘高亮定位用）。current_family 为设置窗口当前草稿的字体（可能
    // 尚未保存），基于过滤后列表反查——保证高亮定位与展示列表一致。
    pub(super) fn on_filter_fonts(&mut self, keyword: &str, current: &str) {
        if let Some(ref w) = self.settings_window.as_mut() {
            let filtered: Vec<FontEntry> = self
                .font_entries
                .borrow()
                .iter()
                .filter(|e| fonts::fuzzy_match(&e.family, keyword))
                .cloned()
                .collect();
            if keyword.trim().is_empty() {
                w.set_font_current_index(Self::family_index(&filtered, current));
            }
            w.set_filtered_font_list(Rc::new(VecModel::from(filtered)).into());
        }
    }

    // 打开设置窗口（懒创建：首次打开时创建，后续仅激活）
    fn open_settings_window(&mut self) {
        if let Some(w) = self.settings_window.as_ref() {
            // 窗口已存在，重新显示并激活
            w.show().ok();
            return;
        }

        let window = SettingsWindow::new().expect("failed to create SettingsWindow");
        self.apply_settings(&window);

        // 取消设置（关闭设置窗口）
        let command_tx = self.event_tx.clone();
        window.on_cancelled(move || {
            command_tx.send(WindowEvent::CloseSettings).unwrap();
        });

        // 保存通用配置
        let command_tx = self.event_tx.clone();
        window.on_save_general(move |info| {
            command_tx.send(WindowEvent::SaveSettings(info)).unwrap();
        });

        let command_tx = self.event_tx.clone();
        window.on_save_profile(move |info| {
            command_tx.send(WindowEvent::SaveProfile(info)).unwrap();
        });

        let command_tx = self.event_tx.clone();
        window.on_add_profile(move || {
            command_tx.send(WindowEvent::AddProfile()).unwrap();
        });

        let command_tx = self.event_tx.clone();
        window.on_delete_profile(move |id| {
            command_tx.send(WindowEvent::DelProfile(id)).unwrap();
        });

        // 字体搜索：关键词变化 → 模糊过滤候选列表回填
        let command_tx = self.event_tx.clone();
        window.on_font_search_edited(move |keyword, current| {
            command_tx
                .send(WindowEvent::FilterFonts(keyword, current))
                .unwrap();
        });

        // 窗口关闭请求 → 隐藏而非销毁
        window
            .window()
            .on_close_requested(move || slint::CloseRequestResponse::HideWindow);

        window.show().expect("failed to show settings window");
        self.settings_window = Some(window);
    }

    // 把当前配置同步到 SettingsWindow UI
    fn apply_settings(&self, window: &SettingsWindow) {
        let mgr = self.config.borrow();

        // 通用配置
        let g = mgr.general();
        window.set_general_settings(GeneralSettingsInfo {
            font_family: g.font_family.clone().into(),
            font_size: g.font_size as i32,
            background_color: g.background_color.clone().into(),
            cursor_style: g.cursor_style.clone().into(),
            color_scheme: g.color_scheme.clone().into(),
            bell_sound: g.bell_sound,
            scrollback_limit: g.scrollback_limit as i32,
            default_profile_id: mgr.default_profile_id().0.clone().into(),
        });

        // Profile 列表
        let profiles: Vec<ProfileEditInfo> = mgr
            .profiles()
            .iter()
            .map(|p| ProfileEditInfo {
                id: p.id.0.as_str().into(),
                name: p.name.as_str().into(),
                command: p.command.as_str().into(),
                args: p.args.join(" ").into(),
                working_directory: p
                    .working_directory
                    .as_ref()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default()
                    .into(),
                env_vars: format_env_vars_text(&p.env_vars).into(),
            })
            .collect();
        window.set_profile_list(Rc::new(VecModel::from(profiles)).into());
        window.set_selected_index(0);

        // 字体下拉框候选列表：内置字体置顶，其后系统字体（同名剔除在
        // candidate_families 内完成）。完整列表缓存进 font_entries（搜索
        // 过滤复用），filtered 初始为完整列表，并回填当前字体索引（下拉
        // 打开时键盘高亮定位用）。
        let (bundled, system) = fonts::candidate_families();
        let font_entries: Vec<FontEntry> = bundled
            .iter()
            .map(|f| FontEntry {
                family: f.as_str().into(),
                is_bundled: true,
            })
            .chain(system.iter().map(|f| FontEntry {
                family: f.as_str().into(),
                is_bundled: false,
            }))
            .collect();
        *self.font_entries.borrow_mut() = font_entries.clone();
        window.set_font_list(Rc::new(VecModel::from(font_entries.clone())).into());
        window.set_filtered_font_list(Rc::new(VecModel::from(font_entries.clone())).into());
        window.set_font_current_index(Self::family_index(&font_entries, &g.font_family));

        // 默认启动项下拉框：由 default-profile-id 反查列表索引注入
        let default_id = mgr.default_profile_id().0.clone();
        let idx = mgr
            .profiles()
            .iter()
            .position(|p| p.id.0 == default_id)
            .map(|i| i as i32)
            .unwrap_or(-1);
        window.set_default_index(idx);
    }

    // 按 family 名精确查找索引，未命中返回 -1（Slint 侧"无高亮"哨兵值）
    fn family_index(entries: &[FontEntry], family: &str) -> i32 {
        entries
            .iter()
            .position(|e| e.family.as_str() == family)
            .map(|i| i as i32)
            .unwrap_or(-1)
    }
}

/// 解析十六进制颜色字符串（如 "#000000"）为 Slint Color
fn parse_color(s: &str) -> Option<slint::Color> {
    let s = s.strip_prefix('#')?;
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some(slint::Color::from_rgb_u8(r, g, b))
        }
        _ => None,
    }
}
