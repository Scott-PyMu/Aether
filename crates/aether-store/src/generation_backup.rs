//! generation 整套备份/恢复（ADR-039 v1.0 D-039-5/D-039-8；ADR-020 v1.1）。
//!
//! 备份集布局：`<target>/<backup_id>/manifest.json` + `manifest.sha256` +
//! `generation/`（`aether.db`、`events/events-0..7.db`、`events/shards.json`）。
//!
//! - 停写 + drain + checkpoint 由维护态调用方保证（`backup_control`）；
//! - 逐库 `VACUUM INTO` 快照；manifest 记录每文件相对路径/大小/sha256/角色；
//! - 恢复：校验 manifest + `manifest.sha256` + 8 分片 + schema；复制到同卷
//!   `staging/restore-<new_generation>/generation`，写 `restore-journal.json`；
//!   `CURRENT` 原子切换由启动序列执行（D-039-8）；
//! - 路径只允许备份集内相对路径；拒绝绝对路径/`..`/空段/越界 symlink。
//!
//! 本模块不打开写连接（备份读连接；恢复 staging 为独立目录复制）。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::backup::{program_schema_version, BackupIntegrity, BackupRecord};
use crate::error::StoreError;
use crate::layout::{
    self, shard_file_name, GenerationJournal, ShardsManifest, StorageLayout, EVENTS_DIR_NAME,
    MAIN_DB_NAME, SHARDS_MANIFEST_NAME, SHARD_COUNT,
};
use crate::store::{quick_check, Store};

/// 备份集 manifest 版本。
pub const BACKUP_SET_VERSION: u32 = 1;
/// 备份集格式标识（`<record>.format`）。
pub const BACKUP_FORMAT_SHARDED_SET: &str = "sharded_set";
/// 旧单文件格式（仅展示；恢复禁用）。
pub const BACKUP_FORMAT_SINGLE_FILE: &str = "single_file";
/// manifest 文件名。
pub const MANIFEST_FILE_NAME: &str = "manifest.json";
/// manifest 哈希文件名。
pub const MANIFEST_SHA256_FILE_NAME: &str = "manifest.sha256";
/// generation 子目录。
pub const BACKUP_GENERATION_DIR: &str = "generation";
/// 备份临时集前缀。
pub const BACKUP_SET_TEMP_PREFIX: &str = ".backup-tmp-";
/// 恢复 staging 空间余量（×1.1）。
pub const RESTORE_SPACE_NUMERATOR: u64 = 11;
/// 恢复 staging 空间余量分母。
pub const RESTORE_SPACE_DENOMINATOR: u64 = 10;

/// 备份集文件条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupFileEntry {
    /// 相对备份集根的路径（`generation/aether.db` / `generation/events/events-<n>.db`）。
    pub path: String,
    /// `main` / `shard_manifest` / `shard`。
    pub role: String,
    pub size: u64,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard_id: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_seq: Option<u64>,
}

/// 备份集 manifest（`source_generation` 仅信息性，不作为路径/绑定依据）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupSetManifest {
    pub v: u32,
    pub backup_id: String,
    pub source_generation: String,
    pub created_at: i64,
    pub app_version: String,
    pub schema_version: i64,
    pub shard_count: usize,
    pub encrypted: bool,
    /// `none` / `keychain` / `passphrase`。
    pub artifact_mode: String,
    pub files: Vec<BackupFileEntry>,
}

/// 校验后的备份集。
#[derive(Debug, Clone)]
pub struct ValidatedBackupSet {
    pub root: PathBuf,
    pub manifest: BackupSetManifest,
    pub manifest_path: PathBuf,
    pub manifest_sha256: String,
    pub total_bytes: u64,
}

