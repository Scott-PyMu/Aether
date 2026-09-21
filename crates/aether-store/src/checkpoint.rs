//! WAL checkpoint 纪律（设计 D3；实施计划 M2-06）。
//!
//! - **关闭序列**（D2）：`wal_checkpoint(TRUNCATE)` 仅在写队列 drain 且全部读连接
//!   关闭后执行（完整顺序见 [`crate::write_queue::ShutdownStep`]）；
//! - **运行期强制 checkpoint**：WAL 超过 [`WAL_FORCE_CHECKPOINT_BYTES`]（256MB）时
//!   执行 `wal_checkpoint(TRUNCATE)`；若因读锁失败 → 退避重试并记录诊断（D3 评审修订 #5）；
//! - **节奏控制**：单次尝试临时把 `busy_timeout` 置 0（不让 SQLite 的 busy 等待主导），
//!   由本层退避调度决定重试间隔，尝试结束恢复原值。
//!
//! 本模块只做机制与诊断，不引入新事件类型、不写库（AGENTS §2.3/§2.4）。

use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode};

use crate::error::StoreError;

/// 运行期强制 checkpoint 的 WAL 阈值（D3：WAL >256MB → 强制 `wal_checkpoint(TRUNCATE)`）。
pub const WAL_FORCE_CHECKPOINT_BYTES: u64 = 256 * 1024 * 1024;
/// 默认最大尝试次数（含首次；即重试 4 次）。
pub const CHECKPOINT_MAX_ATTEMPTS: u32 = 5;
/// 默认首次退避（失败后等待再试）。
pub const CHECKPOINT_INITIAL_BACKOFF: Duration = Duration::from_millis(50);
/// 默认退避上限（指数退避封顶）。
pub const CHECKPOINT_MAX_BACKOFF: Duration = Duration::from_millis(400);

/// checkpoint 退避配置（默认值即 D3 约定；故障注入可参数化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointConfig {
    /// 最大尝试次数（含首次；至少 1）。
    pub max_attempts: u32,
    /// 首次退避时长（第 n 次失败后等待 `initial * 2^(n-1)`，封顶 `max_backoff`）。
    pub initial_backoff: Duration,
    /// 退避上限。
    pub max_backoff: Duration,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            max_attempts: CHECKPOINT_MAX_ATTEMPTS,
            initial_backoff: CHECKPOINT_INITIAL_BACKOFF,
            max_backoff: CHECKPOINT_MAX_BACKOFF,
        }
    }
}

impl CheckpointConfig {
    /// 校验（尝试次数 ≥1；退避上限 ≥ 首次退避）。
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.max_attempts == 0 {
            return Err(StoreError::InvalidCheckpointConfig {
                reason: "max_attempts 必须 ≥1（含首次）".to_owned(),
            });
        }
        if self.max_backoff < self.initial_backoff {
            return Err(StoreError::InvalidCheckpointConfig {
                reason: "max_backoff 不得小于 initial_backoff".to_owned(),
            });
        }
        Ok(())
    }

    /// 第 `failed_attempt` 次失败后的退避时长（指数退避，封顶）。
    pub fn backoff_after_failure(&self, failed_attempt: u32) -> Duration {
        let mut backoff = self.initial_backoff;
        let mut remaining = failed_attempt.saturating_sub(1);
        while remaining > 0 {
            backoff = backoff.saturating_mul(2);
            if backoff >= self.max_backoff {
                return self.max_backoff;
            }
            remaining -= 1;
        }
        backoff.min(self.max_backoff)
    }
}

/// 单次 checkpoint 尝试记录（退避重试诊断的原子条目）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointAttempt {
    /// 第几次尝试（从 1 起；含首次）。
    pub attempt: u32,
    /// 本次尝试是否因读锁（`SQLITE_BUSY`/`SQLITE_LOCKED`）未完成。
    pub busy: bool,
    /// 本次尝试前等待的退避时长（首次为 0）。
    pub waited_ms: u64,
    /// 本次尝试耗时（毫秒）。
    pub duration_ms: u64,
    /// WAL 中的帧数（失败或无法读取为 -1）。
    pub frames_log: i64,
    /// 本次已回写主库的帧数（失败或无法读取为 -1）。
    pub frames_checkpointed: i64,
    /// 失败原因（成功为 `None`）。
    pub error: Option<String>,
}

