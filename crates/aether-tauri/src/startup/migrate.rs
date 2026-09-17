//! 数据目录迁移执行（M1-06）：复制 → 校验（sha256）→ 原子替换。
//!
//! 迁移流（A4 降级路径「迁移到本地目录」；评审 #9 仅迁移/退出）：
//! 1. 源、目标双向 `canonicalize` 关系校验（不得互为祖先）；目标必须已存在、为空目录；
//! 2. 目标再跑一次 A4 检测（目标自身也不得位于同步盘）；
//! 3. 递归复制到目标内的暂存目录（`.aether-migration-*`），逐文件流式 sha256；
//! 4. 校验：重读暂存文件，比对 sha256 与字节数，任一不符即中止并清理暂存；
//! 5. 原子替换：暂存目录内顶层条目逐个 `rename` 进目标（同卷内联原子），
//!    **主库文件最后移动**（提交点）；指针写入由上层在全部成功后执行；
//! 6. 源目录原样保留（迁移失败可回滚，且不主动删除同步盘侧数据）。
//!
//! 多文件目录无法做到跨文件单一原子操作；本实现保证：迁移中途失败时目标不出现
//! 「半套」数据（校验通过前不 rename），且「锁定新目录」以指针文件原子替换为准。

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::ipc::path::is_within;

use super::detect::{detect_data_dir, DetectionContext};

/// 暂存目录前缀（目标目录内的迁移中间态）。
pub const STAGING_PREFIX: &str = ".aether-migration-";
/// 主库文件名（提交点，最后移动）。
pub const MAIN_DB_NAME: &str = "aether.db";
const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// 空间护栏系数（ADR-003 决策 19：可用空间 ≥ 当前 `db+wal` × 1.2）。
pub const SPACE_MARGIN_NUMERATOR: u64 = 6;
pub const SPACE_MARGIN_DENOMINATOR: u64 = 5;

/// 迁移失败分类（命令层据此映射结构化错误码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationErrorKind {
    SourceInvalid,
    TargetInvalid,
    TargetInsideSource,
    TargetNotEmpty,
    SyncTarget,
    UnsupportedEntry,
    ChecksumMismatch,
    /// 目标目录不可写（前置探针失败）。
    TargetNotWritable,
    /// 目标可用空间不足（前置空间护栏；探针可注入）。
    InsufficientSpace,
    Io,
}

#[derive(Debug, Clone)]
pub struct MigrationError {
    pub kind: MigrationErrorKind,
    pub message: String,
}

impl MigrationError {
    pub fn new(kind: MigrationErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// 迁移前置探针（可注入；生产默认 [`NativeMigrationProbe`]）。
///
/// - 可写校验：原生实现为「创建并删除探针文件」；
/// - 空间护栏：`Ok(None)` 表示未知（不阻断）——原生实现暂返回未知，真实磁盘探针
///   按 ADR-003 决策 19 在 M3-04（备份/导出）统一接线；测试替身可返回固定值以覆盖
///   「空间不足」分支。
pub trait MigrationProbe: Send + Sync {
    fn ensure_target_writable(&self, target: &Path) -> Result<(), MigrationError> {
        ensure_dir_writable(target)
    }

    fn available_bytes(&self, target: &Path) -> Result<Option<u64>, String> {
        let _ = target;
        Ok(None)
    }
}

/// 原生迁移探针：真实可写校验 + 空间未知（待 M3-04 接线）。
pub struct NativeMigrationProbe;

impl MigrationProbe for NativeMigrationProbe {}

/// 目标目录可写性探针（创建并删除隐藏探针文件）。
pub fn ensure_dir_writable(dir: &Path) -> Result<(), MigrationError> {
    let probe = dir.join(format!(".aether-write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"probe").map_err(|error| {
        MigrationError::new(
            MigrationErrorKind::TargetNotWritable,
            format!("迁移目标不可写（{}）：{error}", dir.display()),
        )
    })?;
    std::fs::remove_file(&probe)
        .map_err(|error| io_error(MigrationErrorKind::Io, &probe, &error))?;
    Ok(())
}

/// 源目录所需可用空间（全部文件字节数上取整 ×1.2）。
pub fn required_free_bytes(source: &Path) -> Result<u64, MigrationError> {
    let mut total = 0u128;
    let mut stack = vec![source.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let listing = std::fs::read_dir(&dir)
            .map_err(|error| io_error(MigrationErrorKind::Io, &dir, &error))?;
        for item in listing {
            let item = item.map_err(|error| io_error(MigrationErrorKind::Io, &dir, &error))?;
            let path = item.path();
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| io_error(MigrationErrorKind::Io, &path, &error))?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                total += metadata.len() as u128;
            }
        }
    }
    let numerator = u128::from(SPACE_MARGIN_NUMERATOR);
    let denominator = u128::from(SPACE_MARGIN_DENOMINATOR);
    let required = (total * numerator).div_ceil(denominator);
    match u64::try_from(required) {
        // 饱和：超过 u64 的空间需求等价于不可满足，交由空间检查判失败。
        Err(_) => Ok(u64::MAX),
        Ok(bytes) => Ok(bytes),
    }
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for MigrationError {}

/// 单个已复制并校验的文件。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CopiedEntry {
    pub relative: String,
    pub sha256: String,
    pub bytes: u64,
}

