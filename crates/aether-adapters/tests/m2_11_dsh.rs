//! M2-11 / ADR-008：DSH 适配器增强验收（DoD1–8）。
//!
//! 覆盖：
//! - DoD1 版本门闩：pin 不匹配 → `initialize` 1003 + 监督器 `disabled + version_mismatch`；
//! - DoD2 注入：插件落盘（无 BOM）/ overlay 生成；
//! - DoD3 去重：插件 delta 前缀消费 + final-only 后缀；
//! - DoD4 权限：`session/request_permission` 100% 经 `permission.request` 回环（零直通）；
//! - DoD5 兜底：丢弃 end 帧 / 截断通道 → 按 ACP final 重建；
//! - DoD6 通道清理：`session.dispose` 后残留通道数 0；
//! - DoD7 20 次回归：终态零挂起 + 帧级丢失率 ≤1% + 平均长度偏差 ≤5%（夹具口径）；
//! - DoD8 合规：不修改 DSH 源码（插件为独立包）；事件类型 ⊆ 附录 B（静态检查见 verify 脚本）。
//!
//! 运行：`AETHER_DSH_ADAPTER=<编译产物> cargo test -p aether-adapters --test m2_11_dsh -- --nocapture`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::session_client::{RunOutcome, SessionClientError};
use aether_adapters::supervisor::{
    AdapterLedger, AdmissionPolicy, HeartbeatConfig, RuntimeManifest, RuntimeSpec, StartOutcome,
    Supervisor, SupervisorConfig, SysinfoProbe, SystemTreeKiller, TerminationBudget,
};
use aether_adapters::{
    DisabledReason, PermissionGate, PermissionGateFuture, PermissionLoop, PermissionLoopDecision,
    PermissionLoopRequest, RequestError,
};
use aether_core::{EventPayload, EventType, PermissionScope, RuntimeStatus};
use serde_json::json;

/// 测试权限门（allow-once）：验证适配器请求形状可被 M2-10 `PermissionLoop` 直接消费。
struct AllowOnceGate;

impl PermissionGate for AllowOnceGate {
    fn decide(&self, _request: PermissionLoopRequest) -> PermissionGateFuture<'_> {
        Box::pin(async { Ok(PermissionLoopDecision::allow(Some(PermissionScope::Once))) })
    }
}

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
async fn initialize_reports_version_and_plugin_contract_with_injection_artifacts() {
    let Some(mut harness) = common::DshHarness::launch(&[]).await else {
        return;
    };
    let initialized = harness
        .client
        .initialize(json!({}))
        .await
        .expect("initialize");
    assert_eq!(initialized["acknowledged"], true);
    assert_eq!(initialized["dsh_version"], "0.1.5-rc.2");
    assert_eq!(initialized["plugin_contract"], "aether-dsh-stream@1");

    // DoD2：插件包落盘 + package.json 无 BOM + overlay insert 行。
    let plugin_pkg = harness
        .home
        .join("profiles/acp/node_modules/aether-dsh-stream/package.json");
    assert!(
        plugin_pkg.is_file(),
        "插件 package.json 必须落盘：{plugin_pkg:?}"
    );
    let bytes = std::fs::read(&plugin_pkg).expect("读取插件 package.json");
    assert!(
        !(bytes.len() >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF),
        "插件 package.json 不得含 UTF-8 BOM（spike 已知坑 19②）"
    );
    let patch = std::fs::read_to_string(harness.home.join("aether-dsh-acp.patch.yml"))
        .expect("overlay 文件");
    assert!(patch.contains("- insert:"));
    assert!(patch.contains("aether-dsh-stream"));
    assert!(
        patch.contains("- id: acp"),
        "provider/model overlay 必须在案"
    );

    let pong = harness.client.health_ping().await.expect("health.ping");
    assert_eq!(pong["status"], "ok");
    assert_eq!(pong["plugin_contract"], "aether-dsh-stream@1");
    assert!(pong["dsh_pid"].as_u64().unwrap_or(0) > 0);

    harness.shutdown().await;
}

