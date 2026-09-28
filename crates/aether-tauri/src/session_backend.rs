//! 会话命令与消息分页的真实后端（M3-02）。
//!
//! 覆盖 D7 命令面中的会话族与消息分页：
//! - `session_list` / `session_create` / `session_send` / `session_interrupt` /
//!   `session_dispose`：薄适配 [`aether_control::SessionManager`]（状态机/run 串行/
//!   幂等/ack 快路径语义全部在控制层，本层不复刻）；
//! - `messages_page`：双层契约（详见 [`MessagesPageResponse`]）——按 `last_seq` 读
//!   **events 表**（D4 补读语义；`messages.seq` 与 `events.seq` 是两条独立序列，
//!   事件流连续性只能由 events 表承载）；`last_seq` 缺省返回**尾部**一页事件
//!   （升序）并叠加 `messages` 表**尾部**一页消息基线（升序；仅该分支返回）。
//!   缺口 > [`aether_control::READBACK_MAX_GAP`]（10k）→ `readback_gap_too_large`
//!   同码透传（ADR-009 决策 2）；
//! - `runtimes_list`：监督器注册表快照（含 hello 上报的能力清单）。
//!
//! 桥接口径与 `runtime_control` 一致：async 服务 spawn 到核心运行时 + `std::sync::mpsc`
//! 同步等待（不阻塞运行时线程），30s 硬超时。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use aether_adapters::connection::ConnectionState;
use aether_adapters::supervisor::Supervisor;
use aether_control::{LifecycleError, SessionManager, READBACK_MAX_GAP};
use aether_core::{EventEnvelope, Message, Runtime, RuntimeId, SessionId, SessionStatus};
use aether_store::{ReadPool, SessionQuery};
use serde::Serialize;
use serde_json::{json, Value};

use crate::ipc::backend::IpcBackend;
use crate::ipc::dto::{
    MessagesPageRequest, SessionCreateRequest, SessionIdRequest, SessionListRequest,
    SessionSendRequest,
};
use crate::ipc::error::{IpcError, IpcErrorCode};

use crate::adapter_executor::AdapterRunExecutor;

/// 会话命令硬超时（`session.create`/`session.send` 含适配器调用预算，D6 方法表 30s）。
pub const SESSION_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// `messages_page` 响应（ADR-009 决策 1；字段表见下）。
///
/// 双层契约（两条独立 seq 序列，仅 `events` 承载补读水位）：
/// - `events`：D4 补读语义（`last_seq` 断点 / 最近一页）→ `EventBackfillSource`；
///   缺省分支取**尾部** `limit` 条（升序，D4「上限 10k」为缺口上限而非页大小）；
/// - `messages`：会话消息历史（`messages` 表**尾部** `limit` 条，升序）→ 工作台消息
///   基线。**仅最近一页（`last_seq` 缺省）返回**：无消息时为 `Some([])`（字段存在为
///   `[]`）；补读热路径为 `None`（字段省略），不读消息表。
///
/// `complete` 边界：补读分支为「末条 seq == max_seq **或** `last_seq >= max_seq`」
/// （后者视为已到最新，返回空 `events`）；`max_seq` 缺失与最近一页分支恒 `true`。
#[derive(Debug, Clone, Serialize)]
pub struct MessagesPageResponse {
    pub session_id: String,
    /// 请求断点（`None` = 最近一页）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<u64>,
    /// 会话当前最大 seq（无事件为 `null`）。
    pub max_seq: Option<u64>,
    pub events: Vec<EventEnvelope>,
    /// 会话消息历史（升序；仅最近一页返回；无消息为 `Some([])`，补读页为 `None`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<Message>>,
    /// 本页是否已到最新（`false` = 可能仍有后续缺口，调用方续读）。
    pub complete: bool,
}

