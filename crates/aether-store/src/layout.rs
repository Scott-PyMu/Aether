//! 代际布局（ADR-039 v1.0 D-039-1/D-039-6/D-039-8）：`CURRENT` + `generations/<gen>/`
//! 主库 + 8 个事件分片库 + `events/shards.json` + `clean_shutdown.json`。
//!
//! 本模块只做布局/指针/清单/容量与 generation journal 的文件级操作；数据库打开与迁移
//! 由 [`crate::store::Store`] / [`crate::write_queue::StoreRuntime`] 负责。
//!
//! 硬约束：
//! - 不修改已发布迁移；本模块不执行 SQL（只读计数除外）；
//! - `generation` 正则 `^gen-[0-9A-Z]{26}$`；路径必须位于 `<data_dir>/generations/`；
//! - `shards.json` 不含 `generation`（`CURRENT` 唯一绑定）；
//! - `CURRENT` 原子替换：Windows `ReplaceFileW`/`MoveFileExW`，POSIX `rename(2)`；
//!   禁止删除旧文件再 rename 伪装原子。
//! - 路径越界/symlink/junction 解析到根外一律拒绝（内部错误映射 `db_open_failed`）。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::StoreError;
use crate::store::Store;

/// 分片数（ADR-039 D-039-1：N=8 固定）。
pub const SHARD_COUNT: usize = 8;
/// `CURRENT` 格式版本（ADR-039 D-039-1）。
pub const FORMAT_VERSION: u32 = 1;
/// 代际指针文件名。
pub const CURRENT_FILE_NAME: &str = "CURRENT";
/// 代际目录名。
pub const GENERATIONS_DIR_NAME: &str = "generations";
/// 主库文件名。
pub const MAIN_DB_NAME: &str = "aether.db";
/// 分片目录名。
pub const EVENTS_DIR_NAME: &str = "events";
/// 分片清单文件名。
pub const SHARDS_MANIFEST_NAME: &str = "shards.json";
/// 干净关闭 sidecar 文件名。
pub const CLEAN_SHUTDOWN_NAME: &str = "clean_shutdown.json";
/// 恢复/导入 staging 目录名。
pub const STAGING_DIR_NAME: &str = "staging";
/// 恢复 journal 文件名（数据根）。
pub const RESTORE_JOURNAL_NAME: &str = "restore-journal.json";
/// 导入 journal 文件名（数据根）。
pub const IMPORT_JOURNAL_NAME: &str = "import-journal.json";
/// `CURRENT` 临时文件前缀。
pub const CURRENT_TEMP_PREFIX: &str = "CURRENT.tmp-";
/// 旧 generation 回滚点前缀（恢复/导入切换后保留）。
pub const PRE_GENERATION_PREFIX: &str = ".pre-";
/// 失败 generation 前缀。
pub const FAILED_GENERATION_PREFIX: &str = ".failed-";
/// generation journal 版本。
pub const JOURNAL_VERSION: u32 = 1;
/// 恢复 staging 空间余量（ADR-039 D-039-5 §5：≥ 备份 generation ×1.1）。
pub const RESTORE_SPACE_MARGIN_NUMERATOR: u64 = 11;
/// 恢复 staging 空间余量分母。
pub const RESTORE_SPACE_MARGIN_DENOMINATOR: u64 = 10;

/// 会话稳定分片（ADR-039 D-039-1：UTF-8 字节 FNV-1a 64 mod N）。
///
/// 固定测试向量（Windows/Linux 一致）：
/// - `01J8ZQ00000000000000000001` → 2
/// - `01J8ZQ00000000000000000002` → 7
/// - `01J8ZQ00000000000000000003` → 4
/// - `01J8ZQ00000000000000000004` → 1
/// - `01J8ZQ00000000000000000005` → 6
/// - `01J8ZQ00000000000000000006` → 3
/// - `01J8ZQ00000000000000000007` → 0
/// - `01J8ZQ00000000000000000008` → 5
pub fn shard_for(session_id: &str) -> u8 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in session_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // N=8 → 低 3 位；显式取模以固定口径。
    u8::try_from(hash % u64::try_from(SHARD_COUNT).unwrap_or(8)).unwrap_or(0)
}

/// 分片文件名（相对 generation 根的 `events/events-<n>.db`）。
pub fn shard_file_name(shard_id: u8) -> String {
    format!("{EVENTS_DIR_NAME}/events-{shard_id}.db")
}

/// generation id 正则（`^gen-[0-9A-Z]{26}$`）。
pub fn is_valid_generation_id(generation: &str) -> bool {
    let Some(rest) = generation.strip_prefix("gen-") else {
        return false;
    };
    rest.len() == 26
        && rest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
}

/// 代际指针（`CURRENT`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentPointer {
    pub format_version: u32,
    pub generation: String,
    pub shards_sha256: String,
    pub switched_at: i64,
}

/// 分片清单条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardEntry {
    pub id: u8,
    /// 相对 generation 根的 `events/events-<n>.db`。
    pub path: String,
    pub schema_version: i64,
}

/// 分片清单（`events/shards.json`；**不含** `generation`，`CURRENT` 唯一绑定）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardsManifest {
    pub shard_count: usize,
    pub shards: Vec<ShardEntry>,
    pub schema_version: i64,
    pub app_version: String,
    pub created_at: i64,
}

/// 数据库指纹（`clean_shutdown.json`；M10-09 只写/校验 generation，M10-10 细化跳过判定）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseFingerprint {
    pub schema_version: i64,
    /// SQLite 文件头偏移 24 的 4 字节 change counter（不是 `PRAGMA data_version`）。
    pub header_change_counter: u64,
    pub file_size: u64,
    pub wal_size: u64,
    pub last_seq: Option<u64>,
}

/// 分片指纹。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardFingerprint {
    pub shard_id: u8,
    #[serde(flatten)]
    pub database: DatabaseFingerprint,
}

/// 干净关闭 sidecar（ADR-039 附录 B 的 M10-09 子集；M10-10 补齐跳过判定字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanShutdown {
    pub generation: String,
    pub app_version: String,
    pub closed_at: i64,
    pub close_sequence_completed: bool,
    pub write_queue_drained: bool,
    pub background_check_completed: bool,
    pub last_full_check_at: Option<i64>,
    pub clean_start_count: u32,
    pub main: DatabaseFingerprint,
    pub shards: Vec<ShardFingerprint>,
}

