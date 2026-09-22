//! M2-11 / ADR-008：Codex 适配器一致性验收（M2-02 等价口径）。
//!
//! 覆盖：
//! - 握手/initialize/会话创建（ULID 别名作为 `native_id`）/`tools.list`/health；
//! - 流式：`run.started` → `message.delta`（整段 chunk）→ `message.completed` + `run.completed`；
//! - 工具：`command_execution` 正常完成 / `file_change` 失败（附录 B 事件与错误码）；
//! - 异常路径：`turn.failed` / 无终态退出 / 连续坏行 / 会话 1005 / 未知方法 -32601；
//! - 中断：5s 内返回、`run.cancelled`、Codex CLI 进程树整树回收；
//! - 幂等（`client_msg_id`）与 dispose 后拒绝；
//! - Mode R：别名映射跨适配器进程 `exec resume` 恢复；
//! - T5a：外部强杀 → 30s 内 Ready + 在途 run 收口 + Mode R 重放；
//! - 夹具 20 次完成率代理（真实端点 50 次为 opt-in，见 `verify-m2-11`）。
//!
//! 运行：`AETHER_CODEX_ADAPTER=<编译产物> cargo test -p aether-adapters --test m2_11_codex -- --nocapture`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::session_client::{
    AdapterSessionClient, RunOutcome, SessionClientError, ADAPTER_DISCONNECTED_CODE,
};
use aether_adapters::supervisor::{
    kill_tree_system, AdapterLedger, AdmissionPolicy, HeartbeatConfig, RuntimeManifest,
    RuntimeSpec, StartOutcome, Supervisor, SupervisorConfig, SysinfoProbe, SystemTreeKiller,
    TerminationBudget,
};
use aether_adapters::RequestError;
use aether_core::{EventPayload, EventType, RuntimeStatus};
use serde_json::json;

fn delta_text(events: &[aether_core::EventEnvelope], run_id: &str) -> String {
    let mut text = String::new();
    for event in events {
        if event.run_id.as_ref().map(|id| id.as_str()) != Some(run_id) {
            continue;
        }
        if let EventPayload::MessageDelta(payload) = &event.payload {
            text.push_str(&payload.text);
        }
    }
    text
}

#[tokio::test]
async fn handshake_initialize_session_create_tools_and_health() {
    let Some(mut harness) = common::CodexHarness::launch(&[]).await else {
        return;
    };
    let initialized = harness
        .client
        .initialize(json!({}))
        .await
        .expect("initialize");
    assert_eq!(initialized["acknowledged"], true);

    let session = harness
        .client
        .create_session(Some("m2-11-codex"), None, None)
        .await
        .expect("session.create");
    assert_eq!(session.session_id.len(), 26, "别名必须是 26 位 ULID");
    assert_eq!(
        session.native_id.as_deref(),
        Some(session.session_id.as_str()),
        "别名即 native_id（ADR-008 §3.3）"
    );
    assert!(!session.resumed);

    let tools = harness
        .client
        .tools_list(Some(&session.session_id))
        .await
        .expect("tools.list");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    for expected in ["command_execution", "file_change", "mcp_tool_call"] {
        assert!(
            names.contains(&expected),
            "tools.list 缺少 {expected}: {names:?}"
        );
    }

    let pong = harness.client.health_ping().await.expect("health.ping");
    assert_eq!(pong["status"], "ok");

    let resolved = harness
        .client
        .resolve_permission("req-1", "allow", Some("once"))
        .await
        .expect("permission.resolve");
    assert_eq!(resolved["resolved"], false);
    assert!(resolved["reason"]
        .as_str()
        .unwrap_or_default()
        .contains("D9 边界"));

    harness.shutdown().await;
}