/// 会话命令后端（装饰器：其余命令透传内层）。
pub struct SessionBackend {
    inner: Arc<dyn IpcBackend>,
    manager: Option<SessionManager>,
    executor: Option<Arc<AdapterRunExecutor>>,
    reads: Option<ReadPool>,
    supervisor: Option<Arc<Supervisor>>,
    handle: tokio::runtime::Handle,
    timeout: Duration,
    /// 自动补读缺口上限（默认 [`READBACK_MAX_GAP`]；常量级调参，测试注入用）。
    gap_limit: u64,
}

impl SessionBackend {
    pub fn new(
        inner: Arc<dyn IpcBackend>,
        manager: Option<SessionManager>,
        executor: Option<Arc<AdapterRunExecutor>>,
        reads: Option<ReadPool>,
        supervisor: Option<Arc<Supervisor>>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            inner,
            manager,
            executor,
            reads,
            supervisor,
            handle,
            timeout: SESSION_COMMAND_TIMEOUT,
            gap_limit: READBACK_MAX_GAP,
        }
    }

    /// 覆盖自动补读缺口上限（测试/故障注入；默认 `READBACK_MAX_GAP`）。
    #[must_use]
    pub fn with_gap_limit(mut self, gap_limit: u64) -> Self {
        self.gap_limit = gap_limit;
        self
    }

    /// 当前缺口上限（诊断）。
    pub fn gap_limit(&self) -> u64 {
        self.gap_limit
    }

    fn manager_required(&self) -> Result<&SessionManager, IpcError> {
        self.manager.as_ref().ok_or_else(|| {
            IpcError::core_not_ready("会话后端未接线：启动序列尚未完成存储/管线注入")
        })
    }

    fn reads_required(&self) -> Result<&ReadPool, IpcError> {
        self.reads.as_ref().ok_or_else(|| {
            IpcError::core_not_ready("会话后端未接线：读连接池不可用（启动失败或未完成）")
        })
    }

    /// 同步桥接：spawn 到核心运行时并等待（与 `SupervisorControl::call` 同口径）。
    fn call<T, F>(&self, future: F) -> Result<T, IpcError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, IpcError>> + Send + 'static,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.handle.spawn(async move {
            let _ = sender.send(future.await);
        });
        receiver.recv_timeout(self.timeout).map_err(|_| {
            IpcError::internal(format!(
                "会话命令超时（>{:?}，核心未在预算内返回）",
                self.timeout
            ))
        })?
    }
}

impl IpcBackend for SessionBackend {
    fn health(&self) -> Result<Value, IpcError> {
        self.inner.health()
    }

    fn runtimes_list(&self) -> Result<Value, IpcError> {
        let supervisor = self.supervisor.clone().ok_or_else(|| {
            IpcError::core_not_ready("监督器未接线：runtimes_list 不可用（启动序列未完成）")
        })?;
        self.call(async move {
            let mut runtimes = Vec::new();
            for id in supervisor.runtime_ids() {
                let Some(runtime) = supervisor.get(&id) else {
                    continue;
                };
                let (status, reason) = runtime.summary_snapshot();
                let manifest = runtime.manifest();
                let capabilities = match runtime.connection().await {
                    Some(connection) => match connection.state() {
                        ConnectionState::Ready(hello) | ConnectionState::Degraded { hello, .. } => {
                            hello.runtime.capabilities
                        }
                        ConnectionState::Connecting | ConnectionState::Disconnected(_) => {
                            Vec::new()
                        }
                    },
                    None => Vec::new(),
                };
                runtimes.push(json!({
                    "id": manifest.id,
                    "name": manifest.name,
                    "kind": manifest.kind,
                    "version": manifest.version,
                    "protocol": manifest.protocol,
                    "capabilities": capabilities,
                    "enabled": manifest.enabled,
                    "status": status.as_str(),
                    "status_reason": reason.map(|reason| reason.as_str()),
                }));
            }
            runtimes.sort_by(|left, right| {
                left["id"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(right["id"].as_str().unwrap_or(""))
            });
            Ok(Value::Array(runtimes))
        })
    }

    fn session_list(&self, request: &SessionListRequest) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let query = SessionQuery {
            runtime_id: request.runtime_id.clone(),
            status: request.status.map(map_session_status),
            parent_session_id: None,
            limit: request.limit,
        };
        self.call(async move {
            let sessions = reads
                .sessions(query)
                .await
                .map_err(|error| IpcError::internal(format!("会话列表读取失败：{error}")))?;
            serde_json::to_value(sessions)
                .map_err(|error| IpcError::internal(format!("会话列表序列化失败：{error}")))
        })
    }

