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
    PresetAddRequest, PresetDeleteRequest, PresetUpdateRequest, PresetsListRequest,
    ProviderCreateRequest, ProviderDeleteRequest, ProviderModelAddRequest, ProviderModelToggleRequest,
    ProviderModelsFetchRequest, ProviderTestRequest, ProviderToggleRequest, ProviderUpdateRequest,
    ProvidersListRequest, RefPickKind, RefPickRequest,
    RuleAddRequest, RuleDeleteRequest, RuleToggleRequest, RuleUpdateRequest, RulesListRequest,
    RunRetryRequest, RuntimeDisableRequest, RuntimeEnableRequest, RuntimeRetryRequest,
    SessionCreateRequest, SessionExportRequest, SessionIdRequest, SessionListRequest,
    SessionPermissionModeSetRequest, SessionSendRequest, SessionWorkspaceAssignRequest,
    SessionDeleteRequest, SessionRenameRequest, SessionSearchRequest,
    SettingsGetRequest, SettingsSetRequest,
    StartupGetRequest,
    StartupMigrateRequest, StartupPickTargetRequest, WorkspaceCreateRequest,
    WorkspaceDeleteRequest, WorkspaceSetRequest, WorkspaceUpdateRequest, WorkspacesListRequest,
};
use super::error::{IpcError, IpcErrorCode};
use super::validate::{parse_no_params, parse_strict};
use super::IpcState;
use crate::json_payload::JsonPayload;

pub(crate) fn runtimes_list(state: &IpcState) -> Result<JsonPayload, IpcError> {
    state.backend_ready()?.runtimes_list().map(JsonPayload)
}

pub(crate) fn session_list(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionListRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .session_list(&request)
        .map(JsonPayload)
}

pub(crate) fn session_create(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionCreateRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .session_create(&request)
        .map(JsonPayload)
}

/// M7-07/ADR-025 §2.1-2：会话全文检索（标题 + 消息正文；分页上限 100）。
pub(crate) fn session_search(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionSearchRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .session_search(&request)
        .map(JsonPayload)
}

/// M7-07/ADR-025 §2.1-3：会话改名（单写队列；`session.updated` + 审计）。
pub(crate) fn session_rename(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionRenameRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .session_rename(&request)
        .map(JsonPayload)
}

/// M7-07/ADR-025 §2.1-4：硬删除会话（在途/待审批 → `session_busy`；`events` 不级联）。
pub(crate) fn session_delete(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionDeleteRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .session_delete(&request)
        .map(JsonPayload)
}

pub(crate) fn session_send(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionSendRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .session_send(&request)
        .map(JsonPayload)
}

pub(crate) fn session_interrupt(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionIdRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .session_interrupt(&request)
        .map(JsonPayload)
}

/// M7-03/ADR-017：暂停会话（`pause_resume` 能力门在命令层实时复核——未声明 →
/// `pause_unsupported`，不产生假暂停；回执 `{ session_id, status: "paused" }`）。
pub(crate) fn session_pause(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionIdRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .session_pause(&request)
        .map(JsonPayload)
}

/// M7-03/ADR-017：恢复会话（有暂停标记 → 重放并 `paused → running`；
/// 无标记 → `paused → idle`）。
pub(crate) fn session_resume(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionIdRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .session_resume(&request)
        .map(JsonPayload)
}

pub(crate) fn session_dispose(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionIdRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .session_dispose(&request)
        .map(JsonPayload)
}

pub(crate) fn messages_page(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: MessagesPageRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .messages_page(&request)
        .map(JsonPayload)
}

pub(crate) fn permissions_pending(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: PermissionsPendingRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .permissions_pending(&request)
        .map(JsonPayload)
}

pub(crate) fn permission_resolve(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: PermissionResolveRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .permission_resolve(&request)
        .map(JsonPayload)
}

/// M6-01/ADR-014 决策 3：设置会话级权限模式（档位为判定权威入参；
/// 仅影响后续工具调用决策，在途会话沙箱参数不变）。
pub(crate) fn session_permission_mode_set(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionPermissionModeSetRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .session_permission_mode_set(&request)
        .map(JsonPayload)
}

/// M6-01/ADR-019：用户自定义权限规则清单（无参数；严格解析拒绝未知成员）。
pub(crate) fn rules_list(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let _request: RulesListRequest =
        parse_no_params(payload)?;
    state.backend_ready()?.rules_list().map(JsonPayload)
}

/// M6-01/ADR-019：新增权限规则（非法规则拒绝且不落库）。
pub(crate) fn rule_add(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: RuleAddRequest = parse_strict(payload)?;
    state.backend_ready()?.rule_add(&request).map(JsonPayload)
}

