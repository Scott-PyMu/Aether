//! D7 命令面（MVP 全集）：每个命令都走「严格解析 → 语义校验 → 后端」，
//! 校验失败返回结构化错误且不调用后端（不落库、不透传下游）。
//!
//! M1-08 只建立框架与命令面；真实实现由 [`crate::ipc::IpcBackend`] 的后续实现提供。
//! 单测矩阵见 `tests/ipc_validation.rs`。

use serde_json::Value;

use super::dto::{
    AppRestartRequest, BackupCreateRequest, BackupListRequest, BackupRestoreRequest,
    ExportDiagnosticsRequest, MessagesPageRequest, PermissionResolveRequest,
    PermissionsPendingRequest, RunRetryRequest, RuntimeEnableRequest, RuntimeRetryRequest,
    SessionCreateRequest, SessionIdRequest, SessionListRequest, SessionSendRequest,
    SettingsGetRequest, SettingsSetRequest, StartupGetRequest, StartupMigrateRequest,
    StartupPickTargetRequest, WorkspaceSetRequest,
};
use super::error::{IpcError, IpcErrorCode};
use super::path;
use super::validate::{parse_no_params, parse_strict};
use super::IpcState;

#[tauri::command]
pub(crate) fn runtimes_list(state: tauri::State<'_, IpcState>) -> Result<Value, IpcError> {
    state.backend_ready()?.runtimes_list()
}

#[tauri::command]
pub(crate) fn session_list(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SessionListRequest = parse_strict(payload)?;
    state.backend_ready()?.session_list(&request)
}

#[tauri::command]
pub(crate) fn session_create(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SessionCreateRequest = parse_strict(payload)?;
    state.backend_ready()?.session_create(&request)
}

#[tauri::command]
pub(crate) fn session_send(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SessionSendRequest = parse_strict(payload)?;
    state.backend_ready()?.session_send(&request)
}

#[tauri::command]
pub(crate) fn session_interrupt(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SessionIdRequest = parse_strict(payload)?;
    state.backend_ready()?.session_interrupt(&request)
}

#[tauri::command]
pub(crate) fn session_dispose(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SessionIdRequest = parse_strict(payload)?;
    state.backend_ready()?.session_dispose(&request)
}

#[tauri::command]
pub(crate) fn messages_page(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: MessagesPageRequest = parse_strict(payload)?;
    state.backend_ready()?.messages_page(&request)
}

#[tauri::command]
pub(crate) fn permissions_pending(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: PermissionsPendingRequest = parse_strict(payload)?;
    state.backend_ready()?.permissions_pending(&request)
}

#[tauri::command]
pub(crate) fn permission_resolve(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: PermissionResolveRequest = parse_strict(payload)?;
    state.backend_ready()?.permission_resolve(&request)
}

#[tauri::command]
pub(crate) fn settings_get(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SettingsGetRequest = parse_strict(payload)?;
    state.backend_ready()?.settings_get(&request)
}

#[tauri::command]
pub(crate) fn settings_set(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: SettingsSetRequest = parse_strict(payload)?;
    state.backend_ready()?.settings_set(&request)
}

#[tauri::command]
pub(crate) fn backup_create(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: BackupCreateRequest = parse_strict(payload)?;
    state.backend_ready()?.backup_create(&request)
}

/// ADR-004：无参数命令；缺省载荷等价空对象，任何成员都会被严格模式拒绝。
#[tauri::command]
pub(crate) fn backup_list(
    state: tauri::State<'_, IpcState>,
    payload: Option<Value>,
) -> Result<Value, IpcError> {
    let _request: BackupListRequest = parse_no_params(payload.unwrap_or(Value::Null))?;
    state.backend_ready()?.backup_list()
}

/// ADR-004/D13：外部候选先 canonicalize（存在性 + `.db` 后缀）再进入恢复七步。
#[tauri::command]
pub(crate) fn backup_restore(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: BackupRestoreRequest = parse_strict(payload)?;
    let canonical = request.canonical_external_path()?;
    state
        .backend_ready()?
        .backup_restore(&request, canonical.as_deref())
}

/// ADR-004：显式 `confirm:true` 才可重启（复用 D2 关闭序列）。
#[tauri::command]
pub(crate) fn app_restart(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: AppRestartRequest = parse_strict(payload)?;
    state.backend_ready()?.app_restart(&request)
}

/// ADR-004/M3-06：仅终态 run 可重试（`run_id` ULID；状态由后端判定）。
#[tauri::command]
pub(crate) fn run_retry(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: RunRetryRequest = parse_strict(payload)?;
    state.backend_ready()?.run_retry(&request)
}

