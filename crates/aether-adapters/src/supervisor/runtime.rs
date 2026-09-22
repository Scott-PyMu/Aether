//! M1-10 适配器监督器（D5）：状态机、预热、心跳、退避/熔断、台账、终止序列、
//! 资源采样、`runtime_retry` / `runtime_enable` 命令语义。
//!
//! 依赖边界（AGENTS §2.1）：本模块仅依赖 `aether-core` 与本 crate 的协议/进程层；
//! 事件写入与 IPC 接线由 M2-01 / 命令层通过 [`SupervisorObserver`] 完成。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aether_core::{EventType, RuntimeId, RuntimeStatus};
use serde_json::json;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::admission::{AdmissionDecision, AdmissionPolicy, RuntimeManifest};
use super::backoff::{CrashDecision, RestartPolicy, CRASH_WINDOW};
use super::clock::{SharedClock, SystemTimeSource};
use super::heartbeat::{HeartbeatConfig, HeartbeatMonitor, HeartbeatVerdict};
use super::ledger::{
    cmdline_hash, AdapterLedger, CleanupReport, LedgerRecord, LedgerVerdict, ProcessProbe,
    TreeKiller,
};
use super::resources::{ResourceConfig, ResourceMonitor, SysinfoSampler, RESOURCE_SAMPLE_INTERVAL};
use super::state::{
    now_ms, AuditKind, AuditRecord, ResourceEvent, ResourceLimitKind, StateCore, StatusChange,
    SupervisorObserver,
};
use super::termination::{run_termination, TerminationBudget, TerminationReport};
use crate::connection::{AdapterConnection, RequestError};
use crate::process::{AdapterProcess, ProcessTerminationTarget};
use crate::protocol::{code, DisabledInfo, DisabledReason, Method, HANDSHAKE_TIMEOUT};

/// 预热/启动时 `initialize` 超时（D6 方法表 10s）。
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);

/// 监督器配置（默认严格取 D5；测试经 `backoff_override` 压缩，常量级调参记录于任务证据）。
#[derive(Debug, Clone, PartialEq)]
pub struct SupervisorConfig {
    /// 握手超时（D6：启动 10s 内必须 hello）。
    pub handshake_timeout: Duration,
    /// `initialize` 超时（D6 方法表 10s）。
    pub initialize_timeout: Duration,
    /// 心跳参数（D5：10s / 5s / 连续 3 次）。
    pub heartbeat: HeartbeatConfig,
    /// 终止序列逐步硬超时（D5：5s/5s/3s/2s）。
    pub termination: TerminationBudget,
    /// 退避等待压缩（仅测试用；`None` = 严格执行 D5 曲线 1/2/4/8/16/30s）。
    pub backoff_override: Option<Duration>,
}

impl SupervisorConfig {
    /// D5 默认。
    pub const fn d5() -> Self {
        Self {
            handshake_timeout: HANDSHAKE_TIMEOUT,
            initialize_timeout: INITIALIZE_TIMEOUT,
            heartbeat: HeartbeatConfig::d5(),
            termination: TerminationBudget::d5(),
            backoff_override: None,
        }
    }
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self::d5()
    }
}

/// 运行时静态描述（manifest + 本次启动令牌 + 附件目录）。
#[derive(Debug, Clone)]
pub struct RuntimeSpec {
    pub manifest: RuntimeManifest,
    pub launch_token: String,
    /// 附件目录（M2-09/D6：`artifact_ref` 数据体存 artifacts 文件，不进入线协议）。
    ///
    /// 启动时创建 `<artifacts_dir>/<runtime_id>/` 并经 `AETHER_ARTIFACTS_DIR` 注入适配器
    /// 进程环境；`None` = 未启用附件外置（适配器不得上报引用帧——消费侧校验器无根目录
    /// 时一律拒绝）。
    pub artifacts_dir: Option<std::path::PathBuf>,
}

impl RuntimeSpec {
    pub fn new(manifest: RuntimeManifest, launch_token: impl Into<String>) -> Self {
        Self {
            manifest,
            launch_token: launch_token.into(),
            artifacts_dir: None,
        }
    }

    /// 生成新令牌（ULID 形状；时间 + 进程 id + 原子计数保证唯一）。
    pub fn with_fresh_token(manifest: RuntimeManifest) -> Self {
        Self {
            manifest,
            launch_token: new_launch_token(),
            artifacts_dir: None,
        }
    }

    /// 附加附件目录（D6 附件外置；环境注入见 [`RuntimeSupervisor::start`]）。
    pub fn with_artifacts_dir(mut self, artifacts_dir: impl Into<std::path::PathBuf>) -> Self {
        self.artifacts_dir = Some(artifacts_dir.into());
        self
    }
}

/// `AETHER_ARTIFACTS_DIR`：注入适配器进程的附件目录环境变量（M2-09/D6）。
pub const ENV_ARTIFACTS_DIR: &str = "AETHER_ARTIFACTS_DIR";

/// 启动结果。
#[derive(Debug, Clone, PartialEq)]
pub enum StartOutcome {
    /// 完成握手 + `initialize`，进入 `ready`。
    Ready,
    /// 已有进程在运行（拒绝重复启动；D5「双开」预防）。
    AlreadyRunning,
    /// 启动失败：进入 `disabled + reason`，附带 stderr 尾 50 行（DoD⑥）。
    Failed {
        reason: DisabledReason,
        detail: String,
        stderr_tail: Vec<String>,
    },
    /// 准入拒绝：`disabled + untrusted`（DoD⑤）。
    Rejected { detail: String },
}

impl StartOutcome {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready | Self::AlreadyRunning)
    }
}

/// 单次监控采样结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorOutcome {
    /// 心跳成功（或失败计数未达阈值）。
    Healthy,
    /// 连续 3 次心跳失败（触发重启）。
    Unhealthy { consecutive_failures: u32 },
    /// 进程已退出（运行中崩溃，触发重启）。
    ProcessExited { detail: String },
    /// 无运行进程（`cold` / `disabled` / 启动中）。
    NoProcess,
}

/// 自动重启结果。
#[derive(Debug, Clone, PartialEq)]
pub enum RestartOutcome {
    /// 重启成功（`degraded → starting → ready`）。
    Ready,
    /// 熔断：`disabled + crash_loop`（D5：60s 内 ≥5 次崩溃）。
    CircuitBroken { crashes_in_window: usize },
    /// 启动失败（已置 `disabled + reason`）。
    Failed {
        reason: DisabledReason,
        detail: String,
    },
    /// 状态不允许重启（如已被人工禁用）。
    NotApplicable { status: RuntimeStatus },
}

