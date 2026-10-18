//! M5-03 集成测试：加密备份与恢复演练（设计 D13/D10/A3、ADR-020；P1 计划 §2 M5-03 DoD1–DoD5）。
//!
//! 覆盖：
//! - DoD1 加密产物检查：`AEBK1` 头（magic/version/flags/salt/nonce/来源 id/生成时间）与
//!   ADR-020 附录 A-2 一致；密钥材料（主密钥原始/hex、口令）扫描 0 命中（含阳性对照）；
//! - DoD2 演练：加密备份 → 清库（置空业务数据）→ 恢复 → `quick_check` + 行数/哈希一致；
//!   加密标记（`backups.encrypted`）与来源 id 正确；keychain 与 passphrase 两模式；
//! - DoD3 异常路径：口令/密钥错误 → `backup_decrypt_failed` 明确拒绝且现场三件套不被
//!   破坏；缺口令 → `backup_passphrase_required`；主密钥不可用 → `backup_key_unavailable`；
//!   A3 降级（加密文件 + 口令）行为按 ADR-020 断言（主密钥落 `secrets.enc`）；
//! - DoD4 恢复七步在加密路径逐项回归：解密候选 → 七步 2–6（含 `-wal`/`-shm` 现场改名、
//!   `.pre-restore` 现场保护、验收）与故障注入回滚；
//! - DoD5 诊断包/审计/恢复响应不含密钥材料（扫描 0 命中）。
//!
//! 演练不依赖外部凭证：主密钥存储使用测试内确定性实现或 A3 加密文件（口令模式）。
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aether_control::{Clock, ManualClock, SharedClock, StorageState};
use aether_security::{
    BackupCryptoParams, EncryptedFileStore, FileCryptoParams, SecretError, SecretStore, SecretValue,
};
use aether_store::backup::{
    execute_restore, program_schema_version, read_restore_journal, RestoreFault, RestoreRequest,
    RestoreStep, PRE_RESTORE_INFIX,
};
use aether_store::{Store, StoreCommand};
use aether_tauri::backup_auto::{AutoTrigger, BACKUP_KIND_MANUAL};
use aether_tauri::backup_control::{boot_apply_pending_restore, BackupControlBackend, SpaceProbe};
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::diagnostics_control::{
    DiagnosticsControlBackend, DiagnosticsDeps, BACKUP_ENCRYPTION_ENABLED_KEY,
};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    BackupCreateRequest, BackupRestoreRequest, BackupSource, ExportDiagnosticsRequest,
};
use aether_tauri::security_level::{SecurityLevelView, SecurityProbe, SECURITY_LEVEL_OS};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// 演练口令（仅测试夹具；不依赖外部凭证）。
const DRILL_PASSPHRASE: &str = "m5-03-drill-passphrase";
/// A3 加密文件口令（降级路径演练）。
const A3_PASSPHRASE: &str = "m5-03-a3-passphrase";

// ===== 通用夹具 =====

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn seed_database(path: &Path, rows: i64) {
    let store = Store::open(path).expect("打开/迁移数据库");
    {
        let connection = store.connection();
        connection
            .execute_batch(
                "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
                 VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1);",
            )
            .expect("插入运行时");
        for index in 0..rows {
            connection
                .execute_batch(&format!(
                    "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                     VALUES ('01J{index:023}', 'mock', '会话 {index}', 'idle', {created}, {created});",
                    index = index,
                    created = 1_700_000_000_000i64 + index
                ))
                .expect("插入会话");
        }
    }
}

fn session_count(path: &Path) -> i64 {
    let store = Store::open(path).expect("打开数据库");
    store
        .connection()
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .expect("计数")
}

