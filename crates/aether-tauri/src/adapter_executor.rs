//! 生产 run 执行器（M3-02）：核心生命周期 → 适配器会话客户端接线。
//!
//! 职责（`docs/M2-02-证据.md` §4 与 `docs/M2-10-证据.md` §4.3 的承接项）：
//! - 按 `RunRequest.runtime_id` 从监督器取得已就绪连接（未就绪时尝试启动一次）；
//! - 会话映射：核心 `SessionId` ↔ 适配器会话 id；`session.create` 使用
//!   `sessions.config.native_id` 恢复（ADR-005 Mode R），创建后把 `native_id`
//!   写回 `sessions.config`（生产侧持久化由本模块收口）；
//! - 事件转发：适配器 `event` 通知中**核心不拥有**的增量类型（`message.delta` /
//!   `tool.call_*`）经会话/run 归属重写后提交 `EventPipeline`（先日志后广播）；
//!   `run.*` / `session.*` / `message.completed` 由核心生命周期统一产出，不转发
//!   （避免重复事件）；
//! - 终态投影：`RunOutcome` → [`ExecutorOutcome`]（终态行与事件由生命周期落库）；
//!   中断（取消令牌命中）→ 通知适配器 `session.interrupt` 并返回 `Cancelled`。
//!
//! 边界：本模块不写 run/message 行（生命周期负责）、不直接广播（管线负责）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_adapters::connection::ConnectionState;
use aether_adapters::permission_loop::PermissionLoop;
use aether_adapters::session_client::{AdapterSessionClient, RunOutcome};
use aether_adapters::supervisor::Supervisor;
use aether_control::{EventPipeline, ExecutorFuture, ExecutorOutcome, RunExecutor, RunRequest};
use aether_core::{
    ErrorInfo, EventEnvelope, EventType, RunId, RuntimeId, Session, SessionId, TokenUsage,
};
use aether_store::{ReadPool, StoreCommand, WriteQueue};
use serde_json::{json, Value};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::sync::Mutex as AsyncMutex;

use crate::permission_loop::PermissionServiceGate;

/// 适配器不可用（未注册 / 未就绪 / 无连接）。
pub const ADAPTER_UNAVAILABLE_CODE: &str = "adapter_unavailable";
/// 适配器请求失败（会话创建/发送/超时/断连）。
pub const ADAPTER_REQUEST_FAILED_CODE: &str = "adapter_request_failed";
/// 执行器兜底超时（核心 120s 断流看门狗优先；此值仅防止执行任务永久挂起）。
pub const RUN_EXECUTOR_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// 未映射 run 事件的本地缓冲上限（超过即丢弃并计数，缺口由核心补读恢复）。
pub const PENDING_EVENT_LIMIT: usize = 1024;

