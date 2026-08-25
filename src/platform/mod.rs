//! platform — 跨平台抽象（Phase 8 接入）
//!
//! 设计目标：避免业务代码大量使用 `#[cfg(target_os = "...")]`。
//! 抽象 `ShellProvider` trait，按平台实现默认 Profile 生成。
//! Phase 1 的默认 Profile 在 ProfileManager::create_default_profiles 内联生成，
//! Phase 8 将迁移到本模块的 trait 实现中。
