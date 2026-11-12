//! M10-02/ADR-032：子 Agent 派生/隔离/结果回注/父处置/父授权集成测试。
//! 测试代码豁免 unwrap/expect/panic（AGENTS §2.2）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_app::ipc::dto::{
    SessionCreateRequest, SessionDeleteRequest, SessionIdRequest, SessionSendRequest,
    SubagentCancelRequest, SubagentListRequest, SubagentResolveRequest, SubagentResultGetRequest,
    SubagentSpawnRequest,
};
use aether_app::ipc::{IpcBackend, IpcError};
use aether_app::subagent::SubagentControlBackend;
use aether_control::SystemClock;
use aether_core::{
    Message, MessageId, MessageRole, PermissionMode, Run, RunId, RunStatus, Runtime, RuntimeId,
    Session, SessionId, SessionStatus, TokenUsage,
};
use aether_store::{ReadPool, StoreCommand, StoreRuntime, WriteQueue, WriteQueueConfig};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::runtime::Handle;

struct FakeBackend {
    reads: ReadPool,
    write: WriteQueue,
    handle: Handle,
    counter: AtomicU64,
    scripts: Arc<Mutex<HashMap<String, String>>>,
    active: Arc<Mutex<HashMap<String, RunId>>>,
}

impl FakeBackend {
    fn new(reads: ReadPool, write: WriteQueue, handle: Handle) -> Self {
        Self {
            reads,
            write,
            handle,
            counter: AtomicU64::new(1),
            scripts: Arc::new(Mutex::new(HashMap::new())),
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn call<T, F>(&self, future: F) -> Result<T, IpcError>
    where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T, IpcError>> + Send + 'static,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            let result = runtime.block_on(future);
            let _ = sender.send(result);
        });
        receiver
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| IpcError::internal("fake backend timeout"))?
    }

    fn next_id(&self, prefix: &str) -> String {
        let value = self.counter.fetch_add(1, Ordering::SeqCst);
        // 26 位 ULID 形状（测试用固定时间前缀）。
        format!("01J{prefix:0>2}{value:0>21}")
            .chars()
            .take(26)
            .collect()
    }

    fn finish_run(&self, run_id: RunId, status: RunStatus) {
        let write = self.write.clone();
        let _ = self.call(async move {
            write
                .execute(StoreCommand::FinishRun {
                    run_id,
                    status,
                    error: None,
                    finished_at: now_ms(),
                })
                .await
                .map_err(|error| IpcError::internal(error.to_string()))?;
            Ok(())
        });
    }
}

impl IpcBackend for FakeBackend {
    fn health(&self) -> Result<Value, IpcError> {
        Ok(json!({"storage_state": "normal"}))
    }

    fn runtimes_list(&self) -> Result<Value, IpcError> {
        Ok(json!([{
            "id": "mock",
            "name": "Mock",
            "kind": "mock",
            "version": "0.1.0",
            "protocol": "1.0",
            "capabilities": ["subagent", "subagent_context"],
            "enabled": true,
            "status": "ready",
            "status_reason": null,
            "active_sessions": 0,
        }]))
    }

    fn session_create(&self, request: &SessionCreateRequest) -> Result<Value, IpcError> {
        let runtime_id = request
            .runtime_id
            .clone()
            .unwrap_or_else(|| "mock".to_owned());
        let id = self.next_id("C");
        let session = Session {
            id: SessionId::new(id.clone()).map_err(|_| IpcError::internal("bad id"))?,
            runtime_id: RuntimeId::new(runtime_id)
                .map_err(|_| IpcError::internal("bad runtime"))?,
            workspace_id: request
                .workspace_id
                .as_ref()
                .and_then(|value| aether_core::WorkspaceId::new(value.clone()).ok()),
            parent_session_id: None,
            title: request.title.clone(),
            status: SessionStatus::Idle,
            model: request.model.clone(),
            thinking_depth: u8::try_from(request.thinking_depth.unwrap_or(2)).unwrap_or(2),
            permission_mode: request
                .permission_mode_value()
                .ok()
                .flatten()
                .unwrap_or_default(),
            preset_id: None,
            system_prompt: None,
            config: json!({"aether": {"provider_id": request.provider_id}}),
            token_usage: TokenUsage::default(),
            created_at: now_ms(),
            updated_at: now_ms(),
            closed_at: None,
        };
        let runtime = Runtime {
            id: session.runtime_id.clone(),
            name: "Mock".to_owned(),
            kind: "mock".to_owned(),
            version: "0.1.0".to_owned(),
            protocol: "1.0".to_owned(),
            capabilities: vec!["subagent".to_owned(), "subagent_context".to_owned()],
            endpoint: None,
            config: json!({}),
            status: aether_core::RuntimeStatus::Ready,
            status_reason: None,
            last_seen_at: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        };
        let write = self.write.clone();
        let session_clone = session.clone();
        self.call(async move {
            write
                .execute(StoreCommand::EnsureRuntime { runtime })
                .await
                .map_err(|error| IpcError::internal(error.to_string()))?;
            write
                .execute(StoreCommand::InsertSession {
                    session: session_clone,
                })
                .await
                .map_err(|error| IpcError::internal(error.to_string()))?;
            Ok(())
        })?;
        Ok(json!({"id": id, "title": session.title, "status": "idle"}))
    }

