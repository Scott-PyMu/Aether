//! 会话生命周期管理与 run 串行（M2-01；设计 D2/D5/D8、ADR-005）。
//!
//! 职责：
//! - **状态机**：`SessionFsm`（aether-core）为唯一事实来源；行与事件同向更新；
//! - **run 串行**（D8）：每会话同时 1 个执行中 run + 1 个等待队列；超出回
//!   `session_busy`（拒绝发生在持久化之前，不产生孤儿消息行）；
//! - **幂等**（ADR-005）：`client_msg_id` 去重由存储层 `BeginRunIdempotent` 单事务支撑，
//!   **核心重启后重放同值仍不重复**；
//! - **ack 快路径**：消息与 run 行提交后立即返回，不等模型执行（执行器结果异步落库）；
//! - **断流超时**（D8）：run 在 [`LifecycleConfig::run_stream_timeout_ms`]（默认 120s）内
//!   无任何事件 → `run.failed`（`run_stream_timeout`，recoverable）且会话回 idle 可重试；
//!   时钟经 [`Clock`] 注入，测试用 [`crate::clock::ManualClock`]。
//!
//! 取消树与看门狗（M2-05；设计 D8）：
//! - **取消树**（[`crate::cancel::CancelTree`]）：应用根 → 会话节点 → run 子节点；
//!   `interrupt` 取消当前 run（含等待 run），`dispose` 取消会话节点并级联全部子节点与
//!   **子会话**（`parent_session_id` 递归）；权限等待可经 [`RunCancelToken::cancelled`] 取消；
//! - **任务看门狗**（[`crate::cancel::TaskWatchdog`]）：登记会话执行任务；在途 run 被
//!   摘除（中断/超时/降级/关闭）后 10s 未退出 → 记 [`crate::cancel::TaskDump`] 并
//!   强制清理（`AbortHandle::abort`）；dump 进入诊断缓冲（M3-05 诊断包消费）。
//!
//! 任务 panic 隔离（M2-07；设计 D2「长驻任务全部经 `JoinSet` 管理并登记名称；
//! 任务 panic 由 JoinError 捕获记录，不传染」）：
//! - 会话执行任务统一经 `JoinSet` spawn，任务名（`session:<id> run:<id>`）登记进看门狗；
//! - 看门狗周期 [`reap_run_tasks`] 收割完成任务：`JoinError::is_panic()` → 记录该会话
//!   `run.failed`（错误码 [`RUN_TASK_PANIC_CODE`]）+ 管线 sequencer 恢复；**其余会话**因
//!   每会话独立任务且事件流来自管线广播而不受影响；
//! - panic 后若等待队列已提升，重新 spawn 该会话任务继续执行（run 串行不变）。
//!
//! 执行器（[`RunExecutor`]）为 M2-02 真实适配器的接入缝；M2-01 集成测试用测试替身。
//! 管理器内部对「状态机 + 行 + 事件」的每一次状态变更在状态互斥下整体串行，
//! 避免内存状态与持久化行在并发下相互错位。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_core::{
    session_status_is_terminal, ErrorInfo, EventPayload, Message, MessageId, MessageReference,
    MessageRole, MessageSummary, Run, RunCancelledPayload, RunCompletedPayload, RunFailedPayload,
    RunId, RunStartedPayload, Runtime, RuntimeId, Session, SessionClosedPayload,
    SessionCreatedPayload, SessionId, SessionStatus, SessionStatusChangedPayload, SessionSummary,
    SessionUpdatedPayload, TokenUsage, WorkspaceId, ENVELOPE_FIELDS, EVENT_ENVELOPE_VERSION,
};
use aether_store::{
    AuditLogRecord, ReadPool, SessionQuery, StoreCommand, StoreError, StoreOutcome, WriteQueue,
};
use serde_json::{json, Value};
use tokio::runtime::Handle;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::{AbortHandle, JoinError, JoinHandle, JoinSet};

use crate::backpressure::{BackpressureController, BackpressureError};
use crate::cancel::{CancelTree, RunCancelToken, TaskDump, TaskWatchdog};
use crate::clock::SharedClock;
use crate::error::PipelineError;
use crate::gate::{
    max_concurrent_for_free_ram, ConcurrencyGateCell, ConcurrencyGateSnapshot, FreeRamSource,
    GateQueue, GateWaiter, PriorityBucket, WaitReason, CONCURRENCY_GATE_MAX,
    DEFAULT_RUN_TIMEOUT_MS, GATE_RAM_SAMPLE_INTERVAL, MAX_QUEUED_GLOBAL, STARVATION_GUARD_MS,
    STARVATION_GUARD_PENALTY, TIMEOUT_SETTLE_GRACE_MS,
};
use crate::pipeline::{EventPipeline, RunInterrupt, SubmitOutcome};
use crate::routing::{route_priority, RouteDecision};
use crate::scheduler::{
    backoff_with_jitter_ms, is_auto_retryable, manual_retry_blocked, validate_max_attempts,
    validate_priority, validate_timeout_ms,
};
use crate::ulid;

/// 断流超时（D8/M2-01 DoD5：120s 无事件 → run failed 且可重试）。
pub const RUN_STREAM_TIMEOUT_MS: i64 = 120_000;
/// 断流超时错误码（`run.failed.error.code`）。
pub const RUN_STREAM_TIMEOUT_CODE: &str = "run_stream_timeout";
/// run 超时错误码（M10-07/ADR-037：`runs.status='timeout'` + `run.failed`）。
pub const RUN_TIMEOUT_CODE: &str = "run_timeout";
/// 核心重启中断错误码（M3-06 重启状态重建）：未收口 run 在启动收口时写入
/// `run.failed.error.code`（可重试；`run_retry` 准入口径为终态）。
pub const RUN_INTERRUPTED_CODE: &str = "run_interrupted";
/// 会话执行任务 panic 错误码（M2-07；D2 失败表「JoinError 记录 + 会话标 failed」）。
pub const RUN_TASK_PANIC_CODE: &str = "task_panic";
/// 看门狗巡检周期（常量级调参；DoD 只约束 120s 判定，不约束巡检频率）。
pub const WATCHDOG_TICK: Duration = Duration::from_secs(1);
/// 每会话等待队列上限（D8：运行中再收到消息 → 入 1 条等待队列，超过回 `session_busy`）。
pub const MAX_WAITING_RUNS_PER_SESSION: usize = 1;

/// 生命周期配置（默认值即 D8 口径；故障注入/测试可参数化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleConfig {
    /// 断流超时（默认 120s）。
    pub run_stream_timeout_ms: i64,
    /// 看门狗巡检周期（默认 1s）。
    pub watchdog_tick: Duration,
    /// 等待队列上限（默认 1）。
    pub max_waiting_runs: usize,
    /// 取消后任务强制清理阈值（D8：10s；测试经时钟注入）。
    pub task_force_cleanup_ms: i64,
    /// 任务 dump 环形缓冲容量（诊断包消费）。
    pub task_dump_capacity: usize,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            run_stream_timeout_ms: RUN_STREAM_TIMEOUT_MS,
            watchdog_tick: WATCHDOG_TICK,
            max_waiting_runs: MAX_WAITING_RUNS_PER_SESSION,
            task_force_cleanup_ms: crate::cancel::TASK_FORCE_CLEANUP_MS,
            task_dump_capacity: crate::cancel::TASK_DUMP_CAPACITY,
        }
    }
}

/// 会话创建选项（M6-01/ADR-014：档位与思考深度为显式会话级参数；缺省值见
/// [`SessionCreateOptions::default`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCreateOptions {
    pub workspace_id: Option<WorkspaceId>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    /// 预设生效来源（M7-09/ADR-026：`sessions.preset_id`；`None` = 未选预设）。
    pub preset_id: Option<aether_core::PresetId>,
    /// 会话级思考深度（0–4；缺省 [`aether_core::THINKING_DEPTH_DEFAULT`]）。
    pub thinking_depth: u8,
    /// 会话级权限模式（缺省 `workspace_write`）。
    pub permission_mode: aether_core::PermissionMode,
    /// ADR-028：所选供应商 id（模型→供应商解析；`None` = 旧单供应商路径）。
    pub provider_id: Option<String>,
    /// 自动路由决策（M7-02/ADR-022；`Some` = `runtime_id` 缺省经自动路由选中，
    /// 落审计 `route.decided` 且 `session.created.routed=true`；手动指定 = `None`）。
    pub route: Option<RouteDecision>,
}

impl Default for SessionCreateOptions {
    fn default() -> Self {
        Self {
            workspace_id: None,
            model: None,
            system_prompt: None,
            preset_id: None,
            thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
            permission_mode: aether_core::PermissionMode::default(),
            provider_id: None,
            route: None,
        }
    }
}

/// run 执行请求（M2-02 真实适配器据此调用 `session.create/send`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub runtime_id: RuntimeId,
    pub input_message_id: MessageId,
    pub text: String,
    /// 本次 run 的思考深度请求值（`session.send` 覆盖值或会话级值；ADR-010）。
    /// 执行器在 run 启动时按运行时能力门判定生效值并落 `runs.thinking_depth`。
    pub thinking_depth: u8,
    /// `true` = 来自 `session_send` 显式覆盖（透传 `session.send`，仅本次 run）；
    /// `false` = 会话级值（透传 `session.create`，`session.send` 不带字段）。
    pub thinking_override: bool,
    /// 随消息附带的文件引用（M7-06/ADR-024：透传 `session.send.references`；内存态，
    /// 不落库——重放（`run_retry`/暂停恢复）不带引用，见任务证据）。
    pub references: Vec<MessageReference>,
    /// M10-05/ADR-035：随消息附带的图像附件（引用帧；数据体不进线协议）。
    pub attachments: Vec<aether_core::OutboundAttachment>,
    /// M10-02/ADR-032：`session.send.subagent_context`（版本化、可 ack；仅子 Agent
    /// 结果回注路径使用；普通用户消息为 `None`）。
    pub subagent_context: Option<aether_core::SubagentContext>,
    /// 中断令牌（超时/降级/人工中断置位；执行器应尽快返回）。
    pub cancel: RunCancelToken,
}

/// 执行器终态（终态事件与行状态由管理器统一落库）。
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutorOutcome {
    Completed {
        /// 助手终稿（`Some` 时写 `messages` 行并广播 `message.completed`）。
        assistant_text: Option<String>,
        usage: Option<TokenUsage>,
    },
    Failed {
        error: ErrorInfo,
    },
    Cancelled {
        reason: Option<String>,
    },
}

/// 执行器异步结果（object-safe）。
pub type ExecutorFuture<'a> = Pin<Box<dyn Future<Output = ExecutorOutcome> + Send + 'a>>;

/// run 执行器（M2-01 测试替身 / M2-02 真实适配器）。
pub trait RunExecutor: Send + Sync + 'static {
    fn execute(&self, request: RunRequest) -> ExecutorFuture<'_>;
}

/// 闸门排队位置（M7-02/ADR-022 附录 A-5：`session_send` 响应可选 `queue`）。
///
/// `position` 仅指**全局队列**内位置（从 1 计；不含未进全局队列的会话等待槽 run）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueuePosition {
    /// 全局队列内位置（1 起；M10-07 起按优先级分数排序）。
    pub position: usize,
    /// 当前闸门上限（`clamp(floor(free_ram_gb / 1.5), 2, 10)`）。
    pub limit: usize,
}

/// 单次 run 的调度选项（M10-07/ADR-037 D-037-1/D-037-2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunScheduleOptions {
    /// 调用方优先级（−100…100；缺省 0）。
    pub priority: i32,
    /// run 超时（1s–24h；缺省 30min）。
    pub timeout_ms: i64,
    /// 最大尝试次数（1–5；缺省 1 = 自动重试关闭）。
    pub max_attempts: u32,
}

impl Default for RunScheduleOptions {
    fn default() -> Self {
        Self {
            priority: 0,
            timeout_ms: DEFAULT_RUN_TIMEOUT_MS,
            max_attempts: 1,
        }
    }
}

impl RunScheduleOptions {
    /// 校验并归一（越界 → `invalid_value` 语义；命令层应先行校验）。
    pub fn validate(self) -> Result<Self, LifecycleError> {
        validate_priority(self.priority).map_err(|reason| LifecycleError::InvalidOption {
            field: "priority",
            reason,
        })?;
        validate_timeout_ms(self.timeout_ms).map_err(|reason| LifecycleError::InvalidOption {
            field: "timeout_ms",
            reason,
        })?;
        validate_max_attempts(self.max_attempts).map_err(|reason| {
            LifecycleError::InvalidOption {
                field: "max_attempts",
                reason,
            }
        })?;
        Ok(self)
    }
}

/// 队列条目（`queue_list`；M10-07/ADR-037 D-037-1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub run_id: RunId,
    pub session_id: SessionId,
    /// 1 起；按 score/queued_at/run_id 排序后的位置（全局队列 + 会话等待槽合并视图）。
    pub position: usize,
    /// 调用方优先级。
    pub priority: i32,
    /// `base_priority + runtime_weight + wait_boost`（升序，小者先）。
    pub score: i32,
    pub queued_at: i64,
    pub wait_ms: i64,
    /// ADR-037 附录 B 等待原因（`concurrency_gate` 等）。
    pub wait_reason: String,
}

/// 队列查询结果（`queue_list`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueListReport {
    pub items: Vec<QueueEntry>,
    pub total: usize,
    pub gate: ConcurrencyGateSnapshot,
}

/// `queue_list` 单页上限（命令层校验；与既有分页口径一致）。
pub const QUEUE_LIST_MAX_LIMIT: usize = 100;

/// ack 快路径结果（消息与 run 行已提交；模型执行异步进行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendAck {
    pub session_id: SessionId,
    pub message_id: MessageId,
    pub run_id: RunId,
    /// 是否进入**会话等待槽**（D8；当前有 run 在执行）。
    pub queued: bool,
    /// 闸门排队位置（M7-02/ADR-022：超闸门进入全局 FIFO 时附；否则省略）。
    pub queue: Option<QueuePosition>,
    /// 是否幂等命中（相同 `client_msg_id` 重放；返回既有 message/run）。
    pub duplicate: bool,
}

/// 中断结果（M2-01 最小语义；M2-05 扩展取消树）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptReport {
    pub session_id: SessionId,
    pub interrupted_run: Option<RunId>,
    pub cancelled_waiting_run: Option<RunId>,
}

/// 暂停标记（M7-03/ADR-017 §2.1-3⑤：= 被中断 run 的输入，**内存态、不落库、
/// 不跨重启**）。恢复时经 `run_retry` 等价路径重放该输入（Mode R）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseMarker {
    /// 被暂停中断的 run（收口为 `cancelled`，reason=`user_pause`）。
    pub run_id: RunId,
    /// 重放复用原输入消息（不新增消息行；ADR-005）。
    pub input_message_id: MessageId,
}

/// `session.pause` 线协议下发钩子（M7-03/ADR-017 §2.1-1）：在核心侧受控中断收口
/// **之后**调用；best-effort——失败/超时记诊断与审计，不阻塞核心侧暂停收口、
/// 不自动重试。生产实现 = 适配器会话客户端（aether-tauri 接线）；未接线 = 跳过。
pub trait PauseNotifier: Send + Sync + 'static {
    fn notify_pause(
        &self,
        session_id: SessionId,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
}

/// 暂停回执（M7-03/ADR-017 附录 A-1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseReport {
    pub session_id: SessionId,
    /// 转移前状态（`idle` / `running` / `waiting_permission`）。
    pub from: SessionStatus,
    /// 转移后状态（恒为 `paused`）。
    pub to: SessionStatus,
    /// 被受控中断的在途 run（`run.cancelled(reason=user_pause)`）。
    pub interrupted_run: Option<RunId>,
    /// 等待队列保留的 run（不提升、不取消；ADR-017 §2.1-3②）。
    pub waiting_kept: Option<RunId>,
    /// `session.pause` 下发结果（best-effort；`None` = 未接线/无适配器会话）。
    pub pause_notify: Option<Result<(), String>>,
}

/// 恢复回执（M7-03/ADR-017 附录 A-1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeReport {
    pub session_id: SessionId,
    /// 转移前状态（恒为 `paused`）。
    pub from: SessionStatus,
    /// 转移后状态（有标记重放 → `running`；无标记 → `idle`）。
    pub to: SessionStatus,
    /// 重放 run（仅 `running` 分支）。
    pub run_id: Option<RunId>,
    /// 重放的输入消息（仅 `running` 分支；不新增消息行）。
    pub input_message_id: Option<MessageId>,
}

/// 暂停统计（M7-03 DoD6：诊断包 `session_pause` 段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseStats {
    /// 累计暂停次数（进程生命周期内）。
    pub total_pauses: u64,
    /// 累计恢复次数（进程生命周期内）。
    pub total_resumes: u64,
    /// 当前暂停会话数（DB `sessions.status='paused'` 事实源）。
    pub paused_sessions: usize,
}

/// `run_retry` 回执（M3-06）：重放产生的新 run 与复用的输入消息；旧 run 保留审计。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryAck {
    pub session_id: SessionId,
    /// 重放产生的新 run（旧 run 行不改动）。
    pub run_id: RunId,
    /// 重放的输入消息（原 run 的 `input_message_id`，不新增消息行）。
    pub input_message_id: MessageId,
    /// 是否进入等待队列（当前有 run 在执行；D8 串行不变）。
    pub queued: bool,
}

/// 重启状态重建报告（M3-06）：核心重启后未收口 run/会话的收口清单。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// 被收口为 `failed`（`run_interrupted`，可重试）的 run（queued + running；不自动重放）。
    pub runs_failed: Vec<RunId>,
    /// 被收回 `idle` 的会话。
    pub sessions_reset: Vec<SessionId>,
}

/// 生命周期错误（`code()` 为稳定错误码，命令层直接映射）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    SessionNotFound {
        session_id: SessionId,
    },
    /// 等待队列已满（D8：第 3 条消息回 `session_busy`）。
    SessionBusy {
        session_id: SessionId,
    },
    /// 会话处于终态。
    SessionClosed {
        session_id: SessionId,
        status: SessionStatus,
    },
    /// 状态机非法转移（不应发生；触发即 bug）。
    InvalidTransition {
        from: SessionStatus,
        to: SessionStatus,
    },
    /// 重试目标 run 不存在（M3-06 `run_retry`）。
    RunNotFound {
        run_id: RunId,
    },
    /// 重试目标 run 非终态（M3-06：仅 failed/timeout/cancelled 可重试）。
    RunNotRetryable {
        run_id: RunId,
        status: aether_core::RunStatus,
    },
    /// 会话已暂停（M7-03/ADR-017 附录 A-6：`paused` 态 `session_send`/`run_retry`
    /// 一律拒绝，不排队、不绕过；稳定业务码 `session_paused`，不新增 IPC 错误码枚举）。
    SessionPaused {
        session_id: SessionId,
    },
    /// 非法暂停/恢复状态（M7-03/ADR-017 附录 A-6 非法清单：`paused → paused`、
    /// 非 `paused` 态 resume、`creating`/终态 pause；`invalid_value` + 当前状态）。
    PauseState {
        session_id: SessionId,
        status: SessionStatus,
    },
    /// 存储降级（D4：拒绝新 run）。
    PersistDegraded {
        reason: String,
    },
    /// 写队列临时高水位（D8 L2；不改变存储状态）。
    StorageBackpressure {
        depth: usize,
        threshold: usize,
    },
    /// 适配器背压熔断/隔离（M2-04：L3 控制投递积压或存储侧背压隔离）；拒绝新会话/新 run。
    AdapterIsolated {
        runtime_id: RuntimeId,
        /// 熔断来源（`delivery_backlog` / `storage_backpressure`）。
        reason: String,
        since_ms: i64,
    },
    /// 附件重放失败（M10-05/ADR-035：retry/resume 必须重放完整输入；
    /// 不支持/文件缺失/哈希不一致等结构化拒绝，不静默降级为纯文本）。
    AttachmentRejected {
        kind: crate::attachments::AttachmentResolveErrorKind,
        message: String,
    },
    /// 调度选项非法（M10-07/ADR-037：priority/timeout_ms/max_attempts 越界）。
    InvalidOption {
        field: &'static str,
        reason: String,
    },
    /// 不可重试分类（M10-07/ADR-037 D-037-5：`invalid_value` + `data.retryable=false`）。
    RetryNotAllowed {
        run_id: RunId,
        reason: String,
    },
    /// 存储层错误。
    Storage {
        code: String,
        message: String,
    },
    /// 事件管线错误（先日志后广播路径）。
    Pipeline {
        code: String,
        message: String,
    },
    /// 内部不变量破坏。
    Internal {
        reason: String,
    },
}

