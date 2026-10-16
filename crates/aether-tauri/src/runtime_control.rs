//! `runtime_retry` / `runtime_enable` 的 IPC 薄适配（M2-01 DoD6）。
//!
//! 服务契约冻结于 `docs/M1-10-证据.md` §6：
//! - 白名单 / 状态校验 / 转移语义全部在 `aether-adapters::supervisor`（M1-10），
//!   本层**不复刻、不放宽**，只做 DTO → 服务调用 → `Value`/`IpcError` 映射；
//! - 错误码映射：`UnknownRuntime` → `invalid_enum`；`RetryNotAllowed` /
//!   `EnableNotAllowed` / `NeedsRemedy` → `invalid_value`（消息携带当前状态与原因）；
//!   `Transition` / `Ledger` → `internal`；DTO 格式错误由 M1-08 框架在进入后端前拦截；
//! - 桥接：服务方法为 async、`IpcBackend` 为同步——本层把 future spawn 到核心
//!   tokio 运行时并以 `std::sync::mpsc` 等待（不阻塞运行时线程），30s 硬超时。

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aether_adapters::connection::ConnectionState;
use aether_adapters::protocol::DisabledReason;
use aether_adapters::supervisor::{
    now_ms, default_ledger_path, AdapterLedger, AdmissionPolicy, CleanupReport, NoopObserver,
    RuntimeManifest, RuntimeSpec, RuntimeSupervisor, StartOutcome, Supervisor, SupervisorConfig,
    SupervisorError, SupervisorObserver, SysinfoProbe, SystemTreeKiller,
};
use aether_core::{Runtime, RuntimeId, RuntimeStatus};
use aether_store::{AuditLogRecord, ReadPool, StoreCommand, WriteQueue};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::ipc::backend::IpcBackend;
use crate::ipc::dto::{RuntimeDisableRequest, RuntimeEnableRequest, RuntimeRetryRequest};
use crate::ipc::error::IpcError;

/// 控制命令硬超时（启动含握手/initialize，D6 方法表 10s + 进程启动余量）。
pub const RUNTIME_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// 启动序列尾段（孤儿清理 + 预热）的等待上限（单个握手/initialize 各受 D6 超时约束）。
pub const SUPERVISOR_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// 启动序列尾段结果（M2-08；D2 启动序列：孤儿清理 → 适配器预热 → 监控接线）。
pub struct SupervisorStartup {
    /// 启动清理报告（D5 三条件逐条处置）。
    pub cleanup: CleanupReport,
    /// 预热结果（`runtime_id` → `StartOutcome`）。
    pub warmups: Vec<(String, StartOutcome)>,
    /// 心跳/资源监控任务句柄（应用退出时 abort；见 `crate::shutdown::AppShutdown`）。
    pub monitors: Vec<(String, tokio::task::JoinHandle<()>)>,
}

/// 执行启动序列尾段（M2-08 D2 收口）：
/// ① 孤儿清理（D5 台账三条件，绝不误杀）；② 适配器预热（`initialize`，无持久化偏好）；
/// ③ 为全部注册运行时启动监控任务（10s 心跳/连续 3 次失败 → 重启，T5b）。
///
/// 同步阻塞接口：在核心 tokio 运行时上 spawn 后以 `std::sync::mpsc` 等待
/// （与 [`SupervisorControl::call`] 同口径，可从 Tauri 主线程/测试线程调用）。
pub fn run_supervisor_startup(
    supervisor: &Arc<Supervisor>,
    handle: &tokio::runtime::Handle,
) -> Result<SupervisorStartup, SupervisorError> {
    run_supervisor_startup_with_prefs(supervisor, handle, &std::collections::BTreeMap::new())
}

