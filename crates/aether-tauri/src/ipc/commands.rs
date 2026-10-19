//! D7 命令面（MVP 全集）：每个命令都走「严格解析 → 语义校验 → 后端」，
//! 校验失败返回结构化错误且不调用后端（不落库、不透传下游）。
//!
//! M1-08 只建立框架与命令面；真实实现由 [`crate::ipc::IpcBackend`] 的后续实现提供。
//! 单测矩阵见 `tests/ipc_validation.rs`。

use serde_json::Value;

use super::dto::{
    AppExitRequest, AppRestartRequest, ArchiveExportRequest, ArtifactAddRequest,
    ArtifactRemoveRequest, ArtifactsListRequest, AuditExportRequest, AuditQueryRequest,
    BackupCreateRequest, BackupListRequest, BackupRestoreRequest, ExportDiagnosticsRequest,
    HealthRequest, MessagesPageRequest, PermissionResolveRequest, PermissionsPendingRequest,
    ProviderCreateRequest, ProviderDeleteRequest, ProviderModelAddRequest, ProviderModelToggleRequest,
    ProviderModelsFetchRequest, ProviderTestRequest, ProviderToggleRequest, ProviderUpdateRequest,
    ProvidersListRequest, RefPickKind, RefPickRequest,
    RuleAddRequest, RuleDeleteRequest, RuleToggleRequest, RuleUpdateRequest, RulesListRequest,
    RunRetryRequest, RuntimeDisableRequest, RuntimeEnableRequest, RuntimeRetryRequest,
    SessionCreateRequest, SessionExportRequest, SessionIdRequest, SessionListRequest,
    SessionPermissionModeSetRequest, SessionSendRequest, SettingsGetRequest, SettingsSetRequest,
    StartupGetRequest,
    StartupMigrateRequest, StartupPickTargetRequest, WorkspaceSetRequest,
};
use super::error::{IpcError, IpcErrorCode};
use super::validate::{parse_no_params, parse_strict};
use super::IpcState;
use crate::json_payload::JsonPayload;

