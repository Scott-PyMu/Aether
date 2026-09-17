//! M1-06 启动序列：数据目录检测 → 拒绝启动门（迁移/退出）→ 迁移执行 → 单实例。
//!
//! 启动顺序（设计 D1「启动序列」）：单实例锁 → 数据目录检测（A4）→ …；本模块实现
//! 前两步的状态与迁移流；库打开/孤儿清理/迁移/预热随 M1-05/M2 接入。
//!
//! 拒绝启动口径（A4 降级路径，评审 #9）：检测命中 → UI 只呈现「迁移到本地目录」与
//! 「退出」两个动作；命令层同时阻断其余业务命令（[`StartupGate::ensure_ready`]），
//! 主界面不可达；不提供任何「仍要在此目录运行」的覆盖开关。

pub mod detect;
pub mod migrate;
pub mod pointer;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::Serialize;

use crate::ipc::error::{IpcError, IpcErrorCode};
use crate::ipc::path;

use detect::{detect_data_dir, DetectionContext, DetectionReport};
use migrate::{migrate_data_dir, MigrationErrorKind, MigrationOutcome};

pub use detect::{CheckOutcome, PlatformKind, Precision, Verdict};
pub use migrate::CopiedEntry;
pub use pointer::{DATA_DIR_ENV, POINTER_ENV};

/// 启动阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupPhase {
    /// 检测通过：正常进入主界面（后续库打开/迁移/预热由 M1-05/M2 接入）。
    Ready,
    /// 命中同步盘拒绝清单（A4）：仅迁移/退出可达。
    BlockedSyncDir,
    /// 数据目录解析失败等硬错误：仅退出可达。
    BlockedError,
}

impl StartupPhase {
    pub fn is_blocked(self) -> bool {
        self != Self::Ready
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::BlockedSyncDir => "blocked_sync_dir",
            Self::BlockedError => "blocked_error",
        }
    }
}

/// 数据目录来源（诊断与证据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataDirSource {
    EnvOverride,
    Pointer,
    Default,
    Migrated,
}

/// 启动状态快照（IPC `startup_get` / 探针输出）。
#[derive(Debug, Clone, Serialize)]
pub struct StartupSnapshot {
    pub phase: StartupPhase,
    pub data_dir: String,
    pub data_dir_source: DataDirSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detection: Option<DetectionReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration: Option<MigrationOutcome>,
}

struct GateInner {
    phase: StartupPhase,
    data_dir: PathBuf,
    source: DataDirSource,
    detection: Option<DetectionReport>,
    message: Option<String>,
    migrating: bool,
}

/// 启动门：解析数据目录、执行 A4 检测、容纳迁移状态机。
pub struct StartupGate {
    inner: Mutex<GateInner>,
    pointer_file: Option<PathBuf>,
    context: DetectionContext,
}

impl StartupGate {
    /// 生产入口：读取宿主环境与真实检测上下文。
    pub fn bootstrap() -> Self {
        Self::bootstrap_with(DetectionContext::native(), pointer::pointer_file_path())
    }

    /// 可注入入口（样本测试 / E2E 隔离）。
    pub fn bootstrap_with(context: DetectionContext, pointer_file: Option<PathBuf>) -> Self {
        match resolve_candidate(pointer_file.as_deref()) {
            Ok((data_dir, source)) => Self::from_resolved(data_dir, source, context, pointer_file),
            Err((data_dir, message)) => {
                Self::blocked_error(data_dir, message, context, pointer_file)
            }
        }
    }

    /// 直接指定候选目录（样本测试 / 受控部署），跳过环境与指针解析。
    pub fn bootstrap_at(
        candidate: PathBuf,
        source: DataDirSource,
        context: DetectionContext,
        pointer_file: Option<PathBuf>,
    ) -> Self {
        Self::from_resolved(candidate, source, context, pointer_file)
    }

    fn from_resolved(
        data_dir: PathBuf,
        source: DataDirSource,
        context: DetectionContext,
        pointer_file: Option<PathBuf>,
    ) -> Self {
        let detection = detect_data_dir(&data_dir, &context);
        let phase = if detection.is_reject() {
            StartupPhase::BlockedSyncDir
        } else {
            StartupPhase::Ready
        };
        Self {
            inner: Mutex::new(GateInner {
                phase,
                data_dir,
                source,
                detection: Some(detection),
                message: None,
                migrating: false,
            }),
            pointer_file,
            context,
        }
    }

    fn blocked_error(
        data_dir: PathBuf,
        message: String,
        context: DetectionContext,
        pointer_file: Option<PathBuf>,
    ) -> Self {
        Self {
            inner: Mutex::new(GateInner {
                phase: StartupPhase::BlockedError,
                data_dir,
                source: DataDirSource::Default,
                detection: None,
                message: Some(message),
                migrating: false,
            }),
            pointer_file,
            context,
        }
    }

    pub fn snapshot(&self) -> StartupSnapshot {
        let inner = self.lock();
        StartupSnapshot {
            phase: inner.phase,
            data_dir: inner.data_dir.to_string_lossy().to_string(),
            data_dir_source: inner.source,
            detection: inner.detection.clone(),
            message: inner.message.clone(),
            migration: None,
        }
    }

    pub fn snapshot_json(&self) -> Result<serde_json::Value, IpcError> {
        serde_json::to_value(self.snapshot())
            .map_err(|error| IpcError::internal(format!("启动状态序列化失败：{error}")))
    }

