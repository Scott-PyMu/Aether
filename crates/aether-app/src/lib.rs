//! # aether-app — 应用层（transport-agnostic）
//!
//! ADR-029 D-029-2/D-029-4：把与桌面壳无关的业务层（命令面 `dispatch`/`COMMANDS`、
//! 请求 DTO、结构化错误、严格校验、路径白名单、目录选择器 trait、启动门、后端装配、
//! 会话/备份/诊断/供应商等业务编排）收敛到本 crate。
//!
//! 硬约束（AGENTS §2.1）：
//! - **不得**依赖 Tauri/Electron/Node、窗口/对话框实现；`cargo tree -p aether-app`
//!   中不得出现 `tauri` / `wry` / `webkit`；
//! - 只依赖 `aether-core` / `aether-store` / `aether-adapters` / `aether-control` /
//!   `aether-security` 与叶子第三方库；
//! - 依赖方向单向：core → store / adapters / control / security → **app** → daemon / tauri。
//!
//! 命令面入口：[`ipc::IpcState::dispatch`]（方法名 + 内层 `params`），以及
//! [`ipc::COMMANDS`]（63 条命令清单，M9-04 据此生成 `command-manifest.json`）。

// 核心 crate 禁止 unwrap/expect/panic（AGENTS §2.2）；lib 内单元测试显式豁免
// （与 aether-core/store/adapters/control/security 同口径；集成测试各自在文件级豁免）。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod adapter_executor;
pub mod archive_control;
pub mod audit_bridge;
pub mod backup_auto;
pub mod backup_control;
pub mod boot;
pub mod config;
pub mod core_health;
pub mod credential_broker;
pub mod diagnostics_control;
pub mod disk;
pub mod ipc;
pub mod isolation;
pub mod json_payload;
pub mod logging;
pub mod permission_loop;
pub mod picker;
pub mod preset_control;
pub mod provider_catalog;
pub mod provider_connectivity;
pub mod provider_control;
pub mod provider_env;
pub mod registry_identity;
pub mod retention;
pub mod runtime_control;
pub mod runtime_registry;
pub mod security_level;
pub mod session_backend;
pub mod shell_host;
pub mod shutdown;
pub mod startup;

pub use ipc::{IpcBackend, IpcError, IpcErrorCode, IpcState, COMMANDS};
pub use shell_host::ShellHost;

/// 产品名（诊断包/证据行使用；与安装产物 productName 一致）。
pub const APP_NAME: &str = "Aether";

/// 应用版本号——单一版本来源（工作区 `Cargo.toml`，构建时注入）。
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 核心层版本号（诊断与冒烟测试使用）。
pub fn core_version() -> &'static str {
    aether_core::version()
}