/// M6-01/ADR-019：更新权限规则（`rule_id` 不存在 → `rule_not_found`）。
pub(crate) fn rule_update(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: RuleUpdateRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .rule_update(&request)
        .map(JsonPayload)
}

/// M6-01/ADR-019：删除权限规则（`rule_id` 不存在 → `rule_not_found`）。
pub(crate) fn rule_delete(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: RuleDeleteRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .rule_delete(&request)
        .map(JsonPayload)
}

/// M6-01/ADR-019：启用/停用权限规则（`enabled` 非布尔由 serde 严格解析拒绝）。
pub(crate) fn rule_toggle(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: RuleToggleRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .rule_toggle(&request)
        .map(JsonPayload)
}

pub(crate) fn settings_get(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SettingsGetRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .settings_get(&request)
        .map(JsonPayload)
}

pub(crate) fn settings_set(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SettingsSetRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .settings_set(&request)
        .map(JsonPayload)
}

/// M3-04/D13：手动备份（缺省 = 应用 `backups` 目录；`target_dir` = 系统选择器选中的
/// 外部目录，canonicalize 后进入后端执行空间护栏）。
pub(crate) fn backup_create(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: BackupCreateRequest = parse_strict(payload)?;
    let canonical = request.canonical_target_dir()?;
    state
        .backend_ready()?
        .backup_create(&request, canonical.as_deref())
        .map(JsonPayload)
}

/// ADR-004：无参数命令；缺省载荷等价空对象，任何成员都会被严格模式拒绝。
pub(crate) fn backup_list(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let _request: BackupListRequest =
        parse_no_params(payload)?;
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
pub(crate) fn health(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let _request: HealthRequest = parse_no_params(payload)?;
    state.backend_ready()?.health().map(JsonPayload)
}

/// ADR-004/D13：外部候选先 canonicalize（存在性 + `.db` 后缀）再进入恢复七步。
///
/// M3-04：命令层在候选校验通过后请求应用重启（`restart_required`），第 3–6 步由下次
/// 启动序列在无写者窗口执行（现场日志恢复；D13「恢复流程含核心重启」）。
pub(crate) fn backup_restore(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: BackupRestoreRequest = parse_strict(payload)?;
    let canonical = request.canonical_external_path()?;
    let value = state
        .backend_ready()?
        .backup_restore(&request, canonical.as_deref())?;
    if value
        .get("restart_required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        if let Some(shell) = state.shell() {
            shell.request_restart();
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
pub(crate) fn app_restart(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let _request: AppRestartRequest = parse_strict(payload)?;
    let shell = state.shell().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "壳层宿主未接线（仅生产运行形态；框架测试不含壳层）",
        )
    })?;
    shell.request_restart();
    Ok(JsonPayload(serde_json::json!({ "restarting": true })))
}

/// ADR-004/M3-06：仅终态 run 可重试（`run_id` ULID；状态由后端判定）。
pub(crate) fn run_retry(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: RunRetryRequest = parse_strict(payload)?;
    state.backend_ready()?.run_retry(&request).map(JsonPayload)
}

/// ADR-004/M1-10：仅 `disabled + start_failed` 可用（白名单与状态由后端判定）。
pub(crate) fn runtime_retry(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: RuntimeRetryRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .runtime_retry(&request)
        .map(JsonPayload)
}

/// ADR-004/M1-10：仅 `disabled` 可用；`untrusted`/`version_mismatch` 需先修复。
pub(crate) fn runtime_enable(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: RuntimeEnableRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .runtime_enable(&request)
        .map(JsonPayload)
}

/// M7-01/ADR-015：运行期禁用官方适配器（存在在途会话且未确认 → `runtime_busy`；
/// 确认后偏好先写 → `disabled + user_disabled` → 受控终止）。
pub(crate) fn runtime_disable(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: RuntimeDisableRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .runtime_disable(&request)
        .map(JsonPayload)
}

/// ADR-004/D14：`workspace_id` 或 `root_path`（canonicalize + 同步盘拒绝）。
pub(crate) fn workspace_set(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: WorkspaceSetRequest = parse_strict(payload)?;
    let canonical = request.canonical_root_path()?;
    state
        .backend_ready()?
        .workspace_set(&request, canonical.as_deref())
        .map(JsonPayload)
}

/// M7-08/ADR-026 §2.1-1：工作区树清单（无参数；工作区 + 会话计数 + 合成组
/// 「默认（数据目录）」）。
pub(crate) fn workspaces_list(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: WorkspacesListRequest =
        parse_no_params(payload)?;
    state
        .backend_ready()?
        .workspaces_list(&request)
        .map(JsonPayload)
}

/// M7-08/ADR-026 §2.1-1：新建工作区（名称 ≤64；root canonicalize + 存在目录 +
/// 同步盘拒绝；同根路径已存在 → `invalid_value`）。
pub(crate) fn workspace_create(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: WorkspaceCreateRequest = parse_strict(payload)?;
    let canonical = request.canonical_root_path()?;
    state
        .backend_ready()?
        .workspace_create(&request, &canonical)
        .map(JsonPayload)
}

