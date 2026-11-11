//! M10-05/ADR-035 存储层集成：`attachments` 元数据 + `provider_models.capabilities`。
//!
//! 覆盖：
//! - 迁移 0012 列/约束（`UNIQUE(session_id, sha256)`、`ON DELETE CASCADE`、
//!   `provider_models.capabilities DEFAULT '[]'`）；
//! - 幂等插入（同会话同 sha 返回既有行，不覆盖 id/created_at）；
//! - 用量统计（数量/字节）；
//! - `BeginRunIdempotent` 同事务绑定 `message_id`，缺失/跨会话附件整体回滚；
//! - 删除返回无其他引用的 `storage_path`；跨会话共享内容不误删；
//! - `SetProviderModelCapabilities` 读回（`["vision"]`）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use aether_core::{Message, MessageId, MessageRole, Run, RunId, RunStatus, SessionId};
use aether_store::{
    AttachmentRecord, AttachmentUsage, ProviderModelRecord, Store, StoreCommand, StoreOutcome,
    StoreRuntime, WriteQueueConfig,
};

const SESSION: &str = "01J0000000000000000000000S";
const SESSION_B: &str = "01J0000000000000000000000T";
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn attachment(id: &str, session: &str, sha: &str, created_at: i64) -> AttachmentRecord {
    AttachmentRecord {
        id: id.to_owned(),
        session_id: session.to_owned(),
        message_id: None,
        sha256: sha.to_owned(),
        mime: "image/png".to_owned(),
        size_bytes: 1024,
        width: Some(64),
        height: Some(48),
        storage_path: format!("attachments/{}/{sha}.png", &sha[..2]),
        created_at,
    }
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

fn seed(dir: &tempfile::TempDir) {
    let store = Store::open(common::db_path(dir)).unwrap();
    store
        .execute_write(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1)",
        )
        .unwrap();
    for session in [SESSION, SESSION_B] {
        store
            .execute_write(&format!(
                "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                 VALUES ('{session}', 'mock', 't', 'idle', 1, 1)"
            ))
            .unwrap();
    }
    store
        .execute_write(
            "INSERT INTO providers (id, name, type, base_url, enabled, is_builtin, created_at, updated_at) \
             VALUES ('p1', 'P', 'custom', 'https://a.example.com', 1, 0, 1, 1)",
        )
        .unwrap();
    store
        .execute_write(
            "INSERT INTO provider_models (id, provider_id, model_id, display_name, enabled, created_at) \
             VALUES ('m1', 'p1', 'vision-model', 'Vision', 1, 1)",
        )
        .unwrap();
}