fn fingerprint(path: &Path) -> String {
    let store = Store::open(path).expect("指纹连接");
    let connection = store.connection();
    connection
        .execute_batch("PRAGMA query_only = ON;")
        .expect("只读查询");
    let mut hasher = Sha256::new();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .expect("计数");
    hasher.update(format!("sessions:{count};").as_bytes());
    let mut statement = connection
        .prepare("SELECT id, title FROM sessions ORDER BY id")
        .expect("抽查语句");
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("抽查");
    for row in rows {
        let (id, title) = row.expect("行");
        hasher.update(format!("{id}\u{1f}{title}\u{1e}").as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// 文件字节读取（现场哈希/产物扫描）。
fn file_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("读取文件")
}

/// 长路径形式（模拟真实系统选择器返回的完整路径）。
///
/// CI 的 `%TEMP%` 形如 `C:\Users\RUNNER~1\...`，含 8.3 短名——短名是 D9/T7 的
/// **拒绝样本**（`reject_windows_special_forms` 在 canonicalize 之前做形态拒绝），
/// 不能直接作为合法输入；夹具先 canonicalize 并去掉 `\\?\` 前缀（同
/// `m3_04_backup_ipc.rs` 的 `long_path` 口径）。
fn long_path(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).expect("canonicalize 夹具路径");
    let text = canonical.to_string_lossy().to_string();
    #[cfg(windows)]
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        return stripped.to_string();
    }
    text
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// 现场快照（哈希；「不被破坏」断言的证据）。
///
/// 仅覆盖 `db` 与 `-wal`（`-shm` 为 SQLite 共享内存临时文件，任意读连接都会
/// 触碰其内容，不构成「现场破坏」判定面；恢复引擎的 `-shm` 处理由 DoD4 断言）。
fn scene_snapshot(db: &Path) -> Value {
    let mut object = serde_json::Map::new();
    for (name, path) in [
        ("db", db.to_path_buf()),
        ("wal", PathBuf::from(format!("{}-wal", db.display()))),
    ] {
        object.insert(
            name.to_owned(),
            if path.exists() {
                json!(sha256_hex(&file_bytes(&path)))
            } else {
                Value::Null
            },
        );
    }
    Value::Object(object)
}

fn pre_restore_files(db: &Path) -> Vec<PathBuf> {
    let Some(dir) = db.parent() else {
        return Vec::new();
    };
    let name = db
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|listing| {
            listing
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .map(|file| file.to_string_lossy().starts_with(&name))
                        .unwrap_or(false)
                        && path
                            .file_name()
                            .map(|file| file.to_string_lossy().contains(PRE_RESTORE_INFIX))
                            .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn write_evidence(name: &str, value: &Value) {
    println!("[m5-03] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M5_03_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(text) = serde_json::to_string_pretty(value) else {
        return;
    };
    let _ = std::fs::write(dir.join(format!("{name}.json")), text);
}

/// 子串扫描（密钥材料扫描的原子操作）。
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// 密钥材料扫描（返回命中清单；阳性对照保证扫描器本身有效）。
fn scan_secret_material(haystack: &[u8], needles: &[(&str, Vec<u8>)]) -> Vec<String> {
    // 阳性对照：扫描器必须能发现拼接进语料的样本。
    for (_, needle) in needles {
        let mut corpus = b"prefix-".to_vec();
        corpus.extend_from_slice(needle);
        corpus.extend_from_slice(b"-suffix");
        assert!(
            contains_bytes(&corpus, needle),
            "阳性对照失败：扫描器未命中样本"
        );
    }
    needles
        .iter()
        .filter(|(_, needle)| contains_bytes(haystack, needle))
        .map(|(label, _)| (*label).to_owned())
        .collect()
}

/// 确定性内存密钥存储（演练不依赖外部凭证）。
#[derive(Default)]
struct MemoryStore {
    map: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    fn snapshot(&self) -> HashMap<String, String> {
        self.map.lock().expect("内存存储锁").clone()
    }
}

impl SecretStore for MemoryStore {
    fn level(&self) -> aether_security::SecurityLevel {
        aether_security::SecurityLevel::OsKeychain
    }

    fn get(&self, reference: &aether_security::KeychainRef) -> Result<SecretValue, SecretError> {
        self.map
            .lock()
            .expect("内存存储锁")
            .get(&reference.to_uri())
            .map(|value| SecretValue::new(value.clone()))
            .ok_or_else(|| SecretError::NotFound {
                reference: reference.clone(),
            })
    }

    fn set(
        &self,
        reference: &aether_security::KeychainRef,
        value: &SecretValue,
    ) -> Result<(), SecretError> {
        self.map
            .lock()
            .expect("内存存储锁")
            .insert(reference.to_uri(), value.expose().to_owned());
        Ok(())
    }

    fn delete(&self, reference: &aether_security::KeychainRef) -> Result<(), SecretError> {
        self.map
            .lock()
            .expect("内存存储锁")
            .remove(&reference.to_uri());
        Ok(())
    }
}

/// 固定空间探针（足够空间；不触碰真实磁盘探测）。
struct FixedSpaceProbe;

impl SpaceProbe for FixedSpaceProbe {
    fn ensure_writable(&self, _dir: &Path) -> Result<(), String> {
        Ok(())
    }

    fn available_bytes(&self, _dir: &Path) -> Result<u64, String> {
        Ok(u64::MAX)
    }
}

struct StaticSecurityProbe;

impl SecurityProbe for StaticSecurityProbe {
    fn status(&self) -> SecurityLevelView {
        SecurityLevelView {
            level: SECURITY_LEVEL_OS.to_owned(),
            detail: "注入：自检通过".to_owned(),
        }
    }
}

struct Fixture {
    temp: TempDir,
    runtime: tokio::runtime::Runtime,
    boot: CoreBoot,
    clock: Arc<ManualClock>,
    backup: Arc<BackupControlBackend>,
    diagnostics: Arc<DiagnosticsControlBackend>,
    store: Arc<MemoryStore>,
}

impl Fixture {
    fn open() -> Self {
        Self::open_with_store(Some(Arc::new(MemoryStore::default())))
    }

    fn open_with_store(store: Option<Arc<MemoryStore>>) -> Self {
        let temp = tempfile::tempdir().expect("临时数据目录");
        seed_database(&temp.path().join("aether.db"), 64);
        let runtime = new_runtime();
        let handle = runtime.handle().clone();
        let boot = boot_core_full(
            temp.path(),
            &handle,
            Arc::new(StaticRuntimeSummaries::unwired()),
        )
        .expect("启动核心（存储 + 管线）");
        let clock = Arc::new(ManualClock::new(1_800_000_000_000));
        let clock_shared: SharedClock = clock.clone();
        let secrets = store
            .as_ref()
            .map(|store| Arc::clone(store) as Arc<dyn SecretStore>);
        let backup_backend = BackupControlBackend::new(
            Arc::new(NotImplementedBackend),
            temp.path().to_path_buf(),
            Some(boot.reads.clone()),
            Some(boot.write.clone()),
            handle.clone(),
            secrets,
        )
        .with_space_probe(Arc::new(FixedSpaceProbe))
        .with_crypto_params(BackupCryptoParams::test_only_fast())
        .with_clock(clock_shared.clone());
        let backup = Arc::new(backup_backend);
        let deps = DiagnosticsDeps {
            data_dir: temp.path().to_path_buf(),
            reads: Some(boot.reads.clone()),
            write: Some(boot.write.clone()),
            handle: handle.clone(),
            health: aether_tauri::core_health::HealthProvider::new(
                Arc::new(aether_tauri::core_health::StaticHealthSource::new(
                    aether_tauri::core_health::StorageHealthSnapshot {
                        storage_state: "normal".to_owned(),
                        write_queue_depth: 0,
                        degrade_trigger: None,
                        degraded_since_ms: None,
                        detail: None,
                    },
                )),
                Arc::new(StaticRuntimeSummaries::unwired()),
            ),
            logs: None,
            task_dumps: None,
            session_manager: None,
            security: Some(Arc::new(StaticSecurityProbe)),
        };
        let diagnostics = Arc::new(
            DiagnosticsControlBackend::new(Arc::clone(&backup) as Arc<dyn IpcBackend>, deps)
                .with_clock(clock_shared),
        );
        Self {
            temp,
            runtime,
            boot,
            clock,
            backup,
            diagnostics,
            store: store.unwrap_or_else(|| Arc::new(MemoryStore::default())),
        }
    }

    fn db_path(&self) -> PathBuf {
        self.temp.path().join("aether.db")
    }

    fn backups_dir(&self) -> PathBuf {
        self.temp.path().join("backups")
    }

    fn create(
        &self,
        passphrase: Option<&str>,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backup.backup_create(
            &BackupCreateRequest {
                label: None,
                target_dir: None,
                passphrase: passphrase.map(str::to_owned),
            },
            None,
        )
    }

    fn restore_internal(
        &self,
        id: &str,
        passphrase: Option<&str>,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backup.backup_restore(
            &BackupRestoreRequest {
                source: BackupSource::Internal { id: id.to_owned() },
                passphrase: passphrase.map(str::to_owned),
            },
            None,
        )
    }

    fn auto(&self) -> Arc<aether_tauri::backup_auto::AutoBackup> {
        self.backup.auto_backup().expect("调度器已接线")
    }

    /// 以指定密钥存储重建备份后端（测试：主密钥不可用 / A3 降级注入）。
    fn backend_with_secrets(
        &self,
        secrets: Option<Arc<dyn SecretStore>>,
    ) -> Arc<BackupControlBackend> {
        let clock: SharedClock = self.clock.clone();
        Arc::new(
            BackupControlBackend::new(
                Arc::new(NotImplementedBackend),
                self.temp.path().to_path_buf(),
                Some(self.boot.reads.clone()),
                Some(self.boot.write.clone()),
                self.runtime.handle().clone(),
                secrets,
            )
            .with_space_probe(Arc::new(FixedSpaceProbe))
            .with_crypto_params(BackupCryptoParams::test_only_fast())
            .with_clock(clock),
        )
    }

    fn audit_records(&self) -> Vec<aether_store::AuditLogRecord> {
        self.runtime
            .block_on(self.boot.reads.audit_log(100))
            .expect("审计查询")
    }

    fn setting(&self, key: &str) -> Value {
        self.runtime
            .block_on(self.boot.reads.setting(key))
            .expect("设置读取")
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or(Value::Null)
    }

    /// 关闭存储（模拟应用退出；恢复执行前必须无写者）。
    fn shutdown_storage(&mut self) {
        let _ = self.runtime.block_on(self.boot.pipeline.shutdown());
        if let Some(storage) = self.boot.storage.take() {
            let _ = self.runtime.block_on(storage.shutdown());
        }
    }

    fn shutdown(mut self) {
        self.shutdown_storage();
    }
}

// ===== DoD1：加密产物格式与密钥材料扫描 =====

#[test]
fn dod1_artifact_format_matches_adr_and_secret_scan_is_clean() {
    let fixture = Fixture::open();
    let db = fixture.db_path();

    // 主密钥模式产物。
    let keychain_created = fixture.create(None).expect("主密钥模式备份");
    let keychain_record = &keychain_created["backup"];
    assert_eq!(keychain_record["encrypted"], true);
    assert_eq!(keychain_record["kind"], BACKUP_KIND_MANUAL);
    let keychain_path = PathBuf::from(keychain_record["path"].as_str().expect("路径"));
    assert_eq!(
        keychain_path.extension().and_then(|value| value.to_str()),
        Some("aebk"),
        "加密产物扩展名"
    );

    // 口令模式产物。
    let passphrase_created = fixture
        .create(Some(DRILL_PASSPHRASE))
        .expect("口令模式备份");
    let passphrase_record = &passphrase_created["backup"];
    let passphrase_path = PathBuf::from(passphrase_record["path"].as_str().expect("路径"));

    // 头格式（ADR-020 附录 A-2）：magic/version/flags 恰置其一/salt 仅口令模式。
    let keychain_bytes = file_bytes(&keychain_path);
    assert_eq!(&keychain_bytes[..5], b"AEBK1");
    assert_eq!(keychain_bytes[5], 1, "version");
    assert_eq!(keychain_bytes[6], 0x02, "keychain 模式 flags");
    // keychain 模式：flags 后紧跟 nonce(24) + id_len(2) + id(26) + 生成时间(8)。
    let keychain_header_len = 5 + 1 + 1 + 24 + 2 + 26 + 8;
    assert!(
        keychain_bytes.len() > keychain_header_len + 16,
        "密文 + tag 在场"
    );

    let passphrase_bytes = file_bytes(&passphrase_path);
    assert_eq!(passphrase_bytes[6], 0x01, "passphrase 模式 flags");
    let salt_offset = 7;
    assert!(
        passphrase_bytes.len() >= salt_offset + 16 + 24 + 2 + 26 + 8 + 16,
        "口令模式头（salt/nonce/来源/时间）+ 密文 + tag 在场"
    );

    // 密钥材料扫描（含阳性对照）：产物不含主密钥（raw/hex）与口令。
    let store_snapshot = fixture.store.snapshot();
    let master_hex = store_snapshot
        .get("keychain://aether/backup/master")
        .expect("主密钥已生成并落存储");
    let master_raw = hex::decode(master_hex).expect("主密钥 hex");
    let needles: Vec<(&str, Vec<u8>)> = vec![
        ("master_key_hex", master_hex.as_bytes().to_vec()),
        ("master_key_raw", master_raw.clone()),
        ("passphrase", DRILL_PASSPHRASE.as_bytes().to_vec()),
    ];
    let keychain_hits = scan_secret_material(&keychain_bytes, &needles);
    let passphrase_hits = scan_secret_material(&passphrase_bytes, &needles);
    assert!(
        keychain_hits.is_empty(),
        "主密钥模式产物命中：{keychain_hits:?}"
    );
    assert!(
        passphrase_hits.is_empty(),
        "口令模式产物命中：{passphrase_hits:?}"
    );

    write_evidence(
        "dod1_artifact_format",
        &json!({
            "keychain": {
                "path": keychain_path.to_string_lossy(),
                "bytes": keychain_bytes.len(),
                "flags": keychain_bytes[6],
                "version": keychain_bytes[5],
                "magic": String::from_utf8_lossy(&keychain_bytes[..5]),
                "secret_scan_hits": keychain_hits,
            },
            "passphrase": {
                "path": passphrase_path.to_string_lossy(),
                "bytes": passphrase_bytes.len(),
                "flags": passphrase_bytes[6],
                "salt_len": 16,
                "secret_scan_hits": passphrase_hits,
            },
            "master_key_ref": "keychain://aether/backup/master",
            "scan_positive_control": true,
            "db": db.to_string_lossy(),
        }),
    );
    fixture.shutdown();
}

// ===== DoD2：加密备份 → 清库 → 恢复 → 一致（演练主路径） =====

fn drill_roundtrip(passphrase: Option<&str>, evidence_name: &str) {
    let mut fixture = Fixture::open();
    let db = fixture.db_path();
    let before_fingerprint = fingerprint(&db);
    let before_count = session_count(&db);

    let created = fixture.create(passphrase).expect("加密备份");
    let record = created["backup"].clone();
    assert_eq!(record["encrypted"], true, "加密标记");
    let artifact = PathBuf::from(record["path"].as_str().expect("路径"));
    let backup_id = record["id"].as_str().expect("id").to_owned();

    // 来源 id 与加密标记正确（产物头 = 台账 id）。
    let info = aether_security::inspect_backup_artifact(&artifact).expect("产物头解析");
    assert_eq!(info.source_backup_id, backup_id, "来源 id 与台账一致");
    assert_eq!(
        info.mode,
        if passphrase.is_some() {
            aether_security::BackupArtifactMode::Passphrase
        } else {
            aether_security::BackupArtifactMode::Keychain
        }
    );

    // 恢复请求（解密 → 候选校验 → 现场日志登记）。
    let restored = fixture
        .restore_internal(&backup_id, passphrase)
        .expect("恢复请求登记");
    assert_eq!(restored["candidate"]["encrypted"], true);
    assert_eq!(restored["candidate"]["source_backup_id"], backup_id);
    let journal = read_restore_journal(&db)
        .expect("读取 journal")
        .expect("journal 存在");
    assert_eq!(journal.status, "requested");
    let candidate = PathBuf::from(&journal.candidate_path);
    assert!(
        candidate
            .file_name()
            .map(|name| name.to_string_lossy().contains(".restore-cand-"))
            .unwrap_or(false),
        "解密候选临时文件"
    );

    // 清库（T9 语义：删除库与侧车，模拟数据目录丢失；恢复后必须与备份时一致）。
    fixture.shutdown_storage();
    for path in [
        db.clone(),
        PathBuf::from(format!("{}-wal", db.display())),
        PathBuf::from(format!("{}-shm", db.display())),
    ] {
        if path.exists() {
            std::fs::remove_file(&path).expect("清库");
        }
    }
    assert!(!db.exists(), "清库后库文件不存在");

    // 启动序列执行七步 3–6（无写者窗口）。
    let outcome = boot_apply_pending_restore(fixture.temp.path()).expect("启动处理恢复");
    let report = match outcome {
        aether_store::RestoreStartupOutcome::Applied(report) => report,
        other => panic!("恢复未完成：{other:?}"),
    };
    assert!(report.applied && !report.rolled_back);
    let step_codes: Vec<&str> = report.steps.iter().map(|step| step.step.as_str()).collect();
    assert_eq!(
        step_codes,
        vec![
            "validate_candidate",
            "stop_writes",
            "protect_scene",
            "place_candidate",
            "verify"
        ],
        "恢复七步 2–6 逐项"
    );

    // 验收：quick_check + 行数/哈希一致。
    let store = Store::open(&db).expect("恢复后打开");
    assert!(store.quick_check().ok, "恢复后 quick_check");
    assert_eq!(session_count(&db), before_count, "行数一致");
    assert_eq!(fingerprint(&db), before_fingerprint, "哈希抽查一致");

    // 解密候选随恢复收尾清理。
    assert!(!candidate.exists(), "解密候选随恢复收尾清理");
    assert!(read_restore_journal(&db).unwrap().is_none(), "journal 收尾");

    write_evidence(
        evidence_name,
        &json!({
            "backup_id": backup_id,
            "artifact": artifact.to_string_lossy(),
            "artifact_bytes": record["size_bytes"],
            "artifact_encrypted": record["encrypted"],
            "artifact_mode": if passphrase.is_some() { "passphrase" } else { "keychain" },
            "source_backup_id": info.source_backup_id,
            "candidate_path": candidate.to_string_lossy(),
            "steps": step_codes,
            "fingerprint_before": before_fingerprint,
            "fingerprint_after": fingerprint(&db),
            "sessions_before": before_count,
            "sessions_after": session_count(&db),
            "quick_check_ok": true,
            "restart_required": restored["restart_required"],
            "candidate_response": restored["candidate"],
        }),
    );
    fixture.shutdown();
}

#[test]
fn dod2_restore_drill_keychain_mode() {
    drill_roundtrip(None, "dod2_drill_keychain");
}

#[test]
fn dod2_restore_drill_passphrase_mode() {
    drill_roundtrip(Some(DRILL_PASSPHRASE), "dod2_drill_passphrase");
}

// ===== DoD3：异常路径（拒绝且现场三件套不被破坏；A3 降级） =====

#[test]
fn dod3_wrong_credentials_reject_without_touching_scene() {
    let fixture = Fixture::open();
    let db = fixture.db_path();

    // 口令模式产物：缺口令 / 错口令。
    let created = fixture
        .create(Some(DRILL_PASSPHRASE))
        .expect("口令模式备份");
    let passphrase_id = created["backup"]["id"].as_str().expect("id").to_owned();

    // 主密钥模式产物：安全存储不可用（无 Keychain / 无 A3 口令）。
    let keychain_created = fixture.create(None).expect("主密钥模式备份");
    let keychain_id = keychain_created["backup"]["id"]
        .as_str()
        .expect("id")
        .to_owned();

    // 备份登记完成后取现场快照（台账写入会改动库文件）。
    let before_scene = scene_snapshot(&db);

    let missing = fixture.restore_internal(&passphrase_id, None).unwrap_err();
    assert!(
        missing.message.contains("backup_passphrase_required"),
        "缺口令业务码：{}",
        missing.message
    );
    let wrong = fixture
        .restore_internal(&passphrase_id, Some("wrong-passphrase"))
        .unwrap_err();
    assert!(
        wrong.message.contains("backup_decrypt_failed"),
        "错口令业务码：{}",
        wrong.message
    );

    let no_store = fixture.backend_with_secrets(None);
    let unavailable = no_store
        .backup_restore(
            &BackupRestoreRequest {
                source: BackupSource::Internal {
                    id: keychain_id.clone(),
                },
                passphrase: None,
            },
            None,
        )
        .unwrap_err();
    assert!(
        unavailable.message.contains("backup_key_unavailable"),
        "主密钥不可用业务码：{}",
        unavailable.message
    );

    // 篡改产物头（flags 后一位）→ tag 校验失败；现场三件套哈希不变、无 journal。
    let artifact = PathBuf::from(keychain_created["backup"]["path"].as_str().expect("路径"));
    let mut tampered = file_bytes(&artifact);
    tampered[6] ^= 0x01;
    let tampered_path = fixture.backups_dir().join("tampered.aebk");
    std::fs::write(&tampered_path, &tampered).expect("写篡改产物");
    let tampered_error = no_store
        .backup_restore(
            &BackupRestoreRequest {
                source: BackupSource::External {
                    path: tampered_path.to_string_lossy().to_string(),
                },
                passphrase: None,
            },
            Some(&tampered_path),
        )
        .unwrap_err();
    assert!(
        tampered_error.message.contains("backup_candidate_invalid")
            || tampered_error.message.contains("backup_decrypt_failed"),
        "篡改产物拒绝：{}",
        tampered_error.message
    );

    assert_eq!(scene_snapshot(&db), before_scene, "现场三件套不被破坏");
    assert!(
        read_restore_journal(&db).unwrap().is_none(),
        "拒绝路径不登记 journal"
    );

    // 失败不产生半成品（无 `.restore-cand-*` 残留）。
    let residue: Vec<String> = std::fs::read_dir(fixture.temp.path())
        .expect("目录")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains(".restore-cand-"))
        .collect();
    assert!(residue.is_empty(), "无解密候选残留：{residue:?}");

    write_evidence(
        "dod3_reject_paths",
        &json!({
            "missing_passphrase": missing.message,
            "wrong_passphrase": wrong.message,
            "key_unavailable": unavailable.message,
            "tampered_artifact": tampered_error.message,
            "scene_before": before_scene,
            "scene_after": scene_snapshot(&db),
            "journal_absent": true,
            "candidate_residue": residue,
        }),
    );
    fixture.shutdown();
}

/// A3 降级（Keychain 不可用 + 加密文件口令）：主密钥落 `secrets.enc`；备份/恢复可用。
#[test]
fn dod3_a3_degraded_store_supports_encrypted_backup_and_restore() {
    let dir = tempfile::tempdir().expect("A3 目录");
    let secrets_file = dir.path().join("secrets.enc");
    let file_store = Arc::new(
        EncryptedFileStore::open_or_create(
            &secrets_file,
            &SecretValue::new(A3_PASSPHRASE),
            FileCryptoParams::test_only_fast(),
        )
        .expect("创建 A3 加密文件"),
    );
    assert!(secrets_file.exists(), "降级路径创建 secrets.enc");

    let fixture = Fixture::open();
    let backend =
        fixture.backend_with_secrets(Some(Arc::clone(&file_store) as Arc<dyn SecretStore>));
    let db = fixture.db_path();
    let before = fingerprint(&db);

    let created = backend
        .backup_create(
            &BackupCreateRequest {
                label: None,
                target_dir: None,
                passphrase: None,
            },
            None,
        )
        .expect("A3 降级加密备份");
    let record = created["backup"].clone();
    assert_eq!(record["encrypted"], true);
    let backup_id = record["id"].as_str().expect("id").to_owned();

    // 模拟重启：以同一 A3 口令重新打开加密文件（主密钥可恢复）。
    let reopened = Arc::new(
        EncryptedFileStore::open(&secrets_file, &SecretValue::new(A3_PASSPHRASE))
            .expect("重新打开 A3 加密文件"),
    );
    let backend_after_restart =
        fixture.backend_with_secrets(Some(Arc::clone(&reopened) as Arc<dyn SecretStore>));
    let restored = backend_after_restart
        .backup_restore(
            &BackupRestoreRequest {
                source: BackupSource::Internal {
                    id: backup_id.clone(),
                },
                passphrase: None,
            },
            None,
        )
        .expect("A3 降级恢复请求");
    assert_eq!(restored["candidate"]["encrypted"], true);
    assert_eq!(restored["candidate"]["source_backup_id"], backup_id);

    // 口令错误 → 打不开 A3 文件（明确拒绝，不静默重建）。
    let wrong = EncryptedFileStore::open(&secrets_file, &SecretValue::new("wrong-a3"));
    assert!(wrong.is_err(), "A3 口令错误必须明确拒绝");

    // 主密钥确实落在 A3 文件（引用 URI 在密文内，不可从文件明文断言；用解密后的
    // 内存实例读取验证——同一实例即证据）。
    assert!(
        reopened
            .get(&aether_security::backup_master_key_ref().expect("引用"))
            .is_ok(),
        "主密钥经 A3 加密文件可读"
    );

    write_evidence(
        "dod3_a3_degraded",
        &json!({
            "secrets_file": secrets_file.to_string_lossy(),
            "artifact": record["path"],
            "artifact_encrypted": record["encrypted"],
            "restore_candidate": restored["candidate"],
            "wrong_a3_passphrase_rejected": true,
            "fingerprint_before": before,
            "master_key_ref": "keychain://aether/backup/master",
        }),
    );
    fixture.shutdown();
}

/// 自动备份：加密开启 + 主密钥不可用 → 暂停并告警（不产出明文，不误入 persist_degraded）。
#[test]
fn dod3_auto_backup_pauses_when_key_unavailable() {
    let fixture = Fixture::open_with_store(None);
    assert_eq!(
        fixture.setting(BACKUP_ENCRYPTION_ENABLED_KEY),
        Value::Null,
        "设置未写入 → 缺省语义（加密开启）"
    );

    let report = fixture
        .runtime
        .block_on(fixture.auto().run_once(AutoTrigger::Initial));
    assert_eq!(report.result, "failed");
    assert!(
        report
            .error
            .as_deref()
            .unwrap_or("")
            .contains("backup_key_unavailable"),
        "业务码：{:?}",
        report.error
    );
    let leftover: Vec<String> = std::fs::read_dir(fixture.backups_dir())
        .map(|listing| {
            listing
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !fixture.backups_dir().exists() || leftover.is_empty(),
        "不产出明文产物：{leftover:?}"
    );
    let list = fixture.diagnostics.backup_list().expect("清单");
    assert_eq!(list["auto"]["last_result"], "failed");
    assert_eq!(list["auto"]["consecutive_failures"], 1);
    assert!(list["auto"]["next_run_at"].as_i64().is_some());
    let audit = fixture.audit_records();
    assert!(
        audit.iter().any(|record| {
            record.action == aether_tauri::backup_auto::AUDIT_ACTION_AUTO_BACKUP
                && record.result.as_deref() == Some("failed")
        }),
        "失败审计落库（告警）"
    );
    assert_eq!(
        fixture.boot.pipeline.health().storage_state,
        StorageState::Normal,
        "D4 边界：备份域失败不误入 persist_degraded"
    );

    // 显式关闭加密 → 自动备份恢复可用（用户可行动路径）。
    fixture
        .runtime
        .block_on(fixture.boot.write.execute(StoreCommand::UpsertSetting {
            key: BACKUP_ENCRYPTION_ENABLED_KEY.to_owned(),
            value: "false".to_owned(),
            updated_at: 2,
        }))
        .expect("关闭加密");
    fixture.clock.advance(11 * 60 * 1000);
    let recovered = fixture
        .runtime
        .block_on(fixture.auto().run_once(AutoTrigger::Scheduled));
    assert_eq!(recovered.result, "success");
    let record = fixture
        .runtime
        .block_on(fixture.boot.reads.backups())
        .expect("台账")
        .into_iter()
        .max_by_key(|record| record.created_at)
        .expect("产物");
    assert!(!record.encrypted, "显式关闭加密 → P0 `.db` 形态");
    assert_eq!(
        record.path.extension().and_then(|value| value.to_str()),
        Some("db")
    );

    write_evidence(
        "dod3_auto_pause_and_explicit_disable",
        &json!({
            "failed_error": report.error,
            "failed_trigger": report.trigger.as_str(),
            "auto_segment": list["auto"],
            "storage_state": fixture.boot.pipeline.health().storage_state.as_str(),
            "recovered": recovered.result,
            "recovered_encrypted": record.encrypted,
        }),
    );
    fixture.shutdown();
}

// ===== DoD4：恢复七步在加密路径的现场保护与故障回滚 =====

/// 写入现场残留侧车（模拟未 checkpoint 的 `-wal`/`-shm`；库已关闭时写入）。
fn write_dummy_sidecars(db: &Path) {
    std::fs::write(
        PathBuf::from(format!("{}-wal", db.display())),
        b"drill-wal-scene",
    )
    .expect("wal 现场标记");
    std::fs::write(
        PathBuf::from(format!("{}-shm", db.display())),
        b"drill-shm-scene",
    )
    .expect("shm 现场标记");
}

#[test]
fn dod4_encrypted_path_protects_scene_and_rolls_back_on_injected_failure() {
    let mut fixture = Fixture::open();
    let db = fixture.db_path();
    let before = fingerprint(&db);

    // 加密备份 → 恢复请求（得到解密候选）；关闭存储（无写者窗口）。
    let created = fixture
        .create(Some(DRILL_PASSPHRASE))
        .expect("口令模式备份");
    let backup_id = created["backup"]["id"].as_str().expect("id").to_owned();
    fixture
        .restore_internal(&backup_id, Some(DRILL_PASSPHRASE))
        .expect("恢复请求");
    let journal = read_restore_journal(&db).expect("journal").expect("存在");
    let candidate = PathBuf::from(&journal.candidate_path);
    assert!(candidate.exists(), "解密候选在场");
    fixture.shutdown_storage();

    // 故障注入（就位后失败）：现场三件套恢复原状、`.pre-restore` 不残留、journal 清理。
    write_dummy_sidecars(&db);
    let error = execute_restore(
        &RestoreRequest {
            db_path: db.clone(),
            candidate_path: candidate.clone(),
            program_max_version: program_schema_version(),
            now_ms: fixture.clock.now_ms(),
            fault: RestoreFault::FailAfter {
                step: RestoreStep::PlaceCandidate,
                message: "M5-03 注入：加密路径就位后失败".to_owned(),
            },
        },
        || Ok(()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("已回滚现场"), "{error}");
    assert!(
        pre_restore_files(&db).is_empty(),
        "回滚后 .pre-restore 已还原：{:?}",
        pre_restore_files(&db)
    );
    // 先读取还原后的侧车内容（`fingerprint` 会打开库并触碰 WAL）。
    assert_eq!(
        std::fs::read(PathBuf::from(format!("{}-wal", db.display()))).expect("wal 还原"),
        b"drill-wal-scene",
        "回滚还原 -wal 现场"
    );
    assert_eq!(
        std::fs::read(PathBuf::from(format!("{}-shm", db.display()))).expect("shm 还原"),
        b"drill-shm-scene",
        "回滚还原 -shm 现场"
    );
    assert_eq!(fingerprint(&db), before, "回滚后数据一致");
    assert!(read_restore_journal(&db).unwrap().is_none(), "journal 清理");

    // 成功路径（同一解密候选）：`-wal`/`-shm` 现场改名与 `.pre-restore` 保护逐项断言。
    write_dummy_sidecars(&db);
    let report = execute_restore(
        &RestoreRequest {
            db_path: db.clone(),
            candidate_path: candidate.clone(),
            program_max_version: program_schema_version(),
            now_ms: fixture.clock.now_ms() + 1,
            fault: RestoreFault::None,
        },
        || Ok(()),
    )
    .expect("加密路径七步成功");
    assert!(report.applied && !report.rolled_back);
    let scene = report.scene.clone().expect("现场保护记录");
    let db_scene = scene.db.expect("库改名");
    let wal_scene = scene.wal.expect("-wal 改名");
    let shm_scene = scene.shm.expect("-shm 改名");
    assert!(db_scene.to_string_lossy().contains(PRE_RESTORE_INFIX));
    assert_eq!(std::fs::read(&wal_scene).unwrap(), b"drill-wal-scene");
    assert_eq!(std::fs::read(&shm_scene).unwrap(), b"drill-shm-scene");
    assert_eq!(pre_restore_files(&db).len(), 3, "现场三件套完整保留");
    assert_eq!(fingerprint(&db), before, "恢复后数据一致");
    aether_store::finalize_restore(&db).expect("journal 收尾");

    // 解密候选清理（启动序列职责；此处模拟）。
    let _ = std::fs::remove_file(&candidate);

    write_evidence(
        "dod4_scene_and_rollback",
        &json!({
            "backup_id": backup_id,
            "rollback_error": error.to_string(),
            "rollback_pre_restore_files": 0,
            "scene_db": db_scene.to_string_lossy(),
            "scene_wal": wal_scene.to_string_lossy(),
            "scene_shm": shm_scene.to_string_lossy(),
            "pre_restore_files": pre_restore_files(&db).len(),
            "fingerprint": fingerprint(&db),
            "journal_finalized": read_restore_journal(&db).unwrap().is_none(),
        }),
    );
    fixture.shutdown();
}

// ===== DoD5：诊断包/审计/响应不含密钥材料 =====

#[test]
fn dod5_diagnostics_audit_and_response_do_not_contain_secret_material() {
    let fixture = Fixture::open();
    // 口令模式 + 主密钥模式各一份（两类密钥材料都在系统中）。
    fixture.create(None).expect("主密钥模式备份");
    fixture
        .create(Some(DRILL_PASSPHRASE))
        .expect("口令模式备份");

    let store_snapshot = fixture.store.snapshot();
    let master_hex = store_snapshot
        .get("keychain://aether/backup/master")
        .expect("主密钥已生成");
    let master_raw = hex::decode(master_hex).expect("主密钥 hex");
    let needles: Vec<(&str, Vec<u8>)> = vec![
        ("master_key_hex", master_hex.as_bytes().to_vec()),
        ("master_key_raw", master_raw.clone()),
        ("passphrase", DRILL_PASSPHRASE.as_bytes().to_vec()),
        ("sk_pattern", b"sk-".to_vec()),
        ("jwt_pattern", b"eyJ".to_vec()),
        ("pem_pattern", b"BEGIN PRIVATE KEY".to_vec()),
    ];

    // 诊断包（脱敏 + 扫描守门）。
    let target = fixture.temp.path().join("export");
    std::fs::create_dir_all(&target).expect("导出目录");
    let request = ExportDiagnosticsRequest {
        target_dir: long_path(&target),
    };
    let canonical = request.canonical_target_dir().expect("目标校验");
    let response = fixture
        .diagnostics
        .export_diagnostics(&request, &canonical)
        .expect("诊断导出");
    assert_eq!(response["scanned_clean"], true);
    let bundle_text = std::fs::read(response["path"].as_str().expect("路径")).expect("读取");
    let bundle_hits = scan_secret_material(&bundle_text, &needles);
    assert!(bundle_hits.is_empty(), "诊断包命中：{bundle_hits:?}");

    // 审计（backup.auto.run/backup.restore 详情不含密钥材料）。
    let audit_text: Vec<u8> = fixture
        .audit_records()
        .iter()
        .map(|record| {
            format!(
                "{}|{}|{}",
                record.action,
                record.detail.as_deref().unwrap_or(""),
                record.resource.as_deref().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes();
    let audit_hits = scan_secret_material(&audit_text, &needles);
    assert!(audit_hits.is_empty(), "审计命中：{audit_hits:?}");

    // 恢复响应（解密候选路径/来源 id；不含口令/主密钥）。
    let backups = fixture
        .runtime
        .block_on(fixture.boot.reads.backups())
        .expect("台账");
    let passphrase_backup = backups
        .iter()
        .find(|record| record.path.extension().and_then(|value| value.to_str()) == Some("aebk"))
        .expect("加密产物");
    let response_value = fixture
        .restore_internal(&passphrase_backup.id, Some(DRILL_PASSPHRASE))
        .expect("恢复请求");
    let response_bytes = serde_json::to_vec(&response_value).expect("序列化");
    let response_hits = scan_secret_material(&response_bytes, &needles);
    assert!(response_hits.is_empty(), "恢复响应命中：{response_hits:?}");

    write_evidence(
        "dod5_secret_scan",
        &json!({
            "bundle_path": response["path"],
            "bundle_sections": response["sections"],
            "bundle_hits": bundle_hits,
            "audit_hits": audit_hits,
            "response_hits": response_hits,
            "scan_positive_control": true,
            "needles": needles.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
        }),
    );
    fixture.shutdown();
}
