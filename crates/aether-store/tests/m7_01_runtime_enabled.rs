//! M7-01 DoD7：迁移 `0006`（`runtimes.enabled`）的列/约束断言、增量迁移幂等、
//! 既有行不受影响（历史库升级后缺省 `enabled=1`）；以及偏好写路径与读路径
//! （`runtime_preferences` / `active_work_counts`）的存储行为（ADR-015 决策 5）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aether_core::{
    Run, RunId, RunStatus, Runtime, RuntimeId, RuntimeStatus, Session, SessionId, SessionStatus,
    TokenUsage,
};
use aether_store::{
    migrate, migrate_with, StoreCommand, StoreRuntime, WriteQueueConfig, EMBEDDED_MIGRATIONS,
};
use rusqlite::Connection;
use tokio::runtime::Handle;

fn runtime_row() -> Runtime {
    Runtime {
        id: RuntimeId::new("mock").unwrap(),
        name: "Mock".to_owned(),
        kind: "mock".to_owned(),
        version: "0.1.0".to_owned(),
        protocol: "1.0".to_owned(),
        capabilities: vec![],
        endpoint: Some("C:/bundle/aether-mock-adapter.exe".to_owned()),
        config: serde_json::json!({}),
        status: RuntimeStatus::Disabled,
        status_reason: Some("user_disabled".to_owned()),
        last_seen_at: None,
        created_at: 100,
        updated_at: 200,
    }
}

fn open_runtime(path: &std::path::Path) -> StoreRuntime {
    StoreRuntime::open(path, WriteQueueConfig::default(), &Handle::current()).unwrap()
}

