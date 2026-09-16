//! M1-09 DoD1：一致性测试（真实 Mock 适配器进程）。
//!
//! 覆盖：握手（10s/hello/major 校验）、流式（delta 拼接 + seq 单调）、中断（5s）、
//! dispose + shutdown（进程零残留），以及 DoD6 的 5 类工具调用注入清单。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use aether_adapters::{Method, RequestError};
use aether_core::{EventPayload, EventType};
use common::{rpc_code, MockHarness};

#[tokio::test]
async fn handshake_hello_within_10s_and_major_matches() {
    let Some(mut harness) = MockHarness::launch(&[]).await else {
        return;
    };
    let started = Instant::now();
    let hello = harness
        .connection
        .handshake()
        .await
        .expect("hello 必须在 10s 内到达且 major 兼容");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "hello 超出 10s"
    );
    assert_eq!(hello.protocol, "1.0");
    assert_eq!(aether_adapters::protocol_major(&hello.protocol), Some(1));
    assert_eq!(hello.runtime.name, "mock");
    assert!(!hello.runtime.version.is_empty());

    let status = harness.shutdown().await.expect("shutdown 后进程退出");
    assert!(status.success(), "退出码应为 0: {status:?}");
}

#[tokio::test]
async fn streaming_deltas_seq_monotonic_and_completed_matches() {
    let Some(mut harness) =
        MockHarness::launch_ready(&["--stream-deltas", "32", "--stream-interval-ms", "1"]).await
    else {
        return;
    };
    let session_id = harness.open_session().await;
    let run_id = harness.send(&session_id, "流式测试", "m1-09-stream").await;
    harness
        .drive_run(&run_id, None, Duration::from_secs(10))
        .await
        .expect("run 必须到达终态");

    let events = harness.run_events(&run_id);
    assert_eq!(
        events.first().map(|e| e.event_type()),
        Some(EventType::RunStarted)
    );
    assert_eq!(
        events.last().map(|e| e.event_type()),
        Some(EventType::RunCompleted)
    );

    let deltas: Vec<&EventPayload> = events
        .iter()
        .map(|event| &event.payload)
        .filter(|payload| matches!(payload, EventPayload::MessageDelta(_)))
        .collect();
    assert_eq!(deltas.len(), 32, "delta 数量应与请求一致（不丢帧）");

    let mut concatenated = String::new();
    for payload in &deltas {
        if let EventPayload::MessageDelta(delta) = payload {
            concatenated.push_str(&delta.text);
        }
    }
    let completed = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::MessageCompleted(payload) => Some(payload),
            _ => None,
        })
        .expect("必须收到 message.completed 终稿");
    assert_eq!(
        completed.message.content, concatenated,
        "终稿必须等于 delta 拼接"
    );

    // seq 会话内单调唯一；信封字段与 D4 一致。
    let mut seqs: Vec<u64> = events.iter().map(|event| event.seq).collect();
    let unique: std::collections::BTreeSet<u64> = seqs.iter().copied().collect();
    assert_eq!(unique.len(), seqs.len(), "seq 必须唯一");
    seqs.sort_unstable();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "seq 必须单调"
    );
    assert!(events.iter().all(|event| event.v == 1));
    assert!(events
        .iter()
        .all(|event| event.runtime_id.as_str() == "mock"));

    let status = harness.shutdown().await.expect("优雅退出");
    assert!(status.success());
}

#[tokio::test]
async fn interrupt_stops_run_within_5s() {
    let Some(mut harness) = MockHarness::launch_ready(&["--long-stream-interval-ms", "50"]).await
    else {
        return;
    };
    let session_id = harness.open_session().await;
    let run_id = harness.send(&session_id, "long", "m1-09-interrupt").await;

    harness
        .wait_for_event(
            &run_id,
            EventType::MessageDelta,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("长流必须产生 delta");

    let started = Instant::now();
    let interrupted = harness.interrupt(&session_id).await;
    assert_eq!(interrupted["interrupted"], true);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "session.interrupt 必须在 5s 内返回"
    );

    harness
        .drive_run(&run_id, None, Duration::from_secs(5))
        .await
        .expect("中断后必须收口");
    let types = harness.run_types(&run_id);
    assert_eq!(types.last().map(String::as_str), Some("run.cancelled"));

    let status = harness.shutdown().await.expect("优雅退出");
    assert!(status.success());
}

#[tokio::test]
async fn dispose_session_then_shutdown_exits_zero() {
    let Some(mut harness) = MockHarness::launch_ready(&[]).await else {
        return;
    };
    let session_id = harness.open_session().await;
    let disposed = harness
        .connection
        .request(
            Method::SessionDispose,
            serde_json::json!({"session_id": session_id}),
        )
        .await
        .expect("session.dispose（15s 超时内）");
    assert_eq!(disposed["disposed"], true);

    let error = harness
        .connection
        .request(
            Method::SessionSend,
            serde_json::json!({"session_id": session_id, "client_msg_id": "after-dispose", "text": "hi"}),
        )
        .await
        .expect_err("dispose 后会话不可用");
    assert_eq!(rpc_code(&error), 1005, "会话不存在应为应用码 1005");

    let status = harness.shutdown().await.expect("优雅退出");
    assert_eq!(status.code(), Some(0));
}

#[tokio::test]
async fn unknown_method_gets_32601_without_disconnect() {
    let Some(mut harness) = MockHarness::launch_ready(&[]).await else {
        return;
    };
    let error = harness
        .connection
        .request_raw(
            "session.listen",
            serde_json::json!({}),
            Duration::from_secs(5),
        )
        .await
        .expect_err("未知方法必须回错误");
    match error {
        RequestError::Rpc(rpc) => assert_eq!(rpc.code, -32601, "未知方法必须回 -32601"),
        other => panic!("错误类型不符: {other:?}"),
    }
    let pong = harness
        .connection
        .request(Method::HealthPing, serde_json::json!({}))
        .await
        .expect("未知方法不得断连");
    assert_eq!(pong["status"], "ok");

    let status = harness.shutdown().await.expect("优雅退出");
    assert!(status.success());
}

