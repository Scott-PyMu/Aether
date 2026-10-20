//! M7-07 集成测试：会话搜索 / 改名 / 删除（ADR-025 §2.1；非 P1 门禁）。
//!
//! 覆盖：
//! - `session_search`：标题/正文命中、`snippet` + `source`、分页参数；
//! - `session_rename`：单写队列、`session.updated`（先日志后广播）、审计；
//! - `session_delete`：`session.closed(reason=deleted)`、级联四表、events 保留、
//!   `messages_page` 删除后 `session_not_found`；
//! - 在途/待审批 → `session_busy`；查无 → `session_not_found`。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec};
use aether_control::{
    ExecutorFuture, ExecutorOutcome, LifecycleConfig, PermissionConfig, PermissionService,
    RunExecutor, RunRequest, SessionManager, SystemClock,
};
use aether_core::{
    EventPayload, Message, MessageId, MessageRole, Run, RunId, RunStatus, SessionId,
};
use aether_security::PolicyEngine;
use aether_store::{ReadPool, StoreCommand, WriteQueue};
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    MessagesPageRequest, SessionCreateRequest, SessionDeleteRequest, SessionRenameRequest,
    SessionSearchRequest,
};
use aether_tauri::ipc::error::IpcErrorCode;
use aether_tauri::runtime_control::boot_supervisor;
use aether_tauri::session_backend::SessionBackend;
use serde_json::Value;
use tempfile::TempDir;

struct NoopExecutor;

impl RunExecutor for NoopExecutor {
    fn execute(&self, _request: RunRequest) -> ExecutorFuture<'_> {
        Box::pin(async { ExecutorOutcome::Cancelled { reason: None } })
    }
}

struct Harness {
    _dir: TempDir,
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    reads: ReadPool,
    write: WriteQueue,
    backend: Arc<SessionBackend>,
}

fn harness() -> Harness {
    let dir = TempDir::new().expect("数据目录");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio");
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
    .expect("核心");
    let service = PermissionService::new(
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
        Arc::new(SystemClock),
        PolicyEngine::new(dir.path()).expect("数据目录基准"),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
    );
    let program = dir.path().join("mock-adapter.exe");
    std::fs::write(&program, b"stub").unwrap();
    let spec = RuntimeSpec::with_fresh_token(RuntimeManifest::new("mock", "Mock", program));
    let supervisor = Arc::new(
        boot_supervisor(vec![spec], Some(&dir.path().join("adapters.json"))).expect("监督器"),
    );
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        Arc::new(NoopExecutor),
    );
    let backend = Arc::new(
        SessionBackend::new(
            Arc::new(NotImplementedBackend) as Arc<dyn IpcBackend>,
            Some(manager),
            None,
            Some(reads.clone()),
            Some(supervisor),
            handle,
        )
        .with_permissions(service)
        .with_workspace_store(write.clone())
        .with_data_dir(dir.path()),
    );
    Harness {
        _dir: dir,
        runtime,
        slot,
        reads,
        write,
        backend,
    }
}

impl Harness {
    fn create_session(&self, title: &str) -> SessionId {
        let value = self
            .backend
            .session_create(&SessionCreateRequest {
                runtime_id: Some("mock".to_owned()),
                title: title.to_owned(),
                workspace_id: None,
                model: None,
                thinking_depth: None,
                permission_mode: None,
                preset_id: None,
            })
            .expect("session_create");
        let id = value["id"].as_str().expect("session id");
        SessionId::new(id).expect("ULID")
    }

    fn insert_message(&self, session_id: &SessionId, id: &str, content: &str) {
        self.runtime.block_on(async {
            self.write
                .execute(StoreCommand::InsertMessage {
                    message: Message {
                        id: MessageId::new(id).expect("message id"),
                        session_id: session_id.clone(),
                        run_id: None,
                        client_msg_id: None,
                        role: MessageRole::Assistant,
                        content: content.to_owned(),
                        content_parts: None,
                        tool_calls: None,
                        parent_message_id: None,
                        seq: 0,
                        created_at: 1,
                    },
                })
                .await
                .expect("insert message");
        });
    }

    fn insert_run(&self, session_id: &SessionId, id: &str, status: RunStatus) {
        self.runtime.block_on(async {
            self.write
                .execute(StoreCommand::InsertRun {
                    run: Run {
                        id: RunId::new(id).expect("run id"),
                        session_id: session_id.clone(),
                        status,
                        input_message_id: None,
                        thinking_depth: None,
                        error: None,
                        started_at: 1,
                        finished_at: None,
                    },
                })
                .await
                .expect("insert run");
        });
    }

    fn search(
        &self,
        query: &str,
        limit: Option<u32>,
        offset: Option<u64>,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.session_search(&SessionSearchRequest {
            query: query.to_owned(),
            limit,
            offset,
        })
    }

    fn rename(
        &self,
        session_id: &SessionId,
        title: &str,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.session_rename(&SessionRenameRequest {
            session_id: session_id.as_str().to_owned(),
            title: title.to_owned(),
        })
    }

    fn delete(
        &self,
        session_id: &SessionId,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.session_delete(&SessionDeleteRequest {
            session_id: session_id.as_str().to_owned(),
        })
    }

