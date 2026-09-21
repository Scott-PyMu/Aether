//! M2-02 DoD3：T5a 崩溃注入（外部强杀 → 30s 内 Ready；在途 run 标 failed；Mode R 重放重试）。
//!
//! 流程（真实 Claude 适配器进程 + fake-claude 夹具 + M1-10 监督器）：
//! 1. 监督器预热适配器（`runtime_id=claude-code`，官方白名单）；
//! 2. `remember:<token>` 完成（建立原生会话上下文）→ `slow` 在途流式；
//! 3. 跨平台强杀 helper（Unix `SIGKILL` 进程组 / Windows Job Object+`taskkill`，复用 M1-10）
//!    外部强杀适配器进程；
//! 4. 在途 run 由会话客户端收口为 `Disconnected(adapter_disconnected)`（错误码供核心落
//!    `run.failed`；M2-01 生命周期对执行器 Failed 的落库路径已有覆盖）；
//! 5. 监督器监控循环自动 `Degraded(crashed) → Starting → Ready`，断言墙钟 ≤30s；
//! 6. 新连接以 `native_id`（Mode R）恢复原生会话重放「recall」→ 命中口令，每个 run 均有终态。
//!
//! 运行：`AETHER_CLAUDE_ADAPTER=<编译产物> cargo test -p aether-adapters --test m2_02_t5a -- --nocapture`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::session_client::{
    AdapterSessionClient, RunOutcome, ADAPTER_DISCONNECTED_CODE,
};
use aether_adapters::supervisor::{
    kill_tree_system, AdapterLedger, AdmissionPolicy, HeartbeatConfig, RuntimeManifest,
    RuntimeSpec, StartOutcome, Supervisor, SupervisorConfig, SysinfoProbe, SystemTreeKiller,
    TerminationBudget,
};
use aether_core::RuntimeStatus;

/// 测试配置：常量级压缩（心跳 200ms、退避 200ms），DoD 的 30s/120s 墙钟断言不压缩。
fn test_config() -> SupervisorConfig {
    SupervisorConfig {
        handshake_timeout: Duration::from_secs(10),
        initialize_timeout: Duration::from_secs(10),
        heartbeat: HeartbeatConfig {
            interval: Duration::from_millis(200),
            timeout: Duration::from_millis(150),
            max_consecutive_failures: 3,
        },
        termination: TerminationBudget {
            shutdown_rpc: Duration::from_millis(300),
            graceful: Duration::from_secs(1),
            force: Duration::from_secs(1),
            fallback: Duration::from_secs(1),
        },
        backoff_override: Some(Duration::from_millis(200)),
    }
}

fn claude_manifest(home: &std::path::Path) -> Option<RuntimeManifest> {
    let adapter = common::claude_adapter_binary()?;
    let fake = common::fake_claude_cli();
    let pid_file = home.join("pids.txt");
    let workspace = common::unique_temp_dir("m2-02-t5a-ws");
    Some(
        RuntimeManifest::new("claude-code", "Claude Code", adapter)
            .official(true)
            .with_args([
                "--claude-bin".to_owned(),
                common::node_binary(),
                "--claude-arg".to_owned(),
                fake.to_string_lossy().into_owned(),
                "--workspace".to_owned(),
                workspace.to_string_lossy().into_owned(),
                "--tools".to_owned(),
                "none".to_owned(),
            ])
            .with_env([
                (
                    "FAKE_CLAUDE_HOME".to_owned(),
                    home.to_string_lossy().into_owned(),
                ),
                (
                    "FAKE_CLAUDE_PID_FILE".to_owned(),
                    pid_file.to_string_lossy().into_owned(),
                ),
            ]),
    )
}

