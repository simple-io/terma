//! manager — ProfileManager（统一配置管理器）
//!
//! 从 <config_dir>/terma/config.toml 加载完整配置（通用配置 + Profile 列表）。
//! 文件不存在时生成平台默认并落盘。支持 Profile CRUD + 通用配置更新 + 持久化。

use crate::config::color_scheme::{self, ColorScheme};
use crate::config::general::GeneralConfig;
use crate::config::loader::{self, ConfigFile};
use crate::config::profile::Profile;
use crate::session::session_id::ProfileId;

// 配置管理对象
pub struct ConfigManager {
    // 默认启动项
    default_profile_id: ProfileId,
    // 启动项列表
    profiles: Vec<Profile>,
    // 通用配置
    general: GeneralConfig,
}

impl Default for ConfigManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigManager {
    // 加载配置 config.toml, 若不存在时生成平台默认并落盘。
    pub fn new() -> Self {
        if let Some(file) = loader::load() {
            if file.profiles.is_empty() {
                return Self::default_platform();
            }

            // 校验 default_profile 是否有效，无效则回退到第一个
            let default_id = ProfileId::new(file.general.default_profile.clone());
            let default_id = if file.profiles.iter().any(|p| p.id == default_id) {
                default_id
            } else {
                file.profiles[0].id.clone()
            };

            return Self {
                default_profile_id: default_id,
                profiles: file.profiles,
                general: file.general,
            };
        }

        // 无配置文件：生成平台默认并落盘
        let mgr = Self::default_platform();
        let _ = mgr.save();
        mgr
    }

    // 生成默认的配置并存储
    fn default_platform() -> Self {
        let profiles = Profile::create_default_profiles();
        let default_profile_id = profiles
            .first()
            .map(|p| p.id.clone())
            .unwrap_or_else(|| ProfileId::new("default"));
        let mut general = GeneralConfig::default();
        general.default_profile = default_profile_id.0.clone();
        Self {
            profiles,
            default_profile_id,
            general,
        }
    }

    // 获取启动项列表
    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    // 根据 id 获取指定启动项
    pub fn get_profile(&self, id: &ProfileId) -> Option<&Profile> {
        self.profiles.iter().find(|p| &p.id == id)
    }

    // 获取默认启动项 id
    pub fn default_profile_id(&self) -> &ProfileId {
        &self.default_profile_id
    }

    // 获取默认启动项
    pub fn default_profile(&self) -> &Profile {
        self.get_profile(&self.default_profile_id)
            .expect("default profile must exist")
    }

    // 新增启动项，并返回它的索引
    pub fn add_profile(&mut self, profile: Profile) -> usize {
        self.profiles.push(profile);
        let _ = self.save();
        self.profiles.len() - 1
    }

    // 删除指定启动项，若删除的是默认 Profile，自动回退到第一个。
    pub fn delete_profile(&mut self, id: &ProfileId) -> bool {
        if let Some(pos) = self.profiles.iter().position(|p| &p.id == id) {
            self.profiles.remove(pos);
            // 若删除的是默认 Profile，回退到第一个（如有）
            if self.default_profile_id == *id {
                if let Some(first) = self.profiles.first() {
                    self.default_profile_id = first.id.clone();
                    self.general.default_profile = first.id.0.clone();
                }
            }
            let _ = self.save();
            true
        } else {
            false
        }
    }

    // 更新指定启动项， 成功返回 true，否则返回 false
    pub fn update_profile(&mut self, profile: Profile) -> bool {
        if let Some(pos) = self.profiles.iter().position(|p| p.id == profile.id) {
            self.profiles[pos] = profile;
            let _ = self.save();
            true
        } else {
            false
        }
    }

    // 设置默认启动项并持久化。
    pub fn set_default_profile(&mut self, id: ProfileId) -> bool {
        if self.profiles.iter().any(|p| p.id == id) {
            self.default_profile_id = id.clone();
            self.general.default_profile = id.0;
            let _ = self.save();
            true
        } else {
            false
        }
    }

    // 获取通用配置
    pub fn general(&self) -> &GeneralConfig {
        &self.general
    }

    // 当前生效配色方案（自定义优先，内置兜底，最终回退首个内置方案）。
    pub fn active_color_scheme(&self) -> ColorScheme {
        ColorScheme::new_from_schemes(&self.general.color_scheme, &self.general.color_schemes)
    }

    // 内置 + 自定义合并后的完整配色方案列表。
    pub fn all_schemes(&self) -> Vec<ColorScheme> {
        color_scheme::all_schemes(&self.general.color_schemes)
    }

    // 保存（新增或按名覆盖）一个自定义配色方案，并设为当前生效方案。
    // 名称为空时返回 false 且不落盘。
    pub fn save_custom_scheme(&mut self, scheme: ColorScheme) -> bool {
        if scheme.name.trim().is_empty() {
            return false;
        }
        if let Some(existing) = self
            .general
            .color_schemes
            .iter_mut()
            .find(|s| s.name == scheme.name)
        {
            *existing = scheme.clone();
        } else {
            self.general.color_schemes.push(scheme.clone());
        }
        self.general.color_scheme = scheme.name;
        let _ = self.save();
        true
    }

    // 删除指定名称的自定义配色方案；若删除的是当前生效方案，回退到首个内置方案。
    pub fn delete_custom_scheme(&mut self, name: &str) {
        self.general.color_schemes.retain(|s| s.name != name);
        if self.general.color_scheme == name {
            self.general.color_scheme = color_scheme::builtins()[0].name.clone();
        }
        let _ = self.save();
    }

    // 更新通用配置并持久化。
    pub fn update_general(&mut self, general: GeneralConfig) {
        // 确保 default_profile 仍然有效
        let default_id = ProfileId::new(general.default_profile.clone());
        if self.profiles.iter().any(|p| p.id == default_id) {
            self.default_profile_id = default_id;
            self.general = general;
        } else {
            // 无效则回退到当前默认或第一个
            let fallback = self
                .profiles
                .first()
                .map(|p| p.id.0.clone())
                .unwrap_or_default();
            self.general = general;
            self.default_profile_id = ProfileId::new(fallback);
        }
        let _ = self.save();
    }

    // 保存完整配置到 config.toml。
    pub fn save(&self) -> anyhow::Result<()> {
        let config = ConfigFile {
            general: self.general.clone(),
            profiles: self.profiles.clone(),
        };
        loader::save(&config)
    }
}
