//! M2-09 DoD2：`artifact_ref` 大附件引用流端到端（真实 Mock 进程）。
//!
//! 覆盖：
//! - 附件数据体**不进入线协议**：3MiB 附件只落 artifacts 文件，线协议只携带
//!   `artifact_ref` 引用帧（路径 + 元数据，<1MiB）——标记字节在任何已解析帧中 0 命中；
//! - 附件**不落库**：aether-adapters 链路无存储依赖（静态检查见 verify 脚本），
//!   数据体不跨线协议 ⇒ 永远无法进入 events 表（附录 B 亦无附件事件类型）；
//! - `ArtifactValidator` 路径安全：`..` 逃逸引用帧被拒绝（连接不断开，按消费侧拒绝）；
//! - 监督器附件目录注入：`RuntimeSpec::with_artifacts_dir` 创建
//!   `<root>/<runtime_id>/` 并经 `AETHER_ARTIFACTS_DIR` 注入适配器进程
//!   （以真实 Mock 的附件写入行为证明：Mock 仅在收到该环境变量时写文件）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aether_adapters::supervisor::{
    AdapterLedger, NoopObserver, RuntimeManifest, RuntimeSpec, RuntimeSupervisor, StartOutcome,
    SupervisorConfig, SysinfoProbe, ENV_ARTIFACTS_DIR,
};
use aether_adapters::{AdapterNotification, ArtifactError, ArtifactValidator, Method};
use aether_core::{EventType, RuntimeId, RuntimeStatus};
use serde_json::json;
use tokio::sync::Mutex;
use common::{unique_temp_dir, MockHarness};

/// 附件标记字节（Mock 写入附件内容的锚点；断言其在任何帧中 0 命中）。
const ARTIFACT_MARKER: &[u8] = b"AETHER_MOCK_ARTIFACT_MARKER";
/// Mock 默认附件字节数（3MiB > 2MiB 帧上限：证明数据体不可能走线协议）。
const ARTIFACT_BYTES: usize = 3 * 1024 * 1024;

fn artifacts_root(tag: &str) -> PathBuf {
    let root = unique_temp_dir(&format!("m2-09-{tag}"));
    std::fs::create_dir_all(root.join("mock")).unwrap();
    root
}

/// DoD2：附件引用流正常——3MiB 附件仅存 artifacts 路径，线协议只载引用帧。
#[tokio::test]
async fn attachment_body_lives_in_artifacts_dir_and_only_ref_on_wire() {
    let root = artifacts_root("flow");
    let artifacts_dir = root.join("mock");
    let Some(mut harness) = MockHarness::launch_with_env(
        &[],
        &[(OsString::from(ENV_ARTIFACTS_DIR), artifacts_dir.as_os_str().to_os_string())],
    )
    .await
    else {
        return;
    };
    harness.connection.handshake().await.expect("hello");
    let session_id = harness.open_session().await;
    let run_id = harness.send(&session_id, "artifact:big.png", "m2-09-artifact").await;

    // 驱动 run 到终态（run.completed），期间收集事件与引用帧。
    harness
        .drive_run(&run_id, Duration::from_secs(15))
        .await
        .expect("附件 run 必须收到终态");
    assert_eq!(
        harness.run_types(&run_id).last().map(String::as_str),
        Some("run.completed"),
        "附件 run 终态必须是 completed"
    );

    // 1) 引用帧到达且形状正确（路径 + 元数据；数据体不在此处）。
    assert_eq!(
        harness.artifact_refs.len(),
        1,
        "必须恰好收到 1 条 artifact_ref 引用帧（实际 {}）",
        harness.artifact_refs.len()
    );
    let artifact_ref = &harness.artifact_refs[0];
    assert_eq!(artifact_ref.session_id.as_deref(), Some(session_id.as_str()));
    assert_eq!(artifact_ref.run_id.as_deref(), Some(run_id.as_str()));
    assert_eq!(artifact_ref.refs.len(), 1);
    let entry = &artifact_ref.refs[0];
    assert_eq!(entry.path, "big.png");
    assert_eq!(entry.size, ARTIFACT_BYTES as u64);
    assert_eq!(entry.kind.as_deref(), Some("image/png"));
    let wire_bytes = serde_json::to_vec(artifact_ref).unwrap().len();
    assert!(
        wire_bytes < 1024 * 1024,
        "引用帧必须 <1MiB（D6 契约；实际 {wire_bytes} 字节）"
    );

    // 2) 校验器接受：canonicalize 后在 artifacts 根目录内、尺寸一致。
    let validator = ArtifactValidator::new(artifacts_dir.clone());
    let validated = validator.validate(artifact_ref).expect("引用必须通过校验");
    assert_eq!(validated.len(), 1);
    assert_eq!(
        validated[0].absolute,
        artifacts_dir.join("big.png").canonicalize().unwrap()
    );

    // 3) 附件数据体仅存 artifacts 路径：文件存在、尺寸一致、标记锚点可读。
    let file = artifacts_dir.join("big.png");
    let meta = std::fs::metadata(&file).expect("附件文件必须存在于 artifacts 路径");
    assert_eq!(meta.len(), ARTIFACT_BYTES as u64, "附件文件尺寸必须与声明一致");
    let content = std::fs::read(&file).unwrap();
    assert!(
        content.starts_with(ARTIFACT_MARKER),
        "附件内容必须以标记开头（确认文件确为 Mock 写入的附件）"
    );

    // 4) 数据体不进入线协议：标记字节在所有已解析帧（事件信封/通知/错误记录）中 0 命中。
    let marker = String::from_utf8(ARTIFACT_MARKER.to_vec()).unwrap();
    for envelope in &harness.events {
        let raw = serde_json::to_string(&envelope.payload).unwrap_or_default();
        assert!(
            !raw.contains(&marker),
            "事件 payload 不得包含附件数据体: {}",
            envelope.event_type().as_str()
        );
    }
    for params in &harness.artifact_refs {
        let raw = serde_json::to_string(params).unwrap_or_default();
        assert!(!raw.contains(&marker), "引用帧不得携带附件数据体");
    }
    for line in harness.connection.recorded_errors() {
        assert!(!line.contains(&marker), "错误记录不得包含附件数据体");
    }
    assert_eq!(
        harness.connection.invalid_frames_total(),
        0,
        "引用帧不得计为无效帧"
    );

    // 5) 连接保持健康。
    let pong = harness
        .connection
        .request(aether_adapters::Method::HealthPing, serde_json::json!({}))
        .await
        .expect("引用流后连接必须可用");
    assert_eq!(pong["status"], "ok");
    harness.shutdown().await;
}

