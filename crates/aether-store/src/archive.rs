//! 归档导出与事件保留设施（M5-05 / M5-04；设计 D4/D13、ADR-021）。
//!
//! 范围（ADR-021 附录 A-2 归档 JSONL 格式；自动归档与导出共用）：
//! - 会话导出（`source=session-export`）：`sessions` 行 + 该会话 `messages` 终稿
//!   （按 `seq` 升序）+ 该会话 `events`（按 `seq` 升序）；
//! - 全库归档（`source=archive-export`）：`sessions` + `messages` + `events` 全量
//!   + `runs` 摘要；
//! - 保留清理归档（`source=auto-retention`）：待删事件批（白名单 `message.delta`/
//!   `usage`/`log`），**先归档后删除**（删除由调用方经单写队列执行）。
//!
//! 写出纪律：
//! - manifest 为首行（`v/kind/created_at/source/session_id/rows/sha256`；`sha256` =
//!   后续各行拼接内容（行尾 `\n` 归一）的 sha256；`rows` = 后续行数）；
//! - 流式写出（1MiB 有界缓冲，不整库入内存）：payload 暂存 → 回填 manifest →
//!   `sync_all` → 原子 rename；中断不留半成品（临时文件 + rename），历史残留由
//!   [`cleanup_archive_temps`] 清理；
//! - 脱敏由调用方注入 [`ArchiveSanitizer`]（aether-store 不依赖 aether-security；
//!   依赖方向 AGENTS §2.1），逐行执行且必须保证「脱敏后 0 命中」。
//!
//! 本模块不打开写连接（读取走调用方提供的只读连接；删除由调用方经单写队列）。

use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::StoreError;

/// 归档格式版本（manifest `v`；ADR-021 附录 A-2）。
pub const ARCHIVE_FORMAT_VERSION: u32 = 1;
/// manifest `kind` 常量。
pub const ARCHIVE_MANIFEST_KIND: &str = "manifest";
/// 导出来源：单会话（`session_export`）。
pub const ARCHIVE_SOURCE_SESSION_EXPORT: &str = "session-export";
/// 导出来源：全库归档（`archive_export`）。
pub const ARCHIVE_SOURCE_ARCHIVE_EXPORT: &str = "archive-export";
/// 导出来源：保留清理自动归档（M5-04）。
pub const ARCHIVE_SOURCE_AUTO_RETENTION: &str = "auto-retention";
/// payload 暂存文件标记（`<dest>.archive-partial-<ts>`；中断残留清理依据）。
pub const ARCHIVE_PARTIAL_INFIX: &str = ".archive-partial-";
/// 最终产物暂存文件标记（`<dest>.archive-tmp-<ts>`；与产物同目录保证原子 rename）。
pub const ARCHIVE_TEMP_INFIX: &str = ".archive-tmp-";
/// 归档流式缓冲（1MiB 有界；M3-04 先例）。
pub const ARCHIVE_BUFFER_BYTES: usize = 1024 * 1024;
/// 可清理事件类型白名单（ADR-021 §2.1-1；其余事件类型永不清理）。
pub const RETENTION_EVENT_TYPES: [&str; 3] = ["message.delta", "usage", "log"];
/// 单批清理上限（ADR-021 §2.1-5：批大小 ≤5000 行/事务）。
pub const RETENTION_BATCH_MAX: usize = 5_000;

/// 行脱敏 + 0 命中守门（回调由上层注入；逐行调用）。
///
/// 入参为原始行对象；返回**已脱敏且经 0 命中校验**的紧凑 JSON 字符串。
/// 任何失败（含脱敏后仍命中）返回 `Err`，整次归档失败（不落半成品。
pub type ArchiveSanitizer<'a> = &'a dyn Fn(&Value) -> Result<String, StoreError>;

/// 归档 manifest（首行）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveManifest {
    /// 格式版本（恒为 [`ARCHIVE_FORMAT_VERSION`]）。
    pub v: u32,
    /// 恒为 [`ARCHIVE_MANIFEST_KIND`]。
    pub kind: String,
    /// 归档创建时间（Unix epoch 毫秒）。
    pub created_at: i64,
    /// 来源（`session-export` / `archive-export` / `auto-retention`）。
    pub source: String,
    /// 会话 id（全库/保留归档为 `null`）。
    pub session_id: Option<String>,
    /// payload 行数（不含 manifest）。
    pub rows: u64,
    /// payload 各行拼接内容（行尾 `\n` 归一）的 sha256（十六进制小写）。
    pub sha256: String,
}

/// 归档行计数（按 `kind`；证据/断言用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveCounts {
    pub sessions: u64,
    pub messages: u64,
    pub events: u64,
    pub runs: u64,
}

/// 归档产物报告。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveReport {
    pub path: PathBuf,
    pub file_name: String,
    pub bytes: u64,
    pub rows: u64,
    pub sha256: String,
    pub source: String,
    pub session_id: Option<String>,
    pub created_at: i64,
    pub counts: ArchiveCounts,
}

/// 归档校验结果（[`verify_archive_file`]）。
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveVerification {
    pub manifest: ArchiveManifest,
    /// 实际 payload 行数。
    pub rows: u64,
    /// 实际 payload sha256（十六进制小写）。
    pub sha256: String,
    /// 实际值是否与 manifest 一致（行数与哈希逐项）。
    pub matches: bool,
}

