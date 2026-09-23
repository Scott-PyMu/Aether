//! M3-02：真实适配器会话链路集成测试（Mock 适配器 + 监督器 + 生命周期 + IPC 后端）。
//!
//! 覆盖生产接线（`docs/M2-02-证据.md` §4 / `docs/M2-10-证据.md` §4.3 承接项）：
//! - `RunExecutor` → 适配器会话客户端：`session.create` → `session.send`（流式）→ 终态；
//! - 适配器增量事件（`message.delta`）经归属重写（核心 `session_id`/`run_id`）进入事件
//!   管线（先日志后广播），终稿由核心 `message.completed` 落库；
//! - 中断：`session_interrupt` → 取消令牌命中 → 适配器 `session.interrupt` → 核心
//!   `run.cancelled`（run 行 `cancelled`）；
//! - `messages_page` 可读回完整事件流（补读契约的生产路径）。
//!
//! 运行：`AETHER_MOCK_ADAPTER=<Bun 编译产物> cargo test -p aether-tauri --test
//! m3_02_adapter_executor`（由 `scripts/test/m3-02/verify-m3-02.mjs` 构建并设置；
//! 未设置且未要求时显式跳过，`AETHER_REQUIRE_MOCK_ADAPTER=1` 时缺路径直接失败）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, Supervisor};
use aether_control::{EventPipeline, LifecycleConfig, SessionManager, SystemClock};
use aether_core::SessionId;
use aether_store::{ReadPool, WriteQueue};
use aether_tauri::adapter_executor::AdapterRunExecutor;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    MessagesPageRequest, SessionCreateRequest, SessionIdRequest, SessionSendRequest,
};
use aether_tauri::runtime_control::{boot_supervisor, run_supervisor_startup};
use aether_tauri::session_backend::SessionBackend;
use serde_json::Value;
use tempfile::TempDir;

fn mock_binary() -> Option<PathBuf> {
    match std::env::var_os("AETHER_MOCK_ADAPTER") {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            if std::env::var("AETHER_REQUIRE_MOCK_ADAPTER").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_MOCK_ADAPTER=1 但 AETHER_MOCK_ADAPTER 未设置");
            }
            eprintln!(
                "SKIP：AETHER_MOCK_ADAPTER 未设置（运行 pnpm verify:m3-02 构建 Mock 后执行）"
            );
            None
        }
    }
}

struct Harness {
    #[allow(dead_code)]
    dir: TempDir,
    #[allow(dead_code)]
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    pipeline: EventPipeline,
    reads: ReadPool,
    #[allow(dead_code)]
    write: WriteQueue,
    manager: SessionManager,
    backend: Arc<dyn IpcBackend>,
    supervisor: Arc<Supervisor>,
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn harness(binary: &Path) -> Harness {
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let CoreBoot {
        storage: slot,
        reads,
        write,
        pipeline,
        ..
    } = boot_core_full(
        dir.path(),
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("启动核心（存储 + 管线）");
    let spec = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Mock", binary.to_path_buf())
            .official(true)
            .with_args(["--stream-deltas", "8", "--stream-interval-ms", "2"]),
    );
    let supervisor = Arc::new(
        boot_supervisor(vec![spec], Some(&dir.path().join("adapters.json"))).expect("构造监督器"),
    );
    // D2 启动序列尾段：孤儿清理 + 预热（initialize）。
    let startup = run_supervisor_startup(&supervisor, &handle).expect("启动序列尾段");
    for (runtime_id, outcome) in &startup.warmups {
        assert!(
            outcome.is_ready(),
            "{runtime_id} 预热必须 Ready：{outcome:?}"
        );
    }
    let executor = Arc::new(AdapterRunExecutor::new(
        Arc::clone(&supervisor),
        pipeline.clone(),
        reads.clone(),
        write.clone(),
        handle.clone(),
        None,
    ));
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        executor.clone(),
    );
    let backend: Arc<dyn IpcBackend> = Arc::new(SessionBackend::new(
        Arc::new(NotImplementedBackend),
        Some(manager.clone()),
        Some(executor),
        Some(reads.clone()),
        Some(Arc::clone(&supervisor)),
        handle,
    ));
    Harness {
        dir,
        runtime,
        slot,
        pipeline,
        reads,
        write,
        manager,
        backend,
        supervisor,
    }
}

fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

fn event_types(events: &[aether_core::EventEnvelope]) -> Vec<String> {
    events
        .iter()
        .map(|event| event.event_type().as_str().to_owned())
        .collect()
}