    fn session_create(&self, request: &SessionCreateRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let supervisor = self.supervisor.clone();
        let runtime_id = request.runtime_id.clone();
        let title = request.title.clone();
        let workspace_id = request.workspace_id.clone();
        let model = request.model.clone();
        self.call(async move {
            let runtime = {
                let supervisor = supervisor.ok_or_else(|| {
                    IpcError::core_not_ready("监督器未接线：无法解析 runtime_id（启动序列未完成）")
                })?;
                let runtime = supervisor.get(&runtime_id).ok_or_else(|| {
                    IpcError::invalid_enum(
                        "runtime_id",
                        format!("runtime_id {runtime_id:?} 不在注册表（监督器白名单）"),
                    )
                })?;
                let (status, reason) = runtime.summary_snapshot();
                let manifest = runtime.manifest();
                let (version, capabilities) = match runtime.connection().await {
                    Some(connection) => match connection.state() {
                        ConnectionState::Ready(hello) | ConnectionState::Degraded { hello, .. } => {
                            (hello.runtime.version, hello.runtime.capabilities)
                        }
                        ConnectionState::Connecting | ConnectionState::Disconnected(_) => {
                            (manifest.version.clone(), Vec::new())
                        }
                    },
                    None => (manifest.version.clone(), Vec::new()),
                };
                let now = now_ms();
                Runtime {
                    id: RuntimeId::new(manifest.id.clone()).map_err(|error| {
                        IpcError::internal(format!("runtime_id 非法（监督器 manifest）：{error}"))
                    })?,
                    name: manifest.name.clone(),
                    kind: manifest.kind.clone(),
                    version,
                    protocol: manifest.protocol.clone(),
                    capabilities,
                    endpoint: Some(manifest.program.display().to_string()),
                    config: json!({}),
                    status,
                    status_reason: reason.map(|reason| reason.as_str().to_owned()),
                    last_seen_at: None,
                    created_at: now,
                    updated_at: now,
                }
            };
            let workspace_id =
                match workspace_id {
                    Some(value) => Some(aether_core::WorkspaceId::new(value).map_err(|error| {
                        IpcError::internal(format!("workspace_id 非法：{error}"))
                    })?),
                    None => None,
                };
            let session = manager
                .create_session(runtime, &title, workspace_id, model)
                .await
                .map_err(map_lifecycle_error)?;
            serde_json::to_value(session)
                .map_err(|error| IpcError::internal(format!("会话序列化失败：{error}")))
        })
    }

    fn session_send(&self, request: &SessionSendRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let session_id = request.session_id.clone();
        let text = request.text.clone();
        let client_msg_id = request.client_msg_id.clone();
        self.call(async move {
            let session_id = parse_session_id(&session_id)?;
            let ack = manager
                .send(&session_id, &text, &client_msg_id)
                .await
                .map_err(map_lifecycle_error)?;
            Ok(json!({
                "session_id": ack.session_id.as_str(),
                "message_id": ack.message_id.as_str(),
                "run_id": ack.run_id.as_str(),
                "queued": ack.queued,
                "duplicate": ack.duplicate,
            }))
        })
    }

    fn session_interrupt(&self, request: &SessionIdRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let session_id = request.session_id.clone();
        self.call(async move {
            let session_id = parse_session_id(&session_id)?;
            let report = manager
                .interrupt(&session_id)
                .await
                .map_err(map_lifecycle_error)?;
            Ok(json!({
                "session_id": report.session_id.as_str(),
                "interrupted_run": report.interrupted_run.as_ref().map(|run| run.as_str()),
                "cancelled_waiting_run": report.cancelled_waiting_run.as_ref().map(|run| run.as_str()),
            }))
        })
    }