/// 保留清理候选事件行（`rowid` 为删除精确定位；仅白名单类型、超窗）。
#[derive(Debug, Clone, PartialEq)]
pub struct RetentionEventRow {
    pub rowid: i64,
    pub id: String,
    pub session_id: String,
    pub run_id: Option<String>,
    pub runtime_id: String,
    pub seq: i64,
    pub event_type: String,
    pub payload: Value,
    pub ts: i64,
    pub v: i64,
}

impl RetentionEventRow {
    /// 归档事件行（ADR-021 附录 A-2；信封 9 字段 + `kind`）。
    pub fn to_archive_value(&self) -> Value {
        json!({
            "v": self.v,
            "kind": "event",
            "id": self.id,
            "session_id": self.session_id,
            "run_id": self.run_id,
            "runtime_id": self.runtime_id,
            "seq": self.seq,
            "ts": self.ts,
            "type": self.event_type,
            "payload": self.payload,
        })
    }
}

/// payload 暂存写出结果。
struct PayloadDigest {
    rows: u64,
    sha256: String,
    counts: ArchiveCounts,
}

/// payload 流式写出器（逐行脱敏 + 序列化 + 哈希/计数；1MiB 有界缓冲）。
struct PayloadSink {
    path: PathBuf,
    writer: BufWriter<File>,
    hasher: Sha256,
    rows: u64,
    counts: ArchiveCounts,
}

impl PayloadSink {
    fn create(path: &Path) -> Result<Self, StoreError> {
        if path.exists() {
            fs::remove_file(path)?;
        }
        let file = File::create(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            writer: BufWriter::with_capacity(ARCHIVE_BUFFER_BYTES, file),
            hasher: Sha256::new(),
            rows: 0,
            counts: ArchiveCounts::default(),
        })
    }

    /// 写出一行（脱敏 → 紧凑 JSON → `\n`；哈希按行尾 `\n` 归一累加）。
    fn insert(&mut self, value: &Value, sanitize: ArchiveSanitizer<'_>) -> Result<(), StoreError> {
        let line = sanitize(value)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.hasher.update(line.as_bytes());
        self.hasher.update(b"\n");
        self.rows = self.rows.saturating_add(1);
        match value.get("kind").and_then(Value::as_str) {
            Some("session") => self.counts.sessions = self.counts.sessions.saturating_add(1),
            Some("message") => self.counts.messages = self.counts.messages.saturating_add(1),
            Some("event") => self.counts.events = self.counts.events.saturating_add(1),
            Some("run") => self.counts.runs = self.counts.runs.saturating_add(1),
            _ => {}
        }
        Ok(())
    }

    fn finish(mut self) -> Result<PayloadDigest, StoreError> {
        self.writer.flush()?;
        let file = self
            .writer
            .into_inner()
            .map_err(|error| StoreError::Io(error.into_error()))?;
        file.sync_all()?;
        Ok(PayloadDigest {
            rows: self.rows,
            sha256: hex::encode(self.hasher.finalize()),
            counts: self.counts,
        })
    }

    fn abort(self) {
        drop(self.writer);
        let _ = fs::remove_file(&self.path);
    }
}

/// 归档产物文件名（单会话导出：`session-<id>-<ts>.jsonl`；ADR-021 实现要点 3）。
pub fn session_archive_file_name(session_id: &str, created_at_ms: i64) -> String {
    format!("session-{session_id}-{created_at_ms}.jsonl")
}

/// 归档产物文件名（全库导出：`aether-archive-<ts>.jsonl`；ADR-021 实现要点 3）。
pub fn full_archive_file_name(created_at_ms: i64) -> String {
    format!("aether-archive-{created_at_ms}.jsonl")
}

/// 归档产物文件名（保留清理：`events-archive-<yyyymmdd>.jsonl` + 冲突消解）。
pub fn retention_archive_file_name(dir: &Path, created_at_ms: i64) -> String {
    let date = compact_date_utc(created_at_ms);
    let primary = format!("events-archive-{date}.jsonl");
    if !dir.join(&primary).exists() {
        return primary;
    }
    for index in 1u32..=9_999 {
        let candidate = format!("events-archive-{date}-{index}.jsonl");
        if !dir.join(&candidate).exists() {
            return candidate;
        }
    }
    format!("events-archive-{date}-{created_at_ms}.jsonl")
}

