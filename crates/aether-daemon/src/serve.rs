//! `aetherd --stdio` 服务循环（ADR-029 D-029-3/D-029-6；M9-03 DoD-3/DoD-6/DoD-7）。
//!
//! 形态：
//! - **stdin**：JSON-RPC 2.0 over JSONL（有界 2MiB 帧读取器）；
//! - **stdout**：只允许 JSONL（响应 + `daemon.status` 通知；**无任何调试输出**）；
//! - **stderr**：运行期日志（Electron main 负责 drain 并转写 `<data_dir>/logs/aetherd.log`；
//!   本进程同时写同一路径的 `LogSink` 文件，保证诊断包日志段可用）；
//! - `initialize` 首个请求，**不等待适配器预热**；预热在后台阻塞线程执行；
//! - `shutdown` 立即回执 `{accepted:true, shutdown_id}`，随后按 D2 关闭序列收口
//!   （预算由 `aether-app::shutdown` 的 T11 上限约束；M9-09 复核退出预算）；
//! - stdin EOF（Electron 崩溃/被强杀）→ 同一关闭序列（反向孤儿保护）。

use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_app::boot;
use aether_app::event_bridge::{
    DeliveryConfig, DeliveryHandle, EventBridge, EventSink, SinkError, DELIVERY_SHUTDOWN_TIMEOUT,
};
use aether_app::ipc::{is_shell_local, IpcError, IpcErrorCode, IpcState, COMMANDS};
use aether_app::logging::{self, LogSink, DEFAULT_RING_CAPACITY};
use aether_app::permission_notice::{
    DispatchPendingCount, PermissionNotice, PermissionNoticeBridge, PermissionNoticeHandle,
    PermissionNoticeSink, NOTICE_SHUTDOWN_TIMEOUT, PERMISSION_PENDING_METHOD,
};
use aether_app::{core_version, version};
use aether_core::EventEnvelope;
use aether_rpc::Notification;
use aether_rpc::{
    code, encode_line, Connection, DaemonPhase, Frame, FrameReader, Handled, RpcFailure, Server,
};
use serde_json::{json, Value};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

use crate::bootstrap::{Bootstrap, BootstrapError, EXIT_INTERNAL};

/// 关闭时等待后台预热让出存储的最长时间（预热未完成即进入关闭序列时）。
pub const WARM_JOIN_BUDGET: Duration = Duration::from_secs(5);
/// 关闭序列结束后等待 tokio 运行时收尾的时间（超出即放弃后台任务）。
pub const RUNTIME_SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);
/// daemon 关闭总预算（ADR-029 §3.6：8.2s；阶段超时继续下一阶段）。
pub const DAEMON_SHUTDOWN_BUDGET: Duration = Duration::from_millis(8_200);

/// 在硬预算内等待同步任务完成；超时返回 `None`（调用方记录并放行退出）。
///
/// 关闭序列不能被单个卡住的阶段拖过 T11：超时后线程继续运行，但进程即将退出，
/// 由 OS 回收（D2「保证能退出优先」）。
pub fn join_with_budget<T, F>(budget: Duration, task: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(task());
    });
    receiver.recv_timeout(budget).ok()
}