/// 启动序列尾段（M7-01/ADR-015 决策 5 启动矩阵）：`prefs` = `runtimes.enabled`
/// 持久化偏好；无记录回退构建期缺省。`enabled=0` 条目不预热、置
/// `cold → disabled(user_disabled)`（发事件 + 审计保留）。
pub fn run_supervisor_startup_with_prefs(
    supervisor: &Arc<Supervisor>,
    handle: &tokio::runtime::Handle,
    prefs: &std::collections::BTreeMap<String, bool>,
) -> Result<SupervisorStartup, SupervisorError> {
    let cleaner = Arc::clone(supervisor);
    let cleanup = blocking_call(handle, SUPERVISOR_STARTUP_TIMEOUT, async move {
        cleaner.cleanup_orphans().await
    })??;
    let warmer = Arc::clone(supervisor);
    let prefs = prefs.clone();
    let warmups = blocking_call(handle, SUPERVISOR_STARTUP_TIMEOUT, async move {
        warmer.warmup_all_with_prefs(&prefs).await
    })?;
    // `spawn_monitor` 内部使用 `tokio::spawn`：调用方可能位于非运行时线程
    // （Tauri setup / 测试线程），显式进入核心运行时上下文。
    let monitors = {
        let _guard = handle.enter();
        supervisor.spawn_monitors()
    };
    Ok(SupervisorStartup {
        cleanup,
        warmups,
        monitors,
    })
}

/// 在核心运行时上执行 future 并同步等待结果（超时返回 `SupervisorError`）。
fn blocking_call<T, F>(
    handle: &tokio::runtime::Handle,
    timeout: Duration,
    future: F,
) -> Result<T, SupervisorError>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::channel();
    handle.spawn(async move {
        let _ = sender.send(future.await);
    });
    receiver
        .recv_timeout(timeout)
        .map_err(|_| SupervisorError::Transition(format!("启动序列尾段超时（>{timeout:?}）")))
}

/// 运行时控制出口（测试替身/生产监督器共用）。
pub trait RuntimeControl: Send + Sync + 'static {
    /// `runtime_retry`（仅 `disabled + start_failed` 可用）。
    fn retry(&self, runtime_id: &str) -> Result<Value, IpcError>;
    /// `runtime_enable`（仅 `disabled` 可用；`untrusted`/`version_mismatch` 需先修复）。
    fn enable(&self, runtime_id: &str) -> Result<Value, IpcError>;
    /// `runtime_disable`（ADR-015）：在途未确认 → `runtime_busy`；确认后受控终止。
    fn disable(&self, runtime_id: &str, confirm_terminate: bool) -> Result<Value, IpcError>;
}

/// 命令回执（`Value` 编码形状；`outcome` 与 M1-10 `StartOutcome` 一一对应）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeControlReport {
    pub runtime_id: String,
    /// `ready` / `already_running` / `failed` / `rejected` / `disabled`（ADR-015 扩展）。
    pub outcome: String,
    /// 命令执行后的监督器状态（`cold`/`starting`/`ready`/`degraded`/`disabled`）。
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stderr_tail: Vec<String>,
}

/// 运行时启停存储接线（M7-01/ADR-015）：偏好写序（`runtimes.enabled`）与
/// 在途计数读路径（`active_work_counts`，与 `runtimes_list.active_sessions` 同源）。
#[derive(Clone)]
pub struct RuntimeControlStore {
    pub reads: ReadPool,
    pub write: WriteQueue,
}

/// 生产实现：M1-10 监督器 + tokio 句柄桥接。
pub struct SupervisorControl {
    supervisor: Arc<Supervisor>,
    handle: tokio::runtime::Handle,
    timeout: Duration,
    /// 存储接线（M7-01；`runtime_disable` 必需；未接线时 enable 跳过偏好写——仅测试构造）。
    store: Option<RuntimeControlStore>,
}

impl SupervisorControl {
    pub fn new(
        supervisor: Arc<Supervisor>,
        handle: tokio::runtime::Handle,
        timeout: Duration,
    ) -> Self {
        Self {
            supervisor,
            handle,
            timeout,
            store: None,
        }
    }

    /// 注入启停偏好/在途计数存储路径（M7-01 生产装配）。
    #[must_use]
    pub fn with_runtime_store(mut self, reads: ReadPool, write: WriteQueue) -> Self {
        self.store = Some(RuntimeControlStore { reads, write });
        self
    }

    pub fn supervisor(&self) -> &Arc<Supervisor> {
        &self.supervisor
    }