/// 目标目录内不冲突的归档路径（同名追加 `-<n>`；防御同毫秒并发）。
pub fn unique_archive_path(dir: &Path, base: &str) -> Result<PathBuf, StoreError> {
    let candidate = dir.join(base);
    if !candidate.exists() {
        return Ok(candidate);
    }
    let stem = base.strip_suffix(".jsonl").unwrap_or(base);
    for index in 1u32..=9_999 {
        let candidate = dir.join(format!("{stem}-{index}.jsonl"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(StoreError::Internal {
        reason: format!("归档目标命名冲突无法消解：{}", candidate.display()),
    })
}

/// payload 暂存路径（`<dest>.archive-partial-<ts>`；产物同目录）。
fn archive_partial_path(dest: &Path, created_at_ms: i64) -> PathBuf {
    suffix_path(dest, &format!("{ARCHIVE_PARTIAL_INFIX}{created_at_ms}"))
}

/// 最终产物暂存路径（`<dest>.archive-tmp-<ts>`；原子 rename 前写入）。
fn archive_temp_path(dest: &Path, created_at_ms: i64) -> PathBuf {
    suffix_path(dest, &format!("{ARCHIVE_TEMP_INFIX}{created_at_ms}"))
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "archive.jsonl".to_owned());
    path.with_file_name(format!("{name}{suffix}"))
}

/// 清理目标目录内归档临时残片（`*.archive-partial-*` / `*.archive-tmp-*`）。
///
/// 崩溃/强杀（kill -9）留下的暂存文件在下一次归档前清理；返回删除数。
pub fn cleanup_archive_temps(dir: &Path) -> usize {
    let Ok(listing) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0usize;
    for entry in listing.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if (name.contains(ARCHIVE_PARTIAL_INFIX) || name.contains(ARCHIVE_TEMP_INFIX))
            && fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// 归档写出骨架：payload 流式暂存 → manifest 回填 → `sync_all` → 原子 rename。
///
/// 任一步失败清理暂存文件，**不产生半成品**；成功路径删除 payload 暂存。
fn write_archive(
    dest: &Path,
    source: &str,
    session_id: Option<&str>,
    created_at_ms: i64,
    write_payload: impl FnOnce(&mut PayloadSink) -> Result<(), StoreError>,
) -> Result<ArchiveReport, StoreError> {
    let partial = archive_partial_path(dest, created_at_ms);
    let mut sink = PayloadSink::create(&partial)?;
    if let Err(error) = write_payload(&mut sink) {
        sink.abort();
        return Err(error);
    }
    let digest = match sink.finish() {
        Ok(digest) => digest,
        Err(error) => {
            let _ = fs::remove_file(&partial);
            return Err(error);
        }
    };

    let manifest = json!({
        "v": ARCHIVE_FORMAT_VERSION,
        "kind": ARCHIVE_MANIFEST_KIND,
        "created_at": created_at_ms,
        "source": source,
        "session_id": session_id,
        "rows": digest.rows,
        "sha256": digest.sha256,
    });
    let result = finalize_archive(dest, &partial, &manifest, created_at_ms);
    let _ = fs::remove_file(&partial);
    result?;
    let bytes = fs::metadata(dest)?.len();
    let file_name = dest
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(ArchiveReport {
        path: dest.to_path_buf(),
        file_name,
        bytes,
        rows: digest.rows,
        sha256: digest.sha256,
        source: source.to_owned(),
        session_id: session_id.map(str::to_owned),
        created_at: created_at_ms,
        counts: digest.counts,
    })
}

/// manifest + payload 组装 → `sync_all` → 原子 rename；失败清理最终暂存。
fn finalize_archive(
    dest: &Path,
    partial: &Path,
    manifest: &Value,
    created_at_ms: i64,
) -> Result<(), StoreError> {
    let temp = archive_temp_path(dest, created_at_ms);
    let result = (|| -> Result<(), StoreError> {
        let manifest_line = serde_json::to_string(manifest).map_err(serialization_failed)?;
        let file = File::create(&temp)?;
        {
            let mut writer = BufWriter::with_capacity(ARCHIVE_BUFFER_BYTES, &file);
            writer.write_all(manifest_line.as_bytes())?;
            writer.write_all(b"\n")?;
            let mut reader = BufReader::with_capacity(ARCHIVE_BUFFER_BYTES, File::open(partial)?);
            std::io::copy(&mut reader, &mut writer)?;
            writer.flush()?;
        }
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp, dest) {
        let _ = fs::remove_file(&temp);
        return Err(StoreError::Io(error));
    }
    Ok(())
}

fn serialization_failed(error: serde_json::Error) -> StoreError {
    StoreError::Internal {
        reason: format!("归档 JSON 序列化失败：{error}"),
    }
}

/// 单会话归档（`session_export`；ADR-021 §2.1-8）。
///
/// `sessions` 行 + `messages` 终稿（`seq` 升序）+ 该会话 `events`（`seq` 升序）；
/// 会话不存在返回 `Ok(None)`（命令层映射 `session_not_found` 业务码）。
pub fn export_session_archive(
    conn: &Connection,
    dir: &Path,
    session_id: &str,
    created_at_ms: i64,
    sanitize: ArchiveSanitizer<'_>,
) -> Result<Option<ArchiveReport>, StoreError> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sessions WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(None);
    }
    let dest = unique_archive_path(dir, &session_archive_file_name(session_id, created_at_ms))?;
    let report = write_archive(
        &dest,
        ARCHIVE_SOURCE_SESSION_EXPORT,
        Some(session_id),
        created_at_ms,
        |sink| {
            export_sessions(conn, sink, sanitize, Some(session_id))?;
            export_messages(conn, sink, sanitize, Some(session_id))?;
            export_events(conn, sink, sanitize, Some(session_id))?;
            Ok(())
        },
    )?;
    Ok(Some(report))
}

/// 全库归档（`archive_export`；UI-UX §8#16 一键归档）。
///
/// `sessions` + `messages` 全稿 + `events` 全量 + `runs` 摘要；
/// 单读连接 + 单读事务（WAL 快照一致）。
pub fn export_full_archive(
    conn: &Connection,
    dir: &Path,
    created_at_ms: i64,
    sanitize: ArchiveSanitizer<'_>,
) -> Result<ArchiveReport, StoreError> {
    let dest = unique_archive_path(dir, &full_archive_file_name(created_at_ms))?;
    conn.execute_batch("BEGIN DEFERRED;")?;
    let result = write_archive(
        &dest,
        ARCHIVE_SOURCE_ARCHIVE_EXPORT,
        None,
        created_at_ms,
        |sink| {
            export_sessions(conn, sink, sanitize, None)?;
            export_messages(conn, sink, sanitize, None)?;
            export_events(conn, sink, sanitize, None)?;
            export_runs(conn, sink, sanitize)?;
            Ok(())
        },
    );
    let rollback = conn.execute_batch("COMMIT;");
    result.and_then(|report| {
        rollback?;
        Ok(report)
    })
}

/// 保留清理批归档（M5-04；待删行 → `events-archive-<yyyymmdd>.jsonl`）。
///
/// 归档内容 = 传入的待删行（与随后删除的行逐行一致）。
pub fn archive_retention_batch(
    dir: &Path,
    rows: &[RetentionEventRow],
    created_at_ms: i64,
    sanitize: ArchiveSanitizer<'_>,
) -> Result<ArchiveReport, StoreError> {
    if rows.is_empty() {
        return Err(StoreError::Internal {
            reason: "保留清理批为空：拒绝产生空归档产物".to_owned(),
        });
    }
    let dest = unique_archive_path(dir, &retention_archive_file_name(dir, created_at_ms))?;
    write_archive(
        &dest,
        ARCHIVE_SOURCE_AUTO_RETENTION,
        None,
        created_at_ms,
        |sink| {
            for row in rows {
                sink.insert(&row.to_archive_value(), sanitize)?;
            }
            Ok(())
        },
    )
}

/// 校验归档产物：manifest 行数/sha256 与 payload 逐项一致（ADR-021 附录 A-2）。
pub fn verify_archive_file(path: &Path) -> Result<ArchiveVerification, StoreError> {
    let reader = BufReader::with_capacity(ARCHIVE_BUFFER_BYTES, File::open(path)?);
    let mut lines = reader.lines();
    let first = match lines.next() {
        Some(line) => line?,
        None => {
            return Err(StoreError::Internal {
                reason: format!("归档产物为空（{}）", path.display()),
            })
        }
    };
    let manifest: ArchiveManifest =
        serde_json::from_str(&first).map_err(|error| StoreError::Internal {
            reason: format!("归档 manifest 解析失败（{}）：{error}", path.display()),
        })?;
    if manifest.kind != ARCHIVE_MANIFEST_KIND || manifest.v != ARCHIVE_FORMAT_VERSION {
        return Err(StoreError::Internal {
            reason: format!(
                "归档 manifest 形态非法（{}）：kind={} v={}",
                path.display(),
                manifest.kind,
                manifest.v
            ),
        });
    }
    let mut hasher = Sha256::new();
    let mut rows = 0u64;
    for line in lines {
        let line = line?;
        // 行尾 `\n` 归一：`BufRead::lines` 去除行尾；按「行内容 + \n」哈希。
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
        rows = rows.saturating_add(1);
    }
    let sha256 = hex::encode(hasher.finalize());
    let matches = rows == manifest.rows && sha256 == manifest.sha256;
    Ok(ArchiveVerification {
        manifest,
        rows,
        sha256,
        matches,
    })
}

/// 保留清理候选批：白名单类型 + `ts < cutoff`（`rowid` 升序，≤`limit`）。
///
/// `limit` 由调用方以 [`RETENTION_BATCH_MAX`] 约束（ADR-021 §2.1-5）。
pub fn select_retention_batch(
    conn: &Connection,
    cutoff_ts: i64,
    limit: usize,
) -> Result<Vec<RetentionEventRow>, StoreError> {
    let limit =
        i64::try_from(limit.min(RETENTION_BATCH_MAX)).map_err(|_| StoreError::Internal {
            reason: "保留清理批上限转换失败".to_owned(),
        })?;
    let mut statement = conn.prepare(
        "SELECT rowid, id, session_id, run_id, runtime_id, seq, type, payload, ts, v \
         FROM events WHERE type IN (?1, ?2, ?3) AND ts < ?4 \
         ORDER BY rowid ASC LIMIT ?5",
    )?;
    let rows = statement.query_map(
        rusqlite::params![
            RETENTION_EVENT_TYPES[0],
            RETENTION_EVENT_TYPES[1],
            RETENTION_EVENT_TYPES[2],
            cutoff_ts,
            limit,
        ],
        read_retention_row,
    )?;
    let mut batch = Vec::new();
    for row in rows {
        batch.push(row?);
    }
    Ok(batch)
}

fn read_retention_row(row: &Row<'_>) -> rusqlite::Result<RetentionEventRow> {
    let payload: String = row.get(7)?;
    let payload: Value = serde_json::from_str(&payload).map_err(|_| {
        rusqlite::Error::InvalidColumnType(
            7,
            "events.payload 不是合法 JSON".to_owned(),
            rusqlite::types::Type::Text,
        )
    })?;
    Ok(RetentionEventRow {
        rowid: row.get(0)?,
        id: row.get(1)?,
        session_id: row.get(2)?,
        run_id: row.get(3)?,
        runtime_id: row.get(4)?,
        seq: row.get(5)?,
        event_type: row.get(6)?,
        payload,
        ts: row.get(8)?,
        v: row.get(9)?,
    })
}

/// `sessions` 行 → 归档行（`kind=session`；全部列，脱敏由 sanitizer 承担）。
fn export_sessions(
    conn: &Connection,
    sink: &mut PayloadSink,
    sanitize: ArchiveSanitizer<'_>,
    session_id: Option<&str>,
) -> Result<(), StoreError> {
    const SQL_ALL: &str = "SELECT id, runtime_id, workspace_id, parent_session_id, title, status, \
         model, thinking_depth, system_prompt, config, token_usage, created_at, updated_at, closed_at \
         FROM sessions ORDER BY id ASC";
    const SQL_ONE: &str = "SELECT id, runtime_id, workspace_id, parent_session_id, title, status, \
         model, thinking_depth, system_prompt, config, token_usage, created_at, updated_at, closed_at \
         FROM sessions WHERE id = ?1";
    let mut statement = conn.prepare(session_id.map_or(SQL_ALL, |_| SQL_ONE))?;
    let mut emit = |row: &Row<'_>| -> Result<(), StoreError> {
        let config: Option<String> = row.get(9)?;
        let token_usage: Option<String> = row.get(10)?;
        let value = json!({
            "v": ARCHIVE_FORMAT_VERSION,
            "kind": "session",
            "id": row.get::<_, String>(0)?,
            "runtime_id": row.get::<_, String>(1)?,
            "workspace_id": row.get::<_, Option<String>>(2)?,
            "parent_session_id": row.get::<_, Option<String>>(3)?,
            "title": row.get::<_, String>(4)?,
            "status": row.get::<_, String>(5)?,
            "model": row.get::<_, Option<String>>(6)?,
            "thinking_depth": row.get::<_, i64>(7)?,
            "system_prompt": row.get::<_, Option<String>>(8)?,
            "config": parse_optional_json(config, "sessions.config")?,
            "token_usage": parse_optional_json(token_usage, "sessions.token_usage")?,
            "created_at": row.get::<_, i64>(11)?,
            "updated_at": row.get::<_, i64>(12)?,
            "closed_at": row.get::<_, Option<i64>>(13)?,
        });
        sink.insert(&value, sanitize)
    };
    match session_id {
        Some(id) => {
            let mut rows = statement.query([id])?;
            while let Some(row) = rows.next()? {
                emit(row)?;
            }
        }
        None => {
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                emit(row)?;
            }
        }
    }
    Ok(())
}

