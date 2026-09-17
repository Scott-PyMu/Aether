//! IPC 命令层（设计 D7）：命令面、严格参数校验框架与结构化错误。
//!
//! 安全基线（评审 #7）：
//! - 每个命令先经 [`validate::parse_strict`]（serde `deny_unknown_fields` +
//!   长度 / 枚举 / 格式校验）；
//! - 路径参数经 [`path::validate_user_path`]（canonicalize + 允许根目录前缀比较）；
//! - 校验失败返回 [`IpcError`]（结构化错误码），不调用后端、不落库、不透传下游。

pub mod backend;
pub mod commands;
pub mod dto;
pub mod error;
pub mod path;
pub mod validate;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

pub use backend::IpcBackend;
pub use commands::handler;
pub use error::{IpcError, IpcErrorCode};

use crate::startup::StartupGate;

/// 命令层共享状态：后端实现 + 路径白名单根目录 + 启动门（M1-06）。
pub struct IpcState {
    backend: Arc<dyn IpcBackend>,
    allowed_roots: Vec<PathBuf>,
    startup: Option<Arc<StartupGate>>,
    app: OnceLock<tauri::AppHandle>,
}

impl IpcState {
    /// 未接线启动门的构造（M1-08 校验矩阵与框架测试用；生产见 [`Self::with_startup`]）。
    pub fn new(backend: Arc<dyn IpcBackend>, allowed_roots: Vec<PathBuf>) -> Self {
        Self {
            backend,
            allowed_roots,
            startup: None,
            app: OnceLock::new(),
        }
    }

    /// 生产构造：启动门生效后，未 Ready 时业务命令一律 `startup_blocked`。
    pub fn with_startup(
        backend: Arc<dyn IpcBackend>,
        allowed_roots: Vec<PathBuf>,
        startup: Arc<StartupGate>,
    ) -> Self {
        Self {
            backend,
            allowed_roots,
            startup: Some(startup),
            app: OnceLock::new(),
        }
    }

    pub fn startup(&self) -> Option<&StartupGate> {
        self.startup.as_deref()
    }

    pub fn startup_arc(&self) -> Option<Arc<StartupGate>> {
        self.startup.clone()
    }

    /// setup 阶段登记应用句柄（`app_exit` / 目录选择器使用）。
    pub fn set_app_handle(&self, handle: tauri::AppHandle) {
        let _ = self.app.set(handle);
    }

    pub(crate) fn app_handle(&self) -> Option<&tauri::AppHandle> {
        self.app.get()
    }

    /// 业务命令入口：启动门未 Ready 时阻断（主界面不可达的命令层兜底）。
    pub(crate) fn backend_ready(&self) -> Result<&dyn IpcBackend, IpcError> {
        if let Some(gate) = &self.startup {
            gate.ensure_ready()?;
        }
        Ok(&*self.backend)
    }

    pub(crate) fn allowed_roots(&self) -> &[PathBuf] {
        &self.allowed_roots
    }
}
