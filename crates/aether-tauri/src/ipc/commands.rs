//! D7 命令面（MVP 全集）：每个命令都走「严格解析 → 语义校验 → 后端」，
//! 校验失败返回结构化错误且不调用后端（不落库、不透传下游）。
//!
//! M1-08 只建立框架与命令面；真实实现由 [`crate::ipc::IpcBackend`] 的后续实现提供。
//! 单测矩阵见 `tests/ipc_validation.rs`。

use serde_json::Value;

use super::dto::{
    BackupCreateRequest, ExportDiagnosticsRequest, MessagesPageRequest, PermissionResolveRequest,
    PermissionsPendingRequest, SessionCreateRequest, SessionIdRequest, SessionListRequest,
    SessionSendRequest, SettingsGetRequest, SettingsSetRequest,
};
use super::error::IpcError;
use super::path;
use super::validate::parse_strict;
use super::IpcState;

#[tauri::command]
pub(crate) fn runtimes_list(state: tauri::State<'_, IpcState>) -> Result<Value, IpcError> {
    state.backend().runtimes_list()
}

#[tauri::command]
pub(crate) fn session_list(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SessionListRequest = parse_strict(payload)?;
    state.backend().session_list(&request)
}

#[tauri::command]
pub(crate) fn session_create(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SessionCreateRequest = parse_strict(payload)?;
    state.backend().session_create(&request)
}

#[tauri::command]
pub(crate) fn session_send(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SessionSendRequest = parse_strict(payload)?;
    state.backend().session_send(&request)
}

#[tauri::command]
pub(crate) fn session_interrupt(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SessionIdRequest = parse_strict(payload)?;
    state.backend().session_interrupt(&request)
}

#[tauri::command]
pub(crate) fn session_dispose(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SessionIdRequest = parse_strict(payload)?;
    state.backend().session_dispose(&request)
}

#[tauri::command]
pub(crate) fn messages_page(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: MessagesPageRequest = parse_strict(payload)?;
    state.backend().messages_page(&request)
}

#[tauri::command]
pub(crate) fn permissions_pending(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: PermissionsPendingRequest = parse_strict(payload)?;
    state.backend().permissions_pending(&request)
}

#[tauri::command]
pub(crate) fn permission_resolve(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: PermissionResolveRequest = parse_strict(payload)?;
    state.backend().permission_resolve(&request)
}

#[tauri::command]
pub(crate) fn settings_get(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SettingsGetRequest = parse_strict(payload)?;
    state.backend().settings_get(&request)
}

#[tauri::command]
pub(crate) fn settings_set(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SettingsSetRequest = parse_strict(payload)?;
    state.backend().settings_set(&request)
}

#[tauri::command]
pub(crate) fn backup_create(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: BackupCreateRequest = parse_strict(payload)?;
    state.backend().backup_create(&request)
}

#[tauri::command]
pub(crate) fn export_diagnostics(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: ExportDiagnosticsRequest = parse_strict(payload)?;
    let canonical = path::validate_user_path(&request.target_dir, state.allowed_roots())?;
    state.backend().export_diagnostics(&request, &canonical)
}

/// 应用命令面（设计 D7）。
///
/// debug 构建额外注册 E2E 探针命令（`e2e_probe_report`）；release 构建不包含，
/// 与「devtools 仅 debug 可达」的安全姿态一致。
#[cfg(debug_assertions)]
pub fn handler<R: tauri::Runtime>() -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static
{
    tauri::generate_handler![
        runtimes_list,
        session_list,
        session_create,
        session_send,
        session_interrupt,
        session_dispose,
        messages_page,
        permissions_pending,
        permission_resolve,
        settings_get,
        settings_set,
        backup_create,
        export_diagnostics,
        crate::probe::e2e_probe_report,
    ]
}

#[cfg(not(debug_assertions))]
pub fn handler<R: tauri::Runtime>() -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static
{
    tauri::generate_handler![
        runtimes_list,
        session_list,
        session_create,
        session_send,
        session_interrupt,
        session_dispose,
        messages_page,
        permissions_pending,
        permission_resolve,
        settings_get,
        settings_set,
        backup_create,
        export_diagnostics,
    ]
}
