//! events — 跨线程事件类型（Phase 2+ 使用）
//!
//! Phase 1 占位。Phase 2 接入 PTY Output Thread 后，
//! PTY 线程通过 channel 把 TerminalEvent 发送到 UI 线程，
//! UI 线程据此更新 ScreenBuffer 并触发重渲染。

use crate::app::controller::{GeneralSettingsInfo, ProfileEditInfo, SchemeColors};
use crate::session::session_id::ProfileId;
use crate::session::session_id::SessionId;
use slint::SharedString;
use slint::language::KeyboardModifiers;

#[derive(Debug, Clone)]
pub enum TerminalEvent {
    // PTY 有数据到达
    Data,
    // PTY 进程结束
    ChildExited(SessionId),
}

// ====== UI 线程回调事件 =======
pub enum WindowEvent {
    // ==== 终端窗口回调事件 =======
    // pty 终端事件
    Pty(TerminalEvent),
    // 新建标签页
    NewTab,
    // 新建标签页
    NewTabForProfile(ProfileId),
    // 关闭标签页
    CloseTab(SessionId),
    // 选择标签页
    SelectTab(SessionId),
    // 输入字符
    KeyInput(SharedString, KeyboardModifiers),
    // 通过 IME 输入字符
    ImeCommit(SharedString),
    // 鼠标滚动距离
    ScrollDelta(f32),
    // 鼠标滚动到
    ScrollTo(f32),
    // 窗口大小改变
    Resize,
    // 打开设置窗口
    OpenSettings,

    // ==== 设置窗口回调事件 =======
    // 关闭设置窗口
    CloseSettings,
    // 保存设置
    SaveSettings(GeneralSettingsInfo),
    // 保存配置文件
    SaveProfile(ProfileEditInfo),
    // 添加配置文件
    AddProfile(),
    // 删除配置文件
    DelProfile(SharedString),
    // 模糊搜索字体
    FilterFonts(SharedString, SharedString),
    // 配色方案选中（回填编辑器与预览）
    SchemeSelected(SharedString),
    // 另存为自定义配色方案
    SaveCustomScheme(SchemeColors),
    // 删除自定义配色方案
    DeleteCustomScheme(SharedString),
}