impl LifecycleError {
    /// 稳定错误码。
    pub const fn code(&self) -> &'static str {
        match self {
            Self::SessionNotFound { .. } => "session_not_found",
            Self::SessionBusy { .. } => "session_busy",
            Self::SessionClosed { .. } => "session_closed",
            Self::InvalidTransition { .. } => "invalid_session_transition",
            Self::RunNotFound { .. } => "run_not_found",
            Self::RunNotRetryable { .. } => "run_not_retryable",
            Self::SessionPaused { .. } => "session_paused",
            Self::PauseState { .. } => "invalid_value",
            Self::PersistDegraded { .. } => "persist_degraded",
            Self::StorageBackpressure { .. } => "storage_backpressure",
            Self::AdapterIsolated { .. } => "storage_backpressure",
            Self::AttachmentRejected { .. } => "attachment_rejected",
            Self::InvalidOption { .. } => "invalid_value",
            Self::RetryNotAllowed { .. } => "invalid_value",
            Self::Storage { .. } => "storage_error",
            Self::Pipeline { .. } => "pipeline_error",
            Self::Internal { .. } => "internal",
        }
    }
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionNotFound { session_id } => write!(f, "会话不存在：{session_id}"),
            Self::SessionBusy { session_id } => write!(
                f,
                "会话忙（session_busy）：{session_id} 的执行中/等待队列已满（D8 串行，等待队列 1）"
            ),
            Self::SessionClosed { session_id, status } => {
                write!(f, "会话处于终态（{status}），拒绝新 run：{session_id}")
            }
            Self::InvalidTransition { from, to } => {
                write!(f, "非法会话状态转移：{from} → {to}")
            }
            Self::RunNotFound { run_id } => write!(f, "run 不存在（run_not_found）：{run_id}"),
            Self::RunNotRetryable { run_id, status } => write!(
                f,
                "run 非终态（run_not_retryable）：{run_id} 状态为 {status}；\
                 仅 failed/timeout/cancelled 可重试（ADR-004）"
            ),
            Self::SessionPaused { session_id } => write!(
                f,
                "会话已暂停（session_paused）：{session_id} 处于 paused 门禁态，\
                 暂停中拒绝发送与重放；恢复后解除（ADR-017）"
            ),
            Self::PauseState { session_id, status } => write!(
                f,
                "非法暂停/恢复操作（invalid_value）：会话 {session_id} 当前状态为 {status}；\
                 pause 仅接受 idle/running/waiting_permission，resume 仅接受 paused（ADR-017）"
            ),
            Self::PersistDegraded { reason } => {
                write!(f, "存储降级（persist_degraded）：拒绝新 run；{reason}")
            }
            Self::StorageBackpressure { depth, threshold } => write!(
                f,
                "存储写队列背压（storage_backpressure）：{depth} > {threshold}（D8 L2）"
            ),
            Self::AdapterIsolated {
                runtime_id,
                reason,
                since_ms,
            } => write!(
                f,
                "适配器背压熔断（storage_backpressure）：{runtime_id} 于 {since_ms} 隔离\
                 （来源 {reason}）；拒绝新会话/新 run（M2-04/D8）"
            ),
            Self::Storage { code, message } => write!(f, "存储错误（{code}）：{message}"),
            Self::AttachmentRejected { kind, message } => {
                write!(f, "附件重放失败（{kind:?}）：{message}")
            }
            Self::InvalidOption { field, reason } => {
                write!(f, "调度选项非法（invalid_value）：{field}；{reason}")
            }
            Self::RetryNotAllowed { run_id, reason } => write!(
                f,
                "run 不可重试（invalid_value，retryable=false）：{run_id}；{reason}"
            ),
            Self::Pipeline { code, message } => write!(f, "事件管线错误（{code}）：{message}"),
            Self::Internal { reason } => write!(f, "生命周期内部错误：{reason}"),
        }
    }
}

impl std::error::Error for LifecycleError {}

impl From<PipelineError> for LifecycleError {
    fn from(error: PipelineError) -> Self {
        match error {
            PipelineError::PersistDegraded { reason } => Self::PersistDegraded { reason },
            PipelineError::StorageBackpressure { depth, threshold } => {
                Self::StorageBackpressure { depth, threshold }
            }
            other => {
                let code = other.code().to_owned();
                Self::Pipeline {
                    code,
                    message: other.to_string(),
                }
            }
        }
    }
}

impl From<StoreError> for LifecycleError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::StorageBackpressure { depth, threshold } => {
                Self::StorageBackpressure { depth, threshold }
            }
            other => Self::Storage {
                code: other.code().to_owned(),
                message: other.to_string(),
            },
        }
    }
}

impl From<aether_core::SessionTransitionError> for LifecycleError {
    fn from(error: aether_core::SessionTransitionError) -> Self {
        match error {
            aether_core::SessionTransitionError::Illegal { from, to } => {
                Self::InvalidTransition { from, to }
            }
        }
    }
}

/// run 终态是否可重试（M3-06 `run_retry` 的准入口径：仅终态可重试）。
pub const fn run_is_retryable(status: aether_core::RunStatus) -> bool {
    matches!(
        status,
        aether_core::RunStatus::Failed
            | aether_core::RunStatus::Timeout
            | aether_core::RunStatus::Cancelled
    )
}

struct ActiveRun {
    run_id: RunId,
    input_message_id: MessageId,
    runtime_id: RuntimeId,
    text: String,
    /// 本次 run 的思考深度请求值（覆盖值或会话级值；ADR-010）。
    thinking_depth: u8,
    /// `true` = `session_send` 显式覆盖（透传 `session.send`）。
    thinking_override: bool,
    /// 随消息附带的文件引用（M7-06/ADR-024；内存态，不落库）。
    references: Vec<MessageReference>,
    /// M10-05/ADR-035：图像附件（引用帧；`content_parts` 已随消息落库）。
    attachments: Vec<aether_core::OutboundAttachment>,
    /// M10-02/ADR-032：子 Agent 结果回注上下文（内存态；重放不带）。
    subagent_context: Option<aether_core::SubagentContext>,
    last_activity_ms: i64,
    cancel: RunCancelToken,
    /// `true` = 已派发执行（占全局闸门槽位）；`false` = 闸门排队（占「当前 run 位」，
    /// 在全局队列中等待；ADR-022 附录 A-6）。
    dispatched: bool,
    /// M10-07/ADR-037：调用方优先级。
    priority: i32,
    /// M10-07/ADR-037：run 超时（1s–24h）。
    timeout_ms: i64,
    /// M10-07/ADR-037：最大尝试次数（1–5；1 = 自动重试关闭）。
    max_attempts: u32,
    /// M10-07/ADR-037：自动重试链计数（旧 attempt 行保留）。
    retry_count: u32,
    /// M10-07/ADR-037：等待时长起点。
    queued_at: i64,
    /// 派发时刻（超时计时起点；未派发 = `None`）。
    dispatched_at_ms: Option<i64>,
    /// 超时已请求收口（先 interrupt，5s 未收口 → 标 timeout）。
    timeout_requested_at_ms: Option<i64>,
}

struct QueuedRun {
    run_id: RunId,
    input_message_id: MessageId,
    runtime_id: RuntimeId,
    text: String,
    thinking_depth: u8,
    thinking_override: bool,
    references: Vec<MessageReference>,
    attachments: Vec<aether_core::OutboundAttachment>,
    subagent_context: Option<aether_core::SubagentContext>,
    /// M10-07/ADR-037：调度元数据（提升到 active 时继承）。
    priority: i32,
    timeout_ms: i64,
    max_attempts: u32,
    retry_count: u32,
    queued_at: i64,
}

/// 自动重试待执行项（内存态；daemon 重启不重放，旧 run 保留终态可人工重试）。
struct PendingRetry {
    run_id: RunId,
    session_id: SessionId,
    input_message_id: MessageId,
    priority: i32,
    timeout_ms: i64,
    max_attempts: u32,
    retry_count: u32,
    attempt: u32,
    due_at_ms: i64,
    backoff_ms: i64,
}

struct SessionState {
    fsm: aether_core::SessionFsm,
    runtime_id: RuntimeId,
    active: Option<ActiveRun>,
    waiting: Option<QueuedRun>,
    /// 会话取消节点（取消树；run 令牌为其子节点，父/子会话级联）。
    cancel: RunCancelToken,
    /// 暂停标记（M7-03/ADR-017：被中断 run；内存态、不落库、不跨重启）。
    pause_marker: Option<PauseMarker>,
}

/// 全局闸门状态（M7-02/ADR-022）：上限 + FIFO 队列；`running` 计数由会话 in-memory
/// 扫描（`active.dispatched`）得到，不维护冗余计数器（单一事实源，防漂移）。
struct GateState {
    /// 当前上限；初始缺省 = 上限常量（首轮巡检采样后按 `free_ram_gb` 收窄）。
    limit: usize,
    /// 全局 FIFO（单写者；不重排）。
    queue: GateQueue,
}

impl Default for GateState {
    fn default() -> Self {
        Self {
            limit: CONCURRENCY_GATE_MAX,
            queue: GateQueue::default(),
        }
    }
}

#[derive(Default)]
struct ManagerState {
    sessions: HashMap<SessionId, SessionState>,
    /// 并发闸门（M7-02/ADR-022）。
    gate: GateState,
}

struct ManagerInner {
    config: LifecycleConfig,
    clock: SharedClock,
    write: WriteQueue,
    reads: ReadPool,
    pipeline: EventPipeline,
    executor: Arc<dyn RunExecutor>,
    state: Arc<AsyncMutex<ManagerState>>,
    background: Mutex<Vec<JoinHandle<()>>>,
    /// 背压控制器（M2-04）：L3 投递熔断 / 存储侧隔离的适配器级准入；
    /// `None` = 未接线（仅管线全局准入，D8 L2 由 [`EventPipeline::admission`] 覆盖）。
    backpressure: Mutex<Option<Arc<BackpressureController>>>,
    /// 取消树（M2-05）：根 → 会话 → run 级联。
    cancel_tree: CancelTree,
    /// 任务看门狗（M2-05）：取消后 10s 未退出 → dump + 强制清理。
    task_watchdog: TaskWatchdog,
    /// 会话执行任务集合（M2-07；D2：长驻任务全部经 `JoinSet` 管理）。
    ///
    /// M7-02：`std::sync::Mutex`（非异步）——`spawn_run_inner` 保持**同步函数**，
    /// 避免「run 任务内推进闸门 FIFO → 再 spawn run 任务」形成的递归 future
    /// （递归链无法证明 `Send`，同等待队列提升不递归 spawn 的既有约束）。
    run_tasks: Mutex<JoinSet<()>>,
    /// run 任务 id →（会话, run）映射（panic 时经 JoinError 定位并恢复）。
    run_task_index: Mutex<HashMap<tokio::task::Id, (SessionId, RunId)>>,
    /// 空闲内存采样源（M7-02/ADR-022；`None` = 未接线——闸门保持缺省上限，不巡检）。
    free_ram: Mutex<Option<Arc<dyn FreeRamSource>>>,
    /// 附件重放解析器（M10-05；`None` = 未接线，带附件的 retry/resume 结构化拒绝）。
    attachment_resolver: Mutex<Option<Arc<dyn crate::attachments::AttachmentResolver>>>,
    /// 闸门快照单元（`health.concurrency_gate` 同步读；`None` = 未接线）。
    gate_cell: Mutex<Option<ConcurrencyGateCell>>,
    /// `session.pause` 线协议下发钩子（M7-03/ADR-017；`None` = 未接线 = 跳过下发）。
    pause_notifier: Mutex<Option<Arc<dyn PauseNotifier>>>,
    /// 暂停/恢复累计计数（M7-03 DoD6 诊断统计；进程生命周期内）。
    pause_counters: PauseCounters,
    /// 自动重试待执行项（M10-07/ADR-037；同步互斥，临界区无 I/O）。
    pending_retries: Mutex<Vec<PendingRetry>>,
}

/// 暂停/恢复累计计数（诊断；无锁原子）。
struct PauseCounters {
    pauses: AtomicU64,
    resumes: AtomicU64,
}

/// 会话生命周期管理器（克隆共享同一实例；内部状态经异步互斥保护）。
///
/// `write`/`reads` 与核心单写队列/读连接池共享句柄；`pipeline` 承载全部事件
/// （先日志后广播）。
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<ManagerInner>,
}

impl SessionManager {
    /// 组装管理器（不启动后台任务；由 [`SessionManager::spawn_background`] 显式启动）。
    pub fn new(
        config: LifecycleConfig,
        clock: SharedClock,
        write: WriteQueue,
        reads: ReadPool,
        pipeline: EventPipeline,
        executor: Arc<dyn RunExecutor>,
    ) -> Self {
        let task_watchdog =
            TaskWatchdog::new(config.task_force_cleanup_ms, config.task_dump_capacity);
        Self {
            inner: Arc::new(ManagerInner {
                config,
                clock,
                write,
                reads,
                pipeline,
                executor,
                state: Arc::new(AsyncMutex::new(ManagerState::default())),
                background: Mutex::new(Vec::new()),
                backpressure: Mutex::new(None),
                cancel_tree: CancelTree::new(),
                task_watchdog,
                run_tasks: Mutex::new(JoinSet::new()),
                run_task_index: Mutex::new(HashMap::new()),
                free_ram: Mutex::new(None),
                attachment_resolver: Mutex::new(None),
                gate_cell: Mutex::new(None),
                pause_notifier: Mutex::new(None),
                pause_counters: PauseCounters {
                    pauses: AtomicU64::new(0),
                    resumes: AtomicU64::new(0),
                },
                pending_retries: Mutex::new(Vec::new()),
            }),
        }
    }

    /// 接线背压控制器（M2-04；未接线 = 仅管线全局准入）。
    ///
    /// 接线后 `session.create`/`session.send` 额外执行适配器级准入：
    /// L3 控制投递熔断与存储侧背压隔离期返回 `storage_backpressure`；
    /// `persist_degraded` 由管线准入路径返回（本层不重复判定）。
    #[must_use]
    pub fn with_backpressure(&self, controller: Arc<BackpressureController>) -> Self {
        *lock_backpressure(&self.inner) = Some(controller);
        self.clone()
    }

    /// 接线空闲内存采样源（M7-02/ADR-022）：`spawn_background` 起启动即采样，
    /// 并按 [`GATE_RAM_SAMPLE_INTERVAL`] 巡检重算闸门上限（只对新 run 生效）。
    /// 未接线 = 闸门保持缺省上限、不巡检（测试构造与降级路径）。
    #[must_use]
    pub fn with_free_ram(&self, source: Arc<dyn FreeRamSource>) -> Self {
        *lock_free_ram(&self.inner) = Some(source);
        self.clone()
    }

    /// 注入附件重放解析器（M10-05/ADR-035：retry/resume 重放完整输入；
    /// 未接线时带附件的重放返回 `AttachmentRejected`，不静默降级为纯文本）。
    #[must_use]
    pub fn with_attachment_resolver(
        &self,
        resolver: Arc<dyn crate::attachments::AttachmentResolver>,
    ) -> Self {
        *lock_attachment_resolver(&self.inner) = Some(resolver);
        self.clone()
    }

    /// 接线闸门快照单元（M7-02/ADR-022：`health.concurrency_gate` 同步读数据源）。
    #[must_use]
    pub fn with_concurrency_gate(&self, cell: ConcurrencyGateCell) -> Self {
        *lock_gate_cell(&self.inner) = Some(cell);
        self.clone()
    }

    /// 接线 `session.pause` 下发钩子（M7-03/ADR-017：核心侧受控中断收口后调用；
    /// best-effort）。未接线 = 跳过下发（审计记录 `pause_notify: "not_wired"`）。
    #[must_use]
    pub fn with_pause_notifier(&self, notifier: Arc<dyn PauseNotifier>) -> Self {
        *lock_pause_notifier(&self.inner) = Some(notifier);
        self.clone()
    }

    /// 闸门巡检：采样空闲内存 → 重算上限（只对新 run 生效）→ 空槽按 FIFO 推进。
    /// 返回新上限；无采样源/采样失败 → `None`（保持既有值）。
    pub async fn refresh_gate_limit(&self) -> Option<usize> {
        refresh_gate_limit_inner(&self.inner).await
    }

    /// 闸门快照（诊断/测试；`health.concurrency_gate` 经快照单元读取）。
    pub async fn concurrency_gate(&self) -> ConcurrencyGateSnapshot {
        let state = lock_state(&self.inner).await;
        gate_snapshot(&state, self.inner.clock.now_ms())
    }

    /// 生命周期配置。
    pub fn config(&self) -> &LifecycleConfig {
        &self.inner.config
    }

    /// 适配器级背压准入（M2-04）：未接线 → 放行；熔断/隔离 → `storage_backpressure`。
    fn check_backpressure(&self, runtime_id: &RuntimeId) -> Result<(), LifecycleError> {
        let guard = lock_backpressure(&self.inner);
        let Some(controller) = guard.as_ref() else {
            return Ok(());
        };
        controller
            .admission(runtime_id)
            .map_err(|error| match error {
                BackpressureError::PersistDegraded { reason } => {
                    LifecycleError::PersistDegraded { reason }
                }
                BackpressureError::CircuitOpen {
                    runtime_id,
                    reason,
                    since_ms,
                } => LifecycleError::AdapterIsolated {
                    runtime_id,
                    reason: reason.detail_code().to_owned(),
                    since_ms,
                },
                BackpressureError::InvalidConfig { reason } => LifecycleError::Internal { reason },
            })
    }

    /// 创建会话：`EnsureRuntime` → 行（`creating`）→ `session.created` →
    /// `creating → idle` → 行 → `session.status_changed`（先日志后广播由管线保证）。
    ///
    /// `system_prompt`（M3-08/D14）：工作区记忆注入文本（由调用方按优先级与 32KB
    /// 上限组合；`None` = 无工作区/无记忆文件）；落 `sessions.system_prompt` 并经
    /// 执行器 `session.create` 下发适配器。
    pub async fn create_session(
        &self,
        runtime: Runtime,
        title: &str,
        workspace_id: Option<WorkspaceId>,
        model: Option<String>,
        system_prompt: Option<String>,
    ) -> Result<Session, LifecycleError> {
        self.create_session_with_options(
            runtime,
            title,
            SessionCreateOptions {
                workspace_id,
                model,
                system_prompt,
                ..SessionCreateOptions::default()
            },
        )
        .await
    }

    /// 创建会话（ADR-010：显式会话级思考深度；其余语义同 [`Self::create_session`]）。
    pub async fn create_session_with_depth(
        &self,
        runtime: Runtime,
        title: &str,
        workspace_id: Option<WorkspaceId>,
        model: Option<String>,
        system_prompt: Option<String>,
        thinking_depth: u8,
    ) -> Result<Session, LifecycleError> {
        self.create_session_with_options(
            runtime,
            title,
            SessionCreateOptions {
                workspace_id,
                model,
                system_prompt,
                thinking_depth,
                ..SessionCreateOptions::default()
            },
        )
        .await
    }

    /// 创建会话（M6-01/ADR-014 决策 3：显式会话级权限模式；其余语义同
    /// [`Self::create_session_with_depth`]）。
    ///
    /// 档位是持久化的判定权威入参（按会话解析）；适配器启动参数投影由 M7-06 承接，
    /// 本方法只负责落库与判定语义（在途会话沙箱参数不变）。
    pub async fn create_session_with_options(
        &self,
        runtime: Runtime,
        title: &str,
        options: SessionCreateOptions,
    ) -> Result<Session, LifecycleError> {
        // M2-04：L3 熔断/存储侧隔离期拒绝新会话。
        self.check_backpressure(&runtime.id)?;
        let now = self.inner.clock.now_ms();
        let session_id = SessionId::new(ulid::generate()).map_err(internal_from)?;
        let runtime_id = runtime.id.clone();
        // ADR-028：provider_id 落 `sessions.config.aether.provider_id`（重试/恢复可复用）。
        let config = match options.provider_id.as_deref() {
            Some(provider_id) => json!({ "aether": { "provider_id": provider_id } }),
            None => json!({}),
        };
        let session = Session {
            id: session_id.clone(),
            runtime_id: runtime_id.clone(),
            workspace_id: options.workspace_id,
            parent_session_id: None,
            title: title.to_owned(),
            status: SessionStatus::Creating,
            model: options.model,
            thinking_depth: options.thinking_depth,
            permission_mode: options.permission_mode,
            preset_id: options.preset_id,
            system_prompt: options.system_prompt,
            config,
            token_usage: TokenUsage::default(),
            created_at: now,
            updated_at: now,
            closed_at: None,
        };
        self.inner
            .write
            .execute(StoreCommand::EnsureRuntime { runtime })
            .await?;
        self.inner
            .write
            .execute(StoreCommand::InsertSession {
                session: session.clone(),
            })
            .await?;
        // M7-02/ADR-022 附录 A-3：自动路由决策落审计（actor=system；手动指定不写，
        // 手动选择已入 `session.created` 载荷）。
        if let Some(route) = &options.route {
            self.insert_audit(
                &session_id,
                "system",
                "route.decided",
                "selected",
                json!({
                    "selected": route.runtime_id,
                    "priority": route.priority,
                    "reason": route.reason,
                    "candidates": route.candidates.iter().map(|trace| json!({
                        "id": trace.id,
                        "enabled": trace.enabled,
                        "status": trace.status.as_str(),
                        "priority": trace.priority,
                        "active_runs": trace.active_runs,
                        "eligible": trace.eligible,
                        "detail": trace.detail,
                    })).collect::<Vec<_>>(),
                }),
            )
            .await?;
        }
        // M3-07（D9/SE-03 最小审计）：会话生命周期边界事件落审计（创建）。
        self.insert_audit(
            &session_id,
            "user",
            "session.created",
            "created",
            json!({
                "runtime_id": runtime_id.as_str(),
                "title": title,
            }),
        )
        .await?;
        self.emit(
            &session_id,
            None,
            &runtime_id,
            EventPayload::SessionCreated(SessionCreatedPayload {
                summary: SessionSummary {
                    id: session_id.clone(),
                    runtime_id: runtime_id.clone(),
                    workspace_id: session.workspace_id.clone(),
                    title: title.to_owned(),
                    status: SessionStatus::Creating,
                    model: session.model.clone(),
                    thinking_depth: Some(session.thinking_depth),
                    created_at: now,
                    updated_at: now,
                },
                // M7-02/ADR-022 §2.1 实现要点 2：缺省路由回显 `routed=true`。
                routed: options.route.as_ref().map(|_| true),
            }),
        )
        .await?;
        let mut fsm = aether_core::SessionFsm::new(SessionStatus::Creating);
        let change = fsm.transition(SessionStatus::Idle)?;
        self.persist_status_and_emit(&session_id, change, false, &runtime_id)
            .await?;

        let mut state = lock_state(&self.inner).await;
        let cancel = self.inner.cancel_tree.session_token(&session_id, None);
        state.sessions.insert(
            session_id.clone(),
            SessionState {
                fsm,
                runtime_id,
                active: None,
                waiting: None,
                cancel,
                pause_marker: None,
            },
        );
        drop(state);

        let mut created = session;
        created.status = SessionStatus::Idle;
        Ok(created)
    }

