//! pty — PTY 管理层（Phase 2 接入）
//!
//! 职责（Phase 2+）：
//!   - 基于 `portable-pty` 创建 PTY 并 spawn 子进程
//!   - 提供 reader（输出）与 writer（输入）句柄
//!   - 在独立 Output Thread 读取 PTY 输出并通过 channel 上抛
//!   - 提供 resize 接口同步 PTY 尺寸
//!
//! 线程模型：
//!   - PTY Output Thread 拥有 reader，绝不接触 Slint UI
//!   - 更新事件经 channel 发往 UI 线程
//!
//! Phase 1 暂无实现。