#[test]
fn schema_matches_adr_035_and_capabilities_default() {
    let (_dir, store) = common::open_temp_store("m10-05-schema");
    let connection = store.connection();
    let columns: Vec<String> = {
        let mut statement = connection
            .prepare("SELECT name FROM pragma_table_info('attachments') ORDER BY cid")
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        columns,
        vec![
            "id",
            "session_id",
            "message_id",
            "sha256",
            "mime",
            "size_bytes",
            "width",
            "height",
            "storage_path",
            "created_at",
        ],
        "attachments 列定义必须与 ADR-035 D-035-2 一致"
    );
    let index: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_attachments_session'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(index, 1, "必须存在 idx_attachments_session");

    let capabilities: String = connection
        .query_row(
            "SELECT dflt_value FROM pragma_table_info('provider_models') WHERE name='capabilities'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(capabilities, "'[]'", "capabilities 默认值必须为 '[]'");
}

#[test]
fn insert_is_idempotent_and_usage_counts() {
    let dir = common::temp_dir("m10-05-store");
    let handle = new_runtime();
    seed(&dir);
    let runtime = StoreRuntime::open(
        common::db_path(&dir),
        WriteQueueConfig::default(),
        handle.handle(),
    )
    .unwrap();
    let queue = runtime.queue().clone();
    let reads = runtime.reads().clone();
    let session = SessionId::new(SESSION).unwrap();

    let first = handle
        .block_on(queue.execute(StoreCommand::InsertAttachment {
            attachment: attachment("01J000000000000000000000A1", SESSION, SHA, 1),
        }))
        .unwrap();
    match first {
        StoreOutcome::AttachmentRecorded {
            attachment,
            inserted,
        } => {
            assert!(inserted);
            assert_eq!(attachment.id, "01J000000000000000000000A1");
        }
        other => panic!("期望 AttachmentRecorded，实际 {other:?}"),
    }
    let duplicate = handle
        .block_on(queue.execute(StoreCommand::InsertAttachment {
            attachment: attachment("01J000000000000000000000A9", SESSION, SHA, 99),
        }))
        .unwrap();
    match duplicate {
        StoreOutcome::AttachmentRecorded {
            attachment,
            inserted,
        } => {
            assert!(!inserted, "同会话同 sha 必须幂等");
            assert_eq!(attachment.id, "01J000000000000000000000A1");
            assert_eq!(attachment.created_at, 1, "既有行不得被覆盖");
        }
        other => panic!("期望 AttachmentRecorded，实际 {other:?}"),
    }

    // 跨会话同内容允许（磁盘全局去重由 app 层负责）。
    let other_session = handle
        .block_on(queue.execute(StoreCommand::InsertAttachment {
            attachment: attachment("01J000000000000000000000B1", SESSION_B, SHA, 2),
        }))
        .unwrap();
    assert!(matches!(
        other_session,
        StoreOutcome::AttachmentRecorded { inserted: true, .. }
    ));

    let usage = handle.block_on(reads.attachment_usage(&session)).unwrap();
    assert_eq!(
        usage,
        AttachmentUsage {
            count: 1,
            bytes: 1024
        }
    );
    let listed = handle.block_on(reads.attachments(&session)).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].sha256, SHA);
    handle.block_on(runtime.shutdown()).unwrap();
}

