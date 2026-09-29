//! 备份与恢复（设计 D13 / 实施计划 M3-04）。
//!
//! 范围（P0 手动备份）：
//! - 手动备份：`VACUUM INTO` 产物（已 checkpoint 的单文件，**不含 `-wal`/`-shm`**），
//!   保留最近 [`BACKUP_RETENTION`] 份；经只读连接执行（D4：降级/安全模式下备份入口
//!   保持可用）；
//! - 恢复七步（D13，步骤 1/7 由调用方承担——来源选择、核心重启与审计）：
//!   2 校验候选（只读打开 + `quick_check` + `schema_migrations` 版本）→ 3 停写
//!   （调用方回调；启动序列内执行时天然无写者）→ 4 现场保护（当前库改名
//!   `.pre-restore-<ts>`，`-wal`/`-shm` 一并改名）→ 5 就位（跨盘临时文件 + 原子
//!   rename）→ 6 验收（重新打开 + `quick_check` + 迁移到当前版本；任一步失败回滚
//!   现场三件套）；
//! - 恢复现场日志（journal，`<db>.restore-journal.json`）：恢复请求先落日志再执行，
//!   `kill -9` 中断后由 [`recover_or_apply_pending_restore`] 在下次启动时自动回滚或
//!   续做（DoD4「现场三件套完整可回退」）；成功验收后日志置 `done`。
//!
//! 本模块不打开写连接（备份走只读；恢复在无写者窗口执行），写路径由调用方经
//! 单写队列落库（AGENTS §2.4）。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::migration::EMBEDDED_MIGRATIONS;
use crate::store::{quick_check, Store};

/// 备份保留份数（D13：手动备份保留最近 10 份）。
pub const BACKUP_RETENTION: usize = 10;
/// 备份/恢复空间系数分子（ADR-003 决策 19：可用空间 ≥ 当前 `db+wal` ×1.2）。
pub const SPACE_MARGIN_NUMERATOR: u64 = 6;
/// 备份/恢复空间系数分母。
pub const SPACE_MARGIN_DENOMINATOR: u64 = 5;
/// 现场保护改名标记（`aether.db.pre-restore-<ts>`；`-wal`/`-shm` 同后缀）。
pub const PRE_RESTORE_INFIX: &str = ".pre-restore-";
/// 恢复现场日志后缀（`aether.db.restore-journal.json`）。
pub const RESTORE_JOURNAL_SUFFIX: &str = ".restore-journal.json";
/// 恢复临时文件标记（同目录内 `aether.db.restore-tmp-<ts>`）。
pub const RESTORE_TEMP_INFIX: &str = ".restore-tmp-";

/// 备份台账记录（`backups` 表 + 文件元数据；`kind` = `internal`/`external`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupRecord {
    pub id: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub encrypted: bool,
    /// `internal`（应用 `backups` 目录）或 `external`（用户选择的外部目录，M3-04 登记）。
    pub kind: String,
    pub created_at: i64,
}

/// 恢复候选校验结果（D13 七步第 2 步）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateReport {
    pub path: PathBuf,
    pub size_bytes: u64,
    /// `schema_migrations` 最大版本（≤ 当前程序版本）。
    pub schema_version: i64,
}

/// 当前程序支持的最大迁移版本（`EMBEDDED_MIGRATIONS` 末项）。
pub fn program_schema_version() -> i64 {
    EMBEDDED_MIGRATIONS.last().map_or(0, |item| item.version)
}

/// 备份产物文件名（D13：命名含时间戳）。
pub fn backup_file_name(created_at_ms: i64) -> String {
    format!("aether-{created_at_ms}.db")
}

/// 在同毫秒冲突时追加备份 id 尾缀（保证不覆盖既有产物）。
pub fn backup_file_name_for(dest_dir: &Path, created_at_ms: i64, id: &str) -> String {
    let primary = backup_file_name(created_at_ms);
    if !dest_dir.join(&primary).exists() {
        return primary;
    }
    let tail: String = id
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("aether-{created_at_ms}-{tail}.db")
}

/// 备份所需空间：`(db_bytes + wal_bytes) × 1.2`（向上取整）。
pub fn required_space_bytes(db_bytes: u64, wal_bytes: u64) -> u64 {
    let total = u128::from(db_bytes) + u128::from(wal_bytes);
    let required =
        (total * u128::from(SPACE_MARGIN_NUMERATOR)).div_ceil(u128::from(SPACE_MARGIN_DENOMINATOR));
    u64::try_from(required).unwrap_or(u64::MAX)
}

