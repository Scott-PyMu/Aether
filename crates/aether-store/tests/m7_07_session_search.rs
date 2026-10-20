//! M7-07 存储层：FTS5 检索、改名、硬删除与级联边界（ADR-025 §2.1）。
//!
//! 覆盖：
//! - FTS 触发器随 INSERT/UPDATE/DELETE 同步（标题 + 消息正文）；标题命中权重高于正文；
//! - `RenameSession` 单写队列更新 + FTS 同步；
//! - `DeleteSession` 事务内复核在途 run / pending 票据（`SessionBusy`），成功时级联
//!   `messages`/`runs`/`artifacts`/`permissions`、**`events` 不级联**（R7）；
//! - 查无会话 → `SessionNotFound`。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aether_core::{
    Message, MessageId, MessageRole, PermissionDecision, PermissionMode, PermissionStatus, Run,
    RunId, RunStatus, Runtime, RuntimeId, RuntimeStatus, Session, SessionId, SessionStatus,
    TokenUsage, THINKING_DEPTH_DEFAULT,
};
use aether_store::{
    ArtifactRecord, PermissionRecord, StoreCommand, StoreOutcome, StoreRuntime, WriteQueueConfig,
};
use rusqlite::{params, Connection};
use tokio::runtime::Handle;

const SESSION_TITLE: &str = "01J00000000000000000000S1";
const SESSION_BODY: &str = "01J00000000000000000000S2";

fn runtime_row() -> Runtime {
    Runtime {
        id: RuntimeId::new("mock").unwrap(),
        name: "Mock".to_owned(),
        kind: "mock".to_owned(),
        version: "0.1.0".to_owned(),
        protocol: "1.0".to_owned(),
        capabilities: vec![],
        endpoint: None,
        config: serde_json::json!({}),
        status: RuntimeStatus::Ready,
        status_reason: None,
        last_seen_at: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn session_row(id: &str, title: &str) -> Session {
    Session {
        id: SessionId::new(id).unwrap(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: None,
        parent_session_id: None,
        title: title.to_owned(),
        status: SessionStatus::Idle,
        model: None,
        thinking_depth: THINKING_DEPTH_DEFAULT,
        permission_mode: PermissionMode::default(),
        preset_id: None,
        system_prompt: None,
        config: serde_json::json!({}),
        token_usage: TokenUsage::default(),
        created_at: 1,
        updated_at: 1,
        closed_at: None,
    }
}

fn message_row(id: &str, session_id: &str, content: &str) -> Message {
    Message {
        id: MessageId::new(id).unwrap(),
        session_id: SessionId::new(session_id).unwrap(),
        run_id: None,
        client_msg_id: None,
        role: MessageRole::Assistant,
        content: content.to_owned(),
        content_parts: None,
        tool_calls: None,
        parent_message_id: None,
        seq: 0,
        created_at: 2,
    }
}

fn run_row(id: &str, session_id: &str, status: RunStatus) -> Run {
    Run {
        id: RunId::new(id).unwrap(),
        session_id: SessionId::new(session_id).unwrap(),
        status,
        input_message_id: None,
        thinking_depth: None,
        error: None,
        started_at: 1,
        finished_at: None,
    }
}

fn open_runtime(path: &std::path::Path) -> StoreRuntime {
    StoreRuntime::open(path, WriteQueueConfig::default(), &Handle::current()).unwrap()
}

async fn seed(runtime: &StoreRuntime) {
    runtime
        .queue()
        .execute(StoreCommand::EnsureRuntime {
            runtime: runtime_row(),
        })
        .await
        .unwrap();
    for session in [
        session_row(SESSION_TITLE, "alpha needle title"),
        session_row(SESSION_BODY, "unrelated title"),
    ] {
        runtime
            .queue()
            .execute(StoreCommand::InsertSession { session })
            .await
            .unwrap();
    }
    runtime
        .queue()
        .execute(StoreCommand::InsertMessage {
            message: message_row(
                "01J00000000000000000000M1",
                SESSION_BODY,
                "the alpha appears in the body",
            ),
        })
        .await
        .unwrap();
}

/// DoD2：FTS 标题/正文命中，标题权重高于正文；改名后索引同步。
#[tokio::test(flavor = "multi_thread")]
async fn fts_indexes_title_and_body_and_syncs_on_rename() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aether.db");
    let runtime = open_runtime(&path);
    seed(&runtime).await;

    let hits = runtime
        .reads()
        .search_sessions("\"alpha\"", 20, 0)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2, "标题与正文各一条命中");
    assert_eq!(hits[0].session.id.as_str(), SESSION_TITLE, "标题命中权重更高");
    assert_eq!(hits[0].source, "title");
    assert_eq!(hits[1].session.id.as_str(), SESSION_BODY);
    assert_eq!(hits[1].source, "message");
    assert!(hits[1].snippet.contains("[alpha]"), "snippet 标记命中词");

    // 改名：旧标题不再命中，新标题命中。
    runtime
        .queue()
        .execute(StoreCommand::RenameSession {
            id: SessionId::new(SESSION_TITLE).unwrap(),
            title: "renamed omega title".to_owned(),
            updated_at: 99,
        })
        .await
        .unwrap();
    let old = runtime
        .reads()
        .search_sessions("\"alpha\"", 20, 0)
        .await
        .unwrap();
    assert_eq!(old.len(), 1, "旧标题索引应被触发器删除");
    assert_eq!(old[0].session.id.as_str(), SESSION_BODY);
    let renamed = runtime
        .reads()
        .search_sessions("\"omega\"", 20, 0)
        .await
        .unwrap();
    assert_eq!(renamed.len(), 1);
    assert_eq!(renamed[0].session.id.as_str(), SESSION_TITLE);
    assert_eq!(renamed[0].source, "title");
}

/// DoD3：改名只动 title/updated_at；FTS 正文随消息更新同步；删除后检索为空。
#[tokio::test(flavor = "multi_thread")]
async fn message_content_update_and_session_delete_sync_fts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aether.db");
    let runtime = open_runtime(&path);
    seed(&runtime).await;

    runtime
        .queue()
        .execute(StoreCommand::UpdateMessageContent {
            message_id: MessageId::new("01J00000000000000000000M1").unwrap(),
            content: "the beta appears in the body".to_owned(),
        })
        .await
        .unwrap();
    let alpha = runtime
        .reads()
        .search_sessions("\"alpha\"", 20, 0)
        .await
        .unwrap();
    assert_eq!(alpha.len(), 1, "正文旧词应被更新触发器删除");
    assert_eq!(alpha[0].session.id.as_str(), SESSION_TITLE);
    let beta = runtime
        .reads()
        .search_sessions("\"beta\"", 20, 0)
        .await
        .unwrap();
    assert_eq!(beta.len(), 1);
    assert_eq!(beta[0].source, "message");

    let deleted = runtime
        .queue()
        .execute(StoreCommand::DeleteSession {
            id: SessionId::new(SESSION_BODY).unwrap(),
        })
        .await
        .unwrap();
    match deleted {
        StoreOutcome::SessionDeleted { messages, .. } => assert_eq!(messages, 1),
        other => panic!("预期 SessionDeleted，实际 {other:?}"),
    }
    let beta = runtime
        .reads()
        .search_sessions("\"beta\"", 20, 0)
        .await
        .unwrap();
    assert!(beta.is_empty(), "删除会话后正文索引应清空");
}