    /// 读取会话状态（DB 事实源；不存在 → `session_not_found`）。
    pub async fn session_status(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionStatus, LifecycleError> {
        let session = self.inner.reads.session(session_id).await?.ok_or_else(|| {
            LifecycleError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })?;
        Ok(session.status)
    }

    /// 读取 run 行（验收断言/重试准入用）。
    pub async fn run(&self, run_id: &RunId) -> Result<Option<Run>, LifecycleError> {
        Ok(self.inner.reads.run(run_id).await?)
    }

    /// 重放附件解析（M10-05）：无附件 → 空；有附件但解析器未接线/校验失败 →
    /// 结构化 `AttachmentRejected`（不静默降级为纯文本）。
    async fn resolve_replay_attachments(
        &self,
        session_id: &SessionId,
        message: &Message,
    ) -> Result<Vec<aether_core::OutboundAttachment>, LifecycleError> {
        if !crate::attachments::message_has_attachments(message) {
            return Ok(Vec::new());
        }
        let resolver = {
            let guard = lock_attachment_resolver(&self.inner);
            guard.clone()
        };
        let Some(resolver) = resolver else {
            return Err(LifecycleError::AttachmentRejected {
                kind: crate::attachments::AttachmentResolveErrorKind::StorageFailed,
                message: "附件重放解析器未接线：拒绝重放（不静默丢弃附件）".to_owned(),
            });
        };
        resolver
            .resolve(session_id, message)
            .await
            .map_err(|error| LifecycleError::AttachmentRejected {
                kind: error.kind,
                message: error.message,
            })
    }

    /// 一键重放（M3-06 `run_retry`；ADR-004/ADR-005）：
    /// - 准入：run 存在且为终态（`failed`/`timeout`/`cancelled`）；会话非终态；
    ///   管线准入（`persist_degraded`/背压）与适配器级背压检查；
    /// - 重放输入：复用原 run 的 `input_message_id`（**不新增用户消息行**），
    ///   正文取自该消息；新 run 重新编号，**旧 run 行保留审计**；
    /// - 重放模式：由执行器按 ADR-005 决定——存在 `sessions.config.native_id` 且
    ///   适配器支持时走 Mode R（原生恢复），否则 Mode N（新会话重发）；本层不
    ///   自行判定模式，仅负责重新派发。
    pub async fn retry_run(&self, run_id: &RunId) -> Result<RetryAck, LifecycleError> {
        let run =
            self.inner
                .reads
                .run(run_id)
                .await?
                .ok_or_else(|| LifecycleError::RunNotFound {
                    run_id: run_id.clone(),
                })?;
        if !run_is_retryable(run.status) {
            return Err(LifecycleError::RunNotRetryable {
                run_id: run_id.clone(),
                status: run.status,
            });
        }
        // M10-07/ADR-037 D-037-5：可重试分类校验（不可重试 → invalid_value +
        // `data.retryable=false`；`cancelled` 状态保持既有可重试语义）。
        if manual_retry_blocked(run.status, run.error.as_deref()) {
            return Err(LifecycleError::RetryNotAllowed {
                run_id: run_id.clone(),
                reason: format!(
                    "错误分类不可重试（{}）",
                    run.error.as_deref().unwrap_or("unknown")
                ),
            });
        }
        // 人工重试优先：清除同一 run 的待执行自动重试（同一 run 仅一个在途 attempt）。
        remove_pending_retry_run(&self.inner, run_id);
        let previous_schedule = self
            .inner
            .reads
            .run_schedule(run_id)
            .await?
            .unwrap_or_default();
        let retry_count = previous_schedule.retry_count.saturating_add(1);
        let priority = previous_schedule.priority;
        let max_attempts = previous_schedule.max_attempts.max(1);
        let input_message_id =
            run.input_message_id
                .clone()
                .ok_or_else(|| LifecycleError::Internal {
                    reason: format!("run {run_id} 缺少输入消息，无法重放（审计数据不完整）"),
                })?;
        let input_message = self
            .inner
            .reads
            .message(&input_message_id)
            .await?
            .ok_or_else(|| LifecycleError::Internal {
                reason: format!(
                    "run {run_id} 的输入消息 {input_message_id} 不存在（审计数据不完整）"
                ),
            })?;
        // M10-05/ADR-035：重试必须重放完整输入（文本 + 附件），并重新执行能力门；
        // 失败返回结构化错误，不静默降级为纯文本。
        let replay_attachments = self
            .resolve_replay_attachments(&run.session_id, &input_message)
            .await?;

        self.inner
            .pipeline
            .admission()
            .map_err(LifecycleError::from)?;

        let mut state = lock_state(&self.inner).await;
        self.hydrate_locked(&mut state, &run.session_id).await?;
        {
            let session = state.sessions.get(&run.session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: run.session_id.clone(),
                }
            })?;
            if session.fsm.is_terminal() {
                return Err(LifecycleError::SessionClosed {
                    session_id: run.session_id.clone(),
                    status: session.fsm.status(),
                });
            }
            // M7-03/ADR-017 附录 A-6：`paused` 态 run_retry 一律拒绝（防绕过暂停门禁）。
            if session.fsm.status() == SessionStatus::Paused {
                return Err(LifecycleError::SessionPaused {
                    session_id: run.session_id.clone(),
                });
            }
            if session.active.is_some() && session.waiting.is_some() {
                return Err(LifecycleError::SessionBusy {
                    session_id: run.session_id.clone(),
                });
            }
            // M10-07/ADR-037 D-037-1：全局队列上限 64（拒绝先于持久化）。
            if session.active.is_none() && !gate_has_slot(&state) && state.gate.queue.is_full() {
                return Err(LifecycleError::StorageBackpressure {
                    depth: state.gate.queue.len(),
                    threshold: MAX_QUEUED_GLOBAL,
                });
            }
            self.check_backpressure(&session.runtime_id)?;
        }

        // ADR-010 重放口径：按**会话级值**恢复（忽略该 run 原覆盖值），run 启动时
        // 重新执行能力门判定（重放行为与新建 run 一致）。
        let session_row = self
            .inner
            .reads
            .session(&run.session_id)
            .await?
            .ok_or_else(|| LifecycleError::SessionNotFound {
                session_id: run.session_id.clone(),
            })?;
        let thinking_depth = session_row.thinking_depth;

        let now = self.inner.clock.now_ms();
        let retry_run = Run {
            id: RunId::new(ulid::generate()).map_err(internal_from)?,
            session_id: run.session_id.clone(),
            status: aether_core::RunStatus::Queued,
            input_message_id: Some(input_message_id.clone()),
            thinking_depth: Some(thinking_depth),
            error: None,
            started_at: now,
            finished_at: None,
        };
        self.inner
            .write
            .execute(StoreCommand::InsertRun {
                run: retry_run.clone(),
            })
            .await?;
        self.inner
            .write
            .execute(StoreCommand::UpdateRunSchedule {
                run_id: retry_run.id.clone(),
                schedule: aether_store::RunSchedule {
                    priority,
                    queued_at: Some(now),
                    retry_count,
                    max_attempts,
                    wait_reason: Some(WaitReason::ConcurrencyGate.as_str().to_owned()),
                },
            })
            .await?;

