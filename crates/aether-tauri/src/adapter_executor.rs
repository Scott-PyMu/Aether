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
//! - 工具调用审计（M6-03/ADR-023 §2.1-1）：`tool.call_completed` / `tool.call_failed`
//!   **终态事件落库后**聚合为一条 `tool.call` 审计行（actor=`agent`，resource=工具名，
//!   result=success/failed；detail 脱敏）；经单写队列写入（[`ToolCallAuditSink`]），
//!   失败计数供诊断（不阻断事件转发）；
//! - 终态投影：`RunOutcome` → [`ExecutorOutcome`]（终态行与事件由生命周期落库）；
//!   中断（取消令牌命中）→ 通知适配器 `session.interrupt` 并返回 `Cancelled`。
//!
//! 边界：本模块不写 run/message 行（生命周期负责）、不直接广播（管线负责）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_adapters::connection::ConnectionState;
use aether_adapters::permission_loop::{
    PermissionGate, PermissionGateFuture, PermissionLoop, PermissionLoopProbe,
    PermissionLoopRequest, PermissionLoopSnapshot,
};
use aether_adapters::session_client::{AdapterSessionClient, RunOutcome};
use aether_adapters::supervisor::Supervisor;
use aether_control::{
    EventPipeline, ExecutorFuture, ExecutorOutcome, RunExecutor, RunRequest, SubmitOutcome,
};
use aether_core::{
    ErrorInfo, EventEnvelope, EventPayload, EventType, RunId, RuntimeId, Session, SessionId,
    TokenUsage,
};
use aether_security::Redactor;
use aether_store::{AuditLogRecord, ReadPool, StoreCommand, WriteQueue};
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
/// 工具调用审计动作（ADR-023 附录 A-1：`tool.call`；resource=工具名）。
pub const AUDIT_ACTION_TOOL_CALL: &str = "tool.call";

/// 工具调用终态审计出口（M6-03/ADR-023 §2.1-1）。
///
/// `tool.call_completed` / `tool.call_failed` 的**终态事件落库后**由执行器调用
/// [`ToolCallAuditSink::audit_terminal`]：构造 `tool.call` 审计行并经单写队列追加
/// （`execute` 在队列满时 await 容量——不丢行；队列关闭/写失败计入
/// [`ToolCallAuditSink::failures`] 诊断计数，不影响事件转发主路径）。
#[derive(Clone)]
pub struct ToolCallAuditSink {
    write: WriteQueue,
    handle: Handle,
    /// 脱敏器（构造失败 = `None`：detail 省略，行本体照写；D10 宁可少写内容）。
    redactor: Option<Arc<Redactor>>,
    records: Arc<AtomicU64>,
    failures: Arc<AtomicU64>,
}