/// generation 维护 journal（恢复/导入整套切换；`CURRENT` 是唯一提交点）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationJournal {
    pub v: u32,
    /// `restore` / `import`。
    pub kind: String,
    pub old_generation: Option<String>,
    pub new_generation: String,
    /// 新 generation `shards.json` 的哈希。
    pub shards_sha256: String,
    /// 备份 manifest 哈希（恢复；信息性）。
    pub backup_manifest_sha256: Option<String>,
    /// `staged` / `switched` / `committed`。
    pub phase: String,
    pub requested_at: i64,
}

/// generation 状态（诊断/容量/清理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationStatus {
    Current,
    Pre,
    Failed,
    Incomplete,
}

impl GenerationStatus {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Pre => "pre",
            Self::Failed => "failed",
            Self::Incomplete => "incomplete",
        }
    }
}

/// generation 目录信息（诊断包/容量巡检）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationInfo {
    pub path: PathBuf,
    pub status: GenerationStatus,
    pub bytes: u64,
    pub journal_referenced: bool,
}

/// 代际布局（打开/初始化后的只读路径集合）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageLayout {
    data_dir: PathBuf,
    generation: String,
    generation_dir: PathBuf,
    main_db: PathBuf,
    shard_paths: Vec<PathBuf>,
    shards_json: PathBuf,
    current_path: PathBuf,
    sidecar_path: PathBuf,
}

impl StorageLayout {
    /// 数据根。
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// generation id（`gen-<ULID>`）。
    pub fn generation(&self) -> &str {
        &self.generation
    }

    /// generation 目录。
    pub fn generation_dir(&self) -> &Path {
        &self.generation_dir
    }

    /// 主库路径。
    pub fn main_db(&self) -> &Path {
        &self.main_db
    }

    /// 全部分片路径（按 shard_id 升序）。
    pub fn shard_paths(&self) -> &[PathBuf] {
        &self.shard_paths
    }

    /// 指定分片路径。
    pub fn shard_path(&self, shard_id: u8) -> Option<&Path> {
        self.shard_paths
            .get(usize::from(shard_id))
            .map(PathBuf::as_path)
    }

    /// `events/shards.json`。
    pub fn shards_json(&self) -> &Path {
        &self.shards_json
    }

    /// `CURRENT` 指针路径。
    pub fn current_path(&self) -> &Path {
        &self.current_path
    }

    /// `clean_shutdown.json`。
    pub fn sidecar_path(&self) -> &Path {
        &self.sidecar_path
    }

    /// staging 目录（数据根）。
    pub fn staging_dir(&self) -> PathBuf {
        self.data_dir.join(STAGING_DIR_NAME)
    }

    /// 主库 + 全部分片路径（含主库在首位）。
    pub fn all_db_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(1 + self.shard_paths.len());
        paths.push(self.main_db.clone());
        paths.extend(self.shard_paths.iter().cloned());
        paths
    }
}

/// 打开或初始化数据目录（新装/开发库重建；不做旧扁平布局迁移）。
pub fn open_or_initialize(
    data_dir: impl AsRef<Path>,
    app_version: &str,
    now_ms: i64,
) -> Result<StorageLayout, StoreError> {
    let data_dir = normalize_data_dir(data_dir.as_ref())?;
    let current_path = data_dir.join(CURRENT_FILE_NAME);
    let flat_db = data_dir.join(MAIN_DB_NAME);
    let generations_dir = data_dir.join(GENERATIONS_DIR_NAME);

    let has_current = current_path.exists();
    let has_generations = generations_dir.exists();
    let has_flat = flat_db.exists();

    if !has_current && !has_generations && !has_flat {
        return initialize(&data_dir, app_version, now_ms);
    }

    if !has_current {
        // 旧扁平布局：开发阶段不支持就地迁移（ADR-039 D-039-6）。
        if has_flat {
            return Err(StoreError::LayoutInvalid {
                reason: format!(
                    "旧扁平布局 {} 存在但缺少 CURRENT；开发阶段不支持旧库迁移，请删除/移动数据目录后重建",
                    flat_db.display()
                ),
            });
        }
        // 存在 generations/ 但没有 CURRENT：若 journal 可恢复则恢复，否则标记半成品。
        if let Some(outcome) = recover_pending_generation(&data_dir)? {
            tracing::warn!(?outcome, "启动序列已处理 generation journal");
        }
        if data_dir.join(CURRENT_FILE_NAME).exists() {
            return read_existing(&data_dir);
        }
        let generations = list_generation_dirs(&generations_dir);
        return Err(StoreError::IncompleteGeneration { generations });
    }

    if has_flat {
        return Err(StoreError::LayoutInvalid {
            reason: format!(
                "数据目录同时存在旧扁平布局 {} 与 CURRENT；拒绝启动以避免双库歧义",
                flat_db.display()
            ),
        });
    }

    // 有 CURRENT：先处理中断 journal（若 CURRENT 已指向新 generation，则收尾）。
    if let Some(outcome) = recover_pending_generation(&data_dir)? {
        tracing::warn!(?outcome, "启动序列已处理 generation journal");
    }
    read_existing(&data_dir)
}

/// 新装初始化：创建 generation、主库 + 8 分片（完整迁移）、`shards.json`、`CURRENT`。
///
/// 任一步失败删除半成品 generation/staging；清理失败则拒绝启动并报告路径。
pub fn initialize(
    data_dir: &Path,
    app_version: &str,
    now_ms: i64,
) -> Result<StorageLayout, StoreError> {
    let data_dir = normalize_data_dir(data_dir)?;
    let generations_dir = data_dir.join(GENERATIONS_DIR_NAME);
    fs::create_dir_all(&generations_dir)?;

    // 初始化前清理 staging 中无 journal 引用的半成品（不删除 backups/attachments/logs）。
    cleanup_unreferenced_staging(&data_dir)?;

    let generation = format!("gen-{}", ulid::Ulid::new());
    if !is_valid_generation_id(&generation) {
        return Err(StoreError::LayoutInvalid {
            reason: format!("生成的 generation 不符合正则：{generation}"),
        });
    }
    let generation_dir = generations_dir.join(&generation);
    if generation_dir.exists() {
        return Err(StoreError::LayoutInvalid {
            reason: format!("generation 目录已存在：{}", generation_dir.display()),
        });
    }

    let result = create_generation(&data_dir, &generation, app_version, now_ms);
    if result.is_err() {
        // 清理半成品（best-effort；失败在错误中报告路径）。
        if let Err(cleanup) = fs::remove_dir_all(&generation_dir) {
            if generation_dir.exists() {
                return Err(StoreError::LayoutInvalid {
                    reason: format!(
                        "初始化失败且半成品 generation 清理失败（{}）：{cleanup}",
                        generation_dir.display()
                    ),
                });
            }
        }
    }
    result
}

