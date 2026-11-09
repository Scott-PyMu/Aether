//! M10-01/ADR-031：工作流执行引擎（DAG 调度、并行、条件、汇总、重试/超时/取消）。
//!
//! 设计要点：
//! - 所有状态推进**先落库后广播**（D4）；事件 `session_id` = 运行锚点会话；
//! - 节点输入显式构造（workflow_input / upstream_results / allowlisted_context /
//!   node_config），不注入锚点会话消息全文；
//! - 全局并发闸门（默认 10）+ 运行级 `max_parallel` 双层约束；
//! - 取消树级联：运行取消 → 在途节点任务 abort → 节点 `cancelled`；≤5s 收口；
//! - daemon 重启 reconcile 只标 `interrupted`，不自动重放。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aether_control::{Clock, EventPipeline};
use aether_store::{
    AuditLogRecord, NodeRunRecord, ReadPool, StoreCommand, StoreError, StoreOutcome, TaskRecord,
    WorkflowCommand, WorkflowRunRecord, WriteQueue,
};
use serde_json::{json, Value};
use tokio::sync::{watch, Semaphore};

use super::definition::{
    clamp_result, definition_hash, eval_expression, parse_and_validate, parse_expression, truthy,
    ContextRef, DefinitionError, EvalContext, NodeType, WorkflowDefinition, WorkflowNode,
};

/// 全局工作流并发上限（ADR-031 D-031-7：10 并发工作流）。
pub const GLOBAL_WORKFLOW_LIMIT: usize = 10;
/// 取消确认上限（ADR-031 D-031-4：≤5s 收口）。
pub const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

/// agent 节点执行请求（上下文隔离：只含显式输入）。
#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub workflow_run_id: String,
    pub workflow_id: String,
    pub session_id: String,
    pub node_id: String,
    pub attempt: u32,
    pub prompt: String,
    pub context_refs: Vec<ContextRef>,
    pub runtime_id: Option<String>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub timeout_ms: i64,
    pub input: Value,
    pub cancel: Arc<AtomicBool>,
}

/// agent 节点执行结果。
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub output: Value,
    pub session_id: Option<String>,
}

/// agent 节点执行错误。
#[derive(Debug, Clone)]
pub struct AgentError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl AgentError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }
}

impl From<crate::ipc::IpcError> for AgentError {
    fn from(error: crate::ipc::IpcError) -> Self {
        let retryable = matches!(
            error.code,
            crate::ipc::IpcErrorCode::CoreNotReady
                | crate::ipc::IpcErrorCode::Internal
                | crate::ipc::IpcErrorCode::ProviderUnreachable
        );
        Self {
            code: error.code.as_str().to_owned(),
            message: error.message,
            retryable,
        }
    }
}

/// agent 节点执行器（生产 = 会话后端；单测 = 注入替身）。
pub trait AgentExecutor: Send + Sync + 'static {
    fn execute(
        &self,
        request: AgentRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AgentOutcome, AgentError>> + Send + '_>,
    >;
}

/// 未接线执行器：agent 节点按可重试 `adapter_unavailable` 失败。
pub struct UnavailableAgentExecutor;

impl AgentExecutor for UnavailableAgentExecutor {
    fn execute(
        &self,
        _request: AgentRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AgentOutcome, AgentError>> + Send + '_>,
    > {
        Box::pin(async {
            Err(AgentError::new(
                "adapter_unavailable",
                "agent 节点执行器未接线（等待 M10-02/适配器）",
                true,
            ))
        })
    }
}

/// 运行元数据（事件锚点与状态行）。
#[derive(Debug, Clone)]
struct RunMeta {
    run_id: String,
    workflow_id: String,
    session_id: String,
    runtime_id: String,
    priority: i64,
}

/// 运行请求（`workflow_run` 命令创建）。
#[derive(Debug, Clone)]
pub struct RunRequest {
    pub run_id: String,
    pub workflow_id: String,
    pub session_id: String,
    pub runtime_id: String,
    pub priority: i64,
    pub input: Value,
    pub definition: WorkflowDefinition,
    pub definition_value: Value,
}

/// 节点运行态。
#[derive(Debug, Clone)]
struct NodeState {
    status: String,
    attempt: u32,
    node_run_id: String,
    task_id: String,
    output: Option<Value>,
    error: Option<String>,
    retryable: bool,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    retry_at: Option<i64>,
    duration_ms: u64,
}

impl NodeState {
    fn terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            "succeeded" | "failed" | "timeout" | "cancelled" | "skipped"
        )
    }

    fn success(&self) -> bool {
        self.status == "succeeded"
    }
}

/// 节点执行成功。
struct NodeSuccess {
    output: Value,
    session_id: Option<String>,
}

/// 节点执行失败。
struct NodeFailure {
    code: String,
    message: String,
    retryable: bool,
}

struct NodeOutcome {
    node_id: String,
    attempt: u32,
    duration_ms: u64,
    result: Result<NodeSuccess, NodeFailure>,
}

/// 运行句柄（取消/完成通知/指标）。
struct ActiveRun {
    cancel: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
    done: watch::Sender<bool>,
    status: Arc<std::sync::Mutex<String>>,
}

/// 工作流引擎（`Arc` 共享给后台运行任务）。
pub struct WorkflowEngine {
    reads: ReadPool,
    write: WriteQueue,
    pipeline: Option<EventPipeline>,
    clock: Arc<dyn Clock>,
    agent: Arc<dyn AgentExecutor>,
    gate: Arc<Semaphore>,
    max_active_nodes: Arc<AtomicUsize>,
    active: Arc<std::sync::Mutex<HashMap<String, ActiveRun>>>,
}