    /// 业务命令前置：未通过启动自检时阻断（主界面不可达的命令层兜底）。
    pub fn ensure_ready(&self) -> Result<(), IpcError> {
        let inner = self.lock();
        match inner.phase {
            StartupPhase::Ready => Ok(()),
            StartupPhase::BlockedSyncDir => Err(IpcError::startup_blocked(
                "数据目录位于同步盘/云目录，已按 A4 拒绝启动；仅可「迁移到本地目录」或「退出」",
            )),
            StartupPhase::BlockedError => Err(IpcError::startup_blocked(format!(
                "启动自检未通过：{}",
                inner.message.as_deref().unwrap_or("未知错误")
            ))),
        }
    }

    /// 执行迁移：复制 → 校验 → 原子替换 → 写指针锁定新目录。
    pub fn migrate(&self, raw_target: &str) -> Result<serde_json::Value, IpcError> {
        // canonicalize 后 Windows 可能带 `\\?\` verbatim 前缀，统一剥离后再检测/比较。
        let target = detect::resolve_existing_prefix(&path::validate_migration_target(raw_target)?);
        let pointer_file = self.pointer_file.clone().ok_or_else(|| {
            IpcError::new(
                IpcErrorCode::MigrationFailed,
                "无法确定数据目录指针位置，迁移结果无法锁定；请检查本地配置目录",
            )
        })?;
        pointer::check_writable(&pointer_file).map_err(|message| {
            IpcError::new(
                IpcErrorCode::MigrationFailed,
                format!("指针文件不可写，迁移前置检查失败：{message}"),
            )
        })?;

        {
            let mut inner = self.lock();
            if inner.phase != StartupPhase::BlockedSyncDir {
                return Err(IpcError::invalid_value(match inner.phase {
                    StartupPhase::Ready => "当前数据目录已通过检测，无需迁移",
                    _ => "当前启动状态不允许迁移",
                }));
            }
            if inner.migrating {
                return Err(IpcError::new(
                    IpcErrorCode::MigrationFailed,
                    "已有迁移任务在执行",
                ));
            }
            inner.migrating = true;
        }
        let _guard = MigratingGuard { gate: self };

        let source = self.lock().data_dir.clone();
        let outcome =
            migrate_data_dir(&source, &target, &self.context).map_err(map_migration_error)?;

        let detection = detect_data_dir(&target, &self.context);
        if detection.is_reject() {
            return Err(IpcError::path_rejected(format!(
                "迁移目标在复制后复核中命中同步盘拒绝清单：{}",
                detection.reasons.join("；")
            )));
        }

        pointer::write_pointer(&pointer_file, &target).map_err(|message| {
            IpcError::new(
                IpcErrorCode::MigrationFailed,
                format!("迁移已完成但锁定新目录失败：{message}"),
            )
        })?;

        {
            let mut inner = self.lock();
            inner.phase = StartupPhase::Ready;
            inner.data_dir = target;
            inner.source = DataDirSource::Migrated;
            inner.detection = Some(detection);
            inner.message = None;
            inner.migrating = false;
        }

        let mut snapshot = self.snapshot();
        snapshot.migration = Some(outcome);
        serde_json::to_value(snapshot)
            .map_err(|error| IpcError::internal(format!("迁移结果序列化失败：{error}")))
    }

    fn lock(&self) -> MutexGuard<'_, GateInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

struct MigratingGuard<'a> {
    gate: &'a StartupGate,
}

impl Drop for MigratingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.gate.inner.lock() {
            inner.migrating = false;
        }
    }
}

fn resolve_candidate(
    pointer_file: Option<&Path>,
) -> Result<(PathBuf, DataDirSource), (PathBuf, String)> {
    match pointer::env_data_dir() {
        Some(Ok(dir)) => return Ok((dir, DataDirSource::EnvOverride)),
        Some(Err(message)) => return Err((PathBuf::new(), message)),
        None => {}
    }

    if let Some(pointer_path) = pointer_file {
        match pointer::read_pointer(pointer_path) {
            Ok(Some(dir)) => {
                if dir.is_dir() {
                    return Ok((dir, DataDirSource::Pointer));
                }
                return Err((
                    dir,
                    format!(
                        "指针文件指向的数据目录不存在：{}（迁移未完成或目录被移动）",
                        pointer_path.display()
                    ),
                ));
            }
            Ok(None) => {}
            Err(message) => return Err((PathBuf::new(), message)),
        }
    }

    match pointer::default_data_dir() {
        Some(dir) => Ok((dir, DataDirSource::Default)),
        None => Err((
            PathBuf::new(),
            "无法确定缺省数据目录（系统未提供 data_dir）".to_string(),
        )),
    }
}

fn map_migration_error(error: migrate::MigrationError) -> IpcError {
    let code = match error.kind {
        MigrationErrorKind::SyncTarget => IpcErrorCode::PathRejected,
        MigrationErrorKind::TargetInvalid
        | MigrationErrorKind::TargetInsideSource
        | MigrationErrorKind::TargetNotEmpty => IpcErrorCode::PathRejected,
        MigrationErrorKind::SourceInvalid
        | MigrationErrorKind::UnsupportedEntry
        | MigrationErrorKind::ChecksumMismatch
        | MigrationErrorKind::Io => IpcErrorCode::MigrationFailed,
    };
    IpcError::new(code, error.message)
}