    /// 把 future spawn 到核心运行时并等待结果（同步命令层安全，不阻塞运行时线程）。
    fn call<F>(&self, future: F) -> Result<F::Output, IpcError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.handle.spawn(async move {
            let _ = sender.send(future.await);
        });
        receiver.recv_timeout(self.timeout).map_err(|_| {
            IpcError::internal(format!(
                "runtime 控制命令超时（>{:?}，监督器未在预算内返回）",
                self.timeout
            ))
        })
    }

    fn dispatch(&self, runtime_id: &str, mode: ControlMode) -> Result<Value, IpcError> {
        let supervisor = Arc::clone(&self.supervisor);
        let id = runtime_id.to_owned();
        let (result, status, reason) = self.call(async move {
            let result = match mode {
                ControlMode::Retry => supervisor.runtime_retry(&id).await,
                ControlMode::Enable => {
                    // 偏好先写（ADR-015 §2.1#5）在 enable_with_preference 内完成；
                    // 本分支仅为「无存储接线」的测试构造保留（生产经该函数不会命中）。
                    supervisor.runtime_enable(&id).await
                }
            };
            let (status, reason) = match supervisor.get(&id) {
                Some(runtime) => (runtime.status().await, runtime.status_reason().await),
                None => (RuntimeStatus::Disabled, None),
            };
            (result, status, reason)
        })?;
        match result {
            Ok(outcome) => {
                let report = report_from(outcome, runtime_id, status, reason);
                serde_json::to_value(report).map_err(|error| {
                    IpcError::internal(format!("runtime 控制回执序列化失败：{error}"))
                })
            }
            Err(error) => Err(map_supervisor_error(error)),
        }
    }

    /// `runtime_enable`（ADR-015 决策 6）：守卫不变（非 `disabled` → `invalid_value`；
    /// `untrusted`/`version_mismatch` → `NeedsRemedy`）；**偏好先写 `enabled=1`**、
    /// 启动失败按既有 `disabled + reason` 收口；启停动作入审计（actor=`user`）。
    fn enable_with_preference(&self, runtime_id: &str) -> Result<Value, IpcError> {
        let supervisor = Arc::clone(&self.supervisor);
        let store = self.store.clone();
        let id = runtime_id.to_owned();
        self.call(async move {
            let runtime = supervisor.get(&id).ok_or_else(|| {
                map_supervisor_error(SupervisorError::UnknownRuntime(id.clone()))
            })?;
            let status = runtime.status().await;
            let reason = runtime.status_reason().await;
            if status != RuntimeStatus::Disabled {
                return Err(map_supervisor_error(SupervisorError::EnableNotAllowed {
                    status,
                }));
            }
            if let Some(reason) = reason {
                if matches!(
                    reason,
                    DisabledReason::Untrusted | DisabledReason::VersionMismatch
                ) {
                    return Err(map_supervisor_error(SupervisorError::NeedsRemedy { reason }));
                }
            }
            if let Some(store) = store.as_ref() {
                write_runtime_preference(store, &runtime, &id, true).await?;
            }
            let result = supervisor.runtime_enable(&id).await;
            let (status, reason) = match supervisor.get(&id) {
                Some(runtime) => (runtime.status().await, runtime.status_reason().await),
                None => (RuntimeStatus::Disabled, None),
            };
            let result = match result {
                Ok(outcome) => {
                    let report = report_from(outcome, &id, status, reason);
                    serde_json::to_value(report).map_err(|error| {
                        IpcError::internal(format!("runtime 控制回执序列化失败：{error}"))
                    })
                }
                Err(error) => Err(map_supervisor_error(error)),
            };
            if let Some(store) = store.as_ref() {
                let (label, detail) = match &result {
                    Ok(value) => (
                        value
                            .get("outcome")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_owned(),
                        format!(
                            "runtime_enable：{id}；status={}；reason={}",
                            status.as_str(),
                            reason.map(DisabledReason::as_str).unwrap_or("-")
                        ),
                    ),
                    Err(error) => (
                        "error".to_owned(),
                        format!("runtime_enable：{id}；code={}", error.code.as_str()),
                    ),
                };
                write_control_audit(store, "runtime.enable", &id, detail, &label).await;
            }
            result
        })?
    }

    /// `runtime_disable`（ADR-015 决策 2/3/6）：注册表命中（否则 `invalid_enum`）→
    /// 在途计数（`active_work_counts`，与 `runtimes_list.active_sessions` 同源）→
    /// 未确认且 >0 → `runtime_busy`（不转移、不写偏好）→ **偏好先写 `enabled=0`**
    /// （失败即命令失败）→ 转移 `→ disabled(user_disabled)` → 受控终止 → 审计。
    fn disable_with_confirmation(
        &self,
        runtime_id: &str,
        confirm_terminate: bool,
    ) -> Result<Value, IpcError> {
        let store = self.store.clone().ok_or_else(|| {
            IpcError::core_not_ready(
                "runtime_disable 需要存储接线（在途计数与偏好写序；启动序列未完成）",
            )
        })?;
        let supervisor = Arc::clone(&self.supervisor);
        let id = runtime_id.to_owned();
        self.call(async move {
            let runtime = supervisor.get(&id).ok_or_else(|| {
                map_supervisor_error(SupervisorError::UnknownRuntime(id.clone()))
            })?;
            let status = runtime.status().await;
            if status == RuntimeStatus::Disabled {
                return Err(map_supervisor_error(SupervisorError::DisableNotAllowed {
                    status,
                }));
            }
            let counts = store.reads.active_work_counts().await.map_err(|error| {
                IpcError::internal(format!("在途计数读取失败（禁用确认门）：{error}"))
            })?;
            let in_flight = counts.get(&id).copied().unwrap_or(0);
            if in_flight > 0 && !confirm_terminate {
                return Err(IpcError::runtime_busy(&id, in_flight));
            }
            // 偏好先写（失败关闭：宁可维持原状，不产生「内存已停、重启又启动」）。
            write_runtime_preference(&store, &runtime, &id, false).await?;
            let outcome = supervisor
                .runtime_disable(&id)
                .await
                .map_err(map_supervisor_error)?;
            let report = RuntimeControlReport {
                runtime_id: id.clone(),
                outcome: "disabled".to_owned(),
                status: outcome.status.as_str().to_owned(),
                status_reason: Some(outcome.reason.as_str().to_owned()),
                detail: Some(format!(
                    "在途计数={in_flight}；确认={confirm_terminate}；已终止进程={}；机制={:?}；耗时={}ms",
                    outcome.terminated,
                    outcome.termination.mechanisms(),
                    outcome.termination.total_ms
                )),
                stderr_tail: Vec::new(),
            };
            write_control_audit(
                &store,
                "runtime.disable",
                &id,
                format!(
                    "runtime_disable：{id}；在途计数={in_flight}；确认={confirm_terminate}；已终止进程={}；耗时={}ms",
                    outcome.terminated, outcome.termination.total_ms
                ),
                "disabled",
            )
            .await;
            serde_json::to_value(report)
                .map_err(|error| IpcError::internal(format!("runtime 控制回执序列化失败：{error}")))
        })?
    }
}