    fn session_dispose(&self, request: &SessionIdRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let executor = self.executor.clone();
        let session_id = request.session_id.clone();
        self.call(async move {
            let session_id = parse_session_id(&session_id)?;
            let status = manager
                .dispose(&session_id)
                .await
                .map_err(map_lifecycle_error)?;
            if let Some(executor) = executor {
                executor.dispose_session(&session_id).await;
            }
            Ok(json!({
                "session_id": session_id.as_str(),
                "status": status.as_str(),
            }))
        })
    }

    /// M3-06 `run_retry`：一键重放（仅终态 run；重放按 ADR-005 Mode R/N，由执行器
    /// 按 `sessions.config.native_id` 与适配器能力决定）。产生新 run 且旧 run 保留审计。
    fn run_retry(&self, request: &crate::ipc::dto::RunRetryRequest) -> Result<Value, IpcError> {
        let manager = self.manager_required()?.clone();
        let run_id = request.run_id.clone();
        self.call(async move {
            let run_id = aether_core::RunId::new(run_id)
                .map_err(|error| IpcError::internal(format!("run_id 非法：{error}")))?;
            let ack = manager
                .retry_run(&run_id)
                .await
                .map_err(map_lifecycle_error)?;
            Ok(json!({
                "session_id": ack.session_id.as_str(),
                "run_id": ack.run_id.as_str(),
                "input_message_id": ack.input_message_id.as_str(),
                "queued": ack.queued,
            }))
        })
    }

    fn messages_page(&self, request: &MessagesPageRequest) -> Result<Value, IpcError> {
        let reads = self.reads_required()?.clone();
        let session_id = request.session_id.clone();
        let last_seq = request.last_seq;
        let limit = request.limit.unwrap_or(500) as usize;
        let gap_limit = self.gap_limit;
        self.call(async move {
            let session_id = parse_session_id(&session_id)?;
            let max_seq = reads
                .max_seq(&session_id)
                .await
                .map_err(|error| IpcError::internal(format!("events.max(seq) 读取失败：{error}")))?;
            let events = match last_seq {
                Some(last_seq) => {
                    if let Some(max_seq) = max_seq {
                        let gap = max_seq.saturating_sub(last_seq);
                        if gap > gap_limit {
                            return Err(IpcError::new(
                                IpcErrorCode::ReadbackGapTooLarge,
                                format!(
                                    "补读缺口过大（readback_gap_too_large）：{gap} > {gap_limit}，拒绝自动补发；请重开会话"
                                ),
                            ));
                        }
                    }
                    reads
                        .events_page(&session_id, Some(last_seq), limit)
                        .await
                        .map_err(|error| {
                            IpcError::internal(format!("补读分页失败：{error}"))
                        })?
                }
                None => reads
                    .events_latest(&session_id, limit)
                    .await
                    .map_err(|error| IpcError::internal(format!("最近事件分页失败：{error}")))?,
            };
            // 最近一页附带消息历史（工作台基线；`messages` 表尾部 limit 条，升序；
            // 无消息为 `Some([])`；补读热路径不读消息表，字段省略）。
            let messages = if last_seq.is_none() {
                Some(
                    reads
                        .messages_latest(&session_id, limit)
                        .await
                        .map_err(|error| IpcError::internal(format!("消息历史分页失败：{error}")))?,
                )
            } else {
                None
            };
            let complete = match (last_seq, max_seq) {
                (Some(last_seq), Some(max_seq)) => {
                    // `last_seq >= max_seq` 视为已到最新（空 events + complete=true）。
                    last_seq >= max_seq || events.last().map(|event| event.seq) == Some(max_seq)
                }
                (Some(_), None) => true,
                // 最近一页即会话尾部（更早历史属于「基线」，不构成后续缺口）。
                (None, _) => true,
            };
            serde_json::to_value(MessagesPageResponse {
                session_id: session_id.as_str().to_owned(),
                last_seq,
                max_seq,
                events,
                messages,
                complete,
            })
            .map_err(|error| IpcError::internal(format!("消息分页序列化失败：{error}")))
        })
    }
}

