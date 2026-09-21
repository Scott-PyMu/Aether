//! M2-04 DoD⑤（适配器侧）：存储侧背压隔离的监督器语义（D5/D8、ADR-003/ADR-004）。
//!
//! 覆盖：
//! - `isolate(storage_backpressure)`：`ready → degraded + status_reason=storage_backpressure`，
//!   终止进程（停止事件生产）且不自动重启；广播 `runtime.status_changed` + 审计；
//! - 重复隔离/冷启动隔离 → `NotApplicable`（不修改现状）；
//! - `release()`：`degraded → starting → ready` 自动解除并重启（新 PID）；
//! - `release()` 在 `ready` 上 → `NotApplicable`。
//!
//! 运行：`cargo test -p aether-adapters --test m2_04_isolation`（夹具 bin 由 Cargo 提供）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aether_adapters::supervisor::{
    AdapterLedger, AdmissionPolicy, AuditKind, HeartbeatConfig, IsolationOutcome, ReleaseOutcome,
    RuntimeManifest, RuntimeSpec, StartOutcome, Supervisor, SupervisorConfig, SysinfoProbe,
    SystemTreeKiller, TerminationBudget,
};
use aether_adapters::DisabledReason;
use aether_core::RuntimeStatus;
use common::{fixture_binary, unique_temp_dir, RecordingObserver};

fn manifest(args: &[&str]) -> RuntimeManifest {
    RuntimeManifest::new("mock", "Mock Fixture", fixture_binary())
        .official(true)
        .with_args(args.iter().map(|arg| (*arg).to_string()))
}

/// 测试配置：常量级压缩（终止序列每步 ≤1s），生产默认见 `SupervisorConfig::d5`。
fn test_config() -> SupervisorConfig {
    SupervisorConfig {
        handshake_timeout: Duration::from_secs(3),
        initialize_timeout: Duration::from_secs(3),
        heartbeat: HeartbeatConfig {
            interval: Duration::from_millis(200),
            timeout: Duration::from_millis(150),
            max_consecutive_failures: 3,
        },
        termination: TerminationBudget {
            shutdown_rpc: Duration::from_millis(300),
            graceful: Duration::from_millis(1_000),
            force: Duration::from_millis(1_000),
            fallback: Duration::from_millis(1_000),
        },
        backoff_override: Some(Duration::from_millis(20)),
    }
}

struct Harness {
    supervisor: Supervisor,
    observer: Arc<RecordingObserver>,
}

fn harness(specs: Vec<RuntimeSpec>) -> Harness {
    let dir = unique_temp_dir("m2-04-isolation");
    let ledger = Arc::new(tokio::sync::Mutex::new(
        AdapterLedger::load(dir.join("adapters.json")).unwrap(),
    ));
    let observer = Arc::new(RecordingObserver::new());
    let supervisor = Supervisor::new(
        specs,
        test_config(),
        AdmissionPolicy::official(),
        observer.clone(),
        ledger,
        Arc::new(SysinfoProbe::new()),
        Arc::new(SystemTreeKiller),
    )
    .unwrap();
    Harness {
        supervisor,
        observer,
    }
}

#[tokio::test]
async fn isolate_sets_degraded_storage_backpressure_and_release_restarts() {
    // `deaf`：initialize 正常应答（预热可达 Ready）；shutdown RPC 不响应 → 终止序列走
    // 优雅/强杀路径（压缩预算 ≤1s/步）。
    let spec = RuntimeSpec::with_fresh_token(manifest(&["--mode", "deaf", "--seconds", "120"]));
    let h = harness(vec![spec]);

    let outcomes = h.supervisor.warmup_all().await;
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].1, StartOutcome::Ready, "预热必须 Ready");
    let runtime = h.supervisor.get("mock").expect("白名单命中");
    let pid_before = runtime.current_pid().await.expect("运行中 PID");
    assert!(runtime.is_running().await);

    // 隔离：Ready → Degraded(storage_backpressure) + 进程终止。
    let outcome = runtime
        .isolate(
            DisabledReason::StorageBackpressure,
            "写队列临时高水位 >4096",
        )
        .await;
    assert_eq!(
        outcome,
        IsolationOutcome::Isolated {
            status: RuntimeStatus::Degraded
        }
    );
    assert_eq!(runtime.status().await, RuntimeStatus::Degraded);
    assert_eq!(
        runtime.status_reason().await,
        Some(DisabledReason::StorageBackpressure),
        "status_reason 必须为 storage_backpressure（D8）"
    );
    assert!(
        !runtime.is_running().await,
        "隔离必须终止进程（停止事件生产）"
    );
    assert!(
        !common::pid_alive(pid_before),
        "隔离后原进程必须已退出（PID {pid_before}）"
    );

    // 状态转移广播（附录 B）与审计。
    let changes = h.observer.snapshot().status_changes;
    let last = changes.last().expect("必须产生状态转移");
    assert_eq!(last.from, RuntimeStatus::Ready);
    assert_eq!(last.to, RuntimeStatus::Degraded);
    assert_eq!(
        last.payload().reason.as_deref(),
        Some("storage_backpressure")
    );
    assert_eq!(last.event_type().as_str(), "runtime.status_changed");
    assert!(
        h.observer.has_audit(AuditKind::Terminated),
        "隔离终止必须审计"
    );

    // 重复隔离：已隔离 → NotApplicable（不修改现状）。
    let again = runtime
        .isolate(DisabledReason::StorageBackpressure, "重复请求")
        .await;
    assert_eq!(
        again,
        IsolationOutcome::NotApplicable {
            status: RuntimeStatus::Degraded,
            reason: Some(DisabledReason::StorageBackpressure)
        }
    );

    // 解除：degraded → starting → ready（自动重启，新 PID）。
    let released = runtime.release().await;
    assert_eq!(released, ReleaseOutcome::Released);
    assert_eq!(runtime.status().await, RuntimeStatus::Ready);
    assert_eq!(runtime.status_reason().await, None, "ready 必须清除原因");
    assert!(runtime.is_running().await);
    let pid_after = runtime.current_pid().await.expect("重启后 PID");
    assert_ne!(pid_after, pid_before, "解除必须重启为新进程");

    // ready 上重复解除 → NotApplicable。
    assert_eq!(
        runtime.release().await,
        ReleaseOutcome::NotApplicable {
            status: RuntimeStatus::Ready
        }
    );

    runtime.shutdown().await;
}

#[tokio::test]
async fn isolate_on_cold_runtime_is_not_applicable() {
    let spec = RuntimeSpec::with_fresh_token(manifest(&["--mode", "deaf", "--seconds", "120"]));
    let h = harness(vec![spec]);
    let runtime = h.supervisor.get("mock").expect("白名单命中");
    assert_eq!(runtime.status().await, RuntimeStatus::Cold);

    let outcome = runtime
        .isolate(DisabledReason::StorageBackpressure, "未启动")
        .await;
    assert_eq!(
        outcome,
        IsolationOutcome::NotApplicable {
            status: RuntimeStatus::Cold,
            reason: None
        }
    );
    assert_eq!(
        runtime.status().await,
        RuntimeStatus::Cold,
        "现状不得被修改"
    );
    assert!(
        h.observer.snapshot().status_changes.is_empty(),
        "不适用不得产生状态转移"
    );
}
