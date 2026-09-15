//! 存储门面（M1-03）：打开 + PRAGMA 全集 + 迁移 + 启动 `quick_check` + 安全模式。
//!
//! 启动顺序（D3）：
//! 1. 读写打开 + PRAGMA 全集（[`crate::pragma::apply`]）；
//! 2. `PRAGMA quick_check`（快速完整性检查）；
//! 3. 通过 → 迁移 0 → 最新（单版本单事务）；失败 → 只读安全模式（拒绝写入，
//!    保留备份/导出入口，D3「库损坏」失败场景）。
//!
//! M1-04 将在此之上接入 1 写连接 + 4 读连接的队列与 group commit。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, OpenFlags};

use crate::error::StoreError;
use crate::migration::{self, AppliedMigration};
use crate::pragma::{self, PragmaSnapshot};

/// 存储工作模式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreMode {
    /// 读写（PRAGMA 与迁移均已就绪）。
    ReadWrite,
    /// 安全模式（D3）：`quick_check` 失败，只读连接，拒绝写入。
    SafeMode { reason: String },
}

/// `quick_check` 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityReport {
    pub ok: bool,
    pub messages: Vec<String>,
}

impl IntegrityReport {
    /// 诊断用单行摘要。
    pub fn summary(&self) -> String {
        if self.ok {
            "ok".to_owned()
        } else if self.messages.is_empty() {
            "quick_check 失败（无详细信息）".to_owned()
        } else {
            self.messages.join("; ")
        }
    }
}

/// 单表导出结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableExport {
    pub table: String,
    /// 已成功导出的行数。
    pub rows: u64,
    /// 该表读取失败时的错误（损坏页等）；失败后继续导出其余表。
    pub error: Option<String>,
}

/// 可读数据导出报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    pub path: PathBuf,
    pub tables: Vec<TableExport>,
    /// 成功导出的总行数。
    pub rows_written: u64,
}

/// 单文件数据库门面（设计 D3）。
pub struct Store {
    conn: Connection,
    mode: StoreMode,
    path: PathBuf,
}

impl Store {
    /// 打开（或创建）数据库：PRAGMA 全集 → `quick_check` → 迁移。
    ///
    /// - `quick_check` 失败：关闭写连接，改以只读连接进入安全模式（[`StoreMode::SafeMode`]）；
    /// - 迁移记录与内嵌文件 checksum 不一致：返回错误，拒绝启动（D3）；
    /// - 读写与只读均无法打开：返回 [`StoreError::SafeModeUnavailable`]。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }

        let mut conn = match Self::open_read_write(&path) {
            Ok(conn) => conn,
            Err(error) => {
                return Self::enter_safe_mode(&path, format!("打开写连接失败: {error}"));
            }
        };

        let report = quick_check(&conn);
        if !report.ok {
            drop(conn);
            let reason = format!("quick_check 失败: {}", report.summary());
            return Self::enter_safe_mode(&path, reason);
        }

        migration::migrate(&mut conn)?;
        Ok(Self {
            conn,
            mode: StoreMode::ReadWrite,
            path,
        })
    }

    fn open_read_write(path: &Path) -> Result<Connection, StoreError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        pragma::apply(&conn)?;
        Ok(conn)
    }

    fn open_read_only(path: &Path) -> Result<Connection, StoreError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        pragma::apply_read_only(&conn)?;
        Ok(conn)
    }

    fn enter_safe_mode(path: &Path, reason: String) -> Result<Self, StoreError> {
        match Self::open_read_only(path) {
            Ok(conn) => Ok(Self {
                conn,
                mode: StoreMode::SafeMode { reason },
                path: path.to_path_buf(),
            }),
            Err(error) => Err(StoreError::SafeModeUnavailable {
                path: path.to_path_buf(),
                reason: format!("{reason}；只读打开失败: {error}"),
            }),
        }
    }

    /// 当前工作模式。
    pub fn mode(&self) -> &StoreMode {
        &self.mode
    }

    /// 是否处于安全模式（只读）。
    pub fn is_safe_mode(&self) -> bool {
        matches!(self.mode, StoreMode::SafeMode { .. })
    }

    /// 安全模式原因（非安全模式返回 `None`）。
    pub fn safe_mode_reason(&self) -> Option<&str> {
        match &self.mode {
            StoreMode::SafeMode { reason } => Some(reason),
            StoreMode::ReadWrite => None,
        }
    }

    /// 数据库文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 底层连接（迁移已执行；读者可用，写者须经 [`Store::execute_write`] 门禁）。
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// 重新执行 `PRAGMA quick_check`。
    pub fn quick_check(&self) -> IntegrityReport {
        quick_check(&self.conn)
    }

    /// 当前连接的 PRAGMA 现值（诊断断言用）。
    pub fn pragma_snapshot(&self) -> Result<PragmaSnapshot, StoreError> {
        pragma::snapshot(&self.conn)
    }

    /// 已应用的迁移记录。
    pub fn applied_migrations(&self) -> Result<Vec<AppliedMigration>, StoreError> {
        migration::applied_migrations(&self.conn)
    }

    /// 写入门禁：安全模式下返回 [`StoreError::SafeModeWriteRefused`]，拒绝一切写入。
    ///
    /// M1-04 将以此为边界接入单写队列；M1-03 仅提供最小 gated 入口。
    pub fn execute_write(&self, sql: &str) -> Result<usize, StoreError> {
        match &self.mode {
            StoreMode::SafeMode { reason } => Err(StoreError::SafeModeWriteRefused {
                reason: reason.clone(),
            }),
            StoreMode::ReadWrite => Ok(self.conn.execute(sql, [])?),
        }
    }

    /// `VACUUM INTO`（D13 备份）：产物为已 checkpoint 的单文件（不含 `-wal`/`-shm`）。
    ///
    /// 安全模式下同样可用（备份/导出入口）；目标文件必须不存在。
    pub fn backup_to(&self, dest: impl AsRef<Path>) -> Result<u64, StoreError> {
        let dest = dest.as_ref();
        if dest.exists() {
            return Err(StoreError::BackupTargetExists {
                path: dest.to_path_buf(),
            });
        }
        let dest_str = dest.to_str().ok_or_else(|| StoreError::NonUtf8Path {
            path: dest.to_path_buf(),
        })?;
        self.conn.execute("VACUUM INTO ?1", params![dest_str])?;
        Ok(fs::metadata(dest)?.len())
    }

    /// 可读数据导出（安全模式入口，D3「可读数据导出」）：逐表导出 JSONL。
    ///
    /// 单表读取因损坏失败时记录错误并继续导出其余表。
    pub fn export_readable(&self, dest: impl AsRef<Path>) -> Result<ExportReport, StoreError> {
        let dest = dest.as_ref().to_path_buf();
        let mut writer = std::io::BufWriter::new(fs::File::create(&dest)?);
        let mut report = ExportReport {
            path: dest,
            tables: Vec::new(),
            rows_written: 0,
        };

        for table in list_tables(&self.conn)? {
            match export_table(&self.conn, &table, &mut writer) {
                Ok(rows) => {
                    report.rows_written += rows;
                    report.tables.push(TableExport {
                        table,
                        rows,
                        error: None,
                    });
                }
                Err(error) => {
                    report.rows_written += error.rows;
                    report.tables.push(TableExport {
                        table,
                        rows: error.rows,
                        error: Some(error.message),
                    });
                }
            }
        }
        writer.flush()?;
        Ok(report)
    }
}

