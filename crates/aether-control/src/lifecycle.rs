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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_core::{
    session_status_is_terminal, ErrorInfo, EventPayload, Message, MessageId, MessageRole,
    MessageSummary, Run, RunCancelledPayload, RunCompletedPayload, RunFailedPayload, RunId,
    RunStartedPayload, Runtime, RuntimeId, Session, SessionClosedPayload, SessionCreatedPayload,
    SessionId, SessionStatus, SessionStatusChangedPayload, SessionSummary, TokenUsage, WorkspaceId,
    ENVELOPE_FIELDS, EVENT_ENVELOPE_VERSION,
};
use aether_store::{ReadPool, SessionQuery, StoreCommand, StoreError, StoreOutcome, WriteQueue};
use serde_json::{json, Value};
use tokio::runtime::Handle;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::{AbortHandle, JoinError, JoinHandle, JoinSet};

use crate::backpressure::{BackpressureController, BackpressureError};
use crate::cancel::{CancelTree, RunCancelToken, TaskDump, TaskWatchdog};
use crate::clock::SharedClock;
use crate::error::PipelineError;
use crate::pipeline::{EventPipeline, RunInterrupt, SubmitOutcome};
use crate::ulid;

/// 断流超时（D8/M2-01 DoD5：120s 无事件 → run failed 且可重试）。
pub const RUN_STREAM_TIMEOUT_MS: i64 = 120_000;
/// 断流超时错误码（`run.failed.error.code`）。
pub const RUN_STREAM_TIMEOUT_CODE: &str = "run_stream_timeout";
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

/// run 执行请求（M2-02 真实适配器据此调用 `session.create/send`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub runtime_id: RuntimeId,
    pub input_message_id: MessageId,
    pub text: String,
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

/// ack 快路径结果（消息与 run 行已提交；模型执行异步进行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendAck {
    pub session_id: SessionId,
    pub message_id: MessageId,
    pub run_id: RunId,
    /// 是否进入等待队列（当前有 run 在执行）。
    pub queued: bool,
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
            Self::PersistDegraded { .. } => "persist_degraded",
            Self::StorageBackpressure { .. } => "storage_backpressure",
            Self::AdapterIsolated { .. } => "storage_backpressure",
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
    last_activity_ms: i64,
    cancel: RunCancelToken,
}

struct QueuedRun {
    run_id: RunId,
    input_message_id: MessageId,
    runtime_id: RuntimeId,
    text: String,
}

struct SessionState {
    fsm: aether_core::SessionFsm,
    runtime_id: RuntimeId,
    active: Option<ActiveRun>,
    waiting: Option<QueuedRun>,
    /// 会话取消节点（取消树；run 令牌为其子节点，父/子会话级联）。
    cancel: RunCancelToken,
}

