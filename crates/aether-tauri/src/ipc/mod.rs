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

pub use backend::IpcBackend;
pub use commands::handler;
pub use error::{IpcError, IpcErrorCode};

/// 命令层共享状态：后端实现 + 路径白名单根目录。
pub struct IpcState {
    backend: Arc<dyn IpcBackend>,
    allowed_roots: Vec<PathBuf>,
}

impl IpcState {
    pub fn new(backend: Arc<dyn IpcBackend>, allowed_roots: Vec<PathBuf>) -> Self {
        Self {
            backend,
            allowed_roots,
        }
    }

    pub(crate) fn backend(&self) -> &dyn IpcBackend {
        &*self.backend
    }

    pub(crate) fn allowed_roots(&self) -> &[PathBuf] {
        &self.allowed_roots
    }
}