/// 加密后重写备份集 manifest（路径加 `.aebk`；重算大小/哈希）。
///
/// 供 app 在逐文件 AEBK 加密后调用；保持同一 manifest/`manifest.sha256` 约定。
pub fn rewrite_backup_set_manifest(
    set_root: &Path,
    encrypted: bool,
    artifact_mode: &str,
) -> Result<(BackupSetManifest, String), StoreError> {
    let manifest_path = set_root.join(MANIFEST_FILE_NAME);
    let bytes = fs::read(&manifest_path)?;
    let mut manifest: BackupSetManifest =
        serde_json::from_slice(&bytes).map_err(|error| StoreError::BackupCandidateInvalid {
            path: manifest_path.clone(),
            reason: format!("manifest 非法：{error}"),
        })?;
    manifest.encrypted = encrypted;
    manifest.artifact_mode = artifact_mode.to_owned();
    let mut files = Vec::with_capacity(manifest.files.len());
    for entry in manifest.files {
        let candidate = set_root.join(format!("{}.aebk", entry.path));
        if !candidate.exists() {
            return Err(StoreError::BackupCandidateInvalid {
                path: candidate,
                reason: format!("加密产物缺失（{}）", entry.path),
            });
        }
        let bytes = fs::read(&candidate)?;
        files.push(BackupFileEntry {
            path: format!("{}.aebk", entry.path),
            role: entry.role,
            size: bytes.len() as u64,
            sha256: sha256_hex(&bytes),
            shard_id: entry.shard_id,
            max_seq: entry.max_seq,
        });
    }
    manifest.files = files;
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|error| StoreError::Internal {
            reason: format!("备份 manifest 序列化失败：{error}"),
        })?;
    write_synced(&manifest_path, &manifest_bytes)?;
    let sha = sha256_hex(&manifest_bytes);
    write_synced(&set_root.join(MANIFEST_SHA256_FILE_NAME), sha.as_bytes())?;
    Ok((manifest, sha))
}