/// checkpoint 结果与诊断（`succeeded=false` 时 `history` 即退避重试记录）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointReport {
    /// 实际尝试次数（含首次）。
    pub attempts: u32,
    /// 是否成功完成 checkpoint（`busy` 行与 SQL 错误均视为未完成）。
    pub succeeded: bool,
    /// 最后一次尝试是否因读锁未完成。
    pub busy: bool,
    /// WAL 中的帧数（-1 = 无法读取）。
    pub frames_log: i64,
    /// 已回写主库的帧数（-1 = 无法读取）。
    pub frames_checkpointed: i64,
    /// 最终失败原因（成功为 `None`）。
    pub error: Option<String>,
    /// 每次尝试的完整记录（诊断导出源；M3-05 消费）。
    pub history: Vec<CheckpointAttempt>,
    /// 整体耗时（毫秒，含退避等待）。
    pub duration_ms: u64,
}

impl CheckpointReport {
    /// 是否发生退避重试（尝试次数 >1）。
    pub fn retried(&self) -> bool {
        self.attempts > 1
    }

    /// 因读锁未完成的尝试次数。
    pub fn busy_attempts(&self) -> usize {
        self.history.iter().filter(|attempt| attempt.busy).count()
    }

    /// 单行诊断摘要（日志/诊断包展示；不含密钥）。
    pub fn diagnostic_summary(&self) -> String {
        format!(
            "wal_checkpoint(TRUNCATE)：attempts={} succeeded={} busy={} frames={}/{} duration_ms={}{}",
            self.attempts,
            self.succeeded,
            self.busy,
            self.frames_checkpointed,
            self.frames_log,
            self.duration_ms,
            match &self.error {
                Some(error) => format!("；最后错误：{error}"),
                None => String::new(),
            }
        )
    }
}

/// 单次 `wal_checkpoint(TRUNCATE)`（不做退避；供退避驱动与测试复用）。
///
/// `PRAGMA wal_checkpoint(TRUNCATE)` 返回一行 `(busy, log, checkpointed)`：
/// 读锁未释放时以 `busy != 0` 行返回（也可能返回 `SQLITE_BUSY` 错误），两者均视为未完成。
pub fn checkpoint_truncate_once(conn: &Connection) -> CheckpointAttempt {
    let started = Instant::now();
    let queried = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    });
    let duration_ms = elapsed_ms(started);
    match queried {
        Ok((busy, frames_log, frames_checkpointed)) => {
            let busy = busy != 0;
            CheckpointAttempt {
                attempt: 0,
                busy,
                waited_ms: 0,
                duration_ms,
                frames_log,
                frames_checkpointed,
                error: busy.then(|| "SQLITE_BUSY：存在未释放的 WAL 读锁（并发读事务）".to_owned()),
            }
        }
        Err(error) => CheckpointAttempt {
            attempt: 0,
            busy: is_lock_error(&error),
            waited_ms: 0,
            duration_ms,
            frames_log: -1,
            frames_checkpointed: -1,
            error: Some(error.to_string()),
        },
    }
}