#[tokio::test]
async fn streaming_deltas_concat_equals_completed_and_envelope_contract() {
    let Some(mut harness) = common::CodexHarness::launch(&[]).await else {
        return;
    };
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");
    let started = Instant::now();
    let ack = harness
        .client
        .send(&session.session_id, "m2-11-codex-stream", "chat")
        .await
        .expect("session.send ack");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "ack 必须是快路径（实测 {:?}）",
        started.elapsed()
    );

    let outcome = harness
        .wait_outcome(&ack.run_id, Duration::from_secs(30))
        .await
        .expect("run 必须到达终态");
    let RunOutcome::Completed {
        assistant_text,
        usage,
    } = outcome
    else {
        panic!("期望 run.completed，实际 {outcome:?}");
    };
    let types = harness.client.run_event_types(&ack.run_id).await;
    assert_eq!(types.first().map(String::as_str), Some("run.started"));
    assert_eq!(types.last().map(String::as_str), Some("run.completed"));
    assert!(
        types
            .iter()
            .filter(|kind| kind.as_str() == "message.delta")
            .count()
            > 0
    );
    assert_eq!(
        types
            .iter()
            .filter(|kind| kind.as_str() == "message.completed")
            .count(),
        1
    );

    let events = harness.client.events().await;
    let concatenated = delta_text(&events, &ack.run_id);
    assert_eq!(
        assistant_text.as_deref(),
        Some(concatenated.as_str()),
        "终稿必须等于 delta 拼接"
    );
    assert!(concatenated.contains("Aether M2-11 fake codex baseline"));
    assert!(usage.is_some(), "usage 必须随终态上报");

    let run_events: Vec<_> = events
        .iter()
        .filter(|event| event.run_id.as_ref().map(|id| id.as_str()) == Some(ack.run_id.as_str()))
        .collect();
    assert!(run_events.iter().all(|event| event.v == 1));
    assert!(run_events
        .iter()
        .all(|event| event.runtime_id.as_str() == "codex"));
    let mut seqs: Vec<u64> = run_events.iter().map(|event| event.seq).collect();
    let unique: std::collections::BTreeSet<u64> = seqs.iter().copied().collect();
    assert_eq!(unique.len(), seqs.len(), "seq 必须唯一");
    seqs.sort_unstable();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "seq 必须单调"
    );

    println!("[m2-11 codex] 流式 run 事件序列 = {types:?}");
    harness.shutdown().await;
}