/// 将备份集复制/解密到同卷 staging generation，并做每库 `quick_check`/schema 校验。
///
/// `decrypt` 对每个需解密的文件调用（源密文路径、目标明文路径）；未加密备份集
/// 使用 [`stage_restore`] 的复制路径。
pub fn stage_restore_with<F>(
    data_dir: &Path,
    set: &ValidatedBackupSet,
    new_generation: &str,
    now_ms: i64,
    decrypt: F,
) -> Result<GenerationJournal, StoreError>
where
    F: Fn(&Path, &Path) -> Result<(), StoreError>,
{
    let staging = layout::staging_generation_dir(data_dir, "restore", new_generation);
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(staging.join(EVENTS_DIR_NAME))?;
    for entry in &set.manifest.files {
        validate_relative_path(&entry.path)?;
        let source = set.root.join(&entry.path);
        let relative = entry
            .path
            .strip_prefix(&format!("{BACKUP_GENERATION_DIR}/"))
            .ok_or_else(|| StoreError::BackupCandidateInvalid {
                path: source.clone(),
                reason: "备份文件不在 generation/ 目录内".to_owned(),
            })?;
        let dest = staging.join(relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        if entry.path.ends_with(".aebk") {
            let mut dest = dest;
            dest.set_extension("");
            decrypt(&source, &dest)?;
        } else {
            fs::copy(&source, &dest)?;
            sync_file(&dest)?;
        }
    }
    // 加密备份的 manifest 路径带 `.aebk`；复制后的文件名已去后缀。
    if set.manifest.encrypted {
        for entry in &set.manifest.files {
            if entry.path.ends_with(".aebk") {
                let relative = entry
                    .path
                    .trim_start_matches(&format!("{BACKUP_GENERATION_DIR}/"))
                    .trim_end_matches(".aebk");
                let dest = staging.join(relative);
                if !dest.is_file() {
                    return Err(StoreError::BackupCandidateInvalid {
                        path: dest,
                        reason: "解密后文件缺失".to_owned(),
                    });
                }
            }
        }
    }
    for path in staged_db_paths(&staging)? {
        verify_restored_db(&path)?;
    }
    let shards_json = staging.join(EVENTS_DIR_NAME).join(SHARDS_MANIFEST_NAME);
    let shards_sha256 = sha256_file(&shards_json)?;
    Ok(GenerationJournal {
        v: layout::JOURNAL_VERSION,
        kind: "restore".to_owned(),
        old_generation: layout::read_current(data_dir).map(|pointer| pointer.generation),
        new_generation: new_generation.to_owned(),
        shards_sha256,
        backup_manifest_sha256: Some(set.manifest_sha256.clone()),
        phase: "staged".to_owned(),
        requested_at: now_ms,
    })
}

/// 将备份集复制到同卷 staging generation（未加密路径）。
pub fn stage_restore(
    data_dir: &Path,
    set: &ValidatedBackupSet,
    new_generation: &str,
    now_ms: i64,
) -> Result<GenerationJournal, StoreError> {
    if set.manifest.encrypted {
        return Err(StoreError::BackupCandidateInvalid {
            path: set.root.clone(),
            reason: "加密备份集必须提供解密回调（stage_restore_with）".to_owned(),
        });
    }
    stage_restore_with(data_dir, set, new_generation, now_ms, |source, dest| {
        fs::copy(source, dest)?;
        sync_file(dest)?;
        Ok(())
    })
}

/// 将备份集复制到 staging generation 的旧入口（保留名称；未加密）。
pub fn stage_restore_plain(
    data_dir: &Path,
    set: &ValidatedBackupSet,
    new_generation: &str,
    now_ms: i64,
) -> Result<GenerationJournal, StoreError> {
    stage_restore(data_dir, set, new_generation, now_ms)
}

/// 校验 staging generation 是否可提交（每库 quick_check/schema/shard 清单）。
pub fn verify_staged_generation(
    data_dir: &Path,
    kind: &str,
    generation: &str,
) -> Result<(), StoreError> {
    let staging = layout::staging_generation_dir(data_dir, kind, generation);
    for path in staged_db_paths(&staging)? {
        verify_restored_db(&path)?;
    }
    let shards_json = staging.join(EVENTS_DIR_NAME).join(SHARDS_MANIFEST_NAME);
    if !shards_json.is_file() {
        return Err(StoreError::LayoutInvalid {
            reason: format!("staging 缺少 shards.json：{}", shards_json.display()),
        });
    }
    Ok(())
}

/// 已加密备份集的逐文件解密键选择（app 层实现；本模块仅保留接口文档）。
pub const ENCRYPTED_SET_SUFFIX: &str = ".aebk";

/// 备份集创建结果。
#[derive(Debug, Clone)]
pub struct BackupSetOutcome {
    pub record: BackupRecord,
    pub manifest: BackupSetManifest,
    pub total_bytes: u64,
}

/// 恢复 staging 空间需求（备份 generation 解密后总大小 ×1.1）。
pub fn restore_space_required(total_bytes: u64) -> u64 {
    total_bytes
        .saturating_mul(RESTORE_SPACE_NUMERATOR)
        .div_ceil(RESTORE_SPACE_DENOMINATOR)
}

/// 创建整套备份（调用方须已进入维护态/停写并完成 checkpoint）。
#[allow(clippy::too_many_arguments)]
pub fn create_backup_set(
    layout: &StorageLayout,
    target: &Path,
    backup_id: &str,
    kind: &str,
    created_at: i64,
    app_version: &str,
    encrypted: bool,
    artifact_mode: &str,
) -> Result<BackupSetOutcome, StoreError> {
    let set_root = target.join(backup_id);
    if set_root.exists() {
        return Err(StoreError::BackupTargetExists { path: set_root });
    }
    fs::create_dir_all(target)?;
    let temp = target.join(format!("{BACKUP_SET_TEMP_PREFIX}{backup_id}-{created_at}"));
    if temp.exists() {
        fs::remove_dir_all(&temp)?;
    }
    fs::create_dir_all(temp.join(BACKUP_GENERATION_DIR).join(EVENTS_DIR_NAME))?;

    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    let schema_version = program_schema_version();

    // 主库快照。
    let main_target = temp.join(BACKUP_GENERATION_DIR).join(MAIN_DB_NAME);
    vacuum_into(layout.main_db(), &main_target)?;
    files.push(file_entry(&temp, &main_target, "main", None, None)?);

    // 逐分片快照。
    for (shard_id, source) in layout.shard_paths().iter().enumerate() {
        let shard_id = u8::try_from(shard_id).unwrap_or(0);
        let target_path = temp
            .join(BACKUP_GENERATION_DIR)
            .join(shard_file_name(shard_id));
        vacuum_into(source, &target_path)?;
        let max_seq = shard_max_seq(&target_path)?;
        files.push(file_entry(
            &temp,
            &target_path,
            "shard",
            Some(shard_id),
            max_seq,
        )?);
    }

    // 分片清单（`shards.json` 不含 generation）。
    let shards_json_target = temp
        .join(BACKUP_GENERATION_DIR)
        .join(EVENTS_DIR_NAME)
        .join(SHARDS_MANIFEST_NAME);
    fs::copy(layout.shards_json(), &shards_json_target)?;
    files.push(file_entry(
        &temp,
        &shards_json_target,
        "shard_manifest",
        None,
        None,
    )?);

    for file in &files {
        total_bytes = total_bytes.saturating_add(file.size);
    }
    let manifest = BackupSetManifest {
        v: BACKUP_SET_VERSION,
        backup_id: backup_id.to_owned(),
        source_generation: layout.generation().to_owned(),
        created_at,
        app_version: app_version.to_owned(),
        schema_version,
        shard_count: SHARD_COUNT,
        encrypted,
        artifact_mode: artifact_mode.to_owned(),
        files,
    };
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|error| StoreError::Internal {
            reason: format!("备份 manifest 序列化失败：{error}"),
        })?;
    let manifest_path = temp.join(MANIFEST_FILE_NAME);
    write_synced(&manifest_path, &manifest_bytes)?;
    let manifest_sha = sha256_hex(&manifest_bytes);
    write_synced(
        &temp.join(MANIFEST_SHA256_FILE_NAME),
        manifest_sha.as_bytes(),
    )?;
    sync_tree(&temp)?;
    fs::rename(&temp, &set_root).inspect_err(|_| {
        let _ = fs::remove_dir_all(&temp);
    })?;

    let record = BackupRecord {
        id: backup_id.to_owned(),
        path: set_root.clone(),
        size_bytes: total_bytes,
        encrypted,
        kind: kind.to_owned(),
        created_at,
        format: Some(BACKUP_FORMAT_SHARDED_SET.to_owned()),
        set_path: Some(set_root.clone()),
        manifest_path: Some(set_root.join(MANIFEST_FILE_NAME)),
        manifest_sha256: Some(manifest_sha),
        shard_count: Some(SHARD_COUNT),
        integrity: Some(BackupIntegrity {
            status: "ok".to_owned(),
            checked_at: Some(created_at),
        }),
    };
    Ok(BackupSetOutcome {
        record,
        manifest,
        total_bytes,
    })
}

