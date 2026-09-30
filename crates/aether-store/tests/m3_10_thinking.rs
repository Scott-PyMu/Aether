//! M3-10 存储层集成：思考深度列（ADR-010 决策 2；迁移 0003；D3 单写者）。
//!
//! 覆盖实施计划 M3-10 DoD1（存储侧）与 DoD4（重放读侧）：
//! - 迁移 0003 `sessions.thinking_depth`（NOT NULL DEFAULT 2）与
//!   `runs.thinking_depth`（可空；历史行为 NULL）列断言；
//! - 迁移幂等（0001→0003 重复执行返回空；重开库不重复应用）；
//! - 领域命令往返：`InsertSession` / `InsertRun` 落生效值；
//!   `UpdateSessionThinkingDepth`（延迟能力门改写缺省 2）与
//!   `UpdateRunThinkingDepth`（run 生效值改写）经单写队列（D3）；
//! - 读侧 `ReadPool::session` / `run` / `unfinished_runs` 返回 thinking_depth。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use aether_core::{
    Message, MessageId, MessageRole, Run, RunId, RunStatus, Runtime, RuntimeId, RuntimeStatus,
    Session, SessionId, SessionStatus, TokenUsage, THINKING_DEPTH_DEFAULT,
};
use aether_store::migration::applied_migrations;
use aether_store::{
    migrate, ReadPool, Store, StoreCommand, StoreRuntime, WriteQueueConfig,
};
use tokio::runtime::Handle;

const SESSION: &str = "01J0000000000000000000000S";
const RUN_ACTIVE: &str = "01J000000000000000000000R1";
const RUN_HISTORY: &str = "01J000000000000000000000R2";
const MESSAGE: &str = "01J000000000000000000000M1";

