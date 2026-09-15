//! 迁移框架（设计 D3 / 评审修订 #2）：`schema_migrations(version, checksum, applied_at)`。
//!
//! - **唯一版本机制**：不使用 `PRAGMA user_version`；
//! - 迁移文件经 `include_bytes!` 内嵌，checksum 为文件字节的 sha256；
//! - 单版本单事务（`BEGIN`/`COMMIT` 由框架包裹，迁移文件内禁止出现事务控制）；
//! - 启动时已应用版本的 checksum 与内嵌文件不一致 → 拒绝启动（防历史被改）。

use std::collections::BTreeMap;

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

use crate::error::StoreError;

/// 迁移版本表名（唯一版本机制；禁止使用 `PRAGMA user_version`）。
pub const MIGRATIONS_TABLE: &str = "schema_migrations";

/// 内嵌迁移文件（`crates/aether-store/src/migration.rs` → 仓库 `migrations/`）。
#[derive(Debug, Clone, Copy)]
pub struct MigrationFile {
    pub version: i64,
    pub name: &'static str,
    /// 迁移文件原始字节；sha256 即校验口径。
    pub bytes: &'static [u8],
}

/// 全部已发布迁移（按版本升序）。
pub const EMBEDDED_MIGRATIONS: &[MigrationFile] = &[MigrationFile {
    version: 1,
    name: "0001_init.sql",
    bytes: include_bytes!("../../../migrations/0001_init.sql"),
}];

/// 迁移文件 sha256（小写十六进制）。
pub fn checksum(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// 迁移记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedMigration {
    pub version: i64,
    pub checksum: String,
    pub applied_at: i64,
}

/// 应用内嵌迁移清单（0 → 最新）。
///
/// 返回本次实际应用的迁移（幂等：重复调用返回空列表）。
pub fn migrate(conn: &mut Connection) -> Result<Vec<AppliedMigration>, StoreError> {
    migrate_with(conn, EMBEDDED_MIGRATIONS)
}

/// 以指定迁移清单执行迁移（测试用于注入「被篡改的文件」）。
///
/// 流程：读 `schema_migrations` → 校验已应用版本 checksum → 逐版本单事务执行 → 写版本记录。
pub fn migrate_with(
    conn: &mut Connection,
    migrations: &[MigrationFile],
) -> Result<Vec<AppliedMigration>, StoreError> {
    validate(migrations)?;
    let program_max = migrations.last().map_or(0, |item| item.version);
    let recorded = read_recorded(conn)?;

    for (version, row) in &recorded {
        let Some(file) = migrations.iter().find(|item| item.version == *version) else {
            return Err(StoreError::SchemaNewerThanProgram {
                database: *version,
                program: program_max,
            });
        };
        let recomputed = checksum(file.bytes);
        if recomputed != row.checksum {
            return Err(StoreError::MigrationChecksumMismatch {
                version: *version,
                name: file.name,
                recorded: row.checksum.clone(),
                recomputed,
            });
        }
    }

    let mut applied = Vec::new();
    for file in migrations {
        if recorded.contains_key(&file.version) {
            continue;
        }
        let sql = std::str::from_utf8(file.bytes)
            .map_err(|_| StoreError::InvalidMigrationEncoding { name: file.name })?;
        let digest = checksum(file.bytes);
        let applied_at = now_ms();

        let transaction = conn.transaction()?;
        transaction.execute_batch(sql)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (?1, ?2, ?3)",
            params![file.version, digest, applied_at],
        )?;
        transaction.commit()?;

        applied.push(AppliedMigration {
            version: file.version,
            checksum: digest,
            applied_at,
        });
    }
    Ok(applied)
}

/// 已应用的迁移记录（按版本升序；`schema_migrations` 不存在时为空）。
pub fn applied_migrations(conn: &Connection) -> Result<Vec<AppliedMigration>, StoreError> {
    Ok(read_recorded(conn)?.into_values().collect())
}

fn validate(migrations: &[MigrationFile]) -> Result<(), StoreError> {
    if migrations.is_empty() {
        return Err(StoreError::InvalidMigrationSet {
            reason: "迁移清单为空".to_owned(),
        });
    }
    let mut previous = 0;
    for migration in migrations {
        if migration.version <= previous {
            return Err(StoreError::InvalidMigrationSet {
                reason: format!(
                    "迁移版本必须严格升序：v{} 出现在 v{previous} 之后",
                    migration.version
                ),
            });
        }
        if migration.name.is_empty() {
            return Err(StoreError::InvalidMigrationSet {
                reason: format!("迁移 v{} 缺少文件名", migration.version),
            });
        }
        previous = migration.version;
    }
    Ok(())
}

fn read_recorded(conn: &Connection) -> Result<BTreeMap<i64, AppliedMigration>, StoreError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [MIGRATIONS_TABLE],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(BTreeMap::new());
    }

    let mut statement = conn
        .prepare("SELECT version, checksum, applied_at FROM schema_migrations ORDER BY version")?;
    let rows = statement.query_map([], |row| {
        Ok(AppliedMigration {
            version: row.get(0)?,
            checksum: row.get(1)?,
            applied_at: row.get(2)?,
        })
    })?;
    let mut recorded = BTreeMap::new();
    for row in rows {
        let row = row?;
        recorded.insert(row.version, row);
    }
    Ok(recorded)
}

fn now_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}
