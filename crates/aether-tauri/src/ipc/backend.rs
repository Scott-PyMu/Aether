//! IPC 命令后端接口（M1-08：仅框架，实现随里程碑落地）。
//!
//! 命令层职责固定为：严格解析 → 语义校验 → 调用 [`IpcBackend`]。校验失败时后端
//! 方法不会被调用（不落库、不透传下游）。M2/M3 将提供实现该 trait 的真实后端
//! （aether-control / aether-store），命令签名与错误契约保持不变。

use std::path::Path;

use serde_json::Value;

use super::dto::{
    BackupCreateRequest, ExportDiagnosticsRequest, MessagesPageRequest, PermissionResolveRequest,
    PermissionsPendingRequest, SessionCreateRequest, SessionIdRequest, SessionListRequest,
    SessionSendRequest, SettingsGetRequest, SettingsSetRequest,
};
use super::error::IpcError;

/// 命令后端：默认实现全部返回 `not_implemented`（供未接线阶段使用）。
pub trait IpcBackend: Send + Sync + 'static {
    fn runtimes_list(&self) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("runtimes_list"))
    }

    fn session_list(&self, _request: &SessionListRequest) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("session_list"))
    }

    fn session_create(&self, _request: &SessionCreateRequest) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("session_create"))
    }

    fn session_send(&self, _request: &SessionSendRequest) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("session_send"))
    }

    fn session_interrupt(&self, _request: &SessionIdRequest) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("session_interrupt"))
    }

    fn session_dispose(&self, _request: &SessionIdRequest) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("session_dispose"))
    }

    fn messages_page(&self, _request: &MessagesPageRequest) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("messages_page"))
    }

    fn permissions_pending(&self, _request: &PermissionsPendingRequest) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("permissions_pending"))
    }

    fn permission_resolve(&self, _request: &PermissionResolveRequest) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("permission_resolve"))
    }

    fn settings_get(&self, _request: &SettingsGetRequest) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("settings_get"))
    }

    fn settings_set(&self, _request: &SettingsSetRequest) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("settings_set"))
    }

    fn backup_create(&self, _request: &BackupCreateRequest) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("backup_create"))
    }

    fn export_diagnostics(
        &self,
        _request: &ExportDiagnosticsRequest,
        _canonical_target_dir: &Path,
    ) -> Result<Value, IpcError> {
        Err(IpcError::not_implemented("export_diagnostics"))
    }
}

/// 未接线阶段的默认后端：命令面可见，但真实实现随对应里程碑接入。
#[derive(Debug, Default)]
pub struct NotImplementedBackend;

impl IpcBackend for NotImplementedBackend {}