/// 偏好写入（`runtimes.enabled` 唯一事实源；单写队列，D3/AGENTS §2.4）。
///
/// 行为行不存在时以监督器 manifest + 当前状态完成 upsert；失败即命令失败
/// （ADR-015 §2.1#5 偏好写序，失败关闭）。
async fn write_runtime_preference(
    store: &RuntimeControlStore,
    runtime: &Arc<RuntimeSupervisor>,
    runtime_id: &str,
    enabled: bool,
) -> Result<(), IpcError> {
    let manifest = runtime.manifest();
    let (status, reason) = runtime.summary_snapshot();
    let capabilities = match runtime.connection().await {
        Some(connection) => match connection.state() {
            ConnectionState::Ready(hello) | ConnectionState::Degraded { hello, .. } => {
                hello.runtime.capabilities
            }
            ConnectionState::Connecting | ConnectionState::Disconnected(_) => Vec::new(),
        },
        None => Vec::new(),
    };
    let now = now_ms();
    let row = Runtime {
        id: RuntimeId::new(manifest.id.clone())
            .map_err(|error| IpcError::internal(format!("runtime_id 非法（manifest）：{error}")))?,
        name: manifest.name.clone(),
        kind: manifest.kind.clone(),
        version: manifest.version.clone(),
        protocol: manifest.protocol.clone(),
        capabilities,
        endpoint: Some(manifest.program.display().to_string()),
        config: json!({}),
        status,
        status_reason: reason.map(|reason| reason.as_str().to_owned()),
        last_seen_at: None,
        created_at: now,
        updated_at: now,
    };
    store
        .write
        .execute(StoreCommand::SetRuntimeEnabled {
            runtime: row,
            enabled,
        })
        .await
        .map_err(|error| {
            IpcError::internal(format!("运行时启停偏好写入失败（{runtime_id}）：{error}"))
        })?;
    Ok(())
}

