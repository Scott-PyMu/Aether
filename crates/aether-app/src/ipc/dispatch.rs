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
    // M10-01/ADR-031：编排命令面 +11（M9 的 63 条顺序/语义不变）。
    "workflow_create",
    "workflow_list",
    "workflow_get",
    "workflow_update",
    "workflow_delete",
    "workflow_import",
    "workflow_run",
    "workflow_run_get",
    "workflow_run_list",
    "workflow_run_cancel",
    "workflow_node_retry",
    // M10-02/ADR-032：子 Agent 命令面 +5（M9/ M10-01 顺序/语义不变）。
    "subagent_spawn",
    "subagent_list",
    "subagent_cancel",
    "subagent_resolve",
    "subagent_result_get",
    // M10-04/ADR-034：导入命令面 +4。
    "session_import",
    "archive_import",
    "import_status",
    "import_cancel",
];

/// 维护窗口允许的命令（M10-04/ADR-034 D-034-2/D-034-4）。
///
/// 导入活动期间 **所有变更类命令** 拒绝（`storage_backpressure`），只读命令与
/// 生命周期/导入控制命令保留；`backup_restore` 由导入后端返回更精确的
/// `import_job_not_active`。
pub const MAINTENANCE_ALLOWED_COMMANDS: &[&str] = &[
    "health",
    "runtimes_list",
    "session_list",
    "session_search",
    "messages_page",
    "permissions_pending",
    "rules_list",
    "settings_get",
    "backup_list",
    "workspaces_list",
    "artifacts_list",
    "providers_list",
    "presets_list",
    "audit_query",
    "session_export",
    "archive_export",
    "audit_export",
    "export_diagnostics",
    "startup_get",
    "startup_pick_target",
    "ref_pick",
    "app_exit",
    "app_restart",
    "workflow_list",
    "workflow_get",
    "workflow_run_get",
    "workflow_run_list",
    "subagent_list",
    "subagent_result_get",
    "session_import",
    "archive_import",
    "import_status",
    "import_cancel",
    "backup_restore",
];

/// 命令路由：`method` + 内层 `params` → 结果 JSON。
///
/// 同步接口（DoD-2 口径）：阻塞动作（系统选择器/迁移复制）由壳层自行放到阻塞线程池
/// 执行（Tauri：`spawn_blocking`；daemon：请求串行线程），命令语义不受影响。
pub fn dispatch(state: &IpcState, method: &str, params: Value) -> Result<Value, IpcError> {
    if let Some(reason) = state.maintenance_reason() {
        if !MAINTENANCE_ALLOWED_COMMANDS.contains(&method) {
            return Err(
                IpcError::core_not_ready(format!("导入维护中：{reason}；当前命令为只读窗口外操作，已拒绝"))
                    .with_business_code(super::business::BusinessCode::StorageBackpressure),
            );
        }
    }
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
        "workflow_create" => handlers::workflow_create(state, params),
        "workflow_list" => handlers::workflow_list(state, params),
        "workflow_get" => handlers::workflow_get(state, params),
        "workflow_update" => handlers::workflow_update(state, params),
        "workflow_delete" => handlers::workflow_delete(state, params),
        "workflow_import" => handlers::workflow_import(state, params),
        "workflow_run" => handlers::workflow_run(state, params),
        "workflow_run_get" => handlers::workflow_run_get(state, params),
        "workflow_run_list" => handlers::workflow_run_list(state, params),
        "workflow_run_cancel" => handlers::workflow_run_cancel(state, params),
        "workflow_node_retry" => handlers::workflow_node_retry(state, params),
        "subagent_spawn" => handlers::subagent_spawn(state, params),
        "subagent_list" => handlers::subagent_list(state, params),
        "subagent_cancel" => handlers::subagent_cancel(state, params),
        "subagent_resolve" => handlers::subagent_resolve(state, params),
        "subagent_result_get" => handlers::subagent_result_get(state, params),
        "session_import" => handlers::session_import(state, params),
        "archive_import" => handlers::archive_import(state, params),
        "import_status" => handlers::import_status(state, params),
        "import_cancel" => handlers::import_cancel(state, params),
        other => return Err(unknown_method(other)),
    }?;
    Ok(payload.into_value())
}

/// 未知方法错误（M9-03 的 JSON-RPC 层据此映射 `method_not_found`）。
pub fn unknown_method(method: &str) -> IpcError {
    IpcError::not_implemented(method)
}

/// 壳本地命令（4 条；ADR-029 §4.6）：由 Electron main 直接处理，**不进入** daemon。
///
/// 计数口径冻结为 **63 = 59 daemon + 4 shell**；`dialog.pick` 不用于产品路径
/// （M9 仅协议预留，见 ADR-029 §3.4）。
pub const SHELL_COMMANDS: &[&str] = &["app_exit", "app_restart", "ref_pick", "startup_pick_target"];

/// 命令是否由壳本地处理（Electron main：生命周期/原生对话框）。
pub fn is_shell_local(method: &str) -> bool {
    SHELL_COMMANDS.contains(&method)
}

/// daemon 侧 59 条命令（`COMMANDS` 去掉 4 条壳本地命令，顺序不变）。
///
/// M9-03 的 `aetherd` 只接受这 59 条；壳本地命令到达 daemon 时按
/// `method_not_found` 拒绝（路由错误，不得静默成功）。
pub fn daemon_commands() -> Vec<&'static str> {
    COMMANDS
        .iter()
        .copied()
        .filter(|method| !is_shell_local(method))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_and_daemon_routes_partition_the_frozen_command_surface() {
        assert_eq!(SHELL_COMMANDS.len(), 4, "壳本地命令 4 条");
        assert_eq!(
            COMMANDS.len(),
            83,
            "命令面 83 条（M9 63 + M10-01 11 + M10-02 5 + M10-04 4）"
        );
        let daemon = daemon_commands();
        assert_eq!(daemon.len(), 79, "daemon 路由 79 条");
        for method in SHELL_COMMANDS {
            assert!(COMMANDS.contains(method), "壳命令 {method} 必须在 63 内");
            assert!(is_shell_local(method));
            assert!(!daemon.contains(method));
        }
        for method in &daemon {
            assert!(!is_shell_local(method));
        }
        // 保留命令仍属 daemon 路由（M9 不暴露但可经 RPC 层验收）。
        for reserved in ["startup_get", "startup_migrate"] {
            assert!(daemon.contains(&reserved), "{reserved} 属 daemon 路由");
        }
    }
}
