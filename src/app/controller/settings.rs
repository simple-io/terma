//! 设置窗口（SettingsWindow）相关逻辑：懒创建、回调绑定、
//! 通用配置 / Profile 的 CRUD 与字体模糊搜索。
//!
//! 作为 `App`（controller）的子模块，直接复用主控制器的
//! config / event_tx / main_window / sessions 等状态；
//! 保存设置后回写主窗口（字体、背景色、Profile 下拉、强制重绘）。

use super::*;
use crate::config::color_scheme::{self, ColorScheme};
use crate::config::cursor_color::{CursorColor, DEFAULT_CURSOR};
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
    pub(super) fn on_save_settings(&mut self, info: GeneralSettingsInfo) {
        let config = GeneralConfig {
            default_profile: info.default_profile_id.to_string(),
            font_family: info.font_family.to_string(),
            font_size: info.font_size as u32,
            cursor_style: info.cursor_style.to_string(),
            cursor_color: CursorColor {
                color: color_scheme::parse_hex(&info.cursor_color).unwrap_or(DEFAULT_CURSOR),
            },
            cursor_blink: info.cursor_blink,
            color_scheme: info.color_scheme.to_string(),
            color_schemes: self.config.general().color_schemes.clone(),
            bell_sound: info.bell_sound,
            scrollback_limit: info.scrollback_limit as usize,
        };
        self.config.update_general(config);

        // 配色方案即时生效：窗口背景/光标色 + 各会话配色
        self.apply_window_colors();
        self.sessions
            .apply_color_scheme(self.config.active_color_scheme());

        // 如果字体或字号发生变化，则即时生效，重新计算字符度量并注入 UI
        if self.config.general().font_size != self.main_window.get_font_size() as u32
            || self.config.general().font_family != self.main_window.get_font_family().to_string()
        {
            self.apply_font();
        }

        // 刷新主窗口 Profile 下拉菜单（默认 Profile 可能变化）
        self.apply_profiles();
        // 字号变化，重排窗口
        self.on_resized();
        // 强制全量重绘（网格未变时 on_resized 会跳过，此处兜底）
        self.render_active_force();
    }

    // 保存 Profile 修改
    pub(super) fn on_save_profile(&mut self, info: ProfileEditInfo) {
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
        self.config.update_profile(profile);
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
        let idx = self.config.add_profile(profile);

        // 选中新添加的 Profile
        if let Some(w) = self.settings_window.as_mut() {
            w.set_selected_index(idx as i32);
        }
    }

    // 删除 Profile
    pub(super) fn on_delete_profile(&mut self, id: SharedString) {
        let pid = ProfileId::new(id.to_string());
        self.config.delete_profile(&pid);
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

    // 配色方案选中：回填编辑器草稿 + 基线 + 预览色板（不影响主窗口，保存时才生效）。
    pub(super) fn on_scheme_selected(&mut self, name: SharedString) {
        let customs = self.config.general().color_schemes.clone();
        if let Some(w) = self.settings_window.as_mut() {
            let scheme = ColorScheme::new_from_schemes(&name, &customs);
            let ui = ui_scheme_colors(&scheme);
            w.set_scheme_colors(ui.clone());
            w.set_scheme_baseline(ui);
            w.set_scheme_preview(Rc::new(VecModel::from(preview_brushes(&scheme))).into());
        }
    }

    // 配色编辑器草稿变化：解析回填预览色板（编辑即时预览）。
    pub(super) fn on_scheme_colors_changed(&mut self, colors: SchemeColors) {
        let scheme = scheme_from_ui(&colors);
        if let Some(w) = self.settings_window.as_ref() {
            w.set_scheme_preview(Rc::new(VecModel::from(preview_brushes(&scheme))).into());
        }
    }

    // 光标色草稿变化：解析回填光标色预览色块（非法值回退默认）。
    pub(super) fn on_cursor_color_changed(&mut self, hex: SharedString) {
        let c = color_scheme::parse_hex(&hex).unwrap_or(DEFAULT_CURSOR);
        if let Some(w) = self.settings_window.as_ref() {
            w.set_cursor_color_preview(rgb_to_brush(c));
        }
    }

    // 配色方案「确认」临时应用：仅内存生效到终端，不落盘（保存时才持久化）。
    pub(super) fn on_apply_scheme_preview(&mut self, colors: SchemeColors) {
        let scheme = scheme_from_ui(&colors);
        // 终端区背景色（临时，读取草稿方案而非已保存配置）
        self.main_window.set_bg_color(rgb_to_color(scheme.background));
        // 会话配色（临时）
        self.sessions.apply_color_scheme(scheme);
        self.render_active_force();
    }

    // 光标色「确认」临时应用：仅内存生效到终端，不落盘。
    pub(super) fn on_apply_cursor_color_preview(&mut self, hex: SharedString) {
        let c = color_scheme::parse_hex(&hex).unwrap_or(DEFAULT_CURSOR);
        self.main_window
            .set_cursor_color(slint::Color::from_argb_u8(0x99, c.0, c.1, c.2));
        self.render_active_force();
    }

    // 颜色选择器预览请求：解析 hex → 回填弹层预览色（仅选择器内部预览，不触碰面板草稿）。
    // 非法值忽略（保持上一次预览色，避免预览色块闪烁/回跳默认）。
    pub(super) fn on_color_preview_request(&mut self, hex: SharedString) {
        if let Some(c) = color_scheme::parse_hex(&hex) {
            if let Some(w) = self.settings_window.as_ref() {
                w.set_active_preview_brush(rgb_to_brush(c));
                // 用递增计数驱动弹层同步，避免连续输入同一 hex 时因值相等而漏同步
                w.set_preview_response_token(w.get_preview_response_token() + 1);
            }
        }
    }

    // 「另存为」弹窗名称输入：实时校验并回填结果（空值 / 非法字符 / 重名）。
    pub(super) fn on_check_scheme_name(&mut self, name: SharedString) {
        let validation = self.validate_save_as_name(&name);
        if let Some(w) = self.settings_window.as_ref() {
            w.set_save_as_validation(validation);
        }
    }

    // 校验「另存为」新名称。规则：
    //   1) 去首尾空白后非空；
    //   2) 仅允许中英文/数字/空格及 `- _ .`，其余为非法字符；
    //   3) 忽略大小写与现有方案（内置 + 自定义）重名则冲突。
    fn validate_save_as_name(&self, name: &str) -> SaveAsValidation {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return SaveAsValidation {
                ok: false,
                message: "名称不能为空".into(),
            };
        }
        for ch in trimmed.chars() {
            if !(ch.is_alphanumeric() || ch == ' ' || ch == '-' || ch == '_' || ch == '.') {
                return SaveAsValidation {
                    ok: false,
                    message: "名称含非法字符（仅允许中英文、数字、空格及 - _ .）".into(),
                };
            }
        }
        if self
            .config
            .all_schemes()
            .iter()
            .any(|s| s.name.eq_ignore_ascii_case(trimmed))
        {
            return SaveAsValidation {
                ok: false,
                message: "已存在同名配色方案，请换一个名称".into(),
            };
        }
        SaveAsValidation {
            ok: true,
            message: "".into(),
        }
    }

    // 另存为自定义配色方案：持久化 + 设为当前生效 + 回填列表 + 立即应用到终端。
    pub(super) fn on_save_custom_scheme(&mut self, colors: SchemeColors) {
        let scheme = scheme_from_ui(&colors);
        if scheme.name.trim().is_empty() {
            return;
        }
        if self.config.save_custom_scheme(scheme) {
            self.apply_window_colors();
            self.sessions
                .apply_color_scheme(self.config.active_color_scheme());
            if let Some(w) = self.settings_window.as_ref() {
                self.sync_schemes_to_ui(w);
            }
            self.render_active_force();
        }
    }

    // 删除自定义配色方案：回退到内置默认并立即应用。
    pub(super) fn on_delete_custom_scheme(&mut self, name: SharedString) {
        self.config.delete_custom_scheme(&name.to_string());
        self.apply_window_colors();
        self.sessions
            .apply_color_scheme(self.config.active_color_scheme());
        if let Some(w) = self.settings_window.as_ref() {
            self.sync_schemes_to_ui(w);
        }
        self.render_active_force();
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

        // 配色方案选中 → 回填编辑器与预览
        let command_tx = self.event_tx.clone();
        window.on_scheme_selected(move |name| {
            command_tx.send(WindowEvent::SchemeSelected(name)).unwrap();
        });

        // 配色草稿变化 → 解析回填预览色板
        let command_tx = self.event_tx.clone();
        window.on_scheme_colors_changed(move |colors| {
            command_tx
                .send(WindowEvent::SchemeColorsChanged(colors))
                .unwrap();
        });

        // 另存为自定义配色方案
        let command_tx = self.event_tx.clone();
        window.on_save_custom_scheme(move |colors| {
            command_tx
                .send(WindowEvent::SaveCustomScheme(colors))
                .unwrap();
        });

        // 删除自定义配色方案
        let command_tx = self.event_tx.clone();
        window.on_delete_custom_scheme(move |name| {
            command_tx
                .send(WindowEvent::DeleteCustomScheme(name))
                .unwrap();
        });

        // 光标色草稿变化 → 解析回填光标色预览
        let command_tx = self.event_tx.clone();
        window.on_cursor_color_changed(move |hex| {
            command_tx
                .send(WindowEvent::CursorColorChanged(hex))
                .unwrap();
        });

        // 配色方案「确认」临时应用 → 终端（不落盘）
        let command_tx = self.event_tx.clone();
        window.on_apply_scheme_preview(move |colors| {
            command_tx
                .send(WindowEvent::ApplySchemePreview(colors))
                .unwrap();
        });

        // 光标色「确认」临时应用 → 终端（不落盘）
        let command_tx = self.event_tx.clone();
        window.on_apply_cursor_color_preview(move |hex| {
            command_tx
                .send(WindowEvent::ApplyCursorColorPreview(hex))
                .unwrap();
        });

        // 颜色选择器预览请求 → 解析回填弹层预览色（不触碰面板草稿）
        let command_tx = self.event_tx.clone();
        window.on_color_preview_request(move |hex| {
            command_tx
                .send(WindowEvent::ColorPreviewRequest(hex))
                .unwrap();
        });

        // 「另存为」弹窗名称输入 → 实时校验回填
        let command_tx = self.event_tx.clone();
        window.on_save_as_name_edited(move |name| {
            command_tx
                .send(WindowEvent::CheckSchemeName(name))
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
        // let mgr = self.config.borrow();

        // 通用配置
        let general = self.config.general();
        window.set_general_settings(GeneralSettingsInfo {
            font_family: general.font_family.as_str().into(),
            font_size: general.font_size as i32,
            cursor_style: general.cursor_style.as_str().into(),
            cursor_color: color_scheme::to_hex(general.cursor_color.color)
                .as_str()
                .into(),
            cursor_blink: general.cursor_blink,
            color_scheme: general.color_scheme.as_str().into(),
            bell_sound: general.bell_sound,
            scrollback_limit: general.scrollback_limit as i32,
            default_profile_id: self.config.default_profile_id().0.clone().into(),
        });
        // 光标色预览色块初值
        window.set_cursor_color_preview(rgb_to_brush(general.cursor_color.color));

        // Profile 列表
        let profiles: Vec<ProfileEditInfo> = self
            .config
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
        window.set_font_current_index(Self::family_index(&font_entries, &general.font_family));

        // 默认启动项下拉框：由 default-profile-id 反查列表索引注入
        let default_id = self.config.default_profile_id().0.clone();
        let idx = self
            .config
            .profiles()
            .iter()
            .position(|p| p.id.0 == default_id)
            .map(|i| i as i32)
            .unwrap_or(-1);
        window.set_default_index(idx);

        // 配色方案：列表 + 当前生效方案编辑器草稿 + 预览色板
        self.sync_schemes_to_ui(window);
    }

    // 把配色方案列表 / 当前生效方案 / 预览色板同步到设置窗口。
    fn sync_schemes_to_ui(&self, window: &SettingsWindow) {
        let schemes = self.config.all_schemes();
        let list: Vec<ColorSchemeEntry> = schemes
            .iter()
            .map(|s| ColorSchemeEntry {
                name: s.name.as_str().into(),
                is_custom: self
                    .config
                    .general()
                    .color_schemes
                    .iter()
                    .any(|c| c.name == s.name),
                fg: rgb_to_brush(s.foreground),
                bg: rgb_to_brush(s.background),
            })
            .collect();
        window.set_color_scheme_list(Rc::new(VecModel::from(list)).into());

        let general = self.config.general();
        let scheme = ColorScheme::new_from_schemes(&general.color_scheme, &general.color_schemes);
        let ui = ui_scheme_colors(&scheme);
        window.set_scheme_colors(ui.clone());
        window.set_scheme_baseline(ui);
        window.set_scheme_preview(Rc::new(VecModel::from(preview_brushes(&scheme))).into());

        let idx = schemes
            .iter()
            .position(|s| s.name.eq_ignore_ascii_case(&general.color_scheme))
            .map(|i| i as i32)
            .unwrap_or(0);
        window.set_scheme_index(idx);
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

/// 配置层 ColorScheme → 设置窗口编辑器草稿（色值规范化为 `#rrggbb`）。
fn ui_scheme_colors(s: &ColorScheme) -> SchemeColors {
    let ansi = |i: usize| color_scheme::to_hex(s.ansi[i]).into();
    SchemeColors {
        name: s.name.as_str().into(),
        foreground: color_scheme::to_hex(s.foreground).into(),
        background: color_scheme::to_hex(s.background).into(),
        selection: color_scheme::to_hex(s.selection).into(),
        ansi0: ansi(0),
        ansi1: ansi(1),
        ansi2: ansi(2),
        ansi3: ansi(3),
        ansi4: ansi(4),
        ansi5: ansi(5),
        ansi6: ansi(6),
        ansi7: ansi(7),
        ansi8: ansi(8),
        ansi9: ansi(9),
        ansi10: ansi(10),
        ansi11: ansi(11),
        ansi12: ansi(12),
        ansi13: ansi(13),
        ansi14: ansi(14),
        ansi15: ansi(15),
    }
}

/// 设置窗口编辑器草稿 → 配置层 ColorScheme（非法色值逐项回退默认）。
fn scheme_from_ui(c: &SchemeColors) -> ColorScheme {
    let palette = [
        c.ansi0.to_string(),
        c.ansi1.to_string(),
        c.ansi2.to_string(),
        c.ansi3.to_string(),
        c.ansi4.to_string(),
        c.ansi5.to_string(),
        c.ansi6.to_string(),
        c.ansi7.to_string(),
        c.ansi8.to_string(),
        c.ansi9.to_string(),
        c.ansi10.to_string(),
        c.ansi11.to_string(),
        c.ansi12.to_string(),
        c.ansi13.to_string(),
        c.ansi14.to_string(),
        c.ansi15.to_string(),
    ];
    let mut ansi = color_scheme::XTERM_16;
    for (i, s) in palette.iter().enumerate() {
        if let Some(c) = color_scheme::parse_hex(s) {
            ansi[i] = c;
        }
    }
    ColorScheme {
        name: c.name.to_string(),
        foreground: color_scheme::parse_hex(&c.foreground).unwrap_or(color_scheme::DEFAULT_FG),
        background: color_scheme::parse_hex(&c.background).unwrap_or(color_scheme::DEFAULT_BG),
        selection: color_scheme::parse_hex(&c.selection).unwrap_or(color_scheme::DEFAULT_FG),
        ansi,
    }
}

/// 方案解析后的 19 个 brush：前景/背景/选区 + 16 色 ANSI（供预览色板）。
fn preview_brushes(s: &ColorScheme) -> Vec<slint::Brush> {
    let mut v = Vec::with_capacity(19);
    v.push(rgb_to_brush(s.foreground));
    v.push(rgb_to_brush(s.background));
    v.push(rgb_to_brush(s.selection));
    for c in s.ansi {
        v.push(rgb_to_brush(c));
    }
    v
}