fn error_info(code: &str, message: impl Into<String>) -> ErrorInfo {
    ErrorInfo {
        code: code.to_owned(),
        message: message.into(),
        recoverable: true,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 事件转发状态（会话/run 归属映射 + 未映射缓冲）。
#[derive(Default)]
struct ForwardState {
    /// 适配器会话 id → 核心会话 id。
    sessions: HashMap<String, SessionId>,
    /// 适配器 run id → (核心会话 id, 核心 run id)。
    runs: HashMap<String, (SessionId, RunId)>,
    /// 适配器 run id → 待映射事件（`session.send` ack 前到达的 delta）。
    pending: HashMap<String, Vec<EventEnvelope>>,
    /// 缓冲溢出丢弃计数（诊断）。
    dropped: u64,
}

impl ForwardState {
    fn register_session(&mut self, adapter_session: &str, core_session: &SessionId) {
        self.sessions
            .insert(adapter_session.to_owned(), core_session.clone());
    }

    /// 注册 run 映射并返回该 run 的缓冲事件（按到达顺序）。
    fn register_run(
        &mut self,
        adapter_run: &str,
        core_session: &SessionId,
        core_run: &RunId,
    ) -> Vec<EventEnvelope> {
        self.runs.insert(
            adapter_run.to_owned(),
            (core_session.clone(), core_run.clone()),
        );
        // 取出该 run 的缓冲事件（未命中即空集合——非错误路径）。
        std::mem::take(self.pending.entry(adapter_run.to_owned()).or_default())
    }

    /// 处理一条适配器事件：可映射 → 返回 `(核心会话, 核心 run, 事件)`；否则入缓冲。
    fn handle(&mut self, envelope: EventEnvelope) -> Option<(SessionId, RunId, EventEnvelope)> {
        if !forwardable(envelope.event_type()) {
            return None;
        }
        if !self.sessions.contains_key(envelope.session_id.as_str()) {
            return None;
        }
        let adapter_run = envelope
            .run_id
            .as_ref()
            .map(|run| run.as_str().to_owned())?;
        match self.runs.get(&adapter_run).cloned() {
            Some((session, core_run)) => Some((session, core_run, envelope)),
            None => {
                let buffer = self.pending.entry(adapter_run).or_default();
                if buffer.len() < PENDING_EVENT_LIMIT {
                    buffer.push(envelope);
                } else {
                    self.dropped += 1;
                }
                None
            }
        }
    }

    /// 摘除会话相关映射（dispose / 客户端重建）。
    fn remove_session(&mut self, core_session: &SessionId) {
        self.sessions.retain(|_, session| session != core_session);
        self.runs.retain(|_, (session, _)| session != core_session);
    }
}

/// 转发类型白名单：核心生命周期不产出的增量/工具事件。
fn forwardable(event_type: EventType) -> bool {
    matches!(
        event_type,
        EventType::MessageDelta
            | EventType::ToolCallStarted
            | EventType::ToolCallCompleted
            | EventType::ToolCallFailed
    )
}

/// 生产 run 执行器（克隆共享同一实例）。
#[derive(Clone)]
pub struct AdapterRunExecutor {
    supervisor: Arc<Supervisor>,
    pipeline: EventPipeline,
    reads: ReadPool,
    write: WriteQueue,
    handle: Handle,
    permission_gate: Option<Arc<PermissionServiceGate>>,
    /// runtime_id → 会话客户端（连接断开后重建）。
    clients: Arc<AsyncMutex<HashMap<String, Arc<AdapterSessionClient>>>>,
    /// 核心会话 → (runtime_id, 适配器会话 id)。
    adapter_sessions: Arc<Mutex<HashMap<SessionId, (String, String)>>>,
    /// 事件转发状态（与转发任务共享）。
    forward: Arc<Mutex<ForwardState>>,
}

impl AdapterRunExecutor {
    pub fn new(
        supervisor: Arc<Supervisor>,
        pipeline: EventPipeline,
        reads: ReadPool,
        write: WriteQueue,
        handle: Handle,
        permission_gate: Option<Arc<PermissionServiceGate>>,
    ) -> Self {
        Self {
            supervisor,
            pipeline,
            reads,
            write,
            handle,
            permission_gate,
            clients: Arc::new(AsyncMutex::new(HashMap::new())),
            adapter_sessions: Arc::new(Mutex::new(HashMap::new())),
            forward: Arc::new(Mutex::new(ForwardState::default())),
        }
    }

    /// 事件缓冲溢出计数（诊断/测试）。
    pub fn dropped_events(&self) -> u64 {
        lock(&self.forward).dropped
    }

    /// 关闭适配器会话（`session.dispose` 后由命令层调用；幂等）。
    pub async fn dispose_session(&self, session_id: &SessionId) {
        let target = lock(&self.adapter_sessions).remove(session_id);
        let Some((runtime_id, adapter_session)) = target else {
            return;
        };
        lock(&self.forward).remove_session(session_id);
        let client = self.clients.lock().await.get(&runtime_id).cloned();
        if let Some(client) = client {
            let _ = client.dispose(&adapter_session).await;
        }
    }

    /// 取得/重建 runtime 的会话客户端（确保适配器已就绪）。
    async fn client_for(
        &self,
        runtime_id: &RuntimeId,
    ) -> Result<Arc<AdapterSessionClient>, ErrorInfo> {
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(runtime_id.as_str()) {
            if !matches!(
                client.connection().state(),
                ConnectionState::Disconnected(_)
            ) {
                return Ok(Arc::clone(client));
            }
            // 连接断开：丢弃旧客户端及其适配器会话映射（重建时按 Mode R 以
            // `sessions.config.native_id` 恢复；新客户端不识别旧会话 id）。
            clients.remove(runtime_id.as_str());
            let stale: Vec<SessionId> = lock(&self.adapter_sessions)
                .iter()
                .filter(|(_, (runtime, _))| runtime == runtime_id.as_str())
                .map(|(session_id, _)| session_id.clone())
                .collect();
            for session_id in stale {
                lock(&self.adapter_sessions).remove(&session_id);
                lock(&self.forward).remove_session(&session_id);
            }
        }
        let runtime = self.supervisor.get(runtime_id.as_str()).ok_or_else(|| {
            error_info(
                ADAPTER_UNAVAILABLE_CODE,
                format!("runtime {} 未注册（监督器白名单）", runtime_id.as_str()),
            )
        })?;
        if runtime.status().await != aether_core::RuntimeStatus::Ready {
            let outcome = runtime.start().await;
            if !outcome.is_ready() {
                return Err(error_info(
                    ADAPTER_UNAVAILABLE_CODE,
                    format!("runtime {} 未就绪：{outcome:?}", runtime_id.as_str()),
                ));
            }
        }
        let connection = runtime.connection().await.ok_or_else(|| {
            error_info(
                ADAPTER_UNAVAILABLE_CODE,
                format!("runtime {} 无活动连接", runtime_id.as_str()),
            )
        })?;
        let (sink, receiver) = unbounded_channel();
        let permission_loop = self
            .permission_gate
            .clone()
            .map(|gate| PermissionLoop::new(gate));
        let client = Arc::new(AdapterSessionClient::with_channels(
            connection,
            permission_loop,
            Some(sink),
        ));
        self.spawn_forwarder(receiver);
        clients.insert(runtime_id.as_str().to_owned(), Arc::clone(&client));
        Ok(client)
    }

    /// 启动事件转发任务（每个客户端一个；通道关闭即退出）。
    fn spawn_forwarder(&self, receiver: UnboundedReceiver<EventEnvelope>) {
        let forward = Arc::clone(&self.forward);
        let pipeline = self.pipeline.clone();
        self.handle.spawn(async move {
            forward_loop(forward, pipeline, receiver).await;
        });
    }

    /// 确保核心会话在适配器侧存在（Mode R：携带 `native_id` 恢复）。
    async fn ensure_adapter_session(
        &self,
        client: &AdapterSessionClient,
        session: &Session,
    ) -> Result<String, ErrorInfo> {
        if let Some((_, adapter_session)) = lock(&self.adapter_sessions).get(&session.id).cloned() {
            return Ok(adapter_session);
        }
        let native_id = session
            .config
            .get("native_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let created = client
            .create_session(
                Some(&session.title),
                native_id.as_deref(),
                session.model.as_deref(),
            )
            .await
            .map_err(|error| error_info(ADAPTER_REQUEST_FAILED_CODE, error.to_string()))?;
        if let Some(native_id) = &created.native_id {
            let mut config = session.config.clone();
            match config.as_object_mut() {
                Some(object) => {
                    object.insert("native_id".to_owned(), Value::String(native_id.clone()));
                }
                None => config = json!({ "native_id": native_id }),
            }
            if let Err(error) = self
                .write
                .execute(StoreCommand::UpdateSessionConfig {
                    session_id: session.id.clone(),
                    config,
                    updated_at: now_ms(),
                })
                .await
            {
                tracing::warn!(
                    session_id = %session.id,
                    error = %error,
                    "native_id 写回 sessions.config 失败（Mode R 恢复将退化为新会话）"
                );
            }
        }
        lock(&self.forward).register_session(&created.session_id, &session.id);
        lock(&self.adapter_sessions).insert(
            session.id.clone(),
            (
                session.runtime_id.as_str().to_owned(),
                created.session_id.clone(),
            ),
        );
        Ok(created.session_id)
    }

    async fn execute_inner(&self, request: &RunRequest) -> Result<ExecutorOutcome, ErrorInfo> {
        let session = self
            .reads
            .session(&request.session_id)
            .await
            .map_err(|error| error_info("storage_error", error.to_string()))?
            .ok_or_else(|| {
                error_info(
                    "session_not_found",
                    format!("会话不存在：{}", request.session_id.as_str()),
                )
            })?;
        let client = self.client_for(&request.runtime_id).await?;
        let adapter_session = self.ensure_adapter_session(&client, &session).await?;
        let ack = client
            .send(
                &adapter_session,
                request.input_message_id.as_str(),
                &request.text,
            )
            .await
            .map_err(|error| error_info(ADAPTER_REQUEST_FAILED_CODE, error.to_string()))?;
        let buffered = {
            let mut forward = lock(&self.forward);
            forward.register_run(&ack.run_id, &request.session_id, &request.run_id)
        };
        for envelope in buffered {
            submit_mapped(
                &self.pipeline,
                &request.session_id,
                &request.run_id,
                envelope,
            )
            .await;
        }

        let outcome = tokio::select! {
            outcome = client.wait_run_outcome(&ack.run_id, RUN_EXECUTOR_TIMEOUT) => outcome,
            _ = request.cancel.cancelled() => {
                // 取消路径：通知适配器中断（尽力而为），终态由生命周期落库。
                let _ = client.interrupt(&adapter_session).await;
                return Ok(ExecutorOutcome::Cancelled {
                    reason: Some("user_interrupt".to_owned()),
                });
            }
        };
        let outcome = outcome.ok_or_else(|| {
            error_info(
                "run_timeout",
                format!(
                    "适配器在 {:?} 内未返回终态（执行器兜底超时）",
                    RUN_EXECUTOR_TIMEOUT
                ),
            )
        })?;
        let projected = match outcome {
            RunOutcome::Completed {
                assistant_text,
                usage,
            } => ExecutorOutcome::Completed {
                assistant_text,
                usage: usage.and_then(|value| serde_json::from_value::<TokenUsage>(value).ok()),
            },
            RunOutcome::Failed { error } => ExecutorOutcome::Failed { error },
            RunOutcome::Cancelled { reason } => ExecutorOutcome::Cancelled { reason },
            RunOutcome::Disconnected { detail } => {
                return Err(error_info(
                    aether_adapters::session_client::ADAPTER_DISCONNECTED_CODE,
                    format!("适配器连接断开：{detail}"),
                ))
            }
        };
        Ok(projected)
    }
}

impl RunExecutor for AdapterRunExecutor {
    fn execute(&self, request: RunRequest) -> ExecutorFuture<'_> {
        Box::pin(async move {
            match self.execute_inner(&request).await {
                Ok(outcome) => outcome,
                Err(error) => ExecutorOutcome::Failed { error },
            }
        })
    }
}

/// 未接线执行器（监督器不可用时的兜底；run 一律 `failed(adapter_unavailable)`）。
pub struct UnavailableExecutor;

impl RunExecutor for UnavailableExecutor {
    fn execute(&self, _request: RunRequest) -> ExecutorFuture<'_> {
        Box::pin(async {
            ExecutorOutcome::Failed {
                error: error_info(
                    ADAPTER_UNAVAILABLE_CODE,
                    "适配器未接线（监督器不可用：启动失败或台账初始化失败）",
                ),
            }
        })
    }
}

/// 转发任务主循环：事件到达即映射提交（未映射事件在 `register_run` 时由执行器冲刷）。
async fn forward_loop(
    forward: Arc<Mutex<ForwardState>>,
    pipeline: EventPipeline,
    mut receiver: UnboundedReceiver<EventEnvelope>,
) {
    while let Some(envelope) = receiver.recv().await {
        let mapped = {
            let mut state = lock(&forward);
            state.handle(envelope)
        };
        if let Some((session_id, run_id, envelope)) = mapped {
            submit_mapped(&pipeline, &session_id, &run_id, envelope).await;
        }
    }
}

/// 归属重写后提交管线（先日志后广播；归一化失败按管线死信计数，不阻断）。
async fn submit_mapped(
    pipeline: &EventPipeline,
    session_id: &SessionId,
    run_id: &RunId,
    envelope: EventEnvelope,
) {
    let mut value = match serde_json::to_value(&envelope) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(error = %error, "适配器事件序列化失败（丢弃）");
            return;
        }
    };
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object.insert(
        "session_id".to_owned(),
        Value::String(session_id.as_str().to_owned()),
    );
    object.insert(
        "run_id".to_owned(),
        Value::String(run_id.as_str().to_owned()),
    );
    if let Err(error) = pipeline.submit(value).await {
        tracing::warn!(
            session_id = %session_id,
            run_id = %run_id,
            error = %error,
            "适配器增量事件提交管线失败（缺口由补读恢复）"
        );
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

    use aether_core::{EventId, EventPayload, RuntimeId};

    use super::*;

    fn envelope(session: &str, run: Option<&str>, event_type: EventType) -> EventEnvelope {
        EventEnvelope {
            v: 1,
            id: EventId::new("01J00000000000000000000E01").unwrap(),
            session_id: SessionId::new(session).unwrap(),
            run_id: run.map(|run| RunId::new(run).unwrap()),
            runtime_id: RuntimeId::new("mock").unwrap(),
            seq: 1,
            ts: 1,
            payload: match event_type {
                EventType::MessageDelta => {
                    EventPayload::MessageDelta(aether_core::MessageDeltaPayload {
                        message_id: aether_core::MessageId::new("01J00000000000000000000M01")
                            .unwrap(),
                        text: "hi".to_owned(),
                    })
                }
                _ => EventPayload::Log(aether_core::LogPayload {
                    level: aether_core::LogLevel::Info,
                    message: "x".to_owned(),
                }),
            },
        }
    }

    #[test]
    fn forward_state_buffers_until_run_mapping_and_filters_core_owned_types() {
        let core_session = SessionId::new("01J0000000000000000000000S").unwrap();
        let core_run = RunId::new("01J0000000000000000000000R").unwrap();
        let mut state = ForwardState::default();
        state.register_session("adapter-session", &core_session);

        // run 映射未登记：delta 入缓冲。
        let delta = envelope(
            "adapter-session",
            Some("adapter-run"),
            EventType::MessageDelta,
        );
        assert!(state.handle(delta).is_none());
        assert_eq!(state.pending.get("adapter-run").map(Vec::len), Some(1));

        // 核心自有类型（run.started / log）不转发。
        let log = envelope("adapter-session", Some("adapter-run"), EventType::Log);
        assert!(state.handle(log).is_none());

        // 映射登记：返回缓冲事件。
        let buffered = state.register_run("adapter-run", &core_session, &core_run);
        assert_eq!(buffered.len(), 1);
        assert!(state.pending.is_empty());

        // 映射后事件直接可映射。
        let delta = envelope(
            "adapter-session",
            Some("adapter-run"),
            EventType::MessageDelta,
        );
        let mapped = state.handle(delta).expect("映射后事件应可转发");
        assert_eq!(mapped.0, core_session);
        assert_eq!(mapped.1, core_run);

        // 未知会话：丢弃。
        let foreign = envelope("other", Some("adapter-run"), EventType::MessageDelta);
        assert!(state.handle(foreign).is_none());
    }
}
