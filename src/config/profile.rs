//! config — Profile 数据结构
//!
//! 一个 Profile 描述「如何启动一个终端」：
//!   - 用什么 shell / command
//!   - 启动参数
//!   - 工作目录
//!   - 环境变量
//!   - 图标 / 颜色方案（后续）

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::session::session_id::ProfileId;

// 启动项配置
#[derive(Debug, Clone)]
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

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }
}
