//! M2-01 存储层集成：领域写命令（单写队列）+ 读侧 API（会话/消息/run/权限/审计）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;

use aether_core::{
    EventEnvelope, EventId, EventPayload, LogLevel, LogPayload, Message, MessageId, MessageRole,
    PermissionDecision, PermissionScope, PermissionStatus, Run, RunId, RunStatus, Runtime,
    RuntimeId, RuntimeStatus, Session, SessionId, SessionStatus, TokenUsage,
    EVENT_ENVELOPE_VERSION,
};
use aether_store::{
    AuditLogRecord, BatchTrigger, PermissionRecord, ReadPool, SessionQuery, StoreCommand,
    StoreOutcome, StoreRuntime, WriteQueue, WriteQueueConfig,
};
use tokio::runtime::Handle;

const SESSION_A: &str = "01J0000000000000000000000A";
const SESSION_B: &str = "01J0000000000000000000000B";
const MESSAGE_1: &str = "01J000000000000000000000M1";
const RUN_1: &str = "01J000000000000000000000R1";

fn open(path: &std::path::Path) -> StoreRuntime {
    StoreRuntime::open(path, WriteQueueConfig::default(), &Handle::current()).unwrap()
}

fn runtime() -> Runtime {
    Runtime {
        id: RuntimeId::new("mock").unwrap(),
        name: "Mock".to_owned(),
        kind: "mock".to_owned(),
        version: "0.1.0".to_owned(),
        protocol: "1.0".to_owned(),
        capabilities: vec!["stream".to_owned()],
        endpoint: None,
        config: serde_json::json!({}),
        status: RuntimeStatus::Ready,
        status_reason: None,
        last_seen_at: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn session(id: &str, title: &str) -> Session {
    Session {
        id: SessionId::new(id).unwrap(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: None,
        parent_session_id: None,
        title: title.to_owned(),
        status: SessionStatus::Idle,
        model: None,
        system_prompt: None,
        config: serde_json::json!({"native_id": "n-1"}),
        token_usage: TokenUsage::default(),
        created_at: 10,
        updated_at: 10,
        closed_at: None,
    }
}

fn user_message(id: &str, session_id: &str, client: &str) -> Message {
    Message {
        id: MessageId::new(id).unwrap(),
        session_id: SessionId::new(session_id).unwrap(),
        run_id: None,
        client_msg_id: Some(client.to_owned()),
        role: MessageRole::User,
        content: "hi".to_owned(),
        content_parts: None,
        tool_calls: None,
        parent_message_id: None,
        seq: 0,
        created_at: 11,
    }
}

fn run(id: &str, session_id: &str, message_id: &str) -> Run {
    Run {
        id: RunId::new(id).unwrap(),
        session_id: SessionId::new(session_id).unwrap(),
        status: RunStatus::Queued,
        input_message_id: Some(MessageId::new(message_id).unwrap()),
        error: None,
        started_at: 11,
        finished_at: None,
    }
}

async fn seed(queue: &WriteQueue, reads: &ReadPool) {
    queue
        .execute(StoreCommand::EnsureRuntime { runtime: runtime() })
        .await
        .unwrap();
    queue
        .execute(StoreCommand::InsertSession {
            session: session(SESSION_A, "A"),
        })
        .await
        .unwrap();
    queue
        .execute(StoreCommand::InsertSession {
            session: session(SESSION_B, "B"),
        })
        .await
        .unwrap();
    let _ = reads;
}

/// 领域写命令 + 读侧全路径往返。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn domain_commands_and_reads_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let storage = open(&temp.path().join("aether.db"));
    let queue = storage.queue().clone();
    let reads = storage.reads().clone();
    seed(&queue, &reads).await;

    // EnsureRuntime 冲突路径（已存在 → 更新摘要字段）。
    let mut updated_runtime = runtime();
    updated_runtime.name = "Mock v2".to_owned();
    updated_runtime.updated_at = 2;
    queue
        .execute(StoreCommand::EnsureRuntime {
            runtime: updated_runtime,
        })
        .await
        .unwrap();

    // 会话读：单条 + 过滤列表 + 状态计数。
    let read = reads
        .session(&SessionId::new(SESSION_A).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.title, "A");
    assert_eq!(read.config["native_id"], "n-1");
    let sessions = reads
        .sessions(SessionQuery {
            runtime_id: Some("mock".to_owned()),
            status: Some(SessionStatus::Idle),
            limit: Some(10),
            ..SessionQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(sessions.len(), 2);
    let sessions = reads
        .sessions(SessionQuery {
            status: Some(SessionStatus::Running),
            ..SessionQuery::default()
        })
        .await
        .unwrap();
    assert!(sessions.is_empty());
    let counts = reads.session_status_counts().await.unwrap();
    assert_eq!(counts.get("idle"), Some(&2));
    assert!(reads
        .session(&SessionId::new("01J0000000000000000000000Z").unwrap())
        .await
        .unwrap()
        .is_none());

    // 幂等发送（消息 + run 单事务）。
    let message = user_message(MESSAGE_1, SESSION_A, "client-1");
    let run_record = run(RUN_1, SESSION_A, MESSAGE_1);
    let outcome = queue
        .execute(StoreCommand::BeginRunIdempotent {
            message: message.clone(),
            run: run_record.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        outcome,
        StoreOutcome::RunAccepted {
            message_id: message.id.clone(),
            run_id: run_record.id.clone(),
            duplicate: false,
        }
    );

    // run：StartRun → FinishRun → 读回。
    queue
        .execute(StoreCommand::StartRun {
            run_id: run_record.id.clone(),
            started_at: 12,
        })
        .await
        .unwrap();
    let started = reads.run(&run_record.id).await.unwrap().unwrap();
    assert_eq!(started.status, RunStatus::Running);
    assert_eq!(started.started_at, 12);
    queue
        .execute(StoreCommand::FinishRun {
            run_id: run_record.id.clone(),
            status: RunStatus::Failed,
            error: Some("run_stream_timeout".to_owned()),
            finished_at: 13,
        })
        .await
        .unwrap();
    let finished = reads.run(&run_record.id).await.unwrap().unwrap();
    assert_eq!(finished.status, RunStatus::Failed);
    assert_eq!(finished.error.as_deref(), Some("run_stream_timeout"));
    assert_eq!(finished.finished_at, Some(13));
    assert!(reads
        .run(&RunId::new("01J0000000000000000000000Z").unwrap())
        .await
        .unwrap()
        .is_none());

    // 消息读：幂等键 / 分页 / 正文更新。
    let stored = reads
        .message_by_client_msg_id(&SessionId::new(SESSION_A).unwrap(), "client-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.id, message.id);
    assert_eq!(stored.seq, 1, "seq 自动分配");
    let page = reads
        .messages_page(&SessionId::new(SESSION_A).unwrap(), None, 10)
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    let page_after = reads
        .messages_page(&SessionId::new(SESSION_A).unwrap(), Some(1), 10)
        .await
        .unwrap();
    assert!(page_after.is_empty());
    queue
        .execute(StoreCommand::UpdateMessageContent {
            message_id: message.id.clone(),
            content: "updated".to_owned(),
        })
        .await
        .unwrap();
    let updated = reads
        .message_by_client_msg_id(&SessionId::new(SESSION_A).unwrap(), "client-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.content, "updated");

    // 助手消息（client_msg_id = NULL；工具调用 JSON 列）。
    queue
        .execute(StoreCommand::InsertMessage {
            message: Message {
                id: MessageId::new("01J000000000000000000000M2").unwrap(),
                session_id: SessionId::new(SESSION_A).unwrap(),
                run_id: Some(run_record.id.clone()),
                client_msg_id: None,
                role: MessageRole::Assistant,
                content: "done".to_owned(),
                content_parts: Some(serde_json::json!([{"type": "text"}])),
                tool_calls: Some(serde_json::json!([{"name": "read"}])),
                parent_message_id: None,
                seq: 0,
                created_at: 14,
            },
        })
        .await
        .unwrap();
    let page = reads
        .messages_page(&SessionId::new(SESSION_A).unwrap(), None, 10)
        .await
        .unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[1].tool_calls.as_ref().unwrap()[0]["name"], "read");

    // 权限：插入 → 读回 → 待审批过滤 → 决议/超时。
    let record = PermissionRecord {
        id: "01J000000000000000000000P1".to_owned(),
        session_id: Some(SESSION_A.to_owned()),
        request_id: Some("req-1".to_owned()),
        resource: "fs.write".to_owned(),
        action: "write".to_owned(),
        target: Some("C:/ws/a.txt".to_owned()),
        decision: PermissionDecision::Ask,
        scope: None,
        status: PermissionStatus::Pending,
        requested_at: 20,
        resolved_at: None,
        resolver: None,
    };
    queue
        .execute(StoreCommand::InsertPermission {
            record: record.clone(),
        })
        .await
        .unwrap();
    let read = reads.permission(&record.id).await.unwrap().unwrap();
    assert_eq!(read.resource, "fs.write");
    assert_eq!(read.status, PermissionStatus::Pending);
    let pending_a = reads
        .permissions_pending(Some(&SessionId::new(SESSION_A).unwrap()))
        .await
        .unwrap();
    assert_eq!(pending_a.len(), 1);
    let pending_b = reads
        .permissions_pending(Some(&SessionId::new(SESSION_B).unwrap()))
        .await
        .unwrap();
    assert!(pending_b.is_empty());
    let pending_all = reads.permissions_pending(None).await.unwrap();
    assert_eq!(pending_all.len(), 1);
    assert!(reads.permission("missing").await.unwrap().is_none());
    queue
        .execute(StoreCommand::ResolvePermission {
            id: record.id.clone(),
            decision: PermissionDecision::Allow,
            scope: Some(PermissionScope::Session),
            status: PermissionStatus::Resolved,
            resolved_at: 21,
            resolver: Some("user".to_owned()),
        })
        .await
        .unwrap();
    let resolved = reads.permission(&record.id).await.unwrap().unwrap();
    assert_eq!(resolved.decision, PermissionDecision::Allow);
    assert_eq!(resolved.scope, Some(PermissionScope::Session));

    // 审计：写入 + 读取（含 null 字段）。
    queue
        .execute(StoreCommand::InsertAudit {
            record: AuditLogRecord {
                id: "01J000000000000000000000A1".to_owned(),
                session_id: Some(SESSION_A.to_owned()),
                runtime_id: Some("mock".to_owned()),
                actor: "user".to_owned(),
                action: "permission.resolved".to_owned(),
                resource: Some("fs.write:write".to_owned()),
                detail: Some("{\"request_id\":\"req-1\"}".to_owned()),
                result: Some("resolved".to_owned()),
                ts: 21,
            },
        })
        .await
        .unwrap();
    queue
        .execute(StoreCommand::InsertAudit {
            record: AuditLogRecord {
                id: "01J000000000000000000000A2".to_owned(),
                session_id: None,
                runtime_id: None,
                actor: "system".to_owned(),
                action: "permission.timeout".to_owned(),
                resource: None,
                detail: None,
                result: Some("timeout".to_owned()),
                ts: 22,
            },
        })
        .await
        .unwrap();
    let audit = reads.audit_log(10).await.unwrap();
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0].action, "permission.resolved");
    assert_eq!(audit[1].action, "permission.timeout");
    assert!(audit[1].detail.is_none());

    storage.shutdown().await.unwrap();
}

/// 写命令失败路径：唯一约束冲突 → 结构化错误 + 失败计数；队列关闭后拒绝。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_failure_and_closed_queue_are_reported() {
    let temp = tempfile::tempdir().unwrap();
    let storage = open(&temp.path().join("aether.db"));
    let queue = storage.queue().clone();
    queue
        .execute(StoreCommand::EnsureRuntime { runtime: runtime() })
        .await
        .unwrap();
    queue
        .execute(StoreCommand::InsertSession {
            session: session(SESSION_A, "A"),
        })
        .await
        .unwrap();
    // 重复主键 → 事务失败（约束错误码保留）。
    let error = queue
        .execute(StoreCommand::InsertSession {
            session: session(SESSION_A, "dup"),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), "write_transaction_failed");
    let metrics = queue.metrics();
    assert!(metrics.failed_entries >= 1, "失败计数必须递增: {metrics:?}");
    assert!(
        metrics.committed_entries >= 2,
        "成功命令计数必须保留: {metrics:?}"
    );

    // 关停后命令被拒绝。
    let queue_after = queue.clone();
    storage.shutdown().await.unwrap();
    let error = queue_after
        .execute(StoreCommand::InsertAudit {
            record: AuditLogRecord {
                id: "01J000000000000000000000A9".to_owned(),
                session_id: None,
                runtime_id: None,
                actor: "system".to_owned(),
                action: "late".to_owned(),
                resource: None,
                detail: None,
                result: None,
                ts: 1,
            },
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), "write_queue_closed");
}

/// 事件批次收集期间到达命令：先提交事件批次（FIFO），批次触发标记为 `Command`。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_arriving_during_event_batch_preserves_fifo() {
    let temp = tempfile::tempdir().unwrap();
    let storage = open(&temp.path().join("aether.db"));
    let queue = storage.queue().clone();

    let event = EventEnvelope {
        v: EVENT_ENVELOPE_VERSION,
        id: EventId::new("01J00000000000000000000EV1").unwrap(),
        session_id: SessionId::new(SESSION_A).unwrap(),
        run_id: None,
        runtime_id: RuntimeId::new("mock").unwrap(),
        seq: 1,
        ts: 1,
        payload: EventPayload::Log(LogPayload {
            level: LogLevel::Info,
            message: "batch".to_owned(),
        }),
    };
    // 确定性顺序：先把事件推入写队列（轮询一次完成 send），再入队命令；
    // 写任务收下事件进入批次收集窗口时命令已在队列中 → BatchTrigger::Command。
    let mut append = std::pin::pin!(queue.append_events(vec![event]));
    let mut pending = false;
    std::future::poll_fn(|cx| match append.as_mut().poll(cx) {
        std::task::Poll::Pending => {
            pending = true;
            std::task::Poll::Ready(())
        }
        std::task::Poll::Ready(_) => std::task::Poll::Ready(()),
    })
    .await;
    assert!(pending, "append 应在等待落盘回执（事件已入队）");

    let outcome = queue
        .execute(StoreCommand::EnsureRuntime { runtime: runtime() })
        .await
        .unwrap();
    assert_eq!(outcome, StoreOutcome::Applied { affected: 1 });

    let receipt = append.await.unwrap();
    assert_eq!(
        receipt.trigger,
        BatchTrigger::Command,
        "命令到达应中断批次收集（先提交事件批次，保持 FIFO）"
    );
    assert_eq!(receipt.entries, 1);
    // 命令在事件批次之后执行：两者均已提交。
    let metrics = queue.metrics();
    assert!(metrics.committed_entries >= 2);
    storage.shutdown().await.unwrap();
}

/// M2-05 父取消级联：`SessionQuery.parent_session_id` 过滤（含终态子会话仍可读）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_filter_by_parent_session_id() {
    const CHILD: &str = "01J000000000000000000000C";
    const GRANDCHILD: &str = "01J000000000000000000000G";
    let temp = tempfile::tempdir().unwrap();
    let storage = open(&temp.path().join("aether.db"));
    let queue = storage.queue().clone();
    let reads = storage.reads().clone();
    seed(&queue, &reads).await;

    let mut child = session(CHILD, "child");
    child.parent_session_id = Some(SessionId::new(SESSION_A).unwrap());
    queue
        .execute(StoreCommand::InsertSession { session: child })
        .await
        .unwrap();
    let mut grandchild = session(GRANDCHILD, "grandchild");
    grandchild.parent_session_id = Some(SessionId::new(CHILD).unwrap());
    grandchild.status = SessionStatus::Completed;
    queue
        .execute(StoreCommand::InsertSession {
            session: grandchild,
        })
        .await
        .unwrap();

    let children = reads
        .sessions(SessionQuery {
            parent_session_id: Some(SESSION_A.to_owned()),
            ..SessionQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id.as_str(), CHILD);
    assert_eq!(
        children[0]
            .parent_session_id
            .as_ref()
            .map(SessionId::as_str),
        Some(SESSION_A)
    );

    let grandchildren = reads
        .sessions(SessionQuery {
            parent_session_id: Some(CHILD.to_owned()),
            ..SessionQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(grandchildren.len(), 1);
    assert_eq!(grandchildren[0].id.as_str(), GRANDCHILD);
    assert_eq!(grandchildren[0].status, SessionStatus::Completed);

    // 组合过滤：父 + 状态。
    let completed = reads
        .sessions(SessionQuery {
            parent_session_id: Some(CHILD.to_owned()),
            status: Some(SessionStatus::Completed),
            ..SessionQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(completed.len(), 1);
    let idle = reads
        .sessions(SessionQuery {
            parent_session_id: Some(CHILD.to_owned()),
            status: Some(SessionStatus::Idle),
            ..SessionQuery::default()
        })
        .await
        .unwrap();
    assert!(idle.is_empty());

    storage.shutdown().await.unwrap();
}