/// `messages` 行 → 归档行（`kind=message`；ADR-021 附录 A-2 字段集：
/// `session_id/id/role/content/seq/ts`，`ts` = `messages.created_at` 映射）。
fn export_messages(
    conn: &Connection,
    sink: &mut PayloadSink,
    sanitize: ArchiveSanitizer<'_>,
    session_id: Option<&str>,
) -> Result<(), StoreError> {
    const SQL_ALL: &str = "SELECT id, session_id, role, content, seq, created_at FROM messages \
         ORDER BY session_id ASC, seq ASC, rowid ASC";
    const SQL_ONE: &str = "SELECT id, session_id, role, content, seq, created_at FROM messages \
         WHERE session_id = ?1 ORDER BY seq ASC, rowid ASC";
    let mut statement = conn.prepare(session_id.map_or(SQL_ALL, |_| SQL_ONE))?;
    let mut emit = |row: &Row<'_>| -> Result<(), StoreError> {
        let value = json!({
            "v": ARCHIVE_FORMAT_VERSION,
            "kind": "message",
            "session_id": row.get::<_, String>(1)?,
            "id": row.get::<_, String>(0)?,
            "role": row.get::<_, String>(2)?,
            "content": row.get::<_, String>(3)?,
            "seq": row.get::<_, i64>(4)?,
            "ts": row.get::<_, i64>(5)?,
        });
        sink.insert(&value, sanitize)
    };
    match session_id {
        Some(id) => {
            let mut rows = statement.query([id])?;
            while let Some(row) = rows.next()? {
                emit(row)?;
            }
        }
        None => {
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                emit(row)?;
            }
        }
    }
    Ok(())
}

