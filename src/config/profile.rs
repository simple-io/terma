//! config — Profile 数据结构
//!
//! 一个 Profile 描述「如何启动一个终端」：
//!   - 用什么 shell / command
//!   - 启动参数
//!   - 工作目录
//!   - 环境变量
//!   - 图标 / 颜色方案（后续）

use crate::session::session_id::ProfileId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

// 启动项配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    // 唯一标识
    pub id: ProfileId,
    // 名称
    pub name: String,
    // 命令
    pub command: String,
    // 参数
    pub args: Vec<String>,
    // 工作目录
    pub working_directory: Option<PathBuf>,
    // 环境变量
    pub env_vars: BTreeMap<String, String>,
    // 图标
    pub icon: Option<String>,
    // 颜色方案
    pub color_scheme: Option<String>,
}

impl Profile {
    pub fn new(id: impl Into<String>, name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            id: ProfileId::new(id),
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            working_directory: None,
            env_vars: BTreeMap::new(),
            icon: None,
            color_scheme: None,
        }
    }

    // 按平台生成默认 Profile 列表。
    pub fn create_default_profiles() -> Vec<Self> {
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

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }
}

// 把 env_vars 文本解析为 BTreeMap。
// 每个条目为 KEY=VALUE，条目之间支持以换行、逗号、分号（含中文全角）分隔。
pub fn parse_env_vars_text(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for entry in text.split(['\n', '\r', ',', ';', '，', '；']) {
        let entry = entry.trim();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = entry.split_once('=') {
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            map.insert(key.to_string(), value.trim().to_string());
        }
    }
    map
}

// 把 BTreeMap 格式化为 env_vars 文本（KEY=VALUE 每行一对）。
pub fn format_env_vars_text(map: &BTreeMap<String, String>) -> String {
    map.iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("\n")
}
