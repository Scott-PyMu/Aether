//! M2-08 DoD3：T5b 适配器卡死恢复（设计 D5 / 附录 D）。
//!
//! 流程（真实夹具进程 + 真实 10s/5s/连续 3 次心跳配置，**不压缩时间参数**）：
//! 1. 监督器预热 `--mode deaf` 夹具（hello + `initialize` 应答后不再响应 health/请求，
//!    等效「进入不响应模式」，不使用 Windows 不具备的 `SIGSTOP`）；
//! 2. 启动监控任务（与应用启动序列同一路径 `Supervisor::spawn_monitors`）；
//! 3. 计时起点 = 注入时刻（适配器 Ready 后）：
//!    - 10s 心跳 + 5s 超时、连续 3 次失败 → `≤45s` 触发重启（`ready→degraded`
//!      携带 `status_reason=heartbeat_failed`）；
//!    - 终止序列（deaf：`shutdown` RPC 超时 → 强制整树回收）+ 退避 + 重启/握手
//!      → **120s 内 Ready**（新 PID，旧进程无残留）。
//!
//! 运行：`cargo test -p aether-adapters --test m2_08_t5b -- --nocapture`
//!（夹具 bin 由 Cargo 提供 `CARGO_BIN_EXE_aether-adapter-fixture`）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{
    AdapterLedger, AdmissionPolicy, RuntimeManifest, RuntimeSpec, StartOutcome, Supervisor,
    SupervisorConfig, SysinfoProbe, SystemTreeKiller,
};
use aether_core::RuntimeStatus;
use common::{fixture_binary, pid_alive, unique_temp_dir, RecordingObserver};
use serde_json::json;