/// DoD2 性能基准（ADR-025 §2.1-2：≥1e5 会话 / ≥1e6 消息，P95 ≤300ms）。
///
/// 数据构造走测试侧单事务批量 INSERT（触发器同步索引）；`cargo test` 默认跳过，
/// 由 `verify:m7-07` 以 `--ignored` 显式执行并归档耗时。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "perf: 1e5 sessions / 1e6 messages (run via verify:m7-07)"]
async fn search_perf_p95_under_300ms_at_1e5_sessions_1e6_messages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aether.db");
    let runtime = open_runtime(&path);
    let mut conn = Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO runtimes (id, name, kind, version, protocol, capabilities, config, status, created_at, updated_at) \
         VALUES ('mock', 'Mock', 'mock', '0.1.0', '1.0', '[]', '{}', 'ready', 1, 1)",
        [],
    )
    .unwrap();
    let transaction = conn.transaction().unwrap();
    {
        let mut session_stmt = transaction
            .prepare(
                "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                 VALUES (?1, 'mock', ?2, 'idle', ?3, ?3)",
            )
            .unwrap();
        for index in 0..100_000_i64 {
            let id = format!("01J{index:023}");
            let title = format!("session {index} needle-{}", index % 1_000);
            session_stmt
                .execute(params![id, title, index])
                .unwrap();
        }
    }
    {
        let mut message_stmt = transaction
            .prepare(
                "INSERT INTO messages (id, session_id, role, content, seq, created_at) \
                 VALUES (?1, ?2, 'user', ?3, 1, ?4)",
            )
            .unwrap();
        for index in 0..1_000_000_i64 {
            let id = format!("01K{index:023}");
            let session = format!("01J{:023}", index % 100_000);
            let content = format!("message body {index} needle-{}", index % 1_000);
            message_stmt
                .execute(params![id, session, content, index])
                .unwrap();
        }
    }
    transaction.commit().unwrap();

    let mut samples = Vec::with_capacity(20);
    for _ in 0..20 {
        let started = std::time::Instant::now();
        let hits = runtime
            .reads()
            .search_sessions("\"needle-777\"", 20, 0)
            .await
            .unwrap();
        samples.push(started.elapsed().as_millis());
        assert!(!hits.is_empty());
    }
    samples.sort_unstable();
    let p95 = samples[(samples.len() * 95).div_ceil(100) - 1];
    println!("[m7-07 perf] samples={samples:?} p95={p95}ms");
    assert!(
        p95 <= 300,
        "P95 必须 ≤300ms（ADR-025 §2.1-2），实际 {p95}ms"
    );
}