/// 隔离结果（M2-04/D8：存储侧背压熔断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsolationOutcome {
    /// 已隔离：`degraded + reason`，进程已终止（停止事件生产），等待解除。
    Isolated { status: RuntimeStatus },
    /// 不适用：`cold` / `disabled` / 已处于 `degraded`（保留既有原因）。
    NotApplicable {
        status: RuntimeStatus,
        reason: Option<DisabledReason>,
    },
}

/// 解除隔离结果（M2-04/D8：写队列回落 → 自动解除并重启）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// 已解除并回到 `ready`。
    Released,
    /// 当前不在 `degraded`（无需解除；含 `ready`/`cold`/`disabled`）。
    NotApplicable { status: RuntimeStatus },
    /// 重启失败（已置 `disabled + reason`）。
    Failed {
        reason: DisabledReason,
        detail: String,
    },
}

/// 命令层错误（`runtime_retry` / `runtime_enable` 的参数与状态校验，DoD⑦）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SupervisorError {
    #[error("runtime_id {0:?} 不在白名单（runtimes 注册表）")]
    UnknownRuntime(String),
    #[error("runtime_id {0:?} 非法（不得为空）")]
    InvalidRuntimeId(String),
    #[error("runtime_retry 仅适用于 disabled + start_failed（当前 {status} / {reason:?}）")]
    RetryNotAllowed {
        status: RuntimeStatus,
        reason: Option<DisabledReason>,
    },
    #[error("runtime_enable 仅适用于 disabled（当前 {status}）")]
    EnableNotAllowed { status: RuntimeStatus },
    #[error("runtime_enable 被拒：{reason} 必须先修复清单/版本（ADR-004）")]
    NeedsRemedy { reason: DisabledReason },
    #[error("状态转移失败：{0}")]
    Transition(String),
    #[error("台账错误：{0}")]
    Ledger(String),
}

struct RunningProcess {
    process: AdapterProcess,
    connection: Arc<AdapterConnection>,
}

struct SupervisorState {
    fsm: StateCore,
    restart: RestartPolicy,
    heartbeat: HeartbeatMonitor,
    resource: ResourceMonitor,
    running: Option<RunningProcess>,
    ready_since_ms: Option<u64>,
}

/// 单运行时监督器。
pub struct RuntimeSupervisor {
    runtime_id: RuntimeId,
    spec: RuntimeSpec,
    config: SupervisorConfig,
    observer: Arc<dyn SupervisorObserver>,
    ledger: Arc<Mutex<AdapterLedger>>,
    probe: Arc<dyn ProcessProbe>,
    sampler: SysinfoSampler,
    state: Mutex<SupervisorState>,
    /// 同步状态快照缓存（M2-07：`health.runtimes` 摘要的最小读取句柄）。
    ///
    /// `state` 为异步互斥（启动/握手期间可能长时间持锁），健康查询不能等待；
    /// 每次状态转移在 `transition_locked` 内同步刷新本缓存（临界区仅赋值）。
    summary: std::sync::Mutex<(RuntimeStatus, Option<DisabledReason>)>,
}

impl RuntimeSupervisor {
    pub fn new(
        runtime_id: RuntimeId,
        spec: RuntimeSpec,
        config: SupervisorConfig,
        observer: Arc<dyn SupervisorObserver>,
        ledger: Arc<Mutex<AdapterLedger>>,
        probe: Arc<dyn ProcessProbe>,
    ) -> Self {
        let clock: SharedClock = Arc::new(SystemTimeSource::new());
        Self {
            runtime_id: runtime_id.clone(),
            spec,
            config: config.clone(),
            observer,
            ledger,
            probe,
            sampler: SysinfoSampler::new(),
            state: Mutex::new(SupervisorState {
                fsm: StateCore::new(runtime_id),
                restart: RestartPolicy::new(clock),
                heartbeat: HeartbeatMonitor::new(config.heartbeat),
                // M1-10 增量：测试阈值 env 钩子（未设置时严格等于 D5 默认）。
                resource: ResourceMonitor::with_config(ResourceConfig::from_env()),
                running: None,
                ready_since_ms: None,
            }),
            summary: std::sync::Mutex::new((RuntimeStatus::Cold, None)),
        }
    }

    pub fn runtime_id(&self) -> RuntimeId {
        self.runtime_id.clone()
    }

    pub fn manifest(&self) -> &RuntimeManifest {
        &self.spec.manifest
    }

    pub async fn status(&self) -> RuntimeStatus {
        self.state.lock().await.fsm.status()
    }

    pub async fn status_reason(&self) -> Option<DisabledReason> {
        self.state.lock().await.fsm.status_reason()
    }

