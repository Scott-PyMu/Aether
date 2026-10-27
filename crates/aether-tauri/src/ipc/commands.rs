//! D7 命令面（迁移期 Tauri 薄适配层，ADR-029 D-029-2/D-029-4）。
//!
//! **每个 wrapper 只做 Tauri 注册 / State 适配**：参数经 JsonPayload 透传，
//! 业务、校验、错误映射全部在 ether-app（IpcState::dispatch / IpcState::COMMANDS）。
//! 签名与返回类型保持不变（packages/protocol/src/bindings.ts 生成物 0 差异；AGENTS §2.8）。
//!
//! 阻塞动作（系统选择器 / 迁移复制）经 spawn_blocking 调度，不占用 UI 线程；
//! health 的 debug E2E 挂起探针留在壳层（不影响命令语义）。

use serde_json::Value;

use aether_app::ipc::{IpcError, IpcState};
use aether_app::json_payload::JsonPayload;
#[tauri::command]
#[specta::specta]
pub(crate) fn runtimes_list(state: tauri::State<'_, IpcState>) -> Result<JsonPayload, IpcError> {
    state.dispatch("runtimes_list", Value::Null).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_list(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_list", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_create(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_create", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_search(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_search", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_rename(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_rename", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_delete(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_delete", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_send(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_send", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_interrupt(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_interrupt", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_pause(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_pause", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_resume(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_resume", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_dispose(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_dispose", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn messages_page(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("messages_page", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn permissions_pending(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("permissions_pending", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn permission_resolve(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("permission_resolve", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_permission_mode_set(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_permission_mode_set", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn rules_list(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("rules_list", payload.map(JsonPayload::into_value).unwrap_or(Value::Null)).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn rule_add(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("rule_add", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn rule_update(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("rule_update", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn rule_delete(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("rule_delete", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn rule_toggle(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("rule_toggle", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn settings_get(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("settings_get", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn settings_set(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("settings_set", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn backup_create(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("backup_create", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn backup_list(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("backup_list", payload.map(JsonPayload::into_value).unwrap_or(Value::Null)).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn backup_restore(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("backup_restore", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn app_restart(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("app_restart", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn run_retry(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("run_retry", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn runtime_retry(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("runtime_retry", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn runtime_enable(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("runtime_enable", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn runtime_disable(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("runtime_disable", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn workspace_set(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("workspace_set", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn workspaces_list(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("workspaces_list", payload.map(JsonPayload::into_value).unwrap_or(Value::Null)).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn workspace_create(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("workspace_create", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn workspace_update(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("workspace_update", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn workspace_delete(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("workspace_delete", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_workspace_assign(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_workspace_assign", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn ref_pick(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let state = state.inner().clone();
    let params = payload.into_value();
    tauri::async_runtime::spawn_blocking(move || state.dispatch("ref_pick", params))
        .await
        .map_err(|error| IpcError::internal(format!("命令任务执行失败：{error}")))?
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn artifacts_list(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("artifacts_list", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn artifact_add(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("artifact_add", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn artifact_remove(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("artifact_remove", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn providers_list(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("providers_list", payload.map(JsonPayload::into_value).unwrap_or(Value::Null)).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn provider_create(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("provider_create", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn provider_update(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("provider_update", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn provider_delete(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("provider_delete", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn provider_toggle(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("provider_toggle", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn provider_model_add(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("provider_model_add", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn provider_model_toggle(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("provider_model_toggle", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn provider_test(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("provider_test", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn provider_models_fetch(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("provider_models_fetch", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn presets_list(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("presets_list", payload.map(JsonPayload::into_value).unwrap_or(Value::Null)).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn preset_add(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("preset_add", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn preset_update(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("preset_update", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn preset_delete(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("preset_delete", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn export_diagnostics(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("export_diagnostics", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_export(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("session_export", payload.into_value()).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn archive_export(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("archive_export", payload.map(JsonPayload::into_value).unwrap_or(Value::Null)).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn audit_query(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("audit_query", payload.map(JsonPayload::into_value).unwrap_or(Value::Null)).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn audit_export(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("audit_export", payload.map(JsonPayload::into_value).unwrap_or(Value::Null)).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn health(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    #[cfg(debug_assertions)]
    crate::health_probe::maybe_stall_health().await;
    state.dispatch("health", payload.map(JsonPayload::into_value).unwrap_or(Value::Null)).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn startup_get(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("startup_get", payload.map(JsonPayload::into_value).unwrap_or(Value::Null)).map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn startup_pick_target(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    let state = state.inner().clone();
    let params = payload.map(JsonPayload::into_value).unwrap_or(Value::Null);
    tauri::async_runtime::spawn_blocking(move || state.dispatch("startup_pick_target", params))
        .await
        .map_err(|error| IpcError::internal(format!("命令任务执行失败：{error}")))?
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn startup_migrate(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let state = state.inner().clone();
    let params = payload.into_value();
    tauri::async_runtime::spawn_blocking(move || state.dispatch("startup_migrate", params))
        .await
        .map_err(|error| IpcError::internal(format!("命令任务执行失败：{error}")))?
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn app_exit(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    state.dispatch("app_exit", payload.into_value()).map(JsonPayload)
}

/// 命令收集（M3-01/T14：`packages/protocol/src/bindings.ts` 生成用）。
///
/// 与 [`handler`]（release 构建）注册的命令集合一一对应：D7 P0 全集 36 个可调用命令
/// （ADR-004 七命令、ADR-006 四命令、ADR-007 `health`、ADR-010 文件引用四命令
/// 与供应商七命令，M3-09/M3-11 落地）。debug 构建额外注册的 E2E
/// 探针命令不进入绑定，保证生成物与构建配置无关（生成/校验口径见 `docs/M3-01-证据.md`）。
///
/// 运行期命令注册仍走 [`handler`]：本函数只服务于类型导出，不改变 M1-08 校验契约
/// （命令入参经 [`JsonPayload`] 透传，绑定中 payload 为 `unknown`；DTO 类型经
/// `bindings::builder` 的 `.typ::<T>()` 单独导出供前端使用）。
pub fn collected<R: tauri::Runtime>() -> tauri_specta::Commands<R> {
    tauri_specta::collect_commands![
        runtimes_list,
        session_list,
        session_create,
        session_search,
        session_rename,
        session_delete,
        session_send,
        session_interrupt,
        session_pause,
        session_resume,
        session_dispose,
        messages_page,
        permissions_pending,
        permission_resolve,
        session_permission_mode_set,
        rules_list,
        rule_add,
        rule_update,
        rule_delete,
        rule_toggle,
        settings_get,
        settings_set,
        backup_create,
        backup_list,
        backup_restore,
        app_restart,
        run_retry,
        runtime_retry,
        runtime_enable,
        runtime_disable,
        workspace_set,
        workspaces_list,
        workspace_create,
        workspace_update,
        workspace_delete,
        session_workspace_assign,
        ref_pick,
        artifacts_list,
        artifact_add,
        artifact_remove,
        providers_list,
        provider_create,
        provider_update,
        provider_delete,
        provider_toggle,
        provider_model_add,
        provider_model_toggle,
        provider_test,
        provider_models_fetch,
        presets_list,
        preset_add,
        preset_update,
        preset_delete,
        export_diagnostics,
        session_export,
        archive_export,
        audit_query,
        audit_export,
        health,
        startup_get,
        startup_pick_target,
        startup_migrate,
        app_exit,
    ]
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
        session_search,
        session_rename,
        session_delete,
        session_send,
        session_interrupt,
        session_pause,
        session_resume,
        session_dispose,
        messages_page,
        permissions_pending,
        permission_resolve,
        session_permission_mode_set,
        rules_list,
        rule_add,
        rule_update,
        rule_delete,
        rule_toggle,
        settings_get,
        settings_set,
        backup_create,
        backup_list,
        backup_restore,
        app_restart,
        run_retry,
        runtime_retry,
        runtime_enable,
        runtime_disable,
        workspace_set,
        workspaces_list,
        workspace_create,
        workspace_update,
        workspace_delete,
        session_workspace_assign,
        ref_pick,
        artifacts_list,
        artifact_add,
        artifact_remove,
        providers_list,
        provider_create,
        provider_update,
        provider_delete,
        provider_toggle,
        provider_model_add,
        provider_model_toggle,
        provider_test,
        provider_models_fetch,
        presets_list,
        preset_add,
        preset_update,
        preset_delete,
        export_diagnostics,
        session_export,
        archive_export,
        audit_query,
        audit_export,
        health,
        startup_get,
        startup_pick_target,
        startup_migrate,
        app_exit,
        crate::probe::e2e_probe_report,
        crate::startup_probe::e2e_startup_report,
        crate::health_probe::e2e_health_report,
        crate::m3_06_probe::e2e_m3_06_report,
        crate::m4_05_probe::e2e_m4_05_report,
    ]
}

#[cfg(not(debug_assertions))]
pub fn handler<R: tauri::Runtime>() -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static
{
    tauri::generate_handler![
        runtimes_list,
        session_list,
        session_create,
        session_search,
        session_rename,
        session_delete,
        session_send,
        session_interrupt,
        session_pause,
        session_resume,
        session_dispose,
        messages_page,
        permissions_pending,
        permission_resolve,
        session_permission_mode_set,
        rules_list,
        rule_add,
        rule_update,
        rule_delete,
        rule_toggle,
        settings_get,
        settings_set,
        backup_create,
        backup_list,
        backup_restore,
        app_restart,
        run_retry,
        runtime_retry,
        runtime_enable,
        runtime_disable,
        workspace_set,
        workspaces_list,
        workspace_create,
        workspace_update,
        workspace_delete,
        session_workspace_assign,
        ref_pick,
        artifacts_list,
        artifact_add,
        artifact_remove,
        providers_list,
        provider_create,
        provider_update,
        provider_delete,
        provider_toggle,
        provider_model_add,
        provider_model_toggle,
        provider_test,
        provider_models_fetch,
        presets_list,
        preset_add,
        preset_update,
        preset_delete,
        export_diagnostics,
        session_export,
        archive_export,
        audit_query,
        audit_export,
        health,
        startup_get,
        startup_pick_target,
        startup_migrate,
        app_exit,
    ]
}