fn create_generation(
    data_dir: &Path,
    generation: &str,
    app_version: &str,
    now_ms: i64,
) -> Result<StorageLayout, StoreError> {
    let generation_dir = data_dir.join(GENERATIONS_DIR_NAME).join(generation);
    let events_dir = generation_dir.join(EVENTS_DIR_NAME);
    fs::create_dir_all(&events_dir)?;

    let main_db = generation_dir.join(MAIN_DB_NAME);
    Store::create_new(&main_db)?;

    let mut shard_paths = Vec::with_capacity(SHARD_COUNT);
    for shard_id in 0..SHARD_COUNT {
        let path = generation_dir.join(shard_file_name(u8::try_from(shard_id).unwrap_or(0)));
        Store::create_new(&path)?;
        verify_shard_schema(&path)?;
        shard_paths.push(path);
    }

    let schema_version = program_schema_version(&main_db)?;
    let manifest = ShardsManifest {
        shard_count: SHARD_COUNT,
        shards: (0..SHARD_COUNT)
            .map(|index| {
                let id = u8::try_from(index).unwrap_or(0);
                ShardEntry {
                    id,
                    path: shard_file_name(id),
                    schema_version,
                }
            })
            .collect(),
        schema_version,
        app_version: app_version.to_owned(),
        created_at: now_ms,
    };
    let manifest_path = events_dir.join(SHARDS_MANIFEST_NAME);
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|error| StoreError::Internal {
            reason: format!("shards.json 序列化失败：{error}"),
        })?;
    write_file_atomic(&manifest_path, &manifest_bytes)?;

    let pointer = CurrentPointer {
        format_version: FORMAT_VERSION,
        generation: generation.to_owned(),
        shards_sha256: sha256_hex(&manifest_bytes),
        switched_at: now_ms,
    };
    write_current(data_dir, &pointer)?;

    Ok(StorageLayout {
        data_dir: data_dir.to_path_buf(),
        generation: generation.to_owned(),
        generation_dir: generation_dir.clone(),
        main_db,
        shard_paths,
        shards_json: manifest_path,
        current_path: data_dir.join(CURRENT_FILE_NAME),
        sidecar_path: generation_dir.join(CLEAN_SHUTDOWN_NAME),
    })
}

/// 读取并校验现存布局（`CURRENT` 必须存在）。
pub fn read_existing(data_dir: &Path) -> Result<StorageLayout, StoreError> {
    let data_dir = normalize_data_dir(data_dir)?;
    let current_path = data_dir.join(CURRENT_FILE_NAME);
    let bytes = fs::read(&current_path).map_err(|error| StoreError::LayoutInvalid {
        reason: format!("读取 CURRENT 失败（{}）：{error}", current_path.display()),
    })?;
    let pointer: CurrentPointer =
        serde_json::from_slice(&bytes).map_err(|error| StoreError::LayoutInvalid {
            reason: format!("CURRENT 不是合法 JSON：{error}"),
        })?;
    validate_pointer(&data_dir, &pointer)?;
    build_layout(&data_dir, &pointer)
}

fn validate_pointer(data_dir: &Path, pointer: &CurrentPointer) -> Result<(), StoreError> {
    if pointer.format_version != FORMAT_VERSION {
        return Err(StoreError::LayoutInvalid {
            reason: format!(
                "CURRENT.format_version={} 不受支持（期望 {FORMAT_VERSION}）",
                pointer.format_version
            ),
        });
    }
    if !is_valid_generation_id(&pointer.generation) {
        return Err(StoreError::LayoutInvalid {
            reason: format!("CURRENT.generation 不符合正则：{}", pointer.generation),
        });
    }
    let generations_root = data_dir.join(GENERATIONS_DIR_NAME);
    let generation_dir = generations_root.join(&pointer.generation);
    ensure_within(&generation_dir, &generations_root)?;
    if !generation_dir.is_dir() {
        return Err(StoreError::LayoutInvalid {
            reason: format!(
                "CURRENT 指向的 generation 不存在：{}",
                generation_dir.display()
            ),
        });
    }
    Ok(())
}

fn build_layout(data_dir: &Path, pointer: &CurrentPointer) -> Result<StorageLayout, StoreError> {
    let generation_dir = data_dir
        .join(GENERATIONS_DIR_NAME)
        .join(&pointer.generation);
    let shards_json = generation_dir
        .join(EVENTS_DIR_NAME)
        .join(SHARDS_MANIFEST_NAME);
    let manifest_bytes = fs::read(&shards_json).map_err(|error| StoreError::LayoutInvalid {
        reason: format!(
            "读取 shards.json 失败（{}）：{error}",
            shards_json.display()
        ),
    })?;
    let actual_hash = sha256_hex(&manifest_bytes);
    if actual_hash != pointer.shards_sha256 {
        return Err(StoreError::LayoutInvalid {
            reason: format!(
                "CURRENT.shards_sha256 与 shards.json 不匹配（记录 {} / 实际 {actual_hash}）",
                pointer.shards_sha256
            ),
        });
    }
    // `deny_unknown_fields` 含 `generation` 在内一律拒绝。
    let manifest: ShardsManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| StoreError::LayoutInvalid {
            reason: format!("shards.json 非法：{error}"),
        })?;
    if manifest.shard_count != SHARD_COUNT || manifest.shards.len() != SHARD_COUNT {
        return Err(StoreError::LayoutInvalid {
            reason: format!(
                "shards.json shard_count={} / shards={}（期望 {SHARD_COUNT}）",
                manifest.shard_count,
                manifest.shards.len()
            ),
        });
    }
    let mut shard_paths: Vec<PathBuf> = Vec::with_capacity(SHARD_COUNT);
    for index in 0..SHARD_COUNT {
        let id = u8::try_from(index).unwrap_or(0);
        let entry = manifest
            .shards
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| StoreError::LayoutInvalid {
                reason: format!("shards.json 缺少 shard_id={id}"),
            })?;
        if entry.path != shard_file_name(id) {
            return Err(StoreError::LayoutInvalid {
                reason: format!(
                    "shards[{}].path 必须是相对路径 {}（实际 {}）",
                    id,
                    shard_file_name(id),
                    entry.path
                ),
            });
        }
        let path = generation_dir.join(&entry.path);
        ensure_within(&path, &generation_dir)?;
        if !path.is_file() {
            return Err(StoreError::LayoutInvalid {
                reason: format!("分片文件缺失：{}", path.display()),
            });
        }
        shard_paths.push(path);
    }
    let main_db = generation_dir.join(MAIN_DB_NAME);
    if !main_db.is_file() {
        return Err(StoreError::LayoutInvalid {
            reason: format!("主库文件缺失：{}", main_db.display()),
        });
    }
    ensure_within(&main_db, &generation_dir)?;
    Ok(StorageLayout {
        data_dir: data_dir.to_path_buf(),
        generation: pointer.generation.clone(),
        generation_dir,
        main_db,
        shard_paths,
        shards_json,
        current_path: data_dir.join(CURRENT_FILE_NAME),
        sidecar_path: data_dir
            .join(GENERATIONS_DIR_NAME)
            .join(&pointer.generation)
            .join(CLEAN_SHUTDOWN_NAME),
    })
}

