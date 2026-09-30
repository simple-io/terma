//! manager — ProfileManager（统一配置管理器）
//!
//! 从 <config_dir>/terma/config.toml 加载完整配置（通用配置 + Profile 列表）。
//! 文件不存在时生成平台默认并落盘。支持 Profile CRUD + 通用配置更新 + 持久化。

use crate::profile::loader::{self, ConfigFile, GeneralConfig, ProfileEntry};
use crate::profile::profile::Profile;
use crate::session::session_id::ProfileId;

pub struct ProfileManager {
    profiles: Vec<Profile>,
    default_profile_id: ProfileId,
    general: GeneralConfig,
}

impl ProfileManager {
    /// 加载配置：优先读 config.toml；不存在则尝试迁移旧版 profiles.toml；
    /// 两者都不存在时生成平台默认并落盘。
    pub fn new() -> Self {
        if let Some(file) = loader::load() {
            let profiles: Vec<Profile> =
                file.profiles.iter().map(loader::entry_to_profile).collect();
            if profiles.is_empty() {
                return Self::default_platform();
            }

            // 校验 default_profile 是否有效，无效则回退到第一个
            let default_id = ProfileId::new(file.general.default_profile.clone());
            let default_id = if profiles.iter().any(|p| p.id == default_id) {
                default_id
            } else {
                profiles[0].id.clone()
            };

            return Self {
                profiles,
                default_profile_id: default_id,
                general: file.general,
            };
        }

        // 无配置文件：生成平台默认并落盘
        let mgr = Self::default_platform();
        let _ = mgr.save();
        mgr
    }

    fn default_platform() -> Self {
        let profiles = Self::create_default_profiles();
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

    // ─── Profile 查询 ───

    // 获取启动项列表
    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    pub fn get_profile(&self, id: &ProfileId) -> Option<&Profile> {
        self.profiles.iter().find(|p| &p.id == id)
    }

    pub fn default_profile(&self) -> &Profile {
        self.get_profile(&self.default_profile_id)
            .expect("default profile must exist")
    }

    pub fn default_profile_id(&self) -> &ProfileId {
        &self.default_profile_id
    }

    // ─── 通用配置查询 ───

    pub fn general(&self) -> &GeneralConfig {
        &self.general
    }

    // ─── Profile 修改 ───

    /// 设置默认 Profile 并持久化。
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

    /// 添加新 Profile 并持久化。返回新 Profile 的索引。
    pub fn add_profile(&mut self, profile: Profile) -> usize {
        self.profiles.push(profile);
        let _ = self.save();
        self.profiles.len() - 1
    }

    /// 更新指定 Profile 并持久化。若 id 不存在则返回 false。
    pub fn update_profile(&mut self, profile: Profile) -> bool {
        if let Some(pos) = self.profiles.iter().position(|p| p.id == profile.id) {
            self.profiles[pos] = profile;
            let _ = self.save();
            true
        } else {
            false
        }
    }

    /// 删除指定 Profile 并持久化。
    /// 若删除的是默认 Profile，自动回退到第一个。
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

    // ─── 通用配置修改 ───

    /// 更新通用配置并持久化。
    pub fn update_general(&mut self, general: GeneralConfig) {
        // 确保 default_profile 仍然有效
        let default_id = ProfileId::new(general.default_profile.clone());
        if !self.profiles.iter().any(|p| p.id == default_id) {
            // 无效则回退到当前默认或第一个
            let fallback = self
                .profiles
                .first()
                .map(|p| p.id.0.clone())
                .unwrap_or_default();
            let mut fixed = general;
            fixed.default_profile = fallback;
            self.general = fixed;
        } else {
            self.default_profile_id = default_id;
            self.general = general;
        }
        let _ = self.save();
    }

    // ─── 持久化 ───

    /// 保存完整配置到 config.toml。
    pub fn save(&self) -> anyhow::Result<()> {
        let config = ConfigFile {
            general: self.general.clone(),
            profiles: self.profiles.iter().map(loader::profile_to_entry).collect(),
        };
        loader::save(&config)
    }

    // ─── 默认 Profile 生成 ───

    /// 按平台生成默认 Profile 列表。
    fn create_default_profiles() -> Vec<Profile> {
        #[cfg(target_os = "windows")]
        {
            vec![
                Profile::new("powershell", "PowerShell", "powershell.exe"),
                Profile::new("cmd", "Command Prompt", "cmd.exe"),
                Profile::new("wsl-ubuntu", "WSL Ubuntu", "wsl.exe")
                    .with_args(vec!["-d".into(), "Ubuntu".into()]),
            ]
        }

        #[cfg(target_os = "macos")]
        {
            let mut list = Vec::new();
            if std::path::Path::new("/bin/bash").exists() {
                list.push(Profile::new("bash", "Bash", "/bin/bash"));
            }
            if std::path::Path::new("/bin/zsh").exists() {
                list.push(Profile::new("zsh", "Zsh", "/bin/zsh"));
            }
            if list.is_empty() {
                list.push(Profile::new("sh", "Shell", "/bin/sh"));
            }
            list
        }

        #[cfg(target_os = "linux")]
        {
            let mut list = Vec::new();
            if std::path::Path::new("/bin/bash").exists() {
                list.push(Profile::new("bash", "Bash", "/bin/bash"));
            }
            if std::path::Path::new("/bin/zsh").exists() {
                list.push(Profile::new("zsh", "Zsh", "/bin/zsh"));
            }
            if list.is_empty() {
                list.push(Profile::new("sh", "Shell", "/bin/sh"));
            }
            list
        }
    }
}

impl Default for ProfileManager {
    fn default() -> Self {
        Self::new()
    }
}