impl ToolCallAuditSink {
    pub fn new(write: WriteQueue, handle: Handle) -> Self {
        let redactor = match Redactor::new() {
            Ok(redactor) => Some(Arc::new(redactor)),
            Err(error) => {
                tracing::warn!(error = %error, "工具调用审计脱敏器初始化失败（detail 将省略）");
                None
            }
        };
        Self {
            write,
            handle,
            redactor,
            records: Arc::new(AtomicU64::new(0)),
            failures: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 已落库的工具调用审计行数（诊断/断言）。
    pub fn records(&self) -> u64 {
        self.records.load(Ordering::Relaxed)
    }

    /// 审计写入失败计数（诊断；ADR-023「失败计入诊断」）。
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// 终态工具事件 → `tool.call` 审计行（非终态类型为 no-op）。
    pub fn audit_terminal(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        envelope: &EventEnvelope,
    ) {
        let (tool_call_id, tool_name, duration_ms, error) = match &envelope.payload {
            EventPayload::ToolCallCompleted(payload) => (
                payload.tool_call_id.as_str().to_owned(),
                payload.tool_name.clone(),
                payload.duration_ms,
                None,
            ),
            EventPayload::ToolCallFailed(payload) => (
                payload.tool_call_id.as_str().to_owned(),
                payload.tool_name.clone(),
                payload.duration_ms,
                Some(payload.error.clone()),
            ),
            _ => return,
        };
        let result = if error.is_some() { "failed" } else { "success" };
        let mut detail = json!({
            "tool_call_id": tool_call_id,
            "run_id": run_id.as_str(),
            "duration_ms": duration_ms,
        });
        if let Some(error) = &error {
            if let Some(object) = detail.as_object_mut() {
                object.insert(
                    "error".to_owned(),
                    json!({
                        "code": error.code,
                        "message": error.message,
                        "recoverable": error.recoverable,
                    }),
                );
            }
        }
        let detail = self.redacted_detail(&detail);
        let record = AuditLogRecord {
            id: ulid::Ulid::new().to_string(),
            session_id: Some(session_id.as_str().to_owned()),
            runtime_id: Some(envelope.runtime_id.as_str().to_owned()),
            actor: "agent".to_owned(),
            action: AUDIT_ACTION_TOOL_CALL.to_owned(),
            resource: Some(tool_name),
            detail,
            result: Some(result.to_owned()),
            ts: envelope.ts,
        };
        self.enqueue(record);
    }

    /// detail 脱敏（D10）：脱敏后仍命中即省略 detail（行本体照写）。
    fn redacted_detail(&self, value: &Value) -> Option<String> {
        let redactor = self.redactor.as_ref()?;
        let redacted = redactor.redact_json(value);
        let line = serde_json::to_string(&redacted).ok()?;
        if redactor.is_clean(&line) {
            Some(line)
        } else {
            tracing::warn!("工具调用审计 detail 脱敏后仍命中密钥模式，已省略（D10）");
            None
        }
    }

    fn enqueue(&self, record: AuditLogRecord) {
        let write = self.write.clone();
        let records = Arc::clone(&self.records);
        let failures = Arc::clone(&self.failures);
        self.handle.spawn(async move {
            match write.execute(StoreCommand::InsertAudit { record }).await {
                Ok(_) => {
                    records.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => {
                    failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(error = %error, "工具调用审计写入失败（M6-03；计入诊断）");
                }
            }
        });
    }
}

/// 会话归属映射网关（M3-08 生产接线修正）：适配器 `permission.request.session_id`
/// 为**适配器会话 id**，须映射为核心 `sessions.id` 后进入权限服务——否则待审批行
/// 外键失败（`permissions.session_id` → `sessions.id`）且 UI 按核心会话过滤不可见。
///
/// 映射命中后替换 `session_id`；未命中（如提前上报）保持原样，由权限服务按
/// 非法会话处理（存储错误进 `gate_failures`，不静默直通）。
struct SessionMappingGate {
    inner: Arc<PermissionServiceGate>,
    forward: Arc<Mutex<ForwardState>>,
}

impl PermissionGate for SessionMappingGate {
    fn decide(&self, mut request: PermissionLoopRequest) -> PermissionGateFuture<'_> {
        if let Some(adapter_session) = request.session_id.as_deref() {
            let mapped = lock(&self.forward)
                .sessions
                .get(adapter_session)
                .map(|session| session.as_str().to_owned());
            if let Some(core_session) = mapped {
                request.session_id = Some(core_session);
            }
        }
        self.inner.decide(request)
    }
}

/// 执行器权限回环聚合（零直通集成断言；M3-08 DoD7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutorPermissionStats {
    pub snapshot: PermissionLoopSnapshot,
    pub zero_passthrough: bool,
}

impl ExecutorPermissionStats {
    pub fn summary(&self) -> String {
        format!(
            "{} aggregate_zero_passthrough={}",
            self.snapshot.summary(),
            self.zero_passthrough
        )
    }
}

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
        // 取出该 run 的缓冲事件（未命中即空集合——非错误路径）；取出即摘除表项
        // （M7-06 修复：`mem::take` 会残留空 Vec 表项，`pending` 永不收缩）。
        self.pending.remove(adapter_run).unwrap_or_default()
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

/// 运行时是否声明 `thinking_depth` 能力（ADR-010：hello/initialize 字符串项，
/// 存在即支持）。连接非 Ready/Degraded 视为未声明（能力门判定时机 = ready 后）。
fn connection_supports_thinking_depth(client: &AdapterSessionClient) -> bool {
    match client.connection().state() {
        ConnectionState::Ready(hello) | ConnectionState::Degraded { hello, .. } => hello
            .runtime
            .capabilities
            .iter()
            .any(|item| item == aether_core::THINKING_DEPTH_CAPABILITY),
        ConnectionState::Connecting | ConnectionState::Disconnected(_) => false,
    }
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
    /// M3-08 DoD7：执行器回环探针集合（每个客户端一个；零直通集成断言来源）。
    permission_probes: Arc<Mutex<Vec<Arc<PermissionLoopProbe>>>>,
    /// runtime_id → 会话客户端（连接断开后重建）。
    clients: Arc<AsyncMutex<HashMap<String, Arc<AdapterSessionClient>>>>,
    /// 核心会话 → (runtime_id, 适配器会话 id)。
    adapter_sessions: Arc<Mutex<HashMap<SessionId, (String, String)>>>,
    /// 事件转发状态（与转发任务共享）。
    forward: Arc<Mutex<ForwardState>>,
    /// 工具调用终态审计出口（M6-03/ADR-023）。
    tool_audit: ToolCallAuditSink,
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
        let tool_audit = ToolCallAuditSink::new(write.clone(), handle.clone());
        Self {
            supervisor,
            pipeline,
            reads,
            write,
            handle,
            permission_gate,
            permission_probes: Arc::new(Mutex::new(Vec::new())),
            clients: Arc::new(AsyncMutex::new(HashMap::new())),
            adapter_sessions: Arc::new(Mutex::new(HashMap::new())),
            forward: Arc::new(Mutex::new(ForwardState::default())),
            tool_audit,
        }
    }