/// 保留策略：按 `created_at` 升序找出超过 `retention` 的最旧记录（待删除）。
pub fn prune_plan(records: &[BackupRecord], retention: usize) -> Vec<BackupRecord> {
    if records.len() <= retention {
        return Vec::new();
    }
    let mut ordered: Vec<&BackupRecord> = records.iter().collect();
    ordered.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    let excess = ordered.len() - retention;
    ordered.into_iter().take(excess).cloned().collect()
}

/// 通过已打开的连接执行 `VACUUM INTO`（只读连接可用；目标文件必须不存在）。
///
/// 返回产物字节数。降级/安全模式（只读）下备份入口仍可用（D4）。
pub fn create_backup_file(conn: &Connection, dest: &Path) -> Result<u64, StoreError> {
    if dest.exists() {
        return Err(StoreError::BackupTargetExists {
            path: dest.to_path_buf(),
        });
    }
    let dest_str = dest.to_str().ok_or_else(|| StoreError::NonUtf8Path {
        path: dest.to_path_buf(),
    })?;
    conn.execute("VACUUM INTO ?1", [dest_str])?;
    Ok(fs::metadata(dest)?.len())
}

/// 校验恢复候选（D13 七步第 2 步）：
/// 只读打开 + `quick_check` + 读取 `schema_migrations` 最大版本；
/// 版本高于当前程序 → [`StoreError::SchemaNewerThanProgram`]（提示升级程序）。
pub fn validate_candidate(
    path: &Path,
    program_max_version: i64,
) -> Result<CandidateReport, StoreError> {
    let invalid = |reason: String| StoreError::BackupCandidateInvalid {
        path: path.to_path_buf(),
        reason,
    };
    if !path.is_file() {
        return Err(invalid("候选必须是已存在的文件".to_owned()));
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| invalid(format!("只读打开失败：{error}")))?;

    let check = quick_check(&connection);
    if !check.ok {
        return Err(invalid(format!("quick_check 失败：{}", check.summary())));
    }

    let version: i64 = match connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    ) {
        Ok(version) => version,
        Err(error) => return Err(invalid(format!("读取 schema_migrations 失败：{error}"))),
    };
    if version <= 0 {
        return Err(invalid(
            "候选无迁移记录（schema_migrations 为空，非本应用数据目录产物）".to_owned(),
        ));
    }
    if version > program_max_version {
        return Err(StoreError::SchemaNewerThanProgram {
            database: version,
            program: program_max_version,
        });
    }

    let size_bytes = fs::metadata(path)?.len();
    Ok(CandidateReport {
        path: path.to_path_buf(),
        size_bytes,
        schema_version: version,
    })
}

/// 恢复步骤（D13 七步中由本模块执行的 2–6；1=输入、7=重启+审计由调用方承担）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreStep {
    /// 2 校验候选。
    ValidateCandidate,
    /// 3 停写（关闭全部连接与写队列；启动路径回调为 no-op）。
    StopWrites,
    /// 4 现场保护（库/`-wal`/`-shm` 改名 `.pre-restore-<ts>`）。
    ProtectScene,
    /// 5 就位（候选复制到库路径；跨盘临时文件 + 原子 rename）。
    PlaceCandidate,
    /// 6 验收（重新打开 + `quick_check` + 迁移到当前版本）。
    Verify,
}

impl RestoreStep {
    /// 稳定步骤码（报告/证据）。
    pub const fn code(self) -> &'static str {
        match self {
            Self::ValidateCandidate => "validate_candidate",
            Self::StopWrites => "stop_writes",
            Self::ProtectScene => "protect_scene",
            Self::PlaceCandidate => "place_candidate",
            Self::Verify => "verify",
        }
    }
}

/// 恢复故障注入（测试/演练；生产恒为 [`RestoreFault::None`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreFault {
    /// 无注入。
    None,
    /// 在指定步骤完成后暂停（模拟 `kill -9` 中断窗口）。
    PauseAfter { step: RestoreStep, pause: Duration },
    /// 在指定步骤完成后返回注入错误（覆盖回滚路径）。
    FailAfter { step: RestoreStep, message: String },
}