/// `events` 行 → 归档行（`kind=event`；信封 9 字段与附录 B 类型逐字段一致）。
fn export_events(
    conn: &Connection,
    sink: &mut PayloadSink,
    sanitize: ArchiveSanitizer<'_>,
    session_id: Option<&str>,
) -> Result<(), StoreError> {
    const SQL_ALL: &str = "SELECT id, session_id, run_id, runtime_id, seq, type, payload, ts, v \
         FROM events ORDER BY session_id ASC, seq ASC, rowid ASC";
    const SQL_ONE: &str = "SELECT id, session_id, run_id, runtime_id, seq, type, payload, ts, v \
         FROM events WHERE session_id = ?1 ORDER BY seq ASC, rowid ASC";
    let mut statement = conn.prepare(session_id.map_or(SQL_ALL, |_| SQL_ONE))?;
    let mut emit = |row: &Row<'_>| -> Result<(), StoreError> {
        let id: String = row.get(0)?;
        let payload: String = row.get(6)?;
        let payload: Value =
            serde_json::from_str(&payload).map_err(|_| StoreError::InvalidStoredEvent {
                id: id.clone(),
                reason: "payload 不是合法 JSON".to_owned(),
            })?;
        let value = json!({
            "v": row.get::<_, i64>(8)?,
            "kind": "event",
            "id": id,
            "session_id": row.get::<_, String>(1)?,
            "run_id": row.get::<_, Option<String>>(2)?,
            "runtime_id": row.get::<_, String>(3)?,
            "seq": row.get::<_, i64>(4)?,
            "ts": row.get::<_, i64>(7)?,
            "type": row.get::<_, String>(5)?,
            "payload": payload,
        });
        sink.insert(&value, sanitize)
    };
    match session_id {
        Some(id) => {
            let mut rows = statement.query([id])?;
            while let Some(row) = rows.next()? {
                emit(row)?;
            }
        }
        None => {
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                emit(row)?;
            }
        }
    }
    Ok(())
}