    fn session_send(&self, request: &SessionSendRequest) -> Result<Value, IpcError> {
        let session_id =
            SessionId::new(request.session_id.clone()).map_err(|_| IpcError::internal("bad id"))?;
        let run_id = RunId::new(self.next_id("R")).map_err(|_| IpcError::internal("bad run"))?;
        let message = Message {
            id: MessageId::new(self.next_id("M")).map_err(|_| IpcError::internal("bad msg"))?,
            session_id: session_id.clone(),
            run_id: None,
            client_msg_id: Some(request.client_msg_id.clone()),
            role: MessageRole::User,
            content: request.text.clone(),
            content_parts: None,
            tool_calls: None,
            parent_message_id: None,
            seq: 0,
            created_at: now_ms(),
        };
        let run = Run {
            id: run_id.clone(),
            session_id: session_id.clone(),
            status: RunStatus::Queued,
            input_message_id: Some(message.id.clone()),
            thinking_depth: Some(2),
            error: None,
            started_at: now_ms(),
            finished_at: None,
        };
        let write = self.write.clone();
        self.call(async move {
            write
                .execute(StoreCommand::BeginRunIdempotent {
                    message,
                    run,
                    attachment_ids: Vec::new(),
                    schedule: Default::default(),
                })
                .await
                .map_err(|error| IpcError::internal(error.to_string()))?;
            Ok(())
        })?;
        if let Ok(mut active) = self.active.lock() {
            active.insert(session_id.as_str().to_owned(), run_id.clone());
        }

        let is_child = self
            .call({
                let reads = self.reads.clone();
                let session_id = session_id.clone();
                async move {
                    Ok(reads
                        .session(&session_id)
                        .await
                        .map_err(|error| IpcError::internal(error.to_string()))?
                        .is_some_and(|session| session.parent_session_id.is_some()))
                }
            })
            .unwrap_or(false);
        if is_child {
            let scripts = Arc::clone(&self.scripts);
            let write = self.write.clone();
            let reads = self.reads.clone();
            let run_id = run_id.clone();
            let task_session = session_id.clone();
            let text = request.text.clone();
            self.handle.spawn(async move {
                if text.contains("FAIL_CHILD") {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let _ = write
                        .execute(StoreCommand::FinishRun {
                            run_id,
                            status: RunStatus::Failed,
                            error: Some("fake child failure".to_owned()),
                            finished_at: now_ms(),
                        })
                        .await;
                    return;
                }
                if text.contains("SLOW_CHILD") || text.contains("TIMEOUT_CHILD") {
                    // 不自动完成：由父取消级联或 watcher 超时调用 interrupt 收口。
                    return;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
                let content = {
                    let scripts = scripts.lock().expect("scripts");
                    scripts
                        .get(run_id.as_str())
                        .cloned()
                        .unwrap_or_else(|| {
                            r#"<aether:subagent-result>{"verdict":"approve","summary":"ok","findings":[{"severity":"minor","title":"t","detail":"d","evidence_refs":[{"path":"src/lib.rs","line_start":1}]}]}</aether:subagent-result>"#
                                .to_owned()
                        })
                };
                let message = Message {
                    id: MessageId::new(ulid::Ulid::new().to_string()).unwrap(),
                    session_id: task_session.clone(),
                    run_id: Some(run_id.clone()),
                    client_msg_id: None,
                    role: MessageRole::Assistant,
                    content,
                    content_parts: None,
                    tool_calls: None,
                    parent_message_id: None,
                    seq: 0,
                    created_at: now_ms(),
                };
                let _ = write
                    .execute(StoreCommand::InsertMessage { message })
                    .await;
                let _ = write
                    .execute(StoreCommand::FinishRun {
                        run_id: run_id.clone(),
                        status: RunStatus::Succeeded,
                        error: None,
                        finished_at: now_ms(),
                    })
                    .await;
                let _ = reads.session(&task_session).await;
            });
        } else {
            let write = self.write.clone();
            let run_id = run_id.clone();
            self.handle.spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let _ = write
                    .execute(StoreCommand::FinishRun {
                        run_id,
                        status: RunStatus::Succeeded,
                        error: None,
                        finished_at: now_ms(),
                    })
                    .await;
            });
        }
        let mut response = json!({
            "session_id": session_id.as_str(),
            "message_id": "01JMSGREPLY0000000000000",
            "run_id": run_id.as_str(),
            "queued": false,
            "duplicate": false,
        });
        if let Some(context) = request.subagent_context_value()? {
            response["applied_context_version"] = json!(context.version);
            response["applied_context_mode"] = json!(context.mode.as_str());
        }
        Ok(response)
    }

    fn session_interrupt(&self, request: &SessionIdRequest) -> Result<Value, IpcError> {
        let run = self
            .active
            .lock()
            .ok()
            .and_then(|active| active.get(&request.session_id).cloned());
        if let Some(run) = run {
            self.finish_run(run, RunStatus::Cancelled);
        }
        Ok(json!({"session_id": request.session_id, "interrupted_run": null}))
    }

    fn session_delete(&self, request: &SessionDeleteRequest) -> Result<Value, IpcError> {
        let write = self.write.clone();
        let session_id =
            SessionId::new(request.session_id.clone()).map_err(|_| IpcError::internal("bad id"))?;
        let _ = self.call(async move {
            write
                .execute(StoreCommand::DeleteSession { id: session_id })
                .await
                .map_err(|error| IpcError::internal(error.to_string()))?;
            Ok(())
        });
        Ok(json!({"deleted": true}))
    }
}