/// ADR-004/M1-10：仅 `disabled + start_failed` 可用（白名单与状态由后端判定）。
#[tauri::command]
pub(crate) fn runtime_retry(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: RuntimeRetryRequest = parse_strict(payload)?;
    state.backend_ready()?.runtime_retry(&request)
}

/// ADR-004/M1-10：仅 `disabled` 可用；`untrusted`/`version_mismatch` 需先修复。
#[tauri::command]
pub(crate) fn runtime_enable(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: RuntimeEnableRequest = parse_strict(payload)?;
    state.backend_ready()?.runtime_enable(&request)
}

/// ADR-004/D14：`workspace_id` 或 `root_path`（canonicalize + 同步盘拒绝）。
#[tauri::command]
pub(crate) fn workspace_set(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: WorkspaceSetRequest = parse_strict(payload)?;
    let canonical = request.canonical_root_path()?;
    state
        .backend_ready()?
        .workspace_set(&request, canonical.as_deref())
}

#[tauri::command]
pub(crate) fn export_diagnostics(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: ExportDiagnosticsRequest = parse_strict(payload)?;
    let canonical = path::validate_user_path(&request.target_dir, state.allowed_roots())?;
    state
        .backend_ready()?
        .export_diagnostics(&request, &canonical)
}

/// M1-06：启动门快照（拒绝启动时仍可达；UI 据此渲染门界面）。
/// ADR-006：无参数命令；缺省载荷等价空对象，任何成员都会被严格模式拒绝。
#[tauri::command]
pub(crate) fn startup_get(
    state: tauri::State<'_, IpcState>,
    payload: Option<Value>,
) -> Result<Value, IpcError> {
    let _request: StartupGetRequest = parse_no_params(payload.unwrap_or(Value::Null))?;
    let gate = state.startup().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "启动门未接线（仅生产运行形态；框架测试不含启动门）",
        )
    })?;
    gate.snapshot_json()
}

/// M1-06：迁移目标目录选择（`DirectoryPicker` 抽象：生产为系统对话框，测试/E2E 注入替身；
/// 不新增 WebView capability 权限面）。用户取消返回 `{ "target_dir": null }`。
/// ADR-006：无参数命令；缺省载荷等价空对象，任何成员都会被严格模式拒绝。
#[tauri::command]
pub(crate) async fn startup_pick_target(
    state: tauri::State<'_, IpcState>,
    payload: Option<Value>,
) -> Result<Value, IpcError> {
    let _request: StartupPickTargetRequest = parse_no_params(payload.unwrap_or(Value::Null))?;
    let picker = state.picker().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "目录选择器未接线（仅生产运行形态；测试需注入 DirectoryPicker）",
        )
    })?;
    let picked = tauri::async_runtime::spawn_blocking(move || picker.pick_directory())
        .await
        .map_err(|error| IpcError::internal(format!("目录选择器调用失败：{error}")))?
        .map_err(|error| IpcError::internal(format!("目录选择器错误：{error}")))?;
    let target_dir = picked.map(|path| path.to_string_lossy().to_string());
    Ok(serde_json::json!({ "target_dir": target_dir }))
}

/// M1-06/A4：执行迁移（复制 → sha256 校验 → 原子替换 → 指针锁定新目录）。
#[tauri::command]
pub(crate) async fn startup_migrate(
    state: tauri::State<'_, IpcState>,
    payload: Value,
) -> Result<Value, IpcError> {
    let request: StartupMigrateRequest = parse_strict(payload)?;
    let gate = state.startup_arc().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "启动门未接线（仅生产运行形态；框架测试不含启动门）",
        )
    })?;
    let target = request.target_dir.clone();
    tauri::async_runtime::spawn_blocking(move || gate.migrate(&target))
        .await
        .map_err(|error| IpcError::migration_failed(format!("迁移任务执行失败：{error}")))?
}

/// M1-06：退出应用（拒绝启动界面「退出」按钮；不暴露窗口控制能力）。
#[tauri::command]
pub(crate) fn app_exit(state: tauri::State<'_, IpcState>) -> Result<Value, IpcError> {
    let handle = state.app_handle().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "应用句柄未接线（仅生产运行形态）",
        )
    })?;
    handle.exit(0);
    Ok(serde_json::json!({ "exiting": true }))
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
        backup_list,
        backup_restore,
        app_restart,
        run_retry,
        runtime_retry,
        runtime_enable,
        workspace_set,
        export_diagnostics,
        startup_get,
        startup_pick_target,
        startup_migrate,
        app_exit,
        crate::probe::e2e_probe_report,
        crate::startup_probe::e2e_startup_report,
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
        backup_list,
        backup_restore,
        app_restart,
        run_retry,
        runtime_retry,
        runtime_enable,
        workspace_set,
        export_diagnostics,
        startup_get,
        startup_pick_target,
        startup_migrate,
        app_exit,
    ]
}
