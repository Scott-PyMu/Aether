//! M5-02 集成测试：自动备份（设计 D13、ADR-020；P1 计划 §2 M5-02 DoD1–DoD5 后端面）。
//!
//! 覆盖：
//! - DoD1 排期（时钟注入 `ManualClock`）：daily/weekly 频率触发、错过窗口补备（一次）、
//!   关闭开关跳过、`backup_list.auto`（ADR-018 附录 A-5）与状态投影；
//! - DoD2 运行中备份正确性：备份期间持续写入 → 产物 `quick_check` 通过、会话行数/
//!   哈希与备份前一致；写延迟阈值基准见 `aether-store/tests/m5_02_backup_bench.rs`；
//! - DoD3 保留滚动：仅清理自动类最旧产物（手动不自动删除）；`backups` 表与磁盘
//!   清单一致（证据 JSON 由 `scripts/test/m5-02/verify-m5-02.mjs` 再校验）；
//! - DoD4 失败注入（不可写/空间不足/目标不可建）：不产生半成品、不删除既有备份、
//!   审计 `backup.auto.run`（actor=system）告警、冷却后重试；不误入 `persist_degraded`
//!   （D4 边界：`pipeline.health().storage_state` 保持 `normal` 且写路径继续可用）；
//! - DoD5（后端面）7 天提醒联动：自动备份成功刷新提醒计时（同一 `backups` 表），
//!   不重复告警；诊断包 `backup_auto` 段（ADR-020 A-3）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use aether_control::{Clock, ManualClock, SharedClock, StorageState};
use aether_store::backup::{
    cleanup_backup_temps, program_schema_version, validate_candidate, BackupRecord,
};
use aether_store::{Store, StoreCommand};
use aether_tauri::backup_auto::{
    AutoTrigger, BACKUP_KIND_AUTO, BACKUP_KIND_MANUAL, FREQUENCY_DAILY_MS, FREQUENCY_WEEKLY_MS,
    RETRY_COOLDOWN_MS,
};
use aether_tauri::backup_control::{BackupControlBackend, SpaceProbe};
use aether_tauri::core_health::{
    boot_core_full, CoreBoot, HealthProvider, StaticHealthSource, StaticRuntimeSummaries,
    StorageHealthSnapshot,
};
use aether_tauri::diagnostics_control::{
    DiagnosticsControlBackend, DiagnosticsDeps, BACKUP_AUTO_ENABLED_KEY, BACKUP_AUTO_FREQUENCY_KEY,
    BACKUP_AUTO_RETENTION_KEY, BACKUP_REMINDER_KEY,
};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    BackupCreateRequest, ExportDiagnosticsRequest, SettingsSetRequest,
};
use aether_tauri::json_payload::JsonPayload;
use aether_tauri::security_level::{SecurityLevelView, SecurityProbe, SECURITY_LEVEL_OS};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

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

