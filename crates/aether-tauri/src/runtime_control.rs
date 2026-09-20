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

use aether_adapters::protocol::DisabledReason;
use aether_adapters::supervisor::{
    default_ledger_path, AdapterLedger, AdmissionPolicy, NoopObserver, RuntimeManifest,
    RuntimeSpec, StartOutcome, Supervisor, SupervisorConfig, SupervisorError, SysinfoProbe,
    SystemTreeKiller,
};
use aether_core::RuntimeStatus;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::ipc::backend::IpcBackend;
use crate::ipc::dto::{RuntimeEnableRequest, RuntimeRetryRequest};
use crate::ipc::error::IpcError;

/// 控制命令硬超时（启动含握手/initialize，D6 方法表 10s + 进程启动余量）。
pub const RUNTIME_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// 运行时控制出口（测试替身/生产监督器共用）。
pub trait RuntimeControl: Send + Sync + 'static {
    /// `runtime_retry`（仅 `disabled + start_failed` 可用）。
    fn retry(&self, runtime_id: &str) -> Result<Value, IpcError>;
    /// `runtime_enable`（仅 `disabled` 可用；`untrusted`/`version_mismatch` 需先修复）。
    fn enable(&self, runtime_id: &str) -> Result<Value, IpcError>;
}

/// 命令回执（`Value` 编码形状；`outcome` 与 M1-10 `StartOutcome` 一一对应）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeControlReport {
    pub runtime_id: String,
    /// `ready` / `already_running` / `failed` / `rejected`。
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

/// 生产实现：M1-10 监督器 + tokio 句柄桥接。
pub struct SupervisorControl {
    supervisor: Arc<Supervisor>,
    handle: tokio::runtime::Handle,
    timeout: Duration,
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
        }
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
                ControlMode::Enable => supervisor.runtime_enable(&id).await,
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
        self.dispatch(runtime_id, ControlMode::Enable)
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

/// M1-10 §6.3 冻结映射（不新增错误码）。
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
}

/// 生产监督器构造（M2-01 仅空注册表；M2-02 注册真实适配器 spec）。
///
/// 台账路径默认 `~/.aether/run/adapters.json`（D5）；失败时返回错误，调用方降级为
/// `control = None`（命令回 `core_not_ready`），不影响 `health`。
pub fn boot_supervisor(
    specs: Vec<RuntimeSpec>,
    ledger_path: Option<&Path>,
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
        Arc::new(NoopObserver),
        Arc::new(Mutex::new(ledger)),
        Arc::new(SysinfoProbe::new()),
        Arc::new(SystemTreeKiller),
    )
}

/// 便捷构造（空注册表；测试/生产启动）。
pub fn boot_empty_supervisor(ledger_path: Option<&Path>) -> Result<Supervisor, SupervisorError> {
    boot_supervisor(Vec::new(), ledger_path)
}

/// 单 spec 便捷构造（测试用）。
pub fn mock_spec(runtime_id: &str, program: &str) -> RuntimeSpec {
    RuntimeSpec::with_fresh_token(RuntimeManifest::new(runtime_id, "Mock", program))
}