/// 带退避重试的 `wal_checkpoint(TRUNCATE)`（D3：失败退避重试并记录诊断）。
///
/// - **同步阻塞**：在单写者任务内执行；退避等待（最坏 `attempts × max_backoff`）期间
///   写队列积压但保持 FIFO——与单写连接串行语义一致，不引入额外并发；
/// - 单次尝试期间 `busy_timeout=0`（重试节奏由本层控制），结束时恢复原值；
/// - 返回 `succeeded=false` 不视为调用错误：调用方据 `history` 记录诊断并继续关闭序列
///   （D2「保证能退出优先」）。
pub fn checkpoint_truncate_with_backoff(
    conn: &Connection,
    config: &CheckpointConfig,
) -> CheckpointReport {
    let started = Instant::now();
    let previous_timeout_ms = conn
        .pragma_query_value(None, "busy_timeout", |row| row.get::<_, i64>(0))
        .ok();
    let previous_timeout = previous_timeout_ms.map(|ms| Duration::from_millis(ms.max(0) as u64));
    // `busy_timeout=0`：读锁场景立即返回 busy，由本层退避调度。
    let _ = conn.busy_timeout(Duration::ZERO);

    let mut history: Vec<CheckpointAttempt> = Vec::new();
    let mut waited_ms = 0u64;
    for attempt in 1..=config.max_attempts {
        let mut record = checkpoint_truncate_once(conn);
        record.attempt = attempt;
        record.waited_ms = waited_ms;
        let succeeded = !record.busy && record.error.is_none();
        history.push(record);
        if succeeded || attempt >= config.max_attempts {
            break;
        }
        let backoff = config.backoff_after_failure(attempt);
        waited_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX);
        std::thread::sleep(backoff);
    }

    if let Some(timeout) = previous_timeout {
        let _ = conn.busy_timeout(timeout);
    }

    match history.last() {
        Some(last) => CheckpointReport {
            attempts: u32::try_from(history.len()).unwrap_or(u32::MAX),
            succeeded: !last.busy && last.error.is_none(),
            busy: last.busy,
            frames_log: last.frames_log,
            frames_checkpointed: last.frames_checkpointed,
            error: last.error.clone(),
            history,
            duration_ms: elapsed_ms(started),
        },
        None => CheckpointReport {
            attempts: 0,
            succeeded: false,
            busy: false,
            frames_log: -1,
            frames_checkpointed: -1,
            error: Some("checkpoint 未执行（尝试次数为 0）".to_owned()),
            history,
            duration_ms: elapsed_ms(started),
        },
    }
}

