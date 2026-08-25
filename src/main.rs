//! terma — 跨平台 AI 终端
//!
//! Phase 1：项目骨架 + Slint 主窗口 + Tab 栏 + 默认 Tab + + 按钮创建新 Tab
//!          + Tab 切换 + 关闭 Tab + Profile 下拉菜单占位。
//!
//! 后续阶段路线：
//!   Phase 2  单 TerminalSession（portable-pty）
//!   Phase 3  vte 解析 + TerminalState + ScreenBuffer
//!   Phase 4  Terminal Renderer（Slint TerminalView 渲染 Cell 网格）
//!   Phase 5  多 Tab（多 Session 完全隔离）
//!   Phase 6  ProfileManager 持久化
//!   Phase 7  终端高级功能（ANSI 颜色 / 光标 / Scrollback / Alternate Screen / Resize / 键盘映射）
//!   Phase 8  跨平台测试
//!   Phase 9  AI 功能

// 脚手架阶段：pty / terminal / renderer / profile 等模块预置了后续阶段将使用的
// 公共 API 接口与类型，当前阶段尚未被调用。此处统一放开 dead_code / unused_imports
// 警告，待相应 Phase 接入后再按需移除该允许项。
#![allow(dead_code, unused_imports)]

mod app;
mod platform;
mod profile;
mod pty;
mod renderer;
mod session;
mod terminal;

fn main() {
    app::App::run();
}
