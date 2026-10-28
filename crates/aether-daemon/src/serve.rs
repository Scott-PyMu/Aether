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
use std::time::Duration;

use aether_app::boot;
use aether_app::ipc::{is_shell_local, IpcError, IpcErrorCode, IpcState, COMMANDS};
use aether_app::logging::{self, LogSink, DEFAULT_RING_CAPACITY};
use aether_app::{core_version, version};
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

    /// 进入关闭序列：停止预热等待、阶段置 `shutting_down`。
    fn begin_shutdown(&mut self) {
        self.set_phase(DaemonPhase::ShuttingDown, Some("shutdown"));
        if let Some(warm) = self.warm.take() {
            let handle = self.runtime.clone();
            let joined = handle.block_on(async {
                tokio::time::timeout(WARM_JOIN_BUDGET, warm)
                    .await
                    .map(|result| result.is_ok())
                    .unwrap_or(false)
            });
            if !joined {
                tracing::warn!(
                    budget_ms = WARM_JOIN_BUDGET.as_millis(),
                    "预热未在预算内结束：继续关闭序列（退出优先）"
                );
            }
        }
    }

    /// 执行 D2 关闭序列（存储五步 + 适配器终止段；未接线则直接通过）。
    fn shutdown_backend(&self) {
        if let Some(orchestrator) = self.state.shutdown_orchestrator() {
            let report = orchestrator.run_blocking();
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
        } else {
            tracing::info!("关闭序列：无核心编排对象（启动未完成或降级启动），直接退出");
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
        let resource_dir = resource_dir_from_exe();
        self.warm = Some(self.runtime.spawn(async move {
            let blocker = handle.clone();
            let joined = tokio::task::spawn_blocking(move || {
                let bundle = boot::build_backend(&gate, resource_dir, log_sink, &blocker);
                let _ = state.install_backend(bundle.backend);
                if let Some(orchestrator) = bundle.shutdown {
                    let _ = state.install_shutdown(orchestrator);
                }
                degraded_snapshot(&state)
            })
            .await;
            let (next, reason) = match joined {
                Ok(Some(reason)) => (DaemonPhase::Degraded, Some(reason)),
                Ok(None) => (DaemonPhase::Ready, None),
                Err(error) => (
                    DaemonPhase::Degraded,
                    Some(format!("预热任务失败：{error}")),
                ),
            };
            let since_ms = {
                let mut cell = match phase.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                cell.phase = next;
                cell.since_ms = now_ms();
                cell.since_ms
            };
            let mut params = json!({"phase": next.as_str(), "since_ms": since_ms});
            if let Some(reason) = &reason {
                if let Some(object) = params.as_object_mut() {
                    object.insert("reason".to_owned(), Value::from(reason.clone()));
                }
            }
            let _ =
                transport.send(&aether_rpc::Notification::new("daemon.status", params).to_value());
            tracing::info!(phase = next.as_str(), reason = ?reason, "预热阶段结束");
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

    server.begin_shutdown();
    server.shutdown_backend();
    drop(connection);
    let oversized_frames = frames.oversized_frames();
    drop(frames);
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_BUDGET);
    tracing::info!(
        frames = frames_seen,
        oversized = oversized_frames,
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
    fn transport_mapping_covers_all_32_codes() {
        // 32 枚举逐个映射（编译期穷尽性由 match 保证；此处锁定关键分类）。
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
            let failure = map_ipc_error(
                IpcError::invalid_value("injected").with_business_code(business),
            );
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
}