    /// 工具调用审计出口（统计/断言；M6-03）。
    pub fn tool_audit(&self) -> &ToolCallAuditSink {
        &self.tool_audit
    }

    /// 事件缓冲溢出计数（诊断/测试）。
    pub fn dropped_events(&self) -> u64 {
        lock(&self.forward).dropped
    }

    /// 执行器权限回环探针聚合（M3-08 DoD7 零直通集成证据）。
    ///
    /// 多客户端（断连重建）时逐字段求和；`zero_passthrough` 需全部客户端满足。
    pub fn permission_loop_stats(&self) -> ExecutorPermissionStats {
        let probes = lock(&self.permission_probes);
        let mut snapshot = PermissionLoopSnapshot {
            requests_received: 0,
            requests_invalid: 0,
            decisions: 0,
            resolutions_sent: 0,
            resolution_failures: 0,
            gate_failures: 0,
        };
        let mut zero_passthrough = true;
        for probe in probes.iter() {
            let item = probe.snapshot();
            snapshot.requests_received += item.requests_received;
            snapshot.requests_invalid += item.requests_invalid;
            snapshot.decisions += item.decisions;
            snapshot.resolutions_sent += item.resolutions_sent;
            snapshot.resolution_failures += item.resolution_failures;
            snapshot.gate_failures += item.gate_failures;
            zero_passthrough &= item.zero_passthrough();
        }
        ExecutorPermissionStats {
            snapshot,
            zero_passthrough,
        }
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

    /// `session.pause` 下发（M7-03/ADR-017：核心侧受控中断收口后由 `SessionManager`
    /// 经 [`aether_control::PauseNotifier`] 调用；best-effort）。
    ///
    /// 语义：通知适配器取消残余工作、释放运行时侧会话资源；**保留**核心→适配器会话
    /// 映射与 `sessions.config.native_id`（Mode R 重放依赖）。无适配器会话（尚无 run
    /// 或连接重建后未再执行）→ no-op（无残余工作）；失败上抛由调用方记审计/诊断。
    pub async fn pause_session(&self, session_id: &SessionId) -> Result<(), ErrorInfo> {
        let target = lock(&self.adapter_sessions).get(session_id).cloned();
        let Some((runtime_id, adapter_session)) = target else {
            return Ok(());
        };
        let client = self.clients.lock().await.get(&runtime_id).cloned();
        let Some(client) = client else {
            return Ok(());
        };
        client
            .pause(&adapter_session)
            .await
            .map(|_| ())
            .map_err(|error| error_info(ADAPTER_REQUEST_FAILED_CODE, error.to_string()))
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
        let permission_loop = self.permission_gate.clone().map(|gate| {
            // M3-08：适配器会话 id → 核心会话 id 映射（待审批行外键与 UI 过滤一致）。
            let mapping: Arc<dyn PermissionGate> = Arc::new(SessionMappingGate {
                inner: gate,
                forward: Arc::clone(&self.forward),
            });
            let loop_ = PermissionLoop::new(mapping);
            lock(&self.permission_probes).push(loop_.probe());
            loop_
        });
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
        let tool_audit = self.tool_audit.clone();
        self.handle.spawn(async move {
            forward_loop(forward, pipeline, tool_audit, receiver).await;
        });
    }

    /// 确保核心会话在适配器侧存在（Mode R：携带 `native_id` 恢复）。
    ///
    /// `thinking_depth` 为已过能力门的会话级生效值（`None` = 运行时未声明该能力，
    /// 不透传；ADR-010）。
    async fn ensure_adapter_session(
        &self,
        client: &AdapterSessionClient,
        session: &Session,
        thinking_depth: Option<u8>,
    ) -> Result<String, ErrorInfo> {
        if let Some((_, adapter_session)) = lock(&self.adapter_sessions).get(&session.id).cloned() {
            return Ok(adapter_session);
        }
        let native_id = session
            .config
            .get("native_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        // M3-08/D14：工作区记忆注入随 `session.create` 下发（`sessions.system_prompt`
        // 由会话创建路径写入；旧适配器忽略未知字段，ADR-003 兼容）。
        // M3-10/ADR-010：`thinking_depth` 仅在运行时声明能力时透传。
        // M7-06/ADR-024：`permission_mode` 档位随 `session.create` 下发（适配器投影为
        // 运行时启动参数；仅新会话生效——在途会话判定参数不受后续档位变更影响）。
        let created = client
            .create_session_with_options(
                Some(&session.title),
                native_id.as_deref(),
                session.model.as_deref(),
                session.system_prompt.as_deref(),
                thinking_depth,
                Some(session.permission_mode.as_str()),
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
        // M3-10/ADR-010 能力门（判定时机 = runtime ready 后）：运行时在
        // `hello.runtime.capabilities` / `initialize` 响应中以字符串项
        // `thinking_depth` 声明支持。未声明 → 会话级改写为缺省 2、run 落缺省 2、
        // 字段不透传（非阻断；延迟判定路径不产生响应警告）。
        let supported = connection_supports_thinking_depth(&client);
        // 支持：会话级值透传 `session.create`；显式覆盖透传 `session.send`（仅本次 run）。
        // 未支持：字段一律不透传（`sessions`/`runs` 已按缺省 2 落库/改写）。
        let (session_depth, send_override) = if supported {
            (
                Some(session.thinking_depth),
                if request.thinking_override {
                    Some(request.thinking_depth)
                } else {
                    None
                },
            )
        } else {
            (None, None)
        };
        if !supported {
            // 延迟能力门落库（ADR-010：sessions 改写缺省 2、runs 落缺省 2）；
            // 落库失败仅诊断，不阻断 run（能力门非阻断语义）。
            if session.thinking_depth != aether_core::THINKING_DEPTH_DEFAULT {
                let outcome = self
                    .write
                    .execute(StoreCommand::UpdateSessionThinkingDepth {
                        session_id: session.id.clone(),
                        thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
                        updated_at: now_ms(),
                    })
                    .await;
                if let Err(error) = outcome {
                    tracing::warn!(
                        session_id = %session.id,
                        error = %error,
                        "思考深度能力门：sessions.thinking_depth 改写缺省 2 失败（仅诊断）"
                    );
                }
            }
            if request.thinking_depth != aether_core::THINKING_DEPTH_DEFAULT {
                let outcome = self
                    .write
                    .execute(StoreCommand::UpdateRunThinkingDepth {
                        run_id: request.run_id.clone(),
                        thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
                    })
                    .await;
                if let Err(error) = outcome {
                    tracing::warn!(
                        run_id = %request.run_id,
                        error = %error,
                        "思考深度能力门：runs.thinking_depth 改写缺省 2 失败（仅诊断）"
                    );
                }
            }
        }
        let adapter_session = self
            .ensure_adapter_session(&client, &session, session_depth)
            .await?;
        // 适配器侧幂等键（ADR-005「适配器侧对 client_msg_id 同样去重」）取**核心 run id**：
        // 每次派发（含 M3-06 `run_retry` 重放）都是一个新 run，必须产生新的适配器 run；
        // 核心侧的重复 `session.send` 已由存储层 `UNIQUE(session_id, client_msg_id)`
        // 在到达适配器之前去重（ADR-005 持久化支撑），因此以 run id 为键不丢失去重语义，
        // 且避免重放复用输入消息 id 时被适配器幂等命中而返回旧 run。
        let ack = client
            .send_with_references(
                &adapter_session,
                request.run_id.as_str(),
                &request.text,
                send_override,
                &request.references,
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
                &self.tool_audit,
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

/// M7-03/ADR-017：`session.pause` 下发钩子（核心 `SessionManager` → 适配器客户端；
/// best-effort——失败只记审计/诊断，不阻塞核心侧暂停收口）。
impl aether_control::PauseNotifier for AdapterRunExecutor {
    fn notify_pause(
        &self,
        session_id: SessionId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.pause_session(&session_id)
                .await
                .map_err(|error| error.message)
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
    tool_audit: ToolCallAuditSink,
    mut receiver: UnboundedReceiver<EventEnvelope>,
) {
    while let Some(envelope) = receiver.recv().await {
        let mapped = {
            let mut state = lock(&forward);
            state.handle(envelope)
        };
        if let Some((session_id, run_id, envelope)) = mapped {
            submit_mapped(&pipeline, &tool_audit, &session_id, &run_id, envelope).await;
        }
    }
}

/// 归属重写后提交管线（先日志后广播；归一化失败按管线死信计数，不阻断）。
///
/// M6-03/ADR-023：工具调用终态（`tool.call_completed` / `tool.call_failed`）在
/// **落盘成功后**聚合为 `tool.call` 审计行（幂等命中/未落盘不写）。
async fn submit_mapped(
    pipeline: &EventPipeline,
    tool_audit: &ToolCallAuditSink,
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
    match pipeline.submit(value).await {
        Ok(SubmitOutcome::Persisted { .. }) => {
            tool_audit.audit_terminal(session_id, run_id, &envelope);
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                run_id = %run_id,
                error = %error,
                "适配器增量事件提交管线失败（缺口由补读恢复）"
            );
        }
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