/// 校验备份集（目录含 manifest 或 `manifest.json` 文件）。
///
/// 拒绝旧 `.db`/`.aebk` 单文件（开发阶段不再接受单文件恢复）。
pub fn validate_backup_set(path: &Path) -> Result<ValidatedBackupSet, StoreError> {
    let canonical = fs::canonicalize(path).map_err(|_| StoreError::BackupCandidateInvalid {
        path: path.to_path_buf(),
        reason: "路径不存在".to_owned(),
    })?;
    let manifest_path = if canonical.is_dir() {
        canonical.join(MANIFEST_FILE_NAME)
    } else if canonical
        .file_name()
        .is_some_and(|name| name == MANIFEST_FILE_NAME)
    {
        canonical.clone()
    } else {
        return Err(StoreError::BackupCandidateInvalid {
            path: canonical.clone(),
            reason: "外部备份必须是含 manifest.json 的目录或 manifest.json 文件（旧 .db/.aebk 单文件不再接受）".to_owned(),
        });
    };
    let root = manifest_path
        .parent()
        .ok_or_else(|| StoreError::BackupCandidateInvalid {
            path: manifest_path.clone(),
            reason: "manifest 路径没有父目录".to_owned(),
        })?
        .to_path_buf();
    let manifest_bytes =
        fs::read(&manifest_path).map_err(|error| StoreError::BackupCandidateInvalid {
            path: manifest_path.clone(),
            reason: format!("manifest 读取失败：{error}"),
        })?;
    let manifest_sha256 = sha256_hex(&manifest_bytes);
    let recorded = fs::read_to_string(root.join(MANIFEST_SHA256_FILE_NAME)).map_err(|error| {
        StoreError::BackupCandidateInvalid {
            path: root.clone(),
            reason: format!("manifest.sha256 缺失/不可读：{error}"),
        }
    })?;
    if recorded.trim() != manifest_sha256 {
        return Err(StoreError::BackupCandidateInvalid {
            path: root.clone(),
            reason: "manifest.sha256 与 manifest 哈希不匹配".to_owned(),
        });
    }
    let manifest: BackupSetManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        StoreError::BackupCandidateInvalid {
            path: manifest_path.clone(),
            reason: format!("manifest 非法：{error}"),
        }
    })?;
    if manifest.v != BACKUP_SET_VERSION {
        return Err(StoreError::BackupCandidateInvalid {
            path: root.clone(),
            reason: format!("备份集版本 {} 不受支持", manifest.v),
        });
    }
    if manifest.shard_count != SHARD_COUNT || manifest.files.len() < 1 + SHARD_COUNT + 1 {
        return Err(StoreError::BackupCandidateInvalid {
            path: root.clone(),
            reason: format!(
                "备份集分片数 {} / 文件数 {} 不完整（期望 {SHARD_COUNT} 分片）",
                manifest.shard_count,
                manifest.files.len()
            ),
        });
    }
    if manifest.schema_version > program_schema_version() {
        return Err(StoreError::SchemaNewerThanProgram {
            database: manifest.schema_version,
            program: program_schema_version(),
        });
    }

    let mut total_bytes = 0u64;
    let mut seen_main = false;
    let mut seen_manifest = false;
    let mut shard_ids = std::collections::BTreeSet::new();
    for entry in &manifest.files {
        validate_relative_path(&entry.path)?;
        let file_path = root.join(&entry.path);
        let canonical_file =
            fs::canonicalize(&file_path).map_err(|error| StoreError::BackupCandidateInvalid {
                path: file_path.clone(),
                reason: format!("备份文件缺失/不可读：{error}"),
            })?;
        if !canonical_file.starts_with(&root) {
            return Err(StoreError::BackupCandidateInvalid {
                path: file_path,
                reason: "备份文件越界（symlink/junction 指向备份集根外）".to_owned(),
            });
        }
        let bytes = fs::read(&canonical_file)?;
        let actual = sha256_hex(&bytes);
        if actual != entry.sha256 {
            return Err(StoreError::BackupCandidateInvalid {
                path: canonical_file,
                reason: "文件与 manifest sha256 不匹配".to_owned(),
            });
        }
        total_bytes = total_bytes.saturating_add(bytes.len() as u64);
        match entry.role.as_str() {
            "main" => seen_main = true,
            "shard_manifest" => seen_manifest = true,
            "shard" => {
                if let Some(shard_id) = entry.shard_id {
                    shard_ids.insert(shard_id);
                }
            }
            _ => {}
        }
    }
    if !seen_main || !seen_manifest || shard_ids.len() != SHARD_COUNT {
        return Err(StoreError::BackupCandidateInvalid {
            path: root,
            reason: "备份集缺少主库/分片清单/分片文件".to_owned(),
        });
    }
    Ok(ValidatedBackupSet {
        root,
        manifest,
        manifest_path,
        manifest_sha256,
        total_bytes,
    })
}