/// `runs` 行 → 归档行（`kind=run`；全库归档摘要）。
fn export_runs(
    conn: &Connection,
    sink: &mut PayloadSink,
    sanitize: ArchiveSanitizer<'_>,
) -> Result<(), StoreError> {
    let mut statement = conn.prepare(
        "SELECT id, session_id, status, input_message_id, thinking_depth, error, started_at, finished_at \
         FROM runs ORDER BY session_id ASC, started_at ASC, rowid ASC",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let value = json!({
            "v": ARCHIVE_FORMAT_VERSION,
            "kind": "run",
            "id": row.get::<_, String>(0)?,
            "session_id": row.get::<_, String>(1)?,
            "status": row.get::<_, String>(2)?,
            "input_message_id": row.get::<_, Option<String>>(3)?,
            "thinking_depth": row.get::<_, Option<i64>>(4)?,
            "error": row.get::<_, Option<String>>(5)?,
            "started_at": row.get::<_, i64>(6)?,
            "finished_at": row.get::<_, Option<i64>>(7)?,
        });
        sink.insert(&value, sanitize)?;
    }
    Ok(())
}

fn parse_optional_json(text: Option<String>, field: &str) -> Result<Value, StoreError> {
    match text {
        Some(text) => serde_json::from_str(&text).map_err(|error| StoreError::Internal {
            reason: format!("{field} 不是合法 JSON：{error}"),
        }),
        None => Ok(Value::Null),
    }
}

/// UTC 日期（`yyyymmdd`；保留清理归档命名；无第三方日期依赖）。
pub fn compact_date_utc(epoch_ms: i64) -> String {
    let days = epoch_ms.div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}{month:02}{day:02}")
}

/// 天数（1970-01-01 起）→ 公历年月日（Howard Hinnant civil_from_days 算法）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 { year + 1 } else { year },
        u32::try_from(month).unwrap_or(0),
        u32::try_from(day).unwrap_or(0),
    )
}

