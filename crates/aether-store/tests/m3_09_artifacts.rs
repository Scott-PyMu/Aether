//! M3-09 存储层集成：`artifacts` 表（ADR-010 决策 1；D3 单写者）。
//!
//! 覆盖实施计划 M3-09 DoD1/DoD2（存储侧）/DoD3/DoD6：
//! - 迁移 0003 建表约束：`UNIQUE(session_id, path)`、`ON DELETE CASCADE`、列与默认值；
//! - 写路径经单写队列（`WriteQueue::execute`，D3）；
//! - `InsertArtifact` 幂等（同会话同路径返回既有引用，不重复插入、不覆盖原行）；
//! - `RemoveArtifact` 幂等（不存在影响 0 行）；`ReadPool::artifacts` 排序与形状；
//! - 跨重启保留（关闭后重开读取一致）；
//! - 引用写/删不产生 `events` 行、不写 `workspaces`（不触发 `workspace_set` 语义）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use aether_core::SessionId;
use aether_store::{
    ArtifactRecord, Store, StoreCommand, StoreError, StoreOutcome, StoreRuntime, WriteQueueConfig,
};

const SESSION: &str = "01J0000000000000000000000S";

fn artifact(id: &str, path: &str, created_at: i64) -> ArtifactRecord {
    ArtifactRecord {
        id: id.to_owned(),
        session_id: SESSION.to_owned(),
        path: path.to_owned(),
        kind: "file".to_owned(),
        size_bytes: Some(7),
        created_at,
    }
}

fn is_constraint_violation(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(inner, _))
            if inner.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

/// 写事务内 FK 拒绝（`apply_command` 经 `txn_failed` 保留扩展码 787）。
fn is_foreign_key_write_failure(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::WriteTransactionFailed {
            code: Some(787),
            ..
        }
    )
}

