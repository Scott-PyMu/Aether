//! M6-01 DoD1：迁移 `0004`（`sessions.permission_mode` + `permission_rules`）的表/约束/
//! 索引断言、增量迁移幂等与历史 `permissions` 行不受影响；以及规则 CRUD 与授权行重建
//! 读路径的存储行为（ADR-019 附录 A-1/A-2）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use aether_core::{
    PermissionDecision, PermissionMode, PermissionScope, PermissionStatus, Runtime, RuntimeId,
    RuntimeStatus, Session, SessionId, SessionStatus, TokenUsage,
};
use aether_store::{
    migrate_with, PermissionGrantRecord, PermissionRecord, PermissionRuleRecord, StoreCommand,
    StoreError, StoreRuntime, WriteQueueConfig, EMBEDDED_MIGRATIONS,
};
use rusqlite::Connection;
use tokio::runtime::Handle;

const SESSION_A: &str = "01J0000000000000000000000A";

fn open_runtime(path: &std::path::Path) -> StoreRuntime {
    StoreRuntime::open(path, WriteQueueConfig::default(), &Handle::current()).unwrap()
}

/// 经单写队列播种运行时与会话（M6-01：新会话缺省档 = workspace_write）。
async fn seed_runtime_and_session(queue: &aether_store::WriteQueue) -> Session {
    queue
        .execute(StoreCommand::EnsureRuntime {
            runtime: Runtime {
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
            },
        })
        .await
        .unwrap();
    let session = Session {
        id: SessionId::new(SESSION_A).unwrap(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: None,
        parent_session_id: None,
        title: "t".to_owned(),
        status: SessionStatus::Idle,
        model: None,
        thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
        permission_mode: PermissionMode::default(),
        system_prompt: None,
        config: serde_json::json!({}),
        token_usage: TokenUsage::default(),
        created_at: 1,
        updated_at: 1,
        closed_at: None,
    };
    queue
        .execute(StoreCommand::InsertSession {
            session: session.clone(),
        })
        .await
        .unwrap();
    session
}

fn is_constraint_violation(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(inner, _))
            if inner.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

fn pragma_columns(conn: &Connection, table: &str) -> Vec<(String, String, i64, Option<String>)> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn seed_runtime_and_session_sql(conn: &Connection) {
    conn.execute(
        "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
         VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
         VALUES (?1, 'mock', 't', 'idle', 1, 1)",
        [SESSION_A],
    )
    .unwrap();
}

#[test]
fn permission_mode_column_is_text_not_null_default_workspace_write() {
    let (_dir, store) = common::open_temp_store("m6-01-mode-column");
    let columns = pragma_columns(store.connection(), "sessions");
    let mode = columns
        .iter()
        .find(|(name, ..)| name == "permission_mode")
        .expect("sessions.permission_mode 列必须存在");
    assert_eq!(mode.1, "TEXT");
    assert_eq!(mode.2, 1, "sessions.permission_mode 必须 NOT NULL");
    assert_eq!(mode.3.as_deref(), Some("'workspace_write'"), "缺省档");

    // CHECK 枚举：三档合法值接受、非法值拒绝（设计附录 C / ADR-014 附录 B）。
    seed_runtime_and_session_sql(store.connection());

    // 升级后既有会话缺省档为 workspace_write（先于枚举改写断言）。
    let mode: String = store
        .connection()
        .query_row(
            &format!("SELECT permission_mode FROM sessions WHERE id = '{SESSION_A}'"),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(mode, "workspace_write");

    for mode in ["read_only", "workspace_write", "danger_full_access"] {
        store
            .execute_write(&format!(
                "UPDATE sessions SET permission_mode = '{mode}' WHERE id = '{SESSION_A}'"
            ))
            .unwrap_or_else(|error| panic!("permission_mode={mode} 应被接受: {error:?}"));
    }
    let error = store
        .execute_write(&format!(
            "UPDATE sessions SET permission_mode = 'full_access' WHERE id = '{SESSION_A}'"
        ))
        .unwrap_err();
    assert!(
        is_constraint_violation(&error),
        "非法档位必须被 CHECK 拒绝: {error:?}"
    );
}

#[test]
fn permission_rules_table_constraints_and_index() {
    let (_dir, store) = common::open_temp_store("m6-01-rules-table");
    let conn = store.connection();
    let columns = pragma_columns(conn, "permission_rules");
    let names: Vec<&str> = columns.iter().map(|(name, ..)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "id",
            "name",
            "resource",
            "action",
            "target_kind",
            "target",
            "decision",
            "priority",
            "enabled",
            "created_at",
            "updated_at",
        ],
        "permission_rules 列清单必须与 ADR-019 附录 A-1 一致"
    );
    let priority = columns
        .iter()
        .find(|(name, ..)| name == "priority")
        .expect("priority 列");
    assert_eq!(priority.3.as_deref(), Some("0"), "priority 缺省 0");
    let enabled = columns
        .iter()
        .find(|(name, ..)| name == "enabled")
        .expect("enabled 列");
    assert_eq!(enabled.3.as_deref(), Some("1"), "enabled 缺省 1");

    let insert = |sql: &str| conn.execute(sql, []).map(|_| ());
    // resource / action / target_kind / decision CHECK。
    assert!(insert(
        "INSERT INTO permission_rules (id,name,resource,action,target_kind,target,decision,priority,enabled,created_at,updated_at) \
         VALUES ('r-ok','n','fs.write','write','path','C:\\ws','deny',0,1,1,1)"
    )
    .is_ok());
    for bad in [
        "VALUES ('r-bad1','n','browser','read','any',NULL,'deny',0,1,1,1)",
        "VALUES ('r-bad2','n','exec','run','command','rm','deny',0,1,1,1)",
        "VALUES ('r-bad3','n','net','connect','url','x','deny',0,1,1,1)",
        "VALUES ('r-bad4','n','exec','exec','command','rm','ask',0,1,1,1)",
        "VALUES ('r-bad5','n','exec','exec','command','rm','deny',101,1,1,1)",
    ] {
        let sql = format!(
            "INSERT INTO permission_rules (id,name,resource,action,target_kind,target,decision,priority,enabled,created_at,updated_at) {bad}"
        );
        let error = insert(&sql).expect_err(&format!("必须被拒绝: {bad}"));
        let _ = error;
    }
    // 索引存在（DoD1：索引断言）。
    let indexes: Vec<String> = {
        let mut statement = conn
            .prepare("SELECT name FROM pragma_index_list('permission_rules') ORDER BY name")
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        rows.map(Result::unwrap).collect()
    };
    assert!(indexes.contains(&"idx_permission_rules_enabled".to_owned()));
}

#[test]
fn migration_0004_is_idempotent_and_preserves_existing_permissions_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");

    // 旧库：0001 应用后写入一条历史 permissions 行（旧口径 target 原文；无 0004）。
    {
        let mut conn = Connection::open(&path).unwrap();
        migrate_with(&mut conn, &EMBEDDED_MIGRATIONS[..1]).unwrap();
        conn.execute(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES (?1, 'mock', 't', 'idle', 1, 1)",
            [SESSION_A],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO permissions (id, session_id, resource, action, target, decision, scope, status, requested_at) \
             VALUES ('perm-legacy', ?1, 'fs.write', 'write', 'C:\\original\\path.txt', 'allow', 'session', 'resolved', 5)",
            [SESSION_A],
        )
        .unwrap();
    }

    // 以当前程序打开：增量应用 0002–0004（幂等）；历史行保持原样（不被改写）。
    let store = aether_store::Store::open(&path).unwrap();
    let applied = store.applied_migrations().unwrap();
    assert_eq!(applied.len(), EMBEDDED_MIGRATIONS.len());
    assert_eq!(applied.last().map(|item| item.version), Some(4));
    let (target, decision, scope, status): (Option<String>, String, Option<String>, String) = store
        .connection()
        .query_row(
            "SELECT target, decision, scope, status FROM permissions WHERE id = 'perm-legacy'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(target.as_deref(), Some("C:\\original\\path.txt"));
    assert_eq!(decision, "allow");
    assert_eq!(scope.as_deref(), Some("session"));
    assert_eq!(status, "resolved");
    // 新列已含旧会话（缺省档）。
    let mode: String = store
        .connection()
        .query_row(
            &format!("SELECT permission_mode FROM sessions WHERE id = '{SESSION_A}'"),
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(mode, "workspace_write");

    // 再次打开（幂等）：无新增迁移。
    let store2 = aether_store::Store::open(&path).unwrap();
    assert_eq!(
        store2.applied_migrations().unwrap().len(),
        EMBEDDED_MIGRATIONS.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_rule_crud_and_grants_readback() {
    let temp = tempfile::tempdir().unwrap();
    let storage = open_runtime(&temp.path().join("aether.db"));
    let queue = storage.queue().clone();
    let reads = storage.reads().clone();
    let session = seed_runtime_and_session(&queue).await;
    assert_eq!(session.permission_mode, PermissionMode::WorkspaceWrite);

    let rule = PermissionRuleRecord {
        id: "01J000000000000000000000R1".to_owned(),
        name: "deny-secrets".to_owned(),
        resource: "fs.write".to_owned(),
        action: "write".to_owned(),
        target_kind: "path".to_owned(),
        target: Some("C:\\secrets".to_owned()),
        decision: "deny".to_owned(),
        priority: 10,
        enabled: true,
        created_at: 100,
        updated_at: 100,
    };
    queue
        .execute(StoreCommand::InsertPermissionRule { rule: rule.clone() })
        .await
        .unwrap();
    let listed = reads.permission_rules().await.unwrap();
    assert_eq!(listed, vec![rule.clone()]);

    let mut updated = rule.clone();
    updated.name = "deny-secrets-v2".to_owned();
    updated.enabled = false;
    updated.priority = 20;
    updated.updated_at = 200;
    queue
        .execute(StoreCommand::UpdatePermissionRule {
            rule: updated.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        reads.permission_rules().await.unwrap(),
        vec![updated.clone()]
    );

    queue
        .execute(StoreCommand::SetPermissionRuleEnabled {
            id: rule.id.clone(),
            enabled: true,
            updated_at: 300,
        })
        .await
        .unwrap();
    assert!(reads.permission_rules().await.unwrap()[0].enabled);

    queue
        .execute(StoreCommand::DeletePermissionRule {
            id: rule.id.clone(),
        })
        .await
        .unwrap();
    assert!(reads.permission_rules().await.unwrap().is_empty());

    // 授权重建读路径：session/always + resolved + allow + target 非空。
    let record = PermissionRecord {
        id: "perm-1".to_owned(),
        session_id: Some(SESSION_A.to_owned()),
        request_id: Some("req-1".to_owned()),
        resource: "fs.write".to_owned(),
        action: "write".to_owned(),
        target: Some("C:\\ws\\a.txt".to_owned()),
        decision: PermissionDecision::Ask,
        scope: None,
        status: PermissionStatus::Pending,
        requested_at: 1,
        resolved_at: None,
        resolver: None,
    };
    queue
        .execute(StoreCommand::InsertPermission {
            record: record.clone(),
        })
        .await
        .unwrap();
    // 决议以 canonical 形态覆写 target（ADR-019 附录 A-2）。
    queue
        .execute(StoreCommand::ResolvePermission {
            id: record.id.clone(),
            decision: PermissionDecision::Allow,
            scope: Some(PermissionScope::Always),
            status: PermissionStatus::Resolved,
            resolved_at: 2,
            resolver: Some("user".to_owned()),
            target: Some("C:\\ws\\canonical\\a.txt".to_owned()),
        })
        .await
        .unwrap();
    let grants = reads.permission_grants().await.unwrap();
    assert_eq!(
        grants,
        vec![PermissionGrantRecord {
            session_id: Some(SESSION_A.to_owned()),
            resource: "fs.write".to_owned(),
            action: "write".to_owned(),
            target: "C:\\ws\\canonical\\a.txt".to_owned(),
        }]
    );
    // deny 决议不进入授权集。
    let deny_record = PermissionRecord {
        id: "perm-2".to_owned(),
        session_id: Some(SESSION_A.to_owned()),
        request_id: Some("req-2".to_owned()),
        resource: "fs.write".to_owned(),
        action: "write".to_owned(),
        target: Some("C:\\ws\\b.txt".to_owned()),
        decision: PermissionDecision::Ask,
        scope: None,
        status: PermissionStatus::Pending,
        requested_at: 3,
        resolved_at: None,
        resolver: None,
    };
    queue
        .execute(StoreCommand::InsertPermission {
            record: deny_record.clone(),
        })
        .await
        .unwrap();
    queue
        .execute(StoreCommand::ResolvePermission {
            id: deny_record.id.clone(),
            decision: PermissionDecision::Deny,
            scope: None,
            status: PermissionStatus::Resolved,
            resolved_at: 4,
            resolver: Some("user".to_owned()),
            target: None,
        })
        .await
        .unwrap();
    assert_eq!(reads.permission_grants().await.unwrap().len(), 1);

    // 会话级档位写路径（`session_permission_mode_set` 的存储半步）。
    queue
        .execute(StoreCommand::UpdateSessionPermissionMode {
            session_id: SessionId::new(SESSION_A).unwrap(),
            permission_mode: PermissionMode::ReadOnly,
            updated_at: 500,
        })
        .await
        .unwrap();
    let session = reads
        .session(&SessionId::new(SESSION_A).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.permission_mode, PermissionMode::ReadOnly);
    assert_eq!(session.status, SessionStatus::Idle);
    assert_eq!(session.token_usage, TokenUsage::default());
}