/// 恢复请求（本模块执行 D13 第 2–6 步）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreRequest {
    pub db_path: PathBuf,
    pub candidate_path: PathBuf,
    pub program_max_version: i64,
    pub now_ms: i64,
    pub fault: RestoreFault,
}

/// 单步记录（报告/证据）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreStepRecord {
    /// 步骤码（[`RestoreStep::code`]）。
    pub step: String,
    pub ok: bool,
    pub at_ms: i64,
    pub duration_ms: u64,
    pub detail: String,
}

/// 现场保护记录（库/`-wal`/`-shm` 改名目标；`None` = 改名时文件不存在）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneProtection {
    pub db: Option<PathBuf>,
    pub wal: Option<PathBuf>,
    pub shm: Option<PathBuf>,
    pub ts: i64,
}

/// 恢复报告（DoD2 逐项断言的唯一来源；`applied=false` 表示已回滚）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub candidate: CandidateReport,
    pub scene: Option<SceneProtection>,
    /// 是否完成就位与验收（false 且无错误 = 未执行；错误路径不返回报告）。
    pub applied: bool,
    /// 是否发生现场回滚。
    pub rolled_back: bool,
    pub verified_schema_version: i64,
    pub total_ms: u64,
    pub steps: Vec<RestoreStepRecord>,
}

/// 恢复现场日志（`<db>.restore-journal.json`；`requested`/`placing`/`done`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreJournal {
    pub v: u32,
    /// `requested`（命令已接受，待重启执行）/ `placing`（执行中）/ `done`（验收通过）。
    pub status: String,
    pub db_path: String,
    pub candidate_path: String,
    pub requested_at: i64,
    /// 现场保护计划/结果（执行前写入计划，kill 后据此回滚）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<SceneProtection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 启动时对恢复现场日志的处理结果（DoD4；`Applied` 由启动序列执行七步 3–6）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RestoreStartupOutcome {
    /// 无待处理恢复（journal 不存在）。
    None,
    /// 待处理恢复已在本次启动完成（第 3–6 步）。
    Applied(RestoreReport),
    /// 上次恢复被 `kill -9` 中断：已按现场三件套回滚（旧库继续可用）。
    InterruptedRolledBack { detail: String },
    /// 待处理恢复执行失败：已回滚现场，应用以旧库继续。
    Failed { error: String },
    /// 上次恢复已完成（journal 状态 `done`），仅清理日志。
    Completed,
}

/// 恢复现场日志路径。
pub fn restore_journal_path(db_path: &Path) -> PathBuf {
    let file_name = db_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "aether.db".to_owned());
    db_path.with_file_name(format!("{file_name}{RESTORE_JOURNAL_SUFFIX}"))
}

/// 读取恢复现场日志（不存在 → `None`；损坏 → [`StoreError::RestoreJournalInvalid`]）。
pub fn read_restore_journal(db_path: &Path) -> Result<Option<RestoreJournal>, StoreError> {
    let path = restore_journal_path(db_path);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    match serde_json::from_str::<RestoreJournal>(&text) {
        Ok(journal) => Ok(Some(journal)),
        Err(error) => Err(StoreError::RestoreJournalInvalid {
            path,
            reason: format!("JSON 解析失败：{error}"),
        }),
    }
}

/// 原子写恢复现场日志（临时文件 + rename）。
pub fn write_restore_journal(db_path: &Path, journal: &RestoreJournal) -> Result<(), StoreError> {
    let path = restore_journal_path(db_path);
    let temp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(journal).map_err(|error| StoreError::Internal {
        reason: format!("恢复现场日志序列化失败：{error}"),
    })?;
    fs::write(&temp, text)?;
    fs::rename(&temp, &path)?;
    Ok(())
}