fn session_row(id: &str, status: SessionStatus) -> Session {
    Session {
        id: SessionId::new(id).unwrap(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: None,
        parent_session_id: None,
        title: "t".to_owned(),
        status,
        model: None,
        thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
        permission_mode: aether_core::PermissionMode::default(),
        preset_id: None,
        system_prompt: None,
        config: serde_json::json!({}),
        token_usage: TokenUsage::default(),
        created_at: 1,
        updated_at: 1,
        closed_at: None,
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

/// DoD7：迁移 `0006` 落列（NOT NULL DEFAULT 1）；重复迁移幂等；历史行升级后缺省启用。
#[tokio::test(flavor = "multi_thread")]
async fn migration_0006_adds_runtime_enabled_with_default_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("aether.db");

    // 旧库路径：先应用到 0005（模拟历史库），再升级到最新（0006 + M7-09 的 0007）。
    {
        let mut connection = Connection::open(&db).unwrap();
        let before: Vec<_> = EMBEDDED_MIGRATIONS
            .iter()
            .filter(|file| file.version <= 5)
            .copied()
            .collect();
        migrate_with(&mut connection, &before).unwrap();
        // 历史库已有 runtimes 行（不含 enabled 列）。
        connection
            .execute(
                "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
                 VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1)",
                [],
            )
            .unwrap();
        let applied = migrate_with(&mut connection, EMBEDDED_MIGRATIONS).unwrap();
        assert_eq!(
            applied.iter().map(|item| item.version).collect::<Vec<_>>(),
            vec![6, 7],
            "增量应用 0006（本任务）与 0007（M7-09 presets）"
        );
        // 列断言：NOT NULL DEFAULT 1。
        let (notnull, default, kind): (i64, Option<String>, String) = connection
            .query_row(
                "SELECT \"notnull\", dflt_value, type FROM pragma_table_info('runtimes') \
                 WHERE name = 'enabled'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(notnull, 1);
        assert_eq!(default.as_deref(), Some("1"));
        assert!(kind.to_uppercase().contains("INT"), "列类型: {kind}");
        // 历史行升级后缺省 enabled=1（DEFAULT 生效于 ALTER）。
        let enabled: i64 = connection
            .query_row("SELECT enabled FROM runtimes WHERE id = 'mock'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(enabled, 1);
        // 幂等：重复迁移返回空。
        let again = migrate_with(&mut connection, EMBEDDED_MIGRATIONS).unwrap();
        assert!(again.is_empty());
    }

    // 新库路径：一次性迁移到最新（含 0006；M7-09 起为 0007）。
    let fresh = dir.path().join("fresh.db");
    {
        let mut connection = Connection::open(&fresh).unwrap();
        let applied = migrate(&mut connection).unwrap();
        assert_eq!(
            applied.last().map(|item| item.version),
            Some(7),
            "新库应用到最新（0007）"
        );
    }
}

/// DoD7：`SetRuntimeEnabled` 行不存在时完成 upsert；已存在时仅更新
/// `enabled`/`updated_at`（不覆盖 `EnsureRuntime` 维护的摘要字段）；`EnsureRuntime`
/// 的 upsert 不得覆盖 `enabled`（ADR-015 决策 5）。
#[tokio::test(flavor = "multi_thread")]
async fn set_runtime_enabled_upserts_and_ensure_runtime_never_overwrites_preference() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&dir.path().join("aether.db"));

    // 行不存在：完整 upsert（字段来自 Runtime 行）。
    runtime
        .queue()
        .execute(StoreCommand::SetRuntimeEnabled {
            runtime: runtime_row(),
            enabled: false,
        })
        .await
        .unwrap();
    let preferences = runtime.reads().runtime_preferences().await.unwrap();
    assert_eq!(preferences.get("mock"), Some(&false));

    // 已存在：仅更新 enabled/updated_at；名称/版本不被覆盖。
    let mut renamed = runtime_row();
    renamed.name = "不应写入".to_owned();
    renamed.updated_at = 999;
    runtime
        .queue()
        .execute(StoreCommand::SetRuntimeEnabled {
            runtime: renamed,
            enabled: true,
        })
        .await
        .unwrap();
    let preferences = runtime.reads().runtime_preferences().await.unwrap();
    assert_eq!(preferences.get("mock"), Some(&true));
    let connection = Connection::open(dir.path().join("aether.db")).unwrap();
    let (name, updated_at): (String, i64) = connection
        .query_row(
            "SELECT name, updated_at FROM runtimes WHERE id = 'mock'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(name, "Mock", "SetRuntimeEnabled 不得覆盖摘要字段");
    assert_eq!(updated_at, 999);

    // EnsureRuntime upsert：DO UPDATE 不得包含 enabled 列（偏好保持 false）。
    runtime
        .queue()
        .execute(StoreCommand::SetRuntimeEnabled {
            runtime: runtime_row(),
            enabled: false,
        })
        .await
        .unwrap();
    let mut ensured = runtime_row();
    ensured.name = "Mock（Ensure 更新）".to_owned();
    ensured.updated_at = 1_000;
    runtime
        .queue()
        .execute(StoreCommand::EnsureRuntime { runtime: ensured })
        .await
        .unwrap();
    let preferences = runtime.reads().runtime_preferences().await.unwrap();
    assert_eq!(
        preferences.get("mock"),
        Some(&false),
        "EnsureRuntime 不得覆盖用户启停偏好"
    );
    let (name, enabled): (String, i64) = connection
        .query_row(
            "SELECT name, enabled FROM runtimes WHERE id = 'mock'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(name, "Mock（Ensure 更新）");
    assert_eq!(enabled, 0);
}

/// DoD2 读路径口径（ADR-015 §2.1#2）：在途 = 非终态会话（creating/running/paused/
/// waiting_permission）∪ queued/running run；idle/completed/failed/cancelled 与终态 run 不计。
#[tokio::test(flavor = "multi_thread")]
async fn active_work_counts_follow_adr_015_in_flight_definition() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&dir.path().join("aether.db"));
    runtime
        .queue()
        .execute(StoreCommand::EnsureRuntime {
            runtime: runtime_row(),
        })
        .await
        .unwrap();

    // 会话：running（计）+ idle（不计）+ completed（不计）。
    for (id, status) in [
        ("01J00000000000000000000R1", SessionStatus::Running),
        ("01J00000000000000000000I1", SessionStatus::Idle),
        ("01J00000000000000000000C1", SessionStatus::Completed),
    ] {
        runtime
            .queue()
            .execute(StoreCommand::InsertSession {
                session: session_row(id, status),
            })
            .await
            .unwrap();
    }
    // run：queued（计）挂在 idle 会话上（联合口径：run 自身计入）+ succeeded（不计）。
    runtime
        .queue()
        .execute(StoreCommand::InsertRun {
            run: run_row(
                "01J00000000000000000000Q1",
                "01J00000000000000000000I1",
                RunStatus::Queued,
            ),
        })
        .await
        .unwrap();
    runtime
        .queue()
        .execute(StoreCommand::InsertRun {
            run: run_row(
                "01J00000000000000000000S1",
                "01J00000000000000000000C1",
                RunStatus::Succeeded,
            ),
        })
        .await
        .unwrap();

    let counts = runtime.reads().active_work_counts().await.unwrap();
    assert_eq!(counts.get("mock"), Some(&2), "1 running 会话 + 1 queued run");

    // 终态收口后归零。
    runtime
        .queue()
        .execute(StoreCommand::UpdateSessionStatus {
            session_id: SessionId::new("01J00000000000000000000R1").unwrap(),
            status: SessionStatus::Idle,
            updated_at: 2,
            closed_at: None,
        })
        .await
        .unwrap();
    runtime
        .queue()
        .execute(StoreCommand::FinishRun {
            run_id: RunId::new("01J00000000000000000000Q1").unwrap(),
            status: RunStatus::Failed,
            error: Some("adapter_unavailable".to_owned()),
            finished_at: 2,
        })
        .await
        .unwrap();
    let counts = runtime.reads().active_work_counts().await.unwrap();
    assert_eq!(counts.get("mock"), None, "全部终态后该运行时不在计数表");
}