/// DoD1：迁移 0003 `artifacts` 表/约束断言 + `ON DELETE CASCADE` 行为。
#[test]
fn dod1_artifacts_table_matches_adr_010_and_cascades() {
    let (_dir, store) = common::open_temp_store("m3-09-artifacts-schema");
    let connection = store.connection();

    let columns: Vec<(String, String, i64, Option<String>)> = {
        let mut statement = connection
            .prepare(
                "SELECT name, type, \"notnull\", dflt_value FROM pragma_table_info('artifacts') \
                 ORDER BY cid",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let expected: Vec<(String, String, i64, Option<String>)> = vec![
        ("id".to_owned(), "TEXT".to_owned(), 0, None),
        ("session_id".to_owned(), "TEXT".to_owned(), 1, None),
        ("path".to_owned(), "TEXT".to_owned(), 1, None),
        (
            "kind".to_owned(),
            "TEXT".to_owned(),
            1,
            Some("'file'".to_owned()),
        ),
        ("size_bytes".to_owned(), "INTEGER".to_owned(), 0, None),
        ("created_at".to_owned(), "INTEGER".to_owned(), 1, None),
    ];
    assert_eq!(
        columns, expected,
        "artifacts 列定义必须与 ADR-010 附录 A 一致"
    );

    // UNIQUE(session_id, path)：表级约束（自动索引），键序为 (session_id, path)。
    let index_names: Vec<String> = {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'artifacts'",
            )
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let unique_columns: Vec<String> = index_names
        .iter()
        .map(|name| {
            let mut statement = connection
                .prepare(&format!("PRAGMA index_info('{name}')"))
                .unwrap();
            let columns: Vec<String> = statement
                .query_map([], |row| row.get::<_, String>(2))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            columns
        })
        .find(|columns| columns == &["session_id".to_owned(), "path".to_owned()])
        .unwrap_or_else(|| {
            panic!("必须存在 UNIQUE(session_id, path) 自动索引：{index_names:?}");
        });
    assert_eq!(unique_columns, vec!["session_id", "path"]);

    // 外键：session_id → sessions(id) ON DELETE CASCADE。
    let fks: Vec<(String, String, String, String)> = {
        let mut statement = connection
            .prepare("PRAGMA foreign_key_list(artifacts)")
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(3)?, // from
                    row.get::<_, String>(2)?, // referenced table
                    row.get::<_, String>(4)?, // referenced column
                    row.get::<_, String>(6)?, // on_delete
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        fks,
        vec![(
            "session_id".to_owned(),
            "sessions".to_owned(),
            "id".to_owned(),
            "CASCADE".to_owned(),
        )]
    );

    connection
        .execute_batch(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1); \
             INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES ('s1', 'mock', 't', 'idle', 1, 1); \
             INSERT INTO artifacts (id, session_id, path, kind, created_at) \
             VALUES ('a1', 's1', '/tmp/a.txt', 'file', 1);",
        )
        .unwrap();
    let duplicate = connection
        .execute(
            "INSERT INTO artifacts (id, session_id, path, kind, created_at) \
             VALUES ('a2', 's1', '/tmp/a.txt', 'file', 2)",
            [],
        )
        .unwrap_err();
    let duplicate = StoreError::from(duplicate);
    assert!(is_constraint_violation(&duplicate), "实际: {duplicate:?}");

    connection
        .execute("DELETE FROM sessions WHERE id = 's1'", [])
        .unwrap();
    let remaining: i64 = connection
        .query_row("SELECT COUNT(*) FROM artifacts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        remaining, 0,
        "删除会话必须级联删除引用（ON DELETE CASCADE）"
    );
}

fn seed_runtime_and_session(dir: &tempfile::TempDir) {
    let store = Store::open(common::db_path(dir)).unwrap();
    store
        .execute_write(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1)",
        )
        .unwrap();
    store
        .execute_write(&format!(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES ('{SESSION}', 'mock', 't', 'idle', 1, 1)"
        ))
        .unwrap();
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

/// DoD2（存储侧）+ DoD3：单写队列幂等登记 / 删除幂等 / 排序 / 跨重启保留。
#[test]
fn dod2_dod3_write_queue_idempotency_ordering_and_restart() {
    let dir = common::temp_dir("m3-09-artifacts-write");
    let handle = new_runtime();
    seed_runtime_and_session(&dir);
    let runtime = StoreRuntime::open(
        common::db_path(&dir),
        WriteQueueConfig::default(),
        handle.handle(),
    )
    .unwrap();
    let queue = runtime.queue().clone();
    let reads = runtime.reads().clone();
    let session_id = SessionId::new(SESSION).unwrap();

    // 不存在会话时 FK 拒绝（命令后端另有存在性校验；此为 DB 兜底）。
    let fk_error = handle
        .block_on(queue.execute(StoreCommand::InsertArtifact {
            artifact: ArtifactRecord {
                session_id: "01J0000000000000000000000X".to_owned(),
                ..artifact("01J000000000000000000000A9", "/tmp/x.txt", 0)
            },
        }))
        .unwrap_err();
    assert!(
        is_foreign_key_write_failure(&fk_error),
        "实际: {fk_error:?}"
    );

    let committed_before = queue.metrics().committed_entries;

    // 乱序写入 3 条（created_at 2 / 1 / 3），验证列表升序。
    for (id, path, created_at, kind, size) in [
        (
            "01J000000000000000000000A2",
            "/tmp/b.txt",
            2,
            "file",
            Some(7),
        ),
        (
            "01J000000000000000000000A1",
            "/tmp/a.txt",
            1,
            "file",
            Some(7),
        ),
        ("01J000000000000000000000A3", "/tmp/c", 3, "directory", None),
    ] {
        let outcome = handle
            .block_on(queue.execute(StoreCommand::InsertArtifact {
                artifact: ArtifactRecord {
                    id: id.to_owned(),
                    session_id: SESSION.to_owned(),
                    path: path.to_owned(),
                    kind: kind.to_owned(),
                    size_bytes: size,
                    created_at,
                },
            }))
            .unwrap();
        assert!(
            matches!(
                outcome,
                StoreOutcome::ArtifactRecorded { inserted: true, .. }
            ),
            "首次登记必须新建: {outcome:?}"
        );
    }

    // 幂等：同路径再次登记返回既有行（原 id / created_at / kind 不被覆盖）。
    let duplicate = handle
        .block_on(queue.execute(StoreCommand::InsertArtifact {
            artifact: ArtifactRecord {
                id: "01J000000000000000000000A9".to_owned(),
                session_id: SESSION.to_owned(),
                path: "/tmp/a.txt".to_owned(),
                kind: "directory".to_owned(),
                size_bytes: None,
                created_at: 99,
            },
        }))
        .unwrap();
    match duplicate {
        StoreOutcome::ArtifactRecorded { artifact, inserted } => {
            assert!(!inserted, "重复添加必须幂等命中");
            assert_eq!(artifact.id, "01J000000000000000000000A1");
            assert_eq!(artifact.kind, "file", "既有引用不得被覆盖");
            assert_eq!(artifact.created_at, 1);
        }
        other => panic!("期望 ArtifactRecorded，实际 {other:?}"),
    }

    assert_eq!(
        queue.metrics().committed_entries,
        committed_before + 4,
        "引用写路径必须经单写队列（D3）"
    );

    let listed = handle.block_on(reads.artifacts(&session_id)).unwrap();
    let paths: Vec<&str> = listed.iter().map(|item| item.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["/tmp/a.txt", "/tmp/b.txt", "/tmp/c"],
        "按 created_at 升序"
    );
    assert_eq!(listed[2].kind, "directory");
    assert_eq!(listed[2].size_bytes, None, "目录 size_bytes 为 NULL");

    // 删除幂等：命中 1 行；再次删除 0 行（removed=false 语义）。
    let removed = handle
        .block_on(queue.execute(StoreCommand::RemoveArtifact {
            session_id: session_id.clone(),
            artifact_id: "01J000000000000000000000A2".to_owned(),
        }))
        .unwrap();
    assert_eq!(removed, StoreOutcome::Applied { affected: 1 });
    let removed_again = handle
        .block_on(queue.execute(StoreCommand::RemoveArtifact {
            session_id: session_id.clone(),
            artifact_id: "01J000000000000000000000A2".to_owned(),
        }))
        .unwrap();
    assert_eq!(removed_again, StoreOutcome::Applied { affected: 0 });

    // 跨重启保留：关闭存储运行时后重开，列表一致（引用持久化）。
    handle.block_on(runtime.shutdown()).unwrap();
    let reopened = Store::open(common::db_path(&dir)).unwrap();
    let connection = reopened.connection();
    let rows: Vec<(String, String, String)> = {
        let mut statement = connection
            .prepare("SELECT id, path, kind FROM artifacts ORDER BY created_at ASC, rowid ASC")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        rows,
        vec![
            (
                "01J000000000000000000000A1".to_owned(),
                "/tmp/a.txt".to_owned(),
                "file".to_owned()
            ),
            (
                "01J000000000000000000000A3".to_owned(),
                "/tmp/c".to_owned(),
                "directory".to_owned()
            ),
        ],
        "重启后引用列表必须一致"
    );
}

/// DoD6（存储侧）：引用增删不产生事件、不写 `workspaces`。
#[test]
fn dod6_artifact_writes_emit_no_events_and_do_not_bind_workspace() {
    let dir = common::temp_dir("m3-09-artifacts-no-side-effects");
    let handle = new_runtime();
    seed_runtime_and_session(&dir);
    let runtime = StoreRuntime::open(
        common::db_path(&dir),
        WriteQueueConfig::default(),
        handle.handle(),
    )
    .unwrap();
    let queue = runtime.queue().clone();

    {
        let store = Store::open(common::db_path(&dir)).unwrap();
        store
            .execute_write(
                "INSERT INTO events (id, session_id, runtime_id, seq, type, payload, ts) \
                 VALUES ('e1', 's1', 'mock', 1, 'session.created', '{}', 1)",
            )
            .unwrap();
    }

    let outcome = handle
        .block_on(queue.execute(StoreCommand::InsertArtifact {
            artifact: artifact("01J000000000000000000000A1", "/tmp/a.txt", 1),
        }))
        .unwrap();
    assert!(matches!(
        outcome,
        StoreOutcome::ArtifactRecorded { inserted: true, .. }
    ));
    handle
        .block_on(queue.execute(StoreCommand::RemoveArtifact {
            session_id: SessionId::new(SESSION).unwrap(),
            artifact_id: "01J000000000000000000000A1".to_owned(),
        }))
        .unwrap();

    handle.block_on(runtime.shutdown()).unwrap();
    let reopened = Store::open(common::db_path(&dir)).unwrap();
    let connection = reopened.connection();
    let events: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    let workspaces: i64 = connection
        .query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))
        .unwrap();
    let session_workspace: Option<String> = connection
        .query_row(
            "SELECT workspace_id FROM sessions WHERE id = ?1",
            [SESSION],
            |row| row.get(0),
        )
        .unwrap();
    let artifacts: i64 = connection
        .query_row("SELECT COUNT(*) FROM artifacts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(events, 1, "引用操作不得产生事件（事件表行数不变）");
    assert_eq!(
        workspaces, 0,
        "引用操作不得写 workspaces（不触发 workspace_set）"
    );
    assert_eq!(session_workspace, None, "会话工作区绑定不得被引用操作改写");
    assert_eq!(artifacts, 0, "删除后引用行已移除");
}