#[tokio::test]
async fn tool_calls_normal_and_failure_map_to_appendix_b_events() {
    let Some(mut harness) = common::CodexHarness::launch(&[]).await else {
        return;
    };
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");

    let normal = harness
        .client
        .send(&session.session_id, "m2-11-codex-tool", "tool:normal")
        .await
        .expect("send");
    assert!(matches!(
        harness
            .wait_outcome(&normal.run_id, Duration::from_secs(30))
            .await,
        Some(RunOutcome::Completed { .. })
    ));
    let tool_types: Vec<String> = harness
        .client
        .run_event_types(&normal.run_id)
        .await
        .into_iter()
        .filter(|kind| kind.starts_with("tool."))
        .collect();
    assert_eq!(tool_types, vec!["tool.call_started", "tool.call_completed"]);

    let failed = harness
        .client
        .send(&session.session_id, "m2-11-codex-tool-fail", "tool:fail")
        .await
        .expect("send");
    assert!(matches!(
        harness
            .wait_outcome(&failed.run_id, Duration::from_secs(30))
            .await,
        Some(RunOutcome::Completed { .. })
    ));
    let events = harness.client.events().await;
    let tool_failed = events
        .iter()
        .find(|event| {
            event.run_id.as_ref().map(|id| id.as_str()) == Some(failed.run_id.as_str())
                && event.event_type() == EventType::ToolCallFailed
        })
        .expect("tool.call_failed");
    let EventPayload::ToolCallFailed(payload) = &tool_failed.payload else {
        panic!("payload 类型不符");
    };
    assert_eq!(payload.error.code, "tool_execution_failed");
    assert!(payload.error.message.contains("failed"));
    println!(
        "[m2-11 codex] 工具失败事件 = tool_name={} error={}",
        payload.tool_name, payload.error.code
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn exception_paths_turn_failed_orphan_exit_and_unhealthy_stdout() {
    let Some(mut harness) = common::CodexHarness::launch(&[]).await else {
        return;
    };
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");

    let turn = harness
        .client
        .send(&session.session_id, "m2-11-codex-f1", "fail:turn")
        .await
        .expect("send");
    match harness
        .wait_outcome(&turn.run_id, Duration::from_secs(30))
        .await
        .expect("终态")
    {
        RunOutcome::Failed { error } => {
            assert_eq!(error.code, "turn_failed");
            assert!(error.recoverable);
        }
        other => panic!("期望 run.failed(turn_failed)，实际 {other:?}"),
    }

    let orphan = harness
        .client
        .send(&session.session_id, "m2-11-codex-f2", "fail:no-terminal")
        .await
        .expect("send");
    match harness
        .wait_outcome(&orphan.run_id, Duration::from_secs(30))
        .await
        .expect("终态")
    {
        RunOutcome::Failed { error } => {
            assert_eq!(error.code, "cli_exit");
            assert!(
                error.message.contains("exit=7"),
                "message={}",
                error.message
            );
        }
        other => panic!("期望 run.failed(cli_exit)，实际 {other:?}"),
    }

    let unhealthy = harness
        .client
        .send(&session.session_id, "m2-11-codex-f3", "fail:bad-json")
        .await
        .expect("send");
    match harness
        .wait_outcome(&unhealthy.run_id, Duration::from_secs(30))
        .await
        .expect("终态")
    {
        RunOutcome::Failed { error } => {
            assert_eq!(error.code, "cli_exit");
            assert!(error.message.contains("20"), "message={}", error.message);
        }
        other => panic!("期望不健康 run.failed，实际 {other:?}"),
    }

    let missing = harness
        .client
        .send("01ARZ3NDEKTSV4RRFFQ69G5FAV", "m2-11-codex-missing", "chat")
        .await
        .expect_err("会话不存在必须报错");
    match missing {
        SessionClientError::Request(RequestError::Rpc(rpc)) => assert_eq!(rpc.code, 1005),
        other => panic!("错误类型不符: {other:?}"),
    }
    let unknown = harness
        .client
        .connection()
        .request_raw("session.listen", json!({}), Duration::from_secs(5))
        .await
        .expect_err("未知方法必须回错误");
    match unknown {
        RequestError::Rpc(rpc) => assert_eq!(rpc.code, -32601),
        other => panic!("错误类型不符: {other:?}"),
    }
    let pong = harness.client.health_ping().await.expect("仍在服务");
    assert_eq!(pong["status"], "ok");

    harness.shutdown().await;
}

#[tokio::test]
async fn interrupt_cancels_run_and_reaps_codex_tree() {
    let Some(mut harness) = common::CodexHarness::launch(&[]).await else {
        return;
    };
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");
    let ack = harness
        .client
        .send(&session.session_id, "m2-11-codex-interrupt", "slow")
        .await
        .expect("send");
    assert!(
        harness
            .client
            .wait_for_event(&ack.run_id, EventType::RunStarted, Duration::from_secs(10))
            .await
    );
    let fake_pid = common::wait_for_async(
        || async { harness.last_fake_pid().is_some() },
        Duration::from_secs(10),
    )
    .await;
    assert!(fake_pid, "fake-codex pid 必须落盘");
    let fake_pid = harness.last_fake_pid().expect("pid");

    let started = Instant::now();
    let interrupted = harness
        .client
        .interrupt(&session.session_id)
        .await
        .expect("session.interrupt（5s 超时）");
    assert_eq!(interrupted["interrupted"], true);
    assert!(started.elapsed() < Duration::from_secs(5));
    let outcome = harness
        .wait_outcome(&ack.run_id, Duration::from_secs(10))
        .await
        .expect("中断后必须收口");
    assert!(
        matches!(outcome, RunOutcome::Cancelled { .. }),
        "{outcome:?}"
    );
    assert!(
        common::wait_for_async(
            || async { !common::pid_alive(fake_pid) },
            Duration::from_secs(10)
        )
        .await,
        "fake-codex 进程树必须被回收（pid={fake_pid} 仍存活）"
    );
    let again = harness
        .client
        .interrupt(&session.session_id)
        .await
        .expect("再次 interrupt");
    assert_eq!(again["interrupted"], false);
    harness.shutdown().await;
}

#[tokio::test]
async fn idempotent_send_and_dispose_semantics() {
    let Some(mut harness) = common::CodexHarness::launch(&[]).await else {
        return;
    };
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");
    let first = harness
        .client
        .send(&session.session_id, "m2-11-codex-dup", "chat")
        .await
        .expect("send");
    let duplicate = harness
        .client
        .send(&session.session_id, "m2-11-codex-dup", "chat")
        .await
        .expect("重发");
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.run_id, first.run_id);
    assert!(harness
        .wait_outcome(&first.run_id, Duration::from_secs(30))
        .await
        .is_some());

    let disposed = harness
        .client
        .dispose(&session.session_id)
        .await
        .expect("session.dispose");
    assert_eq!(disposed["disposed"], true);
    let after = harness
        .client
        .send(&session.session_id, "m2-11-codex-after", "chat")
        .await
        .expect_err("dispose 后会话不可用");
    match after {
        SessionClientError::Request(RequestError::Rpc(rpc)) => assert_eq!(rpc.code, 1005),
        other => panic!("错误类型不符: {other:?}"),
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn mode_r_alias_resume_across_adapter_process_restart() {
    let home = common::unique_temp_dir("m2-11-codex-mode-r");
    let Some(mut first) = common::CodexHarness::launch_with_home(home.clone(), &[]).await else {
        return;
    };
    let session = first
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");
    let native_id = session.native_id.clone().expect("native_id");
    let remember = first
        .client
        .send(
            &session.session_id,
            "m2-11-codex-remember",
            "remember:AETHER-CODEX-TOKEN",
        )
        .await
        .expect("send");
    assert!(matches!(
        first
            .wait_outcome(&remember.run_id, Duration::from_secs(30))
            .await,
        Some(RunOutcome::Completed { .. })
    ));
    first.shutdown().await;
    drop(first);

    // 适配器进程 2（模拟运行时重启）：仅凭别名恢复（exec resume）。
    let Some(mut second) = common::CodexHarness::launch_with_home(home, &[]).await else {
        return;
    };
    let resumed = second
        .client
        .create_session(None, Some(&native_id), None)
        .await
        .expect("session.create(native_id)");
    assert!(resumed.resumed, "别名命中必须标记 Mode R 恢复");
    assert_eq!(resumed.session_id, native_id);
    let replay = second
        .client
        .send(&resumed.session_id, "m2-11-codex-recall", "recall")
        .await
        .expect("重放 send");
    match second
        .wait_outcome(&replay.run_id, Duration::from_secs(30))
        .await
        .expect("重放必须终态")
    {
        RunOutcome::Completed { assistant_text, .. } => assert_eq!(
            assistant_text.as_deref(),
            Some("AETHER-CODEX-TOKEN"),
            "Mode R 恢复后必须命中原生上下文"
        ),
        other => panic!("期望重放完成，实际 {other:?}"),
    }
    second.shutdown().await;
}

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

fn codex_manifest(home: &std::path::Path) -> Option<RuntimeManifest> {
    let adapter = common::codex_adapter_binary()?;
    let fake = common::fake_codex_cli();
    let pid_file = home.join("pids.txt");
    let workspace = common::unique_temp_dir("m2-11-codex-t5a-ws");
    Some(
        RuntimeManifest::new("codex", "Codex", adapter)
            .official(true)
            .with_args([
                "--codex-bin".to_owned(),
                common::node_binary(),
                "--codex-arg".to_owned(),
                fake.to_string_lossy().into_owned(),
                "--workspace".to_owned(),
                workspace.to_string_lossy().into_owned(),
                "--sandbox".to_owned(),
                "read-only".to_owned(),
                "--codex-home".to_owned(),
                home.to_string_lossy().into_owned(),
            ])
            .with_env([
                ("CODEX_HOME".to_owned(), home.to_string_lossy().into_owned()),
                (
                    "FAKE_CODEX_PID_FILE".to_owned(),
                    pid_file.to_string_lossy().into_owned(),
                ),
            ]),
    )
}

/// T5a：外部强杀 → 30s 内 Ready；在途 run 收口；Mode R 别名重放命中原生上下文。
#[tokio::test]
async fn t5a_kill_codex_adapter_ready_within_30s_and_replay() {
    let home = common::unique_temp_dir("m2-11-codex-t5a-home");
    let Some(manifest) = codex_manifest(&home) else {
        return;
    };
    let spec = RuntimeSpec::with_fresh_token(manifest);
    let ledger_dir = common::unique_temp_dir("m2-11-codex-t5a-ledger");
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
    assert_eq!(outcomes[0].1, StartOutcome::Ready, "预热必须 Ready");
    let runtime = supervisor.get("codex").expect("白名单命中");

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
            "m2-11-codex-t5a-remember",
            "remember:AETHER-CODEX-T5A",
        )
        .await
        .expect("remember send");
    assert!(matches!(
        client1
            .wait_run_outcome(&remember.run_id, Duration::from_secs(30))
            .await,
        Some(RunOutcome::Completed { .. })
    ));

    let inflight = client1
        .send(&session.session_id, "m2-11-codex-t5a-inflight", "slow")
        .await
        .expect("slow send");
    assert!(
        client1
            .wait_for_event(
                &inflight.run_id,
                EventType::RunStarted,
                Duration::from_secs(10)
            )
            .await
    );

    let adapter_pid = runtime.current_pid().await.expect("适配器 pid");
    let monitor = runtime.spawn_monitor();
    let kill_at = Instant::now();
    kill_tree_system(adapter_pid).expect("外部强杀注入");

    let outcome = client1
        .wait_run_outcome(&inflight.run_id, Duration::from_secs(30))
        .await
        .expect("在途 run 必须收口");
    assert!(
        matches!(outcome, RunOutcome::Disconnected { .. }),
        "{outcome:?}"
    );
    assert_eq!(outcome.error_code(), Some(ADAPTER_DISCONNECTED_CODE));

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
        "[m2-11 codex T5a] 强杀 pid={adapter_pid} → Ready 耗时 {recovery:?}（新 pid={:?}）",
        runtime.current_pid().await
    );
    assert!(ready, "监督器必须在 30s 内恢复 Ready（实测 {recovery:?}）");

    let connection2 = runtime.connection().await.expect("恢复后连接");
    let client2 = AdapterSessionClient::new(connection2);
    let resumed = client2
        .create_session(None, Some(&native_id), None)
        .await
        .expect("session.create(native_id)");
    assert!(resumed.resumed);
    let replay = client2
        .send(&resumed.session_id, "m2-11-codex-t5a-replay", "recall")
        .await
        .expect("重放 send");
    match client2
        .wait_run_outcome(&replay.run_id, Duration::from_secs(30))
        .await
        .expect("重放必须终态")
    {
        RunOutcome::Completed { assistant_text, .. } => assert_eq!(
            assistant_text.as_deref(),
            Some("AETHER-CODEX-T5A"),
            "Mode R 重放必须命中原生上下文"
        ),
        other => panic!("期望重放完成，实际 {other:?}"),
    }

    monitor.abort();
    supervisor.shutdown_all().await;
}

/// 夹具 20 次完成率代理（真实端点 20/50 次为 opt-in，见 `verify-m2-11`）。
#[tokio::test]
async fn fixture_twenty_runs_all_reach_terminal_state() {
    let Some(mut harness) = common::CodexHarness::launch(&[]).await else {
        return;
    };
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");
    let total = 20;
    let mut completed = 0;
    for index in 0..total {
        let ack = harness
            .client
            .send(
                &session.session_id,
                &format!("m2-11-codex-rate-{index:02}"),
                "chat",
            )
            .await
            .expect("send");
        let outcome = harness
            .wait_outcome(&ack.run_id, Duration::from_secs(30))
            .await;
        assert!(outcome.is_some(), "run {index} 必须到达终态");
        if matches!(outcome, Some(RunOutcome::Completed { .. })) {
            completed += 1;
        }
    }
    let rate = completed as f64 / total as f64;
    println!("[m2-11 codex] 夹具 20 次完成率 = {completed}/{total}（{rate:.2}）");
    assert!(rate >= 0.95, "完成率必须 ≥95%（实测 {completed}/{total}）");
    harness.shutdown().await;
}