/// 清理恢复现场日志（成功后由调用方执行；保留 `.pre-restore-*` 现场文件）。
pub fn finalize_restore(db_path: &Path) -> Result<(), StoreError> {
    let path = restore_journal_path(db_path);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// 登记待处理恢复请求（`backup_restore` 命令经此落日志；执行在下次启动序列内）。
///
/// 已有 `requested`/`placing` 日志时拒绝重入（避免覆盖未完成的现场）。
pub fn request_restore(
    db_path: &Path,
    candidate_path: &Path,
    now_ms: i64,
) -> Result<RestoreJournal, StoreError> {
    if let Some(existing) = read_restore_journal(db_path)? {
        if existing.status == "requested" || existing.status == "placing" {
            return Err(StoreError::RestoreJournalInvalid {
                path: restore_journal_path(db_path),
                reason: format!("已有待处理恢复（status={}）", existing.status),
            });
        }
    }
    let journal = RestoreJournal {
        v: 1,
        status: "requested".to_owned(),
        db_path: db_path.to_string_lossy().to_string(),
        candidate_path: candidate_path.to_string_lossy().to_string(),
        requested_at: now_ms,
        scene: None,
        error: None,
    };
    write_restore_journal(db_path, &journal)?;
    Ok(journal)
}

/// 执行恢复（D13 第 2–6 步）。
///
/// `stop_writes` 为第 3 步回调：生产运行期由调用方关闭写队列/连接后调用；启动序列内
/// 无写者时传 `|| Ok(())`。任一步骤失败 → 回滚现场并返回错误（错误消息含回滚结果）。
pub fn execute_restore<F>(
    request: &RestoreRequest,
    stop_writes: F,
) -> Result<RestoreReport, StoreError>
where
    F: FnOnce() -> Result<(), StoreError>,
{
    let started = Instant::now();
    let mut steps: Vec<RestoreStepRecord> = Vec::new();

    // 2. 校验候选
    let candidate = step(
        &mut steps,
        RestoreStep::ValidateCandidate,
        || validate_candidate(&request.candidate_path, request.program_max_version),
        |report| {
            format!(
                "候选通过：schema v{} / {} 字节",
                report.schema_version, report.size_bytes
            )
        },
    )?;

    // 3. 停写
    step_unit(&mut steps, RestoreStep::StopWrites, stop_writes, |_| {
        "写队列/连接已关闭（无写者窗口）".to_owned()
    })?;

    // 现场保护计划先落 journal（kill 后按计划回滚）。
    let scene_plan = plan_scene(&request.db_path, request.now_ms);
    let mut journal = RestoreJournal {
        v: 1,
        status: "placing".to_owned(),
        db_path: request.db_path.to_string_lossy().to_string(),
        candidate_path: request.candidate_path.to_string_lossy().to_string(),
        requested_at: request.now_ms,
        scene: Some(scene_plan.clone()),
        error: None,
    };
    write_restore_journal(&request.db_path, &journal)?;

    // 4. 现场保护
    let scene = match step(
        &mut steps,
        RestoreStep::ProtectScene,
        || protect_scene(&request.db_path, &scene_plan),
        |scene| {
            format!(
                "库={} wal={} shm={}",
                display_opt(&scene.db),
                display_opt(&scene.wal),
                display_opt(&scene.shm)
            )
        },
    ) {
        Ok(scene) => scene,
        Err(error) => {
            let _ = finalize_restore(&request.db_path);
            return Err(error);
        }
    };

    if let Err(error) = maybe_fault(&request.fault, RestoreStep::ProtectScene) {
        return Err(abort_with_rollback(
            &request.db_path,
            &scene,
            with_step(RestoreStep::ProtectScene, error),
        ));
    }

    // 5. 就位
    if let Err(error) = step_unit(
        &mut steps,
        RestoreStep::PlaceCandidate,
        || place_candidate(&request.candidate_path, &request.db_path, request.now_ms),
        |_| format!("候选已就位：{}", request.db_path.display()),
    ) {
        return Err(abort_with_rollback(
            &request.db_path,
            &scene,
            with_step(RestoreStep::PlaceCandidate, error),
        ));
    }
    if let Err(error) = maybe_fault(&request.fault, RestoreStep::PlaceCandidate) {
        return Err(abort_with_rollback(
            &request.db_path,
            &scene,
            with_step(RestoreStep::PlaceCandidate, error),
        ));
    }

    // 6. 验收
    let verified_schema_version = match step(
        &mut steps,
        RestoreStep::Verify,
        || verify_restored(&request.db_path),
        |version| format!("验收通过：schema v{version} / quick_check ok"),
    ) {
        Ok(version) => version,
        Err(error) => {
            return Err(abort_with_rollback(
                &request.db_path,
                &scene,
                with_step(RestoreStep::Verify, error),
            ));
        }
    };

    journal.status = "done".to_owned();
    journal.scene = Some(scene.clone());
    write_restore_journal(&request.db_path, &journal)?;

    let total_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(RestoreReport {
        candidate,
        scene: Some(scene),
        applied: true,
        rolled_back: false,
        verified_schema_version,
        total_ms,
        steps,
    })
}

/// 启动序列入口：处理恢复现场日志（D13 第 3–6 步在无写者窗口执行；DoD4 中断回滚）。
///
/// - `requested`：执行恢复（`stop_writes` 天然满足——启动序列尚未打开存储）；
///   成功 → 清理 journal 并返回 [`RestoreStartupOutcome::Applied`]；失败 → 已回滚现场，
///   返回 [`RestoreStartupOutcome::Failed`]（应用以旧库继续）；
/// - `placing`：上次执行被中断（`kill -9`）→ 按 journal 现场回滚；
/// - `done`：上次已验收通过（进程在清理日志前退出）→ 仅清理日志。
pub fn recover_or_apply_pending_restore(
    db_path: &Path,
) -> Result<RestoreStartupOutcome, StoreError> {
    cleanup_restore_temps(db_path)?;
    let Some(journal) = read_restore_journal(db_path)? else {
        return Ok(RestoreStartupOutcome::None);
    };
    match journal.status.as_str() {
        "requested" => {
            let request = RestoreRequest {
                db_path: db_path.to_path_buf(),
                candidate_path: PathBuf::from(&journal.candidate_path),
                program_max_version: program_schema_version(),
                now_ms: now_ms(),
                fault: RestoreFault::None,
            };
            match execute_restore(&request, || Ok(())) {
                Ok(report) => {
                    finalize_restore(db_path)?;
                    Ok(RestoreStartupOutcome::Applied(report))
                }
                Err(error) => {
                    // execute_restore 失败路径已回滚现场并清理 journal；调用方（启动序列）记录诊断。
                    Ok(RestoreStartupOutcome::Failed {
                        error: error.to_string(),
                    })
                }
            }
        }
        "placing" => {
            let scene = journal.scene.clone().unwrap_or_default();
            let detail = match rollback_scene(db_path, &scene) {
                Ok(()) => format!(
                    "恢复被中断（status=placing）：已回滚现场三件套（库={}）",
                    display_opt(&scene.db)
                ),
                Err(error) => {
                    return Err(StoreError::Internal {
                        reason: format!("中断恢复回滚失败：{error}"),
                    });
                }
            };
            finalize_restore(db_path)?;
            Ok(RestoreStartupOutcome::InterruptedRolledBack { detail })
        }
        "done" => {
            finalize_restore(db_path)?;
            Ok(RestoreStartupOutcome::Completed)
        }
        other => Err(StoreError::RestoreJournalInvalid {
            path: restore_journal_path(db_path),
            reason: format!("未知状态：{other}"),
        }),
    }
}

fn plan_scene(db_path: &Path, ts: i64) -> SceneProtection {
    let rename = |source: &Path| -> Option<PathBuf> {
        if source.exists() {
            Some(renamed_path(source, ts))
        } else {
            None
        }
    };
    SceneProtection {
        db: rename(db_path),
        wal: rename(&sidecar_path(db_path, "-wal")),
        shm: rename(&sidecar_path(db_path, "-shm")),
        ts,
    }
}

fn renamed_path(source: &Path, ts: i64) -> PathBuf {
    let name = source
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "aether.db".to_owned());
    source.with_file_name(format!("{name}{PRE_RESTORE_INFIX}{ts}"))
}

fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn protect_scene(db_path: &Path, plan: &SceneProtection) -> Result<SceneProtection, StoreError> {
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (source, dest) in scene_pairs(db_path, plan) {
        match fs::rename(&source, &dest) {
            Ok(()) => done.push((dest, source)),
            Err(error) => {
                // 部分改名失败：回滚已完成的改名，保持现场原样。
                for (dest, source) in done.iter().rev() {
                    let _ = fs::rename(dest, source);
                }
                return Err(StoreError::Io(error));
            }
        }
    }
    Ok(plan.clone())
}

/// 现场三件套「当前路径 → 改名目标」对有值项。
fn scene_pairs(db_path: &Path, scene: &SceneProtection) -> Vec<(PathBuf, PathBuf)> {
    let candidates = [
        (db_path.to_path_buf(), scene.db.clone()),
        (sidecar_path(db_path, "-wal"), scene.wal.clone()),
        (sidecar_path(db_path, "-shm"), scene.shm.clone()),
    ];
    candidates
        .into_iter()
        .filter_map(|(source, dest)| dest.map(|dest| (source, dest)))
        .collect()
}

fn display_opt(path: &Option<PathBuf>) -> String {
    match path {
        Some(path) => path.display().to_string(),
        None => "∅".to_owned(),
    }
}