/// DoD2 负向：`artifact_ref` 引用帧路径逃逸（`..`）→ 消费侧校验拒绝；连接不断开。
#[tokio::test]
async fn artifact_ref_escape_path_is_rejected_by_validator_connection_survives() {
    let root = artifacts_root("escape");
    let Some(mut harness) = MockHarness::launch(&["--inject", "artifact-ref-outside"]).await else {
        return;
    };
    let notification = tokio::time::timeout(
        Duration::from_secs(10),
        harness.connection.next_notification(),
    )
    .await
    .expect("逃逸引用帧必须在 10s 内解析")
    .expect("应有通知");
    let artifact_ref = match notification {
        AdapterNotification::ArtifactRef(artifact_ref) => *artifact_ref,
        other => panic!("通知类型不符: {other:?}"),
    };
    assert_eq!(artifact_ref.refs[0].path, "../outside.bin");

    // 消费侧校验：`..` 段 → InvalidPath（拒绝；不落库、不广播、不产生事件）。
    let validator = ArtifactValidator::new(root.join("mock"));
    let error = validator.validate(&artifact_ref).expect_err("逃逸必须被拒绝");
    assert!(
        matches!(error, ArtifactError::InvalidPath { .. }),
        "逃逸拒绝类型不符: {error:?}"
    );

    // 连接不断开（路径校验在消费侧；线协议形状合法）。
    assert_eq!(harness.connection.invalid_frames_total(), 0);
    let pong = harness
        .connection
        .request(aether_adapters::Method::HealthPing, serde_json::json!({}))
        .await
        .expect("逃逸引用帧不得导致断连");
    assert_eq!(pong["status"], "ok");
    harness.shutdown().await;
}

/// DoD2 负向（线协议层）：形状非法的引用帧按「无效帧」计数（D6 与坏 JSON 同口径）。
#[tokio::test]
async fn malformed_artifact_ref_params_count_as_invalid_frame() {
    let root = artifacts_root("malformed");
    let Some(mut harness) =
        MockHarness::launch(&["--inject", "artifact-ref-malformed"]).await
    else {
        return;
    };
    harness.connection.handshake().await.expect("hello");
    assert!(
        common::wait_until(
            || harness.connection.invalid_frames_total() >= 1,
            Duration::from_secs(5)
        )
        .await,
        "形状非法引用帧必须计为无效帧"
    );
    let pong = harness
        .connection
        .request(aether_adapters::Method::HealthPing, serde_json::json!({}))
        .await
        .expect("单次无效帧不得断连");
    assert_eq!(pong["status"], "ok");
    harness.shutdown().await;
    let _ = root;
}