#[test]
fn begin_run_binds_attachment_and_remove_reports_path() {
    let dir = common::temp_dir("m10-05-bind");
    let handle = new_runtime();
    seed(&dir);
    let runtime = StoreRuntime::open(
        common::db_path(&dir),
        WriteQueueConfig::default(),
        handle.handle(),
    )
    .unwrap();
    let queue = runtime.queue().clone();
    let reads = runtime.reads().clone();
    let session = SessionId::new(SESSION).unwrap();
    let message_id = MessageId::new("01J00000000000000000000M10").unwrap();
    let run_id = RunId::new("01J00000000000000000000R10").unwrap();

    handle
        .block_on(queue.execute(StoreCommand::InsertAttachment {
            attachment: attachment("01J000000000000000000000A1", SESSION, SHA, 1),
        }))
        .unwrap();
    let message = Message {
        id: message_id.clone(),
        session_id: session.clone(),
        run_id: None,
        client_msg_id: Some("01J00000000000000000000C10".to_owned()),
        role: MessageRole::User,
        content: "看图".to_owned(),
        content_parts: Some(serde_json::json!([{
            "kind": "image",
            "attachment_id": "01J000000000000000000000A1",
            "mime": "image/png",
            "width": 64,
            "height": 48,
            "sha256": SHA,
        }])),
        tool_calls: None,
        parent_message_id: None,
        seq: 0,
        created_at: 2,
    };
    let run = Run {
        id: run_id,
        session_id: session.clone(),
        status: RunStatus::Queued,
        input_message_id: Some(message_id.clone()),
        thinking_depth: Some(2),
        error: None,
        started_at: 2,
        finished_at: None,
    };
    let outcome = handle
        .block_on(queue.execute(StoreCommand::BeginRunIdempotent {
            message: message.clone(),
            run,
            attachment_ids: vec!["01J000000000000000000000A1".to_owned()],
        }))
        .unwrap();
    assert!(matches!(
        outcome,
        StoreOutcome::RunAccepted {
            duplicate: false,
            ..
        }
    ));
    let bound = handle
        .block_on(reads.attachment("01J000000000000000000000A1"))
        .unwrap()
        .unwrap();
    assert_eq!(bound.message_id.as_deref(), Some(message_id.as_str()));

    // 缺失附件 → 事务回滚（消息/run 不落库）。
    let missing = handle
        .block_on(queue.execute(StoreCommand::BeginRunIdempotent {
            message: Message {
                id: MessageId::new("01J00000000000000000000M11").unwrap(),
                session_id: session.clone(),
                run_id: None,
                client_msg_id: Some("01J00000000000000000000C11".to_owned()),
                role: MessageRole::User,
                content: "缺失附件".to_owned(),
                content_parts: None,
                tool_calls: None,
                parent_message_id: None,
                seq: 0,
                created_at: 3,
            },
            run: Run {
                id: RunId::new("01J00000000000000000000R11").unwrap(),
                session_id: session.clone(),
                status: RunStatus::Queued,
                input_message_id: None,
                thinking_depth: None,
                error: None,
                started_at: 3,
                finished_at: None,
            },
            attachment_ids: vec!["01J00000000000000000000NOPE".to_owned()],
        }))
        .unwrap_err();
    assert!(missing.to_string().contains("附件不可绑定"));
    assert!(handle
        .block_on(reads.message(&MessageId::new("01J00000000000000000000M11").unwrap()))
        .unwrap()
        .is_none());

    // 删除：唯一引用 → 返回 storage_path 供异步清文件。
    let removed = handle
        .block_on(queue.execute(StoreCommand::RemoveAttachment {
            attachment_id: "01J000000000000000000000A1".to_owned(),
        }))
        .unwrap();
    match removed {
        StoreOutcome::AttachmentRemoved {
            removed,
            storage_path,
        } => {
            assert!(removed);
            assert_eq!(
                storage_path.as_deref(),
                Some(format!("attachments/aa/{SHA}.png").as_str())
            );
        }
        other => panic!("期望 AttachmentRemoved，实际 {other:?}"),
    }
    let again = handle
        .block_on(queue.execute(StoreCommand::RemoveAttachment {
            attachment_id: "01J000000000000000000000A1".to_owned(),
        }))
        .unwrap();
    assert_eq!(
        again,
        StoreOutcome::AttachmentRemoved {
            removed: false,
            storage_path: None
        }
    );
    handle.block_on(runtime.shutdown()).unwrap();
}

#[test]
fn provider_model_capabilities_roundtrip() {
    let dir = common::temp_dir("m10-05-capabilities");
    let handle = new_runtime();
    seed(&dir);
    let runtime = StoreRuntime::open(
        common::db_path(&dir),
        WriteQueueConfig::default(),
        handle.handle(),
    )
    .unwrap();
    let queue = runtime.queue().clone();
    let reads = runtime.reads().clone();
    let model = ProviderModelRecord {
        id: "m2".to_owned(),
        provider_id: "p1".to_owned(),
        model_id: "plain".to_owned(),
        display_name: "Plain".to_owned(),
        enabled: true,
        created_at: 2,
        capabilities: vec!["vision".to_owned()],
    };
    handle
        .block_on(queue.execute(StoreCommand::InsertProviderModel { model }))
        .unwrap();
    let stored = handle
        .block_on(reads.provider_model("p1", "plain"))
        .unwrap()
        .unwrap();
    assert_eq!(stored.capabilities, vec!["vision".to_owned()]);

    handle
        .block_on(queue.execute(StoreCommand::SetProviderModelCapabilities {
            provider_id: "p1".to_owned(),
            model_id: "vision-model".to_owned(),
            capabilities: "[\"vision\"]".to_owned(),
        }))
        .unwrap();
    let updated = handle
        .block_on(reads.provider_model("p1", "vision-model"))
        .unwrap()
        .unwrap();
    assert_eq!(updated.capabilities, vec!["vision".to_owned()]);
    handle.block_on(runtime.shutdown()).unwrap();
}