/// 新 generation id（`gen-<ULID>`）。
pub fn new_generation_id() -> String {
    format!("gen-{}", ulid::Ulid::new())
}

fn staged_db_paths(staging: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let mut paths = vec![staging.join(MAIN_DB_NAME)];
    for index in 0..SHARD_COUNT {
        let id = u8::try_from(index).unwrap_or(0);
        paths.push(staging.join(shard_file_name(id)));
    }
    Ok(paths)
}

fn verify_restored_db(path: &Path) -> Result<(), StoreError> {
    let connection = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let report = quick_check(&connection);
    if !report.ok {
        return Err(StoreError::BackupCandidateInvalid {
            path: path.to_path_buf(),
            reason: format!("quick_check 失败：{}", report.summary()),
        });
    }
    let version: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if version > program_schema_version() {
        return Err(StoreError::SchemaNewerThanProgram {
            database: version,
            program: program_schema_version(),
        });
    }
    Ok(())
}

fn vacuum_into(source: &Path, target: &Path) -> Result<(), StoreError> {
    let target_text = target.to_str().ok_or_else(|| StoreError::NonUtf8Path {
        path: target.to_path_buf(),
    })?;
    let connection = Connection::open_with_flags(
        source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.execute("VACUUM INTO ?1", [target_text])?;
    sync_file(target)?;
    Ok(())
}

fn shard_max_seq(path: &Path) -> Result<Option<u64>, StoreError> {
    let connection = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let max: Option<i64> =
        connection.query_row("SELECT MAX(seq) FROM events", [], |row| row.get(0))?;
    match max {
        Some(value) => u64::try_from(value)
            .map(Some)
            .map_err(|_| StoreError::Internal {
                reason: format!("events.seq 为负：{value}"),
            }),
        None => Ok(None),
    }
}

fn file_entry(
    root: &Path,
    path: &Path,
    role: &str,
    shard_id: Option<u8>,
    max_seq: Option<u64>,
) -> Result<BackupFileEntry, StoreError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| StoreError::Internal {
            reason: format!("备份文件不在备份集根内：{}", path.display()),
        })?
        .to_string_lossy()
        .replace('\\', "/");
    let bytes = fs::read(path)?;
    Ok(BackupFileEntry {
        path: relative,
        role: role.to_owned(),
        size: bytes.len() as u64,
        sha256: sha256_hex(&bytes),
        shard_id,
        max_seq,
    })
}

