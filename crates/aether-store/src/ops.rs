//! 领域写命令与读侧查询（M2-01 生命周期 / M2-03 权限）。
//!
//! 单写者约束（AGENTS §2.4 / 设计 D3）：全部写路径经 [`WriteQueue::execute`] 进入
//! 同一 `mpsc` 写队列，由单写任务串行执行；本模块提供命令的**事务实现**，
//! 不自行打开连接。
//!
//! 幂等（ADR-005）：[`StoreCommand::BeginRunIdempotent`] 在单个事务内完成
//! 「按 `(session_id, client_msg_id)` 查重 → 命中返回既有 id / 未命中插入消息与 run」，
//! 因此重启后重放同值不产生重复行（持久化支撑）。

use std::collections::BTreeMap;
use std::str::FromStr;

use aether_core::{
    Message, MessageId, MessageRole, PermissionDecision, PermissionMode, PermissionScope,
    PermissionStatus, Preset, PresetId, Run, RunId, RunStatus, Runtime, Session, SessionId,
    SessionStatus, Workspace, WorkspaceId,
};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::backup::BackupRecord;
use crate::error::StoreError;
use crate::write_queue::ReadPool;

/// `DeleteEventsByIds` 单条 `DELETE ... IN (...)` 的参数个数上限（语句变量有界）。
const DELETE_EVENTS_CHUNK: usize = 1_000;

/// 权限行（`permissions` 表；ID 为 ULID 文本，本层不做 ULID 形状校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRecord {
    pub id: String,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    pub resource: String,
    pub action: String,
    pub target: Option<String>,
    /// 策略判定（`allow` / `deny` / `ask`）。
    pub decision: PermissionDecision,
    pub scope: Option<PermissionScope>,
    pub status: PermissionStatus,
    pub requested_at: i64,
    pub resolved_at: Option<i64>,
    pub resolver: Option<String>,
}

/// 用户自定义权限规则行（`permission_rules` 表；M6-01/ADR-019 附录 A-1）。
///
/// `resource` / `action` / `target_kind` / `decision` 为 CHECK 枚举文本（应用层先校验）；
/// `target` 为归一化目标形态（`path` = canonical；`any` 为 `None`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRuleRecord {
    pub id: String,
    pub name: String,
    pub resource: String,
    pub action: String,
    pub target_kind: String,
    pub target: Option<String>,
    pub decision: String,
    pub priority: i64,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 持久授权行（`permissions` 表投影；M6-01/ADR-019 附录 A-2：`scope IN ('session','always')`
/// + `status='resolved'` + `decision='allow'`，`target` 为 canonical 形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionGrantRecord {
    pub session_id: Option<String>,
    pub resource: String,
    pub action: String,
    pub target: String,
}

/// 会话引用行（`artifacts` 表；ADR-010 决策 1：文件/目录引用持久化）。
///
/// `path` 为 `artifact_add` canonicalize 后的绝对路径；`kind` 为 `file` / `directory`
/// （应用层校验，不递归）；`size_bytes` 仅文件有值。引用**不预授权**（D9 权限门不变）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRecord {
    pub id: String,
    pub session_id: String,
    pub path: String,
    pub kind: String,
    pub size_bytes: Option<i64>,
    pub created_at: i64,
}

/// 供应商行（`providers` 表；ADR-010 决策 3：模型与供应商配置）。
///
/// `api_key_ref` 只存 `keychain://aether/provider/<id>` 引用（密钥本体在 OS 凭据库 /
/// A3 降级加密文件，D10）；`is_builtin=1` 的内置供应商（迁移 0003 播种 4 条）
/// 不可删除（`provider_delete` 命令层硬拒绝）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRecord {
    pub id: String,
    pub name: String,
    /// `anthropic` / `openai` / `deepseek` / `google` / `custom`（应用层校验）。
    pub provider_type: String,
    pub base_url: Option<String>,
    /// 协议端点 JSON（ADR-028）：`{"anthropic":"…","openai":"…"}`；`{}` = 使用 base_url/类型默认。
    pub endpoints: String,
    pub api_key_ref: Option<String>,
    pub enabled: bool,
    pub is_builtin: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 供应商模型行（`provider_models` 表；`UNIQUE(provider_id, model_id)`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModelRecord {
    pub id: String,
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
    pub enabled: bool,
    pub created_at: i64,
}

/// 审计行（`audit_log` 表；P0 最小集：会话生命周期 + 权限请求/决议 + 适配器状态变化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditLogRecord {
    pub id: String,
    pub session_id: Option<String>,
    pub runtime_id: Option<String>,
    /// `user` / `agent` / `system`。
    pub actor: String,
    pub action: String,
    pub resource: Option<String>,
    pub detail: Option<String>,
    pub result: Option<String>,
    pub ts: i64,
}

/// 审计查询单页上限（ADR-023 §2.1-3：`limit` ≤500）。
pub const AUDIT_QUERY_MAX_LIMIT: usize = 500;
/// 审计查询缺省页大小（ADR-023 §2.1-3：缺省 100）。
pub const AUDIT_QUERY_DEFAULT_LIMIT: usize = 100;

/// 审计查询过滤条件（M6-03/ADR-023 §2.1-3 / v0.3 分页设计修订）。
///
/// 过滤语义：`session_id` / `actor` / `action` / `resource` 为**等值匹配**；
/// 时间过滤为**闭区间** `from_ts ≤ ts ≤ to_ts`；分页经 `limit`（缺省
/// [`AUDIT_QUERY_DEFAULT_LIMIT`]、上限 [`AUDIT_QUERY_MAX_LIMIT`]）与 `offset`；
/// 稳定排序 `ts DESC, id DESC`。导出复用同一过滤条件（忽略 `limit`/`offset`）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuditQuery {
    pub session_id: Option<String>,
    pub actor: Option<String>,
    pub action: Option<String>,
    pub resource: Option<String>,
    pub from_ts: Option<i64>,
    pub to_ts: Option<i64>,
    /// 页大小（`None` = 缺省 100；调用方负责上限校验，本层再钳制）。
    pub limit: Option<usize>,
    pub offset: usize,
}

/// 审计查询结果（`rows` 按 `ts DESC, id DESC` 稳定排序）。
///
/// v0.3 分页设计修订（ADR-023 §6-3）：不提供命中总数（`COUNT(*)` 在多谓词过滤下
/// 需逐行回表，1e6 行实测 0.36–3.4s，超出 P95 目标）；以 `has_more`
/// （多取一行的探测）表达「还有下一页」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditQueryResult {
    pub rows: Vec<AuditLogRecord>,
    pub limit: usize,
    pub offset: usize,
    /// 是否还有下一页（`rows.len()` 回填为 `limit` 后判定）。
    pub has_more: bool,
}

/// 单个工作区的会话计数（M7-08 `workspaces_list`；ADR-026 §2.1-1）。
///
/// `active` = 在途口径（会话状态 `creating`/`running`/`paused`/`waiting_permission`，
/// **或**存在 `queued`/`running` run——与 `active_work_counts` 同源判定）；
/// `total` = 该工作区全部会话数（含终态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkspaceSessionCount {
    pub active: u64,
    pub total: u64,
}

/// `workspaces_list` 的计数快照：按工作区聚合 + 未分配会话（`workspace_id IS NULL`）
/// 的合成组计数（ADR-026 §2.1-1：合成组不建 `workspaces` 行）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceSessionCounts {
    /// `workspace_id → 计数`（无会话的工作区不出现，调用方补 0）。
    pub by_workspace: BTreeMap<String, WorkspaceSessionCount>,
    /// `sessions.workspace_id IS NULL` 的合成组「默认（数据目录）」。
    pub unassigned: WorkspaceSessionCount,
}

/// `session_search` 命中（ADR-025 §2.1-2）：会话摘要 + FTS `snippet()` + 命中来源。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSearchHit {
    pub session: Session,
    /// `snippet()` 摘要（命中词以 `[]` 包裹；最多 32 token）。
    pub snippet: String,
    /// `title` | `message`（标题命中权重高于正文）。
    pub source: String,
}

/// 审计过滤条件 → `WHERE` 子句与命名参数（等值 + 闭区间时间）。
pub(crate) fn audit_filter_sql(query: &AuditQuery) -> (String, Vec<rusqlite::types::Value>) {
    let mut clauses: Vec<&str> = Vec::new();
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    let mut push = |clause: &'static str, value: rusqlite::types::Value| {
        clauses.push(clause);
        values.push(value);
    };
    if let Some(session_id) = &query.session_id {
        push("session_id = ?", rusqlite::types::Value::Text(session_id.clone()));
    }
    if let Some(actor) = &query.actor {
        push("actor = ?", rusqlite::types::Value::Text(actor.clone()));
    }
    if let Some(action) = &query.action {
        push("action = ?", rusqlite::types::Value::Text(action.clone()));
    }
    if let Some(resource) = &query.resource {
        push("resource = ?", rusqlite::types::Value::Text(resource.clone()));
    }
    if let Some(from_ts) = query.from_ts {
        push("ts >= ?", rusqlite::types::Value::Integer(from_ts));
    }
    if let Some(to_ts) = query.to_ts {
        push("ts <= ?", rusqlite::types::Value::Integer(to_ts));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    (where_sql, values)
}

/// `audit_log` 行读取（查询与导出共用口径）。
pub(crate) fn read_audit_row(row: &Row<'_>) -> rusqlite::Result<AuditLogRecord> {
    Ok(AuditLogRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        runtime_id: row.get(2)?,
        actor: row.get(3)?,
        action: row.get(4)?,
        resource: row.get(5)?,
        detail: row.get(6)?,
        result: row.get(7)?,
        ts: row.get(8)?,
    })
}


/// 领域写命令（经单写队列执行；每个命令一个事务）。
#[derive(Debug, Clone, PartialEq)]
pub enum StoreCommand {
    /// 确保运行时注册行存在（FK `sessions.runtime_id` 前置；已存在则更新摘要字段）。
    EnsureRuntime { runtime: Runtime },
    /// 写入运行时用户启停偏好（M7-01/ADR-015 决策 5：`runtimes.enabled` 唯一事实源）。
    ///
    /// 行不存在时为完整 upsert（`Runtime` 携带全部非空列）；已存在时**仅**更新
    /// `enabled` 与 `updated_at`（不得覆盖 `EnsureRuntime` 维护的摘要字段）。
    SetRuntimeEnabled {
        runtime: Runtime,
        /// 目标偏好（`true`=启用 / `false`=用户禁用）。
        enabled: bool,
    },
    /// 插入会话行。
    InsertSession { session: Session },
    /// 更新会话状态（`closed_at` 仅在给出时写入）。
    UpdateSessionStatus {
        session_id: SessionId,
        status: SessionStatus,
        updated_at: i64,
        closed_at: Option<i64>,
    },
    /// 更新会话私有配置（`sessions.config`；M3-02：`native_id` 等适配器映射，
    /// 调用方负责与既有配置合并后整体写入，本层不做键级合并）。
    UpdateSessionConfig {
        session_id: SessionId,
        config: serde_json::Value,
        updated_at: i64,
    },
    /// 改写会话级思考深度（ADR-010 能力门延迟判定：runtime ready 后未声明
    /// `thinking_depth` 能力 → 按缺省 2 改写；M3-10）。
    UpdateSessionThinkingDepth {
        session_id: SessionId,
        thinking_depth: u8,
        updated_at: i64,
    },
    /// 改写会话级权限模式（M6-01/ADR-014 决策 3：`session_permission_mode_set`；
    /// 档位为判定权威入参，仅影响后续工具调用决策——在途会话沙箱参数不变）。
    UpdateSessionPermissionMode {
        session_id: SessionId,
        permission_mode: PermissionMode,
        updated_at: i64,
    },
    /// 插入 run 行。
    InsertRun { run: Run },
    /// 改写 run 生效思考深度（ADR-010 能力门：runtime 未声明能力 → 落缺省 2；M3-10）。
    UpdateRunThinkingDepth { run_id: RunId, thinking_depth: u8 },
    /// 启动排队中的 run（`queued → running`，记录 `started_at`）。
    StartRun { run_id: RunId, started_at: i64 },
    /// 更新 run 终态（`status` / `error` / `finished_at`）。
    FinishRun {
        run_id: RunId,
        status: RunStatus,
        error: Option<String>,
        finished_at: i64,
    },
    /// 原子幂等发送：按 `(session_id, client_msg_id)` 查重，命中返回既有消息/run；
    /// 未命中同事务插入用户消息与 run 行（`messages.seq` 由存储侧分配）。
    BeginRunIdempotent { message: Message, run: Run },
    /// 插入消息（助手/工具/系统消息；`client_msg_id` 为 `NULL`，`seq=0` 时自动分配）。
    InsertMessage { message: Message },
    /// 更新消息正文（终稿覆盖）。
    UpdateMessageContent {
        message_id: MessageId,
        content: String,
    },
    /// 插入权限请求行（`status=pending` 或策略直决）。
    InsertPermission { record: PermissionRecord },
    /// 审批决议：更新行 `decision/scope/status/resolved_at/resolver`。
    ///
    /// `target`（M6-01/ADR-019 附录 A-2）：`session`/`always` 授权决议时以
    /// `ticket.canonical_target` 覆写 `permissions.target`（不新增列）；`None` = 不改写。
    ResolvePermission {
        id: String,
        decision: PermissionDecision,
        scope: Option<PermissionScope>,
        status: PermissionStatus,
        resolved_at: i64,
        resolver: Option<String>,
        target: Option<String>,
    },
    /// 审批超时：置 `status=timeout` 且 `decision=deny`（D9：超时按拒绝处理并审计）。
    TimeoutPermission { id: String, resolved_at: i64 },
    /// 插入用户自定义权限规则（M6-01 `rule_add`；ADR-019）。
    InsertPermissionRule { rule: PermissionRuleRecord },
    /// 整体更新用户自定义权限规则（M6-01 `rule_update`；调用方负责合并后整体写入）。
    UpdatePermissionRule { rule: PermissionRuleRecord },
    /// 删除用户自定义权限规则（M6-01 `rule_delete`；不存在时影响 0 行）。
    DeletePermissionRule { id: String },
    /// 启用/停用用户自定义权限规则（M6-01 `rule_toggle`）。
    SetPermissionRuleEnabled {
        id: String,
        enabled: bool,
        updated_at: i64,
    },
    /// 追加审计行。
    InsertAudit { record: AuditLogRecord },
    /// 登记备份台账行（M3-04 `backup_create`：`VACUUM INTO` 产物 + 元数据）。
    InsertBackup { record: BackupRecord },
    /// 删除备份台账行（M3-04 保留策略：超出保留份数的最旧记录）。
    DeleteBackup { id: String },
    /// 写入/更新设置项（`settings` 表；值为 JSON 文本，M3-05 备份提醒开关）。
    ///
    /// 单写者约束：设置写路径同样经 [`crate::WriteQueue::execute`]（AGENTS §2.4）。
    UpsertSetting {
        key: String,
        value: String,
        updated_at: i64,
    },
    /// 写入/更新工作区行（M3-08 `workspace_set`；`workspaces` 表，按 id upsert）。
    ///
    /// 单写者约束：工作区写路径同样经 [`crate::WriteQueue::execute`]（AGENTS §2.4）。
    UpsertWorkspace { workspace: Workspace },
    /// 工作区改名（M7-08 `workspace_update`；ADR-026 §2.1-1：`root_path` 不可变，
    /// 防权限基准漂移）。不存在时影响 0 行——存在性由命令层先行判定。
    RenameWorkspace {
        id: WorkspaceId,
        name: String,
        updated_at: i64,
    },
    /// 删除工作区行（M7-08 `workspace_delete`；ADR-026 §2.1-1）。
    ///
    /// **存在会话引用（`sessions.workspace_id`）→ 不删除、不级联、不改派**，返回
    /// [`StoreOutcome::WorkspaceInUse`]；引用检查在**同一事务内**复核（ADR-026
    /// 「删除前置检查在事务内复核」）。只删行，不触碰磁盘文件。
    DeleteWorkspace { id: WorkspaceId },
    /// 会话改派工作区（M7-08 `session_workspace_assign`；ADR-026 §2.1-1）。
    ///
    /// 原子写入 `sessions.workspace_id`、`sessions.config`（含权限基准快照标记）与
    /// `sessions.system_prompt`（按新工作区记忆注入重合成的结果）；权限基准**不重算**
    /// （R15/不变量 11）——由调用方在 `config` 中固定创建时基准。
    AssignSessionWorkspace {
        session_id: SessionId,
        workspace_id: WorkspaceId,
        config: serde_json::Value,
        system_prompt: Option<String>,
        updated_at: i64,
    },
    /// 插入 Agent 预设（M7-09 `preset_add`；ADR-026 §2.2-2）。
    ///
    /// 单写者约束：预设写路径同样经 [`crate::WriteQueue::execute`]（AGENTS §2.4）。
    InsertPreset { preset: Preset },
    /// 整体更新 Agent 预设（M7-09 `preset_update`；内置可编辑，`is_builtin` 与
    /// `created_at` 不变——调用方负责合并后整体写入）。
    UpdatePreset { preset: Preset },
    /// 删除自定义预设（M7-09 `preset_delete`；内置拒绝在命令层先行判定；
    /// `sessions.preset_id` 经 `ON DELETE SET NULL` 置空，会话提示词保持原样）。
    DeletePreset { id: PresetId },
    /// 幂等登记会话引用（M3-09 `artifact_add`；ADR-010 决策 1）：单事务内
    /// `INSERT … ON CONFLICT(session_id, path) DO NOTHING` + 回读，同会话同路径
    /// 命中既有行时返回原引用（不重复插入、不改变原 `id`/`created_at`）。
    ///
    /// 单写者约束：引用写路径同样经 [`crate::WriteQueue::execute`]（AGENTS §2.4）。
    InsertArtifact { artifact: ArtifactRecord },
    /// 删除会话引用（M3-09 `artifact_remove`；不存在时影响 0 行——幂等语义）。
    ///
    /// 单写者约束：删除同样经 [`crate::WriteQueue::execute`]（AGENTS §2.4）。
    RemoveArtifact {
        session_id: SessionId,
        artifact_id: String,
    },
    /// 插入供应商行（M3-11 `provider_create`；ADR-010 决策 3）。
    ///
    /// 单写者约束：供应商写路径同样经 [`crate::WriteQueue::execute`]（AGENTS §2.4）。
    InsertProvider { provider: ProviderRecord },
    /// 整体更新供应商行（M3-11 `provider_update`；名称/Base URL/引用/启用态/更新时间）。
    ///
    /// `api_key_ref` 以调用方解析的三态结果整体写入（缺省=不变 / 空串=清除 / 非空=覆盖）；
    /// `type` 创建后不可改（本命令不携带）。
    UpdateProvider { provider: ProviderRecord },
    /// 删除供应商行（M3-11 `provider_delete`；`provider_models` 经 `ON DELETE CASCADE`
    /// 级联删除；不存在时影响 0 行——存在性由命令层先行判定）。
    DeleteProvider { id: String },
    /// 快速启用/停用供应商（M3-11 `provider_toggle`；内置可停用）。
    SetProviderEnabled {
        id: String,
        enabled: bool,
        updated_at: i64,
    },
    /// 插入供应商模型（M3-11 `provider_model_add`；重复 `(provider_id, model_id)`
    /// 由 `UNIQUE` 约束拒绝并映射 [`crate::StoreError::DuplicateProviderModel`]）。
    InsertProviderModel { model: ProviderModelRecord },
    /// 启用/停用供应商模型（M3-11 `provider_model_toggle`）。
    SetProviderModelEnabled {
        provider_id: String,
        model_id: String,
        enabled: bool,
    },
    /// 刷新供应商模型显示名（M7-06/ADR-024 §2.1-4：幂等 upsert 的已存在分支；
    /// `enabled`/`created_at` 保持不变；不存在时影响 0 行——存在性由命令层先行判定）。
    RefreshProviderModelDisplayName {
        provider_id: String,
        model_id: String,
        display_name: String,
    },
    /// 会话改名（M7-07/ADR-025 §2.1-3：`session_rename`；FTS 触发器自动同步）。
    ///
    /// 不存在时影响 0 行——存在性由控制层先行判定（最终由命令层返回 `session_not_found`）。
    RenameSession {
        id: SessionId,
        title: String,
        updated_at: i64,
    },
    /// 硬删除会话（M7-07/ADR-025 §2.1-4：`session_delete`）。
    ///
    /// **同一事务内**复核：会话存在；无 `queued`/`running` run、无 `waiting_permission`/
    /// pending 票据（否则 [`StoreOutcome::SessionBusy`]）。删除 `sessions` 行，经 FK
    /// `ON DELETE CASCADE` 连带 `messages`/`runs`/`artifacts`/`permissions`；
    /// **`events` 不级联**（R7，保留至 M5-04 保留策略）。FTS 触发器同步清理索引。
    DeleteSession { id: SessionId },
    /// 删除指定 `rowid` 的事件行（M5-04 保留清理的删除半步；先归档后删除）。
    ///
    /// 调用方（保留任务）先经 [`crate::archive`] 归档精确行集合并校验通过，再提交本
    /// 命令按 `rowid`（删除精确定位）物理删除；批 ≤ [`crate::archive::RETENTION_BATCH_MAX`]
    /// （5000 行/命令，单事务 <500ms，ADR-021 §2.1-5）。
    ///
    /// 单写者约束：删除同样经 [`crate::WriteQueue::execute`]（AGENTS §2.4）。
    DeleteEventsByIds {
        /// 待删事件行 `rowid` 列表（空列表为 no-op）。
        rowids: Vec<i64>,
    },
}

/// 写命令结果。
#[derive(Debug, Clone, PartialEq)]
pub enum StoreOutcome {
    /// 应用成功；`affected` 为影响行数。
    Applied { affected: usize },
    /// [`StoreCommand::BeginRunIdempotent`] 结果：既有或新建的消息/run。
    RunAccepted {
        message_id: MessageId,
        run_id: RunId,
        /// `true` = 幂等命中（返回既有行，不重复插入）。
        duplicate: bool,
    },
    /// [`StoreCommand::InsertArtifact`] 结果：新增或既有引用。
    ArtifactRecorded {
        artifact: ArtifactRecord,
        /// `true` = 本次新建；`false` = 幂等命中既有行（UNIQUE 兜底）。
        inserted: bool,
    },
    /// [`StoreCommand::DeleteWorkspace`] 结果：存在会话引用（`sessions.workspace_id`），
    /// **未删除**、未级联、未改派（ADR-026 §2.1-1：`workspace_in_use`）。
    WorkspaceInUse {
        /// 引用该工作区的会话数（UI 文案「仍有 N 个会话引用」）。
        sessions: u64,
    },
    /// [`StoreCommand::DeleteSession`] 结果：会话不存在（事务内复核）。
    SessionNotFound,
    /// [`StoreCommand::DeleteSession`] 结果：存在在途 run / 等待审批 / pending 票据，
    /// **未删除**（ADR-025 §2.1-4：`session_busy`）。
    SessionBusy {
        /// `queued`/`running` run 数。
        active_runs: u64,
        /// `status='pending'` 权限票据数。
        pending_permissions: u64,
    },
    /// [`StoreCommand::DeleteSession`] 结果：已硬删除，并给出级联行数摘要
    /// （审计 detail；`events` 不级联，单列保留计数）。
    SessionDeleted {
        messages: u64,
        runs: u64,
        artifacts: u64,
        permissions: u64,
        events_retained: u64,
    },
}

/// 执行一条写命令（单事务；由单写任务调用）。
pub(crate) fn apply_command(
    connection: &mut Connection,
    command: &StoreCommand,
) -> Result<StoreOutcome, StoreError> {
    match command {
        StoreCommand::EnsureRuntime { runtime } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let config = json_text(&runtime.config)?;
            let capabilities = serde_json::to_string(&runtime.capabilities)
                .map_err(|error| serialization_failed("runtimes.capabilities", &error))?;
            let affected = transaction
                .execute(
                    "INSERT INTO runtimes (id, name, kind, version, protocol, capabilities, endpoint, config, \
                     status, status_reason, last_seen_at, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
                     ON CONFLICT(id) DO UPDATE SET name = excluded.name, kind = excluded.kind, \
                     version = excluded.version, protocol = excluded.protocol, \
                     capabilities = excluded.capabilities, endpoint = excluded.endpoint, \
                     config = excluded.config, updated_at = excluded.updated_at",
                    params![
                        runtime.id.as_str(),
                        runtime.name,
                        runtime.kind,
                        runtime.version,
                        runtime.protocol,
                        capabilities,
                        runtime.endpoint,
                        config,
                        runtime.status.as_str(),
                        runtime.status_reason,
                        runtime.last_seen_at,
                        runtime.created_at,
                        runtime.updated_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::SetRuntimeEnabled { runtime, enabled } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let config = json_text(&runtime.config)?;
            let capabilities = serde_json::to_string(&runtime.capabilities)
                .map_err(|error| serialization_failed("runtimes.capabilities", &error))?;
            let affected = transaction
                .execute(
                    "INSERT INTO runtimes (id, name, kind, version, protocol, capabilities, endpoint, config, \
                     status, status_reason, last_seen_at, enabled, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
                     ON CONFLICT(id) DO UPDATE SET enabled = excluded.enabled, \
                     updated_at = excluded.updated_at",
                    params![
                        runtime.id.as_str(),
                        runtime.name,
                        runtime.kind,
                        runtime.version,
                        runtime.protocol,
                        capabilities,
                        runtime.endpoint,
                        config,
                        runtime.status.as_str(),
                        runtime.status_reason,
                        runtime.last_seen_at,
                        enabled,
                        runtime.created_at,
                        runtime.updated_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::InsertSession { session } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let config = json_text(&session.config)?;
            let token_usage = serde_json::to_string(&session.token_usage)
                .map_err(|error| serialization_failed("sessions.token_usage", &error))?;
            let affected = transaction
                .execute(
                    "INSERT INTO sessions (id, runtime_id, workspace_id, parent_session_id, title, status, \
                     model, thinking_depth, permission_mode, system_prompt, config, token_usage, created_at, updated_at, closed_at, preset_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                    params![
                        session.id.as_str(),
                        session.runtime_id.as_str(),
                        session.workspace_id.as_ref().map(WorkspaceId::as_str),
                        session.parent_session_id.as_ref().map(SessionId::as_str),
                        session.title,
                        session.status.as_str(),
                        session.model,
                        session.thinking_depth,
                        session.permission_mode.as_str(),
                        session.system_prompt,
                        config,
                        token_usage,
                        session.created_at,
                        session.updated_at,
                        session.closed_at,
                        session.preset_id.as_ref().map(PresetId::as_str),
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::UpdateSessionStatus {
            session_id,
            status,
            updated_at,
            closed_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE sessions SET status = ?2, updated_at = ?3, \
                     closed_at = CASE WHEN ?4 IS NULL THEN closed_at ELSE ?4 END WHERE id = ?1",
                    params![session_id.as_str(), status.as_str(), updated_at, closed_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::UpdateSessionConfig {
            session_id,
            config,
            updated_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let config = json_text(config)?;
            let affected = transaction
                .execute(
                    "UPDATE sessions SET config = ?2, updated_at = ?3 WHERE id = ?1",
                    params![session_id.as_str(), config, updated_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::UpdateSessionThinkingDepth {
            session_id,
            thinking_depth,
            updated_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE sessions SET thinking_depth = ?2, updated_at = ?3 WHERE id = ?1",
                    params![session_id.as_str(), i64::from(*thinking_depth), updated_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::UpdateSessionPermissionMode {
            session_id,
            permission_mode,
            updated_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE sessions SET permission_mode = ?2, updated_at = ?3 WHERE id = ?1",
                    params![session_id.as_str(), permission_mode.as_str(), updated_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::InsertRun { run } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = insert_run(&transaction, run)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::UpdateRunThinkingDepth {
            run_id,
            thinking_depth,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE runs SET thinking_depth = ?2 WHERE id = ?1",
                    params![run_id.as_str(), i64::from(*thinking_depth)],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::StartRun { run_id, started_at } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE runs SET status = 'running', started_at = ?2 WHERE id = ?1",
                    params![run_id.as_str(), started_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::FinishRun {
            run_id,
            status,
            error,
            finished_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE runs SET status = ?2, error = ?3, finished_at = ?4 WHERE id = ?1",
                    params![run_id.as_str(), status.as_str(), error, finished_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::BeginRunIdempotent { message, run } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let existing: Option<(String, Option<String>)> = transaction
                .query_row(
                    "SELECT id, run_id FROM messages WHERE session_id = ?1 AND client_msg_id = ?2",
                    params![
                        message.session_id.as_str(),
                        message.client_msg_id.as_deref()
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(txn_failed)?;
            if let Some((existing_id, existing_run_id)) = existing {
                let message_id = MessageId::new(existing_id)
                    .map_err(|error| invalid_row("messages.id", &error.to_string()))?;
                let run_id = match existing_run_id {
                    Some(run_id) => RunId::new(run_id)
                        .map_err(|error| invalid_row("messages.run_id", &error.to_string()))?,
                    None => find_run_by_input_message(&transaction, message_id.as_str())?
                        .ok_or_else(|| StoreError::InvalidStoredEvent {
                            id: message_id.to_string(),
                            reason: "幂等命中但 run 缺失（messages.run_id 为空且无关联 run 行）"
                                .to_owned(),
                        })?,
                };
                transaction.commit().map_err(txn_failed)?;
                return Ok(StoreOutcome::RunAccepted {
                    message_id,
                    run_id,
                    duplicate: true,
                });
            }
            insert_message(&transaction, message)?;
            insert_run(&transaction, run)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::RunAccepted {
                message_id: message.id.clone(),
                run_id: run.id.clone(),
                duplicate: false,
            })
        }
        StoreCommand::InsertMessage { message } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = insert_message(&transaction, message)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::UpdateMessageContent {
            message_id,
            content,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE messages SET content = ?2 WHERE id = ?1",
                    params![message_id.as_str(), content],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::InsertPermission { record } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "INSERT INTO permissions (id, session_id, request_id, resource, action, target, decision, \
                     scope, status, requested_at, resolved_at, resolver) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        record.id,
                        record.session_id,
                        record.request_id,
                        record.resource,
                        record.action,
                        record.target,
                        record.decision.as_str(),
                        record.scope.map(PermissionScope::as_str),
                        record.status.as_str(),
                        record.requested_at,
                        record.resolved_at,
                        record.resolver,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::ResolvePermission {
            id,
            decision,
            scope,
            status,
            resolved_at,
            resolver,
            target,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE permissions SET decision = ?2, scope = ?3, status = ?4, resolved_at = ?5, \
                     resolver = ?6, target = CASE WHEN ?7 IS NULL THEN target ELSE ?7 END \
                     WHERE id = ?1 AND status = 'pending'",
                    params![
                        id,
                        decision.as_str(),
                        scope.map(PermissionScope::as_str),
                        status.as_str(),
                        resolved_at,
                        resolver,
                        target,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::TimeoutPermission { id, resolved_at } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE permissions SET decision = 'deny', status = 'timeout', resolved_at = ?2 \
                     WHERE id = ?1 AND status = 'pending'",
                    params![id, resolved_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::InsertPermissionRule { rule } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "INSERT INTO permission_rules (id, name, resource, action, target_kind, target, \
                     decision, priority, enabled, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        rule.id,
                        rule.name,
                        rule.resource,
                        rule.action,
                        rule.target_kind,
                        rule.target,
                        rule.decision,
                        rule.priority,
                        i64::from(rule.enabled),
                        rule.created_at,
                        rule.updated_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::UpdatePermissionRule { rule } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE permission_rules SET name = ?2, resource = ?3, action = ?4, \
                     target_kind = ?5, target = ?6, decision = ?7, priority = ?8, enabled = ?9, \
                     updated_at = ?10 WHERE id = ?1",
                    params![
                        rule.id,
                        rule.name,
                        rule.resource,
                        rule.action,
                        rule.target_kind,
                        rule.target,
                        rule.decision,
                        rule.priority,
                        i64::from(rule.enabled),
                        rule.updated_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::DeletePermissionRule { id } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute("DELETE FROM permission_rules WHERE id = ?1", params![id])
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::SetPermissionRuleEnabled {
            id,
            enabled,
            updated_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE permission_rules SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id, i64::from(*enabled), updated_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::InsertAudit { record } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "INSERT INTO audit_log (id, session_id, runtime_id, actor, action, resource, detail, result, ts) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        record.id,
                        record.session_id,
                        record.runtime_id,
                        record.actor,
                        record.action,
                        record.resource,
                        record.detail,
                        record.result,
                        record.ts,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::InsertBackup { record } => {
            let path = record
                .path
                .to_str()
                .ok_or_else(|| StoreError::NonUtf8Path {
                    path: record.path.clone(),
                })?;
            let size_bytes =
                i64::try_from(record.size_bytes).map_err(|_| StoreError::Internal {
                    reason: format!(
                        "backups.size_bytes 超出 SQLite INTEGER 范围: {}",
                        record.size_bytes
                    ),
                })?;
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "INSERT INTO backups (id, path, size_bytes, encrypted, kind, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        record.id,
                        path,
                        size_bytes,
                        i64::from(record.encrypted),
                        record.kind,
                        record.created_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::DeleteBackup { id } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute("DELETE FROM backups WHERE id = ?1", [id])
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::UpsertSetting {
            key,
            value,
            updated_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
                     updated_at = excluded.updated_at",
                    params![key, value, updated_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::UpsertWorkspace { workspace } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let memory_files = serde_json::to_string(&workspace.memory_files)
                .map_err(|error| serialization_failed("workspaces.memory_files", &error))?;
            let affected = transaction
                .execute(
                    "INSERT INTO workspaces (id, name, root_path, memory_files, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                     ON CONFLICT(id) DO UPDATE SET name = excluded.name, \
                     root_path = excluded.root_path, memory_files = excluded.memory_files, \
                     updated_at = excluded.updated_at",
                    params![
                        workspace.id.as_str(),
                        workspace.name,
                        workspace.root_path,
                        memory_files,
                        workspace.created_at,
                        workspace.updated_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::RenameWorkspace {
            id,
            name,
            updated_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE workspaces SET name = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id.as_str(), name, updated_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::DeleteWorkspace { id } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            // ADR-026 §2.1-1：存在会话引用 → 拒绝（不级联、不改派）；检查与删除同一
            // 事务（防「检查后会话改派」竞态）。
            let references: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .map_err(txn_failed)?;
            if references > 0 {
                return Ok(StoreOutcome::WorkspaceInUse {
                    sessions: u64::try_from(references).unwrap_or(u64::MAX),
                });
            }
            let affected = transaction
                .execute("DELETE FROM workspaces WHERE id = ?1", [id.as_str()])
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::AssignSessionWorkspace {
            session_id,
            workspace_id,
            config,
            system_prompt,
            updated_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let config = json_text(config)?;
            let affected = transaction
                .execute(
                    "UPDATE sessions SET workspace_id = ?2, config = ?3, system_prompt = ?4, \
                     updated_at = ?5 WHERE id = ?1",
                    params![
                        session_id.as_str(),
                        workspace_id.as_str(),
                        config,
                        system_prompt,
                        updated_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::RenameSession {
            id,
            title,
            updated_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id.as_str(), title, updated_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::DeleteSession { id } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            // ADR-025 §2.1-4：前置检查与删除同一事务（防「检查后新 run/审批」竞态）。
            let status: Option<String> = transaction
                .query_row(
                    "SELECT status FROM sessions WHERE id = ?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(txn_failed)?;
            let Some(status) = status else {
                return Ok(StoreOutcome::SessionNotFound);
            };
            let active_runs: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM runs WHERE session_id = ?1 AND status IN ('queued','running')",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .map_err(txn_failed)?;
            let pending_permissions: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM permissions WHERE session_id = ?1 AND status = 'pending'",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .map_err(txn_failed)?;
            if status == "waiting_permission"
                || status == "creating"
                || status == "running"
                || active_runs > 0
                || pending_permissions > 0
            {
                return Ok(StoreOutcome::SessionBusy {
                    active_runs: u64::try_from(active_runs).unwrap_or(u64::MAX),
                    pending_permissions: u64::try_from(pending_permissions).unwrap_or(u64::MAX),
                });
            }
            let messages: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .map_err(txn_failed)?;
            let runs: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM runs WHERE session_id = ?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .map_err(txn_failed)?;
            let artifacts: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM artifacts WHERE session_id = ?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .map_err(txn_failed)?;
            let permissions: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM permissions WHERE session_id = ?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .map_err(txn_failed)?;
            let events_retained: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .map_err(txn_failed)?;
            let affected = transaction
                .execute("DELETE FROM sessions WHERE id = ?1", [id.as_str()])
                .map_err(txn_failed)?;
            if affected == 0 {
                return Ok(StoreOutcome::SessionNotFound);
            }
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::SessionDeleted {
                messages: u64::try_from(messages).unwrap_or(u64::MAX),
                runs: u64::try_from(runs).unwrap_or(u64::MAX),
                artifacts: u64::try_from(artifacts).unwrap_or(u64::MAX),
                permissions: u64::try_from(permissions).unwrap_or(u64::MAX),
                events_retained: u64::try_from(events_retained).unwrap_or(u64::MAX),
            })
        }
        StoreCommand::InsertPreset { preset } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "INSERT INTO presets (id, name, system_prompt, is_builtin, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        preset.id.as_str(),
                        preset.name,
                        preset.system_prompt,
                        i64::from(preset.is_builtin),
                        preset.created_at,
                        preset.updated_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::UpdatePreset { preset } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE presets SET name = ?2, system_prompt = ?3, updated_at = ?4 WHERE id = ?1",
                    params![
                        preset.id.as_str(),
                        preset.name,
                        preset.system_prompt,
                        preset.updated_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::DeletePreset { id } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute("DELETE FROM presets WHERE id = ?1", [id.as_str()])
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::InsertArtifact { artifact } => {
            // 幂等（UNIQUE(session_id, path) 兜底）：先尝试插入，命中冲突回读既有行；
            // 同一事务保证「查重 → 返回」原子，重复添加不产生重复行也不改写既有引用。
            let transaction = connection.transaction().map_err(txn_failed)?;
            let inserted = transaction
                .execute(
                    "INSERT INTO artifacts (id, session_id, path, kind, size_bytes, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                     ON CONFLICT(session_id, path) DO NOTHING",
                    params![
                        artifact.id,
                        artifact.session_id,
                        artifact.path,
                        artifact.kind,
                        artifact.size_bytes,
                        artifact.created_at,
                    ],
                )
                .map_err(txn_failed)?;
            let record = transaction
                .query_row(
                    "SELECT id, session_id, path, kind, size_bytes, created_at FROM artifacts \
                     WHERE session_id = ?1 AND path = ?2",
                    params![artifact.session_id, artifact.path],
                    read_artifact_row,
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::ArtifactRecorded {
                artifact: record,
                inserted: inserted > 0,
            })
        }
        StoreCommand::RemoveArtifact {
            session_id,
            artifact_id,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "DELETE FROM artifacts WHERE id = ?1 AND session_id = ?2",
                    params![artifact_id, session_id.as_str()],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::InsertProvider { provider } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "INSERT INTO providers (id, name, type, base_url, endpoints, api_key_ref, enabled, \
                     is_builtin, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        provider.id,
                        provider.name,
                        provider.provider_type,
                        provider.base_url,
                        provider.endpoints,
                        provider.api_key_ref,
                        i64::from(provider.enabled),
                        i64::from(provider.is_builtin),
                        provider.created_at,
                        provider.updated_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::UpdateProvider { provider } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE providers SET name = ?2, base_url = ?3, endpoints = ?4, api_key_ref = ?5, \
                     enabled = ?6, updated_at = ?7 WHERE id = ?1",
                    params![
                        provider.id,
                        provider.name,
                        provider.base_url,
                        provider.endpoints,
                        provider.api_key_ref,
                        i64::from(provider.enabled),
                        provider.updated_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::DeleteProvider { id } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute("DELETE FROM providers WHERE id = ?1", [id])
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::SetProviderEnabled {
            id,
            enabled,
            updated_at,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE providers SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id, i64::from(*enabled), updated_at],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::InsertProviderModel { model } => {
            // 重复 `(provider_id, model_id)` 由 `UNIQUE` 约束拒绝；将 SQLite 约束冲突
            // 显式映射为 [`StoreError::DuplicateProviderModel`]，供命令层稳定应答
            // `invalid_value`（ADR-010 附录 B.1）。
            let transaction = connection.transaction().map_err(txn_failed)?;
            let duplicate: Option<i64> = transaction
                .query_row(
                    "SELECT 1 FROM provider_models WHERE provider_id = ?1 AND model_id = ?2",
                    params![model.provider_id, model.model_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(txn_failed)?;
            if duplicate.is_some() {
                return Err(StoreError::DuplicateProviderModel {
                    provider_id: model.provider_id.clone(),
                    model_id: model.model_id.clone(),
                });
            }
            let affected = transaction
                .execute(
                    "INSERT INTO provider_models (id, provider_id, model_id, display_name, enabled, \
                     created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        model.id,
                        model.provider_id,
                        model.model_id,
                        model.display_name,
                        i64::from(model.enabled),
                        model.created_at,
                    ],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::SetProviderModelEnabled {
            provider_id,
            model_id,
            enabled,
        } => {
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE provider_models SET enabled = ?3 WHERE provider_id = ?1 AND model_id = ?2",
                    params![provider_id, model_id, i64::from(*enabled)],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::RefreshProviderModelDisplayName {
            provider_id,
            model_id,
            display_name,
        } => {
            // M7-06/ADR-024 §2.1-4：`provider_model_add` / `provider_models_fetch` 幂等
            // upsert 的「已存在仅刷新 display_name」分支（enabled/created_at 保持不变）。
            let transaction = connection.transaction().map_err(txn_failed)?;
            let affected = transaction
                .execute(
                    "UPDATE provider_models SET display_name = ?3 \
                     WHERE provider_id = ?1 AND model_id = ?2",
                    params![provider_id, model_id, display_name],
                )
                .map_err(txn_failed)?;
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
        StoreCommand::DeleteEventsByIds { rowids } => {
            if rowids.is_empty() {
                return Ok(StoreOutcome::Applied { affected: 0 });
            }
            let transaction = connection.transaction().map_err(txn_failed)?;
            // 参数化 `IN` 分批（语句变量数有界）；`rowid` 为删除精确定位（调用方已归档）。
            let mut affected = 0usize;
            for chunk in rowids.chunks(DELETE_EVENTS_CHUNK) {
                let placeholders = std::iter::repeat("?")
                    .take(chunk.len())
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!("DELETE FROM events WHERE rowid IN ({placeholders})");
                let values: Vec<&dyn rusqlite::ToSql> = chunk
                    .iter()
                    .map(|rowid| rowid as &dyn rusqlite::ToSql)
                    .collect();
                affected = affected.saturating_add(
                    transaction
                        .execute(&sql, values.as_slice())
                        .map_err(txn_failed)?,
                );
            }
            transaction.commit().map_err(txn_failed)?;
            Ok(StoreOutcome::Applied { affected })
        }
    }
}

fn insert_message(connection: &Connection, message: &Message) -> Result<usize, StoreError> {
    let tool_calls = match &message.tool_calls {
        Some(value) => Some(json_text(value)?),
        None => None,
    };
    let content_parts = match &message.content_parts {
        Some(value) => Some(json_text(value)?),
        None => None,
    };
    let explicit_seq = i64::try_from(message.seq).map_err(|_| StoreError::Internal {
        reason: format!("messages.seq 超出范围: {}", message.seq),
    })?;
    connection
        .execute(
            "INSERT INTO messages (id, session_id, run_id, client_msg_id, role, content, content_parts, \
             tool_calls, parent_message_id, seq, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, \
                     CASE WHEN ?10 > 0 THEN ?10 ELSE \
                       (SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE session_id = ?2) END, ?11)",
            params![
                message.id.as_str(),
                message.session_id.as_str(),
                message.run_id.as_ref().map(RunId::as_str),
                message.client_msg_id,
                message.role.as_str(),
                message.content,
                content_parts,
                tool_calls,
                message.parent_message_id.as_ref().map(MessageId::as_str),
                explicit_seq,
                message.created_at,
            ],
        )
        .map_err(txn_failed)
}

fn insert_run(connection: &Connection, run: &Run) -> Result<usize, StoreError> {
    connection
        .execute(
            "INSERT INTO runs (id, session_id, status, input_message_id, thinking_depth, error, started_at, finished_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                run.id.as_str(),
                run.session_id.as_str(),
                run.status.as_str(),
                run.input_message_id.as_ref().map(MessageId::as_str),
                run.thinking_depth.map(i64::from),
                run.error,
                run.started_at,
                run.finished_at,
            ],
        )
        .map_err(txn_failed)
}

fn find_run_by_input_message(
    connection: &Connection,
    message_id: &str,
) -> Result<Option<RunId>, StoreError> {
    let run_id: Option<String> = connection
        .query_row(
            "SELECT id FROM runs WHERE input_message_id = ?1 ORDER BY started_at DESC, rowid DESC LIMIT 1",
            [message_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(txn_failed)?;
    match run_id {
        Some(run_id) => RunId::new(run_id)
            .map(Some)
            .map_err(|error| invalid_row("runs.id", &error.to_string())),
        None => Ok(None),
    }
}

/// 行数据反序列化失败（库内数据与当前模型不兼容）。
fn invalid_row(field: &str, reason: &str) -> StoreError {
    StoreError::InvalidStoredEvent {
        id: field.to_owned(),
        reason: reason.to_owned(),
    }
}

fn json_text(value: &serde_json::Value) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|error| StoreError::WriteTransactionFailed {
        code: None,
        message: format!("JSON 序列化失败: {error}"),
    })
}

fn serialization_failed(field: &str, error: &serde_json::Error) -> StoreError {
    StoreError::WriteTransactionFailed {
        code: None,
        message: format!("{field} 序列化失败: {error}"),
    }
}

/// 写事务内 SQLite 错误（保留扩展码，供上层持久化降级/去重分类）。
fn txn_failed(error: rusqlite::Error) -> StoreError {
    match &error {
        rusqlite::Error::SqliteFailure(failure, message) => StoreError::WriteTransactionFailed {
            code: Some(failure.extended_code),
            message: message.clone().unwrap_or_else(|| error.to_string()),
        },
        _ => StoreError::WriteTransactionFailed {
            code: None,
            message: error.to_string(),
        },
    }
}

// ===== 读侧（单写者只约束写路径；读经 4 连接池，WAL 下与写并发）=====

/// 会话列表过滤（`session_list` 命令语义）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionQuery {
    pub runtime_id: Option<String>,
    pub status: Option<SessionStatus>,
    /// 父会话过滤（M2-05 父取消级联：按 `parent_session_id` 遍历子会话）。
    pub parent_session_id: Option<String>,
    pub limit: Option<u32>,
}

impl SessionQuery {
    /// 生成参数化 SQL（过滤条件全部绑定，不做字符串拼接）。
    fn sql(&self) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut clauses: Vec<&str> = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(runtime_id) = &self.runtime_id {
            clauses.push("runtime_id = ?");
            values.push(Box::new(runtime_id.clone()));
        }
        if let Some(status) = self.status {
            clauses.push("status = ?");
            values.push(Box::new(status.as_str().to_owned()));
        }
        if let Some(parent_session_id) = &self.parent_session_id {
            clauses.push("parent_session_id = ?");
            values.push(Box::new(parent_session_id.clone()));
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        let limit = i64::from(self.limit.unwrap_or(500).min(500));
        let sql = format!(
            "SELECT id, runtime_id, workspace_id, parent_session_id, title, status, model, system_prompt, \
             config, token_usage, created_at, updated_at, closed_at, thinking_depth, permission_mode, \
             preset_id \
             FROM sessions{where_clause} \
             ORDER BY created_at DESC, rowid DESC LIMIT ?"
        );
        values.push(Box::new(limit));
        (sql, values)
    }
}

impl ReadPool {
    /// 按 id 读取会话（不存在为 `None`）。
    pub async fn session(&self, session_id: &SessionId) -> Result<Option<Session>, StoreError> {
        let session = session_id.as_str().to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT id, runtime_id, workspace_id, parent_session_id, title, status, model, \
                     system_prompt, config, token_usage, created_at, updated_at, closed_at, thinking_depth, \
                     permission_mode, preset_id \
                     FROM sessions WHERE id = ?1",
                    [session.as_str()],
                    read_session_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 会话列表（`session_list` 语义；上限 500，D7）。
    pub async fn sessions(&self, query: SessionQuery) -> Result<Vec<Session>, StoreError> {
        self.with_connection(move |connection| {
            let (sql, values) = query.sql();
            let mut statement = connection.prepare(&sql)?;
            let bound: Vec<&dyn rusqlite::ToSql> =
                values.iter().map(|value| value.as_ref()).collect();
            let rows = statement.query_map(bound.as_slice(), read_session_row)?;
            let mut sessions = Vec::new();
            for row in rows {
                sessions.push(row?);
            }
            Ok(sessions)
        })
        .await
    }

    /// 会话全文检索（M7-07/ADR-025 §2.1-2：`session_search`）。
    ///
    /// - 标题与消息正文双 FTS5 命中；标题权重 ×5（`bm25` 越低越好）；
    /// - 同一会话多命中取最优一条（`ROW_NUMBER`），返回 `snippet` + `source`；
    /// - `query` 由调用方做长度/空值校验；本层按 FTS5 **短语查询**转义，
    ///   不把用户输入当 FTS 语法执行（防注入）。
    pub async fn search_sessions(
        &self,
        query: &str,
        limit: u32,
        offset: u64,
    ) -> Result<Vec<SessionSearchHit>, StoreError> {
        let phrase = format!("\"{}\"", query.replace('"', "\"\""));
        let limit = i64::from(limit);
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "WITH hits AS ( \
                   SELECT s.id AS session_id, \
                          snippet(sessions_fts, 0, '[', ']', '…', 32) AS snippet, \
                          bm25(sessions_fts) * 5.0 AS rank, \
                          'title' AS source \
                     FROM sessions_fts \
                     JOIN sessions s ON s.rowid = sessions_fts.rowid \
                    WHERE sessions_fts MATCH ?1 \
                   UNION ALL \
                   SELECT s.id, \
                          snippet(messages_fts, 0, '[', ']', '…', 32), \
                          bm25(messages_fts), \
                          'message' \
                     FROM messages_fts \
                     JOIN messages m ON m.rowid = messages_fts.rowid \
                     JOIN sessions s ON s.id = m.session_id \
                    WHERE messages_fts MATCH ?1 \
                 ), ranked AS ( \
                   SELECT session_id, snippet, source, rank, \
                          ROW_NUMBER() OVER ( \
                            PARTITION BY session_id ORDER BY rank ASC, source ASC \
                          ) AS rn \
                     FROM hits \
                 ) \
                 SELECT s.id, s.runtime_id, s.workspace_id, s.parent_session_id, s.title, \
                        s.status, s.model, s.system_prompt, s.config, s.token_usage, \
                        s.created_at, s.updated_at, s.closed_at, s.thinking_depth, \
                        s.permission_mode, s.preset_id, r.snippet, r.source \
                   FROM ranked r \
                   JOIN sessions s ON s.id = r.session_id \
                  WHERE r.rn = 1 \
                  ORDER BY r.rank ASC, s.updated_at DESC, s.id DESC \
                  LIMIT ?2 OFFSET ?3",
            )?;
            let rows = statement.query_map(params![phrase, limit, offset], |row| {
                Ok(SessionSearchHit {
                    session: read_session_row(row)?,
                    snippet: row.get(16)?,
                    source: row.get(17)?,
                })
            })?;
            let mut hits = Vec::new();
            for row in rows {
                hits.push(row?);
            }
            Ok(hits)
        })
        .await
    }

    /// 运行时启停偏好快照（M7-01/ADR-015：`runtimes.enabled`；启动矩阵与
    /// `runtimes_list.enabled` 的唯一读路径）。表可为空（无注册行）。
    pub async fn runtime_preferences(&self) -> Result<BTreeMap<String, bool>, StoreError> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare("SELECT id, enabled FROM runtimes")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
            })?;
            let mut preferences = BTreeMap::new();
            for row in rows {
                let (id, enabled) = row?;
                preferences.insert(id, enabled);
            }
            Ok(preferences)
        })
        .await
    }

    /// 在途工作量计数（M7-01/ADR-015 §2.1#2，禁用确认门与 `active_sessions` 同一读路径口径）：
    /// 会话处于 `creating`/`running`/`paused`/`waiting_permission`，**或**存在
    /// `queued`/`running` run（终态不计）。
    pub async fn active_work_counts(&self) -> Result<BTreeMap<String, u32>, StoreError> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT runtime_id, COUNT(*) FROM ( \
                   SELECT runtime_id FROM sessions \
                    WHERE status IN ('creating','running','paused','waiting_permission') \
                   UNION ALL \
                   SELECT sessions.runtime_id FROM runs \
                     JOIN sessions ON sessions.id = runs.session_id \
                    WHERE runs.status IN ('queued','running') \
                 ) GROUP BY runtime_id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            let mut counts = BTreeMap::new();
            for row in rows {
                let (runtime_id, count) = row?;
                counts.insert(runtime_id, u32::try_from(count).unwrap_or(u32::MAX));
            }
            Ok(counts)
        })
        .await
    }

    /// 会话是否存在在途 run（`queued` / `running`；M7-08 改派前置检查：ADR-026
    /// §2.1-1「在途 run 或 waiting_permission → session_busy」）。
    pub async fn session_has_active_run(
        &self,
        session_id: &SessionId,
    ) -> Result<bool, StoreError> {
        let session = session_id.as_str().to_owned();
        self.with_connection(move |connection| {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM runs WHERE session_id = ?1 AND status IN ('queued','running')",
                    [session.as_str()],
                    |row| row.get(0),
                )
                .map_err(StoreError::from)?;
            Ok(count > 0)
        })
        .await
    }

    /// 按 id 读取 run（不存在为 `None`）。
    pub async fn run(&self, run_id: &RunId) -> Result<Option<Run>, StoreError> {        let run = run_id.as_str().to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT id, session_id, status, input_message_id, thinking_depth, error, started_at, finished_at \
                     FROM runs WHERE id = ?1",
                    [run.as_str()],
                    read_run_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 按 id 读取消息（不存在为 `None`）。
    ///
    /// M3-06：`run_retry` 读取原 run 输入消息正文（重放输入）与 T4 已确认消息零丢失
    /// 校验均经本方法（只读）。
    pub async fn message(&self, message_id: &MessageId) -> Result<Option<Message>, StoreError> {
        let message = message_id.as_str().to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT id, session_id, run_id, client_msg_id, role, content, content_parts, tool_calls, \
                     parent_message_id, seq, created_at FROM messages WHERE id = ?1",
                    [message.as_str()],
                    read_message_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 未收口 run（`queued` / `running`）清单（M3-06 重启状态重建：核心重启后按
    /// 「运行中崩溃 → 在途 run 标 failed（可重试）」收口，D5 失败场景口径）。
    pub async fn unfinished_runs(&self) -> Result<Vec<Run>, StoreError> {
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, session_id, status, input_message_id, thinking_depth, error, started_at, finished_at \
                 FROM runs WHERE status IN ('queued', 'running') ORDER BY started_at ASC, rowid ASC",
            )?;
            let rows = statement.query_map([], read_run_row)?;
            let mut runs = Vec::new();
            for row in rows {
                runs.push(row?);
            }
            Ok(runs)
        })
        .await
    }

    /// 非终态、非空闲会话清单（M3-06 重启状态重建：核心重启后在途会话回 `idle`，
    /// 与 `unfinished_runs` 收口配套；`paused` 在 P0 不可达但一并收口）。
    pub async fn sessions_for_recovery(&self) -> Result<Vec<Session>, StoreError> {
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, runtime_id, workspace_id, parent_session_id, title, status, model, \
                 system_prompt, config, token_usage, created_at, updated_at, closed_at, thinking_depth, \
                 permission_mode, preset_id \
                 FROM sessions \
                 WHERE status NOT IN ('idle', 'completed', 'failed', 'cancelled') \
                 ORDER BY created_at ASC, rowid ASC",
            )?;
            let rows = statement.query_map([], read_session_row)?;
            let mut sessions = Vec::new();
            for row in rows {
                sessions.push(row?);
            }
            Ok(sessions)
        })
        .await
    }

    /// 按幂等键查消息（ADR-005；重启后重放查询用）。
    pub async fn message_by_client_msg_id(
        &self,
        session_id: &SessionId,
        client_msg_id: &str,
    ) -> Result<Option<Message>, StoreError> {
        let session = session_id.as_str().to_owned();
        let client = client_msg_id.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT id, session_id, run_id, client_msg_id, role, content, content_parts, tool_calls, \
                     parent_message_id, seq, created_at FROM messages \
                     WHERE session_id = ?1 AND client_msg_id = ?2",
                    params![session, client],
                    read_message_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 消息分页（升序；`after_seq = None` 从会话起点读取）。
    pub async fn messages_page(
        &self,
        session_id: &SessionId,
        after_seq: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Message>, StoreError> {
        let session = session_id.as_str().to_owned();
        let after = match after_seq {
            Some(value) => Some(i64::try_from(value).map_err(|_| StoreError::Internal {
                reason: format!("after_seq 超出 SQLite INTEGER 范围: {value}"),
            })?),
            None => None,
        };
        let limit = i64::try_from(limit.min(500)).map_err(|_| StoreError::Internal {
            reason: "分页上限转换失败".to_owned(),
        })?;
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, session_id, run_id, client_msg_id, role, content, content_parts, tool_calls, \
                 parent_message_id, seq, created_at FROM messages \
                 WHERE session_id = ?1 AND (?2 IS NULL OR seq > ?2) ORDER BY seq ASC LIMIT ?3",
            )?;
            let rows = statement.query_map(params![session, after, limit], read_message_row)?;
            let mut messages = Vec::new();
            for row in rows {
                messages.push(row?);
            }
            Ok(messages)
        })
        .await
    }

    /// 待审批清单（`permissions_pending` 命令语义；可按会话过滤）。
    pub async fn permissions_pending(
        &self,
        session_id: Option<&SessionId>,
    ) -> Result<Vec<PermissionRecord>, StoreError> {
        let session = session_id.map(|id| id.as_str().to_owned());
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, session_id, request_id, resource, action, target, decision, scope, status, \
                 requested_at, resolved_at, resolver FROM permissions \
                 WHERE status = 'pending' AND (?1 IS NULL OR session_id = ?1) \
                 ORDER BY requested_at ASC, rowid ASC",
            )?;
            let rows = statement.query_map(params![session], read_permission_row)?;
            let mut records = Vec::new();
            for row in rows {
                records.push(row?);
            }
            Ok(records)
        })
        .await
    }

    /// 按 id 读取权限行。
    pub async fn permission(&self, id: &str) -> Result<Option<PermissionRecord>, StoreError> {
        let id = id.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT id, session_id, request_id, resource, action, target, decision, scope, status, \
                     requested_at, resolved_at, resolver FROM permissions WHERE id = ?1",
                    [id.as_str()],
                    read_permission_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 用户自定义权限规则清单（M6-01 `rules_list` 语义；按创建时间升序、`rowid` 兜底）。
    pub async fn permission_rules(&self) -> Result<Vec<PermissionRuleRecord>, StoreError> {
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, name, resource, action, target_kind, target, decision, priority, enabled, \
                 created_at, updated_at FROM permission_rules ORDER BY created_at ASC, rowid ASC",
            )?;
            let rows = statement.query_map([], read_permission_rule_row)?;
            let mut rules = Vec::new();
            for row in rows {
                rules.push(row?);
            }
            Ok(rules)
        })
        .await
    }

    /// 持久授权行（M6-01/ADR-019 附录 A-2 重启重建：`scope IN ('session','always')`
    /// + `status='resolved'` + `decision='allow'` + `target` 非空）。
    pub async fn permission_grants(&self) -> Result<Vec<PermissionGrantRecord>, StoreError> {
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT session_id, resource, action, target FROM permissions \
                 WHERE scope IN ('session','always') AND status = 'resolved' AND decision = 'allow' \
                 AND target IS NOT NULL ORDER BY requested_at ASC, rowid ASC",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(PermissionGrantRecord {
                    session_id: row.get(0)?,
                    resource: row.get(1)?,
                    action: row.get(2)?,
                    target: row.get(3)?,
                })
            })?;
            let mut grants = Vec::new();
            for row in rows {
                grants.push(row?);
            }
            Ok(grants)
        })
        .await
    }

    /// 会话引用清单（M3-09 `artifacts_list` 语义；按 `created_at` 升序、
    /// `rowid` 兜底稳定顺序）。
    pub async fn artifacts(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ArtifactRecord>, StoreError> {
        let session = session_id.as_str().to_owned();
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, session_id, path, kind, size_bytes, created_at FROM artifacts \
                 WHERE session_id = ?1 ORDER BY created_at ASC, rowid ASC",
            )?;
            let rows = statement.query_map([session.as_str()], read_artifact_row)?;
            let mut artifacts = Vec::new();
            for row in rows {
                artifacts.push(row?);
            }
            Ok(artifacts)
        })
        .await
    }

    /// 供应商清单（M3-11 `providers_list` 语义；按 `created_at` 升序、`rowid` 兜底）。
    pub async fn providers(&self) -> Result<Vec<ProviderRecord>, StoreError> {
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, name, type, base_url, api_key_ref, enabled, is_builtin, created_at, updated_at, endpoints \
                 FROM providers ORDER BY created_at ASC, rowid ASC",
            )?;
            let rows = statement.query_map([], read_provider_row)?;
            let mut providers = Vec::new();
            for row in rows {
                providers.push(row?);
            }
            Ok(providers)
        })
        .await
    }

    /// 按 id 读取供应商（不存在为 `None`）。
    pub async fn provider(&self, id: &str) -> Result<Option<ProviderRecord>, StoreError> {
        let id = id.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT id, name, type, base_url, api_key_ref, enabled, is_builtin, created_at, updated_at, endpoints \
                     FROM providers WHERE id = ?1",
                    [id.as_str()],
                    read_provider_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 供应商模型清单（M3-11 `providers_list`；全量按 `created_at` 升序、`rowid` 兜底；
    /// 调用方按 `provider_id` 归组）。
    pub async fn provider_models(&self) -> Result<Vec<ProviderModelRecord>, StoreError> {
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, provider_id, model_id, display_name, enabled, created_at \
                 FROM provider_models ORDER BY created_at ASC, rowid ASC",
            )?;
            let rows = statement.query_map([], read_provider_model_row)?;
            let mut models = Vec::new();
            for row in rows {
                models.push(row?);
            }
            Ok(models)
        })
        .await
    }

    /// 按 `(provider_id, model_id)` 读取供应商模型（不存在为 `None`；M3-11
    /// `provider_model_toggle` 的存在性判定）。
    pub async fn provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<Option<ProviderModelRecord>, StoreError> {
        let provider_id = provider_id.to_owned();
        let model_id = model_id.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT id, provider_id, model_id, display_name, enabled, created_at \
                     FROM provider_models WHERE provider_id = ?1 AND model_id = ?2",
                    params![provider_id, model_id],
                    read_provider_model_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 审计查询（诊断/验收断言；按时间升序）。
    pub async fn audit_log(&self, limit: usize) -> Result<Vec<AuditLogRecord>, StoreError> {
        let limit = i64::try_from(limit.min(5_000)).map_err(|_| StoreError::Internal {
            reason: "审计查询上限转换失败".to_owned(),
        })?;
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, session_id, runtime_id, actor, action, resource, detail, result, ts \
                 FROM audit_log ORDER BY ts ASC, rowid ASC LIMIT ?1",
            )?;
            let rows = statement.query_map([limit], read_audit_row)?;
            let mut records = Vec::new();
            for row in rows {
                records.push(row?);
            }
            Ok(records)
        })
        .await
    }

    /// 审计查询（M6-03 `audit_query`；ADR-023 §2.1-3 / v0.3 分页设计修订）。
    ///
    /// 等值过滤（会话/actor/action/resource）+ 闭区间时间 + `offset`/`limit` 分页；
    /// 稳定排序 `ts DESC, id DESC`；不执行 `COUNT(*)`，以「多取一行」判定
    /// `has_more`（1e6 行下已过滤分页查询 P95 ≤300ms；见任务证据）。
    pub async fn audit_query(
        &self,
        query: AuditQuery,
    ) -> Result<AuditQueryResult, StoreError> {
        self.with_connection(move |connection| {
            let (where_sql, values) = audit_filter_sql(&query);
            let limit = query
                .limit
                .unwrap_or(AUDIT_QUERY_DEFAULT_LIMIT)
                .min(AUDIT_QUERY_MAX_LIMIT);
            // 多取一行用于 has_more 探测（不额外回表/计数）。
            let probe_limit = limit.saturating_add(1);
            let limit_sql = i64::try_from(probe_limit).map_err(|_| StoreError::Internal {
                reason: "审计分页上限转换失败".to_owned(),
            })?;
            let offset_sql = i64::try_from(query.offset).map_err(|_| StoreError::Internal {
                reason: "审计分页偏移转换失败".to_owned(),
            })?;
            let mut statement = connection.prepare(&format!(
                "SELECT id, session_id, runtime_id, actor, action, resource, detail, result, ts \
                 FROM audit_log{where_sql} ORDER BY ts DESC, id DESC LIMIT ? OFFSET ?"
            ))?;
            let rows = statement.query_map(
                rusqlite::params_from_iter(
                    values
                        .iter()
                        .cloned()
                        .chain([
                            rusqlite::types::Value::Integer(limit_sql),
                            rusqlite::types::Value::Integer(offset_sql),
                        ]),
                ),
                read_audit_row,
            )?;
            let mut records = Vec::new();
            for row in rows {
                records.push(row?);
            }
            let has_more = records.len() > limit;
            if has_more {
                records.truncate(limit);
            }
            Ok(AuditQueryResult {
                rows: records,
                limit,
                offset: query.offset,
                has_more,
            })
        })
        .await
    }

    /// 备份台账清单（M3-04 `backup_list`；按 `created_at` 降序 = 最新在前）。
    pub async fn backups(&self) -> Result<Vec<BackupRecord>, StoreError> {
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, path, size_bytes, encrypted, kind, created_at FROM backups \
                 ORDER BY created_at DESC, rowid DESC",
            )?;
            let rows = statement.query_map([], read_backup_row)?;
            let mut records = Vec::new();
            for row in rows {
                records.push(row?);
            }
            Ok(records)
        })
        .await
    }

    /// 按 id 读取备份台账行（M3-04 `backup_restore` 内部来源；不存在为 `None`）。
    pub async fn backup(&self, id: &str) -> Result<Option<BackupRecord>, StoreError> {
        let id = id.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT id, path, size_bytes, encrypted, kind, created_at FROM backups WHERE id = ?1",
                    [id.as_str()],
                    read_backup_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 各状态会话计数（诊断/验收断言）。
    pub async fn session_status_counts(&self) -> Result<BTreeMap<String, u64>, StoreError> {
        self.with_connection(move |connection| {
            let mut statement =
                connection.prepare("SELECT status, COUNT(*) FROM sessions GROUP BY status")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            let mut counts = BTreeMap::new();
            for row in rows {
                let (status, count) = row?;
                counts.insert(status, u64::try_from(count).unwrap_or(0));
            }
            Ok(counts)
        })
        .await
    }

    /// 读取单个设置项（M3-05；不存在为 `None`；值按 JSON 文本原样返回）。
    pub async fn setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        let key = key.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT value FROM settings WHERE key = ?1",
                    [key.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 全量设置项（诊断包「脱敏配置」段；按 key 升序保证输出稳定）。
    pub async fn settings(&self) -> Result<Vec<(String, String)>, StoreError> {
        self.with_connection(move |connection| {
            let mut statement =
                connection.prepare("SELECT key, value FROM settings ORDER BY key ASC")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut settings = Vec::new();
            for row in rows {
                settings.push(row?);
            }
            Ok(settings)
        })
        .await
    }

    /// 全部工作区（M7-08 `workspaces_list`；按 `updated_at` 降序、`rowid` 降序稳定排序）。
    pub async fn workspaces(&self) -> Result<Vec<Workspace>, StoreError> {
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, name, root_path, memory_files, created_at, updated_at \
                 FROM workspaces ORDER BY updated_at DESC, rowid DESC",
            )?;
            let rows = statement.query_map([], read_workspace_row)?;
            let mut workspaces = Vec::new();
            for row in rows {
                workspaces.push(row?);
            }
            Ok(workspaces)
        })
        .await
    }

    /// 按 id 读取工作区（不存在为 `None`；M3-08 `workspace_set` 的 `workspace_id` 分支）。
    pub async fn workspace(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<Workspace>, StoreError> {
        let id = workspace_id.as_str().to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT id, name, root_path, memory_files, created_at, updated_at \
                     FROM workspaces WHERE id = ?1",
                    [id.as_str()],
                    read_workspace_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 按 root_path 读取工作区（canonical 路径精确匹配；不存在为 `None`）。
    ///
    /// M3-08：`workspace_set` 的 `root_path` 分支按 canonical 路径复用既有行
    /// （同一目录重复绑定幂等）。
    pub async fn workspace_by_root(
        &self,
        root_path: &str,
    ) -> Result<Option<Workspace>, StoreError> {
        let root = root_path.to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT id, name, root_path, memory_files, created_at, updated_at \
                     FROM workspaces WHERE root_path = ?1 LIMIT 1",
                    [root.as_str()],
                    read_workspace_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 最近绑定的工作区（启动恢复当前工作区；按 `updated_at` 降序取首行）。
    pub async fn workspaces_latest(&self) -> Result<Option<Workspace>, StoreError> {
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT id, name, root_path, memory_files, created_at, updated_at \
                     FROM workspaces ORDER BY updated_at DESC, rowid DESC LIMIT 1",
                    [],
                    read_workspace_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 工作区会话计数快照（M7-08 `workspaces_list`；ADR-026 §2.1-1）。
    ///
    /// 聚合口径：按 `sessions.workspace_id` 分组；`active` = 在途口径（会话状态
    /// `creating`/`running`/`paused`/`waiting_permission`，或存在 `queued`/`running`
    /// run——与 [`Self::active_work_counts`] 同源）；`total` 含终态。
    /// `workspace_id IS NULL` 归入合成组（不建 `workspaces` 行）。
    pub async fn workspace_session_counts(&self) -> Result<WorkspaceSessionCounts, StoreError> {
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT workspace_id, COUNT(*) AS total, \
                   SUM(CASE WHEN status IN ('creating','running','paused','waiting_permission') \
                              OR EXISTS (SELECT 1 FROM runs \
                                           WHERE runs.session_id = sessions.id \
                                             AND runs.status IN ('queued','running')) \
                        THEN 1 ELSE 0 END) AS active \
                 FROM sessions GROUP BY workspace_id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })?;
            let mut counts = WorkspaceSessionCounts::default();
            for row in rows {
                let (workspace_id, total, active) = row?;
                let count = WorkspaceSessionCount {
                    active: u64::try_from(active.unwrap_or(0)).unwrap_or(u64::MAX),
                    total: u64::try_from(total).unwrap_or(u64::MAX),
                };
                match workspace_id {
                    Some(workspace_id) => {
                        counts.by_workspace.insert(workspace_id, count);
                    }
                    None => counts.unassigned = count,
                }
            }
            Ok(counts)
        })
        .await
    }

    /// Agent 预设清单（M7-09 `presets_list` 语义；内置在前、按 `created_at` 升序、
    /// `rowid` 兜底稳定顺序——内置播种先于自定义行，故自然置前）。
    pub async fn presets(&self) -> Result<Vec<Preset>, StoreError> {
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, name, system_prompt, is_builtin, created_at, updated_at \
                 FROM presets ORDER BY is_builtin DESC, created_at ASC, rowid ASC",
            )?;
            let rows = statement.query_map([], read_preset_row)?;
            let mut presets = Vec::new();
            for row in rows {
                presets.push(row?);
            }
            Ok(presets)
        })
        .await
    }

    /// 按 id 读取预设（不存在为 `None`；`preset_update`/`preset_delete`/`session_create`
    /// 的存在性判定）。
    pub async fn preset(&self, preset_id: &PresetId) -> Result<Option<Preset>, StoreError> {
        let id = preset_id.as_str().to_owned();
        self.with_connection(move |connection| {
            connection
                .query_row(
                    "SELECT id, name, system_prompt, is_builtin, created_at, updated_at \
                     FROM presets WHERE id = ?1",
                    [id.as_str()],
                    read_preset_row,
                )
                .optional()
                .map_err(StoreError::from)
        })
        .await
    }

    /// 库摘要（诊断包「库摘要」段；M3-05 DoD1/DoD4）：
    /// `schema_migrations` 最大版本 + 关键表行数（表名升序，便于扫描/比对）。
    pub async fn store_summary(&self) -> Result<StoreSummary, StoreError> {
        self.with_connection(move |connection| {
            let schema_version: Option<i64> = connection
                .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                    row.get(0)
                })
                .optional()?;
            let mut tables = BTreeMap::new();
            for table in SUMMARY_TABLES {
                let count: i64 =
                    connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
                tables.insert((*table).to_owned(), count);
            }
            Ok(StoreSummary {
                schema_version,
                tables,
            })
        })
        .await
    }
}

/// 诊断库摘要涉及的表（清单；输出按键名升序，与 [`StoreSummary::tables`] 一致）。
pub const SUMMARY_TABLES: &[&str] = &[
    "events",
    "sessions",
    "runs",
    "messages",
    "permissions",
    "audit_log",
    "backups",
    "settings",
];

/// 库摘要（诊断包；M3-05）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StoreSummary {
    /// `schema_migrations` 最大版本（无迁移记录为 `None`）。
    pub schema_version: Option<i64>,
    /// 关键表行数（表名 → 行数；键名升序）。
    pub tables: BTreeMap<String, i64>,
}

fn read_backup_row(row: &Row<'_>) -> rusqlite::Result<BackupRecord> {
    let size: i64 = row.get(2)?;
    let size_bytes = u64::try_from(size).map_err(|_| parse_column_error(2, "size_bytes"))?;
    Ok(BackupRecord {
        id: row.get(0)?,
        path: std::path::PathBuf::from(row.get::<_, String>(1)?),
        size_bytes,
        encrypted: row.get::<_, i64>(3)? != 0,
        kind: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn read_workspace_row(row: &Row<'_>) -> rusqlite::Result<Workspace> {
    let memory_files: String = row.get(3)?;
    Ok(Workspace {
        id: WorkspaceId::new(row.get::<_, String>(0)?).map_err(|_| parse_column_error(0, "id"))?,
        name: row.get(1)?,
        root_path: row.get(2)?,
        memory_files: serde_json::from_str(&memory_files)
            .map_err(|_| parse_column_error(3, "memory_files"))?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn read_preset_row(row: &Row<'_>) -> rusqlite::Result<Preset> {
    Ok(Preset {
        id: PresetId::new(row.get::<_, String>(0)?).map_err(|_| parse_column_error(0, "id"))?,
        name: row.get(1)?,
        system_prompt: row.get(2)?,
        is_builtin: row.get::<_, i64>(3)? != 0,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn read_session_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    let status: String = row.get(5)?;
    let status = status
        .parse::<SessionStatus>()
        .map_err(|_| parse_column_error(5, "status"))?;
    let config: String = row.get(8)?;
    let token_usage: String = row.get(9)?;
    Ok(Session {
        id: SessionId::new(row.get::<_, String>(0)?).map_err(|_| parse_column_error(0, "id"))?,
        runtime_id: aether_core::RuntimeId::new(row.get::<_, String>(1)?)
            .map_err(|_| parse_column_error(1, "runtime_id"))?,
        workspace_id: row
            .get::<_, Option<String>>(2)?
            .map(|value| WorkspaceId::new(value).map_err(|_| parse_column_error(2, "workspace_id")))
            .transpose()?,
        parent_session_id: row
            .get::<_, Option<String>>(3)?
            .map(|value| {
                SessionId::new(value).map_err(|_| parse_column_error(3, "parent_session_id"))
            })
            .transpose()?,
        title: row.get(4)?,
        status,
        model: row.get(6)?,
        system_prompt: row.get(7)?,
        config: serde_json::from_str(&config).map_err(|_| parse_column_error(8, "config"))?,
        token_usage: serde_json::from_str(&token_usage)
            .map_err(|_| parse_column_error(9, "token_usage"))?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        closed_at: row.get(12)?,
        thinking_depth: u8::try_from(row.get::<_, i64>(13)?)
            .map_err(|_| parse_column_error(13, "thinking_depth"))?,
        permission_mode: {
            let value: String = row.get(14)?;
            PermissionMode::from_code(&value)
                .ok_or_else(|| parse_column_error(14, "permission_mode"))?
        },
        preset_id: row
            .get::<_, Option<String>>(15)?
            .map(|value| PresetId::new(value).map_err(|_| parse_column_error(15, "preset_id")))
            .transpose()?,
    })
}

fn read_run_row(row: &Row<'_>) -> rusqlite::Result<Run> {
    let status: String = row.get(2)?;
    let status = status
        .parse::<RunStatus>()
        .map_err(|_| parse_column_error(2, "status"))?;
    Ok(Run {
        id: RunId::new(row.get::<_, String>(0)?).map_err(|_| parse_column_error(0, "id"))?,
        session_id: SessionId::new(row.get::<_, String>(1)?)
            .map_err(|_| parse_column_error(1, "session_id"))?,
        status,
        input_message_id: row
            .get::<_, Option<String>>(3)?
            .map(|value| {
                MessageId::new(value).map_err(|_| parse_column_error(3, "input_message_id"))
            })
            .transpose()?,
        thinking_depth: row
            .get::<_, Option<i64>>(4)?
            .map(|value| u8::try_from(value).map_err(|_| parse_column_error(4, "thinking_depth")))
            .transpose()?,
        error: row.get(5)?,
        started_at: row.get(6)?,
        finished_at: row.get(7)?,
    })
}

pub(crate) fn read_message_row(row: &Row<'_>) -> rusqlite::Result<Message> {
    let role: String = row.get(4)?;
    let role = role
        .parse::<MessageRole>()
        .map_err(|_| parse_column_error(4, "role"))?;
    let seq: i64 = row.get(9)?;
    Ok(Message {
        id: MessageId::new(row.get::<_, String>(0)?).map_err(|_| parse_column_error(0, "id"))?,
        session_id: SessionId::new(row.get::<_, String>(1)?)
            .map_err(|_| parse_column_error(1, "session_id"))?,
        run_id: row
            .get::<_, Option<String>>(2)?
            .map(|value| RunId::new(value).map_err(|_| parse_column_error(2, "run_id")))
            .transpose()?,
        client_msg_id: row.get(3)?,
        role,
        content: row.get(5)?,
        content_parts: parse_optional_json(row.get::<_, Option<String>>(6)?, 6, "content_parts")?,
        tool_calls: parse_optional_json(row.get::<_, Option<String>>(7)?, 7, "tool_calls")?,
        parent_message_id: row
            .get::<_, Option<String>>(8)?
            .map(|value| {
                MessageId::new(value).map_err(|_| parse_column_error(8, "parent_message_id"))
            })
            .transpose()?,
        seq: u64::try_from(seq).map_err(|_| parse_column_error(9, "seq"))?,
        created_at: row.get(10)?,
    })
}

fn read_permission_row(row: &Row<'_>) -> rusqlite::Result<PermissionRecord> {
    let decision: String = row.get(6)?;
    let decision = decision
        .parse::<PermissionDecision>()
        .map_err(|_| parse_column_error(6, "decision"))?;
    let scope = row
        .get::<_, Option<String>>(7)?
        .map(|value| PermissionScope::from_str(&value).map_err(|_| parse_column_error(7, "scope")))
        .transpose()?;
    let status: String = row.get(8)?;
    let status = status
        .parse::<PermissionStatus>()
        .map_err(|_| parse_column_error(8, "status"))?;
    Ok(PermissionRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        request_id: row.get(2)?,
        resource: row.get(3)?,
        action: row.get(4)?,
        target: row.get(5)?,
        decision,
        scope,
        status,
        requested_at: row.get(9)?,
        resolved_at: row.get(10)?,
        resolver: row.get(11)?,
    })
}

fn read_permission_rule_row(row: &Row<'_>) -> rusqlite::Result<PermissionRuleRecord> {
    Ok(PermissionRuleRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        resource: row.get(2)?,
        action: row.get(3)?,
        target_kind: row.get(4)?,
        target: row.get(5)?,
        decision: row.get(6)?,
        priority: row.get(7)?,
        enabled: row.get::<_, i64>(8)? != 0,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn read_artifact_row(row: &Row<'_>) -> rusqlite::Result<ArtifactRecord> {
    Ok(ArtifactRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        path: row.get(2)?,
        kind: row.get(3)?,
        size_bytes: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn read_provider_row(row: &Row<'_>) -> rusqlite::Result<ProviderRecord> {
    Ok(ProviderRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        provider_type: row.get(2)?,
        base_url: row.get(3)?,
        api_key_ref: row.get(4)?,
        enabled: row.get::<_, i64>(5)? != 0,
        is_builtin: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        endpoints: row.get(9)?,
    })
}

fn read_provider_model_row(row: &Row<'_>) -> rusqlite::Result<ProviderModelRecord> {
    Ok(ProviderModelRecord {
        id: row.get(0)?,
        provider_id: row.get(1)?,
        model_id: row.get(2)?,
        display_name: row.get(3)?,
        enabled: row.get::<_, i64>(4)? != 0,
        created_at: row.get(5)?,
    })
}

fn parse_optional_json(
    value: Option<String>,
    index: usize,
    column: &str,
) -> rusqlite::Result<Option<serde_json::Value>> {
    match value {
        Some(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|_| parse_column_error(index, column)),
        None => Ok(None),
    }
}

/// 列解析失败（库内数据与当前模型不兼容；不 panic）。
fn parse_column_error(index: usize, column: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(
        index,
        format!("{column} 解析失败"),
        rusqlite::types::Type::Text,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use aether_core::{TokenUsage, WorkspaceId};

    fn migrated_connection() -> Connection {
        let mut connection = Connection::open_in_memory().unwrap();
        crate::migration::migrate(&mut connection).unwrap();
        connection
    }

    fn runtime(id: &str) -> Runtime {
        Runtime {
            id: aether_core::RuntimeId::new(id).unwrap(),
            name: "Mock".to_owned(),
            kind: "mock".to_owned(),
            version: "0.1.0".to_owned(),
            protocol: "1.0".to_owned(),
            capabilities: vec![],
            endpoint: None,
            config: serde_json::json!({}),
            status: aether_core::RuntimeStatus::Ready,
            status_reason: None,
            last_seen_at: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn session(id: &str) -> Session {
        Session {
            id: SessionId::new(id).unwrap(),
            runtime_id: aether_core::RuntimeId::new("mock").unwrap(),
            workspace_id: Some(WorkspaceId::new("ws").unwrap()),
            parent_session_id: None,
            title: "t".to_owned(),
            status: SessionStatus::Idle,
            model: None,
            thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
            permission_mode: PermissionMode::default(),
            preset_id: None,
            system_prompt: None,
            config: serde_json::json!({}),
            token_usage: TokenUsage::default(),
            created_at: 10,
            updated_at: 10,
            closed_at: None,
        }
    }

    fn user_message(id: &str, session_id: &str, client: &str) -> Message {
        Message {
            id: MessageId::new(id).unwrap(),
            session_id: SessionId::new(session_id).unwrap(),
            run_id: None,
            client_msg_id: Some(client.to_owned()),
            role: MessageRole::User,
            content: "hi".to_owned(),
            content_parts: None,
            tool_calls: None,
            parent_message_id: None,
            seq: 0,
            created_at: 11,
        }
    }

    fn run_record(id: &str, session_id: &str, message_id: &str) -> Run {
        Run {
            id: RunId::new(id).unwrap(),
            session_id: SessionId::new(session_id).unwrap(),
            status: RunStatus::Queued,
            input_message_id: Some(MessageId::new(message_id).unwrap()),
            thinking_depth: None,
            error: None,
            started_at: 11,
            finished_at: None,
        }
    }

    fn seed_runtime_session(connection: &mut Connection) {
        connection
            .execute(
                "INSERT INTO workspaces (id, name, root_path, memory_files, created_at, updated_at) \
                 VALUES ('ws', 'ws', 'C:/ws', '[]', 1, 1)",
                [],
            )
            .unwrap();
        apply_command(
            connection,
            &StoreCommand::EnsureRuntime {
                runtime: runtime("mock"),
            },
        )
        .unwrap();
        apply_command(
            connection,
            &StoreCommand::InsertSession {
                session: session("01J0000000000000000000000A"),
            },
        )
        .unwrap();
    }

    #[test]
    fn begin_run_idempotent_dedupes_in_same_and_reopened_database() {
        let mut connection = migrated_connection();
        seed_runtime_session(&mut connection);
        let message = user_message(
            "01J000000000000000000000M1",
            "01J0000000000000000000000A",
            "01J000000000000000000000CM",
        );
        let run_record_value = run_record(
            "01J000000000000000000000R1",
            "01J0000000000000000000000A",
            "01J000000000000000000000M1",
        );
        let first = apply_command(
            &mut connection,
            &StoreCommand::BeginRunIdempotent {
                message: message.clone(),
                run: run_record_value.clone(),
            },
        )
        .unwrap();
        assert_eq!(
            first,
            StoreOutcome::RunAccepted {
                message_id: message.id.clone(),
                run_id: run_record_value.id.clone(),
                duplicate: false,
            }
        );

        // 同值重放（新 id 同 client_msg_id）：返回既有行，不新增。
        let replay = apply_command(
            &mut connection,
            &StoreCommand::BeginRunIdempotent {
                message: user_message(
                    "01J000000000000000000000M2",
                    "01J0000000000000000000000A",
                    "01J000000000000000000000CM",
                ),
                run: run_record(
                    "01J000000000000000000000R2",
                    "01J0000000000000000000000A",
                    "01J000000000000000000000M2",
                ),
            },
        )
        .unwrap();
        assert_eq!(
            replay,
            StoreOutcome::RunAccepted {
                message_id: message.id.clone(),
                run_id: run_record_value.id.clone(),
                duplicate: true,
            }
        );
        let message_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        let run_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!((message_count, run_count), (1, 1));
    }

    #[test]
    fn message_seq_is_auto_assigned_per_session() {
        let mut connection = migrated_connection();
        seed_runtime_session(&mut connection);
        for (index, (message_id, client)) in [
            ("01J000000000000000000000M1", "01J000000000000000000000C1"),
            ("01J000000000000000000000M2", "01J000000000000000000000C2"),
        ]
        .into_iter()
        .enumerate()
        {
            let run_id = format!("01J000000000000000000000R{}", index + 1);
            apply_command(
                &mut connection,
                &StoreCommand::BeginRunIdempotent {
                    message: user_message(message_id, "01J0000000000000000000000A", client),
                    run: run_record(&run_id, "01J0000000000000000000000A", message_id),
                },
            )
            .unwrap();
        }
        let mut statement = connection
            .prepare("SELECT seq FROM messages ORDER BY created_at, rowid")
            .unwrap();
        let seqs: Vec<i64> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(seqs, vec![1, 2]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovery_reads_expose_unfinished_runs_and_non_idle_sessions() {
        use crate::store::Store;
        use crate::write_queue::{StoreRuntime, WriteQueueConfig};

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("aether.db");
        drop(Store::open(&path).unwrap());
        {
            let connection = Store::open_writer_connection(&path).unwrap();
            connection
                .execute_batch(
                    "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
                     VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1);\
                     INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                     VALUES ('01J0000000000000000000000A', 'mock', 't', 'running', 1, 1);",
                )
                .unwrap();
            for (run_id, message_id, status) in [
                (
                    "01J000000000000000000000R1",
                    "01J000000000000000000000M1",
                    "queued",
                ),
                (
                    "01J000000000000000000000R2",
                    "01J000000000000000000000M2",
                    "running",
                ),
                (
                    "01J000000000000000000000R3",
                    "01J000000000000000000000M3",
                    "succeeded",
                ),
            ] {
                connection
                    .execute(
                        "INSERT INTO messages (id, session_id, role, content, seq, created_at) \
                         VALUES (?1, '01J0000000000000000000000A', 'user', 'hi', 0, 1)",
                        [message_id],
                    )
                    .unwrap();
                connection
                    .execute(
                        "INSERT INTO runs (id, session_id, status, input_message_id, started_at) \
                         VALUES (?1, '01J0000000000000000000000A', ?2, ?3, 1)",
                        params![run_id, status, message_id],
                    )
                    .unwrap();
            }
        }
        let storage = StoreRuntime::open(
            &path,
            WriteQueueConfig::default(),
            &tokio::runtime::Handle::current(),
        )
        .unwrap();
        let reads = storage.reads().clone();
        let runs = reads.unfinished_runs().await.unwrap();
        let run_ids: Vec<&str> = runs.iter().map(|run| run.id.as_str()).collect();
        assert_eq!(
            run_ids,
            vec!["01J000000000000000000000R1", "01J000000000000000000000R2"],
            "未收口 run 必须为 queued + running（succeeded 排除）"
        );
        let sessions = reads.sessions_for_recovery().await.unwrap();
        assert_eq!(sessions.len(), 1, "非 idle 会话必须进入恢复清单");
        assert_eq!(sessions[0].status, SessionStatus::Running);

        let message = reads
            .message(&MessageId::new("01J000000000000000000000M1").unwrap())
            .await
            .unwrap()
            .expect("按 id 读消息");
        assert_eq!(message.content, "hi");
        assert!(reads
            .message(&MessageId::new("01J000000000000000000000M9").unwrap())
            .await
            .unwrap()
            .is_none());
        storage.shutdown().await.unwrap();
    }

    #[test]
    fn permission_resolve_only_applies_to_pending() {
        let mut connection = migrated_connection();
        seed_runtime_session(&mut connection);
        let record = PermissionRecord {
            id: "01J000000000000000000000P1".to_owned(),
            session_id: Some("01J0000000000000000000000A".to_owned()),
            request_id: Some("01J000000000000000000000Q1".to_owned()),
            resource: "fs.write".to_owned(),
            action: "write".to_owned(),
            target: Some("C:/ws/a.txt".to_owned()),
            decision: PermissionDecision::Ask,
            scope: None,
            status: PermissionStatus::Pending,
            requested_at: 1,
            resolved_at: None,
            resolver: None,
        };
        apply_command(
            &mut connection,
            &StoreCommand::InsertPermission {
                record: record.clone(),
            },
        )
        .unwrap();
        let first = apply_command(
            &mut connection,
            &StoreCommand::ResolvePermission {
                id: record.id.clone(),
                decision: PermissionDecision::Allow,
                scope: Some(PermissionScope::Once),
                status: PermissionStatus::Resolved,
                resolved_at: 2,
                resolver: Some("user".to_owned()),
                target: None,
            },
        )
        .unwrap();
        assert_eq!(first, StoreOutcome::Applied { affected: 1 });
        // 重复决议不生效（无丢失/重复语义：第二次 affected=0）。
        let second = apply_command(
            &mut connection,
            &StoreCommand::ResolvePermission {
                id: record.id.clone(),
                decision: PermissionDecision::Deny,
                scope: None,
                status: PermissionStatus::Resolved,
                resolved_at: 3,
                resolver: Some("user".to_owned()),
                target: None,
            },
        )
        .unwrap();
        assert_eq!(second, StoreOutcome::Applied { affected: 0 });
    }

    /// M3-05：设置项 upsert + 读回（同一 key 覆盖更新；值按 JSON 文本存储）。
    #[test]
    fn upsert_setting_roundtrips_and_overwrites() {
        let mut connection = migrated_connection();
        let first = apply_command(
            &mut connection,
            &StoreCommand::UpsertSetting {
                key: "backup.reminder".to_owned(),
                value: "true".to_owned(),
                updated_at: 10,
            },
        )
        .unwrap();
        assert_eq!(first, StoreOutcome::Applied { affected: 1 });

        let stored: String = connection
            .query_row(
                "SELECT value FROM settings WHERE key = 'backup.reminder'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "true");

        let updated = apply_command(
            &mut connection,
            &StoreCommand::UpsertSetting {
                key: "backup.reminder".to_owned(),
                value: "false".to_owned(),
                updated_at: 20,
            },
        )
        .unwrap();
        assert_eq!(
            updated,
            StoreOutcome::Applied { affected: 1 },
            "同 key 覆盖更新不新增行"
        );
        let (value, updated_at): (String, i64) = connection
            .query_row(
                "SELECT value, updated_at FROM settings WHERE key = 'backup.reminder'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((value.as_str(), updated_at), ("false", 20));
        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM settings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "upsert 语义：单 key 单行");
    }

    /// M3-05：`ReadPool::setting` / `ReadPool::settings` 读侧（不存在为 None）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_pool_exposes_setting_values() {
        use crate::store::Store;
        use crate::write_queue::{StoreRuntime, WriteQueueConfig};

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("aether.db");
        drop(Store::open(&path).unwrap());
        let storage = StoreRuntime::open(
            &path,
            WriteQueueConfig::default(),
            &tokio::runtime::Handle::current(),
        )
        .unwrap();
        let reads = storage.reads().clone();
        let write = storage.queue().clone();

        assert!(reads.setting("backup.reminder").await.unwrap().is_none());
        write
            .execute(StoreCommand::UpsertSetting {
                key: "backup.reminder".to_owned(),
                value: "true".to_owned(),
                updated_at: 1,
            })
            .await
            .unwrap();
        assert_eq!(
            reads.setting("backup.reminder").await.unwrap().as_deref(),
            Some("true")
        );
        write
            .execute(StoreCommand::UpsertSetting {
                key: "workspace.root".to_owned(),
                value: "\"C:/ws\"".to_owned(),
                updated_at: 2,
            })
            .await
            .unwrap();
        let all = reads.settings().await.unwrap();
        assert_eq!(
            all,
            vec![
                ("backup.reminder".to_owned(), "true".to_owned()),
                ("workspace.root".to_owned(), "\"C:/ws\"".to_owned()),
            ],
            "全量读按 key 升序"
        );
        let summary = reads.store_summary().await.unwrap();
        assert_eq!(
            summary.schema_version,
            crate::EMBEDDED_MIGRATIONS.last().map(|item| item.version),
            "迁移到最新（当前 0003）"
        );
        assert_eq!(summary.tables.get("settings"), Some(&2), "settings 行数");
        assert_eq!(summary.tables.len(), SUMMARY_TABLES.len());
        let mut table_order: Vec<&str> = summary.tables.keys().map(String::as_str).collect();
        table_order.sort_unstable();
        let mut expected: Vec<&str> = SUMMARY_TABLES.to_vec();
        expected.sort_unstable();
        assert_eq!(table_order, expected, "库摘要表清单齐备且键名可排序");
        storage.shutdown().await.unwrap();
    }
}