#[tauri::command]
#[specta::specta]
pub(crate) fn runtimes_list(state: tauri::State<'_, IpcState>) -> Result<JsonPayload, IpcError> {
    state.backend_ready()?.runtimes_list().map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_list(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: SessionListRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .session_list(&request)
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_create(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: SessionCreateRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .session_create(&request)
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_send(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: SessionSendRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .session_send(&request)
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_interrupt(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: SessionIdRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .session_interrupt(&request)
        .map(JsonPayload)
}

/// M7-03/ADR-017：暂停会话（`pause_resume` 能力门在命令层实时复核——未声明 →
/// `pause_unsupported`，不产生假暂停；回执 `{ session_id, status: "paused" }`）。
#[tauri::command]
#[specta::specta]
pub(crate) fn session_pause(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: SessionIdRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .session_pause(&request)
        .map(JsonPayload)
}

/// M7-03/ADR-017：恢复会话（有暂停标记 → 重放并 `paused → running`；
/// 无标记 → `paused → idle`）。
#[tauri::command]
#[specta::specta]
pub(crate) fn session_resume(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: SessionIdRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .session_resume(&request)
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn session_dispose(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: SessionIdRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .session_dispose(&request)
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn messages_page(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: MessagesPageRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .messages_page(&request)
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn permissions_pending(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: PermissionsPendingRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .permissions_pending(&request)
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn permission_resolve(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: PermissionResolveRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .permission_resolve(&request)
        .map(JsonPayload)
}

/// M6-01/ADR-014 决策 3：设置会话级权限模式（档位为判定权威入参；
/// 仅影响后续工具调用决策，在途会话沙箱参数不变）。
#[tauri::command]
#[specta::specta]
pub(crate) fn session_permission_mode_set(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: SessionPermissionModeSetRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .session_permission_mode_set(&request)
        .map(JsonPayload)
}

/// M6-01/ADR-019：用户自定义权限规则清单（无参数；严格解析拒绝未知成员）。
#[tauri::command]
#[specta::specta]
pub(crate) fn rules_list(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    let _request: RulesListRequest =
        parse_no_params(payload.map(JsonPayload::into_value).unwrap_or(Value::Null))?;
    state.backend_ready()?.rules_list().map(JsonPayload)
}

/// M6-01/ADR-019：新增权限规则（非法规则拒绝且不落库）。
#[tauri::command]
#[specta::specta]
pub(crate) fn rule_add(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: RuleAddRequest = parse_strict(payload.into_value())?;
    state.backend_ready()?.rule_add(&request).map(JsonPayload)
}

/// M6-01/ADR-019：更新权限规则（`rule_id` 不存在 → `rule_not_found`）。
#[tauri::command]
#[specta::specta]
pub(crate) fn rule_update(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: RuleUpdateRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .rule_update(&request)
        .map(JsonPayload)
}

/// M6-01/ADR-019：删除权限规则（`rule_id` 不存在 → `rule_not_found`）。
#[tauri::command]
#[specta::specta]
pub(crate) fn rule_delete(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: RuleDeleteRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .rule_delete(&request)
        .map(JsonPayload)
}

/// M6-01/ADR-019：启用/停用权限规则（`enabled` 非布尔由 serde 严格解析拒绝）。
#[tauri::command]
#[specta::specta]
pub(crate) fn rule_toggle(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: RuleToggleRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .rule_toggle(&request)
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn settings_get(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: SettingsGetRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .settings_get(&request)
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn settings_set(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: SettingsSetRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .settings_set(&request)
        .map(JsonPayload)
}

/// M3-04/D13：手动备份（缺省 = 应用 `backups` 目录；`target_dir` = 系统选择器选中的
/// 外部目录，canonicalize 后进入后端执行空间护栏）。
#[tauri::command]
#[specta::specta]
pub(crate) fn backup_create(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: BackupCreateRequest = parse_strict(payload.into_value())?;
    let canonical = request.canonical_target_dir()?;
    state
        .backend_ready()?
        .backup_create(&request, canonical.as_deref())
        .map(JsonPayload)
}

/// ADR-004：无参数命令；缺省载荷等价空对象，任何成员都会被严格模式拒绝。
#[tauri::command]
#[specta::specta]
pub(crate) fn backup_list(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    let _request: BackupListRequest =
        parse_no_params(payload.map(JsonPayload::into_value).unwrap_or(Value::Null))?;
    state.backend_ready()?.backup_list().map(JsonPayload)
}

/// ADR-007 决策 1：核心健康查询（无参数；严格解析拒绝未知成员）。
///
/// 返回 `HealthReport`（`storage_state` / `write_queue_depth` / `runtimes` 摘要 / `ts`）；
/// 仅本地 IPC，不落库、不产生事件。UI 每 5s 轮询，15s 无响应显示「核心未响应」+ 重启入口。
/// 数据源：`EventPipeline::health()` + 监督器摘要（M2-07 接线）。
///
/// 说明（M2-07）：命令为 async——E2E 探针经 `AETHER_E2E_HEALTH_STALL_MS` 在运行时上
/// 挂起本命令（模拟无响应；不阻塞主线程），生产路径无等待。
#[tauri::command]
#[specta::specta]
pub(crate) async fn health(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    let _request: HealthRequest =
        parse_no_params(payload.map(JsonPayload::into_value).unwrap_or(Value::Null))?;
    #[cfg(debug_assertions)]
    crate::health_probe::maybe_stall_health().await;
    state.backend_ready()?.health().map(JsonPayload)
}

/// ADR-004/D13：外部候选先 canonicalize（存在性 + `.db` 后缀）再进入恢复七步。
///
/// M3-04：命令层在候选校验通过后请求应用重启（`restart_required`），第 3–6 步由下次
/// 启动序列在无写者窗口执行（现场日志恢复；D13「恢复流程含核心重启」）。
#[tauri::command]
#[specta::specta]
pub(crate) fn backup_restore(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: BackupRestoreRequest = parse_strict(payload.into_value())?;
    let canonical = request.canonical_external_path()?;
    let value = state
        .backend_ready()?
        .backup_restore(&request, canonical.as_deref())?;
    if value
        .get("restart_required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        if let Some(handle) = state.app_handle() {
            handle.request_restart();
        }
    }
    Ok(JsonPayload(value))
}

/// ADR-004/M3-06：显式 `confirm:true` 后触发应用重启（与 `app_exit` 同口径的
/// 命令层实现——重启属应用生命周期动作，不经 `IpcBackend` 下游）。
///
/// 机制：`AppHandle::request_restart()` → `RunEvent::ExitRequested` → 既有退出编排
/// （D2 关闭序列：广播 shutdown → 适配器终止段 → 存储五步）→ 进程重启并重跑启动
/// 序列（A4 检测 + `quick_check` + 孤儿清理 + 重启状态重建）。P0 无热恢复（D4）：
/// 存储降级恢复仅经「修复外部条件 + 本入口重启 + 启动自检」。
#[tauri::command]
#[specta::specta]
pub(crate) fn app_restart(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let _request: AppRestartRequest = parse_strict(payload.into_value())?;
    let handle = state.app_handle().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "应用句柄未接线（仅生产运行形态；框架测试不含应用句柄）",
        )
    })?;
    handle.request_restart();
    Ok(JsonPayload(serde_json::json!({ "restarting": true })))
}

/// ADR-004/M3-06：仅终态 run 可重试（`run_id` ULID；状态由后端判定）。
#[tauri::command]
#[specta::specta]
pub(crate) fn run_retry(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: RunRetryRequest = parse_strict(payload.into_value())?;
    state.backend_ready()?.run_retry(&request).map(JsonPayload)
}

/// ADR-004/M1-10：仅 `disabled + start_failed` 可用（白名单与状态由后端判定）。
#[tauri::command]
#[specta::specta]
pub(crate) fn runtime_retry(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: RuntimeRetryRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .runtime_retry(&request)
        .map(JsonPayload)
}

/// ADR-004/M1-10：仅 `disabled` 可用；`untrusted`/`version_mismatch` 需先修复。
#[tauri::command]
#[specta::specta]
pub(crate) fn runtime_enable(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: RuntimeEnableRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .runtime_enable(&request)
        .map(JsonPayload)
}

/// M7-01/ADR-015：运行期禁用官方适配器（存在在途会话且未确认 → `runtime_busy`；
/// 确认后偏好先写 → `disabled + user_disabled` → 受控终止）。
#[tauri::command]
#[specta::specta]
pub(crate) fn runtime_disable(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: RuntimeDisableRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .runtime_disable(&request)
        .map(JsonPayload)
}

/// ADR-004/D14：`workspace_id` 或 `root_path`（canonicalize + 同步盘拒绝）。
#[tauri::command]
#[specta::specta]
pub(crate) fn workspace_set(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: WorkspaceSetRequest = parse_strict(payload.into_value())?;
    let canonical = request.canonical_root_path()?;
    state
        .backend_ready()?
        .workspace_set(&request, canonical.as_deref())
        .map(JsonPayload)
}

#[tauri::command]
#[specta::specta]
pub(crate) fn export_diagnostics(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: ExportDiagnosticsRequest = parse_strict(payload.into_value())?;
    let canonical = request.canonical_target_dir()?;
    state
        .backend_ready()?
        .export_diagnostics(&request, &canonical)
        .map(JsonPayload)
}

/// M5-05/ADR-018：单会话 JSONL 导出（缺省 = 应用 `archives/` 目录；
/// `target_dir` = 系统选择器选中的外部目录，canonicalize 后进入后端空间护栏）。
#[tauri::command]
#[specta::specta]
pub(crate) fn session_export(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: SessionExportRequest = parse_strict(payload.into_value())?;
    let canonical = request.canonical_target_dir()?;
    state
        .backend_ready()?
        .session_export(&request, canonical.as_deref())
        .map(JsonPayload)
}

/// M5-05/ADR-018：全库归档导出（容量「一键归档」；锚点 `archive`）。
///
/// 无参数调用（缺省载荷 / `{}`）合法；`target_dir` 可选（系统选择器选中的外部目录）。
#[tauri::command]
#[specta::specta]
pub(crate) fn archive_export(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    let request: ArchiveExportRequest =
        parse_no_params(payload.map(JsonPayload::into_value).unwrap_or(Value::Null))?;
    let canonical = request.canonical_target_dir()?;
    state
        .backend_ready()?
        .archive_export(&request, canonical.as_deref())
        .map(JsonPayload)
}

/// M6-03/ADR-023：审计查询（过滤：会话/时间/actor/action/resource；分页；
/// `ts DESC, id DESC` 稳定排序）。缺省载荷（`null` / `{}`）= 空过滤（最近 100 条）。
#[tauri::command]
#[specta::specta]
pub(crate) fn audit_query(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    let request: AuditQueryRequest =
        parse_no_params(payload.map(JsonPayload::into_value).unwrap_or(Value::Null))?;
    state
        .backend_ready()?
        .audit_query(&request)
        .map(JsonPayload)
}

/// M6-03/ADR-023：审计导出（过滤同 `audit_query`；缺省写入数据目录 `archives/`，
/// 外部目录经系统选择器 canonicalize 后进入空间护栏；原子写不留半成品）。
#[tauri::command]
#[specta::specta]
pub(crate) fn audit_export(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    let request: AuditExportRequest =
        parse_no_params(payload.map(JsonPayload::into_value).unwrap_or(Value::Null))?;
    let canonical = request.canonical_target_dir()?;
    state
        .backend_ready()?
        .audit_export(&request, canonical.as_deref())
        .map(JsonPayload)
}

/// ADR-010/M3-09：引用选择器（`{ kind: "file" | "directory" }`）。
///
/// Rust 侧系统选择器（复用 `DirectoryPicker` 抽象并扩展文件选择；E2E 注入替身），
/// **不新增 WebView capability 权限面**（与 `startup_pick_target` 先例一致）；
/// 路径原样返回、不做 canonicalize（校验在 `artifact_add`）；用户取消 → `{ path: null }`。
#[tauri::command]
#[specta::specta]
pub(crate) async fn ref_pick(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: RefPickRequest = parse_strict(payload.into_value())?;
    let picker = state.picker().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "目录/文件选择器未接线（仅生产运行形态；测试需注入 DirectoryPicker）",
        )
    })?;
    let kind = request.kind;
    let picked = tauri::async_runtime::spawn_blocking(move || match kind {
        RefPickKind::File => picker.pick_file(),
        RefPickKind::Directory => picker.pick_directory(),
    })
    .await
    .map_err(|error| IpcError::internal(format!("引用选择器调用失败：{error}")))?
    .map_err(|error| IpcError::internal(format!("引用选择器错误：{error}")))?;
    let path = picked.map(|path| path.to_string_lossy().to_string());
    Ok(JsonPayload(serde_json::json!({ "path": path })))
}

/// ADR-010/M3-09：会话引用清单（只读；不触发 `workspace_set`、不产生事件）。
#[tauri::command]
#[specta::specta]
pub(crate) fn artifacts_list(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: ArtifactsListRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .artifacts_list(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-09：登记会话引用（canonicalize + 可访问性检查在命令层；失败
/// `artifact_path_rejected`；同会话同路径幂等）。
#[tauri::command]
#[specta::specta]
pub(crate) fn artifact_add(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: ArtifactAddRequest = parse_strict(payload.into_value())?;
    let resolved = request.resolve_path()?;
    state
        .backend_ready()?
        .artifact_add(&request, &resolved)
        .map(JsonPayload)
}

/// ADR-010/M3-09：删除会话引用（不存在 → 幂等 `{ removed: false }`）。
#[tauri::command]
#[specta::specta]
pub(crate) fn artifact_remove(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: ArtifactRemoveRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .artifact_remove(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：供应商与模型清单（无参数；非空成员一律拒绝）。
#[tauri::command]
#[specta::specta]
pub(crate) fn providers_list(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    let _request: ProvidersListRequest =
        parse_no_params(payload.map(JsonPayload::into_value).unwrap_or(Value::Null))?;
    state
        .backend_ready()?
        .providers_list(&_request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：新建供应商（`api_key` 明文仅传输 → 核心写 keyring → `api_key_ref`）。
#[tauri::command]
#[specta::specta]
pub(crate) fn provider_create(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderCreateRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .provider_create(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：整体更新供应商（`type` 不可改；`api_key` 三态）。
#[tauri::command]
#[specta::specta]
pub(crate) fn provider_update(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderUpdateRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .provider_update(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：删除供应商（内置硬拒绝 `builtin_provider_undeletable`）。
#[tauri::command]
#[specta::specta]
pub(crate) fn provider_delete(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderDeleteRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .provider_delete(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：快速启用/停用供应商（内置可停用）。
#[tauri::command]
#[specta::specta]
pub(crate) fn provider_toggle(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderToggleRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .provider_toggle(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：新增供应商模型（默认启用；重复 `(provider_id, model_id)` →
/// `invalid_value`）。
#[tauri::command]
#[specta::specta]
pub(crate) fn provider_model_add(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderModelAddRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .provider_model_add(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：模型启用/停用（不存在 → `provider_model_not_found`）。
#[tauri::command]
#[specta::specta]
pub(crate) fn provider_model_toggle(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderModelToggleRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .provider_model_toggle(&request)
        .map(JsonPayload)
}

/// M7-06/ADR-024：供应商连通性测试（真实网络请求；五路径错误映射；无密钥不发请求）。
#[tauri::command]
#[specta::specta]
pub(crate) fn provider_test(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderTestRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .provider_test(&request)
        .map(JsonPayload)
}

/// M7-06/ADR-024：从供应商获取模型列表（归一化 + 幂等 upsert）。
#[tauri::command]
#[specta::specta]
pub(crate) fn provider_models_fetch(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderModelsFetchRequest = parse_strict(payload.into_value())?;
    state
        .backend_ready()?
        .provider_models_fetch(&request)
        .map(JsonPayload)
}

/// M1-06：启动门快照（拒绝启动时仍可达；UI 据此渲染门界面）。
/// ADR-006：无参数命令；缺省载荷等价空对象，任何成员都会被严格模式拒绝。
#[tauri::command]
#[specta::specta]
pub(crate) fn startup_get(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    let _request: StartupGetRequest =
        parse_no_params(payload.map(JsonPayload::into_value).unwrap_or(Value::Null))?;
    let gate = state.startup().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "启动门未接线（仅生产运行形态；框架测试不含启动门）",
        )
    })?;
    gate.snapshot_json().map(JsonPayload)
}

/// M1-06：迁移目标目录选择（`DirectoryPicker` 抽象：生产为系统对话框，测试/E2E 注入替身；
/// 不新增 WebView capability 权限面）。用户取消返回 `{ "target_dir": null }`。
/// ADR-006：无参数命令；缺省载荷等价空对象，任何成员都会被严格模式拒绝。
#[tauri::command]
#[specta::specta]
pub(crate) async fn startup_pick_target(
    state: tauri::State<'_, IpcState>,
    payload: Option<JsonPayload>,
) -> Result<JsonPayload, IpcError> {
    let _request: StartupPickTargetRequest =
        parse_no_params(payload.map(JsonPayload::into_value).unwrap_or(Value::Null))?;
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
    Ok(JsonPayload(serde_json::json!({ "target_dir": target_dir })))
}

/// M1-06/A4：执行迁移（复制 → sha256 校验 → 原子替换 → 指针锁定新目录）。
#[tauri::command]
#[specta::specta]
pub(crate) async fn startup_migrate(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let request: StartupMigrateRequest = parse_strict(payload.into_value())?;
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
        .map(JsonPayload)
}

/// M1-06：退出应用（拒绝启动界面「退出」按钮；不暴露窗口控制能力）。
/// ADR-006（v0.3 对齐）：`{ confirm: true }` 显式确认；严格解析拒绝未知成员。
#[tauri::command]
#[specta::specta]
pub(crate) fn app_exit(
    state: tauri::State<'_, IpcState>,
    payload: JsonPayload,
) -> Result<JsonPayload, IpcError> {
    let _request: AppExitRequest = parse_strict(payload.into_value())?;
    let handle = state.app_handle().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "应用句柄未接线（仅生产运行形态）",
        )
    })?;
    handle.exit(0);
    Ok(JsonPayload(serde_json::json!({ "exiting": true })))
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