/// 启动不变量（主库 `events` 空、`sessions.shard_id`/`deletion_pending` 有效）。
///
/// 只读检查；数据库可打开但 `quick_check` 失败由调用方决定安全模式。
pub fn assert_invariants(layout: &StorageLayout) -> Result<(), StoreError> {
    let main = Store::open_read_only(layout.main_db())?;
    let events: i64 = main.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    if events != 0 {
        return Err(StoreError::ShardInvariant {
            reason: format!("主库 events 非空（{events} 行）；主库 events 必须恒空"),
        });
    }
    let mut statement = main.prepare("SELECT id, shard_id FROM sessions")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    for row in rows {
        let (session_id, shard_id) = row?;
        let Some(shard_id) = shard_id else {
            return Err(StoreError::ShardInvariant {
                reason: format!("会话 {session_id} 的 shard_id 为空"),
            });
        };
        let expected = i64::from(shard_for(&session_id));
        if shard_id != expected {
            return Err(StoreError::ShardInvariant {
                reason: format!(
                    "会话 {session_id} 的 shard_id={shard_id} 与稳定哈希 {expected} 不一致（会话不跨片）"
                ),
            });
        }
    }
    let pending: i64 = main.query_row(
        "SELECT COUNT(*) FROM sessions WHERE deletion_pending NOT IN (0,1)",
        [],
        |row| row.get(0),
    )?;
    if pending != 0 {
        return Err(StoreError::ShardInvariant {
            reason: format!("sessions.deletion_pending 存在非 0/1 值（{pending} 行）"),
        });
    }
    Ok(())
}

/// 待处理删除会话列表（启动重试；调用方在写者建立后执行 tombstone 收尾）。
pub fn pending_deletions(layout: &StorageLayout) -> Result<Vec<String>, StoreError> {
    let main = Store::open_read_only(layout.main_db())?;
    let mut statement =
        main.prepare("SELECT id FROM sessions WHERE deletion_pending = 1 ORDER BY id")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}

/// 写入 `clean_shutdown.json`（M10-09：generation 匹配；关闭序列完成后原子写）。
pub fn write_clean_shutdown(
    layout: &StorageLayout,
    sidecar: &CleanShutdown,
) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(sidecar).map_err(|error| StoreError::Internal {
        reason: format!("clean_shutdown 序列化失败：{error}"),
    })?;
    write_file_atomic(layout.sidecar_path(), &bytes)
}

/// 读取 `clean_shutdown.json`（缺失/损坏返回 `None`；由 M10-10 做严格跳过判定）。
pub fn read_clean_shutdown(layout: &StorageLayout) -> Option<CleanShutdown> {
    let bytes = fs::read(layout.sidecar_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 删除 sidecar（恢复/导入/降级/版本升级后失效）。
pub fn invalidate_clean_shutdown(layout: &StorageLayout) -> Result<(), StoreError> {
    match fs::remove_file(layout.sidecar_path()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StoreError::Io(error)),
    }
}

/// 数据库指纹（schema 版本、头 change counter、大小、WAL、last_seq）。
pub fn database_fingerprint(
    path: &Path,
    last_seq: Option<u64>,
) -> Result<DatabaseFingerprint, StoreError> {
    let connection = Store::open_read_only(path)?;
    let schema_version: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    drop(connection);
    Ok(DatabaseFingerprint {
        schema_version,
        header_change_counter: header_change_counter(path)?,
        file_size: file_size(path),
        wal_size: file_size(&sqlite_sidecar(path, "-wal")),
        last_seq,
    })
}

/// 分片当前位置最大 seq（写 sidecar 前读取；空片 → `None`）。
pub fn shard_last_seq(path: &Path) -> Result<Option<u64>, StoreError> {
    let connection = Store::open_read_only(path)?;
    let max: Option<i64> =
        connection.query_row("SELECT MAX(seq) FROM events", [], |row| row.get(0))?;
    match max {
        Some(value) => u64::try_from(value)
            .map(Some)
            .map_err(|_| StoreError::Internal {
                reason: format!("events.seq 为负: {value}"),
            }),
        None => Ok(None),
    }
}

/// SQLite 文件头偏移 24 的 4 字节 change counter。
pub fn header_change_counter(path: &Path) -> Result<u64, StoreError> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut header = [0u8; 28];
    file.read_exact(&mut header)?;
    Ok(u64::from(u32::from_be_bytes([
        header[24], header[25], header[26], header[27],
    ])))
}

