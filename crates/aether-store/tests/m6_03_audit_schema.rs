//! M6-03 DoD1/DoD2：迁移 `0005`（审计三复合索引）schema 断言 + `audit_query`
//! 过滤/排序/分页行为（ADR-023 §2.1-1/§2.1-3）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use aether_store::{
    migrate_with, AuditLogRecord, AuditQuery, StoreCommand, StoreRuntime, WriteQueueConfig,
    EMBEDDED_MIGRATIONS,
};
use rusqlite::Connection;
use tokio::runtime::Handle;

fn audit_indexes(conn: &Connection) -> Vec<String> {
    let mut statement = conn
        .prepare("SELECT name FROM pragma_index_list('audit_log') ORDER BY name")
        .unwrap();
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

#[test]
fn migration_0005_creates_audit_indexes_and_is_idempotent() {
    let (_dir, store) = common::open_temp_store("m6-03-audit-indexes");
    let indexes = audit_indexes(store.connection());
    for expected in [
        "idx_audit_ts",
        "idx_audit_session_ts",
        "idx_audit_actor_ts",
        "idx_audit_action_ts",
    ] {
        assert!(
            indexes.contains(&expected.to_owned()),
            "缺少索引 {expected}：{indexes:?}"
        );
    }
    // 复合索引列序（列, ts, id）与 ADR-023 附录 A-2（v0.3）一致。
    for (index, table, columns) in [
        ("idx_audit_session_ts", "audit_log", "session_id,ts,id"),
        ("idx_audit_actor_ts", "audit_log", "actor,ts,id"),
        ("idx_audit_action_ts", "audit_log", "action,ts,id"),
    ] {
        let mut statement = store
            .connection()
            .prepare(&format!("PRAGMA index_info({index})"))
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .unwrap();
        let mut listed: Vec<(i64, String)> = rows
            .map(|row| {
                let (seq, name) = row.unwrap();
                (seq, name.unwrap_or_default())
            })
            .collect();
        listed.sort();
        let names: Vec<String> = listed.into_iter().map(|(_, name)| name).collect();
        assert_eq!(names.join(","), columns, "{index} 列序不符（表 {table}）");
    }

    let applied = store.applied_migrations().unwrap();
    // 迁移清单推进至最新（M7-09 合入 0007、M7-07 合入 0008；本断言为「应用至最新」语义）。
    assert_eq!(applied.last().map(|item| item.version), Some(8));

    // 旧库（0001）→ 增量迁移：历史 audit_log 行保留，索引补齐（幂等复跑）。
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    {
        let mut conn = Connection::open(&path).unwrap();
        migrate_with(&mut conn, &EMBEDDED_MIGRATIONS[..1]).unwrap();
        conn.execute(
            "INSERT INTO audit_log (id, session_id, runtime_id, actor, action, resource, detail, result, ts) \
             VALUES ('legacy-1', NULL, 'mock', 'system', 'runtime.status_changed', 'runtime:mock', NULL, 'cold→ready', 42)",
            [],
        )
        .unwrap();
    }
    let store = aether_store::Store::open(&path).unwrap();
    assert_eq!(
        store.applied_migrations().unwrap().len(),
        EMBEDDED_MIGRATIONS.len()
    );
    let (action, result, ts): (String, Option<String>, i64) = store
        .connection()
        .query_row(
            "SELECT action, result, ts FROM audit_log WHERE id = 'legacy-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(action, "runtime.status_changed");
    assert_eq!(result.as_deref(), Some("cold→ready"));
    assert_eq!(ts, 42);
    let indexes = audit_indexes(store.connection());
    assert!(indexes.contains(&"idx_audit_session_ts".to_owned()));
}

#[test]
fn filtered_queries_use_audit_indexes() {
    let (_dir, store) = common::open_temp_store("m6-03-audit-query-plan");
    let plan = |sql: &str| -> String {
        let mut statement = store
            .connection()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap();
        rows.map(Result::unwrap).collect::<Vec<String>>().join(" | ")
    };
    let by_actor = plan("SELECT id FROM audit_log WHERE actor = 'agent' ORDER BY ts DESC, id DESC");
    assert!(
        by_actor.contains("idx_audit_actor_ts"),
        "actor 过滤必须命中 idx_audit_actor_ts：{by_actor}"
    );
    let by_session =
        plan("SELECT id FROM audit_log WHERE session_id = 'x' ORDER BY ts DESC, id DESC");
    assert!(
        by_session.contains("idx_audit_session_ts"),
        "会话过滤必须命中 idx_audit_session_ts：{by_session}"
    );
    let by_action = plan("SELECT id FROM audit_log WHERE action = 'tool.call' ORDER BY ts DESC, id DESC");
    assert!(
        by_action.contains("idx_audit_action_ts"),
        "action 过滤必须命中 idx_audit_action_ts：{by_action}"
    );
}

fn record(id: &str, actor: &str, action: &str, resource: &str, ts: i64) -> AuditLogRecord {
    AuditLogRecord {
        id: id.to_owned(),
        session_id: Some("01J0000000000000000000000A".to_owned()),
        runtime_id: Some("mock".to_owned()),
        actor: actor.to_owned(),
        action: action.to_owned(),
        resource: Some(resource.to_owned()),
        detail: Some(format!("{{\"id\":\"{id}\"}}")),
        result: Some("success".to_owned()),
        ts,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_query_filters_sorting_pagination_and_total() {
    let temp = tempfile::tempdir().unwrap();
    let storage = StoreRuntime::open(temp.path().join("aether.db"), WriteQueueConfig::default(), &Handle::current())
        .unwrap();
    let queue = storage.queue().clone();
    let reads = storage.reads().clone();

    let rows = [
        record("a1", "agent", "tool.call", "mock.echo", 10),
        record("a2", "agent", "tool.call", "mock.read_file", 20),
        record("u1", "user", "session.created", "session:01J0000000000000000000000A", 30),
        record("s1", "system", "runtime.status_changed", "runtime:mock", 40),
        record("a3", "agent", "tool.call", "mock.echo", 50),
    ];
    // 同 ts 稳定排序断言：`ts DESC, id DESC`（id 倒序兜底）。
    let tie = record("a0", "agent", "tool.call", "mock.echo", 50);
    for row in rows.iter().cloned().chain([tie]) {
        queue
            .execute(StoreCommand::InsertAudit { record: row })
            .await
            .unwrap();
    }

    // 全量：稳定排序 ts DESC, id DESC（同 ts=50 时 id 大者在前）。
    let all = reads
        .audit_query(AuditQuery::default())
        .await
        .unwrap();
    assert_eq!(all.rows.len(), 6);
    assert!(!all.has_more);
    assert_eq!(all.limit, aether_store::AUDIT_QUERY_DEFAULT_LIMIT);
    let ids: Vec<&str> = all.rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, vec!["a3", "a0", "s1", "u1", "a2", "a1"]);

    // actor 过滤 + limit/offset 分页（has_more 探测：多取一行）。
    let page = reads
        .audit_query(AuditQuery {
            actor: Some("agent".to_owned()),
            limit: Some(2),
            offset: 1,
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert!(page.has_more, "actor=agent 命中 4 行，offset=1/limit=2 后仍有下一页");
    assert_eq!(page.limit, 2);
    assert_eq!(page.offset, 1);
    let ids: Vec<&str> = page.rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, vec!["a0", "a2"]);

    // 尾页：has_more=false。
    let tail = reads
        .audit_query(AuditQuery {
            actor: Some("agent".to_owned()),
            limit: Some(2),
            offset: 3,
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert!(!tail.has_more);
    assert_eq!(tail.rows.len(), 1);

    // 时间闭区间（含端点）。
    let window = reads
        .audit_query(AuditQuery {
            from_ts: Some(20),
            to_ts: Some(50),
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(window.rows.len(), 5, "闭区间 [20,50] 命中 a2/u1/s1/a3/a0");

    // action/resource/session 等值过滤。
    let by_action = reads
        .audit_query(AuditQuery {
            action: Some("tool.call".to_owned()),
            resource: Some("mock.echo".to_owned()),
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(by_action.rows.len(), 3);
    let by_session = reads
        .audit_query(AuditQuery {
            session_id: Some("01J000000000000000000000Z9".to_owned()),
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert!(by_session.rows.is_empty());
    assert!(!by_session.has_more);

    // limit 上限钳制（本层防御；IPC 层已拒绝 >500）。
    let clamped = reads
        .audit_query(AuditQuery {
            limit: Some(usize::MAX),
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(clamped.limit, aether_store::AUDIT_QUERY_MAX_LIMIT);
}
