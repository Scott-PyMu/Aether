//! IPC 命令层（壳适配）——命令实现全部在 `aether-app`（ADR-029 D-029-2/D-029-4）。
//!
//! 本模块只保留：
//! - Tauri 命令 wrapper（[`commands`]）：注册 + `State` 适配后调用 `IpcState::dispatch`；
//! - 应用层 [`aether_app::ipc`] 的类型/模块再导出（DTO、错误、校验、路径、后端、状态），
//!   保持迁移期既有路径（`aether_tauri::ipc::dto::*` 等）与集成测试不失效。

pub mod commands;

pub use aether_app::ipc::*;
pub use commands::handler;
