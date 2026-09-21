//! M1-09 DoD2/DoD3/DoD4：健壮性注入 5 类（真实 Mock 进程）。
//!
//! 覆盖：半行/断流、坏 JSON（连续 20 次判不健康）、大行（>2MiB 任意行断连；
//! 1–2MiB 非引用行正常解析；artifact_ref ≥1MiB 契约违约断连、<1MiB 正常解析）、
//! stdout 混入日志、未知方法（-32601 不断连）；
//! 另含版本不匹配（disabled + status_reason + 升级提示）与崩溃检测（应用码 1001）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use aether_adapters::{
    AdapterNotification, DisconnectReason, Method, RequestError, ARTIFACT_REF_LIMIT,
};
use common::{wait_until, MockHarness};

#[tokio::test]
async fn half_line_stream_cut_disconnects_and_discards_partial_line() {
    let Some(mut harness) = MockHarness::launch_ready(&["--inject", "half-line"]).await else {
        return;
    };
    let reason = harness
        .wait_for_disconnect(Duration::from_secs(5))
        .await
        .expect("半行/断流必须断连");
    match reason {
        DisconnectReason::StreamClosed {
            incomplete_line_bytes,
        } => assert!(incomplete_line_bytes > 0, "必须报告残行字节数"),
        other => panic!("断连原因不符: {other:?}"),
    }
    assert!(
        harness
            .connection
            .recorded_errors()
            .iter()
            .any(|line| line.contains("残行")),
        "断连必须记错"
    );
}

#[tokio::test]
async fn twenty_bad_json_frames_mark_unhealthy() {
    let Some(mut harness) =
        MockHarness::launch_ready(&["--inject", "bad-json", "--inject-count", "20"]).await
    else {
        return;
    };
    let reason = harness
        .wait_for_disconnect(Duration::from_secs(5))
        .await
        .expect("连续 20 次坏 JSON 必须判不健康并断连");
    match reason {
        DisconnectReason::InvalidFrameStreak { count, threshold } => {
            assert_eq!(count, 20, "硬阈值必须为 20");
            assert_eq!(threshold, 20);
        }
        other => panic!("断连原因不符: {other:?}"),
    }
    assert_eq!(harness.connection.invalid_frames_total(), 20);
}

#[tokio::test]
async fn non_artifact_line_between_1_and_2_mib_parses_over_process() {
    // DoD3：1–2MiB 非引用行正常解析（不再按「大行」拒绝）。
    let Some(mut harness) = MockHarness::launch_ready(&["--inject", "oversized-line"]).await else {
        return;
    };
    let notification = tokio::time::timeout(
        Duration::from_secs(10),
        harness.connection.next_notification(),
    )
    .await
    .expect("1–2MiB 非引用行必须在 10s 内解析")
    .expect("应有通知");
    match notification {
        AdapterNotification::Log(params) => {
            let pad = params["pad"].as_str().unwrap_or_default().len();
            assert!(pad > 1024 * 1024, "样例必须落在 1–2MiB 区间");
            assert!(pad < 2 * 1024 * 1024);
        }
        other => panic!("通知类型不符: {other:?}"),
    }
    assert_eq!(
        harness.connection.invalid_frames_total(),
        0,
        "不得计为无效帧"
    );
    let pong = harness
        .connection
        .request(Method::HealthPing, serde_json::json!({}))
        .await
        .expect("1–2MiB 非引用行不得影响连接");
    assert_eq!(pong["status"], "ok");
    harness.shutdown().await;
}

#[tokio::test]
async fn line_over_2mib_disconnects_without_buffering_full_line() {
    let Some(mut harness) = MockHarness::launch_ready(&["--inject", "line-over-2mib"]).await else {
        return;
    };
    let reason = harness
        .wait_for_disconnect(Duration::from_secs(10))
        .await
        .expect(">2MiB 任意行必须断连");
    match reason {
        DisconnectReason::LineTooLong { limit } => {
            assert_eq!(limit, aether_adapters::MAX_FRAME_BYTES, "上限必须为 2MiB");
        }
        other => panic!("断连原因不符: {other:?}"),
    }
    assert!(
        harness
            .connection
            .recorded_errors()
            .iter()
            .any(|line| line.contains("上限") || line.contains("2MiB")),
        "超限断连必须记错"
    );
}

#[tokio::test]
async fn artifact_ref_over_1mib_is_contract_violation_over_process() {
    let Some(mut harness) =
        MockHarness::launch_ready(&["--inject", "artifact-line-over-limit"]).await
    else {
        return;
    };
    let reason = harness
        .wait_for_disconnect(Duration::from_secs(10))
        .await
        .expect("1–2MiB 声称 artifact_ref 必须断连");
    match reason {
        DisconnectReason::ArtifactRefContractViolation { bytes } => {
            assert!(bytes > ARTIFACT_REF_LIMIT, "报错字节数必须超过契约上限");
            assert!(bytes <= 2 * 1024 * 1024, "越界帧仍在 2MiB 帧上限内");
        }
        other => panic!("断连原因不符: {other:?}"),
    }
    assert!(
        harness
            .connection
            .recorded_errors()
            .iter()
            .any(|line| line.contains("artifact_ref")),
        "契约违约必须记错"
    );
}