impl WorkflowEngine {
    pub fn new(
        reads: ReadPool,
        write: WriteQueue,
        pipeline: Option<EventPipeline>,
        clock: Arc<dyn Clock>,
        agent: Arc<dyn AgentExecutor>,
    ) -> Self {
        Self {
            reads,
            write,
            pipeline,
            clock,
            agent,
            gate: Arc::new(Semaphore::new(GLOBAL_WORKFLOW_LIMIT)),
            max_active_nodes: Arc::new(AtomicUsize::new(0)),
            active: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// 健康/队列指标（DoD-5：可查）。
    pub fn metrics(&self) -> Value {
        let active = match self.active.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut running = 0u64;
        let mut queued = 0u64;
        for handle in active.values() {
            match handle.status.lock() {
                Ok(status) if status.as_str() == "running" => running += 1,
                Ok(status) if status.as_str() == "queued" => queued += 1,
                Ok(_) => {}
                Err(poisoned) => {
                    if poisoned.into_inner().as_str() == "running" {
                        running += 1;
                    }
                }
            }
        }
        json!({
            "global_limit": GLOBAL_WORKFLOW_LIMIT,
            "running_runs": running,
            "queued_runs": queued,
            "active_node_tasks": self.max_active_nodes.load(Ordering::Relaxed),
        })
    }

    /// 启动运行（插入 `workflow_runs` + 全量 `node_runs`/`tasks` 后异步调度）。
    pub async fn start(self: &Arc<Self>, request: RunRequest) -> Result<String, StoreError> {
        let now = self.clock.now_ms();
        let node_count = request.definition.nodes.len() as u32;
        let context = json!({
            "input": request.input,
            "values": {},
            "priority": request.priority,
        });
        let run = WorkflowRunRecord {
            id: request.run_id.clone(),
            workflow_id: request.workflow_id.clone(),
            status: "queued".to_owned(),
            context: context.to_string(),
            started_at: now,
            finished_at: None,
            session_id: Some(request.session_id.clone()),
            definition_version: 1,
            definition_hash: Some(definition_hash(&request.definition_value)),
            error: None,
            cancel_requested_at: None,
        };
        self.write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::InsertWorkflowRun { run },
            })
            .await?;
        let mut initial = BTreeMap::new();
        for node in &request.definition.nodes {
            let node_run_id = ulid::Ulid::new().to_string();
            let task_id = ulid::Ulid::new().to_string();
            self.write
                .execute(StoreCommand::Workflow {
                    command: WorkflowCommand::InsertNodeRun {
                        node: NodeRunRecord {
                            id: node_run_id.clone(),
                            workflow_run_id: request.run_id.clone(),
                            node_id: node.id.clone(),
                            session_id: None,
                            status: "pending".to_owned(),
                            input: None,
                            output: None,
                            error: None,
                            attempt: 1,
                            started_at: None,
                            finished_at: None,
                        },
                    },
                })
                .await?;
            self.write
                .execute(StoreCommand::Workflow {
                    command: WorkflowCommand::InsertTask {
                        task: TaskRecord {
                            id: task_id.clone(),
                            session_id: Some(request.session_id.clone()),
                            workflow_run_id: Some(request.run_id.clone()),
                            node_id: Some(node.id.clone()),
                            task_type: node.node_type.clone(),
                            status: "queued".to_owned(),
                            priority: request.priority,
                            dependencies: "[]".to_owned(),
                            input: None,
                            result: None,
                            retry_count: 0,
                            timeout_ms: node.timeout_ms,
                            created_at: now,
                            updated_at: now,
                        },
                    },
                })
                .await?;
            initial.insert(
                node.id.clone(),
                NodeState {
                    status: "pending".to_owned(),
                    attempt: 1,
                    node_run_id,
                    task_id,
                    output: None,
                    error: None,
                    retryable: false,
                    started_at: None,
                    finished_at: None,
                    retry_at: None,
                    duration_ms: 0,
                },
            );
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        let (done, _done_rx) = watch::channel(false);
        let status = Arc::new(std::sync::Mutex::new("queued".to_owned()));
        {
            let mut active = match self.active.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            active.insert(
                request.run_id.clone(),
                ActiveRun {
                    cancel: cancel.clone(),
                    cancel_notify: cancel_notify.clone(),
                    done,
                    status: status.clone(),
                },
            );
        }
        let meta = RunMeta {
            run_id: request.run_id.clone(),
            workflow_id: request.workflow_id.clone(),
            session_id: request.session_id.clone(),
            runtime_id: request.runtime_id.clone(),
            priority: request.priority,
        };
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            engine
                .execute_run(
                    meta,
                    request.definition,
                    request.input,
                    node_count,
                    cancel,
                    cancel_notify,
                    status,
                    Some(RetrySeed { states: initial }),
                )
                .await;
        });
        Ok(request.run_id)
    }

    /// 取消运行（ack ≤1s；≤5s 收口；超时返回 `WorkflowCancelOutcome::Timeout`）。
    pub async fn cancel(&self, run_id: &str, reason: &str) -> Result<CancelOutcome, StoreError> {
        let handle = {
            let active = match self.active.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            active.get(run_id).map(|handle| {
                (
                    handle.cancel.clone(),
                    handle.cancel_notify.clone(),
                    handle.done.subscribe(),
                )
            })
        };
        if let Some((cancel, notify, mut done)) = handle {
            cancel.store(true, Ordering::SeqCst);
            notify.notify_waiters();
            let wait_done = async {
                loop {
                    if *done.borrow() {
                        return true;
                    }
                    if done.changed().await.is_err() {
                        return true;
                    }
                }
            };
            match tokio::time::timeout(CANCEL_TIMEOUT, wait_done).await {
                Ok(_) => return Ok(CancelOutcome::Cancelled),
                Err(_) => {
                    // 5s 未收口：强制标 cancelled + cleanup 记录（ADR-031 D-031-4）。
                    self.force_cancel(run_id, reason).await?;
                    return Ok(CancelOutcome::Timeout);
                }
            }
        }
        // 非本进程活动运行：按 DB 状态收口。
        let Some(run) = self.reads.workflow_run(run_id).await? else {
            return Ok(CancelOutcome::NotFound);
        };
        if run.status == "queued" || run.status == "running" {
            self.force_cancel(run_id, reason).await?;
            return Ok(CancelOutcome::Cancelled);
        }
        Ok(CancelOutcome::NotActive)
    }

    async fn force_cancel(&self, run_id: &str, reason: &str) -> Result<(), StoreError> {
        let now = self.clock.now_ms();
        self.write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::CancelRunNodes {
                    workflow_run_id: run_id.to_owned(),
                    at: now,
                    reason: "cancel_timeout".to_owned(),
                },
            })
            .await?;
        self.write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::UpdateWorkflowRun {
                    id: run_id.to_owned(),
                    status: "cancelled".to_owned(),
                    error: Some("cancel_timeout".to_owned()),
                    finished_at: Some(now),
                    cancel_requested_at: Some(now),
                },
            })
            .await?;
        if let Some(run) = self.reads.workflow_run(run_id).await? {
            self.emit_cancelled(&run, reason, 0).await;
        }
        Ok(())
    }

    /// 重试节点（新建 attempt；终态行不改写）。
    pub async fn retry_node(
        self: &Arc<Self>,
        run_id: &str,
        node_id: &str,
        reset_downstream: bool,
    ) -> Result<(String, u32), RetryError> {
        let run = self
            .reads
            .workflow_run(run_id)
            .await
            .map_err(RetryError::Store)?
            .ok_or(RetryError::RunNotFound)?;
        if run.status == "queued" || run.status == "running" {
            return Err(RetryError::RunActive);
        }
        let workflow = self
            .reads
            .workflow(&run.workflow_id, true)
            .await
            .map_err(RetryError::Store)?
            .ok_or(RetryError::RunNotFound)?;
        let definition_value: Value =
            serde_json::from_str(&workflow.definition).map_err(|_| RetryError::NodeNotRetryable)?;
        let definition =
            parse_and_validate(&definition_value).map_err(|_| RetryError::NodeNotRetryable)?;
        let nodes = self
            .reads
            .node_runs(run_id)
            .await
            .map_err(RetryError::Store)?;
        let latest = latest_attempts(&nodes);
        let Some(target) = latest.get(node_id) else {
            return Err(RetryError::NodeNotFound);
        };
        if !matches!(target.status.as_str(), "failed" | "timeout" | "cancelled") {
            return Err(RetryError::NodeNotRetryable);
        }
        let mut states = BTreeMap::new();
        for node in &definition.nodes {
            if let Some(record) = latest.get(&node.id) {
                states.insert(
                    node.id.clone(),
                    NodeState {
                        status: record.status.clone(),
                        attempt: u32::try_from(record.attempt).unwrap_or(1),
                        node_run_id: record.id.clone(),
                        task_id: String::new(),
                        output: record
                            .output
                            .as_deref()
                            .and_then(|text| serde_json::from_str(text).ok()),
                        error: record.error.clone(),
                        retryable: false,
                        started_at: record.started_at,
                        finished_at: record.finished_at,
                        retry_at: None,
                        duration_ms: duration_between(record.started_at, record.finished_at),
                    },
                );
            }
        }
        let descendants = if reset_downstream {
            descendants_of(&definition, node_id)
        } else {
            BTreeSet::new()
        };
        let mut retry_nodes = BTreeSet::new();
        retry_nodes.insert(node_id.to_owned());
        retry_nodes.extend(descendants);
        let mut target_result = None;
        for id in retry_nodes {
            let state = states.entry(id.clone()).or_insert_with(|| NodeState {
                status: "pending".to_owned(),
                attempt: 0,
                node_run_id: String::new(),
                task_id: String::new(),
                output: None,
                error: None,
                retryable: false,
                started_at: None,
                finished_at: None,
                retry_at: None,
                duration_ms: 0,
            });
            state.status = "pending".to_owned();
            state.attempt = state.attempt.saturating_add(1);
            state.output = None;
            state.error = None;
            state.finished_at = None;
            state.retry_at = None;
            let node_run_id = ulid::Ulid::new().to_string();
            self.write
                .execute(StoreCommand::Workflow {
                    command: WorkflowCommand::InsertNodeRun {
                        node: NodeRunRecord {
                            id: node_run_id.clone(),
                            workflow_run_id: run_id.to_owned(),
                            node_id: id.clone(),
                            session_id: None,
                            status: "pending".to_owned(),
                            input: None,
                            output: None,
                            error: None,
                            attempt: i64::from(state.attempt),
                            started_at: None,
                            finished_at: None,
                        },
                    },
                })
                .await
                .map_err(RetryError::Store)?;
            state.node_run_id = node_run_id.clone();
            if id == node_id {
                target_result = Some((node_run_id, state.attempt));
            }
        }
        self.write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::UpdateWorkflowRun {
                    id: run_id.to_owned(),
                    status: "running".to_owned(),
                    error: None,
                    finished_at: None,
                    cancel_requested_at: None,
                },
            })
            .await
            .map_err(RetryError::Store)?;
        let context_doc: Value = serde_json::from_str(&run.context).unwrap_or_else(|_| json!({}));
        let input = context_doc.get("input").cloned().unwrap_or(Value::Null);
        let session = self
            .reads
            .session(
                &aether_core::SessionId::new(run.session_id.clone().unwrap_or_default())
                    .map_err(|_| RetryError::RunNotFound)?,
            )
            .await
            .map_err(RetryError::Store)?;
        let runtime_id = session
            .as_ref()
            .map(|session| session.runtime_id.as_str().to_owned())
            .unwrap_or_else(|| "unknown".to_owned());
        let meta = RunMeta {
            run_id: run_id.to_owned(),
            workflow_id: run.workflow_id.clone(),
            session_id: run.session_id.clone().unwrap_or_default(),
            runtime_id,
            priority: context_doc
                .get("priority")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        };
        self.audit(
            &meta,
            "workflow.node.retry",
            json!({"node_id": node_id, "attempt": target_result.as_ref().map(|(_, attempt)| *attempt), "reset_downstream": reset_downstream}),
            "retrying",
        )
        .await;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        let (done, _done_rx) = watch::channel(false);
        let status = Arc::new(std::sync::Mutex::new("running".to_owned()));
        {
            let mut active = match self.active.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            active.insert(
                meta.run_id.clone(),
                ActiveRun {
                    cancel: cancel.clone(),
                    cancel_notify: cancel_notify.clone(),
                    done,
                    status: status.clone(),
                },
            );
        }
        let engine = Arc::clone(self);
        let node_count = definition.nodes.len() as u32;
        let seed = RetrySeed { states };
        tokio::spawn(async move {
            engine
                .execute_run(
                    meta,
                    definition,
                    input,
                    node_count,
                    cancel,
                    cancel_notify,
                    status,
                    Some(seed),
                )
                .await;
        });
        target_result.ok_or(RetryError::NodeNotFound)
    }

    /// daemon 启动 reconcile：running/queued → interrupted/cancelled（不自动重放）。
    pub async fn reconcile(&self) -> Result<(u64, u64), StoreError> {
        let now = self.clock.now_ms();
        match self
            .write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::ReconcileRunning { reconciled_at: now },
            })
            .await?
        {
            StoreOutcome::WorkflowReconciled { runs, nodes } => Ok((runs, nodes)),
            _ => Ok((0, 0)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_run(
        self: Arc<Self>,
        meta: RunMeta,
        definition: WorkflowDefinition,
        input: Value,
        node_count: u32,
        cancel: Arc<AtomicBool>,
        cancel_notify: Arc<tokio::sync::Notify>,
        status_slot: Arc<std::sync::Mutex<String>>,
        seed: Option<RetrySeed>,
    ) {
        // 全局并发闸门：排队等待；取消可中断。
        let permit = tokio::select! {
            permit = self.gate.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    self.finalize_gate_closed(&meta).await;
                    return;
                }
            },
            _ = cancel_notify.notified() => {
                if cancel.load(Ordering::SeqCst) {
                    self.finalize_cancelled(&meta, "cancelled_before_start", node_count).await;
                    set_status(&status_slot, "cancelled");
                    return;
                }
                return;
            }
        };
        if cancel.load(Ordering::SeqCst) {
            self.finalize_cancelled(&meta, "cancelled_before_start", node_count)
                .await;
            set_status(&status_slot, "cancelled");
            return;
        }
        set_status(&status_slot, "running");
        let now = self.clock.now_ms();
        if let Err(error) = self
            .write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::UpdateWorkflowRun {
                    id: meta.run_id.clone(),
                    status: "running".to_owned(),
                    error: None,
                    finished_at: None,
                    cancel_requested_at: None,
                },
            })
            .await
        {
            tracing::warn!(run_id = %meta.run_id, error = %error, "工作流运行启动写库失败");
            self.finalize_storage_failure(&meta, node_count, &error)
                .await;
            set_status(&status_slot, "failed");
            return;
        }
        self.emit_started(&meta, node_count).await;
        self.audit(
            &meta,
            "workflow.started",
            json!({"node_count": node_count, "priority": meta.priority}),
            "queued",
        )
        .await;

        let mut states = match seed {
            Some(seed) => seed.states,
            None => initial_states(&definition),
        };
        // 兜底：seed 缺失的节点补 pending。
        for node in &definition.nodes {
            states.entry(node.id.clone()).or_insert(NodeState {
                status: "pending".to_owned(),
                attempt: 1,
                node_run_id: String::new(),
                task_id: String::new(),
                output: None,
                error: None,
                retryable: false,
                started_at: None,
                finished_at: None,
                retry_at: None,
                duration_ms: 0,
            });
        }
        let context = Arc::new(tokio::sync::Mutex::new(
            serde_json::from_str::<Value>(&self.load_context(&meta).await)
                .ok()
                .and_then(|doc| doc.get("values").cloned())
                .unwrap_or_else(|| json!({})),
        ));
        let context_doc = serde_json::from_str::<Value>(&self.load_context(&meta).await).ok();
        let _ = context_doc;
        let deadline = definition
            .run_timeout_ms()
            .map(|timeout| now.saturating_add(timeout));
        let mut running: tokio::task::JoinSet<NodeOutcome> = tokio::task::JoinSet::new();
        let mut started_count = 0usize;
        let mut cancelled = false;
        let mut timed_out = false;
        let mut fail_fast_abort = false;
        let mut storage_error: Option<String> = None;
        let max_parallel = definition.max_parallel();

        loop {
            if cancel.load(Ordering::SeqCst) {
                cancelled = true;
                running.abort_all();
                break;
            }
            if let Some(deadline) = deadline {
                if self.clock.now_ms() >= deadline {
                    timed_out = true;
                    running.abort_all();
                    break;
                }
            }
            if fail_fast_abort {
                running.abort_all();
                break;
            }

            // 处理退避到期的重试：attempt+1 并继续保持 pending。
            let now_ms = self.clock.now_ms();
            let mut retry_ready = false;
            for state in states.values_mut() {
                if state.status == "backoff" && state.retry_at.unwrap_or(0) <= now_ms {
                    state.status = "pending".to_owned();
                    state.attempt = state.attempt.saturating_add(1);
                    retry_ready = true;
                }
            }

            // 跳过：所有入边解析为 false。
            let mut skipped_any = false;
            for node in &definition.nodes {
                if !states
                    .get(&node.id)
                    .is_some_and(|state| state.status == "pending")
                {
                    continue;
                }
                let incoming = definition.incoming(&node.id);
                if incoming.is_empty() {
                    continue;
                }
                let all_resolved = incoming
                    .iter()
                    .all(|edge| states.get(&edge.from).is_some_and(NodeState::terminal));
                if !all_resolved || node.node_type == "join" {
                    continue;
                }
                let mut any_true = false;
                for edge in &incoming {
                    if self
                        .edge_enabled(&definition, &states, edge, &input, &context, &meta)
                        .await
                    {
                        any_true = true;
                        break;
                    }
                }
                if !any_true {
                    if self
                        .mark_skipped(&meta, node, &mut states, "condition_false")
                        .await
                        .is_err()
                    {
                        storage_error = Some("storage_backpressure".to_owned());
                    }
                    skipped_any = true;
                }
            }
            if (retry_ready || skipped_any) && running.len() < max_parallel {
                // 继续调度。
            }

            // 计算可启动节点。
            let mut ready: Vec<String> = Vec::new();
            for node in &definition.nodes {
                if started_count >= super::definition::MAX_NODE_EXECUTIONS {
                    break;
                }
                let Some(state) = states.get(&node.id) else {
                    continue;
                };
                if state.status != "pending" {
                    continue;
                }
                let incoming = definition.incoming(&node.id);
                if incoming.is_empty() {
                    ready.push(node.id.clone());
                    continue;
                }
                let all_resolved = incoming
                    .iter()
                    .all(|edge| states.get(&edge.from).is_some_and(NodeState::terminal));
                if !all_resolved {
                    continue;
                }
                if node.node_type == "join" {
                    ready.push(node.id.clone());
                    continue;
                }
                let mut any_true = false;
                for edge in &incoming {
                    if self
                        .edge_enabled(&definition, &states, edge, &input, &context, &meta)
                        .await
                    {
                        any_true = true;
                        break;
                    }
                }
                if any_true {
                    ready.push(node.id.clone());
                }
            }
            // 启动直到并行上限。
            for node_id in ready {
                if running.len() >= max_parallel {
                    break;
                }
                if started_count >= super::definition::MAX_NODE_EXECUTIONS {
                    fail_fast_abort = true;
                    break;
                }
                let node = match definition.node(&node_id) {
                    Some(node) => node.clone(),
                    None => continue,
                };
                let Some(state) = states.get_mut(&node_id) else {
                    continue;
                };
                state.status = "running".to_owned();
                state.started_at = Some(self.clock.now_ms());
                state.finished_at = None;
                state.error = None;
                state.output = None;
                started_count += 1;
                let attempt = state.attempt;
                let task_id = if state.task_id.is_empty() {
                    let id = ulid::Ulid::new().to_string();
                    state.task_id = id.clone();
                    id
                } else {
                    state.task_id.clone()
                };
                if let Err(error) = self
                    .write
                    .execute(StoreCommand::Workflow {
                        command: WorkflowCommand::UpdateNodeRun {
                            id: state.node_run_id.clone(),
                            status: "running".to_owned(),
                            session_id: None,
                            output: None,
                            error: None,
                            started_at: state.started_at,
                            finished_at: None,
                        },
                    })
                    .await
                {
                    storage_error = Some(format!("storage_backpressure: {error}"));
                    fail_fast_abort = true;
                    break;
                }
                let _ = self
                    .write
                    .execute(StoreCommand::Workflow {
                        command: WorkflowCommand::UpdateTask {
                            id: task_id.clone(),
                            status: "running".to_owned(),
                            result: None,
                            updated_at: self.clock.now_ms(),
                        },
                    })
                    .await;
                let node_input = self
                    .build_node_input(&definition, &states, &node, &input, &context, &meta)
                    .await;
                self.emit_node_started(&meta, &node, attempt).await;
                let engine = Arc::clone(&self);
                let meta_clone = meta.clone();
                let input_clone = input.clone();
                let context_clone = Arc::clone(&context);
                let definition_clone = definition.clone();
                let node_clone = node.clone();
                let cancel_clone = Arc::clone(&cancel);
                self.max_active_nodes.fetch_add(1, Ordering::Relaxed);
                running.spawn(async move {
                    let started = engine.clock.now_ms();
                    let timeout = node_clone
                        .timeout_ms
                        .unwrap_or(super::definition::DEFAULT_NODE_TIMEOUT_MS);
                    let future = engine.execute_node(
                        &meta_clone,
                        &definition_clone,
                        &node_clone,
                        &node_input,
                        &input_clone,
                        &context_clone,
                        attempt,
                        &cancel_clone,
                    );
                    let result = match tokio::time::timeout(
                        Duration::from_millis(u64::try_from(timeout).unwrap_or(u64::MAX)),
                        future,
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(NodeFailure {
                            code: "timeout".to_owned(),
                            message: format!("节点执行超时（{timeout} ms）"),
                            retryable: true,
                        }),
                    };
                    let duration_ms =
                        u64::try_from((engine.clock.now_ms() - started).max(0)).unwrap_or(u64::MAX);
                    engine.max_active_nodes.fetch_sub(1, Ordering::Relaxed);
                    if cancel_clone.load(Ordering::SeqCst) {
                        return NodeOutcome {
                            node_id: node_clone.id,
                            attempt,
                            duration_ms,
                            result: Err(NodeFailure {
                                code: "cancelled".to_owned(),
                                message: "运行已取消".to_owned(),
                                retryable: false,
                            }),
                        };
                    }
                    NodeOutcome {
                        node_id: node_clone.id,
                        attempt,
                        duration_ms,
                        result,
                    }
                });
            }

            if running.is_empty() {
                let all_terminal = states.values().all(NodeState::terminal);
                if all_terminal {
                    break;
                }
                // 有退避等待：睡到最近一次重试。
                let next_retry = states
                    .values()
                    .filter(|state| state.status == "backoff")
                    .filter_map(|state| state.retry_at)
                    .min();
                if let Some(retry_at) = next_retry {
                    let wait = (retry_at - self.clock.now_ms()).max(1);
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(u64::try_from(wait).unwrap_or(1))) => {}
                        _ = cancel_notify.notified() => {}
                    }
                    continue;
                }
                // 无可运行节点且非全终态：依赖死锁/失败上游 → 标记剩余 skipped。
                for node in &definition.nodes {
                    if let Some(state) = states.get_mut(&node.id) {
                        if state.status == "pending" {
                            if let Err(error) = self
                                .mark_skipped(&meta, node, &mut states, "upstream_failed")
                                .await
                            {
                                storage_error = Some(format!("storage_backpressure: {error}"));
                            }
                        }
                    }
                }
                continue;
            }

            tokio::select! {
                joined = running.join_next() => {
                    match joined {
                        Some(Ok(outcome)) => {
                            if let Err(error) = self
                                .handle_node_outcome(
                                    &meta,
                                    &definition,
                                    &mut states,
                                    outcome,
                                    &context,
                                    &mut fail_fast_abort,
                                    &mut storage_error,
                                )
                                .await
                            {
                                storage_error = Some(format!("storage_backpressure: {error}"));
                                fail_fast_abort = true;
                            }
                        }
                        Some(Err(_)) => {
                            storage_error = Some("node_task_join_error".to_owned());
                            fail_fast_abort = true;
                        }
                        None => {}
                    }
                }
                _ = cancel_notify.notified() => {}
            }
        }

        // 收口在途节点：取消/超时/失败策略。
        if cancelled || timed_out || fail_fast_abort {
            running.abort_all();
            while let Some(_joined) = running.join_next().await {}
            let reason = if cancelled {
                "cancelled".to_owned()
            } else if timed_out {
                "workflow_timeout".to_owned()
            } else {
                "fail_fast".to_owned()
            };
            let now_ms = self.clock.now_ms();
            for node in &definition.nodes {
                if let Some(state) = states.get_mut(&node.id) {
                    if !state.terminal() && state.status != "backoff" {
                        state.status = "cancelled".to_owned();
                        state.finished_at = Some(now_ms);
                        state.error = Some(reason.clone());
                        let _ = self
                            .write
                            .execute(StoreCommand::Workflow {
                                command: WorkflowCommand::UpdateNodeRun {
                                    id: state.node_run_id.clone(),
                                    status: "cancelled".to_owned(),
                                    session_id: None,
                                    output: None,
                                    error: Some(reason.clone()),
                                    started_at: None,
                                    finished_at: Some(now_ms),
                                },
                            })
                            .await;
                        self.emit_node_cancelled(&meta, node, state.attempt, &reason)
                            .await;
                    } else if state.status == "backoff" {
                        state.status = "cancelled".to_owned();
                        state.finished_at = Some(now_ms);
                        state.error = Some(reason.clone());
                    }
                }
            }
        }

        let duration = u64::try_from((self.clock.now_ms() - now).max(0)).unwrap_or(u64::MAX);
        let failed_node = states
            .iter()
            .find(|(_, state)| matches!(state.status.as_str(), "failed" | "timeout"))
            .map(|(id, _)| id.clone());
        let any_failed = failed_node.is_some();
        let final_status = if cancelled {
            "cancelled"
        } else if timed_out || storage_error.is_some() || any_failed {
            "failed"
        } else {
            "succeeded"
        };
        let error_text = if timed_out {
            Some("workflow_timeout".to_owned())
        } else if let Some(error) = storage_error {
            Some(error)
        } else {
            failed_node
                .as_ref()
                .and_then(|id| states.get(id))
                .and_then(|state| state.error.clone())
        };
        let finished_at = self.clock.now_ms();
        if let Err(error) = self
            .write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::UpdateWorkflowRun {
                    id: meta.run_id.clone(),
                    status: final_status.to_owned(),
                    error: error_text.clone(),
                    finished_at: Some(finished_at),
                    cancel_requested_at: if cancelled { Some(finished_at) } else { None },
                },
            })
            .await
        {
            tracing::warn!(run_id = %meta.run_id, error = %error, "工作流终态写库失败");
        }
        if cancelled {
            let remaining = states
                .values()
                .filter(|state| matches!(state.status.as_str(), "cancelled"))
                .count() as u32;
            if let Ok(Some(run)) = self.reads.workflow_run(&meta.run_id).await {
                self.emit_cancelled(&run, "user", remaining).await;
            }
            self.audit(
                &meta,
                "workflow.cancelled",
                json!({"remaining_nodes": remaining}),
                "cancelled",
            )
            .await;
        } else if final_status == "failed" {
            self.emit_failed(
                &meta,
                failed_node.as_deref(),
                error_text.as_deref().unwrap_or("workflow_failed"),
                error_text.as_deref() == Some("workflow_timeout"),
            )
            .await;
            self.audit(
                &meta,
                "workflow.failed",
                json!({
                    "failed_node_id": failed_node,
                    "error": error_text,
                }),
                "failed",
            )
            .await;
        } else {
            self.emit_completed(&meta, node_count, duration).await;
            self.audit(
                &meta,
                "workflow.completed",
                json!({"node_count": node_count, "duration_ms": duration}),
                "succeeded",
            )
            .await;
        }
        let _ = self.flush_context(&meta, &context).await;
        set_status(&status_slot, final_status);
        {
            let active = match self.active.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(handle) = active.get(&meta.run_id) {
                let _ = handle.done.send(true);
            }
        }
        {
            let mut active = match self.active.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            active.remove(&meta.run_id);
        }
        drop(permit);
    }

    async fn edge_enabled(
        &self,
        _definition: &WorkflowDefinition,
        states: &BTreeMap<String, NodeState>,
        edge: &super::definition::WorkflowEdge,
        input: &Value,
        context: &Arc<tokio::sync::Mutex<Value>>,
        _meta: &RunMeta,
    ) -> bool {
        let Some(source) = states.get(&edge.from) else {
            return false;
        };
        if !source.success() {
            return false;
        }
        let Some(when) = &edge.when else {
            return true;
        };
        let Ok(expression) = parse_expression(when) else {
            return false;
        };
        let results = results_map(states);
        let context_guard = context.lock().await;
        let environment = EvalContext {
            input,
            context: &context_guard,
            results: &results,
        };
        eval_expression(&expression, &environment)
            .map(|value| truthy(&value))
            .unwrap_or(false)
    }

    async fn build_node_input(
        &self,
        definition: &WorkflowDefinition,
        states: &BTreeMap<String, NodeState>,
        node: &WorkflowNode,
        input: &Value,
        context: &Arc<tokio::sync::Mutex<Value>>,
        _meta: &RunMeta,
    ) -> Value {
        let results = results_map(states);
        let incoming = definition.incoming(&node.id);
        let upstream: BTreeMap<String, Value> = incoming
            .iter()
            .filter(|edge| {
                states
                    .get(&edge.from)
                    .is_some_and(|state| state.success() && state.output.is_some())
            })
            .filter_map(|edge| {
                let output = states
                    .get(&edge.from)
                    .and_then(|state| state.output.clone())?;
                Some((edge.from.clone(), output))
            })
            .collect();
        let incoming_statuses: Vec<Value> = incoming
            .iter()
            .filter_map(|edge| {
                let state = states.get(&edge.from)?;
                Some(json!({
                    "node_id": edge.from,
                    "status": state.status,
                    "output": state.output,
                    "error": state.error,
                }))
            })
            .collect();
        let allowlist = definition.context_allowlist();
        let context_value = context.lock().await.clone();
        let allowed_context: MapLike = allowlist
            .iter()
            .filter_map(|key| {
                context_value
                    .get(key)
                    .map(|value| (key.clone(), value.clone()))
            })
            .collect();
        json!({
            "workflow_input": input,
            "upstream_results": upstream,
            "incoming_statuses": incoming_statuses,
            "allowlisted_context": allowed_context,
            "node_config": {
                "id": node.id,
                "type": node.node_type,
                "handler": node.handler,
                "expression": node.expression,
                "fanout": node.fanout,
                "mode": node.mode,
                "quorum": node.quorum,
                "input": node.input,
            },
            "results_ref": results.keys().cloned().collect::<Vec<_>>(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_node(
        &self,
        meta: &RunMeta,
        definition: &WorkflowDefinition,
        node: &WorkflowNode,
        node_input: &Value,
        run_input: &Value,
        context: &Arc<tokio::sync::Mutex<Value>>,
        attempt: u32,
        cancel: &Arc<AtomicBool>,
    ) -> Result<NodeSuccess, NodeFailure> {
        let Some(node_type) = NodeType::parse(&node.node_type) else {
            return Err(NodeFailure {
                code: "unknown_node_type".to_owned(),
                message: format!("未知节点类型：{}", node.node_type),
                retryable: false,
            });
        };
        if cancel.load(Ordering::SeqCst) {
            return Err(NodeFailure {
                code: "cancelled".to_owned(),
                message: "运行已取消".to_owned(),
                retryable: false,
            });
        }
        match node_type {
            NodeType::Agent => {
                let prompt = node.prompt.clone().unwrap_or_default();
                let future = self.agent.execute(AgentRequest {
                    workflow_run_id: meta.run_id.clone(),
                    workflow_id: meta.workflow_id.clone(),
                    session_id: meta.session_id.clone(),
                    node_id: node.id.clone(),
                    attempt,
                    prompt,
                    context_refs: node.context_refs.clone().unwrap_or_default(),
                    runtime_id: node.runtime_id.clone(),
                    provider_id: node.provider_id.clone(),
                    model: node.model.clone(),
                    permission_mode: node.permission_mode.clone(),
                    timeout_ms: node
                        .timeout_ms
                        .unwrap_or(super::definition::DEFAULT_NODE_TIMEOUT_MS),
                    input: node_input.clone(),
                    cancel: Arc::clone(cancel),
                });
                match future.await {
                    Ok(outcome) => Ok(NodeSuccess {
                        output: outcome.output,
                        session_id: outcome.session_id,
                    }),
                    Err(error) => Err(NodeFailure {
                        code: error.code,
                        message: error.message,
                        retryable: error.retryable,
                    }),
                }
            }
            NodeType::Task => self.execute_task(node, node_input, context).await,
            NodeType::Condition => {
                let Some(expression) = &node.expression else {
                    return Err(NodeFailure {
                        code: "invalid_value".to_owned(),
                        message: "condition 节点缺少 expression".to_owned(),
                        retryable: false,
                    });
                };
                let parsed = parse_expression(expression).map_err(|reason| NodeFailure {
                    code: "invalid_expression".to_owned(),
                    message: reason,
                    retryable: false,
                })?;
                let results: BTreeMap<String, Value> = node_input
                    .get("upstream_results")
                    .and_then(Value::as_object)
                    .map(|object| {
                        object
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                let context_guard = context.lock().await;
                let environment = EvalContext {
                    input: run_input,
                    context: &context_guard,
                    results: &results,
                };
                let value =
                    eval_expression(&parsed, &environment).map_err(|reason| NodeFailure {
                        code: "expression_error".to_owned(),
                        message: reason,
                        retryable: false,
                    })?;
                Ok(NodeSuccess {
                    output: json!({ "value": value }),
                    session_id: None,
                })
            }
            NodeType::Parallel => {
                let fanout = node.fanout.clone().unwrap_or(Value::Null);
                Ok(NodeSuccess {
                    output: json!({
                        "fanout": fanout,
                        "expanded": definition.outgoing(&node.id).len(),
                    }),
                    session_id: None,
                })
            }
            NodeType::Join => {
                let incoming = node_input
                    .get("incoming_statuses")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut results = Vec::new();
                let mut successes = Vec::new();
                let mut failed = Vec::new();
                for entry in &incoming {
                    let id = entry
                        .get("node_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let status = entry
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("failed")
                        .to_owned();
                    let output = entry.get("output").cloned().unwrap_or(Value::Null);
                    if status == "succeeded" {
                        successes.push(id.clone());
                    } else if status != "skipped" {
                        failed.push(id.clone());
                    }
                    results.push(json!({
                        "node_id": id,
                        "status": status,
                        "output": output,
                    }));
                }
                results.sort_by(|left, right| {
                    left.get("node_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .cmp(
                            right
                                .get("node_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        )
                });
                let incoming_count = results.len();
                let mode = node.mode.as_deref().unwrap_or("all");
                let mode_ok = match mode {
                    "all" => failed.is_empty() && !successes.is_empty(),
                    "any" => !successes.is_empty(),
                    "quorum" => successes.len() >= node.quorum.unwrap_or(1) as usize,
                    _ => false,
                };
                let _ = incoming_count;
                if !mode_ok {
                    match node.on_failure.as_deref().unwrap_or("fail") {
                        "continue" => {
                            return Ok(NodeSuccess {
                                output: json!({
                                    "results": results,
                                    "joined": {"status": "partial", "successes": successes},
                                }),
                                session_id: None,
                            });
                        }
                        "skip" => {
                            return Ok(NodeSuccess {
                                output: json!({
                                    "results": results,
                                    "joined": {"status": "skipped", "successes": successes},
                                }),
                                session_id: None,
                            });
                        }
                        _ => {
                            return Err(NodeFailure {
                                code: "join_not_satisfied".to_owned(),
                                message: format!("join mode={mode} 条件不满足"),
                                retryable: false,
                            });
                        }
                    }
                }
                Ok(NodeSuccess {
                    output: json!({
                        "results": results,
                        "joined": {
                            "status": "satisfied",
                            "mode": mode,
                            "successes": successes,
                        },
                    }),
                    session_id: None,
                })
            }
        }
    }

    async fn execute_task(
        &self,
        node: &WorkflowNode,
        node_input: &Value,
        context: &Arc<tokio::sync::Mutex<Value>>,
    ) -> Result<NodeSuccess, NodeFailure> {
        match node.handler.as_deref().unwrap_or("noop") {
            "noop" => Ok(NodeSuccess {
                output: json!({"ok": true}),
                session_id: None,
            }),
            "set_context" => {
                let input = node
                    .input
                    .clone()
                    .or_else(|| {
                        node_input
                            .get("node_config")
                            .and_then(|config| config.get("input"))
                            .cloned()
                    })
                    .unwrap_or_else(|| json!({}));
                if !input.is_object() {
                    return Err(NodeFailure {
                        code: "invalid_value".to_owned(),
                        message: "set_context input 必须是对象".to_owned(),
                        retryable: false,
                    });
                }
                let mut guard = context.lock().await;
                let mut conflicts = Vec::new();
                if let Some(object) = input.as_object() {
                    if !guard.is_object() {
                        *guard = json!({});
                    }
                    if let Some(target) = guard.as_object_mut() {
                        for (key, value) in object {
                            if let Some(existing) = target.get(key) {
                                if existing != value {
                                    conflicts.push(json!({
                                        "key": key,
                                        "previous": existing,
                                        "incoming": value,
                                    }));
                                }
                            }
                            target.insert(key.clone(), value.clone());
                        }
                    }
                }
                Ok(NodeSuccess {
                    output: json!({"set": input, "conflicts": conflicts}),
                    session_id: None,
                })
            }
            "merge_results" => {
                let source = node.input.clone().unwrap_or_else(|| {
                    node_input
                        .get("upstream_results")
                        .cloned()
                        .unwrap_or_else(|| json!({}))
                });
                let merged = merge_objects(&source);
                Ok(NodeSuccess {
                    output: json!({
                        "merged": merged.value,
                        "conflicts": merged.conflicts,
                    }),
                    session_id: None,
                })
            }
            "emit_summary" => {
                let context_guard = context.lock().await;
                let summary = json!({
                    "context": context_guard.clone(),
                    "upstream": node_input.get("upstream_results").cloned().unwrap_or(Value::Null),
                });
                let text = serde_json::to_string(&summary).unwrap_or_else(|_| "{}".to_owned());
                Ok(NodeSuccess {
                    output: json!({
                        "summary": text,
                        "results": node_input.get("upstream_results").cloned().unwrap_or(Value::Null),
                    }),
                    session_id: None,
                })
            }
            other => Err(NodeFailure {
                code: "invalid_enum".to_owned(),
                message: format!("未知 task handler：{other}"),
                retryable: false,
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_node_outcome(
        &self,
        meta: &RunMeta,
        definition: &WorkflowDefinition,
        states: &mut BTreeMap<String, NodeState>,
        outcome: NodeOutcome,
        context: &Arc<tokio::sync::Mutex<Value>>,
        fail_fast_abort: &mut bool,
        storage_error: &mut Option<String>,
    ) -> Result<(), StoreError> {
        let Some(node) = definition.node(&outcome.node_id).cloned() else {
            return Ok(());
        };
        let now = self.clock.now_ms();
        let attempt = outcome.attempt;
        let Some(state) = states.get_mut(&outcome.node_id) else {
            return Ok(());
        };
        state.duration_ms = outcome.duration_ms;
        match outcome.result {
            Ok(success) => {
                let (output, truncated) = clamp_result(success.output);
                let output = if truncated {
                    json!({"truncated": true, "value": output})
                } else {
                    output
                };
                state.status = "succeeded".to_owned();
                state.output = Some(output.clone());
                state.finished_at = Some(now);
                // 先写后广播。
                self.write
                    .execute(StoreCommand::Workflow {
                        command: WorkflowCommand::UpdateNodeRun {
                            id: state.node_run_id.clone(),
                            status: "succeeded".to_owned(),
                            session_id: success.session_id.clone(),
                            output: Some(output.to_string()),
                            error: None,
                            started_at: None,
                            finished_at: Some(now),
                        },
                    })
                    .await?;
                let _ = self
                    .write
                    .execute(StoreCommand::Workflow {
                        command: WorkflowCommand::UpdateTask {
                            id: if state.task_id.is_empty() {
                                String::new()
                            } else {
                                state.task_id.clone()
                            },
                            status: "succeeded".to_owned(),
                            result: Some(output.to_string()),
                            updated_at: now,
                        },
                    })
                    .await;
                self.emit_node_completed(
                    meta,
                    &node,
                    attempt,
                    outcome.duration_ms,
                    &state.node_run_id,
                )
                .await;
            }
            Err(error) => {
                state.error = Some(error.message.clone());
                state.retryable = error.retryable;
                state.finished_at = Some(now);
                let retry = error.retryable
                    && state.attempt
                        < node
                            .retry
                            .as_ref()
                            .map(|retry| retry.max_attempts)
                            .unwrap_or(1);
                let failure_status = if error.code == "timeout" {
                    "timeout"
                } else {
                    "failed"
                };
                state.status = if retry {
                    "backoff".to_owned()
                } else {
                    failure_status.to_owned()
                };
                self.write
                    .execute(StoreCommand::Workflow {
                        command: WorkflowCommand::UpdateNodeRun {
                            id: state.node_run_id.clone(),
                            status: failure_status.to_owned(),
                            session_id: None,
                            output: None,
                            error: Some(error.message.clone()),
                            started_at: None,
                            finished_at: Some(now),
                        },
                    })
                    .await?;
                let _ = self
                    .write
                    .execute(StoreCommand::Workflow {
                        command: WorkflowCommand::UpdateTask {
                            id: state.task_id.clone(),
                            status: "failed".to_owned(),
                            result: Some(error.message.clone()),
                            updated_at: now,
                        },
                    })
                    .await;
                self.emit_node_failed(meta, &node, attempt, &error, retry)
                    .await;
                if retry {
                    let (multiplier, base_backoff) = node
                        .retry
                        .as_ref()
                        .map(|policy| {
                            (
                                if policy.multiplier.is_finite() {
                                    policy.multiplier.clamp(1.0, 4.0)
                                } else {
                                    1.0
                                },
                                policy.backoff_ms,
                            )
                        })
                        .unwrap_or((1.0, 100));
                    let backoff = base_backoff as f64
                        * multiplier
                            .powi(i32::try_from(state.attempt.saturating_sub(1)).unwrap_or(0));
                    let jitter = backoff * 0.2 * pseudo_jitter(meta, &node.id, state.attempt);
                    state.retry_at = Some(
                        now.saturating_add(backoff as i64)
                            .saturating_add(jitter as i64),
                    );
                    // 新 attempt 行在 scheduler 重置 pending 时插入。
                    let attempt = state.attempt.saturating_add(1);
                    let node_run_id = ulid::Ulid::new().to_string();
                    self.write
                        .execute(StoreCommand::Workflow {
                            command: WorkflowCommand::InsertNodeRun {
                                node: NodeRunRecord {
                                    id: node_run_id.clone(),
                                    workflow_run_id: meta.run_id.clone(),
                                    node_id: node.id.clone(),
                                    session_id: None,
                                    status: "pending".to_owned(),
                                    input: None,
                                    output: None,
                                    error: None,
                                    attempt: i64::from(attempt),
                                    started_at: None,
                                    finished_at: None,
                                },
                            },
                        })
                        .await?;
                    state.node_run_id = node_run_id;
                    let task_id = ulid::Ulid::new().to_string();
                    self.write
                        .execute(StoreCommand::Workflow {
                            command: WorkflowCommand::InsertTask {
                                task: TaskRecord {
                                    id: task_id.clone(),
                                    session_id: Some(meta.session_id.clone()),
                                    workflow_run_id: Some(meta.run_id.clone()),
                                    node_id: Some(node.id.clone()),
                                    task_type: node.node_type.clone(),
                                    status: "queued".to_owned(),
                                    priority: meta.priority,
                                    dependencies: "[]".to_owned(),
                                    input: None,
                                    result: None,
                                    retry_count: i64::from(attempt.saturating_sub(1)),
                                    timeout_ms: node.timeout_ms,
                                    created_at: self.clock.now_ms(),
                                    updated_at: self.clock.now_ms(),
                                },
                            },
                        })
                        .await?;
                    state.task_id = task_id;
                } else if definition.fail_fast() {
                    *fail_fast_abort = true;
                }
            }
        }
        // set_context 已在 execute_task 内合并；持久化上下文供重启/重试。
        let _ = storage_error;
        self.flush_context(meta, context).await?;
        Ok(())
    }

    async fn mark_skipped(
        &self,
        meta: &RunMeta,
        node: &WorkflowNode,
        states: &mut BTreeMap<String, NodeState>,
        reason: &str,
    ) -> Result<(), StoreError> {
        let now = self.clock.now_ms();
        let Some(state) = states.get_mut(&node.id) else {
            return Ok(());
        };
        state.status = "skipped".to_owned();
        state.finished_at = Some(now);
        state.error = Some(reason.to_owned());
        self.write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::UpdateNodeRun {
                    id: state.node_run_id.clone(),
                    status: "skipped".to_owned(),
                    session_id: None,
                    output: None,
                    error: Some(reason.to_owned()),
                    started_at: None,
                    finished_at: Some(now),
                },
            })
            .await?;
        self.emit_node_skipped(meta, node, reason).await;
        Ok(())
    }

    async fn load_context(&self, meta: &RunMeta) -> String {
        match self.reads.workflow_run(&meta.run_id).await {
            Ok(Some(run)) => run.context,
            _ => "{}".to_owned(),
        }
    }

    async fn flush_context(
        &self,
        meta: &RunMeta,
        context: &Arc<tokio::sync::Mutex<Value>>,
    ) -> Result<(), StoreError> {
        let values = context.lock().await.clone();
        let doc = json!({
            "input": self
                .reads
                .workflow_run(&meta.run_id)
                .await
                .ok()
                .flatten()
                .and_then(|run| serde_json::from_str::<Value>(&run.context).ok())
                .and_then(|value| value.get("input").cloned())
                .unwrap_or(Value::Null),
            "values": values,
            "priority": meta.priority,
        });
        self.write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::UpdateWorkflowRunContext {
                    id: meta.run_id.clone(),
                    context: doc.to_string(),
                },
            })
            .await?;
        Ok(())
    }

    /// 审计追加（ADR-023 台账；失败仅告警，不阻断运行）。
    async fn audit(&self, meta: &RunMeta, action: &str, detail: Value, result: &str) {
        let record = AuditLogRecord {
            id: ulid::Ulid::new().to_string(),
            session_id: Some(meta.session_id.clone()),
            runtime_id: Some(meta.runtime_id.clone()),
            actor: "system".to_owned(),
            action: action.to_owned(),
            resource: Some(format!("workflow:{}", meta.workflow_id)),
            detail: Some(detail.to_string()),
            result: Some(result.to_owned()),
            ts: self.clock.now_ms(),
        };
        if let Err(error) = self
            .write
            .execute(StoreCommand::InsertAudit { record })
            .await
        {
            tracing::warn!(run_id = %meta.run_id, action, error = %error, "工作流审计写入失败");
        }
    }

    // ---- 事件发射（先写后广播由调用点保证） ----

    fn envelope(
        &self,
        meta: &RunMeta,
        event_type: aether_core::EventType,
        payload: Value,
    ) -> Value {
        json!({
            "v": aether_core::EVENT_ENVELOPE_VERSION,
            "id": ulid::Ulid::new().to_string(),
            "session_id": meta.session_id,
            "run_id": null,
            "runtime_id": meta.runtime_id,
            "seq": null,
            "ts": self.clock.now_ms(),
            "type": event_type.as_str(),
            "payload": payload,
        })
    }

    async fn emit(&self, meta: &RunMeta, event_type: aether_core::EventType, payload: Value) {
        let Some(pipeline) = self.pipeline.clone() else {
            return;
        };
        if let Err(error) = pipeline
            .submit(self.envelope(meta, event_type, payload))
            .await
        {
            tracing::warn!(
                run_id = %meta.run_id,
                event = %event_type,
                error = %error,
                "工作流事件提交失败（状态已落库；事件缺口不阻断运行）"
            );
        }
    }

    async fn emit_started(&self, meta: &RunMeta, node_count: u32) {
        self.emit(
            meta,
            aether_core::EventType::WorkflowStarted,
            json!({
                "workflow_run_id": meta.run_id,
                "workflow_id": meta.workflow_id,
                "session_id": meta.session_id,
                "priority": meta.priority,
                "node_count": node_count,
            }),
        )
        .await;
    }

    async fn emit_node_started(&self, meta: &RunMeta, node: &WorkflowNode, attempt: u32) {
        self.emit(
            meta,
            aether_core::EventType::WorkflowNodeStarted,
            json!({
                "workflow_run_id": meta.run_id,
                "workflow_id": meta.workflow_id,
                "node_id": node.id,
                "node_type": node.node_type,
                "attempt": attempt,
            }),
        )
        .await;
    }

    async fn emit_node_completed(
        &self,
        meta: &RunMeta,
        node: &WorkflowNode,
        attempt: u32,
        duration_ms: u64,
        node_run_id: &str,
    ) {
        self.emit(
            meta,
            aether_core::EventType::WorkflowNodeCompleted,
            json!({
                "workflow_run_id": meta.run_id,
                "workflow_id": meta.workflow_id,
                "node_id": node.id,
                "attempt": attempt,
                "status": "succeeded",
                "duration_ms": duration_ms,
                "result_ref": format!("node_run:{node_run_id}"),
            }),
        )
        .await;
    }

    async fn emit_node_failed(
        &self,
        meta: &RunMeta,
        node: &WorkflowNode,
        attempt: u32,
        error: &NodeFailure,
        retryable: bool,
    ) {
        self.emit(
            meta,
            aether_core::EventType::WorkflowNodeFailed,
            json!({
                "workflow_run_id": meta.run_id,
                "workflow_id": meta.workflow_id,
                "node_id": node.id,
                "attempt": attempt,
                "status": "failed",
                "error": {
                    "code": error.code,
                    "message": error.message,
                    "recoverable": retryable,
                },
                "retryable": retryable,
            }),
        )
        .await;
    }

    async fn emit_node_cancelled(
        &self,
        meta: &RunMeta,
        node: &WorkflowNode,
        attempt: u32,
        reason: &str,
    ) {
        self.emit(
            meta,
            aether_core::EventType::WorkflowNodeCancelled,
            json!({
                "workflow_run_id": meta.run_id,
                "workflow_id": meta.workflow_id,
                "node_id": node.id,
                "attempt": attempt,
                "reason": reason,
            }),
        )
        .await;
    }

    async fn emit_node_skipped(&self, meta: &RunMeta, node: &WorkflowNode, reason: &str) {
        self.emit(
            meta,
            aether_core::EventType::WorkflowNodeSkipped,
            json!({
                "workflow_run_id": meta.run_id,
                "workflow_id": meta.workflow_id,
                "node_id": node.id,
                "reason": reason,
            }),
        )
        .await;
    }

    async fn emit_completed(&self, meta: &RunMeta, node_count: u32, duration_ms: u64) {
        self.emit(
            meta,
            aether_core::EventType::WorkflowCompleted,
            json!({
                "workflow_run_id": meta.run_id,
                "workflow_id": meta.workflow_id,
                "status": "succeeded",
                "node_count": node_count,
                "duration_ms": duration_ms,
            }),
        )
        .await;
    }

    async fn emit_failed(
        &self,
        meta: &RunMeta,
        failed_node_id: Option<&str>,
        message: &str,
        retryable: bool,
    ) {
        self.emit(
            meta,
            aether_core::EventType::WorkflowFailed,
            json!({
                "workflow_run_id": meta.run_id,
                "workflow_id": meta.workflow_id,
                "status": "failed",
                "failed_node_id": failed_node_id,
                "error": {
                    "code": if retryable { "workflow_retryable" } else { "workflow_failed" },
                    "message": message,
                    "recoverable": retryable,
                },
                "retryable": retryable,
            }),
        )
        .await;
    }

    async fn emit_cancelled(&self, run: &WorkflowRunRecord, reason: &str, remaining: u32) {
        let session_id = run.session_id.clone().unwrap_or_default();
        let runtime_id = match aether_core::SessionId::new(session_id.clone()) {
            Ok(id) => self
                .reads
                .session(&id)
                .await
                .ok()
                .flatten()
                .map(|session| session.runtime_id.as_str().to_owned())
                .unwrap_or_else(|| "unknown".to_owned()),
            Err(_) => "unknown".to_owned(),
        };
        let meta = RunMeta {
            run_id: run.id.clone(),
            workflow_id: run.workflow_id.clone(),
            session_id,
            runtime_id,
            priority: 0,
        };
        self.emit(
            &meta,
            aether_core::EventType::WorkflowCancelled,
            json!({
                "workflow_run_id": meta.run_id,
                "workflow_id": meta.workflow_id,
                "reason": reason,
                "remaining_nodes": remaining,
            }),
        )
        .await;
    }

    async fn finalize_cancelled(&self, meta: &RunMeta, reason: &str, node_count: u32) {
        let now = self.clock.now_ms();
        let _ = self
            .write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::CancelRunNodes {
                    workflow_run_id: meta.run_id.clone(),
                    at: now,
                    reason: reason.to_owned(),
                },
            })
            .await;
        let _ = self
            .write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::UpdateWorkflowRun {
                    id: meta.run_id.clone(),
                    status: "cancelled".to_owned(),
                    error: Some(reason.to_owned()),
                    finished_at: Some(now),
                    cancel_requested_at: Some(now),
                },
            })
            .await;
        if let Ok(Some(run)) = self.reads.workflow_run(&meta.run_id).await {
            self.emit_cancelled(&run, reason, node_count).await;
        }
    }

    async fn finalize_storage_failure(&self, meta: &RunMeta, node_count: u32, error: &StoreError) {
        let now = self.clock.now_ms();
        let _ = self
            .write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::UpdateWorkflowRun {
                    id: meta.run_id.clone(),
                    status: "failed".to_owned(),
                    error: Some(format!("storage_backpressure: {error}")),
                    finished_at: Some(now),
                    cancel_requested_at: None,
                },
            })
            .await;
        self.emit_failed(meta, None, &format!("storage_backpressure: {error}"), false)
            .await;
        let _ = node_count;
    }

    async fn finalize_gate_closed(&self, meta: &RunMeta) {
        let now = self.clock.now_ms();
        let _ = self
            .write
            .execute(StoreCommand::Workflow {
                command: WorkflowCommand::UpdateWorkflowRun {
                    id: meta.run_id.clone(),
                    status: "interrupted".to_owned(),
                    error: Some("daemon_shutdown".to_owned()),
                    finished_at: Some(now),
                    cancel_requested_at: None,
                },
            })
            .await;
    }
}

/// 取消结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    Cancelled,
    Timeout,
    NotActive,
    NotFound,
}

/// 重试错误。
#[derive(Debug)]
pub enum RetryError {
    Store(StoreError),
    RunNotFound,
    NodeNotFound,
    RunActive,
    NodeNotRetryable,
}

struct RetrySeed {
    states: BTreeMap<String, NodeState>,
}

type MapLike = serde_json::Map<String, Value>;

fn set_status(slot: &Arc<std::sync::Mutex<String>>, value: &str) {
    match slot.lock() {
        Ok(mut guard) => *guard = value.to_owned(),
        Err(poisoned) => *poisoned.into_inner() = value.to_owned(),
    }
}

fn results_map(states: &BTreeMap<String, NodeState>) -> BTreeMap<String, Value> {
    states
        .iter()
        .filter_map(|(id, state)| state.output.clone().map(|output| (id.clone(), output)))
        .collect()
}

fn initial_states(definition: &WorkflowDefinition) -> BTreeMap<String, NodeState> {
    definition
        .nodes
        .iter()
        .map(|node| {
            (
                node.id.clone(),
                NodeState {
                    status: "pending".to_owned(),
                    attempt: 1,
                    node_run_id: String::new(),
                    task_id: String::new(),
                    output: None,
                    error: None,
                    retryable: false,
                    started_at: None,
                    finished_at: None,
                    retry_at: None,
                    duration_ms: 0,
                },
            )
        })
        .collect()
}

fn latest_attempts(nodes: &[NodeRunRecord]) -> HashMap<String, NodeRunRecord> {
    let mut latest: HashMap<String, NodeRunRecord> = HashMap::new();
    for node in nodes {
        match latest.get(&node.node_id) {
            Some(existing) if existing.attempt >= node.attempt => {}
            _ => {
                latest.insert(node.node_id.clone(), node.clone());
            }
        }
    }
    latest
}

fn descendants_of(definition: &WorkflowDefinition, node_id: &str) -> BTreeSet<String> {
    let mut visited = BTreeSet::new();
    let mut stack = vec![node_id.to_owned()];
    while let Some(current) = stack.pop() {
        for edge in definition.outgoing(&current) {
            if visited.insert(edge.to.clone()) {
                stack.push(edge.to.clone());
            }
        }
    }
    visited
}

fn duration_between(started: Option<i64>, finished: Option<i64>) -> u64 {
    match (started, finished) {
        (Some(start), Some(end)) => u64::try_from((end - start).max(0)).unwrap_or(u64::MAX),
        _ => 0,
    }
}

struct Merged {
    value: Value,
    conflicts: Vec<Value>,
}

fn merge_objects(source: &Value) -> Merged {
    let mut merged = serde_json::Map::new();
    let mut conflicts = Vec::new();
    match source {
        Value::Object(object) => {
            let sorted: BTreeMap<&String, &Value> = object.iter().collect();
            for (key, value) in sorted {
                if let Some(existing) = merged.get(key) {
                    if existing != value {
                        conflicts.push(json!({
                            "key": key,
                            "previous": existing,
                            "incoming": value,
                        }));
                    }
                }
                merged.insert(key.clone(), value.clone());
            }
        }
        Value::Array(items) => {
            for item in items {
                if let Value::Object(object) = item {
                    let sorted: BTreeMap<&String, &Value> = object.iter().collect();
                    for (key, value) in sorted {
                        if let Some(existing) = merged.get(key) {
                            if existing != value {
                                conflicts.push(json!({
                                    "key": key,
                                    "previous": existing,
                                    "incoming": value,
                                }));
                            }
                        }
                        merged.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        _ => {}
    }
    Merged {
        value: Value::Object(merged),
        conflicts,
    }
}

fn pseudo_jitter(meta: &RunMeta, node_id: &str, attempt: u32) -> f64 {
    let mut hash = 1469598103934665603u64;
    for byte in meta
        .run_id
        .bytes()
        .chain(node_id.bytes())
        .chain(attempt.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    (hash % 1000) as f64 / 1000.0
}

/// 把 `DefinitionError` 映射为 IPC 错误（供控制层使用）。
pub fn definition_error(error: DefinitionError) -> crate::ipc::IpcError {
    let details: Vec<Value> = error
        .issues
        .iter()
        .map(|issue| {
            json!({
                "path": issue.path,
                "reason": issue.reason,
                "code": issue.code,
            })
        })
        .collect();
    crate::ipc::IpcError::invalid_value(error.message())
        .with_business_code(crate::ipc::BusinessCode::WorkflowInvalidDefinition)
        .with_details(json!({ "details": details, "truncated": error.truncated }))
}