/// M7-08/ADR-026 §2.1-1：工作区改名（仅名称；`root_path` 不可变）。
pub(crate) fn workspace_update(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: WorkspaceUpdateRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .workspace_update(&request)
        .map(JsonPayload)
}

/// M7-08/ADR-026 §2.1-1：删除工作区（存在会话引用 → `workspace_in_use`；只删行，
/// 不删磁盘文件）。
pub(crate) fn workspace_delete(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: WorkspaceDeleteRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .workspace_delete(&request)
        .map(JsonPayload)
}

/// M7-08/ADR-026 §2.1-1：会话改派工作区（在途 → `session_busy`；记忆注入源切换；
/// 权限基准不重算——R15/不变量 11）。
pub(crate) fn session_workspace_assign(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionWorkspaceAssignRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .session_workspace_assign(&request)
        .map(JsonPayload)
}

pub(crate) fn export_diagnostics(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ExportDiagnosticsRequest = parse_strict(payload)?;
    let canonical = request.canonical_target_dir()?;
    state
        .backend_ready()?
        .export_diagnostics(&request, &canonical)
        .map(JsonPayload)
}

/// M5-05/ADR-018：单会话 JSONL 导出（缺省 = 应用 `archives/` 目录；
/// `target_dir` = 系统选择器选中的外部目录，canonicalize 后进入后端空间护栏）。
pub(crate) fn session_export(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: SessionExportRequest = parse_strict(payload)?;
    let canonical = request.canonical_target_dir()?;
    state
        .backend_ready()?
        .session_export(&request, canonical.as_deref())
        .map(JsonPayload)
}

/// M5-05/ADR-018：全库归档导出（容量「一键归档」；锚点 `archive`）。
///
/// 无参数调用（缺省载荷 / `{}`）合法；`target_dir` 可选（系统选择器选中的外部目录）。
pub(crate) fn archive_export(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ArchiveExportRequest =
        parse_no_params(payload)?;
    let canonical = request.canonical_target_dir()?;
    state
        .backend_ready()?
        .archive_export(&request, canonical.as_deref())
        .map(JsonPayload)
}

/// M6-03/ADR-023：审计查询（过滤：会话/时间/actor/action/resource；分页；
/// `ts DESC, id DESC` 稳定排序）。缺省载荷（`null` / `{}`）= 空过滤（最近 100 条）。
pub(crate) fn audit_query(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: AuditQueryRequest =
        parse_no_params(payload)?;
    state
        .backend_ready()?
        .audit_query(&request)
        .map(JsonPayload)
}

/// M6-03/ADR-023：审计导出（过滤同 `audit_query`；缺省写入数据目录 `archives/`，
/// 外部目录经系统选择器 canonicalize 后进入空间护栏；原子写不留半成品）。
pub(crate) fn audit_export(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: AuditExportRequest =
        parse_no_params(payload)?;
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
pub(crate) fn ref_pick(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: RefPickRequest = parse_strict(payload)?;
    let picker = state.picker().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "目录/文件选择器未接线（仅生产运行形态；测试需注入 DirectoryPicker）",
        )
    })?;
    let kind = request.kind;
    // 壳层负责把阻塞式选择器放到阻塞线程池（Tauri：`spawn_blocking`；daemon：直接调用）。
    let picked = match kind {
        RefPickKind::File => picker.pick_file(),
        RefPickKind::Directory => picker.pick_directory(),
    }
    .map_err(|error| IpcError::internal(format!("引用选择器错误：{error}")))?;
    let path = picked.map(|path| path.to_string_lossy().to_string());
    Ok(JsonPayload(serde_json::json!({ "path": path })))
}