fn parse_session_id(value: &str) -> Result<SessionId, IpcError> {
    SessionId::new(value).map_err(|error| IpcError::internal(format!("session_id 非法：{error}")))
}

/// DTO 会话状态 → 核心状态（取值一一对应；DTO 校验已限定枚举）。
fn map_session_status(status: crate::ipc::dto::SessionStatus) -> SessionStatus {
    match status {
        crate::ipc::dto::SessionStatus::Creating => SessionStatus::Creating,
        crate::ipc::dto::SessionStatus::Idle => SessionStatus::Idle,
        crate::ipc::dto::SessionStatus::Running => SessionStatus::Running,
        crate::ipc::dto::SessionStatus::Paused => SessionStatus::Paused,
        crate::ipc::dto::SessionStatus::WaitingPermission => SessionStatus::WaitingPermission,
        crate::ipc::dto::SessionStatus::Completed => SessionStatus::Completed,
        crate::ipc::dto::SessionStatus::Failed => SessionStatus::Failed,
        crate::ipc::dto::SessionStatus::Cancelled => SessionStatus::Cancelled,
    }
}

/// 生命周期错误 → IPC 结构化错误（状态冲突用 `invalid_value`；内部错误用 `internal`）。
///
/// `session_busy` / `storage_backpressure` / `persist_degraded` 的专用 IPC 错误码
/// 未在本任务登记（避免超出 M3-02 范围新增错误码面）；消息保留稳定业务码，
/// 供 UI 展示与后续 M3-06 降级 UX 消费。
pub fn map_lifecycle_error(error: LifecycleError) -> IpcError {
    match error {
        LifecycleError::Internal { .. } => IpcError::internal(error.to_string()),
        LifecycleError::Storage { .. } | LifecycleError::Pipeline { .. } => {
            IpcError::internal(error.to_string())
        }
        LifecycleError::SessionNotFound { .. }
        | LifecycleError::SessionBusy { .. }
        | LifecycleError::SessionClosed { .. }
        | LifecycleError::InvalidTransition { .. }
        | LifecycleError::RunNotFound { .. }
        | LifecycleError::RunNotRetryable { .. }
        | LifecycleError::PersistDegraded { .. }
        | LifecycleError::StorageBackpressure { .. }
        | LifecycleError::AdapterIsolated { .. } => IpcError::invalid_value(error.to_string()),
    }
}

fn now_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn lifecycle_errors_map_to_structured_ipc_codes() {
        let busy = map_lifecycle_error(LifecycleError::SessionBusy {
            session_id: SessionId::new("01J0000000000000000000000S").unwrap(),
        });
        assert_eq!(busy.code, IpcErrorCode::InvalidValue);
        assert!(busy.message.contains("session_busy"));

        let internal = map_lifecycle_error(LifecycleError::Internal {
            reason: "boom".to_owned(),
        });
        assert_eq!(internal.code, IpcErrorCode::Internal);
    }

    #[test]
    fn session_status_mapping_is_total() {
        assert_eq!(
            map_session_status(crate::ipc::dto::SessionStatus::WaitingPermission),
            SessionStatus::WaitingPermission
        );
        assert_eq!(
            map_session_status(crate::ipc::dto::SessionStatus::Cancelled),
            SessionStatus::Cancelled
        );
    }

    #[test]
    fn readback_gap_code_is_stable() {
        assert_eq!(
            IpcErrorCode::ReadbackGapTooLarge.as_str(),
            "readback_gap_too_large"
        );
        assert_eq!(
            IpcErrorCode::ReadbackGapTooLarge.as_str(),
            aether_control::PipelineError::ReadbackGapTooLarge {
                gap: 10_001,
                limit: 10_000
            }
            .code(),
            "与核心管线错误码一致（同码透传）"
        );
    }
}
