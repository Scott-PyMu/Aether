//! M7-09 DoD1：迁移 `0007`（`presets` 表 + 3 条内置播种 + `sessions.preset_id`）的
//! 结构/播种/外键断言、增量迁移幂等，以及预设读写路径（ADR-026 §2.2 / 附录 A-1）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aether_core::{
    Preset, PresetId, Runtime, RuntimeId, RuntimeStatus, Session, SessionId, SessionStatus,
    TokenUsage,
};
use aether_store::{
    migrate, migrate_with, ReadPool, StoreCommand, StoreRuntime, WriteQueueConfig,
    EMBEDDED_MIGRATIONS,
};
use rusqlite::Connection;
use tokio::runtime::Handle;

const BUILTIN_IDS: [&str; 3] = [
    "01J00000000000000000000P01",
    "01J00000000000000000000P02",
    "01J00000000000000000000P03",
];

/// 迁移前版本（0007 为增量迁移，历史库升级路径）。
const PRE_PRESET_VERSION: i64 = 6;

fn open_runtime(path: &std::path::Path) -> StoreRuntime {
    StoreRuntime::open(path, WriteQueueConfig::default(), &Handle::current()).unwrap()
}

async fn ensure_runtime(reads: &ReadPool, write: &aether_store::WriteQueue) {
    let runtime = Runtime {
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
    };
    write
        .execute(StoreCommand::EnsureRuntime { runtime })
        .await
        .unwrap();
    let _ = reads;
}