fn place_candidate(candidate: &Path, db_path: &Path, ts: i64) -> Result<(), StoreError> {
    let name = db_path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "aether.db".to_owned());
    let temp = db_path.with_file_name(format!("{name}{RESTORE_TEMP_INFIX}{ts}"));
    if temp.exists() {
        fs::remove_file(&temp)?;
    }
    fs::copy(candidate, &temp)?;
    // FlushFileBuffers 需要写句柄（Windows：只读句柄 sync_all 会拒绝访问）。
    fs::OpenOptions::new().write(true).open(&temp)?.sync_all()?;
    fs::rename(&temp, db_path)?;
    Ok(())
}

fn verify_restored(db_path: &Path) -> Result<i64, StoreError> {
    let store = Store::open(db_path)?;
    if store.is_safe_mode() {
        return Err(StoreError::BackupCandidateInvalid {
            path: db_path.to_path_buf(),
            reason: format!(
                "验收打开进入安全模式（只读）：{}",
                store.safe_mode_reason().unwrap_or("quick_check 失败")
            ),
        });
    }
    let check = store.quick_check();
    if !check.ok {
        return Err(StoreError::BackupCandidateInvalid {
            path: db_path.to_path_buf(),
            reason: format!("验收 quick_check 失败：{}", check.summary()),
        });
    }
    let version = store
        .applied_migrations()?
        .last()
        .map(|record| record.version)
        .unwrap_or(0);
    Ok(version)
}

/// 回滚现场（D13 第 6 步失败路径）：删除就位产物与临时文件，还原三件套。
pub fn rollback_scene(db_path: &Path, scene: &SceneProtection) -> Result<(), StoreError> {
    if db_path.exists() {
        fs::remove_file(db_path)?;
    }
    cleanup_restore_temps(db_path)?;
    for (dest, renamed) in [
        (db_path.to_path_buf(), scene.db.clone()),
        (sidecar_path(db_path, "-wal"), scene.wal.clone()),
        (sidecar_path(db_path, "-shm"), scene.shm.clone()),
    ] {
        if let Some(renamed) = renamed {
            if renamed.exists() {
                if dest.exists() {
                    fs::remove_file(&dest)?;
                }
                fs::rename(&renamed, &dest)?;
            }
        }
    }
    Ok(())
}

fn abort_with_rollback(db_path: &Path, scene: &SceneProtection, error: StoreError) -> StoreError {
    match rollback_scene(db_path, scene) {
        Ok(()) => {
            let _ = finalize_restore(db_path);
            StoreError::Internal {
                reason: format!("恢复失败，已回滚现场（旧库三件套完整）：{error}"),
            }
        }
        Err(rollback_error) => StoreError::Internal {
            reason: format!("恢复失败且现场回滚失败：{error}；回滚错误：{rollback_error}"),
        },
    }
}

/// 为错误附加步骤上下文（诊断定位，不改变错误分类）。
fn with_step(step: RestoreStep, error: StoreError) -> StoreError {
    StoreError::Internal {
        reason: format!("第 {} 步失败：{error}", step.code()),
    }
}

/// 清理库目录内的恢复临时文件（`*.restore-tmp-*`）。
pub fn cleanup_restore_temps(db_path: &Path) -> Result<(), StoreError> {
    let Some(dir) = db_path.parent() else {
        return Ok(());
    };
    let name = db_path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let prefix = format!("{name}{RESTORE_TEMP_INFIX}");
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StoreError::Io(error)),
    };
    for item in listing {
        let item = item?;
        if item.file_name().to_string_lossy().starts_with(&prefix) {
            fs::remove_file(item.path())?;
        }
    }
    Ok(())
}

fn maybe_fault(fault: &RestoreFault, step: RestoreStep) -> Result<(), StoreError> {
    match fault {
        RestoreFault::None => Ok(()),
        RestoreFault::PauseAfter { step: at, pause } if *at == step => {
            std::thread::sleep(*pause);
            Ok(())
        }
        RestoreFault::FailAfter { step: at, message } if *at == step => Err(StoreError::Internal {
            reason: format!("故障注入（{}）：{message}", step.code()),
        }),
        _ => Ok(()),
    }
}

