//! M2-02 一致性测试（真实 Claude Code 适配器进程 + fake-claude 夹具）。
//!
//! 覆盖 DoD1（一致性全用例含异常路径）与 DoD2（`tools.list` + 工具调用事件上报）：
//! - 握手/initialize/会话创建（UUID native_id）；
//! - 流式：run.started → message.delta… → message.completed + run.completed（拼接一致）；
//! - 工具：正常完成 / 执行失败（附录 B 序列与错误码）；`tools.list` 观察子集；
//! - 中断：5s 内返回、工具收口 timeout/abort、run.cancelled、CLI 进程树回收；
//! - 异常路径：api 错误 / 无 result / stdout 连续坏行 / spawn 失败 / 会话 1005 / 未知方法 -32601；
//! - 幂等（client_msg_id）与 dispose 后拒绝；
//! - Mode R：跨适配器进程用 `native_id` 恢复并续聊（ADR-005）。
//!
//! 夹具：`scripts/test/m2-02/fake-claude/cli.mjs`（确定性；真实 Runtime 的 50 次完成率
//! 由 `pnpm verify:m2-02` 的 opt-in 步骤执行，需 `AETHER_REQUIRE_REAL_CLAUDE=1` 与凭证）。
//!
//! 运行：`AETHER_CLAUDE_ADAPTER=<编译产物> cargo test -p aether-adapters --test m2_02_consistency`
//! （由 `scripts/test/m2-02/verify-m2-02.mjs` 构建并设置）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use aether_adapters::session_client::{RunOutcome, SessionClientError};
use aether_adapters::RequestError;
use aether_core::{EventPayload, EventType};
use serde_json::json;