/// M2-09（D5/D6）：监督器 `with_artifacts_dir` 创建 `<root>/<runtime_id>/` 并经
/// `AETHER_ARTIFACTS_DIR` 注入适配器进程——以真实 Mock 的附件写入行为证明
/// （Mock 仅在收到该环境变量时写附件文件；未注入则 `artifact:` 触发 run.failed）。
#[tokio::test]
async fn supervisor_injects_artifacts_dir_env_and_creates_per_runtime_dir() {
    let Some(mock) = common::mock_binary() else {
        return;
    };
    let root = unique_temp_dir("m2-09-supervisor");
    let spec = RuntimeSpec::with_fresh_token(RuntimeManifest::new("mock", "Mock", &mock))
        .with_artifacts_dir(root.clone());
    let ledger_path = root.join("ledger.json");
    let ledger = Arc::new(Mutex::new(
        AdapterLedger::load(&ledger_path).expect("台账加载"),
    ));
    let supervisor = RuntimeSupervisor::new(
        RuntimeId::new("mock").unwrap(),
        spec,
        SupervisorConfig::d5(),
        Arc::new(NoopObserver),
        Arc::clone(&ledger),
        Arc::new(SysinfoProbe::new()),
    );

    let outcome = supervisor.start().await;
    assert!(matches!(outcome, StartOutcome::Ready), "启动失败: {outcome:?}");
    let expected_dir = root.join("mock");
    assert!(
        expected_dir.is_dir(),
        "必须创建 <artifacts_root>/<runtime_id> 目录: {}",
        expected_dir.display()
    );
    assert_eq!(supervisor.status().await, RuntimeStatus::Ready);

    // 经监督器连接驱动会话：Mock 收到 AETHER_ARTIFACTS_DIR 才会写附件文件。
    let connection = supervisor.connection().await.expect("连接可用");
    let created = connection
        .request(Method::SessionCreate, json!({"title": "m2-09-supervisor"}))
        .await
        .expect("session.create");
    let session_id = created["session_id"].as_str().expect("session_id").to_owned();
    let ack = connection
        .request(
            Method::SessionSend,
            json!({
                "session_id": session_id,
                "client_msg_id": "m2-09-supervisor",
                "text": "artifact:envproof.png:2048",
            }),
        )
        .await
        .expect("session.send ack");
    let run_id = ack["run_id"].as_str().expect("run_id").to_owned();

    // 附件文件必须出现在 `<root>/mock/envproof.png`（2048 字节）——证明环境注入生效。
    let attachment = expected_dir.join("envproof.png");
    assert!(
        common::wait_until(
            || attachment.exists(),
            Duration::from_secs(10)
        )
        .await,
        "Mock 必须把附件写入监督器注入的目录: {}",
        attachment.display()
    );
    assert_eq!(
        std::fs::metadata(&attachment).map(|meta| meta.len()).unwrap_or(0),
        2048,
        "附件字节数必须与引用帧声明一致"
    );
    let _ = run_id;
    let report = supervisor.shutdown().await;
    assert!(report.exited, "关闭必须终止进程: {report:?}");
}

/// 回归：既有事件类型不受影响（引用帧不产生新事件类型，附录 B 无附件事件）。
#[tokio::test]
async fn artifact_ref_flow_does_not_create_attachment_event_types() {
    let root = artifacts_root("types");
    let artifacts_dir = root.join("mock");
    let Some(mut harness) = MockHarness::launch_with_env(
        &[],
        &[(OsString::from(ENV_ARTIFACTS_DIR), artifacts_dir.as_os_str().to_os_string())],
    )
    .await
    else {
        return;
    };
    harness.connection.handshake().await.expect("hello");
    let session_id = harness.open_session().await;
    let run_id = harness.send(&session_id, "artifact:shot.png:1024", "m2-09-types").await;
    harness
        .drive_run(&run_id, Duration::from_secs(15))
        .await
        .expect("附件 run 必须收到终态");
    for envelope in &harness.events {
        assert!(
            aether_core::EventType::ALL
                .iter()
                .any(|known| known.as_str() == envelope.event_type().as_str()),
            "不得自造事件类型: {}",
            envelope.event_type().as_str()
        );
    }
    assert!(
        harness
            .events
            .iter()
            .all(|event| event.event_type() != EventType::Error),
        "附件流程不得产生 error 事件"
    );
    harness.shutdown().await;
}