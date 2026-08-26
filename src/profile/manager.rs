//! manager — ProfileManager
//!
//! Phase 6：从 <config_dir>/terma/profiles.toml 加载；文件不存在时生成平台默认并落盘。
//! 支持 set_default_profile + save。

use crate::profile::loader;
use crate::profile::profile::Profile;
use crate::session::session_id::ProfileId;

pub struct ProfileManager {
    profiles: Vec<Profile>,
    default_profile_id: ProfileId,
}

impl ProfileManager {
    /// 加载配置：优先读 profiles.toml；不存在则生成平台默认并尝试落盘。
    pub fn new() -> Self {
        if let Some(file) = loader::load() {
            let profiles: Vec<Profile> = file.profiles.iter().map(loader::entry_to_profile).collect();
            if profiles.is_empty() {
                return Self::default_platform();
            }
            // 默认配置
            let default_id = ProfileId::new(file.default_profile);
            let default_id = if profiles.iter().any(|p| p.id == default_id) {
                // 有配置则使用
                default_id
            } else {
                // 无配置, 使用第一个
                profiles[0].id.clone()
            };
            return Self {
                profiles,
                default_profile_id: default_id,
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
        Self {
            profiles,
            default_profile_id,
        }
    }

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

    /// 设置默认 Profile 并持久化。
    pub fn set_default_profile(&mut self, id: ProfileId) -> bool {
        if self.profiles.iter().any(|p| p.id == id) {
            self.default_profile_id = id;
            let _ = self.save();
            true
        } else {
            false
        }
    }

    /// 保存到 profiles.toml。
    pub fn save(&self) -> anyhow::Result<()> {
        loader::save(&self.profiles, &self.default_profile_id)
    }

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