/// 文件字节数（不存在 → 0）。
pub fn file_size(path: &Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

/// SQLite 侧车路径（`<db>-wal` / `<db>-shm`）。
pub fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

/// 当前 generation 主库 + 全部分片 `db+wal` 合计字节（`backup_list.capacity` 口径）。
pub fn generation_live_bytes(layout: &StorageLayout) -> u64 {
    layout
        .all_db_paths()
        .iter()
        .map(|path| file_size(path) + file_size(&sqlite_sidecar(path, "-wal")))
        .sum()
}

/// 当前 generation + `.pre-*` + `.failed-*` 总字节（容量巡检）。
pub fn generations_total_bytes(layout: &StorageLayout) -> u64 {
    list_generations(&layout.data_dir)
        .iter()
        .map(|info| info.bytes)
        .sum()
}

/// 列出数据目录下全部 generation（当前/pre/failed/incomplete）。
pub fn list_generations(data_dir: &Path) -> Vec<GenerationInfo> {
    let current = read_current(data_dir).map(|pointer| pointer.generation);
    let journal_generations = journal_generation_refs(data_dir);
    let generations_dir = data_dir.join(GENERATIONS_DIR_NAME);
    let Ok(entries) = fs::read_dir(&generations_dir) else {
        return Vec::new();
    };
    let mut infos = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let status = if current.as_deref() == Some(name.as_str()) {
            GenerationStatus::Current
        } else if name.starts_with(PRE_GENERATION_PREFIX) {
            GenerationStatus::Pre
        } else if name.starts_with(FAILED_GENERATION_PREFIX) {
            GenerationStatus::Failed
        } else {
            GenerationStatus::Incomplete
        };
        infos.push(GenerationInfo {
            path: path.clone(),
            status,
            bytes: directory_size(&path),
            journal_referenced: journal_generations.contains(&name),
        });
    }
    infos.sort_by(|left, right| left.path.cmp(&right.path));
    infos
}

/// 清理旧 generation（ADR-039 D-039-8 §1）：
/// 保留当前 + 最近 1 份 `.pre-*` + 最近 1 份 `.failed-*`；journal 引用不删除。
/// 返回实际删除的目录路径（失败仅告警，不阻断启动）。
pub fn cleanup_old_generations(data_dir: &Path, now_ms: i64) -> Vec<PathBuf> {
    let current = read_current(data_dir).map(|pointer| pointer.generation);
    if current.is_none() {
        return Vec::new();
    }
    let mut pre: Vec<GenerationInfo> = Vec::new();
    let mut failed: Vec<GenerationInfo> = Vec::new();
    for info in list_generations(data_dir) {
        if info.journal_referenced {
            continue;
        }
        match info.status {
            GenerationStatus::Pre => pre.push(info),
            GenerationStatus::Failed => failed.push(info),
            _ => {}
        }
    }
    // 新→旧排序（目录名含时间戳；不依赖入口顺序）。
    pre.sort_by(|left, right| right.path.cmp(&left.path));
    failed.sort_by(|left, right| right.path.cmp(&left.path));
    let mut removed = Vec::new();
    for info in pre.into_iter().skip(1).chain(failed.into_iter().skip(1)) {
        // 二次确认：不是 CURRENT、不被 journal 引用。
        if current.as_deref()
            == Some(
                info.path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default(),
            )
        {
            continue;
        }
        if journal_generation_refs(data_dir).iter().any(|name| {
            info.path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|candidate| candidate == name)
        }) {
            continue;
        }
        match fs::remove_dir_all(&info.path) {
            Ok(()) => removed.push(info.path),
            Err(error) => {
                tracing::warn!(path = %info.path.display(), error = %error, "旧 generation 清理失败（保留，不阻断）")
            }
        }
    }
    let _ = now_ms;
    removed
}

/// 读取 `CURRENT`（缺失/损坏 → `None`）。
pub fn read_current(data_dir: &Path) -> Option<CurrentPointer> {
    let bytes = fs::read(data_dir.join(CURRENT_FILE_NAME)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 原子写 `CURRENT`（临时文件 + fsync + 平台原子替换）。
pub fn write_current(data_dir: &Path, pointer: &CurrentPointer) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(pointer).map_err(|error| StoreError::Internal {
        reason: format!("CURRENT 序列化失败：{error}"),
    })?;
    let tmp = data_dir.join(format!(
        "{CURRENT_TEMP_PREFIX}{}-{}",
        std::process::id(),
        monotonic_nonce()
    ));
    write_synced(&tmp, &bytes)?;
    replace_file_atomic(&tmp, &data_dir.join(CURRENT_FILE_NAME)).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    sync_directory(data_dir);
    Ok(())
}

/// 写入 generation journal（原子）。
pub fn write_generation_journal(
    data_dir: &Path,
    journal: &GenerationJournal,
) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(journal).map_err(|error| StoreError::Internal {
        reason: format!("journal 序列化失败：{error}"),
    })?;
    let path = data_dir.join(journal_file_name(&journal.kind));
    let tmp = data_dir.join(format!(
        "{}.tmp-{}-{}",
        journal_file_name(&journal.kind),
        std::process::id(),
        monotonic_nonce()
    ));
    write_synced(&tmp, &bytes)?;
    replace_file_atomic(&tmp, &path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// 读取 generation journal（缺失 → `None`；损坏 → 错误）。
pub fn read_generation_journal(
    data_dir: &Path,
    kind: &str,
) -> Result<Option<GenerationJournal>, StoreError> {
    let path = data_dir.join(journal_file_name(kind));
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreError::Io(error)),
    };
    let journal: GenerationJournal =
        serde_json::from_slice(&bytes).map_err(|error| StoreError::GenerationJournalInvalid {
            path: path.clone(),
            reason: format!("journal 不是合法 JSON：{error}"),
        })?;
    if journal.v != JOURNAL_VERSION {
        return Err(StoreError::GenerationJournalInvalid {
            path,
            reason: format!("journal.v={} 不受支持", journal.v),
        });
    }
    Ok(Some(journal))
}

/// 删除 generation journal（提交/回滚收尾）。
pub fn remove_generation_journal(data_dir: &Path, kind: &str) -> Result<(), StoreError> {
    match fs::remove_file(data_dir.join(journal_file_name(kind))) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StoreError::Io(error)),
    }
}

/// journal 文件名（`restore` / `import`）。
pub fn journal_file_name(kind: &str) -> String {
    match kind {
        "restore" => RESTORE_JOURNAL_NAME.to_owned(),
        "import" => IMPORT_JOURNAL_NAME.to_owned(),
        other => format!("{other}-journal.json"),
    }
}

/// 清理无 journal 引用的 staging 半成品（初始化/启动）。
pub fn cleanup_unreferenced_staging(data_dir: &Path) -> Result<(), StoreError> {
    let staging = data_dir.join(STAGING_DIR_NAME);
    if !staging.is_dir() {
        return Ok(());
    }
    let has_restore = data_dir.join(RESTORE_JOURNAL_NAME).exists();
    let has_import = data_dir.join(IMPORT_JOURNAL_NAME).exists();
    for entry in fs::read_dir(&staging)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let referenced = (name.starts_with("restore-") && has_restore)
            || (name.starts_with("import-") && has_import);
        if !referenced {
            if let Err(error) = fs::remove_dir_all(entry.path()) {
                tracing::warn!(path = %entry.path().display(), error = %error, "staging 半成品清理失败");
            }
        }
    }
    Ok(())
}