/// ADR-010/M3-09：会话引用清单（只读；不触发 `workspace_set`、不产生事件）。
pub(crate) fn artifacts_list(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ArtifactsListRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .artifacts_list(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-09：登记会话引用（canonicalize + 可访问性检查在命令层；失败
/// `artifact_path_rejected`；同会话同路径幂等）。
pub(crate) fn artifact_add(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ArtifactAddRequest = parse_strict(payload)?;
    let resolved = request.resolve_path()?;
    state
        .backend_ready()?
        .artifact_add(&request, &resolved)
        .map(JsonPayload)
}

/// ADR-010/M3-09：删除会话引用（不存在 → 幂等 `{ removed: false }`）。
pub(crate) fn artifact_remove(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ArtifactRemoveRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .artifact_remove(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：供应商与模型清单（无参数；非空成员一律拒绝）。
pub(crate) fn providers_list(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let _request: ProvidersListRequest =
        parse_no_params(payload)?;
    state
        .backend_ready()?
        .providers_list(&_request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：新建供应商（`api_key` 明文仅传输 → 核心写 keyring → `api_key_ref`）。
pub(crate) fn provider_create(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderCreateRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .provider_create(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：整体更新供应商（`type` 不可改；`api_key` 三态）。
pub(crate) fn provider_update(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderUpdateRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .provider_update(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：删除供应商（内置硬拒绝 `builtin_provider_undeletable`）。
pub(crate) fn provider_delete(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderDeleteRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .provider_delete(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：快速启用/停用供应商（内置可停用）。
pub(crate) fn provider_toggle(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderToggleRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .provider_toggle(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：新增供应商模型（默认启用；重复 `(provider_id, model_id)` →
/// `invalid_value`）。
pub(crate) fn provider_model_add(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderModelAddRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .provider_model_add(&request)
        .map(JsonPayload)
}

/// ADR-010/M3-11：模型启用/停用（不存在 → `provider_model_not_found`）。
pub(crate) fn provider_model_toggle(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderModelToggleRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .provider_model_toggle(&request)
        .map(JsonPayload)
}

/// M7-06/ADR-024：供应商连通性测试（真实网络请求；五路径错误映射；无密钥不发请求）。
pub(crate) fn provider_test(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderTestRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .provider_test(&request)
        .map(JsonPayload)
}

/// M7-06/ADR-024：从供应商获取模型列表（归一化 + 幂等 upsert）。
pub(crate) fn provider_models_fetch(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: ProviderModelsFetchRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .provider_models_fetch(&request)
        .map(JsonPayload)
}

/// M7-09/ADR-026：Agent 预设清单（无参数；严格解析拒绝未知成员）。
pub(crate) fn presets_list(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: PresetsListRequest =
        parse_no_params(payload)?;
    state
        .backend_ready()?
        .presets_list(&request)
        .map(JsonPayload)
}

/// M7-09/ADR-026：新增自定义预设（名称 ≤64 字符、提示词 ≤32KiB）。
pub(crate) fn preset_add(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: PresetAddRequest = parse_strict(payload)?;
    state.backend_ready()?.preset_add(&request).map(JsonPayload)
}

/// M7-09/ADR-026：更新预设（内置可编辑；`preset_id` 不存在 → `preset_not_found`）。
pub(crate) fn preset_update(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: PresetUpdateRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .preset_update(&request)
        .map(JsonPayload)
}

/// M7-09/ADR-026：删除预设（内置 → `builtin_preset_undeletable`；硬约束）。
pub(crate) fn preset_delete(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: PresetDeleteRequest = parse_strict(payload)?;
    state
        .backend_ready()?
        .preset_delete(&request)
        .map(JsonPayload)
}

/// M1-06：启动门快照（拒绝启动时仍可达；UI 据此渲染门界面）。
/// ADR-006：无参数命令；缺省载荷等价空对象，任何成员都会被严格模式拒绝。
pub(crate) fn startup_get(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let _request: StartupGetRequest =
        parse_no_params(payload)?;
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
pub(crate) fn startup_pick_target(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let _request: StartupPickTargetRequest = parse_no_params(payload)?;
    let picker = state.picker().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "目录选择器未接线（仅生产运行形态；测试需注入 DirectoryPicker）",
        )
    })?;
    let picked = picker
        .pick_directory()
        .map_err(|error| IpcError::internal(format!("目录选择器错误：{error}")))?;
    let target_dir = picked.map(|path| path.to_string_lossy().to_string());
    Ok(JsonPayload(serde_json::json!({ "target_dir": target_dir })))
}

/// M1-06/A4：执行迁移（复制 → sha256 校验 → 原子替换 → 指针锁定新目录）。
pub(crate) fn startup_migrate(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let request: StartupMigrateRequest = parse_strict(payload)?;
    let gate = state.startup_arc().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "启动门未接线（仅生产运行形态；框架测试不含启动门）",
        )
    })?;
    let target = request.target_dir.clone();
    gate.migrate(&target).map(JsonPayload)
}

/// M1-06：退出应用（拒绝启动界面「退出」按钮；不暴露窗口控制能力）。
/// ADR-006（v0.3 对齐）：`{ confirm: true }` 显式确认；严格解析拒绝未知成员。
pub(crate) fn app_exit(
    state: &IpcState,
    payload: Value,
) -> Result<JsonPayload, IpcError> {
    let _request: AppExitRequest = parse_strict(payload)?;
    let shell = state.shell().ok_or_else(|| {
        IpcError::new(
            IpcErrorCode::NotImplemented,
            "壳层宿主未接线（仅生产运行形态）",
        )
    })?;
    shell.exit(0);
    Ok(JsonPayload(serde_json::json!({ "exiting": true })))
}