fn disk_file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|listing| {
            listing
                .flatten()
                .filter(|entry| entry.file_type().map(|kind| kind.is_file()).unwrap_or(false))
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn write_evidence(name: &str, value: &Value) {
    println!("[m5-02] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M5_02_EVIDENCE_DIR") else {
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

/// 可切换的空间探针（失败注入：不可写 / 空间不足）。
struct ToggleSpaceProbe {
    writable: AtomicBool,
    available: AtomicU64,
}

impl ToggleSpaceProbe {
    fn healthy() -> Self {
        Self {
            writable: AtomicBool::new(true),
            available: AtomicU64::new(u64::MAX),
        }
    }
}

impl SpaceProbe for ToggleSpaceProbe {
    fn ensure_writable(&self, _dir: &Path) -> Result<(), String> {
        if self.writable.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("注入：目录不可写".to_owned())
        }
    }

    fn available_bytes(&self, _dir: &Path) -> Result<u64, String> {
        Ok(self.available.load(Ordering::SeqCst))
    }
}

/// 固定安全级别探针（避免测试触碰真实 OS 凭据库）。
struct StaticSecurityProbe;

impl SecurityProbe for StaticSecurityProbe {
    fn status(&self) -> SecurityLevelView {
        SecurityLevelView {
            level: SECURITY_LEVEL_OS.to_owned(),
            detail: "注入：自检通过".to_owned(),
        }
    }
}

fn normal_health() -> HealthProvider {
    HealthProvider::new(
        Arc::new(StaticHealthSource::new(StorageHealthSnapshot {
            storage_state: "normal".to_owned(),
            write_queue_depth: 0,
            degrade_trigger: None,
            degraded_since_ms: None,
            detail: None,
        })),
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
}

struct Fixture {
    temp: TempDir,
    runtime: tokio::runtime::Runtime,
    boot: CoreBoot,
    clock: Arc<ManualClock>,
    backup: Arc<BackupControlBackend>,
    diagnostics: Arc<DiagnosticsControlBackend>,
}

impl Fixture {
    fn open() -> Self {
        Self::open_with_probe(None)
    }

    fn open_with_probe(probe: Option<Arc<dyn SpaceProbe>>) -> Self {
        let temp = tempfile::tempdir().expect("临时数据目录");
        seed_database(&temp.path().join("aether.db"), 200);
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
        let mut backup_backend = BackupControlBackend::new(
            Arc::new(NotImplementedBackend),
            temp.path().to_path_buf(),
            Some(boot.reads.clone()),
            Some(boot.write.clone()),
            handle.clone(),
        )
        .with_clock(clock_shared.clone());
        if let Some(probe) = probe {
            backup_backend = backup_backend.with_space_probe(probe);
        }
        let backup = Arc::new(backup_backend);
        let deps = DiagnosticsDeps {
            data_dir: temp.path().to_path_buf(),
            reads: Some(boot.reads.clone()),
            write: Some(boot.write.clone()),
            handle: handle.clone(),
            health: normal_health(),
            logs: None,
            task_dumps: None,
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
        }
    }

    fn auto(&self) -> Arc<aether_tauri::backup_auto::AutoBackup> {
        self.backup.auto_backup().expect("调度器已接线")
    }

    fn backups_dir(&self) -> PathBuf {
        self.temp.path().join("backups")
    }

    fn backups(&self) -> Vec<BackupRecord> {
        self.runtime
            .block_on(self.boot.reads.backups())
            .expect("备份清单读取")
    }

    fn latest_auto(&self) -> Option<BackupRecord> {
        self.backups()
            .into_iter()
            .filter(|record| record.kind == BACKUP_KIND_AUTO)
            .max_by_key(|record| record.created_at)
    }

    fn manual_backup(&self) -> Value {
        self.backup
            .backup_create(
                &BackupCreateRequest {
                    label: None,
                    target_dir: None,
                },
                None,
            )
            .expect("手动备份成功")
    }

    fn settings_set(&self, key: &str, value: Value) -> Value {
        self.diagnostics
            .settings_set(&SettingsSetRequest {
                key: key.to_owned(),
                value: JsonPayload(value),
            })
            .expect("设置写入")
    }

    fn audit_records(&self) -> Vec<aether_store::AuditLogRecord> {
        self.runtime
            .block_on(self.boot.reads.audit_log(50))
            .expect("审计查询")
    }

    fn shutdown(self) {
        let _ = self.runtime.block_on(self.boot.pipeline.shutdown());
        if let Some(storage) = self.boot.storage.take() {
            let _ = self.runtime.block_on(storage.shutdown());
        }
    }
}

// ===== DoD1：排期（时钟注入）与 backup_list.auto =====

#[test]
fn schedule_triggers_by_frequency_with_single_catchup() {
    let fixture = Fixture::open();
    let auto = fixture.auto();

    // 首次（无历史）→ Initial；产物 kind='auto'。
    let first = fixture.runtime.block_on(auto.tick()).expect("首次触发");
    assert_eq!(first.trigger, AutoTrigger::Initial);
    assert_eq!(first.result, "success");
    let backups = fixture.backups();
    assert_eq!(backups.len(), 1);
    assert_eq!(backups[0].kind, BACKUP_KIND_AUTO);

    // 排期未到 → 不触发。
    assert!(fixture.runtime.block_on(auto.tick()).is_none());
    fixture.clock.advance(FREQUENCY_DAILY_MS - 1);
    assert!(fixture.runtime.block_on(auto.tick()).is_none());

    // 到达 daily 窗口 → Scheduled。
    fixture.clock.advance(1);
    let scheduled = fixture.runtime.block_on(auto.tick()).expect("排期触发");
    assert_eq!(scheduled.trigger, AutoTrigger::Scheduled);

    // 模拟应用未运行连续错过 ≥7 天 → 仅补备一次（再次 tick 不触发）。
    fixture.clock.advance(FREQUENCY_DAILY_MS * 10);
    let catchup = fixture.runtime.block_on(auto.tick()).expect("补备触发");
    assert_eq!(catchup.trigger, AutoTrigger::MissedWindowCatchup);
    assert!(
        fixture.runtime.block_on(auto.tick()).is_none(),
        "连续错过仍只补一次（不追补多次）"
    );

    // `backup_list.auto`（ADR-018 附录 A-5：7 字段）。
    let list = fixture.diagnostics.backup_list().expect("清单");
    assert_eq!(list["auto"]["enabled"], true);
    assert_eq!(list["auto"]["frequency"], "daily");
    assert_eq!(list["auto"]["retention"], 10);
    assert_eq!(list["auto"]["last_result"], "success");
    assert_eq!(list["auto"]["consecutive_failures"], 0);
    assert!(list["auto"]["last_run_at"].as_i64().is_some());
    assert!(list["auto"]["next_run_at"].as_i64().is_some());
    let status = fixture.runtime.block_on(auto.status()).expect("状态");
    assert_eq!(status["missed_window_catchup"], true, "补备标记");
    assert_eq!(status["last_trigger"], "catchup");

    // weekly + 总开关：关闭不得触发；开启后按 weekly 周期触发。
    fixture.settings_set(BACKUP_AUTO_FREQUENCY_KEY, json!("weekly"));
    fixture.settings_set(BACKUP_AUTO_ENABLED_KEY, json!(false));
    fixture.clock.advance(FREQUENCY_WEEKLY_MS * 2);
    assert!(
        fixture.runtime.block_on(auto.tick()).is_none(),
        "总开关关闭不得触发"
    );
    fixture.settings_set(BACKUP_AUTO_ENABLED_KEY, json!(true));
    let weekly = fixture.runtime.block_on(auto.tick()).expect("weekly 到期");
    assert_eq!(weekly.trigger, AutoTrigger::MissedWindowCatchup);
    assert_eq!(fixture.backups().len(), 4);

    write_evidence(
        "dod1_schedule",
        &json!({
            "first": first.trigger.as_str(),
            "scheduled": scheduled.trigger.as_str(),
            "catchup": catchup.trigger.as_str(),
            "auto_segment": list["auto"],
            "status": status,
        }),
    );
    fixture.shutdown();
}

// ===== DoD2：运行中备份正确性（备份期间持续写入） =====

#[test]
fn backup_during_writes_is_verified_and_consistent() {
    let fixture = Fixture::open();
    let db = fixture.temp.path().join("aether.db");
    let before = fingerprint(&db);
    let before_count = session_count(&db);

    // 并发写入：300 条设置写入（覆盖备份窗口；仅触碰 settings，不改变会话快照）。
    let write = fixture.boot.write.clone();
    let writer = fixture.runtime.spawn(async move {
        let mut committed = 0usize;
        for index in 0..300i64 {
            if write
                .execute(StoreCommand::UpsertSetting {
                    key: "backup.reminder".to_owned(),
                    value: "true".to_owned(),
                    updated_at: 1_000 + index,
                })
                .await
                .is_ok()
            {
                committed += 1;
            }
        }
        committed
    });
    let report = fixture
        .runtime
        .block_on(fixture.auto().run_once(AutoTrigger::Scheduled));
    let committed = fixture.runtime.block_on(writer).expect("写入任务");

    assert_eq!(report.result, "success");
    assert_eq!(committed, 300, "备份期间写路径必须持续可用");

    // 产物：quick_check + 迁移版本 + 行数/哈希抽查一致（备份开始时快照）。
    let record = fixture.latest_auto().expect("自动产物");
    let candidate = validate_candidate(&record.path, program_schema_version()).expect("产物可消费");
    assert_eq!(candidate.schema_version, program_schema_version());
    assert_eq!(session_count(&record.path), before_count);
    assert_eq!(
        fingerprint(&record.path),
        before,
        "产物与备份前库一致（行数+哈希）"
    );
    assert_eq!(cleanup_backup_temps(&fixture.backups_dir()), 0, "无临时残留");

    write_evidence(
        "dod2_backup_during_writes",
        &json!({
            "trigger": report.trigger.as_str(),
            "artifact_bytes": record.size_bytes,
            "sessions_before": before_count,
            "fingerprint_before": before,
            "fingerprint_artifact": fingerprint(&record.path),
            "writer_committed": committed,
        }),
    );
    fixture.shutdown();
}

// ===== DoD3：保留滚动（仅自动类）与台账/磁盘一致 =====

#[test]
fn retention_rolls_auto_only_and_table_matches_disk() {
    let fixture = Fixture::open();
    fixture.settings_set(BACKUP_AUTO_RETENTION_KEY, json!(2));

    // 两份手动产物（不自动删除）。
    fixture.manual_backup();
    fixture.manual_backup();

    // 4 次自动运行（时钟推进保证时间戳可区分）→ 按 retention=2 滚动。
    let auto = fixture.auto();
    for _ in 0..4 {
        let report = fixture.runtime.block_on(auto.run_once(AutoTrigger::Scheduled));
        assert_eq!(report.result, "success");
        fixture.clock.advance(1_000);
    }

    let backups = fixture.backups();
    let auto_kept = backups
        .iter()
        .filter(|record| record.kind == BACKUP_KIND_AUTO)
        .count();
    let manual_kept = backups
        .iter()
        .filter(|record| record.kind == BACKUP_KIND_MANUAL)
        .count();
    assert_eq!(auto_kept, 2, "自动产物按 backup.auto.retention 滚动");
    assert_eq!(manual_kept, 2, "手动产物不自动删除（ADR-020 §2.1-1）");
    for record in &backups {
        assert!(record.path.exists(), "台账行必须有磁盘产物：{:?}", record.path);
    }

    // `backups` 表与磁盘清单一致（证据 JSON 由 verify-m5-02 脚本再校验）。
    let disk = disk_file_names(&fixture.backups_dir());
    let mut table: Vec<String> = backups
        .iter()
        .map(|record| {
            record
                .path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        })
        .collect();
    table.sort();
    assert_eq!(table, disk, "backups 表与磁盘清单一致");
    assert_eq!(cleanup_backup_temps(&fixture.backups_dir()), 0, "无临时残留");

    write_evidence(
        "dod3_retention_consistency",
        &json!({
            "table_files": table,
            "disk_files": disk,
            "auto_kept": auto_kept,
            "manual_kept": manual_kept,
        }),
    );
    fixture.shutdown();
}

// ===== DoD4：失败注入 → 隔离、告警、重试；不误入 persist_degraded =====

#[test]
fn failures_are_isolated_recorded_and_retried() {
    let probe = Arc::new(ToggleSpaceProbe::healthy());
    let fixture = Fixture::open_with_probe(Some(probe.clone()));
    let auto = fixture.auto();

    // 先成功一份（既有备份与产物基线）。
    let initial = fixture.runtime.block_on(auto.run_once(AutoTrigger::Initial));
    assert_eq!(initial.result, "success");
    assert_eq!(fixture.backups().len(), 1);
    let files_before = disk_file_names(&fixture.backups_dir());

    // 注入「目标不可写」→ 到期触发失败。
    probe.writable.store(false, Ordering::SeqCst);
    fixture.clock.advance(FREQUENCY_DAILY_MS + 1);
    let failed = fixture.runtime.block_on(auto.tick()).expect("到期触发");
    assert_eq!(failed.result, "failed");
    assert!(
        failed
            .error
            .as_deref()
            .unwrap_or("")
            .contains("backup_target_not_writable"),
        "稳定业务码：{:?}",
        failed.error
    );
    assert_eq!(fixture.backups().len(), 1, "失败不得新增台账/删除既有备份");
    assert_eq!(
        disk_file_names(&fixture.backups_dir()),
        files_before,
        "失败不得产生半成品/删除既有产物"
    );
    assert_eq!(cleanup_backup_temps(&fixture.backups_dir()), 0);

    // 告警记录：审计 backup.auto.run（actor=system, result=failed）落库；连续失败计数 + 下次重试。
    let audit = fixture.audit_records();
    let failed_audit = audit
        .iter()
        .find(|record| {
            record.action == aether_tauri::backup_auto::AUDIT_ACTION_AUTO_BACKUP
                && record.result.as_deref() == Some("failed")
        })
        .expect("失败审计必须落库");
    assert_eq!(failed_audit.actor, "system");
    let list = fixture.diagnostics.backup_list().expect("清单");
    assert_eq!(list["auto"]["consecutive_failures"], 1);
    assert_eq!(list["auto"]["last_result"], "failed");
    assert_eq!(
        list["auto"]["next_run_at"].as_i64(),
        Some(fixture.clock.now_ms() + RETRY_COOLDOWN_MS),
        "下次重试 = 上次尝试 + 冷却"
    );

    // 冷却期内不重试。
    assert!(
        fixture.runtime.block_on(auto.tick()).is_none(),
        "冷却期内 Wait（不重复轰炸）"
    );

    // D4 边界：备份域失败不误入 persist_degraded；写路径继续可用。
    assert_eq!(
        fixture.boot.pipeline.health().storage_state,
        StorageState::Normal,
        "备份失败不得触发存储降级（D4：备份域失败 ≠ 存储写失败）"
    );
    fixture.settings_set(BACKUP_REMINDER_KEY, json!(true));

    // 冷却期满重试仍失败（空间不足路径）→ 连续失败累加。
    probe.writable.store(true, Ordering::SeqCst);
    probe.available.store(0, Ordering::SeqCst);
    fixture.clock.advance(RETRY_COOLDOWN_MS);
    let failed2 = fixture.runtime.block_on(auto.tick()).expect("重试触发");
    assert_eq!(failed2.result, "failed");
    assert!(
        failed2
            .error
            .as_deref()
            .unwrap_or("")
            .contains("backup_space_insufficient"),
        "稳定业务码：{:?}",
        failed2.error
    );
    let list = fixture.diagnostics.backup_list().expect("清单");
    assert_eq!(list["auto"]["consecutive_failures"], 2);

    // 恢复条件 → 下次重试成功；计数归零、排期刷新。
    probe.available.store(u64::MAX, Ordering::SeqCst);
    fixture.clock.advance(RETRY_COOLDOWN_MS);
    let recovered = fixture.runtime.block_on(auto.tick()).expect("恢复重试");
    assert_eq!(recovered.result, "success");
    let list = fixture.diagnostics.backup_list().expect("清单");
    assert_eq!(list["auto"]["consecutive_failures"], 0);
    assert_eq!(list["auto"]["last_result"], "success");
    assert_eq!(fixture.backups().len(), 2);

    write_evidence(
        "dod4_failure_isolation",
        &json!({
            "failed_unwritable": failed.error,
            "failed_space": failed2.error,
            "audit": { "actor": failed_audit.actor, "action": failed_audit.action, "result": failed_audit.result },
            "storage_state": fixture.boot.pipeline.health().storage_state.as_str(),
            "recovered": recovered.result,
            "auto_segment": list["auto"],
        }),
    );
    fixture.shutdown();
}

/// 目标目录不可建（IO 级失败）→ 无半成品、无台账、审计告警。
#[test]
fn uncreatable_target_leaves_no_half_artifact() {
    let fixture = Fixture::open();
    // 以同名文件占位默认备份目录 → `create_dir_all` 失败。
    std::fs::write(fixture.temp.path().join("backups"), b"block").expect("占位文件");

    let failed = fixture
        .runtime
        .block_on(fixture.auto().run_once(AutoTrigger::Initial));
    assert_eq!(failed.result, "failed");
    assert!(
        failed
            .error
            .as_deref()
            .unwrap_or("")
            .contains("创建默认备份目录失败"),
        "失败原因：{:?}",
        failed.error
    );
    assert!(fixture.backups().is_empty(), "失败不得登记台账");
    let audit = fixture.audit_records();
    assert!(
        audit.iter().any(|record| {
            record.action == aether_tauri::backup_auto::AUDIT_ACTION_AUTO_BACKUP
                && record.result.as_deref() == Some("failed")
        }),
        "失败审计必须落库"
    );
    write_evidence(
        "dod4_io_failure",
        &json!({ "error": failed.error, "backups": fixture.backups().len() }),
    );
    fixture.shutdown();
}

// ===== DoD5（后端面）：7 天提醒联动（不重复告警）+ 诊断包 backup_auto 段 =====

#[test]
fn reminder_linkage_and_diagnostics_section() {
    let fixture = Fixture::open();

    // 从未备份 → 提醒 never（缺省开启）。
    let initial = fixture.diagnostics.backup_list().expect("清单");
    assert_eq!(initial["reminder"]["due"], true);
    assert_eq!(initial["reminder"]["reason"], "never");

    // 自动备份成功 → 同一 backups 表刷新提醒计时（联动；无第二条提醒机制）。
    let report = fixture
        .runtime
        .block_on(fixture.auto().run_once(AutoTrigger::Initial));
    assert_eq!(report.result, "success");
    let after = fixture.diagnostics.backup_list().expect("清单");
    assert_eq!(after["reminder"]["enabled"], true, "提醒开关本身不被改写");
    assert_eq!(after["reminder"]["due"], false, "自动成功刷新提醒（不重复告警）");
    assert_eq!(after["reminder"]["reason"], Value::Null);
    assert_eq!(after["reminder"]["last_backup_at"], fixture.clock.now_ms());
    assert_eq!(after["auto"]["last_result"], "success");

    // 诊断包 backup_auto 段（ADR-020 A-3）。
    let target = fixture.temp.path().join("export");
    std::fs::create_dir_all(&target).expect("导出目录");
    let request = ExportDiagnosticsRequest {
        target_dir: target.to_string_lossy().to_string(),
    };
    let canonical = request.canonical_target_dir().expect("目标校验");
    let response = fixture
        .diagnostics
        .export_diagnostics(&request, &canonical)
        .expect("诊断导出");
    let text = std::fs::read_to_string(response["path"].as_str().expect("路径")).expect("读取");
    let bundle: Value = serde_json::from_str(&text).expect("解析");
    assert_eq!(bundle["backup_auto"]["last_result"], "success");
    assert_eq!(bundle["backup_auto"]["consecutive_failures"], 0);
    assert_eq!(bundle["backup_auto"]["missed_window_catchup"], false);
    assert!(bundle["backup_auto"]["next_run_at"].as_i64().is_some());
    assert_eq!(bundle["backup_auto"]["last_trigger"], "initial");
    assert!(
        response["sections"]
            .as_array()
            .expect("sections")
            .iter()
            .any(|section| section == "backup_auto"),
        "sections 必须包含 backup_auto"
    );
    write_evidence(
        "dod5_reminder_linkage",
        &json!({
            "initial_reminder": initial["reminder"],
            "after_reminder": after["reminder"],
            "bundle_backup_auto": bundle["backup_auto"],
            "sections": response["sections"],
        }),
    );
    fixture.shutdown();
}

// ===== DoD6（回归锚点）：手动备份路径保持既有契约（T9 全链见 m3_04 套件） =====

#[test]
fn manual_backup_contract_unchanged() {
    let fixture = Fixture::open();
    let created = fixture.manual_backup();
    assert_eq!(created["backup"]["kind"], "manual");
    assert_eq!(created["pruned"], json!([]));
    let list = fixture.diagnostics.backup_list().expect("清单");
    let backups = list["backups"].as_array().expect("数组");
    assert_eq!(backups.len(), 1);
    assert_eq!(backups[0]["kind"], "manual");
    // 手动路径的产物可被恢复候选校验消费（T9 命令链的候选环节）。
    let path = PathBuf::from(created["backup"]["path"].as_str().expect("路径"));
    validate_candidate(&path, program_schema_version()).expect("手动产物可消费");
    fixture.shutdown();
}