#[derive(Default)]
struct ManagerState {
    sessions: HashMap<SessionId, SessionState>,
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
    run_tasks: AsyncMutex<JoinSet<()>>,
    /// run 任务 id →（会话, run）映射（panic 时经 JoinError 定位并恢复）。
    run_task_index: Mutex<HashMap<tokio::task::Id, (SessionId, RunId)>>,
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
                run_tasks: AsyncMutex::new(JoinSet::new()),
                run_task_index: Mutex::new(HashMap::new()),
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
    pub async fn create_session(
        &self,
        runtime: Runtime,
        title: &str,
        workspace_id: Option<WorkspaceId>,
        model: Option<String>,
    ) -> Result<Session, LifecycleError> {
        // M2-04：L3 熔断/存储侧隔离期拒绝新会话。
        self.check_backpressure(&runtime.id)?;
        let now = self.inner.clock.now_ms();
        let session_id = SessionId::new(ulid::generate()).map_err(internal_from)?;
        let runtime_id = runtime.id.clone();
        let session = Session {
            id: session_id.clone(),
            runtime_id: runtime_id.clone(),
            workspace_id,
            parent_session_id: None,
            title: title.to_owned(),
            status: SessionStatus::Creating,
            model,
            system_prompt: None,
            config: json!({}),
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
                    created_at: now,
                    updated_at: now,
                },
            }),
        )
        .await?;
        let mut fsm = aether_core::SessionFsm::new(SessionStatus::Creating);
        let change = fsm.transition(SessionStatus::Idle)?;
        self.inner
            .write
            .execute(StoreCommand::UpdateSessionStatus {
                session_id: session_id.clone(),
                status: SessionStatus::Idle,
                updated_at: now,
                closed_at: None,
            })
            .await?;
        self.emit(
            &session_id,
            None,
            &runtime_id,
            EventPayload::SessionStatusChanged(SessionStatusChangedPayload {
                session_id: session_id.clone(),
                from: change.from,
                to: change.to,
            }),
        )
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
            if session.active.is_some() && session.waiting.is_some() {
                return Err(LifecycleError::SessionBusy {
                    session_id: session_id.clone(),
                });
            }
            // M2-04：L3 投递熔断/存储侧隔离 → 拒绝新 run（已有 run 不受影响）。
            self.check_backpressure(&session.runtime_id)?;
        }

        let now = self.inner.clock.now_ms();
        let message = Message {
            id: MessageId::new(ulid::generate()).map_err(internal_from)?,
            session_id: session_id.clone(),
            run_id: None,
            client_msg_id: Some(client_msg_id.to_owned()),
            role: MessageRole::User,
            content: text.to_owned(),
            content_parts: None,
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
                duplicate: true,
            });
        }

        let queued = {
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
                    runtime_id,
                    text: text.to_owned(),
                    last_activity_ms: now,
                    cancel,
                });
                false
            } else {
                session.waiting = Some(QueuedRun {
                    run_id: run_id.clone(),
                    input_message_id: message_id.clone(),
                    runtime_id,
                    text: text.to_owned(),
                });
                true
            }
        };
        drop(state);

        if !queued {
            self.spawn_run(session_id.clone(), run_id.clone()).await;
        }
        Ok(SendAck {
            session_id: session_id.clone(),
            message_id,
            run_id,
            queued,
            duplicate: false,
        })
    }

    /// 中断当前 run（最小语义：置取消令牌 + run 终态 `cancelled`；M2-05 扩展取消树）。
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
            let active = session.active.take();
            let waiting = session.waiting.take();
            if let Some(active) = &active {
                active.cancel.cancel();
            }
            (active, waiting, session.runtime_id.clone())
        };

        let interrupted_run = active.as_ref().map(|active| active.run_id.clone());
        let cancelled_waiting_run = waiting.as_ref().map(|waiting| waiting.run_id.clone());

        if let Some(active) = active {
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
        }
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
            let _ = runtime_id;
        }
        drop(state);
        Ok(InterruptReport {
            session_id: session_id.clone(),
            interrupted_run,
            cancelled_waiting_run,
        })
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
        let target = if report.interrupted_run.is_some() || report.cancelled_waiting_run.is_some() {
            SessionStatus::Cancelled
        } else {
            SessionStatus::Completed
        };
        let now = self.inner.clock.now_ms();
        let mut state = lock_state(&self.inner).await;
        self.hydrate_locked(&mut state, session_id).await?;
        let (change, runtime_id) = {
            let session = state.sessions.get_mut(session_id).ok_or_else(|| {
                LifecycleError::SessionNotFound {
                    session_id: session_id.clone(),
                }
            })?;
            (session.fsm.transition(target)?, session.runtime_id.clone())
        };
        self.persist_status_and_emit(session_id, change, true, &runtime_id)
            .await?;
        drop(state);
        self.emit(
            session_id,
            None,
            &runtime_id,
            EventPayload::SessionClosed(SessionClosedPayload {
                session_id: session_id.clone(),
                closed_at: now,
            }),
        )
        .await?;
        Ok(target)
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

    async fn spawn_run(&self, session_id: SessionId, run_id: RunId) {
        spawn_run_inner(&self.inner, session_id, run_id).await;
    }

    async fn hydrate_locked(
        &self,
        state: &mut ManagerState,
        session_id: &SessionId,
    ) -> Result<(), LifecycleError> {
        if state.sessions.contains_key(session_id) {
            return Ok(());
        }
        let session = self.inner.reads.session(session_id).await?.ok_or_else(|| {
            LifecycleError::SessionNotFound {
                session_id: session_id.clone(),
            }
        })?;
        let cancel = self
            .inner
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
            },
        );
        Ok(())
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
async fn spawn_run_inner(inner: &Arc<ManagerInner>, session_id: SessionId, run_id: RunId) {
    let task_inner = Arc::clone(inner);
    let task_session = session_id.clone();
    let task_run = run_id.clone();
    let task_name = format!("session:{} run:{}", session_id.as_str(), run_id.as_str());
    let started_at_ms = inner.clock.now_ms();
    let abort: AbortHandle = {
        let mut set = inner.run_tasks.lock().await;
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
            let mut set = inner.run_tasks.lock().await;
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
            spawn_run_inner(inner, session_id.clone(), next.run_id.clone()).await;
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
    if &active.run_id != run_id {
        return Ok(None);
    }
    Ok(Some(RunStart {
        run_id: active.run_id.clone(),
        input_message_id: active.input_message_id.clone(),
        runtime_id: active.runtime_id.clone(),
        text: active.text.clone(),
        cancel: active.cancel.clone(),
    }))
}

struct RunStart {
    run_id: RunId,
    input_message_id: MessageId,
    runtime_id: RuntimeId,
    text: String,
    cancel: RunCancelToken,
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
async fn detach_and_prepare(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    run_id: &RunId,
) -> Result<Option<(RuntimeId, Option<RunStart>)>, LifecycleError> {
    let mut state = lock_state(inner).await;
    let session =
        state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| LifecycleError::SessionNotFound {
                session_id: session_id.clone(),
            })?;
    match session.active.as_ref() {
        Some(active) if &active.run_id == run_id => {
            let active = session
                .active
                .take()
                .ok_or_else(|| LifecycleError::Internal {
                    reason: "active 位竞态".to_owned(),
                })?;
            let next = session.waiting.take().map(|queued| RunStart {
                run_id: queued.run_id,
                input_message_id: queued.input_message_id,
                runtime_id: queued.runtime_id,
                text: queued.text,
                // M2-05：提升的 run 亦为会话取消树子节点。
                cancel: RunCancelToken::child_of(&session.cancel),
            });
            if let Some(next_run) = &next {
                session.active = Some(ActiveRun {
                    run_id: next_run.run_id.clone(),
                    input_message_id: next_run.input_message_id.clone(),
                    runtime_id: next_run.runtime_id.clone(),
                    text: next_run.text.clone(),
                    last_activity_ms: inner.clock.now_ms(),
                    cancel: next_run.cancel.clone(),
                });
            }
            Ok(Some((active.runtime_id, next)))
        }
        _ => Ok(None),
    }
}

async fn finalize_completed(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    run_id: &RunId,
    assistant_text: Option<String>,
    usage: Option<TokenUsage>,
) -> Result<Option<RunStart>, LifecycleError> {
    let Some((runtime_id, next)) = detach_and_prepare(inner, session_id, run_id).await? else {
        return Ok(None);
    };
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
    Ok(next)
}

async fn finalize_failed(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    run_id: &RunId,
    error: ErrorInfo,
) -> Result<Option<RunStart>, LifecycleError> {
    let Some((runtime_id, next)) = detach_and_prepare(inner, session_id, run_id).await? else {
        return Ok(None);
    };
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
    Ok(next)
}

async fn finalize_cancelled(
    inner: &Arc<ManagerInner>,
    session_id: &SessionId,
    run_id: &RunId,
    reason: Option<String>,
) -> Result<Option<RunStart>, LifecycleError> {
    let Some((runtime_id, next)) = detach_and_prepare(inner, session_id, run_id).await? else {
        return Ok(None);
    };
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
    Ok(next)
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
    let now = inner.clock.now_ms();
    let timed_out: Vec<(SessionId, RunId)> = {
        let state = lock_state(inner).await;
        let mut timed_out = Vec::new();
        for (session_id, session) in &state.sessions {
            if let Some(active) = &session.active {
                if now.saturating_sub(active.last_activity_ms) >= timeout_ms {
                    timed_out.push((session_id.clone(), active.run_id.clone()));
                }
            }
        }
        timed_out
    };
    let mut failed = Vec::new();
    for (session_id, run_id) in timed_out {
        let error = ErrorInfo {
            code: RUN_STREAM_TIMEOUT_CODE.to_owned(),
            message: format!(
                "run 断流超时：{timeout_ms}ms 内无任何事件（D8；可重试，重放按 ADR-005 恢复模式）"
            ),
            recoverable: true,
        };
        if finalize_failed(inner, &session_id, &run_id, error)
            .await
            .is_ok()
        {
            // M2-05：run 被摘除（执行器可能仍卡住）→ 看门狗计时。
            inner
                .task_watchdog
                .mark_orphaned_by_run(&run_id, inner.clock.now_ms());
            failed.push(run_id);
        }
    }
    failed
}

/// 管线降级中断（D4 降级期语义 3）：在途 run 转 `cancelled`（行 + 事件）；
/// 若终态处理提升了等待队列，则连带取消提升的 run（降级期拒绝新 run）。
async fn finalize_degraded_interrupt(
    inner: &Arc<ManagerInner>,
    interrupt: RunInterrupt,
) -> Result<(), LifecycleError> {
    // M2-05：在途 run 被降级中断摘除 → 看门狗计时。
    inner
        .task_watchdog
        .mark_orphaned_by_run(&interrupt.run_id, inner.clock.now_ms());
    let promoted = finalize_cancelled(
        inner,
        &interrupt.session_id,
        &interrupt.run_id,
        Some(interrupt.reason),
    )
    .await?;
    if let Some(next) = promoted {
        finalize_cancelled(
            inner,
            &interrupt.session_id,
            &next.run_id,
            Some("persist_degraded".to_owned()),
        )
        .await?;
    }
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