fn session_row(id: &str, preset_id: Option<&str>) -> Session {
    Session {
        id: SessionId::new(id).unwrap(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: None,
        parent_session_id: None,
        title: "m7-09".to_owned(),
        status: SessionStatus::Idle,
        model: None,
        thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
        permission_mode: aether_core::PermissionMode::default(),
        preset_id: preset_id.map(|value| PresetId::new(value).unwrap()),
        system_prompt: None,
        config: serde_json::json!({}),
        token_usage: TokenUsage::default(),
        created_at: 1,
        updated_at: 1,
        closed_at: None,
    }
}

fn custom_preset(id: &str, name: &str, prompt: &str) -> Preset {
    Preset {
        id: PresetId::new(id).unwrap(),
        name: name.to_owned(),
        system_prompt: prompt.to_owned(),
        is_builtin: false,
        created_at: 10,
        updated_at: 10,
    }
}

/// DoD1：迁移 `0007` 建表 + 三条内置播种 + `sessions.preset_id`（含
/// `ON DELETE SET NULL` 外键）；迁移幂等；历史库升级只增量应用 0007。
#[tokio::test(flavor = "multi_thread")]
async fn migration_0007_creates_presets_seeds_builtins_and_adds_session_column() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("aether.db");

    // 历史库路径：先应用到 0006，再升级到最新（0007 + M7-07 的 0008）。
    {
        let mut connection = Connection::open(&db).unwrap();
        let before: Vec<_> = EMBEDDED_MIGRATIONS
            .iter()
            .filter(|file| file.version <= PRE_PRESET_VERSION)
            .copied()
            .collect();
        migrate_with(&mut connection, &before).unwrap();
        let applied = migrate_with(&mut connection, EMBEDDED_MIGRATIONS).unwrap();
        assert_eq!(
            applied.iter().map(|item| item.version).collect::<Vec<_>>(),
            vec![7, 8],
            "增量应用 0007（本任务）与 0008（M7-07 FTS5）"
        );

        // presets 表列清单（ADR-026 §2.2-1）。
        let columns: Vec<(String, String, i64, Option<String>)> = {
            let mut statement = connection
                .prepare(
                    "SELECT name, type, \"notnull\", dflt_value FROM pragma_table_info('presets') ORDER BY cid",
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
        assert_eq!(
            columns
                .iter()
                .map(|(name, ..)| name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "id",
                "name",
                "system_prompt",
                "is_builtin",
                "created_at",
                "updated_at"
            ]
        );
        assert_eq!(columns[0].2, 0, "id 为主键非 NOT NULL 声明式");
        assert_eq!(columns[3].3.as_deref(), Some("0"), "is_builtin 缺省 0");

        // 三条内置播种（文案以原型 `deepseek_html_V0.0.1.html:684-690` 为准）。
        let seeded: Vec<(String, String, String, i64)> = connection
            .prepare("SELECT id, name, system_prompt, is_builtin FROM presets ORDER BY rowid")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(seeded.len(), 3, "播种 3 条内置预设");
        assert_eq!(
            seeded.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
            BUILTIN_IDS.to_vec()
        );
        assert_eq!(
            seeded
                .iter()
                .map(|row| row.1.as_str())
                .collect::<Vec<_>>(),
            vec!["默认助手", "代码专家", "代码审查"]
        );
        for row in &seeded {
            assert_eq!(row.3, 1, "内置标记 is_builtin=1");
            assert!(!row.2.is_empty(), "内置提示词文案非空（落库定稿）");
        }
        assert!(seeded[0].2.contains("Aether 工作台中的通用 AI 助手"));
        assert!(seeded[1].2.contains("资深软件工程师"));
        assert!(seeded[2].2.contains("严格的代码审查者"));

        // sessions.preset_id 列 + ON DELETE SET NULL 外键（ADR-026 §2.2-4）。
        let (notnull, default): (i64, Option<String>) = connection
            .query_row(
                "SELECT \"notnull\", dflt_value FROM pragma_table_info('sessions') \
                 WHERE name = 'preset_id'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(notnull, 0, "preset_id 可空");
        assert_eq!(default, None);
        let fk: (String, String, String) = connection
            .query_row(
                "SELECT \"table\", \"from\", on_delete FROM pragma_foreign_key_list('sessions') \
                 WHERE \"from\" = 'preset_id'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(fk, ("presets".to_owned(), "preset_id".to_owned(), "SET NULL".to_owned()));

        // 幂等：重复迁移返回空。
        let again = migrate_with(&mut connection, EMBEDDED_MIGRATIONS).unwrap();
        assert!(again.is_empty());
    }

    // 新库路径：一次性迁移到最新（含 0007；M7-07 起为 0008）。
    let fresh = dir.path().join("fresh.db");
    {
        let mut connection = Connection::open(&fresh).unwrap();
        let applied = migrate(&mut connection).unwrap();
        assert_eq!(applied.last().map(|item| item.version), Some(8));
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM presets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }
}

/// DoD2：预设读写路径（清单顺序：内置在前、创建时间升序；`preset` 按 id 读取；
/// 更新保持 `is_builtin`/`created_at`；删除影响 0/1 行）。
#[tokio::test(flavor = "multi_thread")]
async fn preset_commands_round_trip_and_list_order() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&dir.path().join("aether.db"));
    let reads = runtime.reads().clone();
    let write = runtime.queue().clone();

    // 播种 3 条内置 + 1 条自定义（created_at 更大 → 排在自定义组首位）。
    write
        .execute(StoreCommand::InsertPreset {
            preset: custom_preset("01J00000000000000000000C01", "自定义", "自定义提示词"),
        })
        .await
        .unwrap();
    let presets = reads.presets().await.unwrap();
    assert_eq!(presets.len(), 4);
    assert_eq!(
        presets
            .iter()
            .map(|preset| preset.name.as_str())
            .collect::<Vec<_>>(),
        vec!["默认助手", "代码专家", "代码审查", "自定义"],
        "内置在前、创建时间升序"
    );
    assert!(presets[..3].iter().all(|preset| preset.is_builtin));
    assert!(!presets[3].is_builtin);

    // 按 id 读取。
    let id = PresetId::new("01J00000000000000000000C01").unwrap();
    let found = reads.preset(&id).await.unwrap().unwrap();
    assert_eq!(found.name, "自定义");
    assert!(reads
        .preset(&PresetId::new("01J00000000000000000000ZZZZ").unwrap())
        .await
        .unwrap()
        .is_none());

    // 更新：名称/提示词/updated_at 变化，is_builtin 与 created_at 不变。
    let updated = Preset {
        name: "自定义 v2".to_owned(),
        system_prompt: "新提示词".to_owned(),
        updated_at: 99,
        ..found.clone()
    };
    let outcome = write
        .execute(StoreCommand::UpdatePreset {
            preset: updated.clone(),
        })
        .await
        .unwrap();
    assert_eq!(outcome, aether_store::StoreOutcome::Applied { affected: 1 });
    let reloaded = reads.preset(&id).await.unwrap().unwrap();
    assert_eq!(reloaded.name, "自定义 v2");
    assert_eq!(reloaded.system_prompt, "新提示词");
    assert_eq!(reloaded.created_at, found.created_at);
    assert!(!reloaded.is_builtin);

    // 删除：影响 1 行；再次删除影响 0 行（存在性由命令层先行判定）。
    let outcome = write
        .execute(StoreCommand::DeletePreset { id: id.clone() })
        .await
        .unwrap();
    assert_eq!(outcome, aether_store::StoreOutcome::Applied { affected: 1 });
    let outcome = write
        .execute(StoreCommand::DeletePreset { id })
        .await
        .unwrap();
    assert_eq!(outcome, aether_store::StoreOutcome::Applied { affected: 0 });

    runtime.shutdown().await.unwrap();
}

/// DoD3/DoD4：`sessions.preset_id` 随会话行落库/回读；删除预设后既有会话
/// `preset_id` 置空（`ON DELETE SET NULL`），提示词文本保持原样。
#[tokio::test(flavor = "multi_thread")]
async fn session_preset_reference_is_persisted_and_set_null_on_delete() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&dir.path().join("aether.db"));
    let reads = runtime.reads().clone();
    let write = runtime.queue().clone();
    ensure_runtime(&reads, &write).await;

    let preset = custom_preset("01J00000000000000000000C02", "会话预设", "预设提示词");
    write
        .execute(StoreCommand::InsertPreset {
            preset: preset.clone(),
        })
        .await
        .unwrap();

    let session_id = "01J00000000000000000000S01";
    let mut session = session_row(session_id, Some(preset.id.as_str()));
    session.system_prompt = Some(format!(
        "<!-- aether:preset:{} -->\n预设提示词",
        preset.id.as_str()
    ));
    write
        .execute(StoreCommand::InsertSession {
            session: session.clone(),
        })
        .await
        .unwrap();

    let stored = reads
        .session(&SessionId::new(session_id).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.preset_id.as_ref().map(PresetId::as_str),
        Some(preset.id.as_str()),
        "preset_id 随会话行持久化"
    );

    // 删除预设 → 会话 preset_id 置空（提示词文本保持原样）。
    write
        .execute(StoreCommand::DeletePreset {
            id: preset.id.clone(),
        })
        .await
        .unwrap();
    let stored = reads
        .session(&SessionId::new(session_id).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.preset_id, None, "ON DELETE SET NULL");
    assert_eq!(
        stored.system_prompt.as_deref(),
        session.system_prompt.as_deref(),
        "既有会话提示词保持原样"
    );

    // 未选预设的会话：preset_id 为 NULL（与 P0 行为一致）。
    write
        .execute(StoreCommand::InsertSession {
            session: session_row("01J00000000000000000000S02", None),
        })
        .await
        .unwrap();
    let stored = reads
        .session(&SessionId::new("01J00000000000000000000S02").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.preset_id, None);

    runtime.shutdown().await.unwrap();
}
