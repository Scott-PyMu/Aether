//! M3-11 存储层集成：`providers` / `provider_models`（ADR-010 决策 3；D3 单写者）。
//!
//! 覆盖实施计划 M3-11 DoD1 的存储侧：
//! - 迁移 0003 建表约束：`UNIQUE(provider_id, model_id)`、`ON DELETE CASCADE`、
//!   列与默认值、内置 4 条播种（固定 ULID / `enabled=0` / `is_builtin=1` / 无密钥无模型）；
//! - 写路径经单写队列（`WriteQueue::execute`，D3）：增/改/删/启停/模型增改；
//! - 重复 `(provider_id, model_id)` → `StoreError::DuplicateProviderModel`；
//! - 删除供应商级联删除模型；读取排序（`created_at` 升序）与跨重启保留。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use aether_store::{
    ProviderModelRecord, ProviderRecord, Store, StoreCommand, StoreError, StoreOutcome,
    StoreRuntime, WriteQueueConfig,
};

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

fn provider(id: &str, created_at: i64) -> ProviderRecord {
    ProviderRecord {
        id: id.to_owned(),
        name: format!("Provider {id}"),
        provider_type: "custom".to_owned(),
        base_url: Some("https://api.example.com".to_owned()),
        api_key_ref: Some(format!("keychain://aether/provider/{id}")),
        enabled: true,
        is_builtin: false,
        created_at,
        updated_at: created_at,
    }
}

fn model(id: &str, provider_id: &str, model_id: &str, created_at: i64) -> ProviderModelRecord {
    ProviderModelRecord {
        id: id.to_owned(),
        provider_id: provider_id.to_owned(),
        model_id: model_id.to_owned(),
        display_name: model_id.to_owned(),
        enabled: true,
        created_at,
    }
}