/// 测试故障注入：模拟 checkpoint / 写队列卡住（仅 debug 构建；release 为空实现）。
///
/// `AETHER_TEST_SHUTDOWN_STALL=checkpoint|write_queue` 时在关闭编排前阻塞 30s，
/// 用于断言 main 在 9.5s 强杀、T11 ≤10s、无孤儿。**不进入 63 命令/事件/manifest**。
fn apply_test_shutdown_stall() {
    #[cfg(debug_assertions)]
    {
        let Ok(kind) = std::env::var("AETHER_TEST_SHUTDOWN_STALL") else {
            return;
        };
        if kind == "checkpoint" || kind == "write_queue" {
            tracing::warn!(kind = %kind, "测试故障注入：关闭阶段卡住 30s（debug build）");
            std::thread::sleep(Duration::from_secs(30));
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = std::env::var("AETHER_TEST_SHUTDOWN_STALL");
    }
}

/// stdout 传输（唯一 JSONL 出口）。
#[derive(Clone)]
pub struct Transport {
    out: Arc<Mutex<BufWriter<std::io::Stdout>>>,
}

impl Transport {
    pub fn new() -> Self {
        Self {
            out: Arc::new(Mutex::new(BufWriter::new(std::io::stdout()))),
        }
    }

    /// 写一帧（单行 + flush）。
    pub fn send(&self, value: &Value) -> std::io::Result<()> {
        let mut out = match self.out.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        out.write_all(&encode_line(value))?;
        out.flush()
    }
}

impl Default for Transport {
    fn default() -> Self {
        Self::new()
    }
}

/// JSON-RPC 通知方法名（ADR-029 §4.4：`aether.event`）。
pub const EVENT_METHOD: &str = "aether.event";

/// 事件通知帧（`params` = EventEnvelope 9 字段；无 `id`）。
pub fn event_notification(event: &EventEnvelope) -> Value {
    let params = match serde_json::to_value(event) {
        Ok(value) => value,
        Err(error) => {
            // 不可达路径（EventEnvelope 只含可序列化类型）：以占位 payload 保持 9 字段形状。
            tracing::error!(error = %error, "事件信封序列化失败（保持 JSON-RPC 形状）");
            json!({
                "v": aether_core::EVENT_ENVELOPE_VERSION,
                "id": event.id.as_str(),
                "session_id": event.session_id.as_str(),
                "run_id": event.run_id.as_ref().map(aether_core::RunId::as_str),
                "runtime_id": event.runtime_id.as_str(),
                "seq": event.seq,
                "ts": event.ts,
                "type": event.event_type().as_str(),
                "payload": Value::Null,
            })
        }
    };
    Notification::new(EVENT_METHOD, params).to_value()
}

/// 事件桥传输出口：`aether.event` JSON-RPC notification → stdout（只允许 JSONL）。
struct TransportEventSink {
    transport: Transport,
}

impl EventSink for TransportEventSink {
    fn send(&self, event: &EventEnvelope) -> Result<(), SinkError> {
        self.transport
            .send(&event_notification(event))
            .map_err(|error| SinkError::new(format!("stdout 写入失败：{error}")))
    }
}

/// `permission.pending` 通知帧（`params` = `{count, session_id?}`；无 `id`）。
///
/// M9-06 DoD-5：仅刷新提示；决议仍走 `permissions_pending` / `permission_resolve`
/// （ADR-029 §4.4/§16.4），本函数不携带任何决议字段。
pub fn permission_pending_notification(notice: &PermissionNotice) -> Value {
    Notification::new(PERMISSION_PENDING_METHOD, notice.to_params()).to_value()
}

/// 权限通知传输出口：`permission.pending` JSON-RPC notification → stdout。
struct TransportPermissionNoticeSink {
    transport: Transport,
}

impl PermissionNoticeSink for TransportPermissionNoticeSink {
    fn send(&self, notice: &PermissionNotice) -> Result<(), SinkError> {
        self.transport
            .send(&permission_pending_notification(notice))
            .map_err(|error| SinkError::new(format!("stdout 写入失败：{error}")))
    }
}

/// 阶段单元（`DaemonPhase` + `since_ms`）。
struct PhaseCell {
    phase: DaemonPhase,
    since_ms: u64,
}

/// daemon 服务端：实现 [`aether_rpc::Server`]（业务全部委托 `aether-app::dispatch`）。
pub struct DaemonServer {
    state: IpcState,
    data_dir: String,
    phase: Arc<Mutex<PhaseCell>>,
    runtime: Handle,
    log_sink: Option<Arc<LogSink>>,
    transport: Transport,
    warm: Option<JoinHandle<()>>,
    /// 事件桥句柄（预热完成、管线可用后注入；关闭时先停桥再停管线）。
    event_bridge: Arc<Mutex<Option<DeliveryHandle>>>,
    /// 权限待审批通知桥句柄（M9-06；先于事件桥停止）。
    permission_notice: Arc<Mutex<Option<PermissionNoticeHandle>>>,
    shutdown_counter: u64,
}

impl DaemonServer {
    pub fn new(
        state: IpcState,
        data_dir: String,
        runtime: Handle,
        log_sink: Option<Arc<LogSink>>,
        transport: Transport,
        start_phase: DaemonPhase,
    ) -> Self {
        Self {
            state,
            data_dir,
            phase: Arc::new(Mutex::new(PhaseCell {
                phase: start_phase,
                since_ms: now_ms(),
            })),
            runtime,
            log_sink,
            transport,
            warm: None,
            event_bridge: Arc::new(Mutex::new(None)),
            permission_notice: Arc::new(Mutex::new(None)),
            shutdown_counter: 0,
        }
    }

    fn phase_cell(&self) -> std::sync::MutexGuard<'_, PhaseCell> {
        match self.phase.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 切换阶段并推送 `daemon.status` 通知（ADR-029 §4.4）。
    fn set_phase(&self, phase: DaemonPhase, reason: Option<&str>) {
        let since_ms = {
            let mut cell = self.phase_cell();
            if cell.phase == phase {
                return;
            }
            cell.phase = phase;
            cell.since_ms = now_ms();
            cell.since_ms
        };
        // 通知失败不改变语义（stdout 断开时由 stdin EOF/退出路径收口）。
        let mut params = json!({"phase": phase.as_str(), "since_ms": since_ms});
        if let Some(reason) = reason {
            if let Some(object) = params.as_object_mut() {
                object.insert("reason".to_owned(), Value::from(reason));
            }
        }
        let _ = self
            .transport
            .send(&aether_rpc::Notification::new("daemon.status", params).to_value());
    }

    /// 进入关闭序列：先停通知桥/事件桥（避免关闭后继续写 stdout），再停预热等待。
    fn begin_shutdown(&mut self, budget: Duration) {
        let started = Instant::now();
        self.set_phase(DaemonPhase::ShuttingDown, Some("shutdown"));
        // M9-06：先停权限通知桥（仅提示通道；先于事件桥/管线）。
        let notice = match self.permission_notice.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(mut notice) = notice {
            let metrics = notice.metrics();
            let step_budget = budget
                .saturating_sub(started.elapsed())
                .min(NOTICE_SHUTDOWN_TIMEOUT);
            let stopped = self.runtime.block_on(notice.shutdown(step_budget));
            tracing::info!(
                stopped,
                sent = metrics.sent,
                count_failures = metrics.count_failures,
                sink_errors = metrics.sink_errors,
                lagged = metrics.lagged_events,
                "权限通知桥已停止（仅提示通道；不改变决议路径）"
            );
        }
        let bridge = match self.event_bridge.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(mut bridge) = bridge {
            let metrics = bridge.metrics();
            let step_budget = budget
                .saturating_sub(started.elapsed())
                .min(DELIVERY_SHUTDOWN_TIMEOUT);
            let stopped = self.runtime.block_on(bridge.shutdown(step_budget));
            tracing::info!(
                stopped,
                forwarded = metrics.forwarded,
                dropped_delta = metrics.dropped_delta,
                lagged = metrics.lagged_events,
                readback_events = metrics.readback_events,
                readback_failures = metrics.readback_failures,
                connection_rebuilds = metrics.connection_rebuilds,
                "事件桥已停止（先于管线关闭；daemon 未重启）"
            );
        }
        if let Some(warm) = self.warm.take() {
            let handle = self.runtime.clone();
            let step_budget = budget
                .saturating_sub(started.elapsed())
                .min(WARM_JOIN_BUDGET);
            let joined = handle.block_on(async {
                tokio::time::timeout(step_budget, warm)
                    .await
                    .map(|result| result.is_ok())
                    .unwrap_or(false)
            });
            if !joined {
                tracing::warn!(
                    budget_ms = step_budget.as_millis(),
                    "预热未在预算内结束：继续关闭序列（退出优先）"
                );
            }
        }
    }

    /// 执行 D2 关闭序列（存储五步 + 适配器终止段；未接线则直接通过）。
    fn shutdown_backend(&self, budget: Duration) {
        apply_test_shutdown_stall();
        let Some(orchestrator) = self.state.shutdown_orchestrator() else {
            tracing::info!("关闭序列：无核心编排对象（启动未完成或降级启动），直接退出");
            return;
        };
        if budget.is_zero() {
            tracing::warn!("关闭序列预算已耗尽：跳过核心编排（退出优先）");
            return;
        }
        match join_with_budget(budget, move || orchestrator.run_blocking()) {
            Some(report) => {
                tracing::info!(
                    total_ms = report.total_ms,
                    within_budget = report.within_budget,
                    deadline_expired = report.deadline_expired,
                    pipeline_shutdown_ok = report.pipeline_shutdown_ok,
                    adapters = report.adapters.len(),
                    storage_closed = report.storage.is_some(),
                    wal_bytes_after = report
                        .storage
                        .as_ref()
                        .map_or(0, |storage| storage.wal_bytes_after),
                    "关闭序列完成（D2 五步 + T11 预算）"
                );
            }
            None => {
                tracing::warn!(
                    budget_ms = budget.as_millis(),
                    "关闭序列未在 8.2s 预算内完成：记录并放行进程退出（退出优先）"
                );
            }
        }
    }

    /// 预热：`initialize` 之后在阻塞线程执行（不阻塞 stdin 读取/后续请求校验）。
    fn spawn_warm(&mut self) {
        if self.warm.is_some() {
            return;
        }
        let gate = match self.state.startup_arc() {
            Some(gate) => gate,
            None => {
                tracing::warn!("启动门不可用：跳过核心预热（业务命令回 core_not_ready）");
                return;
            }
        };
        let state = self.state.clone();
        let phase = Arc::clone(&self.phase);
        let transport = self.transport.clone();
        let log_sink = self.log_sink.clone();
        let handle = self.runtime.clone();
        let bridge_slot = Arc::clone(&self.event_bridge);
        let notice_slot = Arc::clone(&self.permission_notice);
        let resource_dir = resource_dir_from_exe();
        self.warm = Some(self.runtime.spawn(async move {
            let blocker = handle.clone();
            let bridge_runtime = handle.clone();
            let bridge_transport = transport.clone();
            let notice_transport = transport.clone();
            let notice_state = state.clone();
            let joined = tokio::task::spawn_blocking(move || {
                let bundle = boot::build_backend(&gate, resource_dir, log_sink, &blocker);
                let keychain_reauth = bundle
                    .keychain_preflight
                    .as_ref()
                    .is_some_and(|report| report.needs_reauth());
                let _ = state.install_backend(bundle.backend);
                if let Some(orchestrator) = bundle.shutdown {
                    let _ = state.install_shutdown(orchestrator);
                }
                let degraded = degraded_snapshot(&state);
                (bundle.pipeline, bundle.write, degraded, keychain_reauth)
            })
            .await;
            let (pipeline, write, degraded, keychain_reauth) = match joined {
                Ok(parts) => parts,
                Err(error) => (
                    None,
                    None,
                    Some(format!("预热任务失败：{error}")),
                    false,
                ),
            };
            let (next, reason) = match degraded {
                Some(reason) => (DaemonPhase::Degraded, Some(reason)),
                None => (DaemonPhase::Ready, None),
            };
            // M9-05/M9-06：管线可用后接线事件桥与权限通知桥（均只订阅落盘后广播）。
            let mut bridge = None;
            let mut notice = None;
            if let Some(pipeline) = pipeline {
                let mut event_bridge = EventBridge::new(
                    pipeline.clone(),
                    Arc::new(TransportEventSink {
                        transport: bridge_transport,
                    }),
                )
                .with_config(DeliveryConfig::default());
                if let Some(write) = write {
                    event_bridge = event_bridge.with_audit_write(write);
                }
                match event_bridge.spawn(&bridge_runtime) {
                    Ok(handle) => bridge = Some(handle),
                    Err(error) => {
                        tracing::error!(error = %error, "事件桥启动失败（业务命令仍可用；事件不投递）");
                    }
                }
                notice = Some(
                    PermissionNoticeBridge::new(
                        pipeline,
                        Arc::new(DispatchPendingCount::new(notice_state)),
                        Arc::new(TransportPermissionNoticeSink {
                            transport: notice_transport,
                        }),
                    )
                    .spawn(&bridge_runtime),
                );
            }
            let shutting_down = {
                let mut cell = match phase.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if cell.phase == DaemonPhase::ShuttingDown {
                    true
                } else {
                    cell.phase = next;
                    cell.since_ms = now_ms();
                    false
                }
            };
            if shutting_down {
                // 关闭已开始：不安装任何投递句柄，直接收口。
                if let Some(mut handle) = notice.take() {
                    let _ = handle.shutdown(NOTICE_SHUTDOWN_TIMEOUT).await;
                }
                if let Some(mut handle) = bridge.take() {
                    let _ = handle.shutdown(DELIVERY_SHUTDOWN_TIMEOUT).await;
                }
            } else {
                if let Some(handle) = notice.take() {
                    let mut slot = match notice_slot.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    *slot = Some(handle);
                }
                if let Some(handle) = bridge.take() {
                    let mut slot = match bridge_slot.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    *slot = Some(handle);
                }
                // 关闭可能与预热完成并发：安装后复核，避免关闭后继续投递。
                let shutting_down_now = match phase.lock() {
                    Ok(guard) => guard.phase == DaemonPhase::ShuttingDown,
                    Err(poisoned) => poisoned.into_inner().phase == DaemonPhase::ShuttingDown,
                };
                if shutting_down_now {
                    let notice = match notice_slot.lock() {
                        Ok(mut guard) => guard.take(),
                        Err(poisoned) => poisoned.into_inner().take(),
                    };
                    if let Some(mut handle) = notice {
                        let _ = handle.shutdown(NOTICE_SHUTDOWN_TIMEOUT).await;
                    }
                    let bridge = match bridge_slot.lock() {
                        Ok(mut guard) => guard.take(),
                        Err(poisoned) => poisoned.into_inner().take(),
                    };
                    if let Some(mut handle) = bridge {
                        let _ = handle.shutdown(DELIVERY_SHUTDOWN_TIMEOUT).await;
                    }
                }
            }
            let since_ms = {
                let cell = match phase.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                cell.since_ms
            };
            // 关闭已开始则不得回退阶段（shutting_down 优先于预热完成）。
            let status_phase = if shutting_down {
                DaemonPhase::ShuttingDown
            } else {
                next
            };
            let mut params = json!({"phase": status_phase.as_str(), "since_ms": since_ms});
            if let Some(reason) = &reason {
                if let Some(object) = params.as_object_mut() {
                    object.insert("reason".to_owned(), Value::from(reason.clone()));
                }
            }
            let _ =
                transport.send(&aether_rpc::Notification::new("daemon.status", params).to_value());
            if keychain_reauth {
                // M9-09 DoD-8：S-13 触发（只读 preflight 发现旧条目不可读；不删除）。
                let notice = json!({
                    "phase": status_phase.as_str(),
                    "since_ms": now_ms(),
                    "reason": "keychain_reauth_required"
                });
                let _ = transport.send(
                    &aether_rpc::Notification::new("daemon.status", notice).to_value(),
                );
                tracing::warn!("Keychain preflight 需要重授权/口令降级（S-13；旧条目不删除）");
            }
            tracing::info!(
                phase = status_phase.as_str(),
                reason = ?reason,
                "预热阶段结束"
            );
        }));
    }
}

impl Server for DaemonServer {
    fn phase(&self) -> DaemonPhase {
        self.phase_cell().phase
    }

    fn data_dir(&self) -> String {
        self.data_dir.clone()
    }

    fn daemon_version(&self) -> String {
        version().to_owned()
    }

    fn core_version(&self) -> String {
        core_version().to_owned()
    }

    fn initialize_accepted(&mut self) {
        // 只更新阶段（`initialize` 响应回显 `warming`）；**不推送**通知，避免
        // `daemon.status` 先于 initialize 响应到达客户端。预热完成后再推送。
        {
            let mut cell = self.phase_cell();
            cell.phase = DaemonPhase::Warming;
            cell.since_ms = now_ms();
        }
        self.spawn_warm();
    }

    fn shutdown_id(&mut self) -> String {
        self.shutdown_counter = self.shutdown_counter.saturating_add(1);
        format!("shutdown-{}-{}", now_ms(), self.shutdown_counter)
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcFailure> {
        // ADR-029 §4.6：4 条壳本地命令由 Electron main 处理，**不进入** daemon。
        if is_shell_local(method) {
            return Err(RpcFailure::method_not_found(method).with_reason("shell_local"));
        }
        if !COMMANDS.contains(&method) {
            return Err(RpcFailure::method_not_found(method));
        }
        self.state.dispatch(method, params).map_err(map_ipc_error)
    }
}

/// `IpcError` → JSON-RPC 失败（传输码 + `aether_code` + `business_code`）。
///
/// `data.aether_code` 只取 32 枚举；业务码走 `data.business_code`（ADR-029 §4.3/§16.2；
/// M9-04 逐项可达）。省略 `business_code` 时形状与 M9-01 基线一致。
pub fn map_ipc_error(error: IpcError) -> RpcFailure {
    let transport = transport_code_for(error.code);
    let mut failure = RpcFailure::transport(transport, error.code.as_str(), error.message.clone());
    if let Some(field) = &error.field {
        failure = failure.with_detail("field", field.clone());
    }
    if let Some(business) = error.business_code() {
        failure = failure.with_business(business.as_str());
    }
    failure
}

/// `IpcErrorCode` → JSON-RPC 传输/类别码（ADR-029 §4.3/§16.2）。
///
/// - 参数校验家族 → `-32602 invalid_params`；
/// - 未实现/未知方法 → `-32601 method_not_found`；
/// - 核心未就绪 → `-32002 core_not_ready`；
/// - 其余域错误 → `-32000 application_error`（读 `data.aether_code`）。
pub fn transport_code_for(code: IpcErrorCode) -> i32 {
    match code {
        IpcErrorCode::InvalidJson
        | IpcErrorCode::UnknownField
        | IpcErrorCode::MissingField
        | IpcErrorCode::InvalidType
        | IpcErrorCode::InvalidValue
        | IpcErrorCode::InvalidEnum
        | IpcErrorCode::TooLarge
        | IpcErrorCode::OutOfRange
        | IpcErrorCode::InvalidFormat
        | IpcErrorCode::PathRejected
        | IpcErrorCode::ArtifactPathRejected => code::INVALID_PARAMS,
        IpcErrorCode::NotImplemented => code::METHOD_NOT_FOUND,
        IpcErrorCode::CoreNotReady => code::CORE_NOT_READY,
        IpcErrorCode::StartupBlocked
        | IpcErrorCode::MigrationFailed
        | IpcErrorCode::Internal
        | IpcErrorCode::ReadbackGapTooLarge
        | IpcErrorCode::BuiltinProviderUndeletable
        | IpcErrorCode::ProviderNotFound
        | IpcErrorCode::ProviderModelNotFound
        | IpcErrorCode::RuleNotFound
        | IpcErrorCode::RuntimeBusy
        | IpcErrorCode::NoCandidateRuntime
        | IpcErrorCode::PauseUnsupported
        | IpcErrorCode::ProviderUnreachable
        | IpcErrorCode::ProviderAuthFailed
        | IpcErrorCode::ProviderError
        | IpcErrorCode::PresetNotFound
        | IpcErrorCode::BuiltinPresetUndeletable
        | IpcErrorCode::WorkspaceNotFound
        | IpcErrorCode::WorkspaceInUse
        | IpcErrorCode::SessionNotFound => code::APPLICATION_ERROR,
    }
}

/// 预热后的降级判定：`health.storage_state=persist_degraded`（D3 安全模式）。
///
/// 返回 `Some(reason)` 表示安全模式/降级（附原因），`None` 表示就绪。
fn degraded_snapshot(state: &IpcState) -> Option<String> {
    let health = state.dispatch("health", Value::Null).ok()?;
    let text = serde_json::to_string(&health).unwrap_or_default();
    if !text.contains("persist_degraded") {
        return None;
    }
    let reason = health
        .get("storage")
        .and_then(|storage| storage.get("detail"))
        .and_then(Value::as_str)
        .or_else(|| health.get("detail").and_then(Value::as_str))
        .unwrap_or("quick_check 失败：只读安全模式（persist_degraded）");
    Some(reason.to_owned())
}

/// 资源目录（`runtime-bundle/` 的父目录）。
///
/// 打包布局（ADR-029 §5/M9-11）把 `aetherd` 放在 `bin/`、`runtime-bundle/` 放在
/// resources 根，因此依次探测：
/// 1. `<exe_dir>/runtime-bundle/runtimes.json`（开发构建：`target/debug/runtime-bundle`）；
/// 2. `<exe_dir>/../runtime-bundle/runtimes.json`（安装产物：`resources/bin/aetherd` +
///    `resources/runtime-bundle`）；
/// 3. 回退 `<exe_dir>`（无清单 → 空注册表，不阻塞启动）。
///
/// `AETHER_RUNTIME_REGISTRY_DIR` 优先（测试/E2E 隔离）。
fn resource_dir_from_exe() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var(aether_app::runtime_registry::REGISTRY_DIR_ENV) {
        if !dir.is_empty() {
            return Some(std::path::PathBuf::from(dir));
        }
    }
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?.to_path_buf();
    let has_manifest = |root: &Path| {
        root.join(aether_app::runtime_registry::REGISTRY_DIR_NAME)
            .join(aether_app::runtime_registry::REGISTRY_FILE_NAME)
            .is_file()
    };
    if has_manifest(&exe_dir) {
        return Some(exe_dir);
    }
    if let Some(parent) = exe_dir.parent() {
        if has_manifest(parent) {
            return Some(parent.to_path_buf());
        }
    }
    Some(exe_dir)
}

/// 运行期日志：`LogSink`（`<data_dir>/logs/aetherd.log` + 诊断包环形缓冲）+ stderr 双写。
fn init_logging(log_path: &Path) -> Option<Arc<LogSink>> {
    let sink = match LogSink::with_file(DEFAULT_RING_CAPACITY, log_path) {
        Ok(sink) => sink,
        Err(error) => {
            eprintln!(
                "[aetherd] 日志文件不可写（{error}）：退化为纯环形缓冲（{}）",
                log_path.display()
            );
            LogSink::new(DEFAULT_RING_CAPACITY)
        }
    };
    let _ = logging::register_sink(Arc::clone(&sink));
    let sink_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(logging::LogWriter::new(Arc::clone(&sink)))
        .with_filter(tracing_subscriber::filter::LevelFilter::INFO);
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_filter(tracing_subscriber::filter::LevelFilter::INFO);
    if tracing_subscriber::registry()
        .with(sink_layer)
        .with(stderr_layer)
        .try_init()
        .is_err()
    {
        eprintln!("[aetherd] 日志订阅器已安装（跳过重复初始化）");
    }
    Some(sink)
}

/// `--stdio` 服务主循环（返回进程退出码）。
pub fn run_stdio(bootstrap: Bootstrap) -> u8 {
    let log_sink = init_logging(&bootstrap.log_path);
    tracing::info!(
        version = version(),
        core = core_version(),
        data_dir = %bootstrap.data_dir.display(),
        lock = %bootstrap.lock_path().display(),
        log = %bootstrap.log_path.display(),
        schema_version = bootstrap.preflight.schema_version,
        "aetherd 启动（stdio；预检通过）"
    );

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("aetherd")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(error = %error, "tokio 运行时创建失败");
            return EXIT_INTERNAL;
        }
    };

    let state = IpcState::with_startup_deferred(Vec::new(), Arc::clone(&bootstrap.gate));
    let transport = Transport::new();
    let mut server = DaemonServer::new(
        state,
        bootstrap.data_dir.to_string_lossy().into_owned(),
        runtime.handle().clone(),
        log_sink,
        transport.clone(),
        DaemonPhase::Starting,
    );

    let mut connection = Connection::new();
    let stdin = std::io::stdin();
    let mut frames = FrameReader::new(stdin.lock());
    let mut frames_seen: u64 = 0;
    loop {
        match frames.read_frame() {
            Ok(Some(Frame::Line(raw))) => {
                frames_seen = frames_seen.saturating_add(1);
                match connection.handle(&raw, &mut server) {
                    Handled::Reply(value) => {
                        let _ = transport.send(&value);
                    }
                    Handled::ReplyAndClose(value) => {
                        let _ = transport.send(&value);
                        break;
                    }
                }
            }
            Ok(Some(Frame::Oversized { limit, discarded })) => {
                frames_seen = frames_seen.saturating_add(1);
                tracing::warn!(limit, discarded, "帧超限：只失败本请求（连接与进程保持）");
                let Handled::Reply(value) = connection.oversized(limit, discarded) else {
                    continue;
                };
                let _ = transport.send(&value);
            }
            Ok(None) => {
                tracing::info!("stdin EOF：进入关闭序列（反向孤儿保护）");
                break;
            }
            Err(error) => {
                tracing::error!(error = %error, "读取 stdin 失败：进入关闭序列");
                break;
            }
        }
    }

    let shutdown_started = Instant::now();
    server.begin_shutdown(DAEMON_SHUTDOWN_BUDGET);
    let remaining = DAEMON_SHUTDOWN_BUDGET.saturating_sub(shutdown_started.elapsed());
    server.shutdown_backend(remaining);
    drop(connection);
    let oversized_frames = frames.oversized_frames();
    drop(frames);
    let remaining = DAEMON_SHUTDOWN_BUDGET.saturating_sub(shutdown_started.elapsed());
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_BUDGET.min(remaining));
    let shutdown_ms = u64::try_from(shutdown_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        frames = frames_seen,
        oversized = oversized_frames,
        shutdown_ms,
        budget_ms = DAEMON_SHUTDOWN_BUDGET.as_millis(),
        within_budget = shutdown_started.elapsed() <= DAEMON_SHUTDOWN_BUDGET,
        "aetherd 退出"
    );
    drop(bootstrap);
    crate::bootstrap::EXIT_OK
}