// ===== DoD6：5 类工具调用注入清单（真实 Mock 进程）=====

#[tokio::test]
async fn tool_scenario_1_normal_completion() {
    let Some(mut harness) = MockHarness::launch_ready(&[]).await else {
        return;
    };
    let session_id = harness.open_session().await;
    let run_id = harness
        .send(&session_id, "tool:normal", "m1-09-tool-1")
        .await;
    harness
        .drive_run(&run_id, None, Duration::from_secs(10))
        .await
        .expect("终态");
    assert_eq!(
        harness.tool_sequence(&run_id),
        vec!["tool.call_started", "tool.call_completed"],
        "① 正常完成序列"
    );
    assert_eq!(
        harness.run_types(&run_id).last().map(String::as_str),
        Some("run.completed")
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn tool_scenario_2_execution_failure() {
    let Some(mut harness) = MockHarness::launch_ready(&[]).await else {
        return;
    };
    let session_id = harness.open_session().await;
    let run_id = harness.send(&session_id, "tool:fail", "m1-09-tool-2").await;
    harness
        .drive_run(&run_id, None, Duration::from_secs(10))
        .await
        .expect("终态");
    assert_eq!(
        harness.tool_sequence(&run_id),
        vec!["tool.call_started", "tool.call_failed"],
        "② 执行失败序列"
    );
    let failed = harness
        .payload_of(&run_id, EventType::ToolCallFailed)
        .expect("tool.call_failed");
    if let EventPayload::ToolCallFailed(payload) = &failed.payload {
        assert_eq!(payload.error.code, "tool_execution_failed");
        assert!(payload.error.recoverable);
    } else {
        panic!("payload 类型不符");
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn tool_scenario_3_timeout_interrupted() {
    let Some(mut harness) = MockHarness::launch_ready(&[]).await else {
        return;
    };
    let session_id = harness.open_session().await;
    let run_id = harness
        .send(&session_id, "tool:timeout", "m1-09-tool-3")
        .await;
    harness
        .wait_for_event(
            &run_id,
            EventType::ToolCallStarted,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("tool.call_started");
    let interrupted = harness.interrupt(&session_id).await;
    assert_eq!(interrupted["interrupted"], true);
    harness
        .drive_run(&run_id, None, Duration::from_secs(5))
        .await
        .expect("中断收口");
    assert_eq!(
        harness.tool_sequence(&run_id),
        vec!["tool.call_started", "tool.call_failed"],
        "③ 超时中断序列"
    );
    let failed = harness
        .payload_of(&run_id, EventType::ToolCallFailed)
        .expect("tool.call_failed");
    if let EventPayload::ToolCallFailed(payload) = &failed.payload {
        assert_eq!(payload.error.code, "timeout");
        assert!(
            payload.error.message.contains("abort"),
            "超时错误需体现 abort"
        );
    } else {
        panic!("payload 类型不符");
    }
    assert_eq!(
        harness.run_types(&run_id).last().map(String::as_str),
        Some("run.cancelled")
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn tool_scenario_4_permission_allow() {
    let Some(mut harness) = MockHarness::launch_ready(&[]).await else {
        return;
    };
    let session_id = harness.open_session().await;
    let run_id = harness
        .send(&session_id, "tool:permission-allow", "m1-09-tool-4")
        .await;
    harness
        .drive_run(&run_id, Some(("allow", "once")), Duration::from_secs(10))
        .await
        .expect("终态");
    assert_eq!(
        harness.tool_sequence(&run_id),
        vec![
            "permission.requested",
            "permission.resolved",
            "tool.call_completed"
        ],
        "④ 权限允许序列（M1 预置口径）"
    );
    let resolved = harness
        .payload_of(&run_id, EventType::PermissionResolved)
        .expect("permission.resolved");
    if let EventPayload::PermissionResolved(payload) = &resolved.payload {
        assert_eq!(payload.decision, aether_core::PermissionDecision::Allow);
    } else {
        panic!("payload 类型不符");
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn tool_scenario_5_permission_deny() {
    let Some(mut harness) = MockHarness::launch_ready(&[]).await else {
        return;
    };
    let session_id = harness.open_session().await;
    let run_id = harness
        .send(&session_id, "tool:permission-deny", "m1-09-tool-5")
        .await;
    harness
        .drive_run(&run_id, Some(("deny", "once")), Duration::from_secs(10))
        .await
        .expect("终态");
    assert_eq!(
        harness.tool_sequence(&run_id),
        vec![
            "permission.requested",
            "permission.resolved",
            "tool.call_failed"
        ],
        "⑤ 权限拒绝序列（M1 预置口径）"
    );
    let resolved = harness
        .payload_of(&run_id, EventType::PermissionResolved)
        .expect("permission.resolved");
    if let EventPayload::PermissionResolved(payload) = &resolved.payload {
        assert_eq!(payload.decision, aether_core::PermissionDecision::Deny);
    } else {
        panic!("payload 类型不符");
    }
    let failed = harness
        .payload_of(&run_id, EventType::ToolCallFailed)
        .expect("tool.call_failed");
    if let EventPayload::ToolCallFailed(payload) = &failed.payload {
        assert_eq!(payload.error.code, "denied");
    } else {
        panic!("payload 类型不符");
    }
    harness.shutdown().await;
}