fn columns(
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

/// DoD1：迁移 0003 `providers` / `provider_models` 表/约束断言 + 内置 4 条播种。
#[test]
fn dod1_provider_tables_match_adr_010_and_seed_builtins() {
    let (_dir, store) = common::open_temp_store("m3-11-providers-schema");
    let connection = store.connection();

    assert_eq!(
        columns(connection, "providers"),
        vec![
            ("id".to_owned(), "TEXT".to_owned(), 0, None),
            ("name".to_owned(), "TEXT".to_owned(), 1, None),
            ("type".to_owned(), "TEXT".to_owned(), 1, None),
            ("base_url".to_owned(), "TEXT".to_owned(), 0, None),
            ("api_key_ref".to_owned(), "TEXT".to_owned(), 0, None),
            (
                "enabled".to_owned(),
                "INTEGER".to_owned(),
                1,
                Some("1".to_owned())
            ),
            (
                "is_builtin".to_owned(),
                "INTEGER".to_owned(),
                1,
                Some("0".to_owned())
            ),
            ("created_at".to_owned(), "INTEGER".to_owned(), 1, None),
            ("updated_at".to_owned(), "INTEGER".to_owned(), 1, None),
        ],
        "providers 列定义必须与 ADR-010 附录 A 一致"
    );
    assert_eq!(
        columns(connection, "provider_models"),
        vec![
            ("id".to_owned(), "TEXT".to_owned(), 0, None),
            ("provider_id".to_owned(), "TEXT".to_owned(), 1, None),
            ("model_id".to_owned(), "TEXT".to_owned(), 1, None),
            ("display_name".to_owned(), "TEXT".to_owned(), 1, None),
            (
                "enabled".to_owned(),
                "INTEGER".to_owned(),
                1,
                Some("1".to_owned())
            ),
            ("created_at".to_owned(), "INTEGER".to_owned(), 1, None),
        ],
        "provider_models 列定义必须与 ADR-010 附录 A 一致"
    );

    // UNIQUE(provider_id, model_id)：表级约束（自动索引）。
    let index_names: Vec<String> = {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' \
                 AND tbl_name = 'provider_models'",
            )
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let unique_columns: Vec<Vec<String>> = index_names
        .iter()
        .map(|name| {
            let mut statement = connection
                .prepare(&format!("PRAGMA index_info('{name}')"))
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(2))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        })
        .collect();
    assert!(
        unique_columns.contains(&vec!["provider_id".to_owned(), "model_id".to_owned()]),
        "必须存在 UNIQUE(provider_id, model_id)：{index_names:?}"
    );

    // 外键：provider_models.provider_id → providers(id) ON DELETE CASCADE。
    let fks: Vec<(String, String, String, String)> = {
        let mut statement = connection
            .prepare("PRAGMA foreign_key_list(provider_models)")
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        fks,
        vec![(
            "provider_id".to_owned(),
            "providers".to_owned(),
            "id".to_owned(),
            "CASCADE".to_owned(),
        )],
        "provider_models.provider_id 必须 ON DELETE CASCADE"
    );

    // 内置播种：4 条固定 ULID，`enabled=0`、`is_builtin=1`、无密钥、无模型。
    type SeededRow = (String, String, String, String, Option<String>, i64, i64);
    let seeded: Vec<SeededRow> = {
        let mut statement = connection
            .prepare(
                "SELECT id, name, type, base_url, api_key_ref, enabled, is_builtin \
                 FROM providers ORDER BY created_at ASC, rowid ASC",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(seeded.len(), 4, "内置供应商播种必须恰为 4 条");
    let expected: Vec<(&str, &str, &str, &str)> = vec![
        (
            "01J00000000000000000000B01",
            "Anthropic",
            "anthropic",
            "https://api.anthropic.com",
        ),
        (
            "01J00000000000000000000B02",
            "OpenAI",
            "openai",
            "https://api.openai.com",
        ),
        (
            "01J00000000000000000000B03",
            "DeepSeek",
            "deepseek",
            "https://api.deepseek.com",
        ),
        (
            "01J00000000000000000000B04",
            "Google Gemini",
            "google",
            "https://generativelanguage.googleapis.com",
        ),
    ];
    for (row, want) in seeded.iter().zip(expected.iter()) {
        assert_eq!(row.0, want.0, "固定 ULID");
        assert_eq!(row.1, want.1);
        assert_eq!(row.2, want.2);
        assert_eq!(row.3, want.3);
        assert_eq!(row.4, None, "内置供应商无密钥引用");
        assert_eq!(row.5, 0, "内置供应商默认停用（enabled=0）");
        assert_eq!(row.6, 1, "内置标记 is_builtin=1");
    }
    let model_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM provider_models", [], |row| row.get(0))
        .unwrap();
    assert_eq!(model_count, 0, "内置供应商播种不含模型");

    let schema_version: i64 = connection
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(schema_version, 3, "迁移必须到 0003（ADR-010）");
}

/// DoD1：写路径经单写队列（D3）；增/改/启停/删除级联/重复模型拒绝。
#[test]
fn dod1_provider_writes_go_through_single_write_queue() {
    let dir = common::temp_dir("m3-11-providers-write");
    let handle = new_runtime();
    let runtime = StoreRuntime::open(
        common::db_path(&dir),
        WriteQueueConfig::default(),
        handle.handle(),
    )
    .unwrap();
    let queue = runtime.queue().clone();
    let reads = runtime.reads().clone();

    let committed_before = queue.metrics().committed_entries;
    let first = "01J00000000000000000000P01";
    let second = "01J00000000000000000000P02";

    // 新增两条（乱序 created_at，验证排序）。
    handle
        .block_on(queue.execute(StoreCommand::InsertProvider {
            provider: provider(second, 2),
        }))
        .unwrap();
    handle
        .block_on(queue.execute(StoreCommand::InsertProvider {
            provider: provider(first, 1),
        }))
        .unwrap();
    // 更新（名称/启用态/引用）。
    let mut updated = provider(first, 1);
    updated.name = "Renamed".to_owned();
    updated.enabled = false;
    updated.api_key_ref = None;
    updated.updated_at = 5;
    assert_eq!(
        handle
            .block_on(queue.execute(StoreCommand::UpdateProvider {
                provider: updated.clone(),
            }))
            .unwrap(),
        StoreOutcome::Applied { affected: 1 }
    );
    // 启停。
    handle
        .block_on(queue.execute(StoreCommand::SetProviderEnabled {
            id: second.to_owned(),
            enabled: false,
            updated_at: 6,
        }))
        .unwrap();

    // 模型：新增 / 重复 / 启停。
    let model_id = "01J00000000000000000000M01";
    handle
        .block_on(queue.execute(StoreCommand::InsertProviderModel {
            model: model(model_id, first, "deepseek-v4-pro", 7),
        }))
        .unwrap();
    let duplicate = handle
        .block_on(queue.execute(StoreCommand::InsertProviderModel {
            model: model("01J00000000000000000000M02", first, "deepseek-v4-pro", 8),
        }))
        .unwrap_err();
    match duplicate {
        StoreError::DuplicateProviderModel {
            provider_id,
            model_id,
        } => {
            assert_eq!(provider_id, first);
            assert_eq!(model_id, "deepseek-v4-pro");
        }
        other => panic!("期望 DuplicateProviderModel，实际 {other:?}"),
    }
    assert_eq!(
        handle
            .block_on(queue.execute(StoreCommand::SetProviderModelEnabled {
                provider_id: first.to_owned(),
                model_id: "deepseek-v4-pro".to_owned(),
                enabled: false,
            }))
            .unwrap(),
        StoreOutcome::Applied { affected: 1 }
    );

    assert_eq!(
        queue.metrics().committed_entries,
        committed_before + 6,
        "供应商/模型写路径必须逐条经单写队列（D3；重复插入失败不提交）"
    );

    // 读取：排序（created_at 升序）+ 形状（内置 4 条播种的 created_at = 迁移执行时刻，
    // 晚于夹具时间戳，故自定义供应商在前）。
    let providers = handle.block_on(reads.providers()).unwrap();
    assert_eq!(providers.len(), 6, "4 条内置播种 + 2 条自定义");
    let custom: Vec<&str> = providers
        .iter()
        .filter(|provider| !provider.is_builtin)
        .map(|provider| provider.id.as_str())
        .collect();
    assert_eq!(custom, vec![first, second], "按 created_at 升序");
    assert_eq!(providers[0].name, "Renamed");
    assert!(!providers[0].enabled);
    assert_eq!(providers[0].api_key_ref, None);
    assert!(!providers[1].enabled);
    assert_eq!(
        handle.block_on(reads.provider(first)).unwrap().unwrap().id,
        first
    );
    let models = handle.block_on(reads.provider_models()).unwrap();
    assert_eq!(models.len(), 1);
    assert!(!models[0].enabled, "模型启停已生效");
    assert!(handle
        .block_on(reads.provider_model(first, "deepseek-v4-pro"))
        .unwrap()
        .is_some());
    assert!(handle
        .block_on(reads.provider_model(first, "absent"))
        .unwrap()
        .is_none());

    // 删除级联：删除 first 后其模型一并删除（ON DELETE CASCADE）。
    assert_eq!(
        handle
            .block_on(queue.execute(StoreCommand::DeleteProvider {
                id: first.to_owned(),
            }))
            .unwrap(),
        StoreOutcome::Applied { affected: 1 }
    );
    assert_eq!(handle.block_on(reads.provider_models()).unwrap().len(), 0);
    assert_eq!(handle.block_on(reads.providers()).unwrap().len(), 5);
    assert_eq!(
        queue.metrics().committed_entries,
        committed_before + 7,
        "删除同样经单写队列"
    );

    // 删除不存在：影响 0 行（命令层先行判定不存在）。
    assert_eq!(
        handle
            .block_on(queue.execute(StoreCommand::DeleteProvider {
                id: "01J00000000000000000000PZZ".to_owned(),
            }))
            .unwrap(),
        StoreOutcome::Applied { affected: 0 }
    );

    // 跨重启保留。
    handle.block_on(runtime.shutdown()).unwrap();
    let reopened = Store::open(common::db_path(&dir)).unwrap();
    let rows: i64 = reopened
        .connection()
        .query_row("SELECT COUNT(*) FROM providers", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 5, "4 条内置播种 + 1 条保留（跨重启）");
}
