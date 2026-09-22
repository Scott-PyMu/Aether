//! M1-10 适配器监督器（D5、评审 #3/#4/#5）。
//!
//! 模块划分：
//! - [`state`]：D5 状态机（与 `runtimes.status/status_reason` 一一对应）与观察者出口；
//! - [`backoff`]：退避曲线 1/2/4/8/16/30s 与 60s ≥5 次崩溃熔断；
//! - [`heartbeat`]：10s / 5s / 连续 3 次失败计数；
//! - [`admission`]：官方白名单准入（非官方 → `untrusted` + 审计）；
//! - [`ledger`]：PID 台账三条件（存活 + 启动时间 + `launch_token`）；
//! - [`termination`]：D5 终止序列（逐步硬超时 + 平台机制断言）；
//! - [`resources`]：5s 采样与 RSS/CPU 持续超限告警；
//! - [`runtime`]：上述能力的编排（预热、心跳、重启、`runtime_retry`/`runtime_enable`）；
//! - [`clock`]：可注入时钟（退避/持续超限单测）。

pub mod admission;
pub mod backoff;
pub mod clock;
pub mod heartbeat;
pub mod ledger;
pub mod resources;
pub mod runtime;
pub mod state;
pub mod termination;

pub use admission::{AdmissionDecision, AdmissionPolicy, RuntimeManifest, OFFICIAL_RUNTIME_IDS};
pub use backoff::{
    CrashDecision, RestartPolicy, CRASH_THRESHOLD, CRASH_WINDOW, RESTART_BACKOFF_SECONDS,
};
pub use clock::{ManualTime, SharedClock, SystemTimeSource, TimeSource};
pub use heartbeat::{
    HeartbeatConfig, HeartbeatMonitor, HeartbeatVerdict, HEARTBEAT_INTERVAL,
    HEARTBEAT_MAX_CONSECUTIVE_FAILURES, HEARTBEAT_TIMEOUT,
};
pub use ledger::{
    cmdline_hash, default_ledger_path, evaluate, AdapterLedger, CleanupReport, LedgerAction,
    LedgerError, LedgerRecord, LedgerVerdict, ProcessFacts, ProcessProbe, SysinfoProbe, TreeKiller,
    LEDGER_FILE_NAME, LEDGER_RELATIVE_PATH,
};
pub use resources::{
    ResourceConfig, ResourceMonitor, ResourceObservation, ResourceSample, SysinfoSampler,
    ENV_CPU_THRESHOLD_PCT, ENV_RSS_THRESHOLD_MB, ENV_SUSTAIN_SECS, RESOURCE_BREACH_SUSTAIN,
    RESOURCE_CPU_LIMIT_PERCENT, RESOURCE_RSS_LIMIT_BYTES, RESOURCE_SAMPLE_INTERVAL,
};
pub use runtime::{
    kill_tree_system, new_launch_token, IsolationOutcome, MonitorOutcome, ReleaseOutcome,
    RestartOutcome, RuntimeSpec, RuntimeSupervisor, StartOutcome, Supervisor, SupervisorConfig,
    SupervisorError, SystemTreeKiller, ENV_ARTIFACTS_DIR, INITIALIZE_TIMEOUT,
};
pub use state::{
    now_ms, AuditKind, AuditRecord, NoopObserver, ResourceEvent, ResourceLimitKind, StateCore,
    StatusChange, SupervisorObserver, TransitionError,
};
pub use termination::{
    run_termination, ActionFuture, StepOutcome, StepResult, TerminationBudget, TerminationReport,
    TerminationStep, TerminationTarget, ALIVE_POLL_INTERVAL, FALLBACK_TIMEOUT, FORCE_TIMEOUT,
    GRACEFUL_TIMEOUT, SHUTDOWN_RPC_TIMEOUT,
};