    /// 同步状态快照（M2-07：`health.runtimes` 摘要；不等待异步状态锁）。
    ///
    /// 返回值与 `runtimes.status` / `runtimes.status_reason` 一一对应（D5）；
    /// 由每次 [`RuntimeSupervisor::transition_locked`] 同步刷新。
    pub fn summary_snapshot(&self) -> (RuntimeStatus, Option<DisabledReason>) {
        match self.summary.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    /// 当前是否持有运行中的进程（诊断/断言用）。
    pub async fn is_running(&self) -> bool {
        self.state.lock().await.running.is_some()
    }

    /// 当前进程 PID（诊断/台账断言用）。
    pub async fn current_pid(&self) -> Option<u32> {
        self.state
            .lock()
            .await
            .running
            .as_ref()
            .and_then(|running| running.process.id())
    }

    /// 当前运行中适配器的线协议连接（M2-02：核心会话客户端据此驱动 D6 会话方法）。
    ///
    /// 返回 `None` 表示无运行进程（`cold`/`disabled`/启动中/已崩溃）；
    /// 连接随重启更换，调用方须在每次 `start`/`restart` 成功后重新获取。
    pub async fn connection(&self) -> Option<Arc<AdapterConnection>> {
        self.state
            .lock()
            .await
            .running
            .as_ref()
            .map(|running| Arc::clone(&running.connection))
    }

    /// 非官方 manifest：拒绝加载 → `disabled + untrusted` + 审计（评审 #1）。
    pub async fn reject_untrusted(&self, detail: String) -> StartOutcome {
        let mut state = self.state.lock().await;
        if state.fsm.status() != RuntimeStatus::Disabled {
            let _ = self.transition_locked(
                &mut state,
                RuntimeStatus::Disabled,
                Some(DisabledReason::Untrusted),
                Some(detail.clone()),
            );
        }
        self.observer.on_audit(&AuditRecord::new(
            self.runtime_id.clone(),
            AuditKind::AdmissionRejected,
            detail.clone(),
            now_ms(),
        ));
        StartOutcome::Rejected { detail }
    }

    /// 启动并完成 `initialize`（预热单元）。
    pub async fn start(&self) -> StartOutcome {
        let mut state = self.state.lock().await;
        if state.running.is_some() {
            return StartOutcome::AlreadyRunning;
        }
        let from = state.fsm.status();
        match from {
            RuntimeStatus::Cold | RuntimeStatus::Degraded => {}
            _ => {
                return StartOutcome::Failed {
                    reason: DisabledReason::StartFailed,
                    detail: format!("状态 {from} 不允许启动（先经 runtime_retry/runtime_enable）"),
                    stderr_tail: Vec::new(),
                }
            }
        }
        if let Err(error) = self.transition_locked(&mut state, RuntimeStatus::Starting, None, None)
        {
            return StartOutcome::Failed {
                reason: DisabledReason::StartFailed,
                detail: error.to_string(),
                stderr_tail: Vec::new(),
            };
        }

        // spawn：D5 `--launch-token` 注入 + 进程组/Job Object（process.rs）。
        let mut args = self.spec.manifest.args.clone();
        args.push(format!("--launch-token={}", self.spec.launch_token));
        let mut envs: Vec<(std::ffi::OsString, std::ffi::OsString)> = self
            .spec
            .manifest
            .env
            .iter()
            .map(|(key, value)| {
                (
                    std::ffi::OsString::from(key),
                    std::ffi::OsString::from(value),
                )
            })
            .collect();
        // M2-09（D6 附件外置）：创建 `<artifacts_dir>/<runtime_id>/` 并经环境注入适配器；
        // 目录创建失败按 start_failed 处置（附件契约依赖该目录，不得静默降级）。
        if let Some(root) = self.spec.artifacts_dir.as_ref() {
            let per_runtime = root.join(self.runtime_id.as_str());
            if let Err(error) = std::fs::create_dir_all(&per_runtime) {
                return self.fail_start_locked(
                    &mut state,
                    DisabledReason::StartFailed,
                    format!("附件目录创建失败：{error}"),
                    Vec::new(),
                );
            }
            envs.push((
                std::ffi::OsString::from(ENV_ARTIFACTS_DIR),
                per_runtime.as_os_str().to_os_string(),
            ));
        }
        let mut process =
            match AdapterProcess::spawn_with_env(&self.spec.manifest.program, args, envs).await {
                Ok(process) => process,
                Err(error) => {
                    return self.fail_start_locked(
                        &mut state,
                        DisabledReason::StartFailed,
                        format!("spawn 失败：{error}"),
                        Vec::new(),
                    )
                }
            };

        // 台账登记（pid + OS 启动时间 + 令牌；三条件核对供下次启动清理）。
        if let Some(pid) = process.id() {
            let facts = self.probe.facts(pid);
            let cmdline = if facts.cmdline.is_empty() {
                vec![
                    self.spec.manifest.program.display().to_string(),
                    format!("--launch-token={}", self.spec.launch_token),
                ]
            } else {
                facts.cmdline.clone()
            };
            let record = LedgerRecord {
                adapter_id: self.spec.manifest.id.clone(),
                pid,
                start_time_epoch: facts.start_time_epoch.unwrap_or_default(),
                launch_token: self.spec.launch_token.clone(),
                cmdline_hash: cmdline_hash(&cmdline),
            };
            if let Err(error) = self.ledger.lock().await.record_launch(record) {
                return self.fail_start_locked(
                    &mut state,
                    DisabledReason::StartFailed,
                    format!("台账登记失败：{error}"),
                    Vec::new(),
                );
            }
        }

        let connection = match process.connect() {
            Ok(connection) => Arc::new(connection),
            Err(error) => {
                self.terminate_process(&mut process, None).await;
                return self.fail_start_locked(
                    &mut state,
                    DisabledReason::StartFailed,
                    format!("stdio 连接失败：{error}"),
                    process.stderr_tail(),
                );
            }
        };

        // 握手（D6：10s 内 hello + major 校验）。启动期退出 → start_failed（D5 失败表）。
        if let Err(disabled) = connection
            .handshake_with_timeout(self.config.handshake_timeout)
            .await
        {
            // 给进程一个短暂退出窗口：EOF 先于进程状态可见（启动即崩场景，DoD⑥）。
            let exited = wait_process_exit(&mut process, Duration::from_millis(1_000)).await;
            let (reason, detail) = match exited {
                Some(status) => (
                    DisabledReason::StartFailed,
                    format!("启动期进程退出（{status}）：{}", disabled.detail),
                ),
                None => (
                    classify_handshake_failure(&disabled),
                    disabled.detail.clone(),
                ),
            };
            self.terminate_process(&mut process, Some(&connection))
                .await;
            if exited.is_some() {
                process.join_stderr().await;
            }
            let tail = process.stderr_tail();
            return self.fail_start_locked(&mut state, reason, detail, tail);
        }

        // initialize（D5 预热：启动全部 enabled 适配器并完成 initialize）。
        if let Err(error) = connection
            .request_with_timeout(
                Method::Initialize,
                json!({"config": self.spec.manifest.kind}),
                self.config.initialize_timeout,
            )
            .await
        {
            self.terminate_process(&mut process, Some(&connection))
                .await;
            let tail = process.stderr_tail();
            // M2-11/ADR-008：适配器侧版本门闩（如 DSH pin 不匹配）经应用码 1003 上报，
            // 映射为 `disabled + version_mismatch`（沿用 D5/D6 协议词典，其余仍 start_failed）。
            let reason = classify_initialize_failure(&error);
            return self.fail_start_locked(
                &mut state,
                reason,
                format!("initialize 失败：{error}"),
                tail,
            );
        }

        // 启动期退出竞态：进入 Ready 前复查进程状态。
        if let Some(status) = process.try_status() {
            self.terminate_process(&mut process, Some(&connection))
                .await;
            let tail = process.stderr_tail();
            return self.fail_start_locked(
                &mut state,
                DisabledReason::StartFailed,
                format!("initialize 完成后进程已退出：{status}"),
                tail,
            );
        }

        state.heartbeat = HeartbeatMonitor::new(self.config.heartbeat);
        state.running = Some(RunningProcess {
            process,
            connection,
        });
        let _ = self.transition_locked(&mut state, RuntimeStatus::Ready, None, None);
        StartOutcome::Ready
    }

    /// 一次监控采样：进程存活检查 + 心跳。
    pub async fn monitor_once(&self) -> MonitorOutcome {
        let mut state = self.state.lock().await;
        let now = now_u64();
        let Some(running) = state.running.as_mut() else {
            return MonitorOutcome::NoProcess;
        };
        if let Some(status) = running.process.try_status() {
            return MonitorOutcome::ProcessExited {
                detail: format!("进程已退出：{status}"),
            };
        }
        let result = running
            .connection
            .request_with_timeout(Method::HealthPing, json!({}), self.config.heartbeat.timeout)
            .await;
        match result {
            Ok(_) => {
                state.heartbeat.record_success(now);
                MonitorOutcome::Healthy
            }
            Err(_error) => match state.heartbeat.record_failure(now) {
                HeartbeatVerdict::Unhealthy {
                    consecutive_failures,
                } => MonitorOutcome::Unhealthy {
                    consecutive_failures,
                },
                HeartbeatVerdict::Healthy => MonitorOutcome::Healthy,
            },
        }
    }

    /// 自动重启：终止当前进程 → 退避 → 重新启动（`degraded → starting → ready`）。
    pub async fn restart(&self, trigger: DisabledReason, detail: &str) -> RestartOutcome {
        let wait = {
            let mut state = self.state.lock().await;
            let status = state.fsm.status();
            match status {
                RuntimeStatus::Disabled => return RestartOutcome::NotApplicable { status },
                RuntimeStatus::Cold | RuntimeStatus::Starting => {
                    return RestartOutcome::NotApplicable { status }
                }
                RuntimeStatus::Ready => {
                    let _ = self.transition_locked(
                        &mut state,
                        RuntimeStatus::Degraded,
                        Some(trigger),
                        Some(detail.to_owned()),
                    );
                }
                RuntimeStatus::Degraded => {}
            }
            let decision = state.restart.record_crash();
            let wait = match decision {
                CrashDecision::CircuitBroken { crashes_in_window } => {
                    let _ = self.transition_locked(
                        &mut state,
                        RuntimeStatus::Disabled,
                        Some(DisabledReason::CrashLoop),
                        Some(format!("{crashes_in_window} 次崩溃/重启发生在 60s 内")),
                    );
                    self.observer.on_audit(&AuditRecord::new(
                        self.runtime_id.clone(),
                        AuditKind::CircuitBroken,
                        format!("{detail}；窗口内 {crashes_in_window} 次"),
                        now_ms(),
                    ));
                    // 熔断后不允许残留运行进程/进程槽位（enable/retry 从干净状态重启）。
                    if let Some(mut running) = state.running.take() {
                        let _ = self.terminate_running(&mut running).await;
                    }
                    return RestartOutcome::CircuitBroken { crashes_in_window };
                }
                CrashDecision::Backoff(duration) => {
                    self.config.backoff_override.unwrap_or(duration)
                }
            };
            state.heartbeat = HeartbeatMonitor::new(self.config.heartbeat);
            if let Some(mut running) = state.running.take() {
                let report = self.terminate_running(&mut running).await;
                self.observer.on_audit(&AuditRecord::new(
                    self.runtime_id.clone(),
                    AuditKind::Terminated,
                    format!(
                        "重启前终止（{detail}）；机制={:?}，退出={}，耗时={}ms",
                        report.mechanisms(),
                        report.exited,
                        report.total_ms
                    ),
                    now_ms(),
                ));
            }
            wait
        };
        tokio::time::sleep(wait).await;
        match self.start().await {
            StartOutcome::Ready | StartOutcome::AlreadyRunning => RestartOutcome::Ready,
            StartOutcome::Failed { reason, detail, .. } => {
                RestartOutcome::Failed { reason, detail }
            }
            StartOutcome::Rejected { detail } => RestartOutcome::Failed {
                reason: DisabledReason::Untrusted,
                detail,
            },
        }
    }

    /// 隔离（M2-04/D8 存储侧背压熔断）：`ready`/`starting` → `degraded + reason`，
    /// **终止进程**（停止事件生产）且不自动重启；解除经 [`RuntimeSupervisor::release`]。
    ///
    /// 与 [`RuntimeSupervisor::restart`] 的区别：隔离不计入崩溃退避/熔断（存储压力非
    /// 适配器故障），且不自动回到 `ready`（等待控制层队列回落解除，ADR-004）。
    pub async fn isolate(&self, reason: DisabledReason, detail: &str) -> IsolationOutcome {
        let mut state = self.state.lock().await;
        let status = state.fsm.status();
        match status {
            RuntimeStatus::Cold | RuntimeStatus::Disabled | RuntimeStatus::Degraded => {
                return IsolationOutcome::NotApplicable {
                    status,
                    reason: state.fsm.status_reason(),
                };
            }
            RuntimeStatus::Ready | RuntimeStatus::Starting => {}
        }
        if self
            .transition_locked(
                &mut state,
                RuntimeStatus::Degraded,
                Some(reason),
                Some(detail.to_owned()),
            )
            .is_err()
        {
            return IsolationOutcome::NotApplicable {
                status,
                reason: state.fsm.status_reason(),
            };
        }
        if let Some(mut running) = state.running.take() {
            let report = self.terminate_running(&mut running).await;
            self.observer.on_audit(&AuditRecord::new(
                self.runtime_id.clone(),
                AuditKind::Terminated,
                format!(
                    "背压隔离（{detail}）；机制={:?}，退出={}，耗时={}ms",
                    report.mechanisms(),
                    report.exited,
                    report.total_ms
                ),
                now_ms(),
            ));
        }
        IsolationOutcome::Isolated {
            status: RuntimeStatus::Degraded,
        }
    }

    /// 解除隔离并重启（M2-04/D8：写队列回落 ≤1024 持续 30s → `degraded → starting → ready`）。
    pub async fn release(&self) -> ReleaseOutcome {
        let status = self.status().await;
        match status {
            RuntimeStatus::Degraded => match self.start().await {
                StartOutcome::Ready | StartOutcome::AlreadyRunning => ReleaseOutcome::Released,
                StartOutcome::Failed { reason, detail, .. } => {
                    ReleaseOutcome::Failed { reason, detail }
                }
                StartOutcome::Rejected { detail } => ReleaseOutcome::Failed {
                    reason: DisabledReason::Untrusted,
                    detail,
                },
            },
            other => ReleaseOutcome::NotApplicable { status: other },
        }
    }

    /// 资源采样（D5：5s、RSS >1GB 或 CPU >200% 持续 60s → 告警；不自动杀）。
    pub async fn sample_resources(&self) -> Option<ResourceEvent> {
        let (pid, runtime_id) = {
            let state = self.state.lock().await;
            let running = state.running.as_ref()?;
            (running.process.id()?, self.runtime_id.clone())
        };
        let sample = self.sampler.sample(pid)?;
        let mut state = self.state.lock().await;
        let observation = state.resource.observe(now_u64(), sample)?;
        let event = ResourceEvent {
            runtime_id,
            pid,
            rss_bytes: sample.rss_bytes,
            cpu_percent: sample.cpu_percent,
            sustained_ms: observation.sustained_ms,
            limit: match observation.limit {
                ResourceLimitKind::Rss => ResourceLimitKind::Rss,
                ResourceLimitKind::Cpu => ResourceLimitKind::Cpu,
            },
        };
        self.observer.on_resource_alert(&event);
        Some(event)
    }

    /// 稳定运行 ≥60s 后重置退避级数（D5 曲线不跨稳定期累计）。
    pub async fn mark_stable_if_due(&self) {
        let mut state = self.state.lock().await;
        if state.fsm.status() != RuntimeStatus::Ready {
            return;
        }
        let now = now_u64();
        if let Some(since) = state.ready_since_ms {
            if now.saturating_sub(since) >= CRASH_WINDOW.as_millis() as u64 {
                state.restart.mark_stable();
                state.ready_since_ms = Some(now);
            }
        }
    }

    /// 优雅停止（应用关闭/测试清理）：终止进程 + 清台账；不改状态（D5 关闭序列）。
    pub async fn shutdown(&self) -> TerminationReport {
        let mut state = self.state.lock().await;
        let mut report = TerminationReport::default();
        if let Some(mut running) = state.running.take() {
            report = self.terminate_running(&mut running).await;
        }
        let _ = self.ledger.lock().await.remove(&self.spec.manifest.id);
        report
    }

    /// 监控循环（真实应用由 M2-01 启动；测试用 `monitor_once` 显式驱动）。
    ///
    /// 调用方负责在结束时 abort 该任务（随应用关闭或测试清理）。
    pub fn spawn_monitor(self: &Arc<Self>) -> JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut heartbeat_ticker = tokio::time::interval(this.config.heartbeat.interval);
            heartbeat_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut resource_ticker = tokio::time::interval(RESOURCE_SAMPLE_INTERVAL);
            resource_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = heartbeat_ticker.tick() => {
                        match this.monitor_once().await {
                            MonitorOutcome::Unhealthy { consecutive_failures } => {
                                let _ = this
                                    .restart(
                                        DisabledReason::HeartbeatFailed,
                                        &format!("连续 {consecutive_failures} 次心跳失败"),
                                    )
                                    .await;
                            }
                            MonitorOutcome::ProcessExited { detail } => {
                                let _ = this.restart(DisabledReason::Crashed, &detail).await;
                            }
                            MonitorOutcome::Healthy | MonitorOutcome::NoProcess => {}
                        }
                        this.mark_stable_if_due().await;
                    }
                    _ = resource_ticker.tick() => {
                        let _ = this.sample_resources().await;
                    }
                }
            }
        })
    }

    async fn terminate_running(&self, running: &mut RunningProcess) -> TerminationReport {
        let mut target =
            ProcessTerminationTarget::new(&mut running.process, Some(running.connection.as_ref()));
        let report = run_termination(&mut target, self.config.termination).await;
        let _ = self.ledger.lock().await.remove(&self.spec.manifest.id);
        report
    }

    async fn terminate_process(
        &self,
        process: &mut AdapterProcess,
        connection: Option<&Arc<AdapterConnection>>,
    ) -> TerminationReport {
        let mut target = ProcessTerminationTarget::new(process, connection.map(|c| c.as_ref()));
        let report = run_termination(&mut target, self.config.termination).await;
        let _ = self.ledger.lock().await.remove(&self.spec.manifest.id);
        report
    }

    fn fail_start_locked(
        &self,
        state: &mut SupervisorState,
        reason: DisabledReason,
        detail: String,
        stderr_tail: Vec<String>,
    ) -> StartOutcome {
        let _ = self.transition_locked(
            state,
            RuntimeStatus::Disabled,
            Some(reason),
            Some(detail.clone()),
        );
        StartOutcome::Failed {
            reason,
            detail,
            stderr_tail,
        }
    }

    fn transition_locked(
        &self,
        state: &mut SupervisorState,
        to: RuntimeStatus,
        reason: Option<DisabledReason>,
        detail: Option<String>,
    ) -> Result<StatusChange, SupervisorError> {
        let change = state
            .fsm
            .transition(to, reason, detail)
            .map_err(|error| SupervisorError::Transition(error.to_string()))?;
        if to == RuntimeStatus::Ready {
            state.ready_since_ms = Some(now_u64());
        }
        // M2-07：同步刷新 `health.runtimes` 摘要缓存（临界区仅赋值）。
        match self.summary.lock() {
            Ok(mut summary) => *summary = (change.to, change.reason),
            Err(poisoned) => *poisoned.into_inner() = (change.to, change.reason),
        }
        self.observer.on_status_changed(&change);
        Ok(change)
    }

    /// 状态转移事件类型（断言用：恒为附录 B `runtime.status_changed`）。
    pub fn status_event_type() -> EventType {
        EventType::RuntimeStatusChanged
    }
}