/// generation 切换结果（启动恢复）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationRecovery {
    /// 无 journal。
    None,
    /// 已提交新 generation。
    Committed { generation: String },
    /// 已回滚整到旧 generation。
    RolledBack { generation: Option<String> },
    /// journal 损坏/新旧均不可用。
    Failed { reason: String },
}

fn recover_pending_generation(data_dir: &Path) -> Result<Option<GenerationRecovery>, StoreError> {
    for kind in ["restore", "import"] {
        let Some(journal) = read_generation_journal(data_dir, kind)? else {
            continue;
        };
        let current = read_current(data_dir);
        let staged = staging_generation_dir(data_dir, kind, &journal.new_generation);
        let final_new = data_dir
            .join(GENERATIONS_DIR_NAME)
            .join(&journal.new_generation);
        let old = journal
            .old_generation
            .as_deref()
            .map(|generation| data_dir.join(GENERATIONS_DIR_NAME).join(generation));

        // CURRENT 已指向新 generation：只做收尾/回滚。
        if current
            .as_ref()
            .is_some_and(|pointer| pointer.generation == journal.new_generation)
        {
            if generation_is_complete(&final_new) {
                finalize_old_generation(data_dir, kind, old.as_deref(), &journal)?;
                remove_generation_journal(data_dir, kind)?;
                invalidate_sidecar_at(&final_new);
                return Ok(Some(GenerationRecovery::Committed {
                    generation: journal.new_generation,
                }));
            }
            // 新库校验失败：回指 old（若可用），new 标记 `.failed-*`。
            if let (Some(old_path), Some(old_generation)) =
                (old.as_deref(), journal.old_generation.as_deref())
            {
                if generation_is_complete(old_path) {
                    let old_shards = old_path.join(EVENTS_DIR_NAME).join(SHARDS_MANIFEST_NAME);
                    let hash = sha256_file(&old_shards)?;
                    write_current(
                        data_dir,
                        &CurrentPointer {
                            format_version: FORMAT_VERSION,
                            generation: old_generation.to_owned(),
                            shards_sha256: hash,
                            switched_at: journal.requested_at,
                        },
                    )?;
                    mark_failed(&final_new);
                    remove_generation_journal(data_dir, kind)?;
                    return Ok(Some(GenerationRecovery::RolledBack {
                        generation: Some(old_generation.to_owned()),
                    }));
                }
            }
            return Err(StoreError::LayoutInvalid {
                reason: "journal 指向的新 generation 校验失败且旧 generation 不可用".to_owned(),
            });
        }

        // CURRENT 未指向新 generation：staged 阶段。
        // 恢复 journal 的 staged 是「待下次启动提交」的正常状态，留给 `boot_apply_pending_restore`
        // 处理（不在此丢弃）；导入 staged（切换前崩溃）回滚，既有库不变。
        if journal.phase == "staged" {
            if kind == "restore" {
                continue;
            }
            remove_dir_if_exists(&staged);
            remove_dir_if_exists(&final_new);
            remove_generation_journal(data_dir, kind)?;
            return Ok(Some(GenerationRecovery::RolledBack {
                generation: journal.old_generation.clone(),
            }));
        }
        // switched 但 CURRENT 未指向新（例如 CURRENT 写失败）：回滚。
        remove_dir_if_exists(&staged);
        mark_failed(&final_new);
        remove_generation_journal(data_dir, kind)?;
        return Ok(Some(GenerationRecovery::RolledBack {
            generation: journal.old_generation.clone(),
        }));
    }
    Ok(None)
}

/// 将 staging generation 提交为当前 generation（`CURRENT` 唯一提交点）。
///
/// 用于恢复启动序列/导入启动切换：调用方已校验 staging；本函数负责同卷移动到
/// `generations/<new>`、原子写 `CURRENT`、旧 generation 改名 `.pre-<kind>-<ts>`、
/// 失效新 sidecar、删除 journal。
pub fn commit_staged_generation(
    data_dir: &Path,
    kind: &str,
    journal: &GenerationJournal,
    now_ms: i64,
) -> Result<CurrentPointer, StoreError> {
    if !is_valid_generation_id(&journal.new_generation) {
        return Err(StoreError::LayoutInvalid {
            reason: format!("journal.new_generation 非法：{}", journal.new_generation),
        });
    }
    let staged = staging_generation_dir(data_dir, kind, &journal.new_generation);
    let final_new = data_dir
        .join(GENERATIONS_DIR_NAME)
        .join(&journal.new_generation);
    if !staged.is_dir() && !final_new.is_dir() {
        return Err(StoreError::LayoutInvalid {
            reason: format!("staging generation 不存在：{}", staged.display()),
        });
    }
    if staged.is_dir() {
        if final_new.exists() {
            return Err(StoreError::LayoutInvalid {
                reason: format!("目标 generation 已存在：{}", final_new.display()),
            });
        }
        fs::create_dir_all(data_dir.join(GENERATIONS_DIR_NAME))?;
        fs::rename(&staged, &final_new)?;
    }
    // 失效 staging/new 的 clean sidecar（恢复/导入手册：旧 sidecar 随指针失效）。
    invalidate_sidecar_at(&final_new);

    let shards_json = final_new.join(EVENTS_DIR_NAME).join(SHARDS_MANIFEST_NAME);
    let hash = sha256_file(&shards_json)?;
    if !journal.shards_sha256.is_empty() && hash != journal.shards_sha256 {
        return Err(StoreError::LayoutInvalid {
            reason: "staging generation shards.json 哈希与 journal 不一致".to_owned(),
        });
    }
    let pointer = CurrentPointer {
        format_version: FORMAT_VERSION,
        generation: journal.new_generation.clone(),
        shards_sha256: hash,
        switched_at: now_ms,
    };
    write_current(data_dir, &pointer)?;

    // 旧 generation 保留为 `.pre-<kind>-<ts>`（不回退删除）。
    if let Some(old_generation) = journal.old_generation.as_deref() {
        let old_path = data_dir.join(GENERATIONS_DIR_NAME).join(old_generation);
        if old_path.is_dir() {
            let target = data_dir.join(GENERATIONS_DIR_NAME).join(format!(
                "{PRE_GENERATION_PREFIX}{kind}-{now_ms}-{old_generation}"
            ));
            if let Err(error) = fs::rename(&old_path, &target) {
                tracing::warn!(path = %old_path.display(), error = %error, "旧 generation 改名失败（保留原路径）");
            }
        }
    }
    Ok(pointer)
}

