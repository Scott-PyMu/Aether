//! M1-10 监督器集成：状态机断言、预热、心跳重启、启动即崩（stderr 尾 50 行）、
//! 准入拒绝、退避/熔断与 `runtime_retry`/`runtime_enable` 命令语义。
//!
//! 覆盖 DoD：①（状态转移与 `runtime.status_changed`）、②（退避/熔断，曲线单测在
//! `supervisor::backoff`）、⑤（untrusted + 审计）、⑥（stderr 尾 50 行）、
//! ⑦（retry/enable 参数与状态校验、`disabled → cold → starting`）。
//!
//! 运行：`cargo test -p aether-adapters --test m1_10_supervisor`（夹具 bin 由 Cargo 提供）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aether_adapters::supervisor::{
    kill_tree_system, AdapterLedger, AdmissionPolicy, AuditKind, HeartbeatConfig, MonitorOutcome,
    RestartOutcome, RuntimeManifest, RuntimeSpec, StartOutcome, Supervisor, SupervisorConfig,
    SupervisorError, SysinfoProbe, SystemTreeKiller, TerminationBudget,
};
use aether_adapters::DisabledReason;
use aether_core::RuntimeStatus;
use common::{fixture_binary, unique_temp_dir, ObserverLog, RecordingObserver};

fn manifest(args: &[&str]) -> RuntimeManifest {
    RuntimeManifest::new("mock", "Mock Fixture", fixture_binary())
        .official(true)
        .with_args(args.iter().map(|arg| (*arg).to_string()))
}

/// 测试配置：常量级压缩（心跳 200ms/150ms、退避 20ms），生产默认见 `SupervisorConfig::d5`。
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
    ledger: Arc<tokio::sync::Mutex<AdapterLedger>>,
    observer: Arc<RecordingObserver>,
}

fn harness(specs: Vec<RuntimeSpec>) -> Harness {
    let dir = unique_temp_dir("supervisor");
    let ledger = Arc::new(tokio::sync::Mutex::new(
        AdapterLedger::load(dir.join("adapters.json")).unwrap(),
    ));
    let observer = Arc::new(RecordingObserver::new());
    let supervisor = Supervisor::new(
        specs,
        test_config(),
        AdmissionPolicy::official(),
        observer.clone(),
        Arc::clone(&ledger),
        Arc::new(SysinfoProbe::new()),
        Arc::new(SystemTreeKiller),
    )
    .unwrap();
    Harness {
        supervisor,
        ledger,
        observer,
    }
}

// ===== DoD①：预热、状态机转移、runtime.status_changed、台账登记 =====

#[tokio::test]
async fn warmup_ready_transitions_and_ledger_record() {
    let spec = RuntimeSpec::with_fresh_token(manifest(&["--mode", "deaf", "--seconds", "120"]));
    let h = harness(vec![spec.clone()]);

    let outcomes = h.supervisor.warmup_all().await;
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].0, "mock");
    assert_eq!(
        outcomes[0].1,
        StartOutcome::Ready,
        "预热必须完成握手 + initialize：{:?}",
        outcomes[0].1
    );

    let runtime = h.supervisor.get("mock").expect("注册表命中白名单");
    assert_eq!(runtime.status().await, RuntimeStatus::Ready);
    assert_eq!(
        h.observer.transitions(),
        vec![
            (RuntimeStatus::Cold, RuntimeStatus::Starting),
            (RuntimeStatus::Starting, RuntimeStatus::Ready),
        ]
    );
    assert_eq!(h.observer.reasons(), vec![None, None]);
    // 每次转移均广播 `runtime.status_changed`（附录 B）。
    for change in h.observer.snapshot().status_changes {
        assert_eq!(change.event_type().as_str(), "runtime.status_changed");
        assert_eq!(change.payload().runtime_id.as_str(), "mock");
    }

    // 台账登记：pid 与运行进程一致、令牌与规格一致、启动时间非零、命令行哈希存在。
    let record = h
        .ledger
        .lock()
        .await
        .get("mock")
        .cloned()
        .expect("台账记录");
    assert_eq!(Some(record.pid), runtime.current_pid().await);
    assert_eq!(record.launch_token, spec.launch_token);
    assert!(record.start_time_epoch > 0);
    assert_eq!(record.cmdline_hash.len(), 16);
    assert!(record.cmdline_hash.chars().all(|ch| ch.is_ascii_hexdigit()));

    h.supervisor.shutdown_all().await;
    assert!(
        h.ledger.lock().await.get("mock").is_none(),
        "关闭后台账必须清理"
    );
    assert!(!runtime.is_running().await);
}

