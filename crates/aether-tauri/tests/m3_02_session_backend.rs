//! M3-02：会话命令与消息分页后端集成测试。
//!
//! 覆盖：
//! - `messages_page`：按 `last_seq` 分页（升序、`complete` 判定，含
//!   `last_seq >= max_seq` 边界）；`last_seq` 缺省返回最近一页（events 与 messages
//!   均为**尾部** `limit` 条、升序）；`messages` 空值口径（最近一页 `Some([])` /
//!   补读页字段省略）；缺口 > 10k → `readback_gap_too_large`（同码透传）；
//! - `session_create`：`model` 透传落库（UI-05）；`session_list` 可见；
//! - `session_send` / `session_interrupt` / `session_dispose`：run 串行语义由
//!   `SessionManager` 承接，本层只验证命令接线与回执形状（真实适配器路径见
//!   `m3_02_adapter_executor.rs`）；
//! - `runtimes_list`：监督器注册表快照（含能力清单；未接线 → `core_not_ready`）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_control::{
    ExecutorFuture, ExecutorOutcome, LifecycleConfig, RunExecutor, RunRequest, SessionManager,
    SystemClock,
};
use aether_core::{Message, MessageId, MessageRole, SessionId};
use aether_store::{ReadPool, StoreCommand, WriteQueue};
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    MessagesPageRequest, SessionCreateRequest, SessionIdRequest, SessionListRequest,
    SessionSendRequest,
};
use aether_tauri::ipc::error::IpcErrorCode;
use aether_tauri::runtime_control::{boot_supervisor, mock_spec};
use aether_tauri::session_backend::SessionBackend;
use serde_json::{json, Value};
use tempfile::TempDir;

/// 测试注入的补读缺口上限（默认值等于核心 `READBACK_MAX_GAP`，由单测断言；
/// 此处用小值避免在测试中提交 1 万条事件——常量级调参，见任务证据）。
const GAP_LIMIT: u64 = 8;

/// 测试执行器：记录 `RunRequest` 后等待取消（用于 interrupt 语义）。
#[derive(Default)]
struct StubExecutor {
    requests: Mutex<Vec<RunRequest>>,
}