fn finalize_old_generation(
    data_dir: &Path,
    kind: &str,
    old: Option<&Path>,
    journal: &GenerationJournal,
) -> Result<(), StoreError> {
    if let (Some(old), Some(old_generation)) = (old, journal.old_generation.as_deref()) {
        if old.is_dir() {
            let target = data_dir.join(GENERATIONS_DIR_NAME).join(format!(
                "{PRE_GENERATION_PREFIX}{kind}-{}-{old_generation}",
                journal.requested_at
            ));
            if !target.exists() {
                let _ = fs::rename(old, &target);
            }
        }
    }
    Ok(())
}

/// staging generation 目录（`staging/<kind>-<generation>/generation`）。
pub fn staging_generation_dir(data_dir: &Path, kind: &str, generation: &str) -> PathBuf {
    let job = if kind == "restore" {
        format!("restore-{generation}")
    } else {
        format!("import-{generation}")
    };
    data_dir.join(STAGING_DIR_NAME).join(job).join("generation")
}

fn mark_failed(path: &Path) {
    if !path.is_dir() {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "generation".to_owned());
    let target = parent.join(format!(
        "{FAILED_GENERATION_PREFIX}{}-{name}",
        monotonic_nonce()
    ));
    if let Err(error) = fs::rename(path, &target) {
        tracing::warn!(path = %path.display(), error = %error, "失败 generation 标记失败");
    }
}

fn invalidate_sidecar_at(generation_dir: &Path) {
    let _ = fs::remove_file(generation_dir.join(CLEAN_SHUTDOWN_NAME));
}

fn generation_is_complete(generation_dir: &Path) -> bool {
    if !generation_dir.join(MAIN_DB_NAME).is_file() {
        return false;
    }
    let shards_json = generation_dir
        .join(EVENTS_DIR_NAME)
        .join(SHARDS_MANIFEST_NAME);
    let Ok(bytes) = fs::read(&shards_json) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<ShardsManifest>(&bytes) else {
        return false;
    };
    manifest.shard_count == SHARD_COUNT
        && manifest.shards.len() == SHARD_COUNT
        && (0..SHARD_COUNT).all(|index| {
            let id = u8::try_from(index).unwrap_or(0);
            generation_dir.join(shard_file_name(id)).is_file()
        })
}

fn journal_generation_refs(data_dir: &Path) -> Vec<String> {
    let mut refs = Vec::new();
    for kind in ["restore", "import"] {
        if let Ok(Some(journal)) = read_generation_journal(data_dir, kind) {
            refs.push(journal.new_generation);
            if let Some(old) = journal.old_generation {
                refs.push(old);
            }
        }
    }
    refs
}

fn list_generation_dirs(generations_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(generations_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs
}

fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            total = total.saturating_add(directory_size(&entry_path));
        } else {
            total = total.saturating_add(file_size(&entry_path));
        }
    }
    total
}

fn verify_shard_schema(path: &Path) -> Result<(), StoreError> {
    let connection = Store::open_read_only(path)?;
    let version: i64 = connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    let expected = crate::migration::EMBEDDED_MIGRATIONS
        .last()
        .map(|migration| migration.version)
        .unwrap_or(0);
    if version != expected {
        return Err(StoreError::LayoutInvalid {
            reason: format!(
                "分片 schema 版本 v{version} 与程序 v{expected} 不一致（{}）",
                path.display()
            ),
        });
    }
    for object in ["events", "schema_migrations"] {
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
            [object],
            |row| row.get(0),
        )?;
        if count != 1 {
            return Err(StoreError::LayoutInvalid {
                reason: format!("分片缺少对象 {object}（{}）", path.display()),
            });
        }
    }
    Ok(())
}

fn program_schema_version(main_db: &Path) -> Result<i64, StoreError> {
    let connection = Store::open_read_only(main_db)?;
    Ok(connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?)
}

fn normalize_data_dir(data_dir: &Path) -> Result<PathBuf, StoreError> {
    if data_dir.as_os_str().is_empty() {
        return Err(StoreError::LayoutInvalid {
            reason: "数据目录为空".to_owned(),
        });
    }
    fs::create_dir_all(data_dir)?;
    Ok(fs::canonicalize(data_dir)?)
}

/// canonicalize 后前缀校验（越界拒绝；Windows 大小写不敏感由 `is_within` 口径保证）。
fn ensure_within(path: &Path, root: &Path) -> Result<(), StoreError> {
    let canonical_root = fs::canonicalize(root)?;
    let canonical_path = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) => {
            return Err(StoreError::LayoutInvalid {
                reason: format!("路径无法解析（{}）：{error}", path.display()),
            })
        }
    };
    let root_components: Vec<_> = canonical_root.components().collect();
    let path_components: Vec<_> = canonical_path.components().collect();
    let inside = path_components.len() >= root_components.len()
        && root_components
            .iter()
            .zip(path_components.iter())
            .all(|(left, right)| {
                #[cfg(windows)]
                {
                    left.as_os_str()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
                }
                #[cfg(not(windows))]
                {
                    left == right
                }
            });
    if !inside {
        return Err(StoreError::LayoutInvalid {
            reason: format!(
                "路径越界（{} 不在 {} 内；symlink/junction 解析后指向根外）",
                canonical_path.display(),
                canonical_root.display()
            ),
        });
    }
    Ok(())
}

fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_owned());
    let tmp = path.with_file_name(format!(
        "{file_name}.tmp-{}-{}",
        std::process::id(),
        monotonic_nonce()
    ));
    write_synced(&tmp, bytes)?;
    replace_file_atomic(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    if let Some(parent) = path.parent() {
        sync_directory(parent);
    }
    Ok(())
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) {
    // POSIX 可 fsync 目录；Windows 目录 fsync 不受支持（记录平台限制）。
    #[cfg(unix)]
    {
        if let Ok(file) = fs::File::open(path) {
            let _ = file.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// 平台原子替换（POSIX `rename(2)`；Windows 由 `std::fs::rename` 映射到
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`，命名空间层原子）。
///
/// `CURRENT.tmp` 在替换前已 `sync_all`（数据落盘）；目录 fsync 在 Windows 不受支持，
/// 平台限制记录于 M10-09 证据。不得删除旧文件再 rename。
pub fn replace_file_atomic(tmp: &Path, dest: &Path) -> Result<(), StoreError> {
    fs::rename(tmp, dest)?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<String, StoreError> {
    let bytes = fs::read(path)?;
    Ok(sha256_hex(&bytes))
}

fn remove_dir_if_exists(path: &Path) {
    if path.exists() {
        if let Err(error) = fs::remove_dir_all(path) {
            tracing::warn!(path = %path.display(), error = %error, "清理目录失败");
        }
    }
}

fn monotonic_nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("临时目录")
    }

    #[test]
    fn stable_shard_vectors_are_fixed() {
        let vectors = [
            ("01J8ZQ00000000000000000001", 2),
            ("01J8ZQ00000000000000000002", 7),
            ("01J8ZQ00000000000000000003", 4),
            ("01J8ZQ00000000000000000004", 1),
            ("01J8ZQ00000000000000000005", 6),
            ("01J8ZQ00000000000000000006", 3),
            ("01J8ZQ00000000000000000007", 0),
            ("01J8ZQ00000000000000000008", 5),
        ];
        for (session, expected) in vectors {
            assert_eq!(shard_for(session), expected, "session={session}");
        }
    }

    #[test]
    fn generation_id_regex_is_enforced() {
        assert!(is_valid_generation_id("gen-01J8ZQ00000000000000000001"));
        assert!(!is_valid_generation_id("gen-01j8zq00000000000000000001"));
        assert!(!is_valid_generation_id("gen-short"));
        assert!(!is_valid_generation_id("01J8ZQ00000000000000000001"));
    }

    #[test]
    fn initialize_creates_current_and_eight_shards() {
        let dir = temp_dir();
        let layout = initialize(dir.path(), "0.1.0", 1_700_000_000_000).expect("初始化");
        assert!(layout.main_db().is_file());
        assert_eq!(layout.shard_paths().len(), SHARD_COUNT);
        assert!(layout.shards_json().is_file());
        assert!(layout.current_path().is_file());
        let pointer = read_current(dir.path()).expect("CURRENT");
        assert_eq!(pointer.format_version, FORMAT_VERSION);
        assert_eq!(pointer.generation, layout.generation());
        let reopened = read_existing(dir.path()).expect("重新打开");
        assert_eq!(reopened, layout);
        let manifest: ShardsManifest =
            serde_json::from_slice(&fs::read(layout.shards_json()).expect("读")).expect("解析");
        assert_eq!(manifest.shard_count, SHARD_COUNT);
        assert!(!serde_json::to_string(&manifest)
            .expect("序列化")
            .contains("generation"));
    }

    #[test]
    fn logs_only_dir_still_initializes() {
        let dir = temp_dir();
        fs::create_dir_all(dir.path().join("logs")).expect("logs");
        fs::write(dir.path().join("logs/shell.log"), b"hello").expect("写日志");
        let layout = initialize(dir.path(), "0.1.0", 1).expect("仅 logs 仍能初始化");
        assert!(layout.main_db().is_file());
    }

    #[test]
    fn staging_without_journal_is_cleaned_on_initialize() {
        let dir = temp_dir();
        let staging = dir
            .path()
            .join(STAGING_DIR_NAME)
            .join("restore-gen-01J8ZQ00000000000000000001");
        fs::create_dir_all(staging.join("generation")).expect("staging");
        fs::write(staging.join("generation/aether.db"), b"half").expect("marker");
        let layout = initialize(dir.path(), "0.1.0", 1).expect("initialize");
        assert!(layout.main_db().is_file());
        assert!(!staging.exists(), "无 journal 的 staging 半成品必须清理");
    }

    #[test]
    fn generations_without_current_are_incomplete() {
        let dir = temp_dir();
        let layout = initialize(dir.path(), "0.1.0", 1).expect("初始化");
        fs::remove_file(layout.current_path()).expect("删除 CURRENT");
        let error = read_existing(dir.path()).expect_err("无 CURRENT 必须拒绝");
        match error {
            StoreError::LayoutInvalid { .. } => {}
            other => panic!("期望 LayoutInvalid，实际 {other:?}"),
        }
        let error = open_or_initialize(dir.path(), "0.1.0", 2).expect_err("半成品拒绝");
        assert!(matches!(error, StoreError::IncompleteGeneration { .. }));
    }

    #[test]
    fn shards_sha256_mismatch_is_rejected() {
        let dir = temp_dir();
        let layout = initialize(dir.path(), "0.1.0", 1).expect("初始化");
        let mut pointer = read_current(dir.path()).expect("CURRENT");
        pointer.shards_sha256 = "deadbeef".to_owned();
        write_current(dir.path(), &pointer).expect("写指针");
        let error = read_existing(dir.path()).expect_err("哈希不匹配拒绝");
        assert!(matches!(error, StoreError::LayoutInvalid { .. }));
        let _ = layout;
    }

    #[test]
    fn path_traversal_generation_is_rejected() {
        let dir = temp_dir();
        let _layout = initialize(dir.path(), "0.1.0", 1).expect("初始化");
        let pointer = CurrentPointer {
            format_version: FORMAT_VERSION,
            generation: "gen-../escape000000000000000".to_owned(),
            shards_sha256: String::new(),
            switched_at: 1,
        };
        write_current(dir.path(), &pointer).expect("写指针");
        let error = read_existing(dir.path()).expect_err("路径穿越拒绝");
        assert!(matches!(error, StoreError::LayoutInvalid { .. }));
    }

    #[test]
    fn shards_json_rejects_generation_field() {
        let dir = temp_dir();
        let layout = initialize(dir.path(), "0.1.0", 1).expect("初始化");
        let manifest: ShardsManifest =
            serde_json::from_slice(&fs::read(layout.shards_json()).expect("读")).expect("解析");
        let mut value = serde_json::to_value(&manifest).expect("值");
        value["generation"] = serde_json::json!("gen-01J8ZQ00000000000000000001");
        let bytes = serde_json::to_vec(&value).expect("字节");
        fs::write(layout.shards_json(), &bytes).expect("写");
        let mut pointer = read_current(dir.path()).expect("CURRENT");
        pointer.shards_sha256 = sha256_hex(&bytes);
        write_current(dir.path(), &pointer).expect("写指针");
        let error = read_existing(dir.path()).expect_err("generation 字段拒绝");
        assert!(matches!(error, StoreError::LayoutInvalid { .. }));
    }
}