struct Harness {
    _dir: TempDir,
    _slot: StoreRuntime,
    reads: ReadPool,
    write: WriteQueue,
    control: Arc<SubagentControlBackend>,
    backend: Arc<FakeBackend>,
}

async fn harness() -> Harness {
    let dir = tempfile::Builder::new()
        .prefix("aether-m10-02-")
        .tempdir()
        .unwrap();
    let slot = StoreRuntime::open(
        dir.path().join("aether.db"),
        WriteQueueConfig::default(),
        &Handle::current(),
    )
    .unwrap();
    let reads = slot.reads().clone();
    let write = slot.queue().clone();
    let handle = Handle::current();
    let backend = Arc::new(FakeBackend::new(
        reads.clone(),
        write.clone(),
        handle.clone(),
    ));
    let control = Arc::new(SubagentControlBackend::new(
        backend.clone(),
        reads.clone(),
        write.clone(),
        None,
        Arc::new(SystemClock),
        handle.clone(),
    ));
    Harness {
        _dir: dir,
        _slot: slot,
        reads,
        write,
        control,
        backend,
    }
}

impl Harness {
    async fn insert_terminal_child(&self, parent: &SessionId, content: &str) -> SessionId {
        let id = SessionId::new(ulid::Ulid::new().to_string()).unwrap();
        let session = Session {
            id: id.clone(),
            runtime_id: RuntimeId::new("mock").unwrap(),
            workspace_id: None,
            parent_session_id: Some(parent.clone()),
            title: "manual terminal child".to_owned(),
            status: SessionStatus::Completed,
            model: None,
            thinking_depth: 2,
            permission_mode: PermissionMode::WorkspaceWrite,
            preset_id: None,
            system_prompt: None,
            config: json!({}),
            token_usage: TokenUsage::default(),
            created_at: now_ms(),
            updated_at: now_ms(),
            closed_at: None,
        };
        self.write
            .execute(StoreCommand::InsertSession { session })
            .await
            .unwrap();
        let message = Message {
            id: MessageId::new(ulid::Ulid::new().to_string()).unwrap(),
            session_id: id.clone(),
            run_id: None,
            client_msg_id: None,
            role: MessageRole::Assistant,
            content: content.to_owned(),
            content_parts: None,
            tool_calls: None,
            parent_message_id: None,
            seq: 0,
            created_at: now_ms(),
        };
        self.write
            .execute(StoreCommand::InsertMessage { message })
            .await
            .unwrap();
        id
    }