impl StubExecutor {
    fn requests(&self) -> Vec<RunRequest> {
        match self.requests.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl RunExecutor for StubExecutor {
    fn execute(&self, request: RunRequest) -> ExecutorFuture<'_> {
        Box::pin(async move {
            match self.requests.lock() {
                Ok(mut guard) => guard.push(request.clone()),
                Err(poisoned) => poisoned.into_inner().push(request.clone()),
            }
            request.cancel.cancelled().await;
            ExecutorOutcome::Cancelled {
                reason: Some("stub_interrupt".to_owned()),
            }
        })
    }
}

struct Harness {
    #[allow(dead_code)]
    dir: TempDir,
    #[allow(dead_code)]
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    pipeline: aether_control::EventPipeline,
    reads: ReadPool,
    #[allow(dead_code)]
    write: WriteQueue,
    #[allow(dead_code)]
    manager: SessionManager,
    backend: Arc<dyn IpcBackend>,
    stub: Arc<StubExecutor>,
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn harness() -> Harness {
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
    let supervisor = Arc::new(
        boot_supervisor(
            vec![mock_spec("mock", "definitely-missing-adapter-binary")],
            Some(&dir.path().join("adapters.json")),
        )
        .expect("构造监督器"),
    );
    let stub = Arc::new(StubExecutor::default());
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        stub.clone(),
    );
    let backend: Arc<dyn IpcBackend> = Arc::new(
        SessionBackend::new(
            Arc::new(NotImplementedBackend),
            Some(manager.clone()),
            None,
            Some(reads.clone()),
            Some(Arc::clone(&supervisor)),
            handle,
        )
        .with_gap_limit(GAP_LIMIT),
    );
    Harness {
        dir,
        runtime,
        slot,
        pipeline,
        reads,
        write,
        manager,
        backend,
        stub,
    }
}

fn event(index: usize, session: &str) -> Value {
    json!({
        "v": 1,
        "id": format!("01J{index:023}"),
        "session_id": session,
        "run_id": null,
        "runtime_id": "mock",
        "seq": 0,
        "ts": 1_700_000_000_000i64,
        "type": "log",
        "payload": { "level": "info", "message": format!("m3-02-{index}") },
    })
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

fn session_create(backend: &dyn IpcBackend, title: &str, model: Option<&str>) -> Value {
    let request = SessionCreateRequest {
        runtime_id: "mock".to_owned(),
        title: title.to_owned(),
        workspace_id: None,
        model: model.map(str::to_owned),
        thinking_depth: None,
    };
    backend.session_create(&request).expect("session_create")
}

/// 最近一页（`last_seq` 缺省）。
fn latest_page(h: &Harness, session_id: &str, limit: u32) -> Value {
    h.backend
        .messages_page(&MessagesPageRequest {
            session_id: session_id.to_owned(),
            last_seq: None,
            limit: Some(limit),
        })
        .expect("最近一页")
}

/// 响应 `messages` 数组的 seq 列表（升序断言用）。
fn message_seqs(page: &Value) -> Vec<u64> {
    page["messages"]
        .as_array()
        .expect("messages 数组")
        .iter()
        .map(|message| message["seq"].as_u64().expect("message.seq"))
        .collect()
}

#[test]
fn messages_page_pages_backfill_latest_and_guards_gap() {
    let h = harness();
    let session = "01J8ZQ5R0N7W9Y8X6V4T2S0K1M";
    for index in 1..=5 {
        h.runtime
            .block_on(h.pipeline.submit(event(index, session)))
            .expect("提交事件");
    }

    // 补读：seq > 2（升序、complete、max_seq）。
    let page = h
        .backend
        .messages_page(&MessagesPageRequest {
            session_id: session.to_owned(),
            last_seq: Some(2),
            limit: Some(10),
        })
        .expect("补读");
    assert_eq!(page["max_seq"], 5);
    assert_eq!(page["complete"], true);
    let seqs: Vec<u64> = page["events"]
        .as_array()
        .expect("events 数组")
        .iter()
        .map(|event| event["seq"].as_u64().expect("seq"))
        .collect();
    assert_eq!(seqs, vec![3, 4, 5]);

    // 最近一页：升序返回尾部。
    let latest = h
        .backend
        .messages_page(&MessagesPageRequest {
            session_id: session.to_owned(),
            last_seq: None,
            limit: Some(2),
        })
        .expect("最近一页");
    assert_eq!(latest["max_seq"], 5);
    assert_eq!(latest["complete"], true);
    let seqs: Vec<u64> = latest["events"]
        .as_array()
        .expect("events 数组")
        .iter()
        .map(|event| event["seq"].as_u64().expect("seq"))
        .collect();
    assert_eq!(seqs, vec![4, 5]);

    // 缺口超过注入上限 → readback_gap_too_large（同码透传；不返回事件）。
    // 默认上限等于核心 READBACK_MAX_GAP（10k），由下方 `default_gap_limit_*` 单测锁定；
    // 本用例注入小值以避免在集成测试中提交 1 万条事件。
    for index in 6..=(GAP_LIMIT as usize + 5) {
        h.runtime
            .block_on(h.pipeline.submit(event(index, session)))
            .expect("提交事件");
    }
    let error = h
        .backend
        .messages_page(&MessagesPageRequest {
            session_id: session.to_owned(),
            last_seq: Some(0),
            limit: Some(500),
        })
        .expect_err("缺口超过上限必须拒绝自动补发");
    assert_eq!(error.code, IpcErrorCode::ReadbackGapTooLarge);
    assert_eq!(error.code.as_str(), "readback_gap_too_large");

    // 边界内（gap == 上限）仍可补读。
    let page = h
        .backend
        .messages_page(&MessagesPageRequest {
            session_id: session.to_owned(),
            last_seq: Some(5),
            limit: Some(500),
        })
        .expect("边界内补读");
    assert_eq!(page["complete"], true);

    // `complete` 边界：last_seq > max_seq（越过最新）→ 空 events + complete=true。
    let boundary = h
        .backend
        .messages_page(&MessagesPageRequest {
            session_id: session.to_owned(),
            last_seq: Some(GAP_LIMIT + 100),
            limit: Some(500),
        })
        .expect("越过最新断点");
    assert_eq!(boundary["max_seq"], GAP_LIMIT + 5);
    assert_eq!(
        boundary["complete"], true,
        "越过最新视为已到最新：{boundary}"
    );
    assert_eq!(
        boundary["events"].as_array().expect("events 数组").len(),
        0,
        "越过最新无待补事件：{boundary}"
    );
}

#[test]
fn messages_page_latest_returns_tail_ascending_and_option_shape() {
    let h = harness();

    // 空值口径：最近一页无消息 → `messages` 字段存在且为 `[]`（`Some([])`）。
    // 以无事件、无消息的会话 id 覆盖完整形状（`max_seq=null`）。
    let empty = latest_page(&h, "01J8ZQ5R0N7W9Y8X6V4T2S0K9Z", 2);
    assert!(
        empty.get("messages").is_some(),
        "最近一页必须返回 messages 字段（空为 []）：{empty}"
    );
    assert_eq!(
        empty["messages"].as_array().expect("messages 数组").len(),
        0
    );
    assert_eq!(empty["max_seq"], Value::Null);
    assert_eq!(empty["complete"], true);

    // 创建会话并插入 3 条消息（store 按会话自增 seq → 1..3）。
    let created = session_create(h.backend.as_ref(), "M3-02 尾部消息", None);
    let session_id = created["id"].as_str().expect("会话 id").to_owned();
    for index in 1..=3u32 {
        let message = Message {
            id: MessageId::new(format!("01J8ZQ5R0N7W9Y8X6V4T2S0K{index}")).expect("message id"),
            session_id: SessionId::new(session_id.clone()).expect("session id"),
            run_id: None,
            client_msg_id: None,
            role: MessageRole::User,
            content: format!("尾部消息 {index}"),
            content_parts: None,
            tool_calls: None,
            parent_message_id: None,
            seq: 0,
            created_at: 1_700_000_000_000 + i64::from(index),
        };
        h.runtime
            .block_on(h.write.execute(StoreCommand::InsertMessage { message }))
            .expect("插入消息");
    }

    // 尾部分页：limit=2 → 最后 2 条、升序、首条 seq=2。
    let latest = latest_page(&h, &session_id, 2);
    assert_eq!(message_seqs(&latest), vec![2, 3], "尾部 limit 条（升序）");
    assert_eq!(latest["messages"][0]["seq"], 2, "首条为尾部窗口起点");
    assert_eq!(latest["messages"][0]["content"], "尾部消息 2");
    assert_eq!(latest["messages"][1]["content"], "尾部消息 3");
    assert_eq!(latest["complete"], true);

    // 补读页：`messages` 字段省略（`None` → 不序列化）。
    let backfill = h
        .backend
        .messages_page(&MessagesPageRequest {
            session_id: session_id.clone(),
            last_seq: Some(0),
            limit: Some(2),
        })
        .expect("补读页");
    assert!(
        backfill.get("messages").is_none(),
        "补读热路径不附带消息历史：{backfill}"
    );
}

#[test]
fn default_gap_limit_matches_core_constant() {
    let backend = SessionBackend::new(
        Arc::new(NotImplementedBackend),
        None,
        None,
        None,
        None,
        new_runtime().handle().clone(),
    );
    assert_eq!(
        backend.gap_limit(),
        aether_control::READBACK_MAX_GAP,
        "默认缺口上限必须与核心一致（10k，D4）"
    );
    assert_eq!(backend.gap_limit(), 10_000);
}

#[test]
fn session_create_persists_model_and_send_interrupt_dispose_are_wired() {
    let h = harness();
    let created = session_create(h.backend.as_ref(), "M3-02 工作台", Some("deepseek-v4-pro"));
    let session_id = created["id"].as_str().expect("会话 id").to_owned();
    assert_eq!(created["model"], "deepseek-v4-pro", "UI-05 会话级模型透传");
    assert_eq!(created["status"], "idle");

    // 落库断言：model 写入 sessions.model（读连接池）。
    let stored = h
        .runtime
        .block_on(
            h.reads
                .session(&SessionId::new(session_id.clone()).unwrap()),
        )
        .expect("读会话")
        .expect("会话存在");
    assert_eq!(stored.model.as_deref(), Some("deepseek-v4-pro"));

    // session_list 可见（按 runtime 过滤）。
    let list = h
        .backend
        .session_list(&SessionListRequest {
            runtime_id: Some("mock".to_owned()),
            status: None,
            limit: None,
        })
        .expect("session_list");
    let list = list.as_array().expect("列表数组");
    assert!(list.iter().any(|item| item["id"] == session_id));

    // send：ack 快路径（stub 执行器不产生终态，run 保持执行中）。
    let ack = h
        .backend
        .session_send(&SessionSendRequest {
            session_id: session_id.clone(),
            text: "hello workbench".to_owned(),
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1N".to_owned(),
            thinking_depth: None,
        })
        .expect("session_send");
    let run_id = ack["run_id"].as_str().expect("run_id").to_owned();
    assert_eq!(ack["queued"], false);
    assert_eq!(ack["duplicate"], false);
    assert!(wait_until(Duration::from_secs(5), || {
        !h.stub.requests().is_empty()
    }));
    assert_eq!(h.stub.requests()[0].text, "hello workbench");

    // 最近一页附带消息历史（工作台基线）：用户消息可读回；补读页不附带。
    let latest = h
        .backend
        .messages_page(&MessagesPageRequest {
            session_id: session_id.clone(),
            last_seq: None,
            limit: Some(50),
        })
        .expect("最近一页");
    let messages = latest["messages"].as_array().expect("messages 数组");
    assert!(messages
        .iter()
        .any(|message| message["role"] == "user" && message["content"] == "hello workbench"));
    let backfill = h
        .backend
        .messages_page(&MessagesPageRequest {
            session_id: session_id.clone(),
            last_seq: Some(0),
            limit: Some(50),
        })
        .expect("补读页");
    assert!(
        backfill.get("messages").is_none(),
        "补读热路径不附带消息历史：{backfill}"
    );

    // interrupt：在途 run 转 cancelled（会话回 idle 可续聊由 M2-05 覆盖）。
    let report = h
        .backend
        .session_interrupt(&SessionIdRequest {
            session_id: session_id.clone(),
        })
        .expect("session_interrupt");
    assert_eq!(report["interrupted_run"], run_id);

    // dispose：终态 completed/cancelled。
    let disposed = h
        .backend
        .session_dispose(&SessionIdRequest {
            session_id: session_id.clone(),
        })
        .expect("session_dispose");
    let status = disposed["status"].as_str().unwrap_or_default();
    assert!(matches!(status, "completed" | "cancelled"), "{disposed}");
}

#[test]
fn runtimes_list_reports_registry_and_requires_supervisor() {
    let h = harness();
    let list = h.backend.runtimes_list().expect("runtimes_list");
    let list = list.as_array().expect("数组");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], "mock");
    assert_eq!(list[0]["status"], "cold");
    assert!(list[0]["capabilities"].as_array().is_some());
    assert_eq!(list[0]["enabled"], true);

    // 未接线监督器 → core_not_ready（不伪造空列表）。
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let CoreBoot { reads, storage, .. } = boot_core_full(
        dir.path(),
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("启动核心");
    let backend = SessionBackend::new(
        Arc::new(NotImplementedBackend),
        None,
        None,
        Some(reads),
        None,
        runtime.handle().clone(),
    );
    let error = backend.runtimes_list().expect_err("未接线必须拒绝");
    assert_eq!(error.code, IpcErrorCode::CoreNotReady);
    let _ = storage;
}

#[test]
fn messages_page_requires_read_pool() {
    let backend = SessionBackend::new(
        Arc::new(NotImplementedBackend),
        None,
        None,
        None,
        None,
        new_runtime().handle().clone(),
    );
    let error = backend
        .messages_page(&MessagesPageRequest {
            session_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M".to_owned(),
            last_seq: None,
            limit: None,
        })
        .expect_err("无读连接池必须拒绝");
    assert_eq!(error.code, IpcErrorCode::CoreNotReady);
}