// ===== DoD①/②：心跳连续 3 次失败 → degraded → 自动重启 → ready =====

#[tokio::test]
async fn heartbeat_failure_restarts_to_ready() {
    let spec = RuntimeSpec::with_fresh_token(manifest(&["--mode", "deaf", "--seconds", "120"]));
    let h = harness(vec![spec]);
    h.supervisor.warmup_all().await;
    let runtime = h.supervisor.get("mock").expect("注册表命中");
    let first_pid = runtime.current_pid().await.expect("运行中 pid");

    // deaf 夹具不响应 health.ping：连续 3 次失败（阈值 3）。
    let mut verdict = None;
    for _ in 0..3 {
        verdict = Some(runtime.monitor_once().await);
    }
    assert_eq!(
        verdict,
        Some(MonitorOutcome::Unhealthy {
            consecutive_failures: 3
        })
    );

    let outcome = runtime
        .restart(
            DisabledReason::HeartbeatFailed,
            "集成测试：连续 3 次心跳失败",
        )
        .await;
    assert_eq!(outcome, RestartOutcome::Ready);
    assert_eq!(runtime.status().await, RuntimeStatus::Ready);
    assert_eq!(
        h.observer.transitions(),
        vec![
            (RuntimeStatus::Cold, RuntimeStatus::Starting),
            (RuntimeStatus::Starting, RuntimeStatus::Ready),
            (RuntimeStatus::Ready, RuntimeStatus::Degraded),
            (RuntimeStatus::Degraded, RuntimeStatus::Starting),
            (RuntimeStatus::Starting, RuntimeStatus::Ready),
        ]
    );
    assert_eq!(
        h.observer.reasons()[2].as_deref(),
        Some("heartbeat_failed"),
        "ready→degraded 必须携带 status_reason=heartbeat_failed（D5 恢复转移）"
    );
    let second_pid = runtime.current_pid().await.expect("重启后 pid");
    assert_ne!(first_pid, second_pid, "重启必须是新进程");
    assert!(h.observer.has_audit(AuditKind::Terminated));
    h.supervisor.shutdown_all().await;
}

// ===== DoD①/失败表：运行中崩溃 → degraded(crashed) → ready =====