fn step<T>(
    steps: &mut Vec<RestoreStepRecord>,
    step: RestoreStep,
    operation: impl FnOnce() -> Result<T, StoreError>,
    detail_ok: impl FnOnce(&T) -> String,
) -> Result<T, StoreError> {
    let started = Instant::now();
    let at_ms = now_ms();
    match operation() {
        Ok(value) => {
            steps.push(RestoreStepRecord {
                step: step.code().to_owned(),
                ok: true,
                at_ms,
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                detail: detail_ok(&value),
            });
            Ok(value)
        }
        Err(error) => {
            steps.push(RestoreStepRecord {
                step: step.code().to_owned(),
                ok: false,
                at_ms,
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                detail: error.to_string(),
            });
            Err(error)
        }
    }
}

fn step_unit(
    steps: &mut Vec<RestoreStepRecord>,
    step: RestoreStep,
    operation: impl FnOnce() -> Result<(), StoreError>,
    detail_ok: impl FnOnce(&()) -> String,
) -> Result<(), StoreError> {
    self::step(steps, step, operation, detail_ok)
}

fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn record(id: &str, created_at: i64) -> BackupRecord {
        BackupRecord {
            id: id.to_owned(),
            path: PathBuf::from(format!("{id}.db")),
            size_bytes: 1,
            encrypted: false,
            kind: "internal".to_owned(),
            created_at,
        }
    }

    #[test]
    fn required_space_is_ceiled_by_1_2() {
        assert_eq!(required_space_bytes(0, 0), 0);
        assert_eq!(required_space_bytes(5, 0), 6);
        assert_eq!(required_space_bytes(10, 0), 12);
        assert_eq!(required_space_bytes(10, 10), 24);
        assert_eq!(required_space_bytes(1, 0), 2, "向上取整");
        assert_eq!(required_space_bytes(u64::MAX, u64::MAX), u64::MAX, "饱和");
    }

    #[test]
    fn prune_plan_keeps_newest_ten() {
        let records: Vec<BackupRecord> = (0..12)
            .map(|index| record(&format!("b{index:02}"), index))
            .collect();
        let plan = prune_plan(&records, 10);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].created_at, 0);
        assert_eq!(plan[1].created_at, 1);
        assert!(prune_plan(&records[..10], 10).is_empty());
    }

    #[test]
    fn backup_file_name_contains_timestamp_and_avoids_collision() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(
            backup_file_name(1_700_000_000_000),
            "aether-1700000000000.db"
        );
        let name = backup_file_name_for(dir.path(), 42, "01J00000000000000000000ABCD");
        assert_eq!(name, "aether-42.db");
        std::fs::write(dir.path().join("aether-42.db"), b"x").unwrap();
        let name = backup_file_name_for(dir.path(), 42, "01J00000000000000000000ABCD");
        assert_eq!(name, "aether-42-00ABCD.db");
    }

    #[test]
    fn journal_roundtrip_and_finalize() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("aether.db");
        std::fs::write(&db, b"db").unwrap();
        assert!(read_restore_journal(&db).unwrap().is_none());

        let scene = plan_scene(&db, 7);
        assert!(scene.db.is_some());
        let journal = RestoreJournal {
            v: 1,
            status: "placing".to_owned(),
            db_path: db.to_string_lossy().to_string(),
            candidate_path: "cand.db".to_owned(),
            requested_at: 7,
            scene: Some(scene),
            error: None,
        };
        write_restore_journal(&db, &journal).unwrap();
        let read = read_restore_journal(&db).unwrap().unwrap();
        assert_eq!(read.status, "placing");
        assert!(read.scene.unwrap().db.is_some());
        finalize_restore(&db).unwrap();
        assert!(read_restore_journal(&db).unwrap().is_none());
    }

    #[test]
    fn candidate_validation_rejects_missing_and_plain_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("missing.db");
        let error = validate_candidate(&missing, 2).unwrap_err();
        assert_eq!(error.code(), "backup_candidate_invalid");

        let plain = dir.path().join("plain.db");
        std::fs::write(&plain, b"not a database").unwrap();
        let error = validate_candidate(&plain, 2).unwrap_err();
        assert_eq!(error.code(), "backup_candidate_invalid");
    }
}
