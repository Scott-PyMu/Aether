//! `aetherd` 预 `initialize` 致命启动检查（ADR-029 v0.4 §3.5-4；M9-03 DoD-5）。
//!
//! 顺序（与 ADR 冻结顺序一致）：
//! 1. **解析数据目录**（`AETHER_DATA_DIR` → 指针文件 → 缺省；仍受 A4 约束）；
//! 2. **A4 检测**（同步盘/云目录 → **致命**，无迁移入口/无覆盖开关）；
//! 3. **OS 独占锁** `<data_dir>/.aetherd.lock`（handle-based；崩溃由 OS 释放）；
//! 4. **DB 打开**（不含 `quick_check`）→ **已应用迁移 checksum 校验**
//!    （不匹配 → 致命；M9 不执行迁移）。
//!
//! 失败语义（ADR-029 §4.3 退出码表）：
//!
//! | 失败 | `aether_code` | `business_code` | 退出码 |
//! |---|---|---|---:|
//! | 数据目录解析失败 | `internal` | — | 24 |
//! | A4 同步盘 | `startup_blocked` | `sync_dir_blocked` | 21 |
//! | 数据目录锁被占用 | `internal` | `data_dir_locked` | 20 |
//! | DB 无法打开 / schema 高于程序 | `internal` | `db_open_failed` | 22 |
//! | 已应用迁移 checksum 不符 | `internal` | `schema_checksum_mismatch` | 22 |
//!
//! `quick_check` 失败**不属**本模块：DB 可打开但完整性失败 → 只读安全模式
//! （`persist_degraded`/S-08），由预热阶段 `Store::open` 处理，**不退出**。
//! 退出码 23（迁移执行失败）为保留码位，M9 不触发。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aether_app::ipc::BusinessCode;
use aether_app::startup::{StartupGate, StartupPhase};
use aether_store::{PreflightReport, StoreError};

use crate::lock::DataDirLock;

/// 正常退出。
pub const EXIT_OK: u8 = 0;
/// 协议/参数初始化失败（`--stdio` 缺失、未知参数）。
pub const EXIT_USAGE: u8 = 10;
/// 数据目录锁被占用（`data_dir_locked`）。
pub const EXIT_DATA_DIR_LOCKED: u8 = 20;
/// A4 同步盘/数据目录不可用（`sync_dir_blocked`）。
pub const EXIT_SYNC_DIR_BLOCKED: u8 = 21;
/// DB 无法打开 / 已应用迁移 checksum 不符（**不含** `quick_check` 失败）。
pub const EXIT_DB_OPEN_FAILED: u8 = 22;
/// 迁移执行失败（**保留码位**；M9 不触发）。
pub const EXIT_MIGRATION_FAILED: u8 = 23;
/// 内部错误。
pub const EXIT_INTERNAL: u8 = 24;

/// 数据目录内日志文件（ADR-029 §3.5-3：`<data_dir>/logs/aetherd.log`）。
pub const LOG_DIR_NAME: &str = "logs";
pub const LOG_FILE_NAME: &str = "aetherd.log";

/// 致命启动失败（首帧 error + 退出码必须同时存在）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapError {
    /// 数据目录无法解析（`startup_get.phase=blocked_error` 的对等启动失败）。
    DataDirUnavailable { message: String },
    /// A4 同步盘/云目录：致命，无迁移入口。
    SyncDirBlocked { message: String },
    /// 数据目录锁被其他核心占用。
    DataDirLocked { message: String },
    /// DB 无法打开（缺失/权限/加密头损坏）或 schema 高于程序。
    DbOpenFailed { message: String },
    /// 已应用迁移 checksum 与内嵌文件不符（`R11`）。
    SchemaChecksumMismatch { message: String },
}