/// 备份集内部相对路径校验（禁止绝对路径/`..`/空段/反斜杠段）。
pub fn validate_relative_path(path: &str) -> Result<(), StoreError> {
    let invalid = |reason: &str| StoreError::BackupCandidateInvalid {
        path: PathBuf::from(path),
        reason: reason.to_owned(),
    };
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') {
        return Err(invalid("路径必须是非空相对路径"));
    }
    if path.len() >= 2 && path.as_bytes()[1] == b':' {
        return Err(invalid("禁止绝对路径"));
    }
    for segment in path.split(['/', '\\']) {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(invalid("路径包含空段/`.`/`..`"));
        }
    }
    Ok(())
}

fn sync_tree(path: &Path) -> Result<(), StoreError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            sync_file(&entry.path())?;
        }
    }
    Ok(())
}

fn sync_file(path: &Path) -> Result<(), StoreError> {
    let file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    file.sync_all()?;
    Ok(())
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<String, StoreError> {
    let bytes = fs::read(path)?;
    Ok(sha256_hex(&bytes))
}

/// 只读检查当前 generation 是否满足启动不变量（诊断/测试共用）。
pub fn assert_generation_ready(layout: &StorageLayout) -> Result<(), StoreError> {
    layout::assert_invariants(layout)
}

/// 当前 generation 主库 + 分片 `shards.json` 校验（备份前预检）。
pub fn verify_layout(layout: &StorageLayout) -> Result<ShardsManifest, StoreError> {
    let bytes = fs::read(layout.shards_json())?;
    let manifest: ShardsManifest =
        serde_json::from_slice(&bytes).map_err(|error| StoreError::LayoutInvalid {
            reason: format!("shards.json 非法：{error}"),
        })?;
    if manifest.shard_count != SHARD_COUNT {
        return Err(StoreError::LayoutInvalid {
            reason: format!("shard_count={}（期望 {SHARD_COUNT}）", manifest.shard_count),
        });
    }
    Ok(manifest)
}

/// 诊断：列出数据目录各 generation 的路径/大小/状态。
pub fn generation_diagnostics(data_dir: &Path) -> Vec<serde_json::Value> {
    layout::list_generations(data_dir)
        .into_iter()
        .map(|info| {
            serde_json::json!({
                "path": info.path.to_string_lossy(),
                "status": info.status.code(),
                "bytes": info.bytes,
                "journal_referenced": info.journal_referenced,
            })
        })
        .collect()
}

/// 旧 `.pre-*`/`.failed-*` 清理（保留最近 1 份；当前/journal 引用不删）。
pub fn cleanup_generations(data_dir: &Path, now_ms: i64) -> Vec<PathBuf> {
    layout::cleanup_old_generations(data_dir, now_ms)
}

/// 生成恢复成功提示（附件不随库恢复；ADR-039 D-039-5 §5.8）。
pub const RESTORE_ATTACHMENTS_NOTICE: &str =
    "附件内容未随库恢复（ADR-035）；请单独保留/恢复 attachments 目录";

/// staging generation 目录（测试/恢复启动序列共用）。
pub fn staging_generation(data_dir: &Path, kind: &str, generation: &str) -> PathBuf {
    layout::staging_generation_dir(data_dir, kind, generation)
}

/// 只读打开数据库并返回 `events` 行数（主库空表断言/测试）。
pub fn events_count(path: &Path) -> Result<i64, StoreError> {
    let connection = Store::open_read_only(path)?;
    Ok(connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_are_enforced() {
        assert!(validate_relative_path("generation/aether.db").is_ok());
        assert!(validate_relative_path("generation/events/events-0.db").is_ok());
        assert!(validate_relative_path("../escape.db").is_err());
        assert!(validate_relative_path("/abs.db").is_err());
        assert!(validate_relative_path("C:/abs.db").is_err());
        assert!(validate_relative_path("generation//aether.db").is_err());
    }

    #[test]
    fn restore_space_margin_is_one_point_one() {
        assert_eq!(restore_space_required(100), 110);
        assert_eq!(restore_space_required(0), 0);
    }
}