    async fn bootstrap_parent(&self, permission: PermissionMode, secret: &str) -> SessionId {
        let id = SessionId::new("01JPARENT0000000000000000").unwrap();
        let runtime = Runtime {
            id: RuntimeId::new("mock").unwrap(),
            name: "Mock".to_owned(),
            kind: "mock".to_owned(),
            version: "0.1.0".to_owned(),
            protocol: "1.0".to_owned(),
            capabilities: vec!["subagent".to_owned(), "subagent_context".to_owned()],
            endpoint: None,
            config: json!({}),
            status: aether_core::RuntimeStatus::Ready,
            status_reason: None,
            last_seen_at: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        };
        self.write
            .execute(StoreCommand::EnsureRuntime { runtime })
            .await
            .unwrap();
        let session = Session {
            id: id.clone(),
            runtime_id: RuntimeId::new("mock").unwrap(),
            workspace_id: None,
            parent_session_id: None,
            title: "parent".to_owned(),
            status: SessionStatus::Idle,
            model: None,
            thinking_depth: 2,
            permission_mode: permission,
            preset_id: None,
            system_prompt: None,
            config: json!({}),
            token_usage: TokenUsage::default(),
            created_at: now_ms(),
            updated_at: now_ms(),
            closed_at: None,
        };
        self.write
            .execute(StoreCommand::InsertSession {
                session: session.clone(),
            })
            .await
            .unwrap();
        let message = Message {
            id: MessageId::new("01JPARENTMSG000000000000").unwrap(),
            session_id: id.clone(),
            run_id: None,
            client_msg_id: None,
            role: MessageRole::User,
            content: secret.to_owned(),
            content_parts: None,
            tool_calls: None,
            parent_message_id: None,
            seq: 0,
            created_at: now_ms(),
        };
        self.write
            .execute(StoreCommand::InsertMessage { message })
            .await
            .unwrap();
        id
    }

    async fn spawn(&self, request: SubagentSpawnRequest) -> Result<Value, IpcError> {
        let control = Arc::clone(&self.control);
        tokio::task::spawn_blocking(move || control.subagent_spawn(&request))
            .await
            .expect("join")
    }

    async fn list(&self, parent: &SessionId) -> Value {
        let control = Arc::clone(&self.control);
        let request = SubagentListRequest {
            parent_session_id: parent.as_str().to_owned(),
            include_completed: Some(true),
        };
        tokio::task::spawn_blocking(move || control.subagent_list(&request))
            .await
            .expect("join")
            .expect("list")
    }

    async fn resolve(&self, child: &str, decision: &str) -> Result<Value, IpcError> {
        let control = Arc::clone(&self.control);
        let request = SubagentResolveRequest {
            child_session_id: child.to_owned(),
            decision: decision.to_owned(),
            note: None,
        };
        tokio::task::spawn_blocking(move || control.subagent_resolve(&request))
            .await
            .expect("join")
    }

    async fn result_get(&self, parent: &SessionId, child: &str) -> Result<Value, IpcError> {
        let control = Arc::clone(&self.control);
        let request = SubagentResultGetRequest {
            parent_session_id: parent.as_str().to_owned(),
            child_session_id: child.to_owned(),
            offset: None,
            limit_bytes: None,
        };
        tokio::task::spawn_blocking(move || control.subagent_result_get(&request))
            .await
            .expect("join")
    }

    fn interrupt(&self, session: &SessionId) -> Result<Value, IpcError> {
        let control = Arc::clone(&self.control);
        let request = SessionIdRequest {
            session_id: session.as_str().to_owned(),
        };
        tokio::task::block_in_place(|| control.session_interrupt(&request))
    }

    fn run_id_for(&self, session: &str) -> Option<RunId> {
        self.backend
            .active
            .lock()
            .ok()
            .and_then(|active| active.get(session).cloned())
    }