#[tokio::test]
async fn t5a_kill_adapter_ready_within_30s_inflight_failed_and_mode_r_replay() {
    let home = common::unique_temp_dir("m2-02-t5a-home");
    let Some(manifest) = claude_manifest(&home) else {
        return;
    };
    let spec = RuntimeSpec::with_fresh_token(manifest);
    let ledger_dir = common::unique_temp_dir("m2-02-t5a-ledger");
    let ledger = Arc::new(tokio::sync::Mutex::new(
        AdapterLedger::load(ledger_dir.join("adapters.json")).expect("台账"),
    ));
    let observer = Arc::new(common::RecordingObserver::new());
    let supervisor = Supervisor::new(
        vec![spec],
        test_config(),
        AdmissionPolicy::official(),
        observer.clone(),
        ledger,
        Arc::new(SysinfoProbe::new()),
        Arc::new(SystemTreeKiller),
    )
    .expect("监督器");

    let outcomes = supervisor.warmup_all().await;
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].1, StartOutcome::Ready, "预热必须 Ready");
    let runtime = supervisor.get("claude-code").expect("白名单命中");

    // 客户端 1：会话 + remember（完成）→ slow（在途）。
    let connection1 = runtime.connection().await.expect("运行中连接");
    let client1 = AdapterSessionClient::new(connection1);
    let session = client1
        .create_session(None, None, None)
        .await
        .expect("session.create");
    let native_id = session.native_id.clone().expect("native_id");
    let remember = client1
        .send(
            &session.session_id,
            "t5a-remember",
            "remember:AETHER-T5A-TOKEN",
        )
        .await
        .expect("remember send");
    assert!(matches!(
        client1
            .wait_run_outcome(&remember.run_id, Duration::from_secs(20))
            .await,
        Some(RunOutcome::Completed { .. })
    ));

    let inflight = client1
        .send(&session.session_id, "t5a-inflight", "slow")
        .await
        .expect("slow send");
    assert!(
        client1
            .wait_for_event(
                &inflight.run_id,
                aether_core::EventType::MessageDelta,
                Duration::from_secs(10)
            )
            .await,
        "slow 必须已开始流式"
    );

    let adapter_pid = runtime.current_pid().await.expect("适配器 pid");
    let monitor = runtime.spawn_monitor();
    let kill_at = Instant::now();
    // 跨平台强杀 helper（ADR-003/ADR-004；与 M1-10 终止序列同口径）。
    kill_tree_system(adapter_pid).expect("外部强杀注入");

    // 在途 run：连接断开 → 客户端收口 Disconnected（核心据此落 run.failed）。
    let outcome = client1
        .wait_run_outcome(&inflight.run_id, Duration::from_secs(30))
        .await
        .expect("在途 run 必须收口（不得挂起）");
    assert!(
        matches!(outcome, RunOutcome::Disconnected { .. }),
        "期望 Disconnected，实际 {outcome:?}"
    );
    assert_eq!(outcome.error_code(), Some(ADAPTER_DISCONNECTED_CODE));

    // 监督器自动恢复 Ready（墙钟 ≤30s）。
    let ready = common::wait_for_async(
        || async {
            runtime.status().await == RuntimeStatus::Ready
                && runtime.current_pid().await != Some(adapter_pid)
        },
        Duration::from_secs(30),
    )
    .await;
    let recovery = kill_at.elapsed();
    println!(
        "[m2-02 T5a] 外部强杀 pid={adapter_pid} → Ready 耗时 {recovery:?}（新 pid={:?}）",
        runtime.current_pid().await
    );
    assert!(ready, "监督器必须在 30s 内恢复 Ready（实测 {recovery:?}）");
    assert!(
        recovery <= Duration::from_secs(30),
        "T5a 上限 30s（实测 {recovery:?}）"
    );
    let transitions = observer.transitions();
    assert!(
        transitions.contains(&(RuntimeStatus::Ready, RuntimeStatus::Degraded)),
        "崩溃必须触发 ready→degraded（crashed）：{transitions:?}"
    );
    assert!(
        transitions.contains(&(RuntimeStatus::Degraded, RuntimeStatus::Starting)),
        "自动重启必须经 degraded→starting：{transitions:?}"
    );
    assert!(
        transitions
            .iter()
            .filter(|pair| **pair == (RuntimeStatus::Starting, RuntimeStatus::Ready))
            .count()
            >= 2,
        "恢复后必须再次 starting→ready：{transitions:?}"
    );

    // Mode R：新连接以 native_id 恢复原生会话并重放，命中口令。
    let connection2 = runtime.connection().await.expect("恢复后连接");
    let client2 = AdapterSessionClient::new(connection2);
    let resumed = client2
        .create_session(None, Some(&native_id), None)
        .await
        .expect("session.create(native_id)");
    assert!(resumed.resumed, "Mode R 恢复标记");
    let replay = client2
        .send(&resumed.session_id, "t5a-replay", "recall")
        .await
        .expect("重放重试 send");
    match client2
        .wait_run_outcome(&replay.run_id, Duration::from_secs(20))
        .await
        .expect("重放必须终态")
    {
        RunOutcome::Completed { assistant_text, .. } => assert_eq!(
            assistant_text.as_deref(),
            Some("AETHER-T5A-TOKEN"),
            "Mode R 重放必须命中原生上下文"
        ),
        other => panic!("期望重放完成，实际 {other:?}"),
    }

    monitor.abort();
    supervisor.shutdown_all().await;
}

/// 夹具回归（DoD4 的离线代理指标）：连续 20 次会话全部到达终态。
///
/// 真实 Runtime 的 50 次完成率由 `pnpm verify:m2-02` 的 opt-in 步骤执行
/// （需 `AETHER_REQUIRE_REAL_CLAUDE=1` 与凭证；离线不可代替，见证据文档）。
#[tokio::test]
async fn fixture_twenty_runs_all_reach_terminal_state() {
    let Some(mut harness) = common::ClaudeHarness::launch(&[]).await else {
        return;
    };
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");
    let total = 20;
    let mut terminal = 0;
    for index in 0..total {
        let ack = harness
            .client
            .send(
                &session.session_id,
                &format!("m2-02-rate-{index:02}"),
                "chat",
            )
            .await
            .expect("send");
        let outcome = harness
            .wait_outcome(&ack.run_id, Duration::from_secs(30))
            .await;
        assert!(outcome.is_some(), "run {index} 必须到达终态");
        if matches!(outcome, Some(RunOutcome::Completed { .. })) {
            terminal += 1;
        }
    }
    let rate = terminal as f64 / total as f64;
    println!("[m2-02 DoD4 代理] 夹具 20 次完成率 = {terminal}/{total}（{rate:.2}）");
    assert!(
        rate >= 0.95,
        "夹具完成率必须 ≥95%（实测 {terminal}/{total}）"
    );
    harness.shutdown().await;
}