/// 从事件列表提取 `message.delta` 文本拼接。
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
async fn handshake_initialize_session_create_and_tools_list() {
    let Some(mut harness) = common::ClaudeHarness::launch(&[]).await else {
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
        .create_session(Some("m2-02"), None, None)
        .await
        .expect("session.create");
    assert!(!session.session_id.is_empty());
    assert_eq!(
        session.native_id.as_deref(),
        Some(session.session_id.as_str())
    );
    assert!(!session.resumed, "全新会话不得标记为恢复");

    let tools = harness
        .client
        .tools_list(Some(&session.session_id))
        .await
        .expect("tools.list");
    assert!(!tools.is_empty(), "工具目录不得为空");
    assert!(tools.iter().all(|tool| !tool.name.is_empty()));

    let pong = harness.client.health_ping().await.expect("health.ping");
    assert_eq!(pong["status"], "ok");

    // D9 边界：print 模式适配器无交互审批通道，permission.resolve 明确声明不产生决议。
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
    let Some(mut harness) = common::ClaudeHarness::launch(&[]).await else {
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
        .send(&session.session_id, "m2-02-stream", "chat")
        .await
        .expect("session.send ack");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "ack 必须是快路径（不等模型；实测 {:?}）",
        started.elapsed()
    );
    assert!(!ack.duplicate);

    let outcome = harness
        .wait_outcome(&ack.run_id, Duration::from_secs(20))
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
    let deltas = types
        .iter()
        .filter(|kind| kind.as_str() == "message.delta")
        .count();
    assert!(deltas > 1, "必须逐 token 流式（delta 数 {deltas}）");
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
    assert!(concatenated.contains("Aether M2-02 fake"));
    assert!(usage.is_some(), "usage 必须随终态上报");
    println!("[m2-02] 流式 run 事件序列 = {types:?}");

    // 信封契约：v=1、runtime_id=claude-code、seq 会话内单调唯一。
    let run_events: Vec<_> = events
        .iter()
        .filter(|event| event.run_id.as_ref().map(|id| id.as_str()) == Some(ack.run_id.as_str()))
        .collect();
    assert!(run_events.iter().all(|event| event.v == 1));
    assert!(run_events
        .iter()
        .all(|event| event.runtime_id.as_str() == "claude-code"));
    let mut seqs: Vec<u64> = run_events.iter().map(|event| event.seq).collect();
    let unique: std::collections::BTreeSet<u64> = seqs.iter().copied().collect();
    assert_eq!(unique.len(), seqs.len(), "seq 必须唯一");
    seqs.sort_unstable();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "seq 必须单调"
    );
    // 真实适配器不产出 permission.request 通知（D9 信任边界，记录为证据）。
    assert!(
        harness.client.permission_requests().await.is_empty(),
        "Claude 适配器不得伪造权限回环"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn tool_calls_normal_and_failure_map_to_appendix_b_events() {
    let Some(mut harness) = common::ClaudeHarness::launch(&[]).await else {
        return;
    };
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");

    // ① 正常完成：tool.call_started → tool.call_completed。
    let normal = harness
        .client
        .send(&session.session_id, "m2-02-tool-normal", "tool:normal")
        .await
        .expect("send");
    let outcome = harness
        .wait_outcome(&normal.run_id, Duration::from_secs(20))
        .await
        .expect("终态");
    assert!(matches!(outcome, RunOutcome::Completed { .. }));
    let tool_types: Vec<String> = harness
        .client
        .run_event_types(&normal.run_id)
        .await
        .into_iter()
        .filter(|kind| kind.starts_with("tool."))
        .collect();
    assert_eq!(tool_types, vec!["tool.call_started", "tool.call_completed"]);

    // ② 执行失败：tool.call_started → tool.call_failed（tool_execution_failed）。
    let failed = harness
        .client
        .send(&session.session_id, "m2-02-tool-fail", "tool:fail")
        .await
        .expect("send");
    let outcome = harness
        .wait_outcome(&failed.run_id, Duration::from_secs(20))
        .await
        .expect("终态");
    assert!(matches!(outcome, RunOutcome::Completed { .. }));
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
    assert!(payload.error.message.contains("ENOENT"));
    assert_eq!(payload.tool_name, "Read");
    println!(
        "[m2-02] 工具失败事件 = tool_name={} error={} recoverable={}",
        payload.tool_name, payload.error.code, payload.error.recoverable
    );

    // `tools.list` 返回 CLI init 观察到的工具子集。
    let tools = harness
        .client
        .tools_list(Some(&session.session_id))
        .await
        .expect("tools.list");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    for expected in ["Bash", "Write"] {
        assert!(
            names.contains(&expected),
            "tools.list 缺少 {expected}: {names:?}"
        );
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn interrupt_within_5s_cancels_run_and_reaps_cli_tree() {
    let Some(mut harness) = common::ClaudeHarness::launch(&[]).await else {
        return;
    };
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");
    let ack = harness
        .client
        .send(&session.session_id, "m2-02-interrupt", "tool:slow")
        .await
        .expect("send");
    assert!(
        harness
            .client
            .wait_for_event(
                &ack.run_id,
                EventType::ToolCallStarted,
                Duration::from_secs(10)
            )
            .await,
        "工具必须进入 started"
    );
    let pid = common::wait_for_async(
        || async { harness.last_fake_pid().is_some() },
        Duration::from_secs(10),
    )
    .await;
    assert!(pid, "fake-claude pid 必须落盘");
    let fake_pid = harness.last_fake_pid().expect("pid");

    let started = Instant::now();
    let interrupted = harness
        .client
        .interrupt(&session.session_id)
        .await
        .expect("session.interrupt（5s 超时）");
    assert_eq!(interrupted["interrupted"], true);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "interrupt 必须在 5s 内返回"
    );

    let outcome = harness
        .wait_outcome(&ack.run_id, Duration::from_secs(10))
        .await
        .expect("中断后必须收口");
    assert!(
        matches!(outcome, RunOutcome::Cancelled { .. }),
        "期望 run.cancelled，实际 {outcome:?}"
    );
    let events = harness.client.events().await;
    let failed = events
        .iter()
        .find(|event| {
            event.run_id.as_ref().map(|id| id.as_str()) == Some(ack.run_id.as_str())
                && event.event_type() == EventType::ToolCallFailed
        })
        .expect("中断时在途工具必须收口");
    let EventPayload::ToolCallFailed(payload) = &failed.payload else {
        panic!("payload 类型不符");
    };
    assert_eq!(payload.error.code, "timeout");
    assert!(payload.error.message.contains("abort"));

    // CLI 进程树回收（D5：禁止裸 kill 单 PID）。
    assert!(
        common::wait_for_async(
            || async { !common::pid_alive(fake_pid) },
            Duration::from_secs(10)
        )
        .await,
        "fake-claude 进程必须被整树回收（pid={fake_pid} 仍存活）"
    );

    let no_active = harness
        .client
        .interrupt(&session.session_id)
        .await
        .expect("再次 interrupt");
    assert_eq!(no_active["interrupted"], false);

    harness.shutdown().await;
}

#[tokio::test]
async fn exception_paths_api_error_orphan_exit_unhealthy_and_spawn_failure() {
    let Some(mut harness) = common::ClaudeHarness::launch(&[]).await else {
        return;
    };
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");

    // result.is_error（中转错误）→ run.failed(api_error, recoverable)。
    let api = harness
        .client
        .send(&session.session_id, "m2-02-api", "fail:api-error")
        .await
        .expect("send");
    let outcome = harness
        .wait_outcome(&api.run_id, Duration::from_secs(20))
        .await
        .expect("终态");
    match outcome {
        RunOutcome::Failed { error } => {
            assert_eq!(error.code, "api_error");
            assert!(error.message.contains("503"));
            assert!(error.recoverable);
        }
        other => panic!("期望 run.failed(api_error)，实际 {other:?}"),
    }

    // 进程退出但无 result → run.failed(cli_exit)。
    let orphan = harness
        .client
        .send(&session.session_id, "m2-02-orphan", "fail:no-result")
        .await
        .expect("send");
    match harness
        .wait_outcome(&orphan.run_id, Duration::from_secs(20))
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

    // stdout 连续坏行（≥20）→ 判不健康收口。
    let unhealthy = harness
        .client
        .send(&session.session_id, "m2-02-unhealthy", "fail:bad-json")
        .await
        .expect("send");
    match harness
        .wait_outcome(&unhealthy.run_id, Duration::from_secs(20))
        .await
        .expect("终态")
    {
        RunOutcome::Failed { error } => {
            assert_eq!(error.code, "cli_exit");
            assert!(error.message.contains("20"), "message={}", error.message);
        }
        other => panic!("期望不健康 run.failed，实际 {other:?}"),
    }

    // 会话不存在 → 应用码 1005；未知方法 → -32601（不断连）。
    let missing = harness
        .client
        .send(
            "11111111-2222-3333-4444-555555555555",
            "m2-02-missing",
            "chat",
        )
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
    // 未知方法不断连。
    let pong = harness.client.health_ping().await.expect("仍在服务");
    assert_eq!(pong["status"], "ok");

    harness.shutdown().await;

    // 启动失败（不存在可执行文件）→ run.failed(spawn_failed)。
    let Some(mut broken) =
        common::ClaudeHarness::launch(&["--claude-bin", "aether-missing-claude-binary-xyz"]).await
    else {
        return;
    };
    let session = broken
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");
    let ack = broken
        .client
        .send(&session.session_id, "m2-02-spawn", "chat")
        .await
        .expect("ack 仍应返回");
    match broken
        .wait_outcome(&ack.run_id, Duration::from_secs(20))
        .await
        .expect("终态")
    {
        RunOutcome::Failed { error } => assert_eq!(error.code, "spawn_failed"),
        other => panic!("期望 run.failed(spawn_failed)，实际 {other:?}"),
    }
    broken.shutdown().await;
}

#[tokio::test]
async fn idempotent_send_and_dispose_semantics() {
    let Some(mut harness) = common::ClaudeHarness::launch(&[]).await else {
        return;
    };
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");

    let first = harness
        .client
        .send(&session.session_id, "m2-02-dup", "chat")
        .await
        .expect("send");
    let duplicate = harness
        .client
        .send(&session.session_id, "m2-02-dup", "chat")
        .await
        .expect("重发");
    assert!(duplicate.duplicate, "相同 client_msg_id 必须幂等命中");
    assert_eq!(duplicate.run_id, first.run_id);
    assert!(harness
        .wait_outcome(&first.run_id, Duration::from_secs(20))
        .await
        .is_some());

    let second = harness
        .client
        .send(&session.session_id, "m2-02-dup-2", "chat")
        .await
        .expect("send 2");
    assert_ne!(second.run_id, first.run_id);
    assert!(harness
        .wait_outcome(&second.run_id, Duration::from_secs(20))
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
        .send(&session.session_id, "m2-02-after", "chat")
        .await
        .expect_err("dispose 后会话不可用");
    match after {
        SessionClientError::Request(RequestError::Rpc(rpc)) => assert_eq!(rpc.code, 1005),
        other => panic!("错误类型不符: {other:?}"),
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn mode_r_resume_across_adapter_process_restart() {
    // 适配器进程 1：创建原生会话并记忆口令。
    let Some(mut first) = common::ClaudeHarness::launch(&[]).await else {
        return;
    };
    let home = first.home.clone();
    let session = first
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");
    let native_id = session
        .native_id
        .clone()
        .expect("native_id 必须返回（ADR-005）");
    let remember = first
        .client
        .send(
            &session.session_id,
            "m2-02-remember",
            "remember:AETHER-M2-02-TOKEN",
        )
        .await
        .expect("send");
    assert!(matches!(
        first
            .wait_outcome(&remember.run_id, Duration::from_secs(20))
            .await,
        Some(RunOutcome::Completed { .. })
    ));
    first.shutdown().await;
    drop(first);

    // 适配器进程 2（模拟运行时重启）：仅凭 native_id 恢复并续聊。
    let Some(mut second) = common::ClaudeHarness::launch_with_home(home, &[]).await else {
        return;
    };
    let resumed = second
        .client
        .create_session(None, Some(&native_id), None)
        .await
        .expect("session.create(native_id)");
    assert!(resumed.resumed, "必须标记 Mode R 恢复");
    assert_eq!(resumed.session_id, native_id);
    let replay = second
        .client
        .send(&resumed.session_id, "m2-02-replay", "recall")
        .await
        .expect("重放重试 send");
    match second
        .wait_outcome(&replay.run_id, Duration::from_secs(20))
        .await
        .expect("重放必须到达终态")
    {
        RunOutcome::Completed { assistant_text, .. } => {
            assert_eq!(
                assistant_text.as_deref(),
                Some("AETHER-M2-02-TOKEN"),
                "Mode R 恢复后必须命中原生上下文"
            );
        }
        other => panic!("期望重放完成，实际 {other:?}"),
    }
    second.shutdown().await;
}
