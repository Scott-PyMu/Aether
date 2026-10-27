//! D7 命令面路由（ADR-029 D-029-4）：**唯一**命令实现入口。
//!
//! `dispatch(method, params)` 把「方法名 + 内层 JSON 参数」映射到 [`super::handlers`] 的
//! 业务实现；[`COMMANDS`] 为 63 条命令的冻结清单（M9-04 据此生成
//! `packages/protocol/src/command-manifest.json`，CI 校验 63 = 59 daemon + 4 shell）。
//!
//! 口径：
//! - 无参数命令：`params` 接受 `null` / `{}` / 缺省（壳层把缺省归一为 `Value::Null`），
//!   任何成员都被严格模式拒绝（`unknown_field`）；
//! - 有参数命令：内层 DTO 严格解析（`deny_unknown_fields`），失败返回结构化 [`IpcError`]，
//!   不调用后端、不落库；
//! - 未知方法：`not_implemented`（M9-03 的 JSON-RPC 层映射为 `method_not_found`）。

use serde_json::Value;

use super::error::IpcError;
use super::handlers;
use super::IpcState;

/// 63 条命令清单（顺序与迁移期 `collect_commands!` 一致）。
///
/// 分组口径（ADR-029 §4.6）：**63 = 59 daemon + 4 shell**
/// （shell = `app_exit` / `app_restart` / `ref_pick` / `startup_pick_target`）；
/// `startup_get` / `startup_migrate` 保留在 63 内但 UI 不暴露（reserved）；
/// debug 探针命令不计入本清单。
pub const COMMANDS: &[&str] = &[
    "runtimes_list",
    "session_list",
    "session_create",
    "session_search",
    "session_rename",
    "session_delete",
    "session_send",
    "session_interrupt",
    "session_pause",
    "session_resume",
    "session_dispose",
    "messages_page",
    "permissions_pending",
    "permission_resolve",
    "session_permission_mode_set",
    "rules_list",
    "rule_add",
    "rule_update",
    "rule_delete",
    "rule_toggle",
    "settings_get",
    "settings_set",
    "backup_create",
    "backup_list",
    "backup_restore",
    "app_restart",
    "run_retry",
    "runtime_retry",
    "runtime_enable",
    "runtime_disable",
    "workspace_set",
    "workspaces_list",
    "workspace_create",
    "workspace_update",
    "workspace_delete",
    "session_workspace_assign",
    "ref_pick",
    "artifacts_list",
    "artifact_add",
    "artifact_remove",
    "providers_list",
    "provider_create",
    "provider_update",
    "provider_delete",
    "provider_toggle",
    "provider_model_add",
    "provider_model_toggle",
    "provider_test",
    "provider_models_fetch",
    "presets_list",
    "preset_add",
    "preset_update",
    "preset_delete",
    "export_diagnostics",
    "session_export",
    "archive_export",
    "audit_query",
    "audit_export",
    "health",
    "startup_get",
    "startup_pick_target",
    "startup_migrate",
    "app_exit",
];

/// 命令路由：`method` + 内层 `params` → 结果 JSON。
///
/// 同步接口（DoD-2 口径）：阻塞动作（系统选择器/迁移复制）由壳层自行放到阻塞线程池
/// 执行（Tauri：`spawn_blocking`；daemon：请求串行线程），命令语义不受影响。
pub fn dispatch(state: &IpcState, method: &str, params: Value) -> Result<Value, IpcError> {
    let payload = match method {
        "runtimes_list" => handlers::runtimes_list(state),
        "session_list" => handlers::session_list(state, params),
        "session_create" => handlers::session_create(state, params),
        "session_search" => handlers::session_search(state, params),
        "session_rename" => handlers::session_rename(state, params),
        "session_delete" => handlers::session_delete(state, params),
        "session_send" => handlers::session_send(state, params),
        "session_interrupt" => handlers::session_interrupt(state, params),
        "session_pause" => handlers::session_pause(state, params),
        "session_resume" => handlers::session_resume(state, params),
        "session_dispose" => handlers::session_dispose(state, params),
        "messages_page" => handlers::messages_page(state, params),
        "permissions_pending" => handlers::permissions_pending(state, params),
        "permission_resolve" => handlers::permission_resolve(state, params),
        "session_permission_mode_set" => handlers::session_permission_mode_set(state, params),
        "rules_list" => handlers::rules_list(state, params),
        "rule_add" => handlers::rule_add(state, params),
        "rule_update" => handlers::rule_update(state, params),
        "rule_delete" => handlers::rule_delete(state, params),
        "rule_toggle" => handlers::rule_toggle(state, params),
        "settings_get" => handlers::settings_get(state, params),
        "settings_set" => handlers::settings_set(state, params),
        "backup_create" => handlers::backup_create(state, params),
        "backup_list" => handlers::backup_list(state, params),
        "backup_restore" => handlers::backup_restore(state, params),
        "app_restart" => handlers::app_restart(state, params),
        "run_retry" => handlers::run_retry(state, params),
        "runtime_retry" => handlers::runtime_retry(state, params),
        "runtime_enable" => handlers::runtime_enable(state, params),
        "runtime_disable" => handlers::runtime_disable(state, params),
        "workspace_set" => handlers::workspace_set(state, params),
        "workspaces_list" => handlers::workspaces_list(state, params),
        "workspace_create" => handlers::workspace_create(state, params),
        "workspace_update" => handlers::workspace_update(state, params),
        "workspace_delete" => handlers::workspace_delete(state, params),
        "session_workspace_assign" => handlers::session_workspace_assign(state, params),
        "ref_pick" => handlers::ref_pick(state, params),
        "artifacts_list" => handlers::artifacts_list(state, params),
        "artifact_add" => handlers::artifact_add(state, params),
        "artifact_remove" => handlers::artifact_remove(state, params),
        "providers_list" => handlers::providers_list(state, params),
        "provider_create" => handlers::provider_create(state, params),
        "provider_update" => handlers::provider_update(state, params),
        "provider_delete" => handlers::provider_delete(state, params),
        "provider_toggle" => handlers::provider_toggle(state, params),
        "provider_model_add" => handlers::provider_model_add(state, params),
        "provider_model_toggle" => handlers::provider_model_toggle(state, params),
        "provider_test" => handlers::provider_test(state, params),
        "provider_models_fetch" => handlers::provider_models_fetch(state, params),
        "presets_list" => handlers::presets_list(state, params),
        "preset_add" => handlers::preset_add(state, params),
        "preset_update" => handlers::preset_update(state, params),
        "preset_delete" => handlers::preset_delete(state, params),
        "export_diagnostics" => handlers::export_diagnostics(state, params),
        "session_export" => handlers::session_export(state, params),
        "archive_export" => handlers::archive_export(state, params),
        "audit_query" => handlers::audit_query(state, params),
        "audit_export" => handlers::audit_export(state, params),
        "health" => handlers::health(state, params),
        "startup_get" => handlers::startup_get(state, params),
        "startup_pick_target" => handlers::startup_pick_target(state, params),
        "startup_migrate" => handlers::startup_migrate(state, params),
        "app_exit" => handlers::app_exit(state, params),
        other => return Err(unknown_method(other)),
    }?;
    Ok(payload.into_value())
}

/// 未知方法错误（M9-03 的 JSON-RPC 层据此映射 `method_not_found`）。
pub fn unknown_method(method: &str) -> IpcError {
    IpcError::not_implemented(method)
}