/// 启停动作审计（actor=`user`；ADR-015 附录 C-7；写入失败仅告警，不反向阻断命令）。
async fn write_control_audit(
    store: &RuntimeControlStore,
    action: &str,
    runtime_id: &str,
    detail: String,
    result: &str,
) {
    let record = AuditLogRecord {
        id: ulid::Ulid::new().to_string(),
        session_id: None,
        runtime_id: Some(runtime_id.to_owned()),
        actor: "user".to_owned(),
        action: action.to_owned(),
        resource: Some(format!("runtime:{runtime_id}")),
        detail: Some(detail),
        result: Some(result.to_owned()),
        ts: now_ms(),
    };
    if let Err(error) = store
        .write
        .execute(StoreCommand::InsertAudit { record })
        .await
    {
        tracing::warn!(error = %error, action, "运行时启停审计写入失败（仅告警）");
    }
}

#[derive(Debug, Clone, Copy)]
enum ControlMode {
    Retry,
    Enable,
}

impl RuntimeControl for SupervisorControl {
    fn retry(&self, runtime_id: &str) -> Result<Value, IpcError> {
        self.dispatch(runtime_id, ControlMode::Retry)
    }

    fn enable(&self, runtime_id: &str) -> Result<Value, IpcError> {
        if self.store.is_some() {
            self.enable_with_preference(runtime_id)
        } else {
            // 无存储接线的测试构造：保留原语义（无偏好写；生产装配恒有存储）。
            self.dispatch(runtime_id, ControlMode::Enable)
        }
    }

    fn disable(&self, runtime_id: &str, confirm_terminate: bool) -> Result<Value, IpcError> {
        self.disable_with_confirmation(runtime_id, confirm_terminate)
    }
}

fn report_from(
    outcome: StartOutcome,
    runtime_id: &str,
    status: RuntimeStatus,
    reason: Option<DisabledReason>,
) -> RuntimeControlReport {
    let (outcome_code, detail, stderr_tail) = match outcome {
        StartOutcome::Ready => ("ready".to_owned(), None, Vec::new()),
        StartOutcome::AlreadyRunning => ("already_running".to_owned(), None, Vec::new()),
        StartOutcome::Failed {
            reason,
            detail,
            stderr_tail,
        } => (
            "failed".to_owned(),
            Some(format!("{}：{detail}", reason.as_str())),
            stderr_tail,
        ),
        StartOutcome::Rejected { detail } => ("rejected".to_owned(), Some(detail), Vec::new()),
        StartOutcome::Disabled { reason } => (
            "disabled".to_owned(),
            Some(format!("用户禁用（{}）：未预热", reason.as_str())),
            Vec::new(),
        ),
    };
    RuntimeControlReport {
        runtime_id: runtime_id.to_owned(),
        outcome: outcome_code,
        status: status.as_str().to_owned(),
        status_reason: reason.map(|reason| reason.as_str().to_owned()),
        detail,
        stderr_tail,
    }
}

/// M1-10 §6.3 冻结映射（`runtime_busy` 为命令层新增码，不经本映射）。
pub fn map_supervisor_error(error: SupervisorError) -> IpcError {
    match error {
        SupervisorError::UnknownRuntime(id) => IpcError::invalid_enum(
            "runtime_id",
            format!("runtime_id {id:?} 不在白名单（runtimes 注册表）"),
        ),
        SupervisorError::InvalidRuntimeId(id) => {
            IpcError::invalid_format("runtime_id", format!("runtime_id {id:?} 非法"))
        }
        SupervisorError::RetryNotAllowed { .. }
        | SupervisorError::EnableNotAllowed { .. }
        | SupervisorError::DisableNotAllowed { .. }
        | SupervisorError::NeedsRemedy { .. } => IpcError::invalid_value(format!("{error}")),
        SupervisorError::Transition(_) | SupervisorError::Ledger(_) => {
            IpcError::internal(format!("{error}"))
        }
    }
}