fn pragma_columns(
    connection: &rusqlite::Connection,
    table: &str,
) -> Vec<(String, String, i64, Option<String>)> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT name, type, \"notnull\", dflt_value FROM pragma_table_info('{table}') ORDER BY cid"
        ))
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// DoD1：迁移 0003 列断言 + 0001→0003 幂等 + 历史 run 行为 NULL。
#[test]
fn dod1_migration_0003_columns_idempotent_and_historical_null() {
    let (dir, store) = common::open_temp_store("m3-10-schema");
    let connection = store.connection();

    // 迁移清单应用至 v3（0001 → 0002 → 0003）。
    let applied = applied_migrations(connection).unwrap();
    assert_eq!(
        applied.iter().map(|item| item.version).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "必须应用 0001/0002/0003"
    );

    // sessions.thinking_depth：INTEGER NOT NULL DEFAULT 2（ADR-010 附录 A）。
    let session_columns = pragma_columns(connection, "sessions");
    let thinking = session_columns
        .iter()
        .find(|(name, ..)| name == "thinking_depth")
        .expect("sessions.thinking_depth 列必须存在");
    assert_eq!(thinking.1, "INTEGER");
    assert_eq!(thinking.2, 1, "sessions.thinking_depth 必须 NOT NULL");
    assert_eq!(
        thinking.3.as_deref(),
        Some("2"),
        "sessions.thinking_depth 缺省必须为 2"
    );

    // runs.thinking_depth：INTEGER（可空；迁移前历史行为 NULL）。
    let run_columns = pragma_columns(connection, "runs");
    let thinking = run_columns
        .iter()
        .find(|(name, ..)| name == "thinking_depth")
        .expect("runs.thinking_depth 列必须存在");
    assert_eq!(thinking.1, "INTEGER");
    assert_eq!(thinking.2, 0, "runs.thinking_depth 必须可空（历史行为 NULL）");
    assert_eq!(thinking.3, None, "runs.thinking_depth 不得有缺省值");

    // 未显式提供 thinking_depth 的行：会话落缺省 2；历史 run 落 NULL。
    connection
        .execute_batch(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1); \
             INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES ('s1', 'mock', 't', 'idle', 1, 1); \
             INSERT INTO runs (id, session_id, status, started_at) \
             VALUES ('r1', 's1', 'succeeded', 1);",
        )
        .unwrap();
    let depth: i64 = connection
        .query_row("SELECT thinking_depth FROM sessions WHERE id = 's1'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(depth, i64::from(THINKING_DEPTH_DEFAULT), "会话缺省必须为 2");
    let historical: Option<i64> = connection
        .query_row("SELECT thinking_depth FROM runs WHERE id = 'r1'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(historical, None, "历史 run 行必须为 NULL（不回溯填充）");

    let snapshot = applied.clone();
    drop(store);

    // 重开库：不重复应用（幂等）；再显式执行 migrate 返回空列表。
    let store = Store::open(common::db_path(&dir)).unwrap();
    assert_eq!(
        applied_migrations(store.connection()).unwrap(),
        snapshot,
        "重开库不得重复应用迁移（checksum 记录一致）"
    );
    drop(store);
    let mut connection = rusqlite::Connection::open(common::db_path(&dir)).unwrap();
    let applied_now = migrate(&mut connection).unwrap();
    assert!(
        applied_now.is_empty(),
        "0001→0003 重复执行必须幂等（返回空应用列表），实际 {applied_now:?}"
    );
}

fn open_storage(path: &std::path::Path) -> StoreRuntime {
    StoreRuntime::open(path, WriteQueueConfig::default(), &Handle::current()).unwrap()
}

fn runtime() -> Runtime {
    Runtime {
        id: RuntimeId::new("mock").unwrap(),
        name: "Mock".to_owned(),
        kind: "mock".to_owned(),
        version: "0.1.0".to_owned(),
        protocol: "1.0".to_owned(),
        capabilities: vec!["thinking_depth".to_owned()],
        endpoint: None,
        config: serde_json::json!({}),
        status: RuntimeStatus::Ready,
        status_reason: None,
        last_seen_at: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn session(id: &str, thinking_depth: u8) -> Session {
    Session {
        id: SessionId::new(id).unwrap(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: None,
        parent_session_id: None,
        title: "m3-10".to_owned(),
        status: SessionStatus::Idle,
        model: None,
        thinking_depth,
        system_prompt: None,
        config: serde_json::json!({}),
        token_usage: TokenUsage::default(),
        created_at: 1,
        updated_at: 1,
        closed_at: None,
    }
}

fn message(id: &str, session_id: &str, client: &str) -> Message {
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
        created_at: 1,
    }
}

fn run(id: &str, session_id: &str, message_id: &str, thinking_depth: Option<u8>) -> Run {
    Run {
        id: RunId::new(id).unwrap(),
        session_id: SessionId::new(session_id).unwrap(),
        status: RunStatus::Queued,
        input_message_id: Some(MessageId::new(message_id).unwrap()),
        thinking_depth,
        error: None,
        started_at: 1,
        finished_at: None,
    }
}

/// DoD1/DoD4：领域命令落库往返 + 更新命令经单写队列 + 读侧返回生效值。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dod1_roundtrip_and_update_commands() {
    let temp = tempfile::tempdir().unwrap();
    let storage = open_storage(&temp.path().join("aether.db"));
    let queue = storage.queue().clone();
    let reads: ReadPool = storage.reads().clone();

    queue
        .execute(StoreCommand::EnsureRuntime { runtime: runtime() })
        .await
        .unwrap();
    queue
        .execute(StoreCommand::InsertSession {
            session: session(SESSION, 4),
        })
        .await
        .unwrap();
    queue
        .execute(StoreCommand::BeginRunIdempotent {
            message: message(MESSAGE, SESSION, "01J8ZQ5R0N7W9Y8X6V4T2S0KCM"),
            run: run(RUN_ACTIVE, SESSION, MESSAGE, Some(4)),
        })
        .await
        .unwrap();
    queue
        .execute(StoreCommand::InsertRun {
            run: run(RUN_HISTORY, SESSION, MESSAGE, None),
        })
        .await
        .unwrap();

    let read = reads
        .session(&SessionId::new(SESSION).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.thinking_depth, 4, "会话级思考深度必须往返一致");
    let read_run = reads
        .run(&RunId::new(RUN_ACTIVE).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read_run.thinking_depth, Some(4), "run 生效值必须往返一致");
    let historical = reads
        .run(&RunId::new(RUN_HISTORY).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(historical.thinking_depth, None, "历史 run 行为 NULL");

    // 未收口 run 读侧（重启收口路径）同样携带 thinking_depth（按行取值，含 NULL）。
    let unfinished = reads.unfinished_runs().await.unwrap();
    assert_eq!(unfinished.len(), 2);
    let active = unfinished
        .iter()
        .find(|item| item.id.as_str() == RUN_ACTIVE)
        .unwrap();
    assert_eq!(active.thinking_depth, Some(4));
    let historical_unfinished = unfinished
        .iter()
        .find(|item| item.id.as_str() == RUN_HISTORY)
        .unwrap();
    assert_eq!(historical_unfinished.thinking_depth, None);

    // 延迟能力门改写：sessions / runs 落缺省 2（ADR-010 决策 2）。
    queue
        .execute(StoreCommand::UpdateSessionThinkingDepth {
            session_id: SessionId::new(SESSION).unwrap(),
            thinking_depth: THINKING_DEPTH_DEFAULT,
            updated_at: 2,
        })
        .await
        .unwrap();
    queue
        .execute(StoreCommand::UpdateRunThinkingDepth {
            run_id: RunId::new(RUN_ACTIVE).unwrap(),
            thinking_depth: THINKING_DEPTH_DEFAULT,
        })
        .await
        .unwrap();

    let read = reads
        .session(&SessionId::new(SESSION).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.thinking_depth, THINKING_DEPTH_DEFAULT);
    assert_eq!(read.updated_at, 2, "改写必须更新 updated_at");
    let read_run = reads
        .run(&RunId::new(RUN_ACTIVE).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read_run.thinking_depth, Some(THINKING_DEPTH_DEFAULT));

    // sessions 列表查询（session_list 语义）返回 thinking_depth。
    let sessions = reads
        .sessions(aether_store::SessionQuery::default())
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].thinking_depth, THINKING_DEPTH_DEFAULT);

    storage.shutdown().await.unwrap();
}