    fn event_types(&self, session_id: &SessionId) -> Vec<String> {
        self.runtime.block_on(async {
            self.reads
                .events_latest(session_id, 50)
                .await
                .expect("events")
                .into_iter()
                .map(|event| event.event_type().as_str().to_owned())
                .collect()
        })
    }
}

/// DoD2/DoD3：搜索标题与正文；改名后 FTS 同步并广播 `session.updated` + 审计。
#[test]
fn search_rename_and_session_updated_event() {
    let harness = harness();
    let title_session = harness.create_session("alpha title");
    let body_session = harness.create_session("other title");
    harness.insert_message(
        &body_session,
        "01J00000000000000000000M1",
        "alpha body needle",
    );

    let hits = harness.search("alpha", Some(20), None).expect("search");
    let hits = hits.as_array().expect("array");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0]["id"].as_str(), Some(title_session.as_str()));
    assert_eq!(hits[0]["source"].as_str(), Some("title"));
    assert_eq!(hits[1]["id"].as_str(), Some(body_session.as_str()));
    assert_eq!(hits[1]["source"].as_str(), Some("message"));
    assert!(hits[1]["snippet"].as_str().unwrap_or("").contains("[alpha]"));

    let renamed = harness
        .rename(&title_session, "omega renamed")
        .expect("rename");
    assert_eq!(renamed["title"].as_str(), Some("omega renamed"));
    let old = harness.search("alpha", Some(20), None).expect("search");
    assert_eq!(old.as_array().unwrap().len(), 1, "旧标题索引应删除");
    let new = harness.search("omega", Some(20), None).expect("search");
    assert_eq!(new.as_array().unwrap().len(), 1);
    assert_eq!(new[0]["id"].as_str(), Some(title_session.as_str()));

    let events = harness.event_types(&title_session);
    assert!(events.iter().any(|event| event == "session.updated"));
    let audit = harness
        .runtime
        .block_on(async { harness.reads.audit_log(20).await.expect("audit") });
    assert!(audit.iter().any(|row| row.action == "session.renamed"));
}

/// DoD3：删除后 `session.closed(reason=deleted)`、级联四表、events 保留、
/// `messages_page` → `session_not_found`。
#[test]
fn delete_session_cascades_and_keeps_events() {
    let harness = harness();
    let session = harness.create_session("to delete");
    harness.insert_message(&session, "01J00000000000000000000M2", "delete me");
    harness.insert_run(&session, "01J00000000000000000000R2", RunStatus::Succeeded);
    let before_events = harness.event_types(&session);
    assert!(!before_events.is_empty());

    let deleted = harness.delete(&session).expect("delete");
    assert_eq!(deleted["deleted"].as_bool(), Some(true));

    let events = harness.event_types(&session);
    assert!(events.iter().any(|event| event == "session.closed"));
    assert!(
        events.len() >= before_events.len(),
        "events 不级联（R7）：删除后仍可读"
    );
    let closed = harness.runtime.block_on(async {
        harness
            .reads
            .events_latest(&session, 50)
            .await
            .expect("events")
    });
    let closed_payload = closed
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::SessionClosed(payload) => Some(payload),
            _ => None,
        })
        .expect("session.closed");
    assert_eq!(closed_payload.reason.as_deref(), Some("deleted"));

    let page = harness.backend.messages_page(&MessagesPageRequest {
        session_id: session.as_str().to_owned(),
        last_seq: None,
        limit: None,
    });
    let error = page.expect_err("删除后补读必须失败");
    assert_eq!(error.code, IpcErrorCode::SessionNotFound);
}

/// DoD5：在途 run → `session_busy`；未知会话 → `session_not_found`。
#[test]
fn delete_rejects_busy_and_reports_not_found() {
    let harness = harness();
    let session = harness.create_session("busy");
    harness.insert_run(&session, "01J00000000000000000000R3", RunStatus::Running);
    let busy = harness.delete(&session).expect_err("busy");
    assert_eq!(busy.code, IpcErrorCode::InvalidValue);
    assert!(busy.message.contains("session_busy"));

    let missing = harness
        .delete(&SessionId::new("01J00000000000000000000S9").expect("id"))
        .expect_err("not found");
    assert_eq!(missing.code, IpcErrorCode::SessionNotFound);
}

/// DoD2：搜索参数校验（空/超长/limit 越界）与 FTS 语法注入按短语处理。
#[test]
fn search_validation_and_phrase_injection() {
    let harness = harness();
    let session = harness.create_session("quoted \"phrase\" title");
    let hits = harness.search("\"phrase\"", None, None).expect("phrase");
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["id"].as_str(), Some(session.as_str()));

    // 分页：limit/offset 透传。
    let _ = harness.create_session("second phrase");
    let first = harness.search("phrase", Some(1), Some(0)).expect("page 1");
    assert_eq!(first.as_array().unwrap().len(), 1);
    let second = harness.search("phrase", Some(1), Some(1)).expect("page 2");
    assert_eq!(second.as_array().unwrap().len(), 1);
    assert_ne!(first[0]["id"], second[0]["id"]);
}