#[tokio::test]
async fn streaming_prefix_dedupe_final_suffix_and_fallback() {
    let Some(mut harness) = common::DshHarness::launch(&[]).await else {
        return;
    };
    harness
        .client
        .initialize(json!({}))
        .await
        .expect("initialize");
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");

    // 默认：插件 token 级 delta 与 ACP committed 一致 → 拼接等于终稿。
    let ack = harness
        .client
        .send(&session.session_id, "m2-11-dsh-stream", "chat")
        .await
        .expect("send");
    assert!(matches!(
        harness
            .wait_outcome(&ack.run_id, Duration::from_secs(30))
            .await,
        Some(RunOutcome::Completed { .. })
    ));
    let events = harness.client.events().await;
    let deltas = delta_text(&events, &ack.run_id);
    let completed = harness
        .wait_outcome(&ack.run_id, Duration::from_secs(5))
        .await
        .and_then(|outcome| match outcome {
            RunOutcome::Completed { assistant_text, .. } => assistant_text,
            _ => None,
        })
        .unwrap_or_default();
    assert!(completed.contains("Aether M2-11 fake dsh baseline"));
    assert_eq!(deltas, completed, "拼接必须等于 ACP 终稿");
    let delta_count = harness
        .client
        .run_event_types(&ack.run_id)
        .await
        .iter()
        .filter(|kind| kind.as_str() == "message.delta")
        .count();
    assert!(
        delta_count > 3,
        "必须是 token 级增量（delta 数 {delta_count}）"
    );

    // final-only 后缀：committed 比流式多出的尾部必须补发为增量。
    let suffix = harness
        .client
        .send(&session.session_id, "m2-11-dsh-suffix", "dedupe:suffix")
        .await
        .expect("send");
    assert!(matches!(
        harness
            .wait_outcome(&suffix.run_id, Duration::from_secs(30))
            .await,
        Some(RunOutcome::Completed { .. })
    ));
    let events = harness.client.events().await;
    let suffix_deltas = delta_text(&events, &suffix.run_id);
    assert!(suffix_deltas.contains("final-only suffix"));
    assert_eq!(
        suffix_deltas.matches("final-only suffix").count(),
        1,
        "final-only 后缀不得重复"
    );

    // 丢弃 end 帧：不影响去重与终稿。
    let no_end = harness
        .client
        .send(&session.session_id, "m2-11-dsh-no-end", "no-end")
        .await
        .expect("send");
    assert!(matches!(
        harness
            .wait_outcome(&no_end.run_id, Duration::from_secs(30))
            .await,
        Some(RunOutcome::Completed { .. })
    ));
    let events = harness.client.events().await;
    assert_eq!(
        delta_text(&events, &no_end.run_id)
            .matches("dedupe")
            .count(),
        1
    );

    // 截断通道（前缀不连续）→ 兜底：终稿以 ACP final 为准。
    let dropped = harness
        .client
        .send(&session.session_id, "m2-11-dsh-drop", "drop-frames")
        .await
        .expect("send");
    let dropped_outcome = harness
        .wait_outcome(&dropped.run_id, Duration::from_secs(30))
        .await
        .expect("终态");
    match dropped_outcome {
        RunOutcome::Completed { assistant_text, .. } => {
            let completed = assistant_text.unwrap_or_default();
            assert!(completed.contains("Aether M2-11 fake dsh dedupe"));
            let events = harness.client.events().await;
            assert_ne!(
                delta_text(&events, &dropped.run_id),
                completed,
                "兜底场景下已流式内容不得冒充终稿（终稿取 ACP final）"
            );
        }
        other => panic!("期望 completed，实际 {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn tool_events_and_permission_loopback_are_full() {
    let Some(mut harness) = common::DshHarness::launch(&[]).await else {
        return;
    };
    harness
        .client
        .initialize(json!({}))
        .await
        .expect("initialize");
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");

    // 工具正常/失败序列（附录 B）。
    let normal = harness
        .client
        .send(&session.session_id, "m2-11-dsh-tool", "tool:normal")
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
        .send(&session.session_id, "m2-11-dsh-tool-fail", "tool:fail")
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

    // 权限回环：ask → allow / deny；100% 经 permission.request（零直通）。
    // allow 路径直接复用 M2-10 的 `PermissionLoop`（dispatch → 决议 → permission.resolve），
    // 以 `zero_passthrough()` 探针断言请求/决议/回执严格相等。
    let permission_loop = PermissionLoop::new(Arc::new(AllowOnceGate));
    let loop_probe = permission_loop.probe();
    let allow_ack = harness
        .client
        .send(&session.session_id, "m2-11-dsh-allow", "permission:allow")
        .await
        .expect("send");
    let request = harness
        .wait_permission_request(1, Duration::from_secs(10))
        .await
        .expect("permission.request 通知");
    assert_eq!(request["tool_name"], "edit");
    assert_eq!(request["resource"], "fs.write");
    assert_eq!(request["action"], "write");
    assert_eq!(request["target"], "a.txt");
    permission_loop.dispatch(
        Arc::clone(harness.client.connection()),
        request.clone(),
        Some("deepseek-harness".to_owned()),
    );
    let zero_passthrough = common::wait_for_async(
        || {
            let probe = loop_probe.clone();
            async move {
                let snapshot = probe.snapshot();
                snapshot.zero_passthrough() && snapshot.requests_received == 1
            }
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(
        zero_passthrough,
        "PermissionLoop 探针必须零直通（{:?}）",
        loop_probe.snapshot()
    );
    assert!(matches!(
        harness
            .wait_outcome(&allow_ack.run_id, Duration::from_secs(30))
            .await,
        Some(RunOutcome::Completed { .. })
    ));
    let allow_tools: Vec<String> = harness
        .client
        .run_event_types(&allow_ack.run_id)
        .await
        .into_iter()
        .filter(|kind| kind.starts_with("tool."))
        .collect();
    assert_eq!(
        allow_tools,
        vec!["tool.call_started", "tool.call_completed"]
    );

    let deny_ack = harness
        .client
        .send(&session.session_id, "m2-11-dsh-deny", "permission:deny")
        .await
        .expect("send");
    let request2 = harness
        .wait_permission_request(2, Duration::from_secs(10))
        .await
        .expect("第二条 permission.request");
    assert_ne!(request2["request_id"], request["request_id"]);
    let request2_id = request2["request_id"]
        .as_str()
        .expect("request_id")
        .to_owned();
    harness
        .client
        .resolve_permission(&request2_id, "deny", None)
        .await
        .expect("permission.resolve deny");
    assert!(matches!(
        harness
            .wait_outcome(&deny_ack.run_id, Duration::from_secs(30))
            .await,
        Some(RunOutcome::Completed { .. })
    ));
    let deny_events = harness.client.events().await;
    assert!(
        deny_events.iter().any(|event| {
            event.run_id.as_ref().map(|id| id.as_str()) == Some(deny_ack.run_id.as_str())
                && event.event_type() == EventType::ToolCallFailed
        }),
        "deny 后工具必须收口为 failed"
    );
    // 100% 回环：两次 ask 恰有两条 permission.request 通知（无直通、无重复）。
    assert_eq!(harness.client.permission_requests().await.len(), 2);

    println!("[m2-11 dsh] 权限回环 2/2（allow/deny），零直通");
    harness.shutdown().await;
}

#[tokio::test]
async fn interrupt_settles_within_5s_and_dispose_drops_channels() {
    let Some(mut harness) = common::DshHarness::launch(&[]).await else {
        return;
    };
    harness
        .client
        .initialize(json!({}))
        .await
        .expect("initialize");
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");

    let ack = harness
        .client
        .send(&session.session_id, "m2-11-dsh-cancel", "cancel:long")
        .await
        .expect("send");
    assert!(
        harness
            .client
            .wait_for_event(&ack.run_id, EventType::RunStarted, Duration::from_secs(10))
            .await
    );
    let started = Instant::now();
    let interrupted = harness
        .client
        .interrupt(&session.session_id)
        .await
        .expect("session.interrupt（5s 超时）");
    assert_eq!(interrupted["interrupted"], true);
    assert!(started.elapsed() <= Duration::from_secs(5));
    assert!(matches!(
        harness
            .wait_outcome(&ack.run_id, Duration::from_secs(10))
            .await,
        Some(RunOutcome::Cancelled { .. })
    ));

    // DoD6：dispose 后残留通道数 0（响应字段 + 后续查询双断言）。
    let chat = harness
        .client
        .send(&session.session_id, "m2-11-dsh-dispose", "chat")
        .await
        .expect("send");
    assert!(harness
        .wait_outcome(&chat.run_id, Duration::from_secs(30))
        .await
        .is_some());
    let disposed = harness
        .client
        .dispose(&session.session_id)
        .await
        .expect("session.dispose");
    assert_eq!(disposed["disposed"], true);
    assert_eq!(disposed["residual_channels"], 0);
    let after = harness
        .client
        .send(&session.session_id, "m2-11-dsh-after", "chat")
        .await
        .expect_err("dispose 后不可用");
    match after {
        SessionClientError::Request(RequestError::Rpc(rpc)) => assert_eq!(rpc.code, 1005),
        other => panic!("错误类型不符: {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn mode_r_resume_across_adapter_process_restart() {
    let home = common::unique_temp_dir("m2-11-dsh-mode-r");
    let Some(mut first) = common::DshHarness::launch_with_home(home.clone(), &[]).await else {
        return;
    };
    first
        .client
        .initialize(json!({}))
        .await
        .expect("initialize");
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
            "m2-11-dsh-remember",
            "remember:AETHER-DSH-TOKEN",
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

    let Some(mut second) = common::DshHarness::launch_with_home(home, &[]).await else {
        return;
    };
    second
        .client
        .initialize(json!({}))
        .await
        .expect("initialize");
    let resumed = second
        .client
        .create_session(None, Some(&native_id), None)
        .await
        .expect("session.create(native_id)");
    assert!(resumed.resumed, "session/resume 必须标记 Mode R");
    assert_eq!(resumed.session_id, native_id);
    let recall = second
        .client
        .send(&resumed.session_id, "m2-11-dsh-recall", "recall")
        .await
        .expect("send");
    match second
        .wait_outcome(&recall.run_id, Duration::from_secs(30))
        .await
        .expect("终态")
    {
        RunOutcome::Completed { assistant_text, .. } => assert_eq!(
            assistant_text.as_deref(),
            Some("AETHER-DSH-TOKEN"),
            "Mode R 必须命中原生上下文"
        ),
        other => panic!("期望 completed，实际 {other:?}"),
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

fn dsh_manifest(home: &std::path::Path, version: &str) -> Option<RuntimeManifest> {
    let adapter = common::dsh_adapter_binary()?;
    let fake = common::fake_dsh_server();
    let pid_file = home.join("pids.txt");
    let workspace = common::unique_temp_dir("m2-11-dsh-latch-ws");
    Some(
        RuntimeManifest::new("deepseek-harness", "DeepSeek Harness", adapter)
            .official(true)
            .with_args([
                "--dsh-bin".to_owned(),
                fake.to_string_lossy().into_owned(),
                "--dsh-node".to_owned(),
                common::node_binary(),
                "--dsh-home".to_owned(),
                home.to_string_lossy().into_owned(),
                "--dsh-profile".to_owned(),
                "acp".to_owned(),
                "--dsh-version".to_owned(),
                version.to_owned(),
                "--workspace".to_owned(),
                workspace.to_string_lossy().into_owned(),
            ])
            .with_env([(
                "FAKE_DSH_PID_FILE".to_owned(),
                pid_file.to_string_lossy().into_owned(),
            )]),
    )
}

/// DoD1：版本门闩失败 → 监督器 `disabled + status_reason=version_mismatch` + 升级提示。
#[tokio::test]
async fn version_latch_mismatch_disables_with_version_mismatch() {
    let home = common::unique_temp_dir("m2-11-dsh-latch-home");
    let Some(manifest) = dsh_manifest(&home, "0.1.1-rc.2") else {
        return;
    };
    let spec = RuntimeSpec::with_fresh_token(manifest);
    let ledger_dir = common::unique_temp_dir("m2-11-dsh-latch-ledger");
    let ledger = Arc::new(tokio::sync::Mutex::new(
        AdapterLedger::load(ledger_dir.join("adapters.json")).expect("台账"),
    ));
    let observer = Arc::new(common::RecordingObserver::new());
    let supervisor = Supervisor::new(
        vec![spec],
        test_config(),
        AdmissionPolicy::official(),
        observer,
        ledger,
        Arc::new(SysinfoProbe::new()),
        Arc::new(SystemTreeKiller),
    )
    .expect("监督器");

    let outcomes = supervisor.warmup_all().await;
    match &outcomes[0].1 {
        StartOutcome::Failed { reason, detail, .. } => {
            assert_eq!(*reason, DisabledReason::VersionMismatch);
            assert!(
                detail.contains("0.1.5-rc.2"),
                "升级提示必须含 pin：{detail}"
            );
        }
        other => panic!("期望 Failed(version_mismatch)，实际 {other:?}"),
    }
    let runtime = supervisor.get("deepseek-harness").expect("白名单命中");
    assert_eq!(runtime.status().await, RuntimeStatus::Disabled);
    assert_eq!(
        runtime.status_reason().await,
        Some(DisabledReason::VersionMismatch)
    );
    supervisor.shutdown_all().await;
}

/// DoD7：20 次回归（夹具口径）——终态零挂起、帧级丢失率 ≤1%、平均长度偏差 ≤5%。
#[tokio::test]
async fn twenty_run_regression_frame_loss_and_length_deviation() {
    let Some(mut harness) = common::DshHarness::launch(&[]).await else {
        return;
    };
    harness
        .client
        .initialize(json!({}))
        .await
        .expect("initialize");
    let session = harness
        .client
        .create_session(None, None, None)
        .await
        .expect("session.create");

    let total = 20usize;
    let mut completed = 0usize;
    let mut hang = 0usize;
    let mut expected_frames = 0usize;
    let mut received_frames = 0usize;
    let mut deviation_sum = 0.0f64;

    for index in 0..total {
        let ack = harness
            .client
            .send(
                &session.session_id,
                &format!("m2-11-dsh-rate-{index:02}"),
                "chat",
            )
            .await
            .expect("send");
        let outcome = harness
            .wait_outcome(&ack.run_id, Duration::from_secs(30))
            .await;
        let Some(RunOutcome::Completed { assistant_text, .. }) = outcome else {
            hang += 1;
            continue;
        };
        completed += 1;
        let completed_text = assistant_text.unwrap_or_default();
        let events = harness.client.events().await;
        let deltas = delta_text(&events, &ack.run_id);
        let received = harness
            .client
            .run_event_types(&ack.run_id)
            .await
            .iter()
            .filter(|kind| kind.as_str() == "message.delta")
            .count();
        // 夹具按 8 字符切片，期望帧数 = ceil(len/8)（与 committed 对齐）。
        let expected = completed_text.chars().count().div_ceil(8);
        expected_frames += expected;
        received_frames += received;
        let deviation = if completed_text.is_empty() {
            0.0
        } else {
            (deltas.chars().count() as f64 - completed_text.chars().count() as f64).abs()
                / completed_text.chars().count() as f64
        };
        deviation_sum += deviation;
        assert_eq!(deltas, completed_text, "run {index} 拼接必须等于终稿");
    }

    let loss_rate = if expected_frames == 0 {
        0.0
    } else {
        (expected_frames.saturating_sub(received_frames)) as f64 / expected_frames as f64
    };
    let deviation_avg = deviation_sum / total as f64;
    println!(
        "[m2-11 dsh DoD7] 完成 {completed}/{total}；挂起 {hang}；帧级丢失率 {loss_rate:.4}；平均长度偏差 {deviation_avg:.4}"
    );
    assert_eq!(completed, total, "终态零挂起（completed={completed}）");
    assert!(
        loss_rate <= 0.01,
        "帧级丢失率必须 ≤1%（实测 {loss_rate:.4}）"
    );
    assert!(
        deviation_avg <= 0.05,
        "平均长度偏差必须 ≤5%（实测 {deviation_avg:.4}）"
    );
    harness.shutdown().await;
}