/// 迁移成功的结果（供 IPC 返回与 E2E 断言）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationOutcome {
    pub source: String,
    pub target: String,
    pub entries: Vec<CopiedEntry>,
    pub total_bytes: u64,
}

/// `sha256` 文件摘要（与 aether-store 迁移 checksum 同口径）。
pub fn sha256_file(path: &Path) -> Result<String, MigrationError> {
    let mut reader =
        File::open(path).map_err(|error| io_error(MigrationErrorKind::Io, path, &error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| io_error(MigrationErrorKind::Io, path, &error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 执行迁移（原生前置探针）；成功后源目录保持不变，调用方负责持久化指针并切换运行状态。
pub fn migrate_data_dir(
    source: &Path,
    target: &Path,
    ctx: &DetectionContext,
) -> Result<MigrationOutcome, MigrationError> {
    migrate_data_dir_with(source, target, ctx, &NativeMigrationProbe)
}

/// 执行迁移（可注入前置探针：可写校验 / 空间护栏）。
pub fn migrate_data_dir_with(
    source: &Path,
    target: &Path,
    ctx: &DetectionContext,
    probe: &dyn MigrationProbe,
) -> Result<MigrationOutcome, MigrationError> {
    if !source.is_dir() {
        return Err(MigrationError::new(
            MigrationErrorKind::SourceInvalid,
            format!("源数据目录不存在或不是目录：{}", source.display()),
        ));
    }
    if !target.is_dir() {
        return Err(MigrationError::new(
            MigrationErrorKind::TargetInvalid,
            format!("迁移目标必须是已存在的目录：{}", target.display()),
        ));
    }
    let source_resolved = super::detect::resolve_existing_prefix(source);
    let target_resolved = super::detect::resolve_existing_prefix(target);
    if super::detect::same_path(&source_resolved, &target_resolved) {
        return Err(MigrationError::new(
            MigrationErrorKind::TargetInvalid,
            "迁移目标不能是源数据目录自身",
        ));
    }
    if is_within(&target_resolved, &source_resolved) {
        return Err(MigrationError::new(
            MigrationErrorKind::TargetInsideSource,
            "迁移目标不能位于源数据目录内部",
        ));
    }
    if is_within(&source_resolved, &target_resolved) {
        return Err(MigrationError::new(
            MigrationErrorKind::TargetInvalid,
            "源数据目录不能位于迁移目标内部",
        ));
    }

    let target_report = detect_data_dir(target, ctx);
    if target_report.is_reject() {
        return Err(MigrationError::new(
            MigrationErrorKind::SyncTarget,
            format!(
                "迁移目标同样命中同步盘拒绝清单：{}",
                target_report.reasons.join("；")
            ),
        ));
    }

    // 前置探针：目标可写 + 空间护栏（可注入；见 [`MigrationProbe`]）。
    probe.ensure_target_writable(target)?;
    match probe.available_bytes(target) {
        Ok(Some(available)) => {
            let required = required_free_bytes(source)?;
            if available < required {
                return Err(MigrationError::new(
                    MigrationErrorKind::InsufficientSpace,
                    format!(
                        "迁移目标可用空间不足：可用 {available} 字节 < 需求 {required} 字节（源总量 ×1.2）"
                    ),
                ));
            }
        }
        Ok(None) => {}
        Err(message) => {
            return Err(MigrationError::new(
                MigrationErrorKind::Io,
                format!("可用空间探测失败：{message}"),
            ));
        }
    }

    clean_stale_staging(target)?;
    let mut existing = std::fs::read_dir(target)
        .map_err(|error| io_error(MigrationErrorKind::TargetInvalid, target, &error))?;
    if existing.next().is_some() {
        return Err(MigrationError::new(
            MigrationErrorKind::TargetNotEmpty,
            format!(
                "迁移目标必须为空目录（避免覆盖既有数据）：{}",
                target.display()
            ),
        ));
    }

    let staging = target.join(format!(
        "{STAGING_PREFIX}{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir_all(&staging)
        .map_err(|error| io_error(MigrationErrorKind::Io, &staging, &error))?;

    let result = run_migration(source, target, &staging);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn run_migration(
    source: &Path,
    target: &Path,
    staging: &Path,
) -> Result<MigrationOutcome, MigrationError> {
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    copy_tree(source, source, staging, &mut entries, &mut total_bytes)?;

    for entry in &entries {
        verify_entry(staging, entry)?;
    }

    commit_entries(staging, target)?;
    std::fs::remove_dir_all(staging)
        .map_err(|error| io_error(MigrationErrorKind::Io, staging, &error))?;

    Ok(MigrationOutcome {
        source: source.to_string_lossy().to_string(),
        target: target.to_string_lossy().to_string(),
        entries,
        total_bytes,
    })
}

fn copy_tree(
    source_root: &Path,
    dir: &Path,
    staging_root: &Path,
    entries: &mut Vec<CopiedEntry>,
    total_bytes: &mut u64,
) -> Result<(), MigrationError> {
    let listing =
        std::fs::read_dir(dir).map_err(|error| io_error(MigrationErrorKind::Io, dir, &error))?;
    for item in listing {
        let item = item.map_err(|error| io_error(MigrationErrorKind::Io, dir, &error))?;
        let path = item.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| io_error(MigrationErrorKind::Io, &path, &error))?;
        let relative = path
            .strip_prefix(source_root)
            .map_err(|_| {
                MigrationError::new(
                    MigrationErrorKind::Io,
                    format!("无法计算相对路径：{}", path.display()),
                )
            })?
            .to_path_buf();
        if metadata.file_type().is_symlink() {
            return Err(MigrationError::new(
                MigrationErrorKind::UnsupportedEntry,
                format!(
                    "数据目录包含符号链接/Junction，拒绝迁移以保证数据完整性：{}",
                    relative.display()
                ),
            ));
        }
        if metadata.is_dir() {
            let staged_dir = staging_root.join(&relative);
            std::fs::create_dir_all(&staged_dir)
                .map_err(|error| io_error(MigrationErrorKind::Io, &staged_dir, &error))?;
            copy_tree(source_root, &path, staging_root, entries, total_bytes)?;
        } else if metadata.is_file() {
            let staged_file = staging_root.join(&relative);
            if let Some(parent) = staged_file.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| io_error(MigrationErrorKind::Io, parent, &error))?;
            }
            copy_file(&path, &staged_file, &relative, entries, total_bytes)?;
        } else {
            return Err(MigrationError::new(
                MigrationErrorKind::UnsupportedEntry,
                format!(
                    "数据目录包含非普通文件条目，拒绝迁移：{}",
                    relative.display()
                ),
            ));
        }
    }
    Ok(())
}

fn copy_file(
    source: &Path,
    staged: &Path,
    relative: &Path,
    entries: &mut Vec<CopiedEntry>,
    total_bytes: &mut u64,
) -> Result<(), MigrationError> {
    let mut reader =
        File::open(source).map_err(|error| io_error(MigrationErrorKind::Io, source, &error))?;
    let mut writer =
        File::create(staged).map_err(|error| io_error(MigrationErrorKind::Io, staged, &error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    let mut bytes = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| io_error(MigrationErrorKind::Io, source, &error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        writer
            .write_all(&buffer[..read])
            .map_err(|error| io_error(MigrationErrorKind::Io, staged, &error))?;
        bytes += read as u64;
    }
    writer
        .sync_all()
        .map_err(|error| io_error(MigrationErrorKind::Io, staged, &error))?;

    entries.push(CopiedEntry {
        relative: relative.display().to_string(),
        sha256: hex::encode(hasher.finalize()),
        bytes,
    });
    *total_bytes += bytes;
    Ok(())
}

fn verify_entry(staging_root: &Path, entry: &CopiedEntry) -> Result<(), MigrationError> {
    let staged = staging_root.join(&entry.relative);
    let actual = sha256_file(&staged)?;
    if actual != entry.sha256 {
        return Err(MigrationError::new(
            MigrationErrorKind::ChecksumMismatch,
            format!(
                "校验失败：{} 的 sha256 复制前后不一致（源 {} / 副本 {}）",
                entry.relative, entry.sha256, actual
            ),
        ));
    }
    let size = std::fs::metadata(&staged)
        .map_err(|error| io_error(MigrationErrorKind::Io, &staged, &error))?
        .len();
    if size != entry.bytes {
        return Err(MigrationError::new(
            MigrationErrorKind::ChecksumMismatch,
            format!(
                "校验失败：{} 字节数不一致（源 {} / 副本 {}）",
                entry.relative, entry.bytes, size
            ),
        ));
    }
    Ok(())
}

fn commit_entries(staging: &Path, target: &Path) -> Result<(), MigrationError> {
    let mut top_level: Vec<(String, PathBuf)> = Vec::new();
    let listing = std::fs::read_dir(staging)
        .map_err(|error| io_error(MigrationErrorKind::Io, staging, &error))?;
    for item in listing {
        let item = item.map_err(|error| io_error(MigrationErrorKind::Io, staging, &error))?;
        top_level.push((item.file_name().to_string_lossy().to_string(), item.path()));
    }
    // 主库文件最后移动：其出现即视为迁移提交点。
    top_level.sort_by(|left, right| {
        let left_db = left.0 == MAIN_DB_NAME;
        let right_db = right.0 == MAIN_DB_NAME;
        left_db.cmp(&right_db).then_with(|| left.0.cmp(&right.0))
    });
    for (name, path) in top_level {
        let destination = target.join(&name);
        std::fs::rename(&path, &destination)
            .map_err(|error| io_error(MigrationErrorKind::Io, &destination, &error))?;
    }
    Ok(())
}

fn clean_stale_staging(target: &Path) -> Result<(), MigrationError> {
    let listing = std::fs::read_dir(target)
        .map_err(|error| io_error(MigrationErrorKind::TargetInvalid, target, &error))?;
    for item in listing {
        let item =
            item.map_err(|error| io_error(MigrationErrorKind::TargetInvalid, target, &error))?;
        if item
            .file_name()
            .to_string_lossy()
            .starts_with(STAGING_PREFIX)
        {
            let path = item.path();
            std::fs::remove_dir_all(&path)
                .map_err(|error| io_error(MigrationErrorKind::Io, &path, &error))?;
        }
    }
    Ok(())
}

fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("{:x}", duration.as_nanos()),
        Err(_) => "0".to_string(),
    }
}

fn io_error(kind: MigrationErrorKind, path: &Path, error: &std::io::Error) -> MigrationError {
    MigrationError::new(kind, format!("{} 操作失败：{error}", path.display()))
}