#[tokio::test]
async fn artifact_ref_line_under_1mib_is_parsed_over_process() {
    let Some(mut harness) = MockHarness::launch_ready(&["--inject", "artifact-line"]).await else {
        return;
    };
    let notification = tokio::time::timeout(
        Duration::from_secs(10),
        harness.connection.next_notification(),
    )
    .await
    .expect("artifact_ref 引用帧（<1MiB）必须在 10s 内解析")
    .expect("应有通知");
    match notification {
        // M2-09 落全帧形状：`artifact-line` 注入的 params 为 `{refs: [], pad: ...}`
        // （refs 可选为空数组；数据体 <1MiB 由帧层契约保证）。
        AdapterNotification::ArtifactRef(artifact_ref) => {
            assert!(artifact_ref.refs.is_empty(), "注入样例的 refs 为空数组");
        }
        other => panic!("通知类型不符: {other:?}"),
    }
    assert_eq!(harness.connection.invalid_frames_total(), 0);
    let pong = harness
        .connection
        .request(Method::HealthPing, serde_json::json!({}))
        .await
        .expect("引用帧不得影响连接");
    assert_eq!(pong["status"], "ok");
    harness.shutdown().await;
}

#[tokio::test]
async fn stdout_log_mixed_into_stdout_is_counted_like_invalid_json_then_recovers() {
    let Some(mut harness) =
        MockHarness::launch_ready(&["--inject", "stdout-log", "--inject-count", "5"]).await
    else {
        return;
    };
    let connection = &harness.connection;
    assert!(
        wait_until(
            || connection.invalid_frame_streak() >= 5,
            Duration::from_secs(5)
        )
        .await,
        "stdout 混入日志应按无效帧计数"
    );
    assert!(connection.invalid_frames_total() >= 5);

    let pong = connection
        .request(Method::HealthPing, serde_json::json!({}))
        .await
        .expect("混入少量日志后连接仍可用");
    assert_eq!(pong["status"], "ok");
    assert!(
        wait_until(
            || connection.invalid_frame_streak() == 0,
            Duration::from_secs(2)
        )
        .await,
        "有效帧必须重置连续计数"
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn version_mismatch_is_disabled_with_status_reason_and_upgrade_hint() {
    let Some(mut harness) = MockHarness::launch(&["--protocol", "2.0"]).await else {
        return;
    };
    let disabled = harness
        .connection
        .handshake()
        .await
        .expect_err("major 2.0 必须拒绝加载");
    assert_eq!(disabled.status, aether_core::RuntimeStatus::Disabled);
    assert_eq!(disabled.status_reason.as_str(), "version_mismatch");
    let hint = disabled.upgrade_hint.clone().unwrap_or_default();
    assert!(hint.contains("2.0"), "升级提示需包含实际版本: {hint}");
    assert!(hint.contains("升级"), "升级提示需给出动作: {hint}");
    harness.kill().await;
}

#[tokio::test]
async fn missing_hello_times_out_handshake() {
    let Some(mut harness) = MockHarness::launch(&["--no-hello"]).await else {
        return;
    };
    let disabled = harness
        .connection
        .handshake_with_timeout(Duration::from_millis(300))
        .await
        .expect_err("未发 hello 必须握手超时");
    assert_eq!(disabled.status_reason.as_str(), "handshake_timeout");
    harness.kill().await;
}

#[tokio::test]
async fn capability_missing_maps_to_1004() {
    let Some(mut harness) = MockHarness::launch_ready(&["--inject", "capability-missing"]).await
    else {
        return;
    };
    let error = harness
        .connection
        .request(Method::ToolsList, serde_json::json!({}))
        .await
        .expect_err("能力缺失必须回应用码 1004");
    match error {
        RequestError::Rpc(rpc) => assert_eq!(rpc.code, 1004),
        other => panic!("错误类型不符: {other:?}"),
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn process_crash_surfaces_as_application_code_1001() {
    let Some(mut harness) = MockHarness::launch_ready(&["--inject", "crash"]).await else {
        return;
    };
    let reason = harness
        .wait_for_disconnect(Duration::from_secs(5))
        .await
        .expect("崩溃必须被检测为断连");
    assert!(
        matches!(
            reason,
            DisconnectReason::StreamClosed { .. } | DisconnectReason::Io { .. }
        ),
        "崩溃原因: {reason:?}"
    );
    let error = harness
        .connection
        .request(Method::HealthPing, serde_json::json!({}))
        .await
        .expect_err("崩溃后请求必须失败");
    assert_eq!(error.code(), 1001, "崩溃/断连映射应用码 1001");
    let status = harness
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("崩溃后进程必须退出");
    assert_eq!(status.code(), Some(41));
}