/// `PRAGMA quick_check`（D3 启动快速完整性检查）；不返回 `Err`，失败即 `ok = false`。
pub fn quick_check(conn: &Connection) -> IntegrityReport {
    let mut messages = Vec::new();
    match conn.prepare("PRAGMA quick_check") {
        Ok(mut statement) => match statement.query_map([], |row| row.get::<_, String>(0)) {
            Ok(rows) => {
                for row in rows {
                    match row {
                        Ok(message) => messages.push(message),
                        Err(error) => {
                            return IntegrityReport {
                                ok: false,
                                messages: vec![error.to_string()],
                            };
                        }
                    }
                }
            }
            Err(error) => {
                return IntegrityReport {
                    ok: false,
                    messages: vec![error.to_string()],
                };
            }
        },
        Err(error) => {
            return IntegrityReport {
                ok: false,
                messages: vec![error.to_string()],
            };
        }
    }

    let ok = messages.len() == 1 && messages[0].eq_ignore_ascii_case("ok");
    if ok {
        messages.clear();
    }
    IntegrityReport { ok, messages }
}

fn list_tables(conn: &Connection) -> Result<Vec<String>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut tables = Vec::new();
    for row in rows {
        tables.push(row?);
    }
    Ok(tables)
}

struct TableExportError {
    rows: u64,
    message: String,
}

fn export_table(
    conn: &Connection,
    table: &str,
    writer: &mut impl Write,
) -> Result<u64, TableExportError> {
    let sql = format!("SELECT * FROM {}", quote_identifier(table));
    let mut statement = conn.prepare(&sql).map_err(|error| TableExportError {
        rows: 0,
        message: error.to_string(),
    })?;
    let columns: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut rows = statement.query([]).map_err(|error| TableExportError {
        rows: 0,
        message: error.to_string(),
    })?;

    let mut count = 0u64;
    loop {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(error) => {
                return Err(TableExportError {
                    rows: count,
                    message: error.to_string(),
                });
            }
        };
        let mut object = serde_json::Map::with_capacity(columns.len());
        for (index, column) in columns.iter().enumerate() {
            let value = row.get_ref(index).map_err(|error| TableExportError {
                rows: count,
                message: error.to_string(),
            })?;
            object.insert(column.clone(), sqlite_value_to_json(value));
        }
        let line = serde_json::json!({ "table": table, "row": object }).to_string();
        writeln!(writer, "{line}").map_err(|error| TableExportError {
            rows: count,
            message: error.to_string(),
        })?;
        count += 1;
    }
    Ok(count)
}

fn sqlite_value_to_json(value: ValueRef<'_>) -> serde_json::Value {
    match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(integer) => serde_json::Value::from(integer),
        ValueRef::Real(real) => serde_json::Value::from(real),
        ValueRef::Text(text) => serde_json::Value::from(String::from_utf8_lossy(text).into_owned()),
        // 库内无 BLOB 列；通用导出用十六进制字符串表示，便于恢复核对。
        ValueRef::Blob(blob) => serde_json::Value::from(hex::encode(blob)),
    }
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