/// 是否读锁类错误（busy/locked；含扩展码）。
fn is_lock_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(ffi, _)
            if matches!(ffi.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn default_config_matches_design_constants() {
        let config = CheckpointConfig::default();
        assert_eq!(config.max_attempts, 5, "含首次共 5 次尝试（重试 4 次）");
        assert_eq!(config.initial_backoff, Duration::from_millis(50));
        assert_eq!(config.max_backoff, Duration::from_millis(400));
        assert_eq!(WAL_FORCE_CHECKPOINT_BYTES, 268_435_456, "D3：WAL >256MB");
        config.validate().expect("默认配置必须合法");
    }

    #[test]
    fn invalid_config_is_rejected() {
        let cases = [
            CheckpointConfig {
                max_attempts: 0,
                ..CheckpointConfig::default()
            },
            CheckpointConfig {
                initial_backoff: Duration::from_millis(500),
                max_backoff: Duration::from_millis(100),
                ..CheckpointConfig::default()
            },
        ];
        for config in cases {
            let error = config.validate().expect_err("非法配置必须拒绝");
            assert!(
                matches!(error, StoreError::InvalidCheckpointConfig { .. }),
                "实际: {error:?}"
            );
            assert_eq!(error.code(), "invalid_checkpoint_config");
        }
    }

    #[test]
    fn backoff_schedule_is_exponential_and_capped() {
        let config = CheckpointConfig {
            max_attempts: 10,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(40),
        };
        let schedule: Vec<u64> = (1..=6)
            .map(|attempt| {
                u64::try_from(config.backoff_after_failure(attempt).as_millis()).unwrap()
            })
            .collect();
        assert_eq!(schedule, vec![10, 20, 40, 40, 40, 40], "指数退避 + 封顶");
    }

    /// 读锁占用下：退避重试直到释放成功；诊断记录 busy 尝试。
    #[test]
    fn backoff_retries_until_reader_releases() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("aether.db");
        let writer = Connection::open(&db).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (v TEXT);")
            .unwrap();
        writer
            .execute_batch("INSERT INTO t VALUES ('a'), ('b'), ('c');")
            .unwrap();
        writer.busy_timeout(Duration::from_millis(5_000)).unwrap();

        let reader = Connection::open(&db).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM t;")
            .unwrap();
        // 90ms 后释放读锁（独立线程完成同步 COMMIT）。
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(90));
            reader.execute_batch("COMMIT;")
        });

        let config = CheckpointConfig {
            max_attempts: 40,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(20),
        };
        let report = checkpoint_truncate_with_backoff(&writer, &config);
        release.join().unwrap().unwrap();

        assert!(report.succeeded, "读锁释放后必须成功：{report:?}");
        assert!(report.busy_attempts() >= 1, "必须记录 busy 尝试");
        assert!(report.retried(), "必须发生退避重试");
        assert!(report.duration_ms >= 10, "退避等待必须计入耗时");
        assert!(
            report.history.iter().any(|attempt| attempt.error.is_some()),
            "失败尝试必须带诊断"
        );
        assert_eq!(
            writer
                .pragma_query_value(None, "busy_timeout", |row| row.get::<_, i64>(0))
                .unwrap(),
            5_000,
            "尝试结束后必须恢复 busy_timeout"
        );
        assert!(
            !checkpoint_truncate_once(&writer).busy,
            "释放读锁后单次 checkpoint 必须成功"
        );
    }

    /// 读锁持续占用：尝试耗尽后返回失败报告（诊断含 busy 尝试）。
    #[test]
    fn exhausted_attempts_report_busy_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("aether.db");
        let writer = Connection::open(&db).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (v TEXT);")
            .unwrap();
        writer
            .execute_batch("INSERT INTO t VALUES ('a'), ('b');")
            .unwrap();

        let reader = Connection::open(&db).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM t;")
            .unwrap();

        let config = CheckpointConfig {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(10),
        };
        let report = checkpoint_truncate_with_backoff(&writer, &config);
        assert!(!report.succeeded, "读锁未释放时不得成功");
        assert_eq!(report.attempts, 3, "尝试次数 = max_attempts");
        assert_eq!(report.busy_attempts(), 3, "全部尝试均因读锁未完成");
        assert!(report.busy);
        assert!(
            report.error.as_deref().unwrap_or_default().contains("BUSY"),
            "失败原因必须标记读锁: {:?}",
            report.error
        );
        assert_eq!(report.history.len(), 3, "退避重试记录逐次保留");
        assert!(
            report
                .history
                .iter()
                .skip(1)
                .all(|attempt| attempt.waited_ms > 0),
            "重试必须经历退避等待"
        );

        // 释放读锁后再次执行 → 成功（证明失败源于读锁而非库损坏）。
        reader.execute_batch("COMMIT;").unwrap();
        let again = checkpoint_truncate_with_backoff(&writer, &CheckpointConfig::default());
        assert!(again.succeeded);
    }

    /// 未校验的 `max_attempts=0` 配置：不执行 SQL、返回显式诊断（防御性路径）。
    #[test]
    fn zero_max_attempts_reports_without_executing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("aether.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (v TEXT);")
            .unwrap();
        conn.busy_timeout(Duration::from_millis(5_000)).unwrap();

        let report = checkpoint_truncate_with_backoff(
            &conn,
            &CheckpointConfig {
                max_attempts: 0,
                initial_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(1),
            },
        );
        assert_eq!(report.attempts, 0);
        assert!(!report.succeeded);
        assert!(report.history.is_empty());
        assert!(report
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("未执行"));
        assert_eq!(
            conn.pragma_query_value(None, "busy_timeout", |row| row.get::<_, i64>(0))
                .unwrap(),
            5_000,
            "防御性路径也必须恢复 busy_timeout"
        );
    }
}