fn now_u64() -> u64 {
    u64::try_from(now_ms()).unwrap_or(0)
}

/// 在超时窗口内等待进程退出（启动期 EOF 先于进程状态可见的竞态兜底）。
async fn wait_process_exit(
    process: &mut AdapterProcess,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(status) = process.try_status() {
            return Some(status);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn classify_handshake_failure(disabled: &DisabledInfo) -> DisabledReason {
    // 非「启动期退出」类失败：保留 M1-09 词典取值
    //（version_mismatch / handshake_timeout / protocol_error）。
    disabled.status_reason
}

/// `initialize` 失败归因（M2-11/ADR-008 §3.4）。
///
/// 适配器侧版本门闩（DSH pin / 插件契约帧不匹配）经应用码 `1003 VERSION_MISMATCH`
/// 上报 → `disabled + version_mismatch`；其余一律 `start_failed`（不改变既有语义）。
fn classify_initialize_failure(error: &RequestError) -> DisabledReason {
    match error {
        RequestError::Rpc(rpc) if rpc.code == code::VERSION_MISMATCH => {
            DisabledReason::VersionMismatch
        }
        _ => DisabledReason::StartFailed,
    }
}

/// 生成 ULID 形状启动令牌（26 位 Crockford Base32；时间 48bit + pid 32bit + 计数 48bit）。
pub fn new_launch_token() -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let timestamp = u128::from(now_u64() & 0xFFFF_FFFF_FFFF);
    let pid = u128::from(u64::from(std::process::id()) & 0xFFFF_FFFF);
    let counter = u128::from(COUNTER.fetch_add(1, Ordering::Relaxed) & 0xFFFF_FFFF_FFFF);
    let value = (timestamp << 80) | (pid << 48) | counter;

    let mut output = [0_u8; 26];
    for (index, slot) in output.iter_mut().enumerate() {
        let shift = 125_u32.saturating_sub(5 * u32::try_from(index).unwrap_or(u32::MAX));
        let chunk = ((value >> shift) & 0x1F) as usize;
        *slot = ALPHABET[chunk];
    }
    String::from_utf8_lossy(&output).into_owned()
}

/// 系统进程树回收实现（台账三条件全命中后的清理；D5/评审 #5）。
#[derive(Debug, Default)]
pub struct SystemTreeKiller;

impl TreeKiller for SystemTreeKiller {
    fn kill_tree(&self, pid: u32) -> Result<(), String> {
        kill_tree_system(pid)
    }
}

/// 跨平台整树强杀（Windows `taskkill /T /F`；Unix `kill -KILL -<pgid>`）。
pub fn kill_tree_system(pid: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|error| format!("taskkill 不可用：{error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("taskkill /PID {pid} /T /F 退出码 {status}"))
        }
    }
    #[cfg(unix)]
    {
        // `--` 必须显式给出：GNU/BSD kill 会把 `-<pgid>` 误当选项解析，
        // 且 GNU kill 在该误解析下仍以退出码 0 结束（CI Linux 实测），会静默漏杀。
        let status = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .status()
            .map_err(|error| format!("kill 不可用：{error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("kill -KILL -- -{pid} 退出码 {status}"))
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Err("不支持的平台".to_owned())
    }
}

/// 监督器注册表（`runtime_retry` / `runtime_enable` 的命令层入口，DoD⑦）。
pub struct Supervisor {
    config: SupervisorConfig,
    observer: Arc<dyn SupervisorObserver>,
    policy: AdmissionPolicy,
    ledger: Arc<Mutex<AdapterLedger>>,
    probe: Arc<dyn ProcessProbe>,
    killer: Arc<dyn TreeKiller>,
    runtimes: BTreeMap<String, Arc<RuntimeSupervisor>>,
}

impl Supervisor {
    /// 组装注册表（共享台账/探针/回收器）；`runtime_id` 必须非空且唯一。
    pub fn new(
        specs: Vec<RuntimeSpec>,
        config: SupervisorConfig,
        policy: AdmissionPolicy,
        observer: Arc<dyn SupervisorObserver>,
        ledger: Arc<Mutex<AdapterLedger>>,
        probe: Arc<dyn ProcessProbe>,
        killer: Arc<dyn TreeKiller>,
    ) -> Result<Self, SupervisorError> {
        let mut runtimes = BTreeMap::new();
        for spec in specs {
            let raw_id = spec.manifest.id.clone();
            let runtime_id = RuntimeId::new(raw_id.clone())
                .map_err(|_| SupervisorError::InvalidRuntimeId(raw_id.clone()))?;
            if runtimes.contains_key(&raw_id) {
                return Err(SupervisorError::InvalidRuntimeId(format!(
                    "{raw_id}（重复注册）"
                )));
            }
            runtimes.insert(
                raw_id,
                Arc::new(RuntimeSupervisor::new(
                    runtime_id,
                    spec,
                    config.clone(),
                    Arc::clone(&observer),
                    Arc::clone(&ledger),
                    Arc::clone(&probe),
                )),
            );
        }
        Ok(Self {
            config,
            observer,
            policy,
            ledger,
            probe,
            killer,
            runtimes,
        })
    }

    /// 启动清理（D5：孤儿进程台账三条件；D2 启动序列的孤儿清理步骤）。
    ///
    /// 三条件全命中 → 整树回收；PID 已不存在 → 移除陈旧记录；其余仅记录（绝不 kill）。
    pub async fn cleanup_orphans(&self) -> Result<CleanupReport, SupervisorError> {
        let mut ledger = self.ledger.lock().await;
        let report = ledger
            .cleanup(self.probe.as_ref(), self.killer.as_ref())
            .map_err(|error| SupervisorError::Ledger(error.to_string()))?;
        for action in &report.actions {
            let kind = if action.killed {
                AuditKind::LedgerReclaimed
            } else if matches!(action.verdict, LedgerVerdict::Stale) {
                AuditKind::LedgerStaleRecord
            } else {
                AuditKind::LedgerSkipped
            };
            if let Ok(runtime_id) = RuntimeId::new(action.adapter_id.clone()) {
                self.observer.on_audit(&AuditRecord::new(
                    runtime_id,
                    kind,
                    action.detail.clone(),
                    now_ms(),
                ));
            }
        }
        Ok(report)
    }

    pub fn config(&self) -> &SupervisorConfig {
        &self.config
    }

    /// 白名单查询（`runtime_id` 必须存在于注册表）。
    pub fn get(&self, runtime_id: &str) -> Option<Arc<RuntimeSupervisor>> {
        self.runtimes.get(runtime_id).cloned()
    }

    pub fn runtime_ids(&self) -> Vec<String> {
        self.runtimes.keys().cloned().collect()
    }

    /// 预热：应用启动即启动全部 `enabled` 适配器并完成 `initialize`（D5）。
    pub async fn warmup_all(&self) -> Vec<(String, StartOutcome)> {
        let mut outcomes = Vec::new();
        let mut ready = Vec::new();
        for (id, runtime) in &self.runtimes {
            let manifest = runtime.manifest();
            if !manifest.enabled {
                continue;
            }
            match self.policy.admit(manifest) {
                AdmissionDecision::Admitted => ready.push((id.clone(), Arc::clone(runtime))),
                AdmissionDecision::Untrusted { detail } => {
                    let outcome = runtime.reject_untrusted(detail).await;
                    outcomes.push((id.clone(), outcome));
                }
            }
        }
        // 并行预热（会话创建 <2s 关键路径）。
        let mut handles = Vec::new();
        for (id, runtime) in ready {
            handles.push((id, tokio::spawn(async move { runtime.start().await })));
        }
        for (id, handle) in handles {
            let outcome = match handle.await {
                Ok(outcome) => outcome,
                Err(_) => StartOutcome::Failed {
                    reason: DisabledReason::StartFailed,
                    detail: "预热任务 panic（JoinError）".to_owned(),
                    stderr_tail: Vec::new(),
                },
            };
            outcomes.push((id, outcome));
        }
        outcomes
    }

    /// `runtime_retry`（ADR-004）：`runtime_id` 白名单 + 仅 `disabled + start_failed`；
    /// 转移 `disabled → cold → starting`。
    pub async fn runtime_retry(&self, runtime_id: &str) -> Result<StartOutcome, SupervisorError> {
        let runtime = self
            .get(runtime_id)
            .ok_or_else(|| SupervisorError::UnknownRuntime(runtime_id.to_owned()))?;
        runtime.begin_retry().await?;
        Ok(runtime.start().await)
    }

    /// `runtime_enable`（ADR-004）：`runtime_id` 白名单 + 仅 `disabled`；
    /// `untrusted` / `version_mismatch` 必须先修复清单/版本（禁止直接启用）。
    pub async fn runtime_enable(&self, runtime_id: &str) -> Result<StartOutcome, SupervisorError> {
        let runtime = self
            .get(runtime_id)
            .ok_or_else(|| SupervisorError::UnknownRuntime(runtime_id.to_owned()))?;
        runtime.begin_enable().await?;
        Ok(runtime.start().await)
    }

    /// 应用关闭：终止全部运行时并清台账。
    pub async fn shutdown_all(&self) {
        for runtime in self.runtimes.values() {
            let _ = runtime.shutdown().await;
        }
    }

    /// 观察者（测试/诊断）。
    pub fn observer(&self) -> &Arc<dyn SupervisorObserver> {
        &self.observer
    }
}

impl RuntimeSupervisor {
    /// `runtime_retry` 的状态校验与 `disabled → cold` 转移（DoD⑦）。
    pub async fn begin_retry(&self) -> Result<(), SupervisorError> {
        let mut state = self.state.lock().await;
        let status = state.fsm.status();
        let reason = state.fsm.status_reason();
        if status != RuntimeStatus::Disabled || reason != Some(DisabledReason::StartFailed) {
            return Err(SupervisorError::RetryNotAllowed { status, reason });
        }
        self.transition_locked(&mut state, RuntimeStatus::Cold, None, None)?;
        state.restart.reset();
        Ok(())
    }

    /// `runtime_enable` 的状态校验与 `disabled → cold` 转移（DoD⑦）。
    pub async fn begin_enable(&self) -> Result<(), SupervisorError> {
        let mut state = self.state.lock().await;
        let status = state.fsm.status();
        if status != RuntimeStatus::Disabled {
            return Err(SupervisorError::EnableNotAllowed { status });
        }
        let reason = state.fsm.status_reason();
        if let Some(reason) = reason {
            if matches!(
                reason,
                DisabledReason::Untrusted | DisabledReason::VersionMismatch
            ) {
                return Err(SupervisorError::NeedsRemedy { reason });
            }
        }
        self.transition_locked(&mut state, RuntimeStatus::Cold, None, None)?;
        state.restart.reset();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::ledger::SysinfoProbe;
    use crate::supervisor::state::NoopObserver;

    fn test_supervisor(program: &str) -> (RuntimeSupervisor, Arc<Mutex<AdapterLedger>>) {
        let path = std::env::temp_dir()
            .join("aether-m1-10-unit")
            .join(format!("ledger-{}.json", std::process::id()));
        let ledger = Arc::new(Mutex::new(AdapterLedger::load(&path).unwrap()));
        let spec = RuntimeSpec::new(
            RuntimeManifest::new("mock", "Mock", program),
            new_launch_token(),
        );
        let supervisor = RuntimeSupervisor::new(
            RuntimeId::new("mock").unwrap(),
            spec,
            SupervisorConfig::d5(),
            Arc::new(NoopObserver),
            Arc::clone(&ledger),
            Arc::new(SysinfoProbe::new()),
        );
        (supervisor, ledger)
    }

    #[test]
    fn launch_token_is_ulid_shaped_and_unique() {
        let first = new_launch_token();
        let second = new_launch_token();
        assert_eq!(first.len(), 26);
        assert_ne!(first, second);
        assert!(first
            .chars()
            .all(|ch: char| ch.is_ascii_digit() || ch.is_ascii_uppercase()));
        assert!(
            !first.contains(['I', 'L', 'O', 'U']),
            "Crockford 无 I/L/O/U：{first}"
        );
    }

    #[test]
    fn config_defaults_are_d5() {
        let config = SupervisorConfig::d5();
        assert_eq!(config.handshake_timeout, Duration::from_secs(10));
        assert_eq!(config.initialize_timeout, Duration::from_secs(10));
        assert_eq!(config.heartbeat.interval, Duration::from_secs(10));
        assert_eq!(config.heartbeat.timeout, Duration::from_secs(5));
        assert_eq!(config.heartbeat.max_consecutive_failures, 3);
        assert_eq!(config.termination, TerminationBudget::d5());
        assert!(config.backoff_override.is_none());
        assert_eq!(SupervisorConfig::default(), config);
    }

    #[test]
    fn status_event_type_is_appendix_b() {
        assert_eq!(
            RuntimeSupervisor::status_event_type(),
            EventType::RuntimeStatusChanged
        );
        assert_eq!(
            RuntimeSupervisor::status_event_type().as_str(),
            "runtime.status_changed"
        );
    }

    /// M2-07：`health.runtimes` 摘要同步快照与状态机一一对应（含 `status_reason`）。
    #[tokio::test(flavor = "current_thread")]
    async fn summary_snapshot_tracks_state_core_transitions() {
        let (supervisor, _ledger) = test_supervisor("aether-test-nonexistent-program");
        assert_eq!(
            supervisor.summary_snapshot(),
            (RuntimeStatus::Cold, None),
            "初始与 DDL 默认（cold）一致"
        );
        assert_eq!(
            supervisor.summary_snapshot(),
            (supervisor.status().await, supervisor.status_reason().await)
        );

        // 非官方 manifest → disabled + untrusted；快照必须同步。
        let outcome = supervisor
            .reject_untrusted("非官方 manifest（M2-07 单测）".to_owned())
            .await;
        assert!(matches!(outcome, StartOutcome::Rejected { .. }));
        let (status, reason) = supervisor.summary_snapshot();
        assert_eq!(status, RuntimeStatus::Disabled);
        assert_eq!(reason, Some(DisabledReason::Untrusted));
        assert_eq!(status, supervisor.status().await);
        assert_eq!(reason, supervisor.status_reason().await);
    }

    #[test]
    fn handshake_failure_classification_maps_dictionary() {
        assert_eq!(
            classify_handshake_failure(&DisabledInfo::version_mismatch("2.0")),
            DisabledReason::VersionMismatch
        );
        assert_eq!(
            classify_handshake_failure(&DisabledInfo::handshake_timeout(Duration::from_secs(10))),
            DisabledReason::HandshakeTimeout
        );
        assert_eq!(
            classify_handshake_failure(&DisabledInfo::protocol_error("bad")),
            DisabledReason::ProtocolError
        );
    }

    /// M2-11/ADR-008 §3.4：`initialize` 返回应用码 1003 → `version_mismatch`；其余 `start_failed`。
    #[test]
    fn initialize_failure_classification_maps_version_mismatch_only() {
        let version_mismatch = RequestError::Rpc(crate::connection::RpcError {
            code: code::VERSION_MISMATCH,
            message: "DSH 版本门闩失败：pin 0.1.5-rc.2，实际 0.1.1-rc.2".to_owned(),
            data: None,
        });
        assert_eq!(
            classify_initialize_failure(&version_mismatch),
            DisabledReason::VersionMismatch
        );
        let other = RequestError::Rpc(crate::connection::RpcError {
            code: -32603,
            message: "initialize 内部错误".to_owned(),
            data: None,
        });
        assert_eq!(
            classify_initialize_failure(&other),
            DisabledReason::StartFailed
        );
        let timeout = RequestError::Timeout {
            method: Method::Initialize,
            timeout: Duration::from_secs(10),
        };
        assert_eq!(
            classify_initialize_failure(&timeout),
            DisabledReason::StartFailed
        );
    }

    #[tokio::test]
    async fn retry_requires_disabled_start_failed() {
        let (supervisor, _ledger) = test_supervisor("missing-binary");
        let error = supervisor.begin_retry().await.unwrap_err();
        assert!(matches!(error, SupervisorError::RetryNotAllowed { .. }));
        let error = supervisor.begin_enable().await.unwrap_err();
        assert!(matches!(error, SupervisorError::EnableNotAllowed { .. }));
    }

    #[tokio::test]
    async fn retry_from_disabled_start_failed_transitions_cold_then_starting() {
        let (supervisor, _ledger) = test_supervisor("missing-binary");
        {
            let mut state = supervisor.state.lock().await;
            let _ = supervisor.transition_locked(&mut state, RuntimeStatus::Starting, None, None);
            let _ = supervisor.transition_locked(
                &mut state,
                RuntimeStatus::Disabled,
                Some(DisabledReason::StartFailed),
                None,
            );
        }
        supervisor.begin_retry().await.unwrap();
        assert_eq!(supervisor.status().await, RuntimeStatus::Cold);
        let outcome = supervisor.start().await;
        assert!(matches!(outcome, StartOutcome::Failed { .. }));
        assert_eq!(supervisor.status().await, RuntimeStatus::Disabled);
        assert_eq!(
            supervisor.status_reason().await,
            Some(DisabledReason::StartFailed)
        );
    }

    #[tokio::test]
    async fn enable_blocks_untrusted_until_remedied() {
        let (supervisor, _ledger) = test_supervisor("missing-binary");
        let outcome = supervisor
            .reject_untrusted("第三方 manifest".to_owned())
            .await;
        assert!(matches!(outcome, StartOutcome::Rejected { .. }));
        assert_eq!(supervisor.status().await, RuntimeStatus::Disabled);
        let error = supervisor.begin_enable().await.unwrap_err();
        assert_eq!(
            error,
            SupervisorError::NeedsRemedy {
                reason: DisabledReason::Untrusted
            }
        );
    }

    #[tokio::test]
    async fn enable_blocks_version_mismatch_and_retry_rejects_it() {
        let (supervisor, _ledger) = test_supervisor("missing-binary");
        {
            let mut state = supervisor.state.lock().await;
            let _ = supervisor.transition_locked(&mut state, RuntimeStatus::Starting, None, None);
            let _ = supervisor.transition_locked(
                &mut state,
                RuntimeStatus::Disabled,
                Some(DisabledReason::VersionMismatch),
                None,
            );
        }
        let error = supervisor.begin_enable().await.unwrap_err();
        assert_eq!(
            error,
            SupervisorError::NeedsRemedy {
                reason: DisabledReason::VersionMismatch
            }
        );
        // retry 仅限 start_failed：version_mismatch 同样被拒。
        let error = supervisor.begin_retry().await.unwrap_err();
        assert!(matches!(error, SupervisorError::RetryNotAllowed { .. }));
    }

    #[tokio::test]
    async fn enable_from_crash_loop_transitions_to_cold() {
        let (supervisor, _ledger) = test_supervisor("missing-binary");
        {
            let mut state = supervisor.state.lock().await;
            let _ = supervisor.transition_locked(&mut state, RuntimeStatus::Starting, None, None);
            let _ = supervisor.transition_locked(
                &mut state,
                RuntimeStatus::Disabled,
                Some(DisabledReason::CrashLoop),
                None,
            );
        }
        supervisor.begin_enable().await.unwrap();
        assert_eq!(supervisor.status().await, RuntimeStatus::Cold);
    }

    #[test]
    fn env_artifacts_dir_constant_is_frozen() {
        assert_eq!(ENV_ARTIFACTS_DIR, "AETHER_ARTIFACTS_DIR");
    }
}