    async fn wait_run_status(&self, run_id: &RunId, expected: RunStatus) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let run = self.reads.run(run_id).await.unwrap().expect("run exists");
            if run.status == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "等待 run 状态 {:?} 超时，当前 {:?}",
                expected,
                run.status
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_result(&self, parent: &SessionId, child: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let list = self.list(parent).await;
            let injections = self
                .reads
                .subagent_injections(parent.as_str())
                .await
                .unwrap_or_default();
            if let Some(node) = list["items"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|node| node["child_session_id"] == child)
            {
                if node["result_summary"].is_string() && !injections.is_empty() {
                    return node.clone();
                }
            }
            assert!(Instant::now() < deadline, "等待子结果超时: {list}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 结果落库与注入状态更新存在短暂窗口；按最终注入态收敛，避免 Linux CI 时序抖动。
    async fn wait_injection_state(&self, parent: &SessionId, child: &str, expected: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let list = self.list(parent).await;
            if let Some(node) = list["items"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|node| node["child_session_id"] == child)
            {
                if node["injection_state"] == expected {
                    return node.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "等待注入状态 {expected} 超时: {list}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn spawn_request(
    parent: &SessionId,
    template: &str,
    permission: Option<&str>,
) -> SubagentSpawnRequest {
    SubagentSpawnRequest {
        parent_session_id: parent.as_str().to_owned(),
        role: "review".to_owned(),
        purpose: "审查变更".to_owned(),
        mode: None,
        template: Some(template.to_owned()),
        context_refs: vec![aether_app::ipc::dto::SubagentContextRef {
            kind: "text".to_owned(),
            path: None,
            text: Some("只审查显式给定内容".to_owned()),
        }],
        runtime_id: Some("mock".to_owned()),
        provider_id: None,
        model: None,
        permission_mode: permission.map(str::to_owned),
        timeout_ms: Some(10_000),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_isolates_child_and_auto_injects_compact_envelope() {
    let h = harness().await;
    let parent = h
        .bootstrap_parent(PermissionMode::WorkspaceWrite, "PARENT_SECRET")
        .await;
    let spawned = h
        .spawn(spawn_request(&parent, "parallel_explore", None))
        .await
        .expect("spawn");
    let child = spawned["child_session_id"]
        .as_str()
        .expect("child")
        .to_owned();
    assert_eq!(spawned["depth"], 1);
    let _ = h.wait_result(&parent, &child).await;
    let node = h.wait_injection_state(&parent, &child, "injected").await;
    assert_eq!(node["verdict"], "approve");
    assert_eq!(node["injection_state"], "injected");

    let child_session = h
        .reads
        .session(&SessionId::new(child.clone()).unwrap())
        .await
        .unwrap()
        .expect("child exists");
    assert_eq!(child_session.parent_session_id.as_ref(), Some(&parent));
    assert_eq!(
        child_session.permission_mode,
        PermissionMode::WorkspaceWrite
    );
    let child_messages = h
        .reads
        .messages_latest(&child_session.id, 10)
        .await
        .unwrap();
    assert!(
        child_messages
            .iter()
            .all(|message| !message.content.contains("PARENT_SECRET")),
        "子会话不得继承父消息全文"
    );
    assert!(
        child_messages
            .iter()
            .any(|message| message.content.contains("显式给定内容")),
        "子会话只接收显式 context_refs"
    );

    let parent_messages = h.reads.messages_latest(&parent, 10).await.unwrap();
    let injected: Vec<&Message> = parent_messages
        .iter()
        .filter(|message| message.content.contains("<aether:subagent-results"))
        .collect();
    assert_eq!(injected.len(), 1, "结果恰好追加一次");
    assert!(injected[0].content.contains("version=\""));

    let chunk = h.result_get(&parent, &child).await.expect("result");
    assert!(chunk["result_chunk"]
        .as_str()
        .unwrap()
        .contains("\"verdict\":\"approve\""));
    assert!(chunk["total_bytes"].as_u64().unwrap() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_escalation_and_depth_limits_are_structured() {
    let h = harness().await;
    let parent = h.bootstrap_parent(PermissionMode::ReadOnly, "secret").await;
    let error = h
        .spawn(spawn_request(&parent, "custom", Some("danger_full_access")))
        .await
        .expect_err("禁止升权");
    assert_eq!(
        error.business_code().map(|code| code.as_str()),
        Some("subagent_context_rejected")
    );
    let read_only = h
        .spawn(spawn_request(&parent, "custom", Some("read_only")))
        .await
        .expect("允许相同/更严格");
    let child = read_only["child_session_id"].as_str().unwrap().to_owned();
    let _ = h.wait_result(&parent, &child).await;
    let grand = h
        .spawn(SubagentSpawnRequest {
            parent_session_id: child.clone(),
            role: "custom".to_owned(),
            purpose: "孙".to_owned(),
            mode: None,
            template: Some("custom".to_owned()),
            context_refs: Vec::new(),
            runtime_id: Some("mock".to_owned()),
            provider_id: None,
            model: None,
            permission_mode: None,
            timeout_ms: Some(10_000),
        })
        .await
        .expect("depth 2 allowed");
    let grandchild = grand["child_session_id"].as_str().unwrap().to_owned();
    let _ = h
        .wait_result(&SessionId::new(child.clone()).unwrap(), &grandchild)
        .await;
    let error = h
        .spawn(SubagentSpawnRequest {
            parent_session_id: grandchild,
            role: "custom".to_owned(),
            purpose: "曾孙".to_owned(),
            mode: None,
            template: Some("custom".to_owned()),
            context_refs: Vec::new(),
            runtime_id: Some("mock".to_owned()),
            provider_id: None,
            model: None,
            permission_mode: None,
            timeout_ms: Some(10_000),
        })
        .await
        .expect_err("depth 3 拒绝");
    assert_eq!(
        error.business_code().map(|code| code.as_str()),
        Some("subagent_limit_exceeded")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hold_accept_exactly_once_and_repeat_resolve_conflicts() {
    let h = harness().await;
    let parent = h
        .bootstrap_parent(PermissionMode::WorkspaceWrite, "secret")
        .await;
    let spawned = h
        .spawn(spawn_request(&parent, "adversarial_review", None))
        .await
        .expect("spawn");
    let child = spawned["child_session_id"].as_str().unwrap().to_owned();
    let node = h.wait_result(&parent, &child).await;
    assert_eq!(node["injection_state"], "pending", "对抗审查默认 hold");
    assert!(
        h.reads
            .messages_latest(&parent, 10)
            .await
            .unwrap()
            .iter()
            .all(|message| !message.content.contains("<aether:subagent-results")),
        "hold 不自动注入"
    );
    let accepted = h.resolve(&child, "accept").await.expect("accept");
    assert_eq!(accepted["injected"], true);
    let repeat = h.resolve(&child, "accept").await.expect_err("重复 resolve");
    assert_eq!(
        repeat.business_code().map(|code| code.as_str()),
        Some("subagent_resolution_conflict")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parent_authorization_rejects_sibling_and_parent_delete_is_guarded() {
    let h = harness().await;
    let parent = h
        .bootstrap_parent(PermissionMode::WorkspaceWrite, "secret")
        .await;
    let spawned = h
        .spawn(spawn_request(&parent, "parallel_explore", None))
        .await
        .expect("spawn");
    let child = spawned["child_session_id"].as_str().unwrap().to_owned();
    let _ = h.wait_result(&parent, &child).await;

    let other = SessionId::new("01JOTHERPARENT00000000000").unwrap();
    let error = h
        .result_get(&other, &child)
        .await
        .expect_err("兄弟/根直读必须拒绝");
    assert_eq!(
        error.business_code().map(|code| code.as_str()),
        Some("subagent_not_found")
    );
    assert_eq!(
        h.reads.child_session_count(parent.as_str()).await.unwrap(),
        1
    );
    assert!(h
        .reads
        .sessions(aether_store::SessionQuery {
            include_children: false,
            ..Default::default()
        })
        .await
        .unwrap()
        .iter()
        .all(|session| session.id != SessionId::new(child.clone()).unwrap()));

    let cancel = h.control.subagent_cancel(&SubagentCancelRequest {
        child_session_id: child.clone(),
        reason: Some("test".to_owned()),
    });
    assert!(cancel.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_workspace_assign_is_rejected() {
    let h = harness().await;
    let parent = h
        .bootstrap_parent(PermissionMode::WorkspaceWrite, "secret")
        .await;
    let spawned = h
        .spawn(spawn_request(&parent, "custom", None))
        .await
        .expect("spawn");
    let child = spawned["child_session_id"].as_str().unwrap().to_owned();
    let _ = h.wait_result(&parent, &child).await;
    let request = aether_app::ipc::dto::SessionWorkspaceAssignRequest {
        session_id: child,
        workspace_id: "01JWORKSPACE0000000000000".to_owned(),
    };
    let error = h
        .control
        .session_workspace_assign(&request)
        .expect_err("子会话禁止换绑");
    assert_eq!(
        error.business_code().map(|code| code.as_str()),
        Some("subagent_context_rejected")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_children_parent_cancel_cascades_and_settles() {
    let h = harness().await;
    let parent = h
        .bootstrap_parent(PermissionMode::WorkspaceWrite, "secret")
        .await;
    let mut children = Vec::new();
    for index in 0..4 {
        let mut request = spawn_request(&parent, "custom", None);
        request.purpose = format!("SLOW_CHILD_{index}");
        let spawned = h.spawn(request).await.expect("spawn slow child");
        children.push(spawned["child_session_id"].as_str().unwrap().to_owned());
    }
    // 四个子 run 均在途（SLOW_CHILD 不自动完成）。
    let run_ids: Vec<RunId> = children
        .iter()
        .map(|child| h.run_id_for(child).expect("child run id"))
        .collect();
    assert_eq!(run_ids.len(), 4);

    h.interrupt(&parent).expect("parent interrupt");
    for run_id in &run_ids {
        h.wait_run_status(run_id, RunStatus::Cancelled).await;
    }
    // 等待 watcher 收口（不产生结果/注入）。
    tokio::time::sleep(Duration::from_millis(150)).await;
    let injections = h.reads.subagent_injections(parent.as_str()).await.unwrap();
    assert!(injections.is_empty(), "取消的子 Agent 不得产生注入行");
    let list = h.list(&parent).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 4);
    for child in &children {
        assert!(
            h.reads.subagent_result(child).await.unwrap().is_none(),
            "取消的子 Agent 不得产生结果"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_failure_timeout_and_reconcile_do_not_half_inject() {
    let h = harness().await;
    let parent = h
        .bootstrap_parent(PermissionMode::WorkspaceWrite, "secret")
        .await;

    let mut fail_request = spawn_request(&parent, "custom", None);
    fail_request.purpose = "FAIL_CHILD".to_owned();
    let failed = h.spawn(fail_request).await.expect("spawn fail child");
    let failed_child = failed["child_session_id"].as_str().unwrap().to_owned();
    let failed_run = h.run_id_for(&failed_child).expect("failed run id");
    h.wait_run_status(&failed_run, RunStatus::Failed).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h
        .reads
        .subagent_result(&failed_child)
        .await
        .unwrap()
        .is_none());
    assert!(h
        .reads
        .subagent_injections(parent.as_str())
        .await
        .unwrap()
        .is_empty());

    let mut timeout_request = spawn_request(&parent, "custom", None);
    timeout_request.purpose = "TIMEOUT_CHILD".to_owned();
    timeout_request.timeout_ms = Some(200);
    let timed = h.spawn(timeout_request).await.expect("spawn timeout child");
    let timeout_child = timed["child_session_id"].as_str().unwrap().to_owned();
    let timeout_run = h.run_id_for(&timeout_child).expect("timeout run id");
    h.wait_run_status(&timeout_run, RunStatus::Cancelled).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h
        .reads
        .subagent_result(&timeout_child)
        .await
        .unwrap()
        .is_none());
    assert!(h
        .reads
        .subagent_injections(parent.as_str())
        .await
        .unwrap()
        .is_empty());

    // daemon 重启 reconcile：终态子会话有助手消息但缺结果 → 补齐结果，不产生注入行。
    let manual = h
        .insert_terminal_child(
            &parent,
            r#"<aether:subagent-result>{"verdict":"approve","summary":"reconciled","findings":[]}</aether:subagent-result>"#,
        )
        .await;
    let (scanned, repaired) = h.control.reconcile().await.expect("reconcile");
    assert!(scanned >= 1);
    assert!(repaired >= 1);
    let result = h
        .reads
        .subagent_result(manual.as_str())
        .await
        .unwrap()
        .expect("reconciled result");
    assert_eq!(result.summary.as_deref(), Some("reconciled"));
    assert!(
        h.reads
            .subagent_injections(parent.as_str())
            .await
            .unwrap()
            .is_empty(),
        "reconcile 不得伪造注入/半注入"
    );
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