/// DoD4：删除事务内复核在途 run / pending 票据；成功级联四表、events 保留。
#[tokio::test(flavor = "multi_thread")]
async fn delete_session_refuses_busy_and_cascades_without_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aether.db");
    let runtime = open_runtime(&path);
    seed(&runtime).await;

    // 在途 run → SessionBusy（不删除）。
    runtime
        .queue()
        .execute(StoreCommand::InsertRun {
            run: run_row(
                "01J00000000000000000000R1",
                SESSION_TITLE,
                RunStatus::Running,
            ),
        })
        .await
        .unwrap();
    let busy = runtime
        .queue()
        .execute(StoreCommand::DeleteSession {
            id: SessionId::new(SESSION_TITLE).unwrap(),
        })
        .await
        .unwrap();
    assert!(matches!(
        busy,
        StoreOutcome::SessionBusy { active_runs: 1, .. }
    ));
    assert!(
        runtime
            .reads()
            .session(&SessionId::new(SESSION_TITLE).unwrap())
            .await
            .unwrap()
            .is_some(),
        "busy 时不得删除会话行"
    );
    runtime
        .queue()
        .execute(StoreCommand::FinishRun {
            run_id: RunId::new("01J00000000000000000000R1").unwrap(),
            status: RunStatus::Cancelled,
            error: None,
            finished_at: 3,
        })
        .await
        .unwrap();

    // pending 票据 → SessionBusy；决议后可删除。
    let permission = PermissionRecord {
        id: "01J00000000000000000000P1".to_owned(),
        session_id: Some(SESSION_TITLE.to_owned()),
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
    runtime
        .queue()
        .execute(StoreCommand::InsertPermission {
            record: permission.clone(),
        })
        .await
        .unwrap();
    let busy = runtime
        .queue()
        .execute(StoreCommand::DeleteSession {
            id: SessionId::new(SESSION_TITLE).unwrap(),
        })
        .await
        .unwrap();
    assert!(matches!(
        busy,
        StoreOutcome::SessionBusy {
            pending_permissions: 1,
            ..
        }
    ));
    runtime
        .queue()
        .execute(StoreCommand::ResolvePermission {
            id: permission.id,
            decision: PermissionDecision::Deny,
            scope: None,
            status: PermissionStatus::Resolved,
            resolved_at: 21,
            resolver: Some("user".to_owned()),
            target: None,
        })
        .await
        .unwrap();

    // 引用 + 消息 + 终态 run；events 由测试侧直接写入（无 StoreCommand 事件入口）。
    runtime
        .queue()
        .execute(StoreCommand::InsertArtifact {
            artifact: ArtifactRecord {
                id: "01J00000000000000000000A1".to_owned(),
                session_id: SESSION_TITLE.to_owned(),
                path: "C:/ws/a.txt".to_owned(),
                kind: "file".to_owned(),
                size_bytes: Some(1),
                created_at: 2,
            },
        })
        .await
        .unwrap();
    runtime
        .queue()
        .execute(StoreCommand::InsertMessage {
            message: message_row("01J00000000000000000000M2", SESSION_TITLE, "hello"),
        })
        .await
        .unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
         VALUES (?1, ?2, NULL, 'mock', 1, 'session.updated', '{}', 1, 1)",
        params!["01J00000000000000000000E1", SESSION_TITLE],
    )
    .unwrap();

    let deleted = runtime
        .queue()
        .execute(StoreCommand::DeleteSession {
            id: SessionId::new(SESSION_TITLE).unwrap(),
        })
        .await
        .unwrap();
    match deleted {
        StoreOutcome::SessionDeleted {
            messages,
            runs,
            artifacts,
            permissions,
            events_retained,
        } => {
            assert_eq!(messages, 1);
            assert_eq!(runs, 1);
            assert_eq!(artifacts, 1);
            assert_eq!(permissions, 1);
            assert_eq!(events_retained, 1);
        }
        other => panic!("预期 SessionDeleted，实际 {other:?}"),
    }

    assert!(
        runtime
            .reads()
            .session(&SessionId::new(SESSION_TITLE).unwrap())
            .await
            .unwrap()
            .is_none()
    );
    let messages: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            [SESSION_TITLE],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(messages, 0, "messages 必须级联删除");
    let permissions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM permissions WHERE session_id = ?1",
            [SESSION_TITLE],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(permissions, 0, "permissions 必须级联删除");
    let events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1",
            [SESSION_TITLE],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(events, 1, "events 不级联（R7）");

    // 查无会话。
    let missing = runtime
        .queue()
        .execute(StoreCommand::DeleteSession {
            id: SessionId::new("01J00000000000000000000S9").unwrap(),
        })
        .await
        .unwrap();
    assert!(matches!(missing, StoreOutcome::SessionNotFound));
}