/// 预 `initialize` 致命失败：写唯一首帧 `id:null` error 后返回退出码。
pub fn bootstrap_failure_exit(error: &BootstrapError, log_path: &Path) -> u8 {
    let message = error.message().to_owned();
    let frame = aether_rpc::initialize::bootstrap_failure(
        error.aether_code(),
        error.business_code().map(|code| code.as_str()),
        message,
        DaemonPhase::Failed,
        &log_path.to_string_lossy(),
    );
    let transport = Transport::new();
    let _ = transport.send(&aether_rpc::Response::error(Value::Null, frame).to_value());
    error.exit_code()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_budget_is_frozen_at_8_2s() {
        assert_eq!(DAEMON_SHUTDOWN_BUDGET, Duration::from_millis(8_200));
        assert!(DAEMON_SHUTDOWN_BUDGET < Duration::from_secs(10), "必须小于 T11 10s");
    }

    #[test]
    fn join_with_budget_returns_value_within_budget() {
        assert_eq!(join_with_budget(Duration::from_millis(500), || 7_u8), Some(7));
    }

    #[test]
    fn join_with_budget_returns_none_when_stuck() {
        let started = Instant::now();
        let result = join_with_budget(Duration::from_millis(30), || {
            std::thread::sleep(Duration::from_millis(250));
            7_u8
        });
        assert_eq!(result, None);
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "预算必须硬约束（实测 {:?}）",
            started.elapsed()
        );
    }

    #[test]
    fn transport_mapping_covers_all_32_codes() {
        assert_eq!(
            transport_code_for(IpcErrorCode::UnknownField),
            code::INVALID_PARAMS
        );
        assert_eq!(
            transport_code_for(IpcErrorCode::InvalidJson),
            code::INVALID_PARAMS
        );
        assert_eq!(
            transport_code_for(IpcErrorCode::NotImplemented),
            code::METHOD_NOT_FOUND
        );
        assert_eq!(
            transport_code_for(IpcErrorCode::CoreNotReady),
            code::CORE_NOT_READY
        );
        assert_eq!(
            transport_code_for(IpcErrorCode::SessionNotFound),
            code::APPLICATION_ERROR
        );
        assert_eq!(
            transport_code_for(IpcErrorCode::RuntimeBusy),
            code::APPLICATION_ERROR
        );
        assert_eq!(
            transport_code_for(IpcErrorCode::StartupBlocked),
            code::APPLICATION_ERROR
        );
    }

    #[test]
    fn ipc_error_maps_to_structured_rpc_failure() {
        let failure = map_ipc_error(IpcError::unknown_field("payload"));
        assert_eq!(failure.transport_code(), code::INVALID_PARAMS);
        assert_eq!(failure.aether_code(), "unknown_field");
        assert_eq!(failure.details(), Some(&json!({"field": "payload"})));
    }

    /// M9-04 DoD-2：`IpcErrorCode` 32 枚举逐项映射，`aether_code` 均落在 32 集合内。
    #[test]
    fn all_32_ipc_codes_map_to_transport_and_aether_code() {
        assert_eq!(IpcErrorCode::ALL.len(), 32);
        for ipc_code in IpcErrorCode::ALL {
            let failure = map_ipc_error(IpcError::new(ipc_code, "injected"));
            assert_eq!(failure.aether_code(), ipc_code.as_str());
            assert!(
                [
                    code::INVALID_PARAMS,
                    code::METHOD_NOT_FOUND,
                    code::CORE_NOT_READY,
                    code::APPLICATION_ERROR,
                ]
                .contains(&failure.transport_code()),
                "{} 的传输码未分类：{}",
                ipc_code.as_str(),
                failure.transport_code()
            );
            assert!(
                failure.business_code().is_none(),
                "未附加业务码时不得输出 business_code"
            );
        }
    }

    /// M9-04 DoD-2：9 业务码逐项经 `error.data.business_code` 承载（不占 32 枚举）。
    #[test]
    fn all_9_business_codes_are_carried_in_error_data() {
        use aether_app::ipc::BusinessCode;
        assert_eq!(BusinessCode::ALL.len(), 9);
        for business in BusinessCode::ALL {
            let failure =
                map_ipc_error(IpcError::invalid_value("injected").with_business_code(business));
            assert_eq!(failure.business_code(), Some(business.as_str()));
            // 业务码不改变既有 `invalid_value` 的传输分类（参数校验家族 -32602）。
            assert_eq!(failure.transport_code(), code::INVALID_PARAMS);
            assert_eq!(failure.aether_code(), "invalid_value");
        }
    }

    #[test]
    fn shell_local_commands_are_rejected_by_the_daemon_router() {
        for method in aether_app::SHELL_COMMANDS {
            assert!(is_shell_local(method));
        }
        assert!(!is_shell_local("health"));
    }

    /// M9-05 DoD-1：`aether.event` 通知无 `id`，`params` 恰好 9 字段（附录 B 语义不变）。
    #[test]
    fn event_notification_is_aether_event_with_nine_fields() {
        use aether_core::{
            EventEnvelope, EventId, EventPayload, RunId, RuntimeId, SessionId,
            SessionUpdatedPayload, ENVELOPE_FIELDS,
        };
        let session = SessionId::new("01J0000000000000000000000A").expect("session");
        let event = EventEnvelope {
            v: aether_core::EVENT_ENVELOPE_VERSION,
            id: EventId::new("01J00000000000000000000001").expect("id"),
            session_id: session.clone(),
            run_id: Some(RunId::new("01J000000000000000000000R1").expect("run")),
            runtime_id: RuntimeId::new("mock").expect("runtime"),
            seq: 7,
            ts: 1_700_000_000_000,
            payload: EventPayload::SessionUpdated(SessionUpdatedPayload {
                session_id: session,
                changed_fields: vec!["title".to_owned()],
                title: Some("M9-05".to_owned()),
            }),
        };
        let frame = event_notification(&event);
        assert_eq!(frame["jsonrpc"], json!("2.0"));
        assert_eq!(frame["method"], json!(EVENT_METHOD));
        assert!(frame.get("id").is_none(), "通知不得携带 id");
        let params = frame["params"].as_object().expect("params 对象");
        assert_eq!(params.len(), 9, "EventEnvelope 必须恰好 9 字段");
        for field in ENVELOPE_FIELDS {
            assert!(params.contains_key(field), "缺少信封字段 {field}");
        }
        assert_eq!(params["seq"], json!(7));
        assert_eq!(params["type"], json!("session.updated"));
    }

    /// M9-06 DoD-5：`permission.pending` 通知无 `id`，`params` 只有 `count`/`session_id`，
    /// 不携带任何决议字段（决议仍只走 `permission_resolve`）。
    #[test]
    fn permission_pending_notification_is_a_hint_only() {
        let notice = PermissionNotice::new(2, Some("01J00000000000000000000S06".to_owned()));
        let frame = permission_pending_notification(&notice);
        assert_eq!(frame["jsonrpc"], json!("2.0"));
        assert_eq!(frame["method"], json!("permission.pending"));
        assert!(frame.get("id").is_none(), "通知不得携带 id");
        let params = frame["params"].as_object().expect("params 对象");
        assert_eq!(params.len(), 2, "只允许 count + session_id：{params:?}");
        assert_eq!(params["count"], json!(2));
        assert_eq!(params["session_id"], json!("01J00000000000000000000S06"));
        for forbidden in ["request_id", "decision", "scope", "ticket_id"] {
            assert!(
                !params.contains_key(forbidden),
                "通知不得携带决议字段 {forbidden}"
            );
        }

        let global = permission_pending_notification(&PermissionNotice::new(0, None));
        let params = global["params"].as_object().expect("params 对象");
        assert_eq!(params.len(), 1, "无会话时只有 count：{params:?}");
        assert_eq!(params["count"], json!(0));
    }
}