#[test]
fn mock_adapter_end_to_end_create_stream_interrupt_and_backfill() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);

    // 1) 创建会话（含会话级模型透传）。
    let created = h
        .backend
        .session_create(&SessionCreateRequest {
            runtime_id: "mock".to_owned(),
            title: "M3-02 E2E".to_owned(),
            workspace_id: None,
            model: Some("mock-model".to_owned()),
        })
        .expect("session_create");
    let session_id = created["id"].as_str().expect("会话 id").to_owned();
    let session = SessionId::new(session_id.clone()).unwrap();

    // 2) 发送：流式 run（8 delta + 终稿）。
    let ack = h
        .backend
        .session_send(&SessionSendRequest {
            session_id: session_id.clone(),
            text: "hello e2e".to_owned(),
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1N".to_owned(),
        })
        .expect("session_send");
    let run_id = ack["run_id"].as_str().expect("run_id").to_owned();
    assert!(
        wait_until(Duration::from_secs(30), || {
            let run = h
                .runtime
                .block_on(
                    h.reads
                        .run(&aether_core::RunId::new(run_id.clone()).unwrap()),
                )
                .ok()
                .flatten();
            run.map(|run| run.status) == Some(aether_core::RunStatus::Succeeded)
        }),
        "run 必须到达终态 succeeded"
    );

    // 3) 事件管线：delta 归属重写 + 核心终稿（先日志后广播）。
    //    run 行终态与 run.completed 事件之间存在落库顺序窗口，轮询等待事件收口。
    assert!(
        wait_until(Duration::from_secs(10), || {
            let frame = h.runtime.block_on(h.pipeline.readback(&session, 0)).ok();
            frame
                .map(|frame| {
                    frame
                        .events
                        .iter()
                        .any(|event| event.event_type() == aether_core::EventType::RunCompleted)
                })
                .unwrap_or(false)
        }),
        "run.completed 必须在窗口内落库"
    );
    let frame = h
        .runtime
        .block_on(h.pipeline.readback(&session, 0))
        .expect("补读");
    let types = event_types(&frame.events);
    assert!(types.iter().any(|kind| kind == "run.started"), "{types:?}");
    assert!(
        types.iter().any(|kind| kind == "message.delta"),
        "适配器增量必须进入管线：{types:?}"
    );
    assert!(
        types.iter().any(|kind| kind == "message.completed"),
        "{types:?}"
    );
    assert!(
        types.iter().any(|kind| kind == "run.completed"),
        "{types:?}"
    );
    for event in &frame.events {
        assert_eq!(event.session_id, session, "归属重写为核心会话");
        if let Some(event_run) = &event.run_id {
            assert_eq!(event_run.as_str(), run_id);
        }
    }
    // 助手终稿落库（message.completed 来自核心生命周期），且等于管线 delta 拼接
    // （流式与终稿一致性；核心 delta 合并只改批次不改内容）。
    let mut delta_text = String::new();
    for event in &frame.events {
        if let aether_core::EventPayload::MessageDelta(payload) = &event.payload {
            delta_text.push_str(&payload.text);
        }
    }
    assert!(!delta_text.is_empty(), "delta 拼接不得为空");
    let messages = h
        .runtime
        .block_on(h.reads.messages_page(&session, None, 50))
        .expect("消息分页");
    let assistant = messages
        .iter()
        .find(|message| message.role == aether_core::MessageRole::Assistant)
        .expect("助手消息必须落库");
    assert_eq!(
        assistant.content, delta_text,
        "助手终稿必须等于 delta 拼接（流式一致性）"
    );

    // 4) messages_page（生产补读路径）可读回事件流。
    let page = h
        .backend
        .messages_page(&MessagesPageRequest {
            session_id: session_id.clone(),
            last_seq: Some(0),
            limit: Some(500),
        })
        .expect("messages_page");
    assert_eq!(page["complete"], true);
    assert!(
        !page["events"].as_array().expect("events").is_empty(),
        "补读页不得为空"
    );

    // 5) 中断：长流式 run → session_interrupt → 适配器收口 → run.cancelled。
    let long_ack = h
        .backend
        .session_send(&SessionSendRequest {
            session_id: session_id.clone(),
            text: "long".to_owned(),
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1P".to_owned(),
        })
        .expect("session_send(long)");
    let long_run = long_ack["run_id"].as_str().expect("run_id").to_owned();
    assert!(wait_until(Duration::from_secs(10), || {
        !h.manager.clone().task_dumps().is_empty()
            || h.runtime
                .block_on(
                    h.reads
                        .run(&aether_core::RunId::new(long_run.clone()).unwrap()),
                )
                .ok()
                .flatten()
                .map(|run| run.status)
                == Some(aether_core::RunStatus::Running)
    }));
    let report = h
        .backend
        .session_interrupt(&SessionIdRequest {
            session_id: session_id.clone(),
        })
        .expect("session_interrupt");
    assert_eq!(report["interrupted_run"], long_run);
    assert!(
        wait_until(Duration::from_secs(10), || {
            let run = h
                .runtime
                .block_on(
                    h.reads
                        .run(&aether_core::RunId::new(long_run.clone()).unwrap()),
                )
                .ok()
                .flatten();
            run.map(|run| run.status) == Some(aether_core::RunStatus::Cancelled)
        }),
        "中断后 run 行必须为 cancelled"
    );

    // 6) dispose：适配器会话关闭（幂等）。
    let disposed = h
        .backend
        .session_dispose(&SessionIdRequest {
            session_id: session_id.clone(),
        })
        .expect("session_dispose");
    let status = disposed["status"].as_str().unwrap_or_default();
    assert!(matches!(status, "completed" | "cancelled"), "{disposed}");

    h.runtime.block_on(h.supervisor.shutdown_all());
    let _ = &h.slot;
    let _ = &h.dir;
}

/// 会话映射辅助（保持编译期断言：后端返回的会话 id 是 ULID）。
#[allow(dead_code)]
fn session_of(value: &Value) -> SessionId {
    SessionId::new(value["id"].as_str().unwrap_or_default().to_owned())
        .expect("会话 id 必须为 ULID")
}