        let (queued, gate_dispatched) = {
            let has_slot = gate_has_slot(&state);
            let session = state.sessions.get_mut(&run.session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: run.session_id.clone(),
                }
            })?;
            let runtime_id = session.runtime_id.clone();
            if session.active.is_none() {
                let cancel = RunCancelToken::child_of(&session.cancel);
                session.active = Some(ActiveRun {
                    run_id: retry_run.id.clone(),
                    input_message_id: input_message_id.clone(),
                    runtime_id: runtime_id.clone(),
                    text: input_message.content.clone(),
                    thinking_depth,
                    thinking_override: false,
                    // M7-06/ADR-024：引用为内存态不落库——重放不带引用（任务证据登记）。
                    references: Vec::new(),
                    // M10-05/ADR-035：附件从 content_parts 重新水合（含能力门/文件校验）。
                    attachments: replay_attachments.clone(),
                    subagent_context: None,
                    last_activity_ms: now,
                    cancel,
                    // M7-02/ADR-022：闸门前置于 run 启动（`run_retry` 同 `session_send`）。
                    dispatched: has_slot,
                    priority,
                    timeout_ms: DEFAULT_RUN_TIMEOUT_MS,
                    max_attempts,
                    retry_count,
                    queued_at: now,
                    dispatched_at_ms: has_slot.then_some(now),
                    timeout_requested_at_ms: None,
                });
                if has_slot {
                    (false, true)
                } else {
                    let waiter = GateWaiter::new(
                        run.session_id.clone(),
                        retry_run.id.clone(),
                        priority,
                        runtime_weight(&runtime_id),
                        now,
                        WaitReason::ConcurrencyGate,
                    );
                    if !state.gate.queue.enqueue(waiter) {
                        return Err(LifecycleError::StorageBackpressure {
                            depth: state.gate.queue.len(),
                            threshold: MAX_QUEUED_GLOBAL,
                        });
                    }
                    (false, false)
                }
            } else {
                session.waiting = Some(QueuedRun {
                    run_id: retry_run.id.clone(),
                    input_message_id: input_message_id.clone(),
                    runtime_id,
                    text: input_message.content.clone(),
                    thinking_depth,
                    thinking_override: false,
                    references: Vec::new(),
                    attachments: replay_attachments,
                    subagent_context: None,
                    priority,
                    timeout_ms: DEFAULT_RUN_TIMEOUT_MS,
                    max_attempts,
                    retry_count,
                    queued_at: now,
                });
                (true, false)
            }
        };
        publish_gate_snapshot(&self.inner, &state);
        drop(state);

        if gate_dispatched {
            self.spawn_run(run.session_id.clone(), retry_run.id.clone());
        }
        Ok(RetryAck {
            session_id: run.session_id,
            run_id: retry_run.id,
            input_message_id,
            queued,
        })
    }

    /// 暂停会话（M7-03/ADR-017 §2.1-3：核心层「受控中断 + 恢复重放」语义）。
    ///
    /// 流程：① 状态检查（`idle`/`running`/`waiting_permission`，否则 [`LifecycleError::PauseState`]）；
    /// ② 在途 run 按既有 interrupt 路径受控收口为 `cancelled`（reason=`user_pause`，
    /// 可 `run_retry`）+ 取消树级联 + 看门狗兜底；等待队列保留不提升；已派发的 run
    /// 释放全局闸门槽位并按 FIFO 推进其他会话；
    /// ③ 下发线协议 `session.pause`（best-effort；未接线/无适配器会话 = 跳过；失败只记
    /// 审计/诊断，不阻塞收口、不自动重试）；
    /// ④ 状态转移 `paused` + `session.status_changed`（先日志后广播）+ 审计 `session.pause`；
    /// ⑤ 有在途 run 时登记暂停标记（内存，不落库）。
    ///
    /// 能力门（`pause_resume`）由命令层在调用前实时复核（未声明 → `pause_unsupported`，
    /// 不做任何状态转移；本层不感知适配器能力）。实现注记（不改语义）：状态转移在
    /// 受控中断收口后先于 `session.pause` 的下发结果落库——下发为收尾性 best-effort，
    /// 语义上不影响暂停成立（ADR-017 §2.1-1），且避免持全局状态锁等待最长 10s 的
    /// D6 调用阻塞其他会话。
    pub async fn pause(&self, session_id: &SessionId) -> Result<PauseReport, LifecycleError> {
        let mut state = lock_state(&self.inner).await;
        self.hydrate_locked(&mut state, session_id).await?;
        // ① 状态检查 + ② 受控中断（等待队列保留）。
        let (from, runtime_id, active, waiting_kept) = {
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            let from = session.fsm.status();
            if !matches!(
                from,
                SessionStatus::Idle | SessionStatus::Running | SessionStatus::WaitingPermission
            ) {
                return Err(LifecycleError::PauseState {
                    session_id: session_id.clone(),
                    status: from,
                });
            }
            let active = session.active.take();
            let waiting_kept = session.waiting.as_ref().map(|queued| queued.run_id.clone());
            if let Some(active) = &active {
                if active.dispatched {
                    // 取消树级联：绑定该 run 的权限等待级联取消（D8）。
                    active.cancel.cancel();
                }
            }
            (from, session.runtime_id.clone(), active, waiting_kept)
        };
        let interrupted_run = active.as_ref().map(|active| active.run_id.clone());
        let mut dispatches: Vec<(SessionId, RunId)> = Vec::new();
        if let Some(active) = &active {
            if !active.dispatched {
                // 闸门排队 run（未派发）：从全局 FIFO 移除并取消。
                state.gate.queue.remove(&active.run_id);
            }
            self.finish_run(&active.run_id, aether_core::RunStatus::Cancelled, None)
                .await?;
            if active.dispatched {
                // M2-05：任务从在途 run 摘除 → 看门狗计时（10s 未退出 → dump + 强制清理）。
                self.inner
                    .task_watchdog
                    .mark_orphaned_by_run(&active.run_id, self.inner.clock.now_ms());
            }
            self.emit(
                session_id,
                Some(&active.run_id),
                &active.runtime_id,
                EventPayload::RunCancelled(RunCancelledPayload {
                    run_id: active.run_id.clone(),
                    reason: Some("user_pause".to_owned()),
                }),
            )
            .await?;
            // 释放全局闸门槽位（已派发）并按 FIFO 推进**其他会话**的排队 run；
            // 本会话等待队列保留不提升（ADR-017 §2.1-3②）。
            dispatches = gate_release_and_advance(&self.inner, &mut state);
        }
        // ④ 状态转移（`running`/`waiting_permission`/`idle` 一跳 → `paused`）+ ⑤ 标记。
        let change = {
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            let change = session.fsm.transition(SessionStatus::Paused)?;
            if let Some(active) = &active {
                session.pause_marker = Some(PauseMarker {
                    run_id: active.run_id.clone(),
                    input_message_id: active.input_message_id.clone(),
                });
            }
            change
        };
        self.persist_status_and_emit(session_id, change, false, &runtime_id)
            .await?;
        publish_gate_snapshot(&self.inner, &state);
        drop(state);
        for (spawn_session, spawn_run) in dispatches {
            self.spawn_run(spawn_session, spawn_run);
        }
        // ③ 下发 `session.pause`（best-effort；受控中断收口与状态转移完成之后）。
        let pause_notify = {
            let notifier = lock_pause_notifier(&self.inner).clone();
            match notifier {
                Some(notifier) => Some(notifier.notify_pause(session_id.clone()).await),
                None => None,
            }
        };
        // 审计（actor=user；from→to、在途 run 收口摘要、waiting 保留数、下发结果）。
        let notify_detail = match &pause_notify {
            None => json!("not_wired"),
            Some(Ok(())) => json!("ok"),
            Some(Err(detail)) => json!({ "failed": detail }),
        };
        let mut audit_detail = json!({
            "from": from.as_str(),
            "to": SessionStatus::Paused.as_str(),
            "interrupted_run": interrupted_run.as_ref().map(|run| run.as_str()),
            "waiting_kept": waiting_kept.as_ref().map(|run| run.as_str()),
            "pause_notify": notify_detail,
        });
        if interrupted_run.is_some() {
            // 在途 run 收口摘要：终止原因（`run.cancelled.reason`；ADR-017 附录 A-3）。
            audit_detail["interrupt_reason"] = json!("user_pause");
        }
        self.insert_audit(
            session_id,
            "user",
            "session.pause",
            &status_transition_result(from, SessionStatus::Paused),
            audit_detail,
        )
        .await?;
        self.inner
            .pause_counters
            .pauses
            .fetch_add(1, Ordering::Relaxed);
        Ok(PauseReport {
            session_id: session_id.clone(),
            from,
            to: SessionStatus::Paused,
            interrupted_run,
            waiting_kept,
            pause_notify,
        })
    }

    /// 恢复会话（M7-03/ADR-017 §2.1-4）：
    /// ① 状态 = `paused`（否则 [`LifecycleError::PauseState`]）+ 准入检查（管线段 +
    /// 适配器级背压；运行时可用性由命令层复核——disabled/未注册 → 拒绝并保持 paused）；
    /// ② 有暂停标记：以 `run_retry` 等价路径创建重放 run（复用原输入消息与
    /// `client_msg_id` 语义——不新增消息行；旧 run 保留审计；新 run 先于保留的 queued
    /// run 设为 active），`paused → running`（**先转移、后 spawn**，避免
    /// `dispatch_and_finalize` 的 idle 守卫竞态）+ 审计 `session.resume`，清除标记；
    /// ③ 无标记（含 idle 暂停、重启后）：`paused → idle` + 审计。
    ///
    /// 恢复失败（准入拒绝）保持 `paused` 且标记保留，可重试。
    pub async fn resume(&self, session_id: &SessionId) -> Result<ResumeReport, LifecycleError> {
        let mut state = lock_state(&self.inner).await;
        self.hydrate_locked(&mut state, session_id).await?;
        let (runtime_id, marker) = {
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            let status = session.fsm.status();
            if status != SessionStatus::Paused {
                return Err(LifecycleError::PauseState {
                    session_id: session_id.clone(),
                    status,
                });
            }
            (session.runtime_id.clone(), session.pause_marker.clone())
        };
        // ① 准入门（D4/ADR-017 §2.1-4）：拒绝时保持 paused 且标记保留（可重试）。
        self.inner
            .pipeline
            .admission()
            .map_err(LifecycleError::from)?;
        self.check_backpressure(&runtime_id)?;

        let Some(marker) = marker else {
            // ③ 无标记：`paused → idle`（由用户显式 `run_retry`/发送继续）。
            let change = {
                let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                    LifecycleError::SessionNotFound {
                        session_id: session_id.clone(),
                    }
                })?;
                session.fsm.transition(SessionStatus::Idle)?
            };
            self.persist_status_and_emit(session_id, change, false, &runtime_id)
                .await?;
            drop(state);
            self.insert_audit(
                session_id,
                "user",
                "session.resume",
                &status_transition_result(change.from, change.to),
                json!({
                    "from": change.from.as_str(),
                    "to": change.to.as_str(),
                }),
            )
            .await?;
            self.inner
                .pause_counters
                .resumes
                .fetch_add(1, Ordering::Relaxed);
            return Ok(ResumeReport {
                session_id: session_id.clone(),
                from: SessionStatus::Paused,
                to: SessionStatus::Idle,
                run_id: None,
                input_message_id: None,
            });
        };

        // ② 有标记：retry_run 等价重放（复用原输入消息；旧 run 保留审计）。
        let input_message = self
            .inner
            .reads
            .message(&marker.input_message_id)
            .await?
            .ok_or_else(|| LifecycleError::Internal {
                reason: format!(
                    "暂停重放：输入消息 {} 不存在（审计数据不完整）",
                    marker.input_message_id
                ),
            })?;
        // M10-05/ADR-035：暂停恢复同样重放完整输入（文本 + 附件），并重新执行能力门。
        let replay_attachments = self
            .resolve_replay_attachments(session_id, &input_message)
            .await?;
        let session_row = self.inner.reads.session(session_id).await?.ok_or_else(|| {
            LifecycleError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })?;
        // ADR-010 重放口径：按会话级值恢复（run 启动时重新执行能力门判定）。
        let thinking_depth = session_row.thinking_depth;
        let previous_schedule = self
            .inner
            .reads
            .run_schedule(&marker.run_id)
            .await?
            .unwrap_or_default();
        let priority = previous_schedule.priority;
        let max_attempts = previous_schedule.max_attempts.max(1);
        let retry_count = previous_schedule.retry_count.saturating_add(1);
        let now = self.inner.clock.now_ms();
        let replay_run = Run {
            id: RunId::new(ulid::generate()).map_err(internal_from)?,
            session_id: session_id.clone(),
            status: aether_core::RunStatus::Queued,
            input_message_id: Some(marker.input_message_id.clone()),
            thinking_depth: Some(thinking_depth),
            error: None,
            started_at: now,
            finished_at: None,
        };
        self.inner
            .write
            .execute(StoreCommand::InsertRun {
                run: replay_run.clone(),
            })
            .await?;
        self.inner
            .write
            .execute(StoreCommand::UpdateRunSchedule {
                run_id: replay_run.id.clone(),
                schedule: aether_store::RunSchedule {
                    priority,
                    queued_at: Some(now),
                    retry_count,
                    max_attempts,
                    wait_reason: Some(WaitReason::ConcurrencyGate.as_str().to_owned()),
                },
            })
            .await?;
        // 新 run 先于保留的 queued run 设为 active（D8 串行：等待槽后续按闸门提升）。
        let replay_dispatched = {
            let has_slot = gate_has_slot(&state);
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            if session.active.is_some() {
                return Err(LifecycleError::SessionBusy {
                    session_id: session_id.clone(),
                });
            }
            let runtime_id = session.runtime_id.clone();
            let cancel = RunCancelToken::child_of(&session.cancel);
            session.active = Some(ActiveRun {
                run_id: replay_run.id.clone(),
                input_message_id: marker.input_message_id.clone(),
                runtime_id: runtime_id.clone(),
                text: input_message.content.clone(),
                thinking_depth,
                thinking_override: false,
                // M7-06/ADR-024：暂停重放不带引用（内存态；任务证据登记）。
                references: Vec::new(),
                // M10-05/ADR-035：附件重新水合（失败时保持 paused，已在上方返回结构化错误）。
                attachments: replay_attachments,
                subagent_context: None,
                last_activity_ms: now,
                cancel,
                dispatched: has_slot,
                priority,
                timeout_ms: DEFAULT_RUN_TIMEOUT_MS,
                max_attempts,
                retry_count,
                queued_at: now,
                dispatched_at_ms: has_slot.then_some(now),
                timeout_requested_at_ms: None,
            });
            if !has_slot {
                state.gate.queue.enqueue(GateWaiter::new(
                    session_id.clone(),
                    replay_run.id.clone(),
                    priority,
                    runtime_weight(&runtime_id),
                    now,
                    WaitReason::ConcurrencyGate,
                ));
            }
            has_slot
        };
        // 先转移（`paused → running`）、后 spawn（避免 `dispatch_and_finalize` 的
        // idle 守卫竞态；ADR-017 §2.1-4）。
        let change = {
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            session.pause_marker = None;
            session.fsm.transition(SessionStatus::Running)?
        };
        self.persist_status_and_emit(session_id, change, false, &runtime_id)
            .await?;
        publish_gate_snapshot(&self.inner, &state);
        drop(state);
        if replay_dispatched {
            self.spawn_run(session_id.clone(), replay_run.id.clone());
        }
        self.insert_audit(
            session_id,
            "user",
            "session.resume",
            &status_transition_result(SessionStatus::Paused, SessionStatus::Running),
            json!({
                "from": SessionStatus::Paused.as_str(),
                "to": SessionStatus::Running.as_str(),
                "run_id": replay_run.id.as_str(),
                "replay_of": marker.run_id.as_str(),
            }),
        )
        .await?;
        self.inner
            .pause_counters
            .resumes
            .fetch_add(1, Ordering::Relaxed);
        Ok(ResumeReport {
            session_id: session_id.clone(),
            from: SessionStatus::Paused,
            to: SessionStatus::Running,
            run_id: Some(replay_run.id),
            input_message_id: Some(marker.input_message_id),
        })
    }

    /// 暂停统计（M7-03 DoD6：诊断包 `session_pause` 段；`paused_sessions` 以
    /// `sessions.status='paused'` 为事实源，重启后仍可读）。
    pub async fn pause_stats(&self) -> Result<PauseStats, LifecycleError> {
        let paused = self
            .inner
            .reads
            .sessions(SessionQuery {
                status: Some(SessionStatus::Paused),
                ..SessionQuery::default()
            })
            .await?;
        Ok(PauseStats {
            total_pauses: self.inner.pause_counters.pauses.load(Ordering::Relaxed),
            total_resumes: self.inner.pause_counters.resumes.load(Ordering::Relaxed),
            paused_sessions: paused.len(),
        })
    }

    /// ???????M3-06?D5 ?? + M10-07/ADR-037 v1.1 D-037-4??
    /// ????????/????????????????????
    /// 1. **????? run?`queued` + `running`?? `failed`**????
    ///    [`RUN_INTERRUPTED_CODE`]?`recoverable=true`?+ `run.failed` ???
    ///    **?????????????**?fail-closed??????????
    /// 2. ??????????`creating`/`running`/`waiting_permission`??
    ///    `idle` + `session.status_changed` ???
    ///    **`paused` ?????**?M7-03/ADR-017 ?2.1-6??????????????
    /// 3. ??? `run_retry` ?????Mode R/N ????? ADR-005 ????
    ///
    /// ?????????????????????????
    pub async fn reconcile_interrupted_runs(&self) -> Result<ReconcileReport, LifecycleError> {
        let runs = self.inner.reads.unfinished_runs().await?;
        let mut report = ReconcileReport::default();
        for run in runs {
            let session = match self.inner.reads.session(&run.session_id).await? {
                Some(session) => session,
                None => {
                    tracing::warn!(
                        run_id = %run.id,
                        session_id = %run.session_id,
                        "?????run ??????????????"
                    );
                    continue;
                }
            };
            let error = ErrorInfo {
                code: RUN_INTERRUPTED_CODE.to_owned(),
                message: "???????? run ???????????????? run_retry ????"
                    .to_owned(),
                recoverable: true,
            };
            if let Err(failure) = self
                .finish_run(
                    &run.id,
                    aether_core::RunStatus::Failed,
                    Some(error.code.clone()),
                )
                .await
            {
                tracing::warn!(run_id = %run.id, error = %failure, "?????run ????????");
                continue;
            }
            if let Err(failure) = self
                .emit(
                    &run.session_id,
                    Some(&run.id),
                    &session.runtime_id,
                    EventPayload::RunFailed(RunFailedPayload {
                        run_id: run.id.clone(),
                        error,
                    }),
                )
                .await
            {
                tracing::warn!(run_id = %run.id, error = %failure, "?????run.failed ???????????");
            }
            report.runs_failed.push(run.id);
        }

        let sessions = self.inner.reads.sessions_for_recovery().await?;
        for session in sessions {
            // M7-03/ADR-017 ?2.1-6?`paused` ?????**?? paused**???????
            // ??????????????????????? `session_resume` ?
            // `paused ? idle`???? run ? `run_interrupted` ?????? `run_retry`??
            if session.status == SessionStatus::Paused {
                tracing::info!(
                    session_id = %session.id,
                    "?????paused ?????ADR-017??????/???"
                );
                continue;
            }
            let now = self.inner.clock.now_ms();
            if let Err(failure) = self
                .inner
                .write
                .execute(StoreCommand::UpdateSessionStatus {
                    session_id: session.id.clone(),
                    status: SessionStatus::Idle,
                    updated_at: now,
                    closed_at: None,
                })
                .await
            {
                tracing::warn!(session_id = %session.id, error = %failure, "???????????????");
                continue;
            }
            // M3-07?D9/SE-03??????????????????????????????
            if let Err(failure) = insert_audit_inner(
                &self.inner,
                &session.id,
                "system",
                "session.status_changed",
                &status_transition_result(session.status, SessionStatus::Idle),
                json!({
                    "from": session.status.as_str(),
                    "to": SessionStatus::Idle.as_str(),
                    "recovered": true,
                    "replay": "fail_closed",
                }),
            )
            .await
            {
                tracing::warn!(session_id = %session.id, error = %failure, "????????????????");
            }
            if let Err(failure) = self
                .emit(
                    &session.id,
                    None,
                    &session.runtime_id,
                    EventPayload::SessionStatusChanged(SessionStatusChangedPayload {
                        session_id: session.id.clone(),
                        from: session.status,
                        to: SessionStatus::Idle,
                    }),
                )
                .await
            {
                tracing::warn!(session_id = %session.id, error = %failure, "????????????????????");
            }
            report.sessions_reset.push(session.id);
        }
        Ok(report)
    }

    /// 发送消息（ack 快路径）：消息与 run 行提交后立即返回。
    ///
    /// 顺序：准入检查（降级/背压）→ 会话状态/等待队列容量检查（拒绝先于持久化）→
    /// `BeginRunIdempotent`（幂等单事务）→ 立即执行或入等待队列 → 异步执行（`run.started`）。
    pub async fn send(
        &self,
        session_id: &SessionId,
        text: &str,
        client_msg_id: &str,
    ) -> Result<SendAck, LifecycleError> {
        self.send_with_thinking_depth(session_id, text, client_msg_id, None)
            .await
    }

    /// 发送消息 + 可选本次 run 思考深度覆盖（ADR-010：`session_send` 覆盖仅本次 run，
    /// 不回写会话级值）。
    pub async fn send_with_thinking_depth(
        &self,
        session_id: &SessionId,
        text: &str,
        client_msg_id: &str,
        thinking_override: Option<u8>,
    ) -> Result<SendAck, LifecycleError> {
        self.send_with_references(
            session_id,
            text,
            client_msg_id,
            thinking_override,
            Vec::new(),
            None,
        )
        .await
    }

    /// `session_send` + 可选文件引用（M7-06/ADR-024：`references` 随 `session.send`
    /// 透传适配器；≤32 条与 canonicalize 校验在命令层完成；引用不落库、不预授权）。
    ///
    /// M10-02/ADR-032：`subagent_context` 为子 Agent 结果回注载荷（版本化、可 ack；
    /// `None` = 普通用户消息）。
    pub async fn send_with_references(
        &self,
        session_id: &SessionId,
        text: &str,
        client_msg_id: &str,
        thinking_override: Option<u8>,
        references: Vec<MessageReference>,
        subagent_context: Option<aether_core::SubagentContext>,
    ) -> Result<SendAck, LifecycleError> {
        self.send_with_attachments(
            session_id,
            text,
            client_msg_id,
            thinking_override,
            references,
            Vec::new(),
            subagent_context,
        )
        .await
    }

    /// M10-05/ADR-035：`session_send` + 图像附件（引用帧；消息 `content_parts` 与
    /// 附件绑定在同一存储事务内完成，见 `BeginRunIdempotent.attachment_ids`）。
    #[allow(clippy::too_many_arguments)]
    pub async fn send_with_attachments(
        &self,
        session_id: &SessionId,
        text: &str,
        client_msg_id: &str,
        thinking_override: Option<u8>,
        references: Vec<MessageReference>,
        attachments: Vec<aether_core::OutboundAttachment>,
        subagent_context: Option<aether_core::SubagentContext>,
    ) -> Result<SendAck, LifecycleError> {
        self.send_with_schedule(
            session_id,
            text,
            client_msg_id,
            thinking_override,
            references,
            attachments,
            subagent_context,
            RunScheduleOptions::default(),
        )
        .await
    }

    /// M10-07/ADR-037：`session_send` + 优先级/超时/最大尝试次数。
    ///
    /// 「闸门前置」不变：优先级只影响**排队顺序**，不能绕过并发闸门/健康约束。
    #[allow(clippy::too_many_arguments)]
    pub async fn send_with_schedule(
        &self,
        session_id: &SessionId,
        text: &str,
        client_msg_id: &str,
        thinking_override: Option<u8>,
        references: Vec<MessageReference>,
        attachments: Vec<aether_core::OutboundAttachment>,
        subagent_context: Option<aether_core::SubagentContext>,
        schedule: RunScheduleOptions,
    ) -> Result<SendAck, LifecycleError> {
        let schedule = schedule.validate()?;
        self.inner
            .pipeline
            .admission()
            .map_err(LifecycleError::from)?;

        let mut state = lock_state(&self.inner).await;
        self.hydrate_locked(&mut state, session_id).await?;
        {
            let session =
                state
                    .sessions
                    .get(session_id)
                    .ok_or_else(|| LifecycleError::SessionNotFound {
                        session_id: session_id.clone(),
                    })?;
            if session.fsm.is_terminal() {
                return Err(LifecycleError::SessionClosed {
                    session_id: session_id.clone(),
                    status: session.fsm.status(),
                });
            }
            // M7-03/ADR-017 附录 A-6：`paused` 态 `session_send` 拒绝（不排队；稳定
            // 业务码 `session_paused`，恢复后解除）。
            if session.fsm.status() == SessionStatus::Paused {
                return Err(LifecycleError::SessionPaused {
                    session_id: session_id.clone(),
                });
            }
            if session.active.is_some() && session.waiting.is_some() {
                return Err(LifecycleError::SessionBusy {
                    session_id: session_id.clone(),
                });
            }
            // M10-07/ADR-037 D-037-1：全局队列上限 64（拒绝先于持久化）。
            if session.active.is_none() && !gate_has_slot(&state) && state.gate.queue.is_full() {
                return Err(LifecycleError::StorageBackpressure {
                    depth: state.gate.queue.len(),
                    threshold: MAX_QUEUED_GLOBAL,
                });
            }
            // M2-04：L3 投递熔断/存储侧隔离 → 拒绝新 run（已有 run 不受影响）。
            self.check_backpressure(&session.runtime_id)?;
        }

        // ADR-010：run 生效候选值 = 本次覆盖（如有）?? 会话级值；能力门生效值由
        // 执行器在 run 启动时判定并落库（未支持 → 缺省 2、不回写会话级）。
        let requested_thinking_depth = match thinking_override {
            Some(value) => value,
            None => {
                self.inner
                    .reads
                    .session(session_id)
                    .await?
                    .ok_or_else(|| LifecycleError::SessionNotFound {
                        session_id: session_id.clone(),
                    })?
                    .thinking_depth
            }
        };
        let thinking_is_override = thinking_override.is_some();

        let now = self.inner.clock.now_ms();
        // M10-05/ADR-035：附件引用写入 `messages.content_parts`（不存二进制）。
        let content_parts = if attachments.is_empty() {
            None
        } else {
            let parts: Vec<aether_core::AttachmentContentPart> =
                attachments.iter().map(|item| item.content_part()).collect();
            Some(
                serde_json::to_value(parts).map_err(|error| LifecycleError::Internal {
                    reason: format!("附件 content_parts 序列化失败：{error}"),
                })?,
            )
        };
        let attachment_ids: Vec<String> = attachments
            .iter()
            .map(|item| item.attachment_id.clone())
            .collect();
        let message = Message {
            id: MessageId::new(ulid::generate()).map_err(internal_from)?,
            session_id: session_id.clone(),
            run_id: None,
            client_msg_id: Some(client_msg_id.to_owned()),
            role: MessageRole::User,
            content: text.to_owned(),
            content_parts,
            tool_calls: None,
            parent_message_id: None,
            seq: 0,
            created_at: now,
        };
        let run = Run {
            id: RunId::new(ulid::generate()).map_err(internal_from)?,
            session_id: session_id.clone(),
            status: aether_core::RunStatus::Queued,
            input_message_id: Some(message.id.clone()),
            thinking_depth: Some(requested_thinking_depth),
            error: None,
            started_at: now,
            finished_at: None,
        };
        let outcome = self
            .inner
            .write
            .execute(StoreCommand::BeginRunIdempotent {
                message: message.clone(),
                run: run.clone(),
                attachment_ids,
                schedule: aether_store::RunSchedule {
                    priority: schedule.priority,
                    queued_at: Some(now),
                    retry_count: 0,
                    max_attempts: schedule.max_attempts,
                    wait_reason: Some(WaitReason::ConcurrencyGate.as_str().to_owned()),
                },
            })
            .await?;
        let (message_id, run_id, duplicate) = match outcome {
            StoreOutcome::RunAccepted {
                message_id,
                run_id,
                duplicate,
            } => (message_id, run_id, duplicate),
            other => {
                return Err(LifecycleError::Internal {
                    reason: format!("BeginRunIdempotent 返回非预期结果: {other:?}"),
                })
            }
        };
        if duplicate {
            return Ok(SendAck {
                session_id: session_id.clone(),
                message_id,
                run_id,
                queued: false,
                queue: None,
                duplicate: true,
            });
        }

        let (queued, gate_queue) = {
            // M7-02/ADR-022 §2.1#6 / M10-07/ADR-037：闸门计数 = 全局 running run 数；
            // 超限 → run 入优先级全局队列（`runs.status='queued'`，占「当前 run 位」，
            // 不超卖；优先级只影响排队顺序）。
            let has_slot = gate_has_slot(&state);
            let limit = state.gate.limit;
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            let runtime_id = session.runtime_id.clone();
            if session.active.is_none() {
                let cancel = RunCancelToken::child_of(&session.cancel);
                session.active = Some(ActiveRun {
                    run_id: run_id.clone(),
                    input_message_id: message_id.clone(),
                    runtime_id: runtime_id.clone(),
                    text: text.to_owned(),
                    thinking_depth: requested_thinking_depth,
                    thinking_override: thinking_is_override,
                    references: references.clone(),
                    attachments: attachments.clone(),
                    subagent_context: subagent_context.clone(),
                    last_activity_ms: now,
                    cancel,
                    dispatched: has_slot,
                    priority: schedule.priority,
                    timeout_ms: schedule.timeout_ms,
                    max_attempts: schedule.max_attempts,
                    retry_count: 0,
                    queued_at: now,
                    dispatched_at_ms: has_slot.then_some(now),
                    timeout_requested_at_ms: None,
                });
                if has_slot {
                    (false, None)
                } else {
                    let waiter = GateWaiter::new(
                        session_id.clone(),
                        run_id.clone(),
                        schedule.priority,
                        runtime_weight(&runtime_id),
                        now,
                        WaitReason::ConcurrencyGate,
                    );
                    if !state.gate.queue.enqueue(waiter) {
                        return Err(LifecycleError::StorageBackpressure {
                            depth: state.gate.queue.len(),
                            threshold: MAX_QUEUED_GLOBAL,
                        });
                    }
                    let queue = state
                        .gate
                        .queue
                        .position(&run_id, now)
                        .map(|position| QueuePosition { position, limit });
                    (false, queue)
                }
            } else {
                session.waiting = Some(QueuedRun {
                    run_id: run_id.clone(),
                    input_message_id: message_id.clone(),
                    runtime_id,
                    text: text.to_owned(),
                    thinking_depth: requested_thinking_depth,
                    thinking_override: thinking_is_override,
                    references,
                    attachments,
                    subagent_context,
                    priority: schedule.priority,
                    timeout_ms: schedule.timeout_ms,
                    max_attempts: schedule.max_attempts,
                    retry_count: 0,
                    queued_at: now,
                });
                (true, None)
            }
        };
        publish_gate_snapshot(&self.inner, &state);
        drop(state);

        if !queued && gate_queue.is_none() {
            self.spawn_run(session_id.clone(), run_id.clone());
        }
        Ok(SendAck {
            session_id: session_id.clone(),
            message_id,
            run_id,
            queued,
            queue: gate_queue,
            duplicate: false,
        })
    }

    /// 中断当前 run（最小语义：置取消令牌 + run 终态 `cancelled`；M2-05 扩展取消树）。
    ///
    /// M7-02/ADR-022 附录 A-6 合成语义：
    /// - 已派发的当前 run：P0 语义不变（取消当前 run 与等待槽 run；释放闸门槽位并按
    ///   FIFO 推进）；
    /// - 闸门排队的当前 run：从全局 FIFO 移除并取消（槽位让给 FIFO 下一项）；
    ///   **等待槽不连带清空**——随后按闸门判定提升（有空槽 → 派发；否则入 FIFO 尾部）。
    pub async fn interrupt(
        &self,
        session_id: &SessionId,
    ) -> Result<InterruptReport, LifecycleError> {
        let mut state = lock_state(&self.inner).await;
        self.hydrate_locked(&mut state, session_id).await?;
        let (active, waiting, runtime_id) = {
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            // M7-03/ADR-017 附录 A-6：paused 态 interrupt = no-op（不动标记、
            // 不动等待队列；暂停已收口在途 run）。
            if session.fsm.status() == SessionStatus::Paused {
                return Ok(InterruptReport {
                    session_id: session_id.clone(),
                    interrupted_run: None,
                    cancelled_waiting_run: None,
                });
            }
            let active = session.active.take();
            let waiting = session.waiting.take();
            if let Some(active) = &active {
                if active.dispatched {
                    active.cancel.cancel();
                }
            }
            (active, waiting, session.runtime_id.clone())
        };

        let interrupted_run = active.as_ref().map(|active| active.run_id.clone());
        let mut cancelled_waiting_run = None;
        let mut spawn_queue: Vec<(SessionId, RunId)> = Vec::new();
        let mut dispatches: Vec<(SessionId, RunId)> = Vec::new();

        match &active {
            Some(active) if active.dispatched => {
                self.finish_run(&active.run_id, aether_core::RunStatus::Cancelled, None)
                    .await?;
                // M2-05：任务从在途 run 摘除 → 看门狗计时（10s 未退出 → dump + 强制清理）。
                self.inner
                    .task_watchdog
                    .mark_orphaned_by_run(&active.run_id, self.inner.clock.now_ms());
                self.emit(
                    session_id,
                    Some(&active.run_id),
                    &active.runtime_id,
                    EventPayload::RunCancelled(RunCancelledPayload {
                        run_id: active.run_id.clone(),
                        reason: Some("user_interrupt".to_owned()),
                    }),
                )
                .await?;
                // 会话回 idle（D9 等待审批时经 running 收口；取消树已级联取消权限等待）。
                settle_idle_locked(&self.inner, &mut state, session_id, &active.runtime_id).await?;
                // P0：等待槽 run 连带取消。
                if let Some(waiting) = waiting {
                    self.finish_run(&waiting.run_id, aether_core::RunStatus::Cancelled, None)
                        .await?;
                    self.emit(
                        session_id,
                        Some(&waiting.run_id),
                        &waiting.runtime_id,
                        EventPayload::RunCancelled(RunCancelledPayload {
                            run_id: waiting.run_id.clone(),
                            reason: Some("user_interrupt".to_owned()),
                        }),
                    )
                    .await?;
                    cancelled_waiting_run = Some(waiting.run_id);
                }
                // 闸门释放与推进（单写者；降级期不推进）。
                dispatches = gate_release_and_advance(&self.inner, &mut state);
            }
            Some(active) => {
                // 闸门排队 run（未派发）：从全局 FIFO 移除并取消；不释放槽位。
                state.gate.queue.remove(&active.run_id);
                self.finish_run(&active.run_id, aether_core::RunStatus::Cancelled, None)
                    .await?;
                self.emit(
                    session_id,
                    Some(&active.run_id),
                    &active.runtime_id,
                    EventPayload::RunCancelled(RunCancelledPayload {
                        run_id: active.run_id.clone(),
                        reason: Some("user_interrupt".to_owned()),
                    }),
                )
                .await?;
                // 等待槽不连带清空（ADR-022 附录 A-6）：回填等待槽后按闸门判定提升。
                if let Some(waiting) = waiting {
                    if let Some(session) = state.sessions.get_mut(session_id) {
                        session.waiting = Some(waiting);
                    }
                }
                dispatches = gate_release_and_advance(&self.inner, &mut state);
                if let Some((promoted_session, promoted_run)) =
                    promote_waiting_locked(&self.inner, &mut state, session_id)
                {
                    spawn_queue.push((promoted_session, promoted_run));
                }
            }
            None => {
                // P0：无当前 run 时中断等待槽 run。
                if let Some(waiting) = waiting {
                    self.finish_run(&waiting.run_id, aether_core::RunStatus::Cancelled, None)
                        .await?;
                    self.emit(
                        session_id,
                        Some(&waiting.run_id),
                        &waiting.runtime_id,
                        EventPayload::RunCancelled(RunCancelledPayload {
                            run_id: waiting.run_id.clone(),
                            reason: Some("user_interrupt".to_owned()),
                        }),
                    )
                    .await?;
                    cancelled_waiting_run = Some(waiting.run_id);
                }
            }
        }
        publish_gate_snapshot(&self.inner, &state);
        drop(state);
        let _ = runtime_id;

        for (spawn_session, spawn_run) in spawn_queue.into_iter().chain(dispatches) {
            self.spawn_run(spawn_session, spawn_run);
        }
        Ok(InterruptReport {
            session_id: session_id.clone(),
            interrupted_run,
            cancelled_waiting_run,
        })
    }

    /// 会话改名（M7-07/ADR-025 §2.1-3：`session_rename`）。
    ///
    /// 单写队列更新 `sessions.title`（FTS 触发器自动同步）→ 审计 `session.renamed`
    /// → 先日志后广播 `session.updated`（`changed_fields=["title"]` + 新标题）。
    pub async fn rename_session(
        &self,
        session_id: &SessionId,
        title: &str,
    ) -> Result<Session, LifecycleError> {
        let now = self.inner.clock.now_ms();
        let mut session = self.inner.reads.session(session_id).await?.ok_or_else(|| {
            LifecycleError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })?;
        let outcome = self
            .inner
            .write
            .execute(StoreCommand::RenameSession {
                id: session_id.clone(),
                title: title.to_owned(),
                updated_at: now,
            })
            .await?;
        match outcome {
            StoreOutcome::Applied { affected } if affected > 0 => {}
            StoreOutcome::Applied { .. } => {
                return Err(LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                });
            }
            other => {
                return Err(LifecycleError::Internal {
                    reason: format!("session_rename 非预期存储结果：{other:?}"),
                });
            }
        }
        self.insert_audit(
            session_id,
            "user",
            "session.renamed",
            "renamed",
            json!({
                "title": title,
                "previous_title": session.title.clone(),
            }),
        )
        .await?;
        self.emit(
            session_id,
            None,
            &session.runtime_id,
            EventPayload::SessionUpdated(SessionUpdatedPayload {
                session_id: session_id.clone(),
                changed_fields: vec!["title".to_owned()],
                title: Some(title.to_owned()),
            }),
        )
        .await?;
        session.title = title.to_owned();
        session.updated_at = now;
        Ok(session)
    }

    /// 硬删除会话（M7-07/ADR-025 §2.1-4：`session_delete`）。
    ///
    /// 前置：无在途 run（`queued`/`running`）、无 `waiting_permission`/pending 票据
    /// （否则 [`LifecycleError::SessionBusy`]）；存储层在同一事务内复核。
    /// 删除 `sessions` 行（FK 级联 `messages`/`runs`/`artifacts`/`permissions`；
    /// `events` 不级联，R7）→ 审计 `session.deleted`（级联行数摘要）→
    /// 广播 `session.closed(reason="deleted")`。
    pub async fn delete_session(&self, session_id: &SessionId) -> Result<(), LifecycleError> {
        {
            let state = lock_state(&self.inner).await;
            if let Some(session) = state.sessions.get(session_id) {
                if session.active.is_some() || session.waiting.is_some() {
                    return Err(LifecycleError::SessionBusy {
                        session_id: session_id.clone(),
                    });
                }
            }
        }
        let session = self.inner.reads.session(session_id).await?.ok_or_else(|| {
            LifecycleError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })?;
        let outcome = self
            .inner
            .write
            .execute(StoreCommand::DeleteSession {
                id: session_id.clone(),
            })
            .await?;
        let (messages, runs, artifacts, permissions, events_retained) = match outcome {
            StoreOutcome::SessionDeleted {
                messages,
                runs,
                artifacts,
                permissions,
                events_retained,
            } => (messages, runs, artifacts, permissions, events_retained),
            StoreOutcome::SessionBusy { .. } => {
                return Err(LifecycleError::SessionBusy {
                    session_id: session_id.clone(),
                });
            }
            StoreOutcome::SessionNotFound => {
                return Err(LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                });
            }
            other => {
                return Err(LifecycleError::Internal {
                    reason: format!("session_delete 非预期存储结果：{other:?}"),
                });
            }
        };
        let now = self.inner.clock.now_ms();
        {
            let mut state = lock_state(&self.inner).await;
            state.sessions.remove(session_id);
        }
        self.inner.cancel_tree.cancel_session(session_id);
        self.insert_audit(
            session_id,
            "user",
            "session.deleted",
            "deleted",
            json!({
                "messages": messages,
                "runs": runs,
                "artifacts": artifacts,
                "permissions": permissions,
                "events_retained": events_retained,
                "events_cascaded": false,
            }),
        )
        .await?;
        self.emit(
            session_id,
            None,
            &session.runtime_id,
            EventPayload::SessionClosed(SessionClosedPayload {
                session_id: session_id.clone(),
                closed_at: now,
                reason: Some("deleted".to_owned()),
            }),
        )
        .await?;
        Ok(())
    }

    /// 关闭会话（取消树根动作）：取消会话节点（级联全部 run 子节点与权限等待）→
    /// 取消在途 run；**子会话按 `parent_session_id` 递归关闭**（D8 父取消级联）；
    /// 空闲会话置 `completed`，有在途 run 置 `cancelled`；广播 `session.closed`
    /// （D9 最小审计 + 会话生命周期事件）。
    pub async fn dispose(&self, session_id: &SessionId) -> Result<SessionStatus, LifecycleError> {
        // 父取消级联：先关闭全部后代（最深优先），再关闭本会话。
        let descendants = self.descendants_of(session_id).await?;
        for child in descendants.iter().rev() {
            self.dispose_single(child).await?;
        }
        self.dispose_single(session_id).await
    }

    /// 关闭单个会话（不含后代遍历；[`SessionManager::dispose`] 的级联单元）。
    async fn dispose_single(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionStatus, LifecycleError> {
        // 会话节点取消：run 子节点与绑定其上的权限等待级联取消（D8）。
        self.inner.cancel_tree.cancel_session(session_id);
        let report = self.interrupt(session_id).await?;
        // M7-02（ADR-022 附录 A-6）：闸门排队的当前 run 被 interrupt 时等待槽不连带
        // 清空（可能已提升为当前 run）；dispose 为会话级全清（取消树语义），补收残余。
        let residual = self.cancel_residual_runs(session_id).await?;
        let target = if report.interrupted_run.is_some()
            || report.cancelled_waiting_run.is_some()
            || residual
        {
            SessionStatus::Cancelled
        } else {
            SessionStatus::Completed
        };
        let now = self.inner.clock.now_ms();
        let mut state = lock_state(&self.inner).await;
        self.hydrate_locked(&mut state, session_id).await?;
        let (change, runtime_id, target) = {
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            // M7-03/ADR-017 附录 A-6：paused 态关闭 → 终态 `cancelled`（修正既有
            // `dispose_single` 在 paused 无在途时误算 `completed` 的路径，避免命中
            // 白名单外转移；`paused → cancelled` 为既有边）。
            let target = if session.fsm.status() == SessionStatus::Paused {
                SessionStatus::Cancelled
            } else {
                target
            };
            (
                session.fsm.transition(target)?,
                session.runtime_id.clone(),
                target,
            )
        };
        self.persist_status_and_emit(session_id, change, true, &runtime_id)
            .await?;
        drop(state);
        // M3-07（D9/SE-03 最小审计）：会话生命周期边界事件落审计（关闭）。
        self.insert_audit(
            session_id,
            "user",
            "session.closed",
            target.as_str(),
            json!({
                "closed_at": now,
            }),
        )
        .await?;
        self.emit(
            session_id,
            None,
            &runtime_id,
            EventPayload::SessionClosed(SessionClosedPayload {
                session_id: session_id.clone(),
                closed_at: now,
                reason: None,
            }),
        )
        .await?;
        Ok(target)
    }

    /// 取消会话内残余 run（M7-02 闸门语义补充；dispose 全清辅助）。
    ///
    /// 覆盖：闸门排队 run（从 FIFO 移除 + 取消）与 interrupt 后提升的 run（取消；
    /// 已派发时同时释放闸门槽位并按 FIFO 推进）。返回是否存在被取消的残余 run。
    async fn cancel_residual_runs(&self, session_id: &SessionId) -> Result<bool, LifecycleError> {
        let mut state = lock_state(&self.inner).await;
        self.hydrate_locked(&mut state, session_id).await?;
        let (active, waiting, released) = {
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            let active = session.active.take();
            let waiting = session.waiting.take();
            let released = active
                .as_ref()
                .map(|active| active.dispatched)
                .unwrap_or(false);
            if let Some(active) = &active {
                if active.dispatched {
                    active.cancel.cancel();
                }
            }
            (active, waiting, released)
        };
        if let Some(active) = &active {
            if !active.dispatched {
                state.gate.queue.remove(&active.run_id);
            }
        }
        let mut found = false;
        if let Some(active) = &active {
            self.finish_run(&active.run_id, aether_core::RunStatus::Cancelled, None)
                .await?;
            if active.dispatched {
                self.inner
                    .task_watchdog
                    .mark_orphaned_by_run(&active.run_id, self.inner.clock.now_ms());
            }
            self.emit(
                session_id,
                Some(&active.run_id),
                &active.runtime_id,
                EventPayload::RunCancelled(RunCancelledPayload {
                    run_id: active.run_id.clone(),
                    reason: Some("user_interrupt".to_owned()),
                }),
            )
            .await?;
            found = true;
        }
        if let Some(waiting) = &waiting {
            self.finish_run(&waiting.run_id, aether_core::RunStatus::Cancelled, None)
                .await?;
            self.emit(
                session_id,
                Some(&waiting.run_id),
                &waiting.runtime_id,
                EventPayload::RunCancelled(RunCancelledPayload {
                    run_id: waiting.run_id.clone(),
                    reason: Some("user_interrupt".to_owned()),
                }),
            )
            .await?;
            found = true;
        }
        let dispatches = if released {
            gate_release_and_advance(&self.inner, &mut state)
        } else {
            Vec::new()
        };
        publish_gate_snapshot(&self.inner, &state);
        drop(state);
        for (spawn_session, spawn_run) in dispatches {
            self.spawn_run(spawn_session, spawn_run);
        }
        Ok(found)
    }

    /// 收集全部非终态后代会话（BFS；`sessions.parent_session_id` 为唯一事实来源）。
    async fn descendants_of(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionId>, LifecycleError> {
        let mut descendants = Vec::new();
        let mut frontier = vec![session_id.clone()];
        while let Some(parent) = frontier.pop() {
            let children = self
                .inner
                .reads
                .sessions(SessionQuery {
                    parent_session_id: Some(parent.as_str().to_owned()),
                    ..SessionQuery::default()
                })
                .await?;
            for child in children {
                if session_status_is_terminal(child.status) {
                    continue;
                }
                descendants.push(child.id.clone());
                frontier.push(child.id);
            }
        }
        Ok(descendants)
    }

    /// 看门狗单次巡检：返回本轮判定断流的 run（已落 `run.failed` + 会话回 idle）。
    ///
    /// 背景任务按 [`LifecycleConfig::watchdog_tick`] 周期调用；测试可直接驱动以省略等待。
    pub async fn watchdog_once(&self) -> Vec<RunId> {
        let timeout = self.inner.config.run_stream_timeout_ms;
        watchdog_once_inner(&self.inner, timeout).await
    }

    /// 自动重试待执行项数（诊断/测试）。
    pub fn pending_retry_count(&self) -> usize {
        lock_pending_retries(&self.inner).len()
    }

    /// 执行到期自动重试（watchdog 周期调用；测试可直接驱动）。
    pub async fn process_pending_retries_once(&self) -> usize {
        process_pending_retries(&self.inner).await
    }

    /// 饥饿保护巡检单次（watchdog 周期调用；测试可直接驱动）。
    pub async fn sweep_starvation_guard_once(&self) -> usize {
        sweep_starvation_guard(&self.inner).await
    }

    /// 队列查询（M10-07/ADR-037 D-037-1：全量/按会话；全局队列 + 会话等待槽合并视图）。
    ///
    /// 位置按 `score ASC → queued_at ASC → run_id ASC` 排序后从 1 计；`gate.queued`
    /// 仍只计全局队列（与会话等待槽两层不重复计数）。
    pub async fn queue_list(
        &self,
        session_id: Option<&SessionId>,
        limit: usize,
        offset: usize,
    ) -> QueueListReport {
        let state = lock_state(&self.inner).await;
        let now = self.inner.clock.now_ms();
        let gate = gate_snapshot(&state, now);
        let mut candidates: Vec<GateWaiter> = state.gate.queue.entries().iter().cloned().collect();
        for (id, session) in &state.sessions {
            let Some(waiting) = &session.waiting else {
                continue;
            };
            candidates.push(GateWaiter::new(
                id.clone(),
                waiting.run_id.clone(),
                waiting.priority,
                runtime_weight(&waiting.runtime_id),
                waiting.queued_at,
                WaitReason::ConcurrencyGate,
            ));
        }
        candidates.sort_by(|left, right| {
            left.score(now)
                .cmp(&right.score(now))
                .then(left.queued_at.cmp(&right.queued_at))
                .then_with(|| left.run_id.as_str().cmp(right.run_id.as_str()))
        });
        let total = candidates
            .iter()
            .filter(|waiter| {
                session_id
                    .map(|id| &waiter.session_id == id)
                    .unwrap_or(true)
            })
            .count();
        let items: Vec<QueueEntry> = candidates
            .into_iter()
            .enumerate()
            .filter(|(_, waiter)| {
                session_id
                    .map(|id| &waiter.session_id == id)
                    .unwrap_or(true)
            })
            .map(|(index, waiter)| QueueEntry {
                run_id: waiter.run_id.clone(),
                session_id: waiter.session_id.clone(),
                position: index + 1,
                priority: waiter.base_priority,
                score: waiter.score(now),
                queued_at: waiter.queued_at,
                wait_ms: waiter.wait_ms(now),
                wait_reason: waiter.wait_reason.as_str().to_owned(),
            })
            .skip(offset)
            .take(limit.min(QUEUE_LIST_MAX_LIMIT))
            .collect();
        QueueListReport { items, total, gate }
    }

    /// 各运行时已派发 run 计数（M10-07/ADR-037 D-037-3：自动路由负载 tie-break）。
    pub async fn runtime_loads(&self) -> std::collections::BTreeMap<String, usize> {
        let state = lock_state(&self.inner).await;
        let mut loads: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for session in state.sessions.values() {
            if session
                .active
                .as_ref()
                .map(|active| active.dispatched)
                .unwrap_or(false)
            {
                *loads
                    .entry(session.runtime_id.as_str().to_owned())
                    .or_insert(0) += 1;
            }
        }
        loads
    }

    /// 会话当前 active run 的优先级（子 Agent/内部节点继承；无 → `None`）。
    pub async fn session_active_priority(&self, session_id: &SessionId) -> Option<i32> {
        let state = lock_state(&self.inner).await;
        state
            .sessions
            .get(session_id)
            .and_then(|session| session.active.as_ref())
            .map(|active| active.priority)
    }

    /// 任务看门狗单次巡检：返回本轮记录的任务 dump（M2-05；取消后超阈值未退出 →
    /// dump + 强制清理）。背景任务随看门狗周期调用；测试可直接驱动。
    pub fn sweep_tasks_once(&self) -> Vec<TaskDump> {
        self.inner.task_watchdog.sweep(self.inner.clock.now_ms())
    }

    /// 收割已完成的会话执行任务并处理 panic（M2-07；D2 `JoinError` 捕获）。
    ///
    /// 返回本轮收割的任务数；panic 任务会触发该会话 `run.failed`（`task_panic`）
    /// 与管线 sequencer 恢复，**其余会话不受影响**。背景任务随看门狗周期调用；
    /// 测试可直接驱动以获得确定性断言。
    pub async fn reap_run_tasks_once(&self) -> usize {
        reap_run_tasks(&self.inner).await
    }

    /// 任务 dump 快照（诊断包消费；M3-05 集成）。
    pub fn task_dumps(&self) -> Vec<TaskDump> {
        self.inner.task_watchdog.dumps()
    }

    /// 在册会话执行任务数（诊断/测试断言；`sweep_tasks_once` 后为真实运行数）。
    pub fn active_task_count(&self) -> usize {
        self.inner.task_watchdog.active_count()
    }

    /// 会话取消树节点句柄（M2-05；权限等待可经 [`RunCancelToken::cancelled`] 级联取消）。
    pub async fn session_cancel_token(
        &self,
        session_id: &SessionId,
    ) -> Result<RunCancelToken, LifecycleError> {
        let mut state = lock_state(&self.inner).await;
        self.hydrate_locked(&mut state, session_id).await?;
        state
            .sessions
            .get(session_id)
            .map(|session| session.cancel.clone())
            .ok_or_else(|| LifecycleError::SessionNotFound {
                session_id: session_id.clone(),
            })
    }

    /// 当前在途 run 的取消令牌（无在途 run → `None`；权限等待绑定该令牌）。
    pub async fn active_run_cancel_token(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<RunCancelToken>, LifecycleError> {
        let mut state = lock_state(&self.inner).await;
        self.hydrate_locked(&mut state, session_id).await?;
        Ok(state
            .sessions
            .get(session_id)
            .and_then(|session| session.active.as_ref())
            .map(|active| active.cancel.clone()))
    }

    /// 等待审批置位（M3-12/ADR-011 决策 1）：同会话 pending 票据计数 `0→1` 时由
    /// 权限服务观察者接入（组合根经 `PermissionService::set_pending_observer` 装配）。
    ///
    /// 置位仅当当前状态为 `running` 时生效（`running → waiting_permission`，复用既有
    /// 19 边状态机白名单）；非适用态（`idle`/`creating`/终态/已在等待）一律 **no-op**
    /// ——ask 与 run 终态竞态不产生非法/多余转移。返回是否发生转移。
    ///
    /// 状态行与 `session.status_changed` 仅由本管理器落行并广播（单一写者）。
    pub async fn mark_waiting_permission(
        &self,
        session_id: &SessionId,
    ) -> Result<bool, LifecycleError> {
        self.transition_if(
            session_id,
            SessionStatus::Running,
            SessionStatus::WaitingPermission,
        )
        .await
    }

    /// 等待审批回程（M3-12/ADR-011 决策 1）：同会话 pending 票据计数 `1→0` 时接入。
    ///
    /// 回程仅当当前状态为 `waiting_permission` 时生效（`waiting_permission → running`，
    /// 运行继续）；其余状态一律 **no-op**——重启恢复票据的迟到决议/超时、run 已收口
    /// （取消/失败/超时 → 相关转移已由既有收口路径完成）均不产生多余
    /// `session.status_changed`。返回是否发生转移。
    pub async fn clear_waiting_permission(
        &self,
        session_id: &SessionId,
    ) -> Result<bool, LifecycleError> {
        self.transition_if(
            session_id,
            SessionStatus::WaitingPermission,
            SessionStatus::Running,
        )
        .await
    }

    /// 受守卫的等待态转移（置位/回程共用）：仅当前状态恰为 `from` 且 `from → to`
    /// 命中状态机白名单时转移（落行 + 事件）；否则 no-op（不触碰状态）。
    async fn transition_if(
        &self,
        session_id: &SessionId,
        from: SessionStatus,
        to: SessionStatus,
    ) -> Result<bool, LifecycleError> {
        let mut state = lock_state(&self.inner).await;
        self.hydrate_locked(&mut state, session_id).await?;
        let (change, runtime_id) = {
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            if session.fsm.status() != from || !session.fsm.can_transition(to) {
                return Ok(false);
            }
            (session.fsm.transition(to)?, session.runtime_id.clone())
        };
        self.persist_status_and_emit(session_id, change, false, &runtime_id)
            .await?;
        Ok(true)
    }

    /// 记录 run 活动（事件监听器在任意落盘事件到达时调用；重置断流计时）。
    pub async fn touch_run(&self, run_id: &RunId) {
        let now = self.inner.clock.now_ms();
        let mut state = lock_state(&self.inner).await;
        for session in state.sessions.values_mut() {
            if let Some(active) = session.active.as_mut() {
                if &active.run_id == run_id {
                    active.last_activity_ms = now;
                    return;
                }
            }
        }
    }

    /// 启动后台任务：看门狗巡检 + 管线事件活动监听 + 降级中断监听。
    ///
    /// 返回本管理器持有的任务数（诊断）；任务在 [`SessionManager::shutdown_background`]
    /// 前持续运行。
    pub fn spawn_background(&self, handle: &Handle) -> usize {
        let watchdog_inner = Arc::clone(&self.inner);
        let watchdog = handle.spawn(async move {
            loop {
                let tick = watchdog_inner.config.watchdog_tick;
                tokio::time::sleep(tick).await;
                let timeout = watchdog_inner.config.run_stream_timeout_ms;
                let _ = watchdog_once_inner(&watchdog_inner, timeout).await;
                // M10-07/ADR-037：饥饿保护审计 + 到期自动重试（每 tick 一次）。
                let _ = sweep_starvation_guard(&watchdog_inner).await;
                let _ = process_pending_retries(&watchdog_inner).await;
                // M2-07：收割会话执行任务；panic 经 JoinError 捕获并恢复（D2 不传染）。
                let _ = reap_run_tasks(&watchdog_inner).await;
                // M2-05：取消后 10s 未退出的会话任务 → dump + 强制清理。
                let _ = watchdog_inner
                    .task_watchdog
                    .sweep(watchdog_inner.clock.now_ms());
            }
        });

        let events_inner = Arc::clone(&self.inner);
        let mut events = self.inner.pipeline.subscribe();
        let event_listener = handle.spawn(async move {
            loop {
                match events.recv().await {
                    Ok(envelope) => {
                        if let Some(run_id) = envelope.run_id.clone() {
                            let now = events_inner.clock.now_ms();
                            let mut state = lock_state(&events_inner).await;
                            for session in state.sessions.values_mut() {
                                if let Some(active) = session.active.as_mut() {
                                    if active.run_id == run_id {
                                        active.last_activity_ms = now;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let interrupt_inner = Arc::clone(&self.inner);
        let mut interrupts = self.inner.pipeline.subscribe_run_interrupts();
        let interrupt_listener = handle.spawn(async move {
            loop {
                match interrupts.recv().await {
                    Ok(interrupt) => {
                        let _ = finalize_degraded_interrupt(&interrupt_inner, interrupt).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let mut background = match self.inner.background.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        background.push(watchdog);
        background.push(event_listener);
        background.push(interrupt_listener);
        // M7-02/ADR-022：空闲内存采样任务（启动即采样 + 按 `GATE_RAM_SAMPLE_INTERVAL`
        // 巡检重算闸门上限；快照总是在巡检后发布——`health.concurrency_gate` 在首次
        // 活动前即可读）。仅接线采样源或快照单元时启动（未接线 = 不巡检）。
        let gate_wired = {
            let free_ram = lock_free_ram(&self.inner).is_some();
            let cell = lock_gate_cell(&self.inner).is_some();
            free_ram || cell
        };
        if gate_wired {
            let gate_inner = Arc::clone(&self.inner);
            let gate_sampler = handle.spawn(async move {
                loop {
                    let _ = refresh_gate_limit_inner(&gate_inner).await;
                    tokio::time::sleep(GATE_RAM_SAMPLE_INTERVAL).await;
                }
            });
            background.push(gate_sampler);
        }
        background.len()
    }

    /// 停止后台任务（关闭序列；等待任务退出）。
    pub async fn shutdown_background(&self) {
        let handles: Vec<JoinHandle<()>> = {
            let mut background = match self.inner.background.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            background.drain(..).collect()
        };
        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
    }

    // ===== 内部（调用方持有状态锁的路径不再重复加锁）=====

    fn spawn_run(&self, session_id: SessionId, run_id: RunId) {
        spawn_run_inner(&self.inner, session_id, run_id);
    }

    async fn hydrate_locked(
        &self,
        state: &mut ManagerState,
        session_id: &SessionId,
    ) -> Result<(), LifecycleError> {
        hydrate_inner(&self.inner, state, session_id).await
    }

    /// 会话状态行 + `session.status_changed` 事件（调用方持有状态锁；内存状态已转移）。
    async fn persist_status_and_emit(
        &self,
        session_id: &SessionId,
        change: aether_core::SessionStatusChange,
        closed: bool,
        runtime_id: &RuntimeId,
    ) -> Result<(), LifecycleError> {
        let now = self.inner.clock.now_ms();
        self.inner
            .write
            .execute(StoreCommand::UpdateSessionStatus {
                session_id: session_id.clone(),
                status: change.to,
                updated_at: now,
                closed_at: if closed { Some(now) } else { None },
            })
            .await?;
        // M3-07（D9/SE-03 最小审计）：会话生命周期状态转移落审计。
        insert_audit_inner(
            &self.inner,
            session_id,
            "system",
            "session.status_changed",
            &status_transition_result(change.from, change.to),
            json!({
                "from": change.from.as_str(),
                "to": change.to.as_str(),
            }),
        )
        .await?;
        self.emit(
            session_id,
            None,
            runtime_id,
            EventPayload::SessionStatusChanged(SessionStatusChangedPayload {
                session_id: session_id.clone(),
                from: change.from,
                to: change.to,
            }),
        )
        .await?;
        Ok(())
    }

    async fn finish_run(
        &self,
        run_id: &RunId,
        status: aether_core::RunStatus,
        error: Option<String>,
    ) -> Result<(), LifecycleError> {
        self.inner
            .write
            .execute(StoreCommand::FinishRun {
                run_id: run_id.clone(),
                status,
                error,
                finished_at: self.inner.clock.now_ms(),
            })
            .await?;
        Ok(())
    }

    /// 会话生命周期审计写入（M3-07；D9/SE-03 P0 最小集）。
    ///
    /// 经单写队列追加 `audit_log` 行（`actor`/`resource`/`result`/`ts` 齐备；应用层
    /// 无 UPDATE/DELETE 路径）。审计动作限定三类：会话生命周期 / 权限决议 /
    /// 适配器状态变化。
    async fn insert_audit(
        &self,
        session_id: &SessionId,
        actor: &str,
        action: &str,
        result: &str,
        detail: Value,
    ) -> Result<(), LifecycleError> {
        insert_audit_inner(&self.inner, session_id, actor, action, result, detail).await
    }

    async fn emit(
        &self,
        session_id: &SessionId,
        run_id: Option<&RunId>,
        runtime_id: &RuntimeId,
        payload: EventPayload,
    ) -> Result<(), LifecycleError> {
        let value = payload
            .to_value()
            .map_err(|error| LifecycleError::Internal {
                reason: format!("事件 payload 序列化失败: {error}"),
            })?;
        let event_type = payload.event_type();
        let mut object = serde_json::Map::with_capacity(ENVELOPE_FIELDS.len());
        object.insert("v".to_owned(), Value::from(EVENT_ENVELOPE_VERSION));
        object.insert("id".to_owned(), Value::from(ulid::generate()));
        object.insert("session_id".to_owned(), Value::from(session_id.as_str()));
        object.insert(
            "run_id".to_owned(),
            run_id.map_or(Value::Null, |run_id| Value::from(run_id.as_str())),
        );
        object.insert("runtime_id".to_owned(), Value::from(runtime_id.as_str()));
        object.insert("seq".to_owned(), Value::from(0_u64));
        object.insert("ts".to_owned(), Value::from(self.inner.clock.now_ms()));
        object.insert("type".to_owned(), Value::from(event_type.as_str()));
        object.insert("payload".to_owned(), value);
        match self
            .inner
            .pipeline
            .submit(Value::Object(object))
            .await
            .map_err(LifecycleError::from)?
        {
            SubmitOutcome::Persisted { .. } | SubmitOutcome::Buffered => Ok(()),
            SubmitOutcome::Duplicate { .. } => Err(LifecycleError::Internal {
                reason: "生命周期事件意外命中幂等去重".to_owned(),
            }),
            SubmitOutcome::DeadLettered { code, reason } => Err(LifecycleError::Internal {
                reason: format!("生命周期事件被死信（{code}）：{reason}"),
            }),
        }
    }
}

/// 权限服务等待态观察者（M3-12/ADR-011）：把「同会话 pending 计数 0↔1」薄映射到
/// [`SessionManager::mark_waiting_permission`] / [`SessionManager::clear_waiting_permission`]
/// （状态守卫在管理器内；非适用态 no-op）。
///
/// 仅上报、不阻断：任何控制层错误（如会话行缺失）只记录诊断——权限决议路径
/// （allow/deny/超时/取消）不因等待态写入失败而改变语义。
impl crate::permission::PendingTicketObserver for SessionManager {
    fn on_pending(&self, session_id: SessionId) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            match self.mark_waiting_permission(&session_id).await {
                Ok(_transitioned) => {}
                Err(error) => tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "等待审批置位失败（仅记录，不阻断权限路径）"
                ),
            }
        })
    }

    fn on_pending_cleared(
        &self,
        session_id: SessionId,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            match self.clear_waiting_permission(&session_id).await {
                Ok(_transitioned) => {}
                Err(error) => tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "等待审批回程失败（仅记录，不阻断权限路径）"
                ),
            }
        })
    }
}

/// 执行器派发与终态落库（独立任务；ack 已返回）。
///
/// 单会话 run 串行：等待队列的提升在**同任务内循环**处理（不递归 spawn——递归
/// future 无法证明 `Send`）；其他会话的任务并行。
async fn run_active(
    inner: &Arc<ManagerInner>,
    session_id: SessionId,
    run_id: RunId,
) -> Result<(), LifecycleError> {
    let Some(mut current) = take_run_start(inner, &session_id, &run_id).await? else {
        return Ok(());
    };
    loop {
        match dispatch_and_finalize(inner, &session_id, current).await? {
            Some(next) => current = next,
            None => return Ok(()),
        }
    }
}

/// 经 `JoinSet` spawn 单会话执行任务（M2-07；D2 长驻任务统一经 `JoinSet` 管理）。
///
/// 任务名与 `AbortHandle` 登记进看门狗（M2-05 取消兜底）；`task::Id → (会话, run)`
/// 映射供 [`reap_run_tasks`] 在 panic 时定位会话。
///
/// M7-02：**同步函数**（`run_tasks` 为非异步互斥）——run 任务内推进闸门 FIFO 时可安全
/// 再 spawn（不形成递归 future 的 `Send` 证明环）。
fn spawn_run_inner(inner: &Arc<ManagerInner>, session_id: SessionId, run_id: RunId) {
    let task_inner = Arc::clone(inner);
    let task_session = session_id.clone();
    let task_run = run_id.clone();
    let task_name = format!("session:{} run:{}", session_id.as_str(), run_id.as_str());
    let started_at_ms = inner.clock.now_ms();
    let abort: AbortHandle = {
        let mut set = lock_run_tasks(inner);
        set.spawn(async move {
            let _ = run_active(&task_inner, task_session, task_run).await;
        })
    };
    let task_id = abort.id();
    match inner.run_task_index.lock() {
        Ok(mut index) => {
            index.insert(task_id, (session_id.clone(), run_id.clone()));
        }
        Err(poisoned) => {
            poisoned
                .into_inner()
                .insert(task_id, (session_id.clone(), run_id.clone()));
        }
    }
    // M2-05：登记任务（取消后 10s 未退出 → dump + 强制清理）。
    inner
        .task_watchdog
        .register(task_name, session_id, run_id, started_at_ms, abort);
}

/// 收割已完成的会话执行任务（M2-07）：`JoinError::is_panic()` → 该会话隔离恢复。
///
/// 返回本轮收割的任务数。取消（`abort`）与正常完成不产生恢复动作。
async fn reap_run_tasks(inner: &Arc<ManagerInner>) -> usize {
    let mut reaped = 0usize;
    loop {
        let joined = {
            let mut set = lock_run_tasks(inner);
            set.try_join_next_with_id()
        };
        let Some(joined) = joined else { break };
        reaped += 1;
        let task_id = match &joined {
            Ok((id, ())) => *id,
            Err(error) => error.id(),
        };
        let target = match inner.run_task_index.lock() {
            Ok(mut index) => index.remove(&task_id),
            Err(poisoned) => poisoned.into_inner().remove(&task_id),
        };
        match joined {
            Ok((_id, ())) => {}
            Err(error) if error.is_panic() => {
                let detail = panic_detail(error);
                match target {
                    Some((session_id, run_id)) => {
                        handle_run_task_panic(inner, session_id, run_id, detail).await;
                    }
                    None => {
                        tracing::error!(
                            panic = %detail,
                            "会话执行任务 panic（JoinError）：任务索引缺失，仅记录（D2）"
                        );
                    }
                }
            }
            Err(_cancelled) => {}
        }
    }
    reaped
}

/// 提取 panic payload 文本（`&str` / `String`；其余类型以占位描述呈现）。
fn panic_detail(error: JoinError) -> String {
    let payload = error.into_panic();
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "非字符串 panic payload".to_owned()
    }
}

/// panic 隔离恢复（M2-07；D2 失败表）：
/// 1. 记录 JoinError（会话/run/panic 详情）；
/// 2. 管线 sequencer 恢复（丢弃未落盘 delta，下一次提交从库中 `max(seq)+1` 继续）；
/// 3. 在途 run 标 `failed`（`task_panic`，可重试）且会话回 `idle`；
/// 4. 若等待队列已提升，重新 spawn 该会话任务续跑（run 串行不变）。
async fn handle_run_task_panic(
    inner: &Arc<ManagerInner>,
    session_id: SessionId,
    run_id: RunId,
    detail: String,
) {
    tracing::error!(
        target: "aether_control::lifecycle",
        session_id = %session_id,
        run_id = %run_id,
        panic = %detail,
        "会话执行任务 panic（JoinError）：仅该会话标记 failed，其余会话不受影响（D2）"
    );
    // D4 sequencer 崩溃恢复：丢弃未落盘 delta，下一次提交从库中 max(seq)+1 恢复。
    if let Err(error) = inner.pipeline.restart_session(session_id.clone()).await {
        tracing::warn!(
            session_id = %session_id,
            error = %error,
            "panic 后 sequencer 恢复调用失败（继续终态收口）"
        );
    }

    let active_run = {
        let state = lock_state(inner).await;
        state
            .sessions
            .get(&session_id)
            .and_then(|session| session.active.as_ref())
            .map(|active| active.run_id.clone())
    };
    let Some(active_run) = active_run else {
        // 无在途 run：panic 发生在终态收尾窗口时确保会话不卡在 running。
        let runtime_id = {
            let state = lock_state(inner).await;
            state
                .sessions
                .get(&session_id)
                .map(|session| session.runtime_id.clone())
        };
        if let Some(runtime_id) = runtime_id {
            if let Err(error) = settle_session_idle(inner, &session_id, &runtime_id).await {
                tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "panic 后会话 idle 收口失败"
                );
            }
        }
        return;
    };
    if active_run != run_id {
        // M7-02：panic 任务属于已收口 run（超时/中断后迟到），活跃位已归其他 run
        // （等待槽提升等）——不得误伤当前 run（活跃位匹配为唯一收口依据）。
        tracing::warn!(
            session_id = %session_id,
            run_id = %run_id,
            active_run = %active_run,
            "panic 任务与活跃位不匹配（迟到 panic）：仅记录，不触碰当前 run（D2）"
        );
        return;
    }

    let error = ErrorInfo {
        code: RUN_TASK_PANIC_CODE.to_owned(),
        message: format!(
            "会话执行任务 panic（JoinError）：{detail}；仅该会话标记 failed（D2），\
             重放按 ADR-005 恢复模式"
        ),
        recoverable: true,
    };
    match finalize_failed(inner, &session_id, &active_run, error).await {
        Ok(Some(next)) => {
            // 等待队列已提升：原任务已死，重新 spawn 该会话任务继续执行。
            spawn_run_inner(inner, session_id.clone(), next.run_id.clone());
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!(
                session_id = %session_id,
                run_id = %active_run,
                error = %error,
                "panic 后 run 终态收口失败（等待看门狗/重启恢复）"
            );
        }
    }
}

/// 从活跃位取得 run 启动信息（不匹配 = 已被超时/中断摘除）。
async fn take_run_start(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    run_id: &RunId,
) -> Result<Option<RunStart>, LifecycleError> {
    let state = lock_state(inner).await;
    let session =
        state
            .sessions
            .get(session_id)
            .ok_or_else(|| LifecycleError::SessionNotFound {
                session_id: session_id.clone(),
            })?;
    let Some(active) = session.active.as_ref() else {
        return Ok(None);
    };
    // M7-02/ADR-022：仅已派发 run 由执行任务接管；闸门排队 run 不启动执行。
    if &active.run_id != run_id || !active.dispatched {
        return Ok(None);
    }
    Ok(Some(RunStart {
        run_id: active.run_id.clone(),
        input_message_id: active.input_message_id.clone(),
        runtime_id: active.runtime_id.clone(),
        text: active.text.clone(),
        thinking_depth: active.thinking_depth,
        thinking_override: active.thinking_override,
        references: active.references.clone(),
        attachments: active.attachments.clone(),
        subagent_context: active.subagent_context.clone(),
        cancel: active.cancel.clone(),
        priority: active.priority,
        timeout_ms: active.timeout_ms,
        max_attempts: active.max_attempts,
        retry_count: active.retry_count,
        queued_at: active.queued_at,
    }))
}

struct RunStart {
    run_id: RunId,
    input_message_id: MessageId,
    runtime_id: RuntimeId,
    text: String,
    thinking_depth: u8,
    thinking_override: bool,
    references: Vec<MessageReference>,
    attachments: Vec<aether_core::OutboundAttachment>,
    subagent_context: Option<aether_core::SubagentContext>,
    cancel: RunCancelToken,
    priority: i32,
    timeout_ms: i64,
    max_attempts: u32,
    retry_count: u32,
    queued_at: i64,
}

async fn dispatch_and_finalize(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    start: RunStart,
) -> Result<Option<RunStart>, LifecycleError> {
    let request = RunRequest {
        session_id: session_id.clone(),
        run_id: start.run_id.clone(),
        runtime_id: start.runtime_id.clone(),
        input_message_id: start.input_message_id.clone(),
        text: start.text.clone(),
        thinking_depth: start.thinking_depth,
        thinking_override: start.thinking_override,
        references: start.references.clone(),
        attachments: start.attachments.clone(),
        subagent_context: start.subagent_context.clone(),
        cancel: start.cancel.clone(),
    };
    // queued → running（行）→ run.started（事件）→ 会话 running。
    inner
        .write
        .execute(StoreCommand::StartRun {
            run_id: start.run_id.clone(),
            started_at: inner.clock.now_ms(),
        })
        .await?;
    emit_inner(
        inner,
        session_id,
        Some(&start.run_id),
        &start.runtime_id,
        EventPayload::RunStarted(RunStartedPayload {
            run_id: start.run_id.clone(),
        }),
    )
    .await?;
    {
        let mut state = lock_state(inner).await;
        if let Some(session) = state.sessions.get_mut(session_id) {
            if session.fsm.status() == SessionStatus::Idle {
                let change = session.fsm.transition(SessionStatus::Running)?;
                persist_status_inner(inner, session_id, change, false, &start.runtime_id).await?;
            }
        }
    }

    // 执行（真实模型调用在 M2-02 接入；此 await 不阻塞 ack）。
    let outcome = inner.executor.execute(request).await;
    match outcome {
        ExecutorOutcome::Completed {
            assistant_text,
            usage,
        } => finalize_completed(inner, session_id, &start.run_id, assistant_text, usage).await,
        ExecutorOutcome::Failed { error } => {
            finalize_failed(inner, session_id, &start.run_id, error).await
        }
        ExecutorOutcome::Cancelled { reason } => {
            finalize_cancelled(inner, session_id, &start.run_id, reason).await
        }
    }
}

/// 终态公共流程：摘除活跃位 → 行终态 → 事件 → 会话 idle；等待队列原子提升为
/// 活跃位（返回 `RunStart`，由调用方在同任务内继续执行）。
///
/// 返回 `None` 表示该 run 已被超时/中断处理（迟到结果忽略）。
/// 终态调度（M7-02/ADR-022 附录 A-6）：run 终态后的两层队列合成结果。
struct TerminalSchedule {
    /// 经全局闸门推进的 run（已置 `dispatched=true`；调用方 `spawn`）。
    dispatches: Vec<(SessionId, RunId)>,
    /// 同会话等待槽提升且已派发（本任务续跑，不重复 spawn）。
    next: Option<RunStart>,
    /// 同会话等待槽提升但未派发（仅降级准入失败；由收口方按 D4 语义连带取消）。
    suspended: Option<RunStart>,
}

/// 摘除的活跃 run 元数据（M10-07：超时/自动重试判定）。
struct DetachedRun {
    run_id: RunId,
    session_id: SessionId,
    input_message_id: MessageId,
    runtime_id: RuntimeId,
    priority: i32,
    timeout_ms: i64,
    max_attempts: u32,
    retry_count: u32,
    timeout_requested_at_ms: Option<i64>,
}

/// 终态公共流程：摘除活跃位 → 等待槽提升 → 闸门调度（FIFO 推进/降级挂起）→
/// 行终态 → 事件 → 会话 idle；返回调度结果（`None` = 该 run 已被超时/中断处理）。
///
/// M7-02/ADR-022 合成顺序（单写者）：当前 run 终态释放槽位 → 全局 FIFO 按序推进
/// （仅新 run 准入可行时）→ 等待槽 run 提升判定（有空槽 → 派发；饱和 → FIFO 尾部；
/// 降级 → 不派发不排队，由收口方连带取消）。
async fn detach_and_prepare(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    run_id: &RunId,
) -> Result<Option<(DetachedRun, TerminalSchedule)>, LifecycleError> {
    let mut state = lock_state(inner).await;
    let (detached, promoted) =
        {
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            match session.active.as_ref() {
                Some(active) if &active.run_id == run_id => {
                    let active = session
                        .active
                        .take()
                        .ok_or_else(|| LifecycleError::Internal {
                            reason: "active 位竞态".to_owned(),
                        })?;
                    let detached = DetachedRun {
                        run_id: active.run_id.clone(),
                        session_id: session_id.clone(),
                        input_message_id: active.input_message_id.clone(),
                        runtime_id: active.runtime_id.clone(),
                        priority: active.priority,
                        timeout_ms: active.timeout_ms,
                        max_attempts: active.max_attempts,
                        retry_count: active.retry_count,
                        timeout_requested_at_ms: active.timeout_requested_at_ms,
                    };
                    let promoted = session.waiting.take().map(|queued| RunStart {
                        run_id: queued.run_id,
                        input_message_id: queued.input_message_id,
                        runtime_id: queued.runtime_id,
                        text: queued.text,
                        thinking_depth: queued.thinking_depth,
                        thinking_override: queued.thinking_override,
                        references: queued.references,
                        attachments: queued.attachments,
                        subagent_context: queued.subagent_context,
                        // M2-05：提升的 run 亦为会话取消树子节点。
                        cancel: RunCancelToken::child_of(&session.cancel),
                        priority: queued.priority,
                        timeout_ms: queued.timeout_ms,
                        max_attempts: queued.max_attempts,
                        retry_count: queued.retry_count,
                        queued_at: queued.queued_at,
                    });
                    if let Some(start) = &promoted {
                        session.active = Some(ActiveRun {
                            run_id: start.run_id.clone(),
                            input_message_id: start.input_message_id.clone(),
                            runtime_id: start.runtime_id.clone(),
                            text: start.text.clone(),
                            thinking_depth: start.thinking_depth,
                            thinking_override: start.thinking_override,
                            references: start.references.clone(),
                            attachments: start.attachments.clone(),
                            subagent_context: start.subagent_context.clone(),
                            last_activity_ms: inner.clock.now_ms(),
                            cancel: start.cancel.clone(),
                            dispatched: false,
                            priority: start.priority,
                            timeout_ms: start.timeout_ms,
                            max_attempts: start.max_attempts,
                            retry_count: start.retry_count,
                            queued_at: start.queued_at,
                            dispatched_at_ms: None,
                            timeout_requested_at_ms: None,
                        });
                    }
                    (detached, promoted)
                }
                _ => return Ok(None),
            }
        };

    // 闸门调度（锁内单写者）：释放（终态 run 已摘除）→ 队列推进 → 等待槽提升判定。
    let now = inner.clock.now_ms();
    let admission_ok = inner.pipeline.admission().is_ok();
    let dispatches = if admission_ok {
        gate_advance_locked(&mut state, now)
    } else {
        Vec::new()
    };
    let has_slot = admission_ok && gate_has_slot(&state);
    let (next, suspended) = match promoted {
        None => (None, None),
        Some(start) => {
            if !admission_ok {
                // 降级（准入失败）：提升但不派发、不入队；由收口方连带取消（D4 语义 3）。
                (None, Some(start))
            } else if has_slot {
                if let Some(active) = state
                    .sessions
                    .get_mut(session_id)
                    .and_then(|session| session.active.as_mut())
                {
                    if active.run_id == start.run_id {
                        active.dispatched = true;
                        active.dispatched_at_ms = Some(now);
                    }
                }
                (Some(start), None)
            } else {
                // 等待槽 run 晋升时闸门饱和 → 进入优先级队列（ADR-022 附录 A-6）。
                let runtime = start.runtime_id.clone();
                state.gate.queue.enqueue(GateWaiter::new(
                    session_id.clone(),
                    start.run_id.clone(),
                    start.priority,
                    runtime_weight(&runtime),
                    start.queued_at,
                    WaitReason::ConcurrencyGate,
                ));
                (None, None)
            }
        }
    };
    publish_gate_snapshot(inner, &state);
    Ok(Some((
        detached,
        TerminalSchedule {
            dispatches,
            next,
            suspended,
        },
    )))
}

/// 应用终态调度：跨会话推进 run 逐个 spawn（单写者顺序）；降级挂起的提升 run 按
/// D4 语义连带取消（`persist_degraded`），返回本任务续跑的提升 run。
async fn apply_terminal_schedule(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    schedule: TerminalSchedule,
) -> Result<Option<RunStart>, LifecycleError> {
    for (spawn_session, spawn_run) in schedule.dispatches {
        spawn_run_inner(inner, spawn_session, spawn_run);
    }
    if let Some(suspended) = schedule.suspended {
        // 降级（准入失败）：提升的 run 不派发；连带取消（不递归 finalize_cancelled，
        // 该 run 已在活跃位且无等待槽——等价原 `next` 处理路径）。
        cancel_suspended_run(inner, session_id, &suspended).await?;
    }
    Ok(schedule.next)
}

/// 降级挂起 run 的连带取消（活跃位摘除 → `cancelled` 行 + `run.cancelled` 事件；
/// 未派发故不释放槽位、不触发 FIFO 推进）。
async fn cancel_suspended_run(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    suspended: &RunStart,
) -> Result<(), LifecycleError> {
    let runtime_id =
        {
            let mut state = lock_state(inner).await;
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            match session.active.as_ref() {
                Some(active) if active.run_id == suspended.run_id => {
                    let active = session
                        .active
                        .take()
                        .ok_or_else(|| LifecycleError::Internal {
                            reason: "active 位竞态".to_owned(),
                        })?;
                    active.runtime_id
                }
                _ => return Ok(()),
            }
        };
    inner
        .write
        .execute(StoreCommand::FinishRun {
            run_id: suspended.run_id.clone(),
            status: aether_core::RunStatus::Cancelled,
            error: None,
            finished_at: inner.clock.now_ms(),
        })
        .await?;
    emit_inner(
        inner,
        session_id,
        Some(&suspended.run_id),
        &runtime_id,
        EventPayload::RunCancelled(RunCancelledPayload {
            run_id: suspended.run_id.clone(),
            reason: Some("persist_degraded".to_owned()),
        }),
    )
    .await
}

/// 自动重试登记（M10-07/ADR-037 D-037-2）：仅 `max_attempts>1` 且错误分类可重试，
/// 按 1s/2s/4s/8s/16s/30s/60s ±20% 退避（run id 确定性抖动）。
fn schedule_auto_retry(inner: &Arc<ManagerInner>, detached: &DetachedRun, code: &str) {
    if detached.max_attempts <= 1 || !is_auto_retryable(code) {
        return;
    }
    let attempt = detached.retry_count.saturating_add(1);
    if attempt >= detached.max_attempts {
        return;
    }
    let backoff_ms = backoff_with_jitter_ms(attempt, detached.run_id.as_str());
    let due_at_ms = inner.clock.now_ms().saturating_add(backoff_ms);
    let pending = PendingRetry {
        run_id: detached.run_id.clone(),
        session_id: detached.session_id.clone(),
        input_message_id: detached.input_message_id.clone(),
        priority: detached.priority,
        timeout_ms: detached.timeout_ms,
        max_attempts: detached.max_attempts,
        retry_count: detached.retry_count,
        attempt,
        due_at_ms,
        backoff_ms,
    };
    let mut list = lock_pending_retries(inner);
    list.retain(|item| item.run_id != detached.run_id);
    list.push(pending);
}

/// 移除同一 run 的待执行自动重试（人工 `run_retry` 优先；同一 run 仅一个在途 attempt）。
fn remove_pending_retry_run(inner: &Arc<ManagerInner>, run_id: &RunId) -> bool {
    let mut list = lock_pending_retries(inner);
    let before = list.len();
    list.retain(|item| &item.run_id != run_id);
    before != list.len()
}

/// 执行到期的自动重试（watchdog 周期调用；返回创建的新 attempt 数）。
async fn process_pending_retries(inner: &Arc<ManagerInner>) -> usize {
    let now = inner.clock.now_ms();
    let due: Vec<PendingRetry> = {
        let mut list = lock_pending_retries(inner);
        let mut due = Vec::new();
        let mut index = 0;
        while index < list.len() {
            if list[index].due_at_ms <= now {
                due.push(list.remove(index));
            } else {
                index += 1;
            }
        }
        due
    };
    let mut created = 0;
    for pending in due {
        match create_auto_retry_run(inner, pending).await {
            Ok(true) => created += 1,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(error = %error, "自动重试创建失败（旧 run 保留终态，可人工 run_retry）");
            }
        }
    }
    created
}

/// 单一到期项的重试 run 创建（锁内复核终态/attempt 链/会话状态，不重复执行）。
async fn create_auto_retry_run(
    inner: &Arc<ManagerInner>,
    pending: PendingRetry,
) -> Result<bool, LifecycleError> {
    let run =
        inner
            .reads
            .run(&pending.run_id)
            .await?
            .ok_or_else(|| LifecycleError::RunNotFound {
                run_id: pending.run_id.clone(),
            })?;
    if !run_is_retryable(run.status) {
        return Ok(false);
    }
    let code = run.error.as_deref().unwrap_or("unknown");
    if !is_auto_retryable(code) {
        return Ok(false);
    }
    // attempt 链：仅当前 run 是最新 attempt 且无在途 attempt 时才创建。
    let attempts = inner
        .reads
        .runs_for_input_message(&pending.input_message_id)
        .await?;
    if attempts.iter().any(|item| {
        matches!(
            item.status,
            aether_core::RunStatus::Queued | aether_core::RunStatus::Running
        )
    }) {
        return Ok(false);
    }
    if attempts
        .first()
        .map(|item| item.run_id != pending.run_id)
        .unwrap_or(true)
    {
        return Ok(false);
    }
    let input_message = inner
        .reads
        .message(&pending.input_message_id)
        .await?
        .ok_or_else(|| LifecycleError::Internal {
            reason: format!(
                "自动重试：输入消息 {} 不存在（审计数据不完整）",
                pending.input_message_id
            ),
        })?;
    let attachments = if crate::attachments::message_has_attachments(&input_message) {
        let resolver = { lock_attachment_resolver(inner).clone() };
        let Some(resolver) = resolver else {
            tracing::warn!("自动重试：附件解析器未接线，跳过重试（不静默丢弃附件）");
            return Ok(false);
        };
        match resolver.resolve(&pending.session_id, &input_message).await {
            Ok(items) => items,
            Err(error) => {
                tracing::warn!(error = ?error, "自动重试：附件重新水合失败，跳过重试");
                return Ok(false);
            }
        }
    } else {
        Vec::new()
    };
    inner.pipeline.admission().map_err(LifecycleError::from)?;
    let session_row = inner
        .reads
        .session(&pending.session_id)
        .await?
        .ok_or_else(|| LifecycleError::SessionNotFound {
            session_id: pending.session_id.clone(),
        })?;
    if session_row.status == SessionStatus::Paused
        || aether_core::session_status_is_terminal(session_row.status)
    {
        return Ok(false);
    }
    let now = inner.clock.now_ms();
    let retry_count = pending.retry_count.saturating_add(1);
    let new_run = Run {
        id: RunId::new(ulid::generate()).map_err(internal_from)?,
        session_id: pending.session_id.clone(),
        status: aether_core::RunStatus::Queued,
        input_message_id: Some(pending.input_message_id.clone()),
        thinking_depth: Some(session_row.thinking_depth),
        error: None,
        started_at: now,
        finished_at: None,
    };
    inner
        .write
        .execute(StoreCommand::InsertRun {
            run: new_run.clone(),
        })
        .await?;
    inner
        .write
        .execute(StoreCommand::UpdateRunSchedule {
            run_id: new_run.id.clone(),
            schedule: aether_store::RunSchedule {
                priority: pending.priority,
                queued_at: Some(now),
                retry_count,
                max_attempts: pending.max_attempts,
                wait_reason: Some(WaitReason::ConcurrencyGate.as_str().to_owned()),
            },
        })
        .await?;
    let (queued, gate_dispatched) = {
        let mut state = lock_state(inner).await;
        hydrate_inner(inner, &mut state, &pending.session_id).await?;
        let has_slot = gate_has_slot(&state);
        let queue_full = state.gate.queue.is_full();
        let Some(session) = state.sessions.get_mut(&pending.session_id) else {
            return Ok(false);
        };
        if session.fsm.is_terminal() || session.fsm.status() == SessionStatus::Paused {
            return Ok(false);
        }
        if session.active.is_some() && session.waiting.is_some() {
            return Ok(false);
        }
        if session.active.is_none() && !has_slot && queue_full {
            return Ok(false);
        }
        let runtime_id = session.runtime_id.clone();
        if session.active.is_none() {
            let cancel = RunCancelToken::child_of(&session.cancel);
            session.active = Some(ActiveRun {
                run_id: new_run.id.clone(),
                input_message_id: pending.input_message_id.clone(),
                runtime_id: runtime_id.clone(),
                text: input_message.content.clone(),
                thinking_depth: session_row.thinking_depth,
                thinking_override: false,
                references: Vec::new(),
                attachments: attachments.clone(),
                subagent_context: None,
                last_activity_ms: now,
                cancel,
                dispatched: has_slot,
                priority: pending.priority,
                timeout_ms: pending.timeout_ms,
                max_attempts: pending.max_attempts,
                retry_count,
                queued_at: now,
                dispatched_at_ms: None,
                timeout_requested_at_ms: None,
            });
            if let Some(active) = session.active.as_mut() {
                if active.dispatched {
                    active.dispatched_at_ms = Some(now);
                    (false, true)
                } else {
                    state.gate.queue.enqueue(GateWaiter::new(
                        pending.session_id.clone(),
                        new_run.id.clone(),
                        pending.priority,
                        runtime_weight(&runtime_id),
                        now,
                        WaitReason::ConcurrencyGate,
                    ));
                    (false, false)
                }
            } else {
                (false, false)
            }
        } else {
            session.waiting = Some(QueuedRun {
                run_id: new_run.id.clone(),
                input_message_id: pending.input_message_id.clone(),
                runtime_id,
                text: input_message.content.clone(),
                thinking_depth: session_row.thinking_depth,
                thinking_override: false,
                references: Vec::new(),
                attachments,
                subagent_context: None,
                priority: pending.priority,
                timeout_ms: pending.timeout_ms,
                max_attempts: pending.max_attempts,
                retry_count,
                queued_at: now,
            });
            (true, false)
        }
    };
    {
        let state = lock_state(inner).await;
        publish_gate_snapshot(inner, &state);
    }
    if gate_dispatched {
        spawn_run_inner(inner, pending.session_id.clone(), new_run.id.clone());
    }
    if let Err(failure) = insert_audit_inner(
        inner,
        &pending.session_id,
        "system",
        "run.retry",
        "retried",
        json!({
            "run_id": pending.run_id.as_str(),
            "new_run_id": new_run.id.as_str(),
            "auto": true,
            "attempt": pending.attempt,
            "max_attempts": pending.max_attempts,
            "backoff_ms": pending.backoff_ms,
            "queued": queued,
        }),
    )
    .await
    {
        tracing::warn!(error = %failure, "自动重试审计写入失败（仅记录）");
    }
    Ok(true)
}

/// 饥饿保护巡检（M10-07/ADR-037 D-037-1）：等待 ≥5min 的排队 run 每 run 审计一次。
async fn sweep_starvation_guard(inner: &Arc<ManagerInner>) -> usize {
    let now = inner.clock.now_ms();
    let candidates: Vec<(SessionId, RunId, i64)> = {
        let mut state = lock_state(inner).await;
        let ready: Vec<(SessionId, RunId, i64)> = state
            .gate
            .queue
            .entries()
            .iter()
            .filter(|waiter| waiter.needs_starvation_guard(now))
            .map(|waiter| {
                (
                    waiter.session_id.clone(),
                    waiter.run_id.clone(),
                    waiter.wait_ms(now),
                )
            })
            .collect();
        for (_, run_id, _) in &ready {
            state.gate.queue.mark_starvation_guarded(run_id);
        }
        ready
    };
    for (session_id, run_id, wait_ms) in &candidates {
        if let Err(failure) = insert_audit_inner(
            inner,
            session_id,
            "system",
            "scheduler.starvation_guard",
            "boosted",
            json!({
                "run_id": run_id.as_str(),
                "wait_ms": wait_ms,
                "boost": STARVATION_GUARD_PENALTY,
            }),
        )
        .await
        {
            tracing::warn!(error = %failure, "饥饿保护审计写入失败（仅记录）");
        }
    }
    candidates.len()
}

async fn finalize_completed(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    run_id: &RunId,
    assistant_text: Option<String>,
    usage: Option<TokenUsage>,
) -> Result<Option<RunStart>, LifecycleError> {
    let Some((detached, schedule)) = detach_and_prepare(inner, session_id, run_id).await? else {
        return Ok(None);
    };
    let runtime_id = detached.runtime_id.clone();
    if let Some(text) = assistant_text {
        let now = inner.clock.now_ms();
        let message = Message {
            id: MessageId::new(crate::ulid::generate()).map_err(internal_from)?,
            session_id: session_id.clone(),
            run_id: Some(run_id.clone()),
            client_msg_id: None,
            role: MessageRole::Assistant,
            content: text.clone(),
            content_parts: None,
            tool_calls: None,
            parent_message_id: None,
            seq: 0,
            created_at: now,
        };
        if let Err(error) = inner
            .write
            .execute(StoreCommand::InsertMessage {
                message: message.clone(),
            })
            .await
        {
            tracing::warn!(error = %error, "助手消息落库失败：跳过 message.completed 事件");
        } else {
            emit_inner(
                inner,
                session_id,
                Some(run_id),
                &runtime_id,
                EventPayload::MessageCompleted(aether_core::MessageCompletedPayload {
                    message: MessageSummary {
                        id: message.id,
                        session_id: session_id.clone(),
                        run_id: Some(run_id.clone()),
                        role: MessageRole::Assistant,
                        content: text,
                        created_at: now,
                    },
                    usage,
                }),
            )
            .await?;
        }
    }
    inner
        .write
        .execute(StoreCommand::FinishRun {
            run_id: run_id.clone(),
            status: aether_core::RunStatus::Succeeded,
            error: None,
            finished_at: inner.clock.now_ms(),
        })
        .await?;
    emit_inner(
        inner,
        session_id,
        Some(run_id),
        &runtime_id,
        EventPayload::RunCompleted(RunCompletedPayload {
            run_id: run_id.clone(),
            usage,
        }),
    )
    .await?;
    settle_session_idle(inner, session_id, &runtime_id).await?;
    apply_terminal_schedule(inner, session_id, schedule).await
}

async fn finalize_failed(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    run_id: &RunId,
    error: ErrorInfo,
) -> Result<Option<RunStart>, LifecycleError> {
    let Some((detached, schedule)) = detach_and_prepare(inner, session_id, run_id).await? else {
        return Ok(None);
    };
    let runtime_id = detached.runtime_id.clone();
    let error_code = error.code.clone();
    inner
        .write
        .execute(StoreCommand::FinishRun {
            run_id: run_id.clone(),
            status: aether_core::RunStatus::Failed,
            error: Some(error.code.clone()),
            finished_at: inner.clock.now_ms(),
        })
        .await?;
    emit_inner(
        inner,
        session_id,
        Some(run_id),
        &runtime_id,
        EventPayload::RunFailed(RunFailedPayload {
            run_id: run_id.clone(),
            error,
        }),
    )
    .await?;
    settle_session_idle(inner, session_id, &runtime_id).await?;
    let next = apply_terminal_schedule(inner, session_id, schedule).await?;
    schedule_auto_retry(inner, &detached, &error_code);
    Ok(next)
}

/// 超时收口（M10-07/ADR-037 D-037-2）：`runs.status='timeout'` + `run.failed`
/// （错误码 `run_timeout`，可重试）；自动重试分类同失败路径。
async fn finalize_timeout(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    run_id: &RunId,
    detached: DetachedRun,
    schedule: TerminalSchedule,
) -> Result<Option<RunStart>, LifecycleError> {
    let runtime_id = detached.runtime_id.clone();
    let error = ErrorInfo {
        code: crate::lifecycle::RUN_TIMEOUT_CODE.to_owned(),
        message: format!(
            "run 超时（{}ms；先中断，{}ms 未收口 → 标 timeout；可重试）",
            detached.timeout_ms, TIMEOUT_SETTLE_GRACE_MS
        ),
        recoverable: true,
    };
    inner
        .write
        .execute(StoreCommand::FinishRun {
            run_id: run_id.clone(),
            status: aether_core::RunStatus::Timeout,
            error: Some(error.code.clone()),
            finished_at: inner.clock.now_ms(),
        })
        .await?;
    emit_inner(
        inner,
        session_id,
        Some(run_id),
        &runtime_id,
        EventPayload::RunFailed(RunFailedPayload {
            run_id: run_id.clone(),
            error,
        }),
    )
    .await?;
    if let Err(failure) = insert_audit_inner(
        inner,
        session_id,
        "system",
        "run.timeout",
        "timeout",
        json!({
            "run_id": run_id.as_str(),
            "timeout_ms": detached.timeout_ms,
            "grace_ms": TIMEOUT_SETTLE_GRACE_MS,
        }),
    )
    .await
    {
        tracing::warn!(run_id = %run_id, error = %failure, "超时审计写入失败（仅记录）");
    }
    settle_session_idle(inner, session_id, &runtime_id).await?;
    let next = apply_terminal_schedule(inner, session_id, schedule).await?;
    schedule_auto_retry(inner, &detached, RUN_TIMEOUT_CODE);
    Ok(next)
}

async fn finalize_cancelled(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    run_id: &RunId,
    reason: Option<String>,
) -> Result<Option<RunStart>, LifecycleError> {
    let Some((detached, schedule)) = detach_and_prepare(inner, session_id, run_id).await? else {
        return Ok(None);
    };
    // 超时路径：先 interrupt（取消令牌），适配器在 5s 内收口为 cancelled → 仍按
    // `timeout` 终态收口（原因已由超时决定，不误记为普通取消）。
    if detached.timeout_requested_at_ms.is_some() {
        return finalize_timeout(inner, session_id, run_id, detached, schedule).await;
    }
    let runtime_id = detached.runtime_id.clone();
    inner
        .write
        .execute(StoreCommand::FinishRun {
            run_id: run_id.clone(),
            status: aether_core::RunStatus::Cancelled,
            error: None,
            finished_at: inner.clock.now_ms(),
        })
        .await?;
    emit_inner(
        inner,
        session_id,
        Some(run_id),
        &runtime_id,
        EventPayload::RunCancelled(RunCancelledPayload {
            run_id: run_id.clone(),
            reason,
        }),
    )
    .await?;
    settle_session_idle(inner, session_id, &runtime_id).await?;
    apply_terminal_schedule(inner, session_id, schedule).await
}

/// run 终态后会话回 `idle`（行 + 事件；等待提升的运行随后置 `running`）。
async fn settle_session_idle(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    runtime_id: &RuntimeId,
) -> Result<(), LifecycleError> {
    let mut state = lock_state(inner).await;
    settle_idle_locked(inner, &mut state, session_id, runtime_id).await
}

/// 会话回 `idle`（调用方持有状态锁；D9 等待审批经 `running` 收口——状态机白名单约束）。
async fn settle_idle_locked(
    inner: &Arc<ManagerInner>,
    state: &mut ManagerState,
    session_id: &SessionId,
    runtime_id: &RuntimeId,
) -> Result<(), LifecycleError> {
    let session =
        state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| LifecycleError::SessionNotFound {
                session_id: session_id.clone(),
            })?;
    let changes = idle_transitions(&mut session.fsm)?;
    for change in changes {
        persist_status_inner(inner, session_id, change, false, runtime_id).await?;
    }
    Ok(())
}

/// 当前状态回 `idle` 的合法转移序列（白名单：`waiting_permission → idle` 非法，
/// 必须先回 `running`；其余状态无操作）。
fn idle_transitions(
    fsm: &mut aether_core::SessionFsm,
) -> Result<Vec<aether_core::SessionStatusChange>, LifecycleError> {
    match fsm.status() {
        SessionStatus::Running => Ok(vec![fsm.transition(SessionStatus::Idle)?]),
        SessionStatus::WaitingPermission => Ok(vec![
            fsm.transition(SessionStatus::Running)?,
            fsm.transition(SessionStatus::Idle)?,
        ]),
        _ => Ok(Vec::new()),
    }
}

async fn watchdog_once_inner(inner: &Arc<ManagerInner>, timeout_ms: i64) -> Vec<RunId> {
    /// 单轮超时分类：断流超时 / 已请求中断 / 宽限强制超时。
    type TimeoutBatch = (
        Vec<(SessionId, RunId)>,
        Vec<(SessionId, RunId)>,
        Vec<(SessionId, RunId)>,
    );
    let now = inner.clock.now_ms();
    let (timed_out, timeout_requests, forced_timeouts): TimeoutBatch = {
        let mut state = lock_state(inner).await;
        let mut timed_out = Vec::new();
        let mut timeout_requests = Vec::new();
        let mut forced_timeouts = Vec::new();
        for (session_id, session) in state.sessions.iter_mut() {
            let Some(active) = session.active.as_mut() else {
                continue;
            };
            if !active.dispatched {
                continue;
            }
            // M10-07/ADR-037 D-037-2：run 超时（先中断；宽限 5s 未收口 → 标 timeout）。
            if let Some(requested_at) = active.timeout_requested_at_ms {
                if now.saturating_sub(requested_at) >= TIMEOUT_SETTLE_GRACE_MS {
                    forced_timeouts.push((session_id.clone(), active.run_id.clone()));
                }
                continue;
            }
            if let Some(dispatched_at) = active.dispatched_at_ms {
                if now.saturating_sub(dispatched_at) >= active.timeout_ms {
                    active.timeout_requested_at_ms = Some(now);
                    active.cancel.cancel();
                    timeout_requests.push((session_id.clone(), active.run_id.clone()));
                    continue;
                }
            }
            // M7-02/ADR-022：闸门排队 run（未派发）不适用断流计时（无执行即无事件）。
            if now.saturating_sub(active.last_activity_ms) >= timeout_ms {
                timed_out.push((session_id.clone(), active.run_id.clone()));
            }
        }
        (timed_out, timeout_requests, forced_timeouts)
    };
    for (session_id, run_id) in &timeout_requests {
        tracing::warn!(
            session_id = %session_id,
            run_id = %run_id,
            "run 超时：已发出中断，等待适配器收口（宽限后才标记 timeout）"
        );
    }
    let mut failed = Vec::new();
    for (session_id, run_id) in timed_out {
        let error = ErrorInfo {
            code: RUN_STREAM_TIMEOUT_CODE.to_owned(),
            message: format!(
                "run 断流超时：{timeout_ms}ms 内无任何事件（D8；可重试，重放按 ADR-005 恢复模式）"
            ),
            recoverable: true,
        };
        match finalize_failed(inner, &session_id, &run_id, error).await {
            Ok(next) => {
                // M2-05：run 被摘除（执行器可能仍卡住）→ 看门狗计时。
                inner
                    .task_watchdog
                    .mark_orphaned_by_run(&run_id, inner.clock.now_ms());
                // M7-02：原任务可能长期卡死，等待槽提升的 run 需新任务续跑（单写者顺序）。
                if let Some(next) = next {
                    spawn_run_inner(inner, session_id.clone(), next.run_id.clone());
                }
                failed.push(run_id);
            }
            Err(error) => {
                tracing::warn!(run_id = %run_id, error = %error, "断流超时收口失败（继续）");
            }
        }
    }
    for (session_id, run_id) in forced_timeouts {
        match force_timeout(inner, &session_id, &run_id).await {
            Ok(next) => {
                inner
                    .task_watchdog
                    .mark_orphaned_by_run(&run_id, inner.clock.now_ms());
                if let Some(next) = next {
                    spawn_run_inner(inner, session_id.clone(), next.run_id.clone());
                }
                failed.push(run_id);
            }
            Err(error) => {
                tracing::warn!(run_id = %run_id, error = %error, "超时收口失败（继续）");
            }
        }
    }
    failed
}

/// 强制超时收口（watchdog 宽限后；适配器未在 5s 内收口）。
async fn force_timeout(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    run_id: &RunId,
) -> Result<Option<RunStart>, LifecycleError> {
    let Some((detached, schedule)) = detach_and_prepare(inner, session_id, run_id).await? else {
        return Ok(None);
    };
    finalize_timeout(inner, session_id, run_id, detached, schedule).await
}

/// 管线降级中断（D4 降级期语义 3）：在途 run 转 `cancelled`（行 + 事件）；
/// 提升的 run 经 `apply_terminal_schedule` 按降级语义连带取消（准入失败 → suspended；
/// 降级期不做 FIFO 推进）。
async fn finalize_degraded_interrupt(
    inner: &Arc<ManagerInner>,
    interrupt: RunInterrupt,
) -> Result<(), LifecycleError> {
    // M2-05：在途 run 被降级中断摘除 → 看门狗计时。
    inner
        .task_watchdog
        .mark_orphaned_by_run(&interrupt.run_id, inner.clock.now_ms());
    let _ = finalize_cancelled(
        inner,
        &interrupt.session_id,
        &interrupt.run_id,
        Some(interrupt.reason),
    )
    .await?;
    Ok(())
}

async fn persist_status_inner(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    change: aether_core::SessionStatusChange,
    closed: bool,
    runtime_id: &RuntimeId,
) -> Result<(), LifecycleError> {
    let now = inner.clock.now_ms();
    inner
        .write
        .execute(StoreCommand::UpdateSessionStatus {
            session_id: session_id.clone(),
            status: change.to,
            updated_at: now,
            closed_at: if closed { Some(now) } else { None },
        })
        .await?;
    // M3-07（D9/SE-03 最小审计）：会话生命周期状态转移落审计。
    insert_audit_inner(
        inner,
        session_id,
        "system",
        "session.status_changed",
        &status_transition_result(change.from, change.to),
        json!({
            "from": change.from.as_str(),
            "to": change.to.as_str(),
        }),
    )
    .await?;
    emit_inner(
        inner,
        session_id,
        None,
        runtime_id,
        EventPayload::SessionStatusChanged(SessionStatusChangedPayload {
            session_id: session_id.clone(),
            from: change.from,
            to: change.to,
        }),
    )
    .await
}

async fn emit_inner(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    run_id: Option<&RunId>,
    runtime_id: &RuntimeId,
    payload: EventPayload,
) -> Result<(), LifecycleError> {
    let value = payload
        .to_value()
        .map_err(|error| LifecycleError::Internal {
            reason: format!("事件 payload 序列化失败: {error}"),
        })?;
    let event_type = payload.event_type();
    let mut object = serde_json::Map::with_capacity(ENVELOPE_FIELDS.len());
    object.insert("v".to_owned(), Value::from(EVENT_ENVELOPE_VERSION));
    object.insert("id".to_owned(), Value::from(ulid::generate()));
    object.insert("session_id".to_owned(), Value::from(session_id.as_str()));
    object.insert(
        "run_id".to_owned(),
        run_id.map_or(Value::Null, |run_id| Value::from(run_id.as_str())),
    );
    object.insert("runtime_id".to_owned(), Value::from(runtime_id.as_str()));
    object.insert("seq".to_owned(), Value::from(0_u64));
    object.insert("ts".to_owned(), Value::from(inner.clock.now_ms()));
    object.insert("type".to_owned(), Value::from(event_type.as_str()));
    object.insert("payload".to_owned(), value);
    match inner
        .pipeline
        .submit(Value::Object(object))
        .await
        .map_err(LifecycleError::from)?
    {
        SubmitOutcome::Persisted { .. } | SubmitOutcome::Buffered => Ok(()),
        SubmitOutcome::Duplicate { .. } => Err(LifecycleError::Internal {
            reason: "生命周期事件意外命中幂等去重".to_owned(),
        }),
        SubmitOutcome::DeadLettered { code, reason } => Err(LifecycleError::Internal {
            reason: format!("生命周期事件被死信（{code}）：{reason}"),
        }),
    }
}

fn internal_from(error: impl std::fmt::Display) -> LifecycleError {
    LifecycleError::Internal {
        reason: error.to_string(),
    }
}

/// 会话状态转移的审计 `result` 文案（`from→to`；与事件 payload 同源信息）。
fn status_transition_result(from: SessionStatus, to: SessionStatus) -> String {
    format!("{}→{}", from.as_str(), to.as_str())
}

/// 追加会话生命周期审计行（M3-07；D9/SE-03 P0 最小集，经单写队列）。
async fn insert_audit_inner(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    actor: &str,
    action: &str,
    result: &str,
    detail: Value,
) -> Result<(), LifecycleError> {
    let record = AuditLogRecord {
        id: ulid::generate(),
        session_id: Some(session_id.as_str().to_owned()),
        runtime_id: None,
        actor: actor.to_owned(),
        action: action.to_owned(),
        resource: Some(format!("session:{}", session_id.as_str())),
        detail: Some(detail.to_string()),
        result: Some(result.to_owned()),
        ts: inner.clock.now_ms(),
    };
    inner
        .write
        .execute(StoreCommand::InsertAudit { record })
        .await?;
    Ok(())
}

/// 从库装配会话内存态（M2-01；`SessionState` 不存在时按行状态建立 FSM/取消节点）。
async fn hydrate_inner(
    inner: &Arc<ManagerInner>,
    state: &mut ManagerState,
    session_id: &SessionId,
) -> Result<(), LifecycleError> {
    if state.sessions.contains_key(session_id) {
        return Ok(());
    }
    let session =
        inner
            .reads
            .session(session_id)
            .await?
            .ok_or_else(|| LifecycleError::SessionNotFound {
                session_id: session_id.clone(),
            })?;
    let cancel = inner
        .cancel_tree
        .session_token(session_id, session.parent_session_id.as_ref());
    state.sessions.insert(
        session_id.clone(),
        SessionState {
            fsm: aether_core::SessionFsm::new(session.status),
            runtime_id: session.runtime_id,
            active: None,
            waiting: None,
            cancel,
            // M7-03/ADR-017：暂停标记不跨重启（内存态）。
            pause_marker: None,
        },
    );
    Ok(())
}

/// 取状态锁并以 `OwnedMappedMutexGuard` 返回（`OwnedMutexGuard` 非 `Send`，
/// 不能跨 await 持有；mapped 形态为 `Send`，保证状态机+行+事件的整体串行）。
async fn lock_state(
    inner: &Arc<ManagerInner>,
) -> tokio::sync::OwnedMappedMutexGuard<ManagerState, ManagerState> {
    tokio::sync::OwnedMutexGuard::map(inner.state.clone().lock_owned().await, |state| state)
}

/// 背压控制器槽位加锁（中毒容忍；临界区不做 I/O）。
fn lock_backpressure(
    inner: &Arc<ManagerInner>,
) -> std::sync::MutexGuard<'_, Option<Arc<BackpressureController>>> {
    match inner.backpressure.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 自动重试待执行队列加锁（中毒容忍；临界区无 I/O）。
fn lock_pending_retries(inner: &Arc<ManagerInner>) -> std::sync::MutexGuard<'_, Vec<PendingRetry>> {
    match inner.pending_retries.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 会话执行任务集合加锁（M7-02：非异步互斥；临界区仅同步 spawn/收割，无 I/O）。
fn lock_run_tasks(inner: &Arc<ManagerInner>) -> std::sync::MutexGuard<'_, JoinSet<()>> {
    match inner.run_tasks.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

// ===== M7-02/ADR-022：并发闸门辅助（锁内单写者语义；`state` 由调用方持锁）=====

/// 全局 `running` run 数（已派发的当前 run；闸门计数唯一来源）。
fn running_count(state: &ManagerState) -> usize {
    state
        .sessions
        .values()
        .filter(|session| {
            session
                .active
                .as_ref()
                .map(|active| active.dispatched)
                .unwrap_or(false)
        })
        .count()
}

/// 闸门空槽判定（不超卖：`running < limit`）。
fn gate_has_slot(state: &ManagerState) -> bool {
    running_count(state) < state.gate.limit
}

/// 闸门快照（`limit/active/queued` + M10-07 优先级分桶/饥饿保护计数）。
fn gate_snapshot(state: &ManagerState, now_ms: i64) -> ConcurrencyGateSnapshot {
    let snapshot = state.gate.queue.snapshot(now_ms);
    let mut buckets: Vec<PriorityBucket> = Vec::new();
    for entry in &snapshot {
        if let Some(bucket) = buckets
            .iter_mut()
            .find(|bucket| bucket.priority == entry.priority)
        {
            bucket.count += 1;
        } else {
            buckets.push(PriorityBucket {
                priority: entry.priority,
                count: 1,
            });
        }
    }
    buckets.sort_by_key(|bucket| std::cmp::Reverse(bucket.priority));
    let starvation_guard = state
        .gate
        .queue
        .entries()
        .iter()
        .filter(|waiter| waiter.wait_ms(now_ms) >= STARVATION_GUARD_MS)
        .count();
    ConcurrencyGateSnapshot {
        limit: state.gate.limit,
        active: running_count(state),
        queued: state.gate.queue.len(),
        queued_by_priority: Some(buckets),
        starvation_guard: Some(starvation_guard),
    }
}

/// 发布闸门快照到共享单元（`health.concurrency_gate` 同步读）。
fn publish_gate_snapshot(inner: &Arc<ManagerInner>, state: &ManagerState) {
    let now = inner.clock.now_ms();
    let guard = lock_gate_cell(inner);
    if let Some(cell) = guard.as_ref() {
        cell.set(gate_snapshot(state, now));
    }
}

/// 固定运行时权重（ADR-022 附录 A-4；队列分数用）。
fn runtime_weight(runtime_id: &RuntimeId) -> i32 {
    i32::try_from(route_priority(runtime_id.as_str())).unwrap_or(100)
}

/// 全局队列推进：有槽则按分数逐个启动队首（每次推进仅启动队首 run，不超卖）。
fn gate_advance_locked(state: &mut ManagerState, now_ms: i64) -> Vec<(SessionId, RunId)> {
    let mut dispatches = Vec::new();
    loop {
        if !gate_has_slot(state) {
            break;
        }
        let Some(waiter) = state.gate.queue.pop_best(now_ms) else {
            break;
        };
        let Some(session) = state.sessions.get_mut(&waiter.session_id) else {
            continue;
        };
        let Some(active) = session.active.as_mut() else {
            continue;
        };
        if active.run_id != waiter.run_id || active.dispatched {
            continue;
        }
        active.dispatched = true;
        active.dispatched_at_ms = Some(now_ms);
        dispatches.push((waiter.session_id, waiter.run_id));
    }
    dispatches
}

/// 闸门释放与推进（run 终态语义）：仅新 run 准入可行时推进（降级期不启动排队 run）。
fn gate_release_and_advance(
    inner: &Arc<ManagerInner>,
    state: &mut ManagerState,
) -> Vec<(SessionId, RunId)> {
    if inner.pipeline.admission().is_err() {
        return Vec::new();
    }
    let now = inner.clock.now_ms();
    gate_advance_locked(state, now)
}

/// 等待槽提升（interrupt 的闸门排队路径用）：有空槽且准入可行 → 派发（返回
/// `(session, run)` 供调用方 spawn）；饱和 → 入优先级队列尾部；降级 → 不提升（保留等待槽）。
fn promote_waiting_locked(
    inner: &Arc<ManagerInner>,
    state: &mut ManagerState,
    session_id: &SessionId,
) -> Option<(SessionId, RunId)> {
    let has_waiting = state
        .sessions
        .get(session_id)
        .map(|session| session.waiting.is_some())
        .unwrap_or(false);
    if !has_waiting {
        return None;
    }
    if inner.pipeline.admission().is_err() {
        return None;
    }
    let now = inner.clock.now_ms();
    let has_slot = gate_has_slot(state);
    let session = state.sessions.get_mut(session_id)?;
    let queued = session.waiting.take()?;
    let run_id = queued.run_id.clone();
    let runtime_id = queued.runtime_id.clone();
    let priority = queued.priority;
    let timeout_ms = queued.timeout_ms;
    let max_attempts = queued.max_attempts;
    let retry_count = queued.retry_count;
    let queued_at = queued.queued_at;
    let cancel = RunCancelToken::child_of(&session.cancel);
    session.active = Some(ActiveRun {
        run_id: queued.run_id,
        input_message_id: queued.input_message_id,
        runtime_id: queued.runtime_id,
        text: queued.text,
        thinking_depth: queued.thinking_depth,
        thinking_override: queued.thinking_override,
        references: queued.references,
        attachments: queued.attachments,
        subagent_context: queued.subagent_context,
        last_activity_ms: now,
        cancel,
        dispatched: has_slot,
        priority,
        timeout_ms,
        max_attempts,
        retry_count,
        queued_at,
        dispatched_at_ms: has_slot.then_some(now),
        timeout_requested_at_ms: None,
    });
    if has_slot {
        Some((session_id.clone(), run_id))
    } else {
        state.gate.queue.enqueue(GateWaiter::new(
            session_id.clone(),
            run_id,
            priority,
            runtime_weight(&runtime_id),
            queued_at,
            WaitReason::ConcurrencyGate,
        ));
        None
    }
}

/// 闸门巡检主体（`refresh_gate_limit` 与采样任务共用）：采样 → 重算上限（只对新 run
/// 生效）→ 空槽按 FIFO 推进 → **总是发布快照**（采样失败也发布，保持既有上限；
/// `health.concurrency_gate` 在首次活动前即可读）。返回新上限；无采样源/采样失败 → `None`。
async fn refresh_gate_limit_inner(inner: &Arc<ManagerInner>) -> Option<usize> {
    let sampled = {
        let guard = lock_free_ram(inner);
        guard
            .as_ref()
            .and_then(|source| source.free_ram_gb())
            .map(max_concurrent_for_free_ram)
    };
    let mut state = lock_state(inner).await;
    if let Some(limit) = sampled {
        state.gate.limit = limit;
    }
    let dispatches = gate_release_and_advance(inner, &mut state);
    publish_gate_snapshot(inner, &state);
    drop(state);
    for (spawn_session, spawn_run) in dispatches {
        spawn_run_inner(inner, spawn_session, spawn_run);
    }
    sampled
}

/// 空闲内存采样源槽位加锁（中毒容忍）。
fn lock_free_ram(
    inner: &Arc<ManagerInner>,
) -> std::sync::MutexGuard<'_, Option<Arc<dyn FreeRamSource>>> {
    match inner.free_ram.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_attachment_resolver(
    inner: &Arc<ManagerInner>,
) -> std::sync::MutexGuard<'_, Option<Arc<dyn crate::attachments::AttachmentResolver>>> {
    match inner.attachment_resolver.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 闸门快照单元槽位加锁（中毒容忍）。
fn lock_gate_cell(
    inner: &Arc<ManagerInner>,
) -> std::sync::MutexGuard<'_, Option<ConcurrencyGateCell>> {
    match inner.gate_cell.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// `session.pause` 下发钩子槽位加锁（中毒容忍；M7-03/ADR-017）。
fn lock_pause_notifier(
    inner: &Arc<ManagerInner>,
) -> std::sync::MutexGuard<'_, Option<Arc<dyn PauseNotifier>>> {
    match inner.pause_notifier.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_d8() {
        let config = LifecycleConfig::default();
        assert_eq!(config.run_stream_timeout_ms, 120_000, "D8：120s 断流");
        assert_eq!(config.max_waiting_runs, 1, "D8：等待队列 1");
        assert_eq!(config.watchdog_tick, Duration::from_secs(1));
        assert_eq!(
            config.task_force_cleanup_ms, 10_000,
            "D8：取消后 10s 未退出 → 强制清理 + dump"
        );
        assert_eq!(config.task_dump_capacity, 64);
    }

    #[test]
    fn lifecycle_error_codes_are_stable() {
        let cases: Vec<(LifecycleError, &str)> = vec![
            (
                LifecycleError::SessionNotFound {
                    session_id: SessionId::new("s").unwrap(),
                },
                "session_not_found",
            ),
            (
                LifecycleError::SessionBusy {
                    session_id: SessionId::new("s").unwrap(),
                },
                "session_busy",
            ),
            (
                LifecycleError::SessionClosed {
                    session_id: SessionId::new("s").unwrap(),
                    status: SessionStatus::Completed,
                },
                "session_closed",
            ),
            (
                LifecycleError::PersistDegraded {
                    reason: "x".to_owned(),
                },
                "persist_degraded",
            ),
            (
                LifecycleError::StorageBackpressure {
                    depth: 1,
                    threshold: 0,
                },
                "storage_backpressure",
            ),
            (
                LifecycleError::InvalidTransition {
                    from: SessionStatus::Idle,
                    to: SessionStatus::WaitingPermission,
                },
                "invalid_session_transition",
            ),
            (
                LifecycleError::SessionPaused {
                    session_id: SessionId::new("s").unwrap(),
                },
                "session_paused",
            ),
            (
                LifecycleError::PauseState {
                    session_id: SessionId::new("s").unwrap(),
                    status: SessionStatus::Paused,
                },
                "invalid_value",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code);
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn timeout_error_is_retryable() {
        assert_eq!(RUN_STREAM_TIMEOUT_CODE, "run_stream_timeout");
        assert!(run_is_retryable(aether_core::RunStatus::Failed));
        assert!(run_is_retryable(aether_core::RunStatus::Cancelled));
        assert!(run_is_retryable(aether_core::RunStatus::Timeout));
        assert!(!run_is_retryable(aether_core::RunStatus::Succeeded));
        assert!(!run_is_retryable(aether_core::RunStatus::Running));
        assert!(!run_is_retryable(aether_core::RunStatus::Queued));
    }

    #[test]
    fn cancel_token_flips_once_and_stays() {
        // 取消令牌本体语义（含等价性/默认构造）在 `crate::cancel` 单测覆盖；
        // 此处保留生命周期侧的最小冒烟（令牌来自取消树子节点）。
        let session = RunCancelToken::new();
        let run = RunCancelToken::child_of(&session);
        assert!(!run.is_cancelled());
        session.cancel();
        assert!(run.is_cancelled(), "会话取消级联到 run 子节点");
    }

    #[test]
    fn error_conversions_and_display_cover_all_families() {
        let cases: Vec<(LifecycleError, &str)> = vec![
            (
                LifecycleError::SessionNotFound {
                    session_id: SessionId::new("s").unwrap(),
                },
                "session_not_found",
            ),
            (
                LifecycleError::SessionBusy {
                    session_id: SessionId::new("s").unwrap(),
                },
                "session_busy",
            ),
            (
                LifecycleError::SessionClosed {
                    session_id: SessionId::new("s").unwrap(),
                    status: SessionStatus::Completed,
                },
                "session_closed",
            ),
            (
                LifecycleError::InvalidTransition {
                    from: SessionStatus::Idle,
                    to: SessionStatus::WaitingPermission,
                },
                "invalid_session_transition",
            ),
            (
                LifecycleError::SessionPaused {
                    session_id: SessionId::new("s").unwrap(),
                },
                "session_paused",
            ),
            (
                LifecycleError::PauseState {
                    session_id: SessionId::new("s").unwrap(),
                    status: SessionStatus::Cancelled,
                },
                "invalid_value",
            ),
            (
                LifecycleError::PersistDegraded {
                    reason: "写失败".to_owned(),
                },
                "persist_degraded",
            ),
            (
                LifecycleError::StorageBackpressure {
                    depth: 4_097,
                    threshold: 4_096,
                },
                "storage_backpressure",
            ),
            (
                LifecycleError::Storage {
                    code: "sqlite".to_owned(),
                    message: "boom".to_owned(),
                },
                "storage_error",
            ),
            (
                LifecycleError::Pipeline {
                    code: "readback_gap_too_large".to_owned(),
                    message: "gap".to_owned(),
                },
                "pipeline_error",
            ),
            (
                LifecycleError::Internal {
                    reason: "invariant".to_owned(),
                },
                "internal",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code);
            assert!(!error.to_string().is_empty());
        }

        // PipelineError → LifecycleError（降级/背压/其它）。
        let degraded = LifecycleError::from(PipelineError::PersistDegraded {
            reason: "disk".to_owned(),
        });
        assert_eq!(degraded.code(), "persist_degraded");
        let backpressure = LifecycleError::from(PipelineError::StorageBackpressure {
            depth: 10,
            threshold: 9,
        });
        assert_eq!(backpressure.code(), "storage_backpressure");
        let other = LifecycleError::from(PipelineError::PipelineClosed);
        assert_eq!(other.code(), "pipeline_error");
        assert!(matches!(other, LifecycleError::Pipeline { .. }));

        // StoreError → LifecycleError（背压与其它）。
        let store_backpressure = LifecycleError::from(StoreError::StorageBackpressure {
            depth: 8,
            threshold: 4,
        });
        assert_eq!(store_backpressure.code(), "storage_backpressure");
        let store_other = LifecycleError::from(StoreError::WriteQueueClosed);
        assert_eq!(store_other.code(), "storage_error");

        // 状态机非法转移 → invalid_session_transition。
        let mut fsm = aether_core::SessionFsm::new(SessionStatus::Idle);
        let transition_error = fsm
            .transition(SessionStatus::WaitingPermission)
            .unwrap_err();
        let mapped = LifecycleError::from(transition_error);
        assert_eq!(mapped.code(), "invalid_session_transition");
    }

    #[test]
    fn lifecycle_config_accessor_matches_default() {
        let config = LifecycleConfig {
            run_stream_timeout_ms: 5,
            watchdog_tick: Duration::from_millis(5),
            max_waiting_runs: 2,
            task_force_cleanup_ms: 50,
            task_dump_capacity: 4,
        };
        assert_eq!(config.clone(), config);
        assert_eq!(config.run_stream_timeout_ms, 5);
        assert_eq!(config.task_force_cleanup_ms, 50);
        assert_eq!(LifecycleConfig::default().max_waiting_runs, 1);
    }
}