impl BootstrapError {
    /// `error.data.aether_code`（只取 32 枚举）。
    pub fn aether_code(&self) -> &'static str {
        match self {
            Self::SyncDirBlocked { .. } => "startup_blocked",
            Self::DataDirUnavailable { .. }
            | Self::DataDirLocked { .. }
            | Self::DbOpenFailed { .. }
            | Self::SchemaChecksumMismatch { .. } => "internal",
        }
    }

    /// `error.data.business_code`（只取 9 项业务码；无对应业务码时为 `None`）。
    pub fn business_code(&self) -> Option<BusinessCode> {
        match self {
            Self::DataDirUnavailable { .. } => None,
            Self::SyncDirBlocked { .. } => Some(BusinessCode::SyncDirBlocked),
            Self::DataDirLocked { .. } => Some(BusinessCode::DataDirLocked),
            Self::DbOpenFailed { .. } => Some(BusinessCode::DbOpenFailed),
            Self::SchemaChecksumMismatch { .. } => Some(BusinessCode::SchemaChecksumMismatch),
        }
    }

    /// 进程退出码（ADR-029 §4.3）。
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::DataDirUnavailable { .. } => EXIT_INTERNAL,
            Self::SyncDirBlocked { .. } => EXIT_SYNC_DIR_BLOCKED,
            Self::DataDirLocked { .. } => EXIT_DATA_DIR_LOCKED,
            Self::DbOpenFailed { .. } | Self::SchemaChecksumMismatch { .. } => EXIT_DB_OPEN_FAILED,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::DataDirUnavailable { message }
            | Self::SyncDirBlocked { message }
            | Self::DataDirLocked { message }
            | Self::DbOpenFailed { message }
            | Self::SchemaChecksumMismatch { message } => message,
        }
    }
}

/// 启动成功后的上下文（锁与启动门在进程生命周期内持有）。
pub struct Bootstrap {
    pub gate: Arc<StartupGate>,
    pub data_dir: PathBuf,
    pub log_path: PathBuf,
    pub preflight: PreflightReport,
    lock: DataDirLock,
}

impl Bootstrap {
    /// 锁文件路径（证据/诊断）。
    pub fn lock_path(&self) -> &Path {
        self.lock.path()
    }
}

/// 数据目录内日志路径（`<data_dir>/logs/aetherd.log`）。
pub fn log_path_for(data_dir: &Path) -> PathBuf {
    data_dir.join(LOG_DIR_NAME).join(LOG_FILE_NAME)
}

/// 执行预 `initialize` 致命启动检查。
pub fn bootstrap() -> Result<Bootstrap, (BootstrapError, PathBuf)> {
    let gate = StartupGate::bootstrap();
    bootstrap_with_gate(Arc::new(gate))
}

