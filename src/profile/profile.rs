//! profile — Profile 数据结构
//!
//! 一个 Profile 描述「如何启动一个终端」：
//!   - 用什么 shell / command
//!   - 启动参数
//!   - 工作目录
//!   - 图标 / 颜色方案（后续）

use std::path::PathBuf;

use crate::session::session_id::ProfileId;

#[derive(Debug, Clone)]
pub struct Profile {
    pub id: ProfileId,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub working_directory: Option<PathBuf>,
    pub icon: Option<String>,
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
            icon: None,
            color_scheme: None,
        }
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }
}