/// 归档行键集合（字段白名单；测试/校验脚本共用口径）。
///
/// 返回 `(kind, 允许字段集)`；`manifest` 由格式定义单独校验。
pub fn archive_line_whitelist(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "session" => Some(&[
            "v",
            "kind",
            "id",
            "runtime_id",
            "workspace_id",
            "parent_session_id",
            "title",
            "status",
            "model",
            "thinking_depth",
            "system_prompt",
            "config",
            "token_usage",
            "created_at",
            "updated_at",
            "closed_at",
        ]),
        "message" => Some(&[
            "v",
            "kind",
            "session_id",
            "id",
            "role",
            "content",
            "seq",
            "ts",
        ]),
        "event" => Some(&[
            "v",
            "kind",
            "id",
            "session_id",
            "run_id",
            "runtime_id",
            "seq",
            "ts",
            "type",
            "payload",
        ]),
        "run" => Some(&[
            "v",
            "kind",
            "id",
            "session_id",
            "status",
            "input_message_id",
            "thinking_depth",
            "error",
            "started_at",
            "finished_at",
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 恒等脱敏（测试内无密钥样本；0 命中守门由 tauri 层覆盖）。
    fn identity(value: &Value) -> Result<String, StoreError> {
        serde_json::to_string(value).map_err(serialization_failed)
    }

    fn migrated_connection() -> Connection {
        let mut connection = Connection::open_in_memory().unwrap();
        crate::migration::migrate(&mut connection).unwrap();
        connection
    }

    fn seed(connection: &Connection) {
        connection
            .execute_batch(
                "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
                 VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1);\
                 INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                 VALUES ('01J0000000000000000000000A', 'mock', '会话 A', 'idle', 10, 10);\
                 INSERT INTO messages (id, session_id, role, content, seq, created_at) \
                 VALUES ('01J000000000000000000000M1', '01J0000000000000000000000A', 'user', '你好', 1, 11),\
                        ('01J000000000000000000000M2', '01J0000000000000000000000A', 'assistant', '回复', 2, 12);\
                 INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
                 VALUES ('01J000000000000000000000E1', '01J0000000000000000000000A', NULL, 'mock', 1, 'session.created', '{\"title\":\"会话 A\"}', 10, 1),\
                        ('01J000000000000000000000E2', '01J0000000000000000000000A', NULL, 'mock', 2, 'message.delta', '{\"message_id\":\"m1\",\"text\":\"chunk\"}', 11, 1),\
                        ('01J000000000000000000000E3', '01J0000000000000000000000A', NULL, 'mock', 3, 'message.completed', '{\"message_id\":\"m1\"}', 12, 1);\
                 INSERT INTO runs (id, session_id, status, input_message_id, started_at) \
                 VALUES ('01J000000000000000000000R1', '01J0000000000000000000000A', 'succeeded', '01J000000000000000000000M1', 11);",
            )
            .unwrap();
    }

    #[test]
    fn session_archive_roundtrips_manifest_and_rows() {
        let dir = tempfile::TempDir::new().unwrap();
        let connection = migrated_connection();
        seed(&connection);
        let report = export_session_archive(
            &connection,
            dir.path(),
            "01J0000000000000000000000A",
            1_700_000_000_000,
            &identity,
        )
        .unwrap()
        .expect("会话存在");
        assert_eq!(report.source, ARCHIVE_SOURCE_SESSION_EXPORT);
        assert_eq!(report.counts.sessions, 1);
        assert_eq!(report.counts.messages, 2);
        assert_eq!(report.counts.events, 3);
        assert_eq!(report.rows, 6);
        assert!(report
            .file_name
            .starts_with("session-01J0000000000000000000000A-"));
        assert!(report.file_name.ends_with(".jsonl"));

        let verification = verify_archive_file(&report.path).unwrap();
        assert!(
            verification.matches,
            "manifest 行数/哈希必须与 payload 一致"
        );
        assert_eq!(verification.manifest.rows, 6);
        assert_eq!(verification.manifest.source, ARCHIVE_SOURCE_SESSION_EXPORT);
        assert_eq!(
            verification.manifest.session_id.as_deref(),
            Some("01J0000000000000000000000A")
        );

        // 首行 manifest；事件行按 seq 升序；字段白名单逐项一致。
        let text = std::fs::read_to_string(&report.path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 7);
        let manifest: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(manifest["kind"], "manifest");
        let mut event_seqs = Vec::new();
        for line in &lines[1..] {
            let value: Value = serde_json::from_str(line).unwrap();
            let kind = value["kind"].as_str().unwrap();
            let whitelist = archive_line_whitelist(kind).unwrap();
            for key in value.as_object().unwrap().keys() {
                assert!(
                    whitelist.contains(&key.as_str()),
                    "未知字段 {key}（kind={kind}）"
                );
            }
            if kind == "event" {
                event_seqs.push(value["seq"].as_i64().unwrap());
                assert_eq!(value["v"], 1);
            }
        }
        assert_eq!(event_seqs, vec![1, 2, 3], "events 按 seq 升序");
    }

    #[test]
    fn session_archive_missing_session_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let connection = migrated_connection();
        seed(&connection);
        let report = export_session_archive(
            &connection,
            dir.path(),
            "01J0000000000000000000000Z",
            1,
            &identity,
        )
        .unwrap();
        assert!(report.is_none());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "无会话不得产生任何文件"
        );
    }

    #[test]
    fn full_archive_covers_all_tables_and_is_verifiable() {
        let dir = tempfile::TempDir::new().unwrap();
        let connection = migrated_connection();
        seed(&connection);
        let report =
            export_full_archive(&connection, dir.path(), 1_700_000_000_001, &identity).unwrap();
        assert_eq!(report.source, ARCHIVE_SOURCE_ARCHIVE_EXPORT);
        assert_eq!(report.counts.sessions, 1);
        assert_eq!(report.counts.messages, 2);
        assert_eq!(report.counts.events, 3);
        assert_eq!(report.counts.runs, 1);
        assert_eq!(report.rows, 7);
        assert!(report.file_name.starts_with("aether-archive-"));
        let verification = verify_archive_file(&report.path).unwrap();
        assert!(verification.matches);
    }

    #[test]
    fn tampered_archive_is_detected() {
        let dir = tempfile::TempDir::new().unwrap();
        let connection = migrated_connection();
        seed(&connection);
        let report = export_full_archive(&connection, dir.path(), 7, &identity).unwrap();
        let text = std::fs::read_to_string(&report.path).unwrap();
        let tampered = format!("{text}X\n");
        std::fs::write(&report.path, tampered).unwrap();
        let verification = verify_archive_file(&report.path).unwrap();
        assert!(!verification.matches, "篡改必须被检出（行数/哈希不一致）");
    }

    #[test]
    fn archive_error_injection_leaves_no_partial_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let connection = migrated_connection();
        seed(&connection);
        let failing = |value: &Value| -> Result<String, StoreError> {
            if value.get("kind").and_then(Value::as_str) == Some("message") {
                return Err(StoreError::Internal {
                    reason: "注入：脱敏阶段失败".to_owned(),
                });
            }
            serde_json::to_string(value).map_err(serialization_failed)
        };
        let result = export_full_archive(&connection, dir.path(), 7, &failing);
        assert!(result.is_err(), "注入失败必须向上传播");
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            leftovers.is_empty(),
            "失败路径不得产生半成品/暂存残留：{leftovers:?}"
        );
    }

    #[test]
    fn stale_partials_are_cleaned() {
        let dir = tempfile::TempDir::new().unwrap();
        let dest = dir.path().join("aether-archive-1.jsonl");
        let partial = archive_partial_path(&dest, 1);
        let temp = archive_temp_path(&dest, 1);
        std::fs::write(&partial, b"partial").unwrap();
        std::fs::write(&temp, b"temp").unwrap();
        std::fs::write(dest.with_file_name("unrelated.txt"), b"keep").unwrap();
        assert_eq!(cleanup_archive_temps(dir.path()), 2);
        assert!(!partial.exists() && !temp.exists());
        assert!(dest.with_file_name("unrelated.txt").exists());
        assert_eq!(cleanup_archive_temps(dir.path()), 0);
    }

    #[test]
    fn retention_batch_selection_honors_whitelist_and_window() {
        let connection = migrated_connection();
        connection
            .execute_batch(
                "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) VALUES \
                 ('e1', 's', NULL, 'mock', 1, 'message.delta', '{\"a\":1}', 100, 1),\
                 ('e2', 's', NULL, 'mock', 2, 'usage', '{\"tokens\":5}', 100, 1),\
                 ('e3', 's', NULL, 'mock', 3, 'log', '{\"level\":\"info\"}', 100, 1),\
                 ('e4', 's', NULL, 'mock', 4, 'message.completed', '{\"m\":1}', 100, 1),\
                 ('e5', 's', NULL, 'mock', 5, 'permission.requested', '{\"p\":1}', 100, 1),\
                 ('e6', 's', NULL, 'mock', 6, 'message.delta', '{\"a\":2}', 900, 1),\
                 ('e7', 's', NULL, 'mock', 7, 'tool.call_started', '{\"tool\":\"x\"}', 100, 1),\
                 ('e8', 's', NULL, 'mock', 8, 'error', '{\"code\":\"x\"}', 100, 1),\
                 ('e9', 's', NULL, 'mock', 9, 'runtime.status_changed', '{\"status\":\"ready\"}', 100, 1),\
                 ('e10', 's', NULL, 'mock', 10, 'session.created', '{\"t\":\"x\"}', 100, 1);",
            )
            .unwrap();
        let batch = select_retention_batch(&connection, 500, 5_000).unwrap();
        let ids: Vec<&str> = batch.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["e1", "e2", "e3"],
            "仅白名单类型且 ts < cutoff（e4/e6/e7/e8/e9/e10 保留）"
        );
        // limit 约束。
        let limited = select_retention_batch(&connection, 500, 2).unwrap();
        assert_eq!(limited.len(), 2);
        // 删除精确定位行后，非白名单行完整保留。
        let rowids: Vec<i64> = batch.iter().map(|row| row.rowid).collect();
        let mut connection = connection;
        let outcome = crate::ops::apply_command(
            &mut connection,
            &crate::ops::StoreCommand::DeleteEventsByIds { rowids },
        )
        .unwrap();
        assert_eq!(outcome, crate::ops::StoreOutcome::Applied { affected: 3 });
        let remaining: Vec<String> = connection
            .prepare("SELECT id FROM events ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            remaining,
            vec!["e10", "e4", "e5", "e6", "e7", "e8", "e9"],
            "白名单外事件与控制事件必须完整保留"
        );
    }

    #[test]
    fn retention_archive_contains_exact_batch_rows() {
        let dir = tempfile::TempDir::new().unwrap();
        let connection = migrated_connection();
        connection
            .execute_batch(
                "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) VALUES \
                 ('e1', 's', NULL, 'mock', 1, 'message.delta', '{\"a\":1}', 100, 1),\
                 ('e2', 's', NULL, 'mock', 2, 'usage', '{\"tokens\":5}', 100, 1);",
            )
            .unwrap();
        let batch = select_retention_batch(&connection, 500, 5_000).unwrap();
        let report =
            archive_retention_batch(dir.path(), &batch, 1_700_000_000_000, &identity).unwrap();
        assert_eq!(report.source, ARCHIVE_SOURCE_AUTO_RETENTION);
        assert_eq!(report.counts.events, 2);
        assert!(report.file_name.starts_with("events-archive-"));
        let verification = verify_archive_file(&report.path).unwrap();
        assert!(verification.matches);
    }

    #[test]
    fn compact_date_matches_known_utc_dates() {
        assert_eq!(compact_date_utc(0), "19700101");
        assert_eq!(compact_date_utc(1_700_000_000_000), "20231114");
        // 2026-10-12T00:00:00Z = 1_791_763_200_000
        assert_eq!(compact_date_utc(1_791_763_200_000), "20261012");
    }
}