/// 以给定启动门执行检查（单测/E2E 注入；生产入口见 [`bootstrap`]）。
///
/// 顺序与 ADR-029 §3.5-4 一致：**解析数据目录 → 取 OS 独占锁 → A4 判定 → DB 打开 →
/// checksum 校验**。锁先于 A4 判定，保证「同目录第二个核心」优先得到 `data_dir_locked`
/// （20），而不是取决于该目录是否位于同步盘。
pub fn bootstrap_with_gate(gate: Arc<StartupGate>) -> Result<Bootstrap, (BootstrapError, PathBuf)> {
    let snapshot = gate.snapshot();
    let raw_dir = PathBuf::from(&snapshot.data_dir);
    let log_path = if raw_dir.as_os_str().is_empty() {
        PathBuf::from(LOG_FILE_NAME)
    } else {
        log_path_for(&raw_dir)
    };

    // 1) 解析失败（无数据目录）→ 24（无法取锁/无法写首帧日志路径）。
    if raw_dir.as_os_str().is_empty() || snapshot.phase == StartupPhase::BlockedError {
        let message = snapshot
            .message
            .unwrap_or_else(|| "数据目录解析失败（未提供原因）".to_owned());
        return Err((BootstrapError::DataDirUnavailable { message }, log_path));
    }

    // 2) OS 独占锁（handle-based；崩溃由 OS 释放）。
    let data_dir = raw_dir;
    let lock = DataDirLock::acquire(&data_dir)
        .map_err(|message| (BootstrapError::DataDirLocked { message }, log_path.clone()))?;

    // 3) A4 判定（致命：同步盘/云目录；无迁移入口）。
    if snapshot.phase == StartupPhase::BlockedSyncDir {
        let reasons = snapshot
            .detection
            .as_ref()
            .map(|report| report.reasons.join("；"))
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "A4 同步盘/云目录检测命中".to_owned());
        return Err((
            BootstrapError::SyncDirBlocked {
                message: format!(
                    "数据目录位于同步盘/云目录，已按 A4 拒绝启动：{reasons}；\
                     请将数据目录迁移到本地磁盘后重启（M9 不提供迁移入口）"
                ),
            },
            log_path,
        ));
    }

    let preflight = aether_store::preflight(data_dir.join("aether.db")).map_err(|error| {
        let mapped = match &error {
            StoreError::MigrationChecksumMismatch {
                version,
                name,
                recorded,
                recomputed,
            } => BootstrapError::SchemaChecksumMismatch {
                message: format!(
                    "已应用迁移与内嵌文件 checksum 不符（v{version} {name}）：\
                     记录 {recorded} ≠ 重算 {recomputed}；拒绝启动（防历史被改）"
                ),
            },
            _ => BootstrapError::DbOpenFailed {
                message: format!("数据库无法打开（{error}）；请从备份恢复或检查数据目录权限"),
            },
        };
        (mapped, log_path.clone())
    })?;

    tracing::info!(
        data_dir = %data_dir.display(),
        lock = %lock.path().display(),
        schema_version = preflight.schema_version,
        applied = preflight.applied.len(),
        existed = preflight.existed,
        "预初始化致命启动检查通过（数据目录/锁/A4/DB 打开/迁移 checksum）"
    );

    // M3-05/D10：安全级别启动探针（A3 自检：写 → 读 → 删；失败 = degraded）。
    // 与迁移期 Tauri 壳同口径，保证 `startup_get.security_level` 契约一致（G2）。
    let level = aether_app::security_level::probe();
    tracing::info!(level = %level.level, detail = %level.detail, "安全级别探针完成");
    gate.set_security_level(level);

    Ok(Bootstrap {
        gate,
        data_dir,
        log_path,
        preflight,
        lock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_app::startup::detect::{
        DetectionContext, NetworkDriveSource, PlatformKind, RegistrySource, ReparseSource,
    };
    use std::collections::BTreeMap;

    /// 注入检测上下文（不触碰真实 OneDrive 配置/注册表）。
    fn sample_context(platform: PlatformKind, home: Option<PathBuf>) -> DetectionContext {
        DetectionContext {
            platform,
            home,
            env: BTreeMap::new(),
            registry: RegistrySource::Unavailable,
            reparse: ReparseSource::Unavailable,
            network_drives: NetworkDriveSource::Unavailable,
        }
    }

    fn gate_at(dir: &Path, context: DetectionContext) -> Arc<StartupGate> {
        Arc::new(StartupGate::bootstrap_at(
            dir.to_path_buf(),
            aether_app::startup::DataDirSource::EnvOverride,
            context,
            None,
        ))
    }

    #[test]
    fn exit_codes_and_business_codes_are_frozen() {
        let cases = [
            (
                BootstrapError::DataDirUnavailable {
                    message: "x".to_owned(),
                },
                EXIT_INTERNAL,
                "internal",
                None,
            ),
            (
                BootstrapError::SyncDirBlocked {
                    message: "x".to_owned(),
                },
                EXIT_SYNC_DIR_BLOCKED,
                "startup_blocked",
                Some(BusinessCode::SyncDirBlocked),
            ),
            (
                BootstrapError::DataDirLocked {
                    message: "x".to_owned(),
                },
                EXIT_DATA_DIR_LOCKED,
                "internal",
                Some(BusinessCode::DataDirLocked),
            ),
            (
                BootstrapError::DbOpenFailed {
                    message: "x".to_owned(),
                },
                EXIT_DB_OPEN_FAILED,
                "internal",
                Some(BusinessCode::DbOpenFailed),
            ),
            (
                BootstrapError::SchemaChecksumMismatch {
                    message: "x".to_owned(),
                },
                EXIT_DB_OPEN_FAILED,
                "internal",
                Some(BusinessCode::SchemaChecksumMismatch),
            ),
        ];
        for (error, exit, aether, business) in cases {
            assert_eq!(error.exit_code(), exit, "{error:?}");
            assert_eq!(error.aether_code(), aether, "{error:?}");
            assert_eq!(error.business_code(), business, "{error:?}");
        }
        assert_eq!(EXIT_MIGRATION_FAILED, 23, "保留码位（M9 不触发）");
    }

    #[test]
    fn log_path_is_inside_data_dir() {
        let path = log_path_for(Path::new(r"C:\Aether"));
        assert!(path.ends_with(Path::new(LOG_DIR_NAME).join(LOG_FILE_NAME)));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("aetherd.log")
        );
    }

    #[test]
    fn fresh_directory_bootstraps_and_holds_the_lock() {
        let dir = tempfile::tempdir().expect("临时目录");
        let context = sample_context(PlatformKind::Other, None);
        let bootstrap = bootstrap_with_gate(gate_at(dir.path(), context)).expect("预检应通过");
        assert_eq!(bootstrap.preflight.schema_version, 0);
        assert!(bootstrap.lock_path().exists());
        // 同目录第二个核心必须被锁拒绝（data_dir_locked / 20）。
        let second = bootstrap_with_gate(gate_at(
            dir.path(),
            sample_context(PlatformKind::Other, None),
        ));
        match second {
            Err((BootstrapError::DataDirLocked { .. }, _)) => {}
            Err((other, _)) => panic!("期望 DataDirLocked，实际 {other:?}"),
            Ok(_) => panic!("第二个 daemon 不得取得锁"),
        }
        drop(bootstrap);
    }

    #[test]
    fn checksum_mismatch_is_mapped_to_fatal_22() {
        let dir = tempfile::tempdir().expect("临时目录");
        let db = dir.path().join("aether.db");
        {
            let _store = aether_store::Store::open(&db).expect("建库");
        }
        {
            let conn = rusqlite::Connection::open(&db).expect("直连");
            conn.execute(
                "UPDATE schema_migrations SET checksum = 'tampered' WHERE version = 1",
                [],
            )
            .expect("篡改");
        }
        match bootstrap_with_gate(gate_at(
            dir.path(),
            sample_context(PlatformKind::Other, None),
        )) {
            Err((error, log_path)) => {
                assert_eq!(error.exit_code(), EXIT_DB_OPEN_FAILED);
                assert_eq!(
                    error.business_code(),
                    Some(BusinessCode::SchemaChecksumMismatch)
                );
                assert_eq!(
                    log_path.file_name().and_then(|n| n.to_str()),
                    Some("aetherd.log")
                );
            }
            Ok(_) => panic!("checksum 不符必须致命"),
        }
    }

    #[test]
    fn corrupt_database_is_mapped_to_db_open_failed() {
        let dir = tempfile::tempdir().expect("临时目录");
        std::fs::write(dir.path().join("aether.db"), b"not a sqlite file").expect("写坏库");
        match bootstrap_with_gate(gate_at(
            dir.path(),
            sample_context(PlatformKind::Other, None),
        )) {
            Err((error, _)) => {
                assert_eq!(error.exit_code(), EXIT_DB_OPEN_FAILED);
                assert_eq!(error.business_code(), Some(BusinessCode::DbOpenFailed));
            }
            Ok(_) => panic!("坏库必须致命"),
        }
    }

    #[test]
    fn lock_is_acquired_before_a4_check() {
        // ADR-029 §3.5-4 顺序：解析数据目录 → 取 OS 锁 → A4 → DB。
        // 即使目录命中 A4，第二个核心也必须先得到 data_dir_locked（20），
        // 而不是随 A4 判定结果变化。
        let home = tempfile::tempdir().expect("临时 home");
        let data_dir = home
            .path()
            .join("Library")
            .join("Mobile Documents")
            .join("Aether");
        let _held = DataDirLock::acquire(&data_dir).expect("测试先持有锁");
        match bootstrap_with_gate(gate_at(
            &data_dir,
            sample_context(PlatformKind::MacOs, Some(home.path().to_path_buf())),
        )) {
            Err((BootstrapError::DataDirLocked { .. }, _)) => {}
            Err((other, _)) => panic!("期望先命中 DataDirLocked，实际 {other:?}"),
            Ok(_) => panic!("锁被占用时不得启动"),
        }
    }

    #[test]
    fn a4_reject_is_fatal_21_with_sync_dir_blocked() {
        let home = tempfile::tempdir().expect("临时 home");
        let data_dir = home
            .path()
            .join("Library")
            .join("Mobile Documents")
            .join("Aether");
        // 注入 macOS 检测上下文：iCloud 容器路径前缀命中即 Reject（跨宿主可断言）。
        let gate = gate_at(
            &data_dir,
            sample_context(PlatformKind::MacOs, Some(home.path().to_path_buf())),
        );
        match bootstrap_with_gate(gate) {
            Err((error, _)) => {
                assert_eq!(error.exit_code(), EXIT_SYNC_DIR_BLOCKED);
                assert_eq!(error.business_code(), Some(BusinessCode::SyncDirBlocked));
                assert_eq!(error.aether_code(), "startup_blocked");
            }
            Ok(_) => panic!("A4 命中必须致命"),
        }
    }
}