#[tokio::test]
async fn process_exit_triggers_crashed_restart() {
    let spec = RuntimeSpec::with_fresh_token(manifest(&["--mode", "deaf", "--seconds", "120"]));
    let h = harness(vec![spec]);
    h.supervisor.warmup_all().await;
    let runtime = h.supervisor.get("mock").expect("注册表命中");
    let pid = runtime.current_pid().await.expect("运行中 pid");

    // 外部强杀（模拟 T5a 崩溃注入）。
    kill_tree_system(pid).expect("强杀注入");
    let observed = common::wait_for_async(
        || async {
            matches!(
                runtime.monitor_once().await,
                MonitorOutcome::ProcessExited { .. }
            )
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(observed, "监控必须观测到进程退出");

    let outcome = runtime
        .restart(DisabledReason::Crashed, "集成测试：外部强杀")
        .await;
    assert_eq!(outcome, RestartOutcome::Ready);
    let reasons = h.observer.reasons();
    assert!(reasons.contains(&Some("crashed".to_owned())), "{reasons:?}");
    h.supervisor.shutdown_all().await;
}

// ===== DoD⑥：启动即崩 → stderr 尾 50 行 + disabled + start_failed =====

#[tokio::test]
async fn start_failure_captures_stderr_tail_of_50_lines() {
    let spec =
        RuntimeSpec::with_fresh_token(manifest(&["--mode", "stderr-crash", "--lines", "60"]));
    let h = harness(vec![spec]);

    let outcomes = h.supervisor.warmup_all().await;
    match &outcomes[0].1 {
        StartOutcome::Failed {
            reason,
            stderr_tail,
            ..
        } => {
            assert_eq!(*reason, DisabledReason::StartFailed);
            assert_eq!(stderr_tail.len(), 50, "stderr 尾必须截取 50 行");
            assert_eq!(
                stderr_tail.first().map(String::as_str),
                Some("#10 fixture stderr line")
            );
            assert_eq!(
                stderr_tail.last().map(String::as_str),
                Some("#59 fixture stderr line")
            );
        }
        other => panic!("启动即崩必须为 Failed，实际 {other:?}"),
    }

    let runtime = h.supervisor.get("mock").expect("注册表命中");
    assert_eq!(runtime.status().await, RuntimeStatus::Disabled);
    assert_eq!(
        runtime.status_reason().await,
        Some(DisabledReason::StartFailed)
    );
    assert_eq!(
        h.observer.transitions(),
        vec![
            (RuntimeStatus::Cold, RuntimeStatus::Starting),
            (RuntimeStatus::Starting, RuntimeStatus::Disabled),
        ]
    );
    assert_eq!(h.observer.reasons()[1].as_deref(), Some("start_failed"));
    assert!(
        h.ledger.lock().await.get("mock").is_none(),
        "失败后台账必须清理"
    );
}

// ===== DoD⑤：非官方 manifest → disabled + untrusted + 审计 =====

#[tokio::test]
async fn untrusted_manifest_is_disabled_with_audit() {
    let third_party = RuntimeManifest::new("third-party", "第三方", fixture_binary()).with_args([
        "--mode",
        "deaf",
        "--seconds",
        "120",
    ]);
    let h = harness(vec![RuntimeSpec::with_fresh_token(third_party)]);

    let outcomes = h.supervisor.warmup_all().await;
    assert!(matches!(outcomes[0].1, StartOutcome::Rejected { .. }));

    let runtime = h.supervisor.get("third-party").expect("注册表命中");
    assert_eq!(runtime.status().await, RuntimeStatus::Disabled);
    assert_eq!(
        runtime.status_reason().await,
        Some(DisabledReason::Untrusted)
    );
    assert!(h.observer.has_audit(AuditKind::AdmissionRejected));
    assert!(
        h.ledger.lock().await.get("third-party").is_none(),
        "拒绝加载不得登记台账"
    );
    // 禁止直接启用（未修复清单/版本）。
    let error = h
        .supervisor
        .runtime_enable("third-party")
        .await
        .unwrap_err();
    assert_eq!(
        error,
        SupervisorError::NeedsRemedy {
            reason: DisabledReason::Untrusted
        }
    );
}

// ===== DoD②/⑦：熔断 crash_loop（60s ≥5 次）→ runtime_enable 恢复 =====

#[tokio::test]
async fn crash_loop_circuit_breaker_then_enable_recovers() {
    let spec = RuntimeSpec::with_fresh_token(manifest(&["--mode", "deaf", "--seconds", "120"]));
    let h = harness(vec![spec]);
    h.supervisor.warmup_all().await;
    let runtime = h.supervisor.get("mock").expect("注册表命中");

    let mut last = None;
    for cycle in 0..5 {
        let pid = runtime.current_pid().await.expect("运行中 pid");
        kill_tree_system(pid).expect("崩溃注入");
        let observed = common::wait_for_async(
            || async {
                matches!(
                    runtime.monitor_once().await,
                    MonitorOutcome::ProcessExited { .. }
                )
            },
            Duration::from_secs(10),
        )
        .await;
        assert!(observed, "第 {} 次崩溃未被观测", cycle + 1);
        last = Some(runtime.restart(DisabledReason::Crashed, "熔断测试").await);
        if cycle < 4 {
            assert_eq!(
                last,
                Some(RestartOutcome::Ready),
                "第 {} 次应正常重启",
                cycle + 1
            );
        }
    }
    assert_eq!(
        last,
        Some(RestartOutcome::CircuitBroken {
            crashes_in_window: 5
        }),
        "60s 内第 5 次崩溃必须熔断"
    );
    assert_eq!(runtime.status().await, RuntimeStatus::Disabled);
    assert_eq!(
        runtime.status_reason().await,
        Some(DisabledReason::CrashLoop)
    );
    assert!(h.observer.has_audit(AuditKind::CircuitBroken));

    // crash_loop 不允许 retry（仅 disabled + start_failed 可用）。
    let error = h.supervisor.runtime_retry("mock").await.unwrap_err();
    assert!(
        matches!(error, SupervisorError::RetryNotAllowed { .. }),
        "{error:?}"
    );

    // enable 可用于 crash_loop（人工动作）：disabled → cold → starting → ready。
    let outcome = h.supervisor.runtime_enable("mock").await.unwrap();
    assert_eq!(outcome, StartOutcome::Ready);
    assert_eq!(runtime.status().await, RuntimeStatus::Ready);
    let transitions = h.observer.transitions();
    let tail: Vec<_> = transitions.iter().rev().take(3).rev().cloned().collect();
    assert_eq!(
        tail,
        vec![
            (RuntimeStatus::Disabled, RuntimeStatus::Cold),
            (RuntimeStatus::Cold, RuntimeStatus::Starting),
            (RuntimeStatus::Starting, RuntimeStatus::Ready),
        ]
    );
    h.supervisor.shutdown_all().await;
}

// ===== DoD⑦：runtime_retry 参数（白名单）与状态校验、disabled → cold → starting =====

#[tokio::test]
async fn runtime_retry_requires_whitelist_and_start_failed() {
    let spec = RuntimeSpec::with_fresh_token(manifest(&["--mode", "stderr-crash", "--lines", "3"]));
    let h = harness(vec![spec]);
    h.supervisor.warmup_all().await;
    let runtime = h.supervisor.get("mock").expect("注册表命中");
    assert_eq!(runtime.status().await, RuntimeStatus::Disabled);

    // 白名单外 runtime_id 拒绝。
    let error = h
        .supervisor
        .runtime_retry("not-registered")
        .await
        .unwrap_err();
    assert_eq!(
        error,
        SupervisorError::UnknownRuntime("not-registered".to_owned())
    );
    let error = h
        .supervisor
        .runtime_enable("not-registered")
        .await
        .unwrap_err();
    assert!(matches!(error, SupervisorError::UnknownRuntime(_)));

    // start_failed → retry 允许；转移序列必须含 disabled → cold → starting。
    let outcome = h.supervisor.runtime_retry("mock").await.unwrap();
    assert!(
        matches!(outcome, StartOutcome::Failed { .. }),
        "夹具仍会启动即崩：{outcome:?}"
    );
    let transitions = h.observer.transitions();
    assert!(
        transitions.windows(2).any(|pair| pair
            == [
                (RuntimeStatus::Disabled, RuntimeStatus::Cold),
                (RuntimeStatus::Cold, RuntimeStatus::Starting)
            ]
            .as_slice()),
        "必须观察到 disabled → cold → starting：{transitions:?}"
    );
    assert_eq!(runtime.status().await, RuntimeStatus::Disabled);
}

// ===== 资源采样：运行中进程可采样（未超限不告警；超限逻辑见单元测试） =====

#[tokio::test]
async fn resource_sampling_reads_live_process_without_false_alert() {
    let spec = RuntimeSpec::with_fresh_token(manifest(&["--mode", "deaf", "--seconds", "120"]));
    let h = harness(vec![spec]);
    h.supervisor.warmup_all().await;
    let runtime = h.supervisor.get("mock").expect("注册表命中");
    for _ in 0..3 {
        assert_eq!(
            runtime.sample_resources().await,
            None,
            "远低于 1GB/200% 不应告警"
        );
    }
    let log: ObserverLog = h.observer.snapshot();
    assert!(log.alerts.is_empty());
    h.supervisor.shutdown_all().await;
}