/// 装饰器后端：`health` 透传内层；`runtime_retry`/`runtime_enable` 走控制出口；
/// 其余命令保持内层/默认实现（M2-07 再扩展 runtimes 摘要）。
pub struct RuntimeControlBackend {
    inner: Arc<dyn IpcBackend>,
    control: Option<Arc<dyn RuntimeControl>>,
}

impl RuntimeControlBackend {
    pub fn new(inner: Arc<dyn IpcBackend>, control: Option<Arc<dyn RuntimeControl>>) -> Self {
        Self { inner, control }
    }

    fn control_required(&self) -> Result<&dyn RuntimeControl, IpcError> {
        match self.control.as_deref() {
            Some(control) => Ok(control),
            None => Err(IpcError::core_not_ready(
                "监督器未接线：runtime 控制命令不可用（启动序列未完成或台账初始化失败）",
            )),
        }
    }
}

impl IpcBackend for RuntimeControlBackend {
    fn health(&self) -> Result<Value, IpcError> {
        self.inner.health()
    }

    fn runtime_retry(&self, request: &RuntimeRetryRequest) -> Result<Value, IpcError> {
        self.control_required()?.retry(&request.runtime_id)
    }

    fn runtime_enable(&self, request: &RuntimeEnableRequest) -> Result<Value, IpcError> {
        self.control_required()?.enable(&request.runtime_id)
    }

    fn runtime_disable(&self, request: &RuntimeDisableRequest) -> Result<Value, IpcError> {
        self.control_required()?
            .disable(&request.runtime_id, request.confirm_terminate.unwrap_or(false))
    }
}

/// 生产监督器构造（M2-01 仅空注册表；M2-02 注册真实适配器 spec）。
///
/// 台账路径默认 `~/.aether/run/adapters.json`（D5）；失败时返回错误，调用方降级为
/// `control = None`（命令回 `core_not_ready`），不影响 `health`。
///
/// 观察者默认 [`NoopObserver`]；M3-07 起生产装配经
/// [`boot_supervisor_with_observer`] 注入审计落库出口（见 `audit_bridge`）。
pub fn boot_supervisor(
    specs: Vec<RuntimeSpec>,
    ledger_path: Option<&Path>,
) -> Result<Supervisor, SupervisorError> {
    boot_supervisor_with_observer(specs, ledger_path, Arc::new(NoopObserver))
}

/// 带观察者的监督器构造（M3-07：适配器状态变化审计落库出口注入点）。
///
/// 观察者语义与冻结契约一致（`docs/M1-10-证据.md` §6）：状态转移与审计记录
/// 经 `SupervisorObserver` 上报；本函数只做装配，不复刻/不放宽监督语义。
pub fn boot_supervisor_with_observer(
    specs: Vec<RuntimeSpec>,
    ledger_path: Option<&Path>,
    observer: Arc<dyn SupervisorObserver>,
) -> Result<Supervisor, SupervisorError> {
    let ledger_path = ledger_path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_ledger_path);
    let ledger = AdapterLedger::load(ledger_path)
        .map_err(|error| SupervisorError::Ledger(error.to_string()))?;
    Supervisor::new(
        specs,
        SupervisorConfig::d5(),
        AdmissionPolicy::official(),
        observer,
        Arc::new(Mutex::new(ledger)),
        Arc::new(SysinfoProbe::new()),
        Arc::new(SystemTreeKiller),
    )
}

/// 便捷构造（空注册表；测试/生产启动）。
pub fn boot_empty_supervisor(ledger_path: Option<&Path>) -> Result<Supervisor, SupervisorError> {
    boot_supervisor(Vec::new(), ledger_path)
}

/// 带观察者的空注册表构造（M3-07 生产装配：延迟审计出口）。
pub fn boot_empty_supervisor_with_observer(
    ledger_path: Option<&Path>,
    observer: Arc<dyn SupervisorObserver>,
) -> Result<Supervisor, SupervisorError> {
    boot_supervisor_with_observer(Vec::new(), ledger_path, observer)
}

/// 单 spec 便捷构造（测试用）。
pub fn mock_spec(runtime_id: &str, program: &str) -> RuntimeSpec {
    RuntimeSpec::with_fresh_token(RuntimeManifest::new(runtime_id, "Mock", program))
}