/// T5b 触发上限：10s 心跳连续 3 次失败（含 5s 超时）≤45s（附录 D）。
const TRIGGER_LIMIT: Duration = Duration::from_secs(45);
/// T5b 总上限：重启/握手预算内 Ready ≤120s（附录 D）。
const READY_LIMIT: Duration = Duration::from_secs(120);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t5b_deaf_adapter_heartbeat_restarts_ready_within_120s() {
    // 严格 D5：心跳 10s / 超时 5s / 连续 3 次；退避 1/2/4/8/16/30s（不压缩）。
    let config = SupervisorConfig::d5();
    assert_eq!(config.heartbeat.interval, Duration::from_secs(10));
    assert_eq!(config.heartbeat.timeout, Duration::from_secs(5));
    assert_eq!(config.heartbeat.max_consecutive_failures, 3);
    assert!(
        config.backoff_override.is_none(),
        "T5b 必须使用 D5 真实退避"
    );

    let dir = unique_temp_dir("m2-08-t5b");
    let ledger = Arc::new(tokio::sync::Mutex::new(
        AdapterLedger::load(dir.join("adapters.json")).expect("台账"),
    ));
    let observer = Arc::new(RecordingObserver::new());
    let spec = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Mock Fixture", fixture_binary())
            .official(true)
            .with_args(["--mode", "deaf", "--seconds", "300"]),
    );
    let supervisor = Supervisor::new(
        vec![spec],
        config,
        AdmissionPolicy::official(),
        observer.clone(),
        ledger,
        Arc::new(SysinfoProbe::new()),
        Arc::new(SystemTreeKiller),
    )
    .expect("监督器");

    let outcomes = supervisor.warmup_all().await;
    assert_eq!(
        outcomes[0].1,
        StartOutcome::Ready,
        "deaf 夹具必须完成 hello + initialize 进入 Ready：{:?}",
        outcomes[0].1
    );
    let runtime = supervisor.get("mock").expect("白名单命中");
    let first_pid = runtime.current_pid().await.expect("首次 pid");

    // 注入时刻：适配器已 Ready 且自该时刻起不响应 health.ping（不响应模式）。
    let inject_at = Instant::now();
    let monitor = runtime.spawn_monitor();

    // 采样墙钟：degraded（熔断触发重启）与二次 ready（新 PID）。
    let mut degraded_at: Option<Instant> = None;
    let mut ready_at: Option<Instant> = None;
    let mut ready_pid: Option<u32> = None;
    let deadline = inject_at + READY_LIMIT;
    while Instant::now() < deadline {
        let status = runtime.status().await;
        let pid = runtime.current_pid().await;
        if degraded_at.is_none() && status == RuntimeStatus::Degraded {
            degraded_at = Some(Instant::now());
        }
        if degraded_at.is_some() && status == RuntimeStatus::Ready && pid != Some(first_pid) {
            ready_at = Some(Instant::now());
            ready_pid = pid;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    monitor.abort();

    let Some(degraded_at) = degraded_at else {
        panic!("T5b 失败：120s 内未观测到 ready→degraded（心跳熔断未触发）");
    };
    let Some(ready_at) = ready_at else {
        panic!("T5b 失败：120s 内未恢复 Ready（新进程）");
    };
    let trigger_ms =
        u64::try_from(degraded_at.duration_since(inject_at).as_millis()).unwrap_or(u64::MAX);
    let ready_ms =
        u64::try_from(ready_at.duration_since(inject_at).as_millis()).unwrap_or(u64::MAX);

    // 触发时限 ≤45s（10s 心跳连续 3 次失败 + 超时预算）。
    assert!(
        Duration::from_millis(trigger_ms) <= TRIGGER_LIMIT,
        "心跳熔断触发必须 ≤45s（实测 {trigger_ms}ms）"
    );
    // Ready 时限 ≤120s（终止序列 ≤15s + 退避 + 启动/握手预算）。
    assert!(
        Duration::from_millis(ready_ms) <= READY_LIMIT,
        "T5b Ready 必须 ≤120s（实测 {ready_ms}ms）"
    );

    // 熔断原因必须为 heartbeat_failed；恢复路径 degraded → starting → ready。
    let reasons = observer.reasons();
    assert!(
        reasons
            .iter()
            .any(|reason| reason.as_deref() == Some("heartbeat_failed")),
        "ready→degraded 必须携带 status_reason=heartbeat_failed：{reasons:?}"
    );
    let transitions = observer.transitions();
    assert!(
        transitions.contains(&(RuntimeStatus::Degraded, RuntimeStatus::Starting)),
        "自动重启必须经 degraded→starting：{transitions:?}"
    );
    let ready_pid = ready_pid.expect("恢复后新 pid");
    assert_ne!(ready_pid, first_pid, "重启必须产生新进程");
    assert!(!pid_alive(first_pid), "旧进程必须被整树回收（无残留）");

    // 证据行（脚本解析 + 归档）。
    println!(
        "AETHER_M2_08_T5B {}",
        json!({
            "trigger_ms": trigger_ms,
            "ready_ms": ready_ms,
            "trigger_limit_ms": TRIGGER_LIMIT.as_millis() as u64,
            "ready_limit_ms": READY_LIMIT.as_millis() as u64,
            "first_pid": first_pid,
            "second_pid": ready_pid,
            "first_pid_residual": pid_alive(first_pid),
            "transitions": transitions
                .iter()
                .map(|(from, to)| format!("{}→{}", from.as_str(), to.as_str()))
                .collect::<Vec<_>>(),
            "reasons": reasons,
            "heartbeat": {"interval_s": 10, "timeout_s": 5, "max_failures": 3},
        })
    );

    supervisor.shutdown_all().await;
}

/// 孤儿夹具语义（DoD1 前提）：stdin EOF（核心被强杀）后仍存活。
///
/// `--survive-eof` = 真实卡死适配器语义（不因核心死亡自行退出）；对照（无该参数）
/// 必须自行退出，证明差异来自显式开关而非读取阻塞。
#[test]
fn fixture_survive_eof_keeps_orphan_alive_on_stdin_eof() {
    use std::process::{Command, Stdio};

    let spawn = |extra: &[&str]| {
        let mut command = Command::new(fixture_binary());
        command
            .args(["--mode", "deaf", "--seconds", "10"])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("启动夹具");
        drop(child.stdin.take()); // 模拟核心被强杀：stdin 写端关闭（EOF）
        child
    };

    let mut survivor = spawn(&["--survive-eof"]);
    let mut control = spawn(&[]);
    std::thread::sleep(Duration::from_millis(800));

    let survivor_status = survivor.try_wait().expect("查询存活夹具");
    assert!(
        survivor_status.is_none(),
        "--survive-eof 必须在 stdin EOF 后保持存活（孤儿语义）"
    );
    let control_status = control.try_wait().expect("查询对照夹具");
    assert!(
        control_status.is_some(),
        "无 --survive-eof 时必须因 stdin EOF 自行退出（对照）"
    );

    let _ = survivor.kill();
    let _ = survivor.wait();
    let _ = control.wait();
    println!(
        "AETHER_M2_08_FIXTURE_EOF {{\"survivor_alive\":true,\"control_exited\":{}}}",
        control_status.is_some()
    );
}
