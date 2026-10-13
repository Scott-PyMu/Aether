//! M5-04 集成测试：事件保留与滚动策略（设计 D4/D13、ADR-021、ADR-009）。
//!
//! 覆盖：
//! - DoD1 策略（时钟注入）：超窗白名单事件（`message.delta`/`usage`/`log`）清理；
//!   控制事件与 `audit_log` 保留范围断言；保留窗口（settings）参数校验；
//! - DoD2 补读边界：清理后 `last_seq` 补读行为与 `readback_gap_too_large` 错误码不变
//!   （M3-01「缺口 >10k → 会话重开」路径不受清理影响；`message.completed` 保留使历史
//!   会话仍可读）；
//! - DoD3 先归档后删除：归档产物完整（行数/哈希可校验）；归档失败跳过该批清理
//!   （失败注入）且不删除任何行；
//! - DoD4 阈值联动：2GB（警告档）不自动启用、5GB（强提示档）/增长 >500MB·月自动置
//!   `true`（参数注入断言 + 审计 `retention.auto_armed`）；
//! - DoD5 空间回收证据与长事务口径：清理前后 `db+wal` 变化记录；单事务 <500ms
//!   （5000 行批删除的 store 级计时见 `aether-store/tests/m5_04_retention_ops.rs`）；
//! - DoD6 诊断包 `retention`/`export` 段（ADR-021 附录 A-3 指标）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aether_control::ManualClock;
use aether_store::archive::verify_archive_file;
use aether_store::{Store, StoreCommand};
use aether_tauri::archive_control::{ArchiveControlBackend, AUDIT_ACTION_SESSION_EXPORTED};
use aether_tauri::backup_control::{BackupControlBackend, CapacityConfig};
use aether_tauri::core_health::{
    boot_core_full, CoreBoot, HealthProvider, StaticHealthSource, StaticRuntimeSummaries,
    StorageHealthSnapshot,
};
use aether_tauri::diagnostics_control::{
    DiagnosticsControlBackend, DiagnosticsDeps, EVENTS_RETENTION_DAYS_KEY,
    EVENTS_RETENTION_ENABLED_KEY,
};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    ExportDiagnosticsRequest, MessagesPageRequest, SessionExportRequest, SettingsGetRequest,
    SettingsSetRequest,
};
use aether_tauri::json_payload::JsonPayload;
use aether_tauri::retention::{
    ArmReason, RetentionOutcome, RetentionTask, RetentionThresholds, AUDIT_ACTION_RETENTION_ARMED,
    AUDIT_ACTION_RETENTION_CLEANED, DAY_MS,
};
use aether_tauri::security_level::{SecurityLevelView, SecurityProbe, SECURITY_LEVEL_OS};
use aether_tauri::session_backend::SessionBackend;
use serde_json::{json, Value};
use tempfile::TempDir;

const SESSION_ID: &str = "01J0000000000000000000000A";
const OLD_TS: i64 = 1_700_000_000_000;
const NOW_MS: i64 = 1_800_000_000_000;

/// 长路径形式（Windows `%TEMP%` 可能含 8.3 短名；同 m3_04/m5_02 口径）。
fn long_path(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).expect("canonicalize 夹具路径");
    let text = canonical.to_string_lossy().to_string();
    #[cfg(windows)]
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        return stripped.to_string();
    }
    text
}

fn write_evidence(name: &str, value: &Value) {
    println!("[m5-04] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M5_04_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(value) {
        let _ = std::fs::write(dir.join(format!("{name}.json")), text);
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

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

struct Fixture {
    temp: TempDir,
    runtime: tokio::runtime::Runtime,
    boot: CoreBoot,
    archive: Arc<ArchiveControlBackend>,
    diagnostics: Arc<DiagnosticsControlBackend>,
    session_backend: Arc<dyn IpcBackend>,
}

impl Fixture {
    fn open() -> Self {
        let temp = tempfile::tempdir().expect("临时数据目录");
        let runtime = new_runtime();
        let handle = runtime.handle().clone();
        let boot = boot_core_full(
            temp.path(),
            &handle,
            Arc::new(StaticRuntimeSummaries::unwired()),
        )
        .expect("启动核心（存储 + 管线）");
        let clock = Arc::new(ManualClock::new(NOW_MS));
        // 真实装饰链片段（Archive → Backup）：`backup_list.retention` 段断言需要内层
        // 备份面（其余命令由 NotImplementedBackend 兜底）。
        let backup = Arc::new(BackupControlBackend::new(
            Arc::new(NotImplementedBackend),
            temp.path().to_path_buf(),
            Some(boot.reads.clone()),
            Some(boot.write.clone()),
            handle.clone(),
            None,
        ));
        let archive = Arc::new(
            ArchiveControlBackend::new(
                backup as Arc<dyn IpcBackend>,
                temp.path().to_path_buf(),
                Some(boot.reads.clone()),
                Some(boot.write.clone()),
                handle.clone(),
                clock.clone(),
            )
            .with_retention_thresholds(RetentionThresholds::default()),
        );
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
            DiagnosticsControlBackend::new(Arc::clone(&archive) as Arc<dyn IpcBackend>, deps)
                .with_clock(clock.clone()),
        );
        // 补读边界（DoD2）：SessionBackend 的 `messages_page` 只依赖 reads。
        let session_backend: Arc<dyn IpcBackend> = Arc::new(SessionBackend::new(
            Arc::new(NotImplementedBackend),
            None,
            None,
            Some(boot.reads.clone()),
            None,
            handle,
        ));
        Self {
            temp,
            runtime,
            boot,
            archive,
            diagnostics,
            session_backend,
        }
    }

    fn task(&self) -> Arc<RetentionTask> {
        self.archive.retention_task().expect("保留任务已接线")
    }

    fn archives_dir(&self) -> PathBuf {
        self.temp.path().join("archives")
    }

    fn db_path(&self) -> PathBuf {
        self.temp.path().join("aether.db")
    }

    fn db_wal_bytes(&self) -> (u64, u64) {
        let db = std::fs::metadata(self.db_path())
            .map(|meta| meta.len())
            .unwrap_or(0);
        let wal = std::fs::metadata(format!("{}-wal", self.db_path().display()))
            .map(|meta| meta.len())
            .unwrap_or(0);
        (db, wal)
    }

    fn enable_retention(&self, days: i64) {
        self.settings_set(EVENTS_RETENTION_ENABLED_KEY, json!(true));
        self.settings_set(EVENTS_RETENTION_DAYS_KEY, json!(days));
    }

    fn settings_set(&self, key: &str, value: Value) -> Value {
        self.diagnostics
            .settings_set(&SettingsSetRequest {
                key: key.to_owned(),
                value: JsonPayload(value),
            })
            .expect("设置写入")
    }

    fn settings_get(&self, key: &str) -> Value {
        self.diagnostics
            .settings_get(&SettingsGetRequest {
                key: key.to_owned(),
            })
            .expect("设置读取")["value"]
            .clone()
    }

    fn run_cleanup(&self) -> aether_tauri::retention::RetentionTickReport {
        self.runtime.block_on(self.task().run_once())
    }

    fn retention_status(&self) -> Value {
        self.archive
            .retention_status()
            .expect("状态可读")
            .expect("保留任务已接线")
    }

    fn remaining_event_ids(&self) -> Vec<String> {
        self.runtime
            .block_on(self.boot.reads.with_connection(|connection| {
                let mut statement = connection
                    .prepare("SELECT id FROM events ORDER BY seq ASC, rowid ASC")
                    .map_err(aether_store::StoreError::from)?;
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(aether_store::StoreError::from)?;
                let mut ids = Vec::new();
                for row in rows {
                    ids.push(row.map_err(aether_store::StoreError::from)?);
                }
                Ok(ids)
            }))
            .expect("事件清单")
    }

    fn audit_rows(&self) -> Vec<(String, String, Option<String>, Option<String>)> {
        self.runtime
            .block_on(self.boot.reads.audit_log(100))
            .expect("审计查询")
            .into_iter()
            .map(|record| (record.id, record.action, record.result, record.detail))
            .collect()
    }

    fn shutdown(self) {
        let _ = self.runtime.block_on(self.boot.pipeline.shutdown());
        if let Some(storage) = self.boot.storage.take() {
            let _ = self.runtime.block_on(storage.shutdown());
        }
    }
}

/// 事件种子描述：`(seq, type, payload, ts)`。
fn event_sql(
    id: &str,
    session: &str,
    seq: i64,
    event_type: &str,
    payload: &str,
    ts: i64,
) -> String {
    let payload = payload.replace('\'', "''");
    format!(
        "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
         VALUES ('{id}', '{session}', NULL, 'mock', {seq}, '{event_type}', '{payload}', {ts}, 1)"
    )
}

fn delta_payload(index: i64) -> String {
    format!("{{\"message_id\":\"01J000000000000000000000M1\",\"text\":\"chunk-{index}\"}}")
}

fn completed_payload() -> String {
    "{\"message\":{\"id\":\"01J000000000000000000000M1\",\"session_id\":\"01J0000000000000000000000A\",\"run_id\":null,\"role\":\"assistant\",\"content\":\"ok\",\"created_at\":1},\"usage\":null}".to_owned()
}

fn usage_payload() -> String {
    "{\"tokens\":{\"input_tokens\":5,\"output_tokens\":7}}".to_owned()
}

// ===== DoD1：白名单/窗口清理 + 控制事件与审计保留 =====

#[test]
fn cleanup_cleans_whitelist_only_and_preserves_audit() {
    let fixture = Fixture::open();
    let old = OLD_TS;
    let in_window = NOW_MS - 1_000;
    let statements = vec![
        event_sql("01J0000000000000000000E0001", SESSION_ID, 1, "message.delta", &delta_payload(1), old),
        event_sql("01J0000000000000000000E0002", SESSION_ID, 2, "usage", &usage_payload(), old),
        event_sql("01J0000000000000000000E0003", SESSION_ID, 3, "log", "{\"level\":\"info\",\"message\":\"old\"}", old),
        event_sql("01J0000000000000000000E0004", SESSION_ID, 4, "message.completed", &completed_payload(), old),
        event_sql("01J0000000000000000000E0005", SESSION_ID, 5, "tool.call_started", "{\"tool_call_id\":\"01J000000000000000000000T1\",\"tool_name\":\"read\",\"args\":{}}", old),
        event_sql("01J0000000000000000000E0006", SESSION_ID, 6, "permission.requested", "{\"request_id\":\"01J000000000000000000000P1\",\"resource\":\"fs.read\",\"action\":\"read\",\"target\":null}", old),
        event_sql("01J0000000000000000000E0007", SESSION_ID, 7, "runtime.status_changed", "{\"runtime_id\":\"mock\",\"from\":\"cold\",\"to\":\"ready\",\"reason\":null}", old),
        event_sql("01J0000000000000000000E0008", SESSION_ID, 8, "error", "{\"code\":\"x\",\"message\":\"e\",\"recoverable\":true}", old),
        event_sql("01J0000000000000000000E0009", SESSION_ID, 9, "message.delta", &delta_payload(9), in_window),
        event_sql("01J0000000000000000000E0010", SESSION_ID, 10, "session.created", "{\"title\":\"t\"}", old),
    ];
    seed_session_and_events(&fixture, &statements);
    // 审计行（清理不得触碰）。
    let audit_id = "01J000000000000000000000A1";
    fixture
        .runtime
        .block_on(fixture.boot.write.execute(StoreCommand::InsertAudit {
            record: aether_store::AuditLogRecord {
                id: audit_id.to_owned(),
                session_id: Some(SESSION_ID.to_owned()),
                runtime_id: None,
                actor: "user".to_owned(),
                action: "session.created".to_owned(),
                resource: None,
                detail: Some("must-survive-retention".to_owned()),
                result: Some("success".to_owned()),
                ts: old,
            },
        }))
        .expect("审计写入");

    // 参数校验（DoD1）：非法窗口/类型被既有结构化码拒绝。
    assert_eq!(
        fixture
            .diagnostics
            .settings_set(&SettingsSetRequest {
                key: EVENTS_RETENTION_DAYS_KEY.to_owned(),
                value: JsonPayload(json!(6)),
            })
            .expect_err("6 天越界")
            .code
            .as_str(),
        "out_of_range"
    );
    assert_eq!(
        fixture
            .diagnostics
            .settings_set(&SettingsSetRequest {
                key: EVENTS_RETENTION_DAYS_KEY.to_owned(),
                value: JsonPayload(json!(366)),
            })
            .expect_err("366 天越界")
            .code
            .as_str(),
        "out_of_range"
    );
    assert_eq!(
        fixture
            .diagnostics
            .settings_set(&SettingsSetRequest {
                key: EVENTS_RETENTION_ENABLED_KEY.to_owned(),
                value: JsonPayload(json!("yes")),
            })
            .expect_err("非布尔")
            .code
            .as_str(),
        "invalid_type"
    );

    // 策略关闭时不得清理。
    assert_eq!(fixture.run_cleanup().result(), "disabled");
    assert_eq!(fixture.remaining_event_ids().len(), 10);

    fixture.enable_retention(30);
    let report = fixture.run_cleanup();
    match &report.outcome {
        RetentionOutcome::Cleaned { rows, archives, .. } => {
            assert_eq!(*rows, 3, "仅三类超窗白名单事件被清理");
            assert_eq!(*archives, 1);
        }
        other => panic!("必须清理成功：{other:?}"),
    }
    let remaining = fixture.remaining_event_ids();
    assert_eq!(
        remaining,
        vec![
            "01J0000000000000000000E0004", // message.completed
            "01J0000000000000000000E0005", // tool.call_started
            "01J0000000000000000000E0006", // permission.requested
            "01J0000000000000000000E0007", // runtime.status_changed
            "01J0000000000000000000E0008", // error
            "01J0000000000000000000E0009", // 窗口内 delta
            "01J0000000000000000000E0010", // session.created
        ],
        "控制事件与窗口内事件必须保留"
    );
    let audit = fixture.audit_rows();
    assert!(
        audit.iter().any(|(id, _, _, detail)| {
            id == audit_id && detail.as_deref() == Some("must-survive-retention")
        }),
        "audit_log 不得被保留策略触碰：{audit:?}"
    );
    assert!(
        audit.iter().any(|(_, action, result, _)| {
            action == AUDIT_ACTION_RETENTION_CLEANED && result.as_deref() == Some("success")
        }),
        "清理审计必须落库"
    );
    write_evidence(
        "dod1_cleanup_scope",
        &json!({
            "remaining": remaining,
            "audit_rows": audit.len(),
            "retention": fixture.retention_status(),
        }),
    );
    fixture.shutdown();
}

fn seed_session_and_events(fixture: &Fixture, statements: &[String]) {
    let path = fixture.db_path();
    let store = Store::open(&path).expect("打开种子库");
    {
        let connection = store.connection();
        connection
            .execute_batch(
                "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
                 VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1);",
            )
            .expect("插入运行时");
        connection
            .execute(
                "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                 VALUES (?1, 'mock', '保留测试', 'idle', 10, 10)",
                [SESSION_ID],
            )
            .expect("插入会话");
        for statement in statements {
            connection.execute(statement, []).expect("插入事件");
        }
    }
}

// ===== DoD2：清理后补读边界（M3-01 路径不受影响）=====

#[test]
fn readback_after_cleanup_keeps_gap_contract() {
    let fixture = Fixture::open();
    let path = fixture.db_path();
    let store = Store::open(&path).expect("打开种子库");
    {
        let connection = store.connection();
        connection
            .execute_batch(
                "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
                 VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1);",
            )
            .expect("插入运行时");
        connection
            .execute(
                "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                 VALUES (?1, 'mock', '补读测试', 'idle', 10, 10)",
                [SESSION_ID],
            )
            .expect("插入会话");
        // 12000 条：seq 1..10000 超窗 delta（可清理）；10001..12000 message.completed
        // （控制事件，保留）→ 清理后 max_seq 仍为 12000。
        let delta_payload = delta_payload(1);
        let completed = completed_payload();
        let sql = format!(
            "WITH RECURSIVE seqs(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seqs WHERE i < 12000) \
             INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
             SELECT printf('01J%023d', i), '{SESSION_ID}', NULL, 'mock', i, \
                    CASE WHEN i <= 10000 THEN 'message.delta' ELSE 'message.completed' END, \
                    CASE WHEN i <= 10000 THEN '{delta_payload}' ELSE '{completed}' END, \
                    {OLD_TS}, 1 \
             FROM seqs;"
        );
        connection.execute_batch(&sql).expect("插入 12000 事件");
    }

    fixture.enable_retention(30);
    let report = fixture.run_cleanup();
    assert_eq!(report.result(), "success");
    match &report.outcome {
        RetentionOutcome::Cleaned { rows, .. } => assert_eq!(*rows, 10_000),
        other => panic!("必须清理 10000 条超窗 delta：{other:?}"),
    }
    // 再跑一轮：无剩余白名单超窗事件（控制事件保留）。
    assert_eq!(fixture.run_cleanup().result(), "nothing");

    // 补读边界：缺口与错误码契约不变（ADR-009 / M3-01）。
    let max_seq = fixture
        .runtime
        .block_on(
            fixture
                .boot
                .reads
                .max_seq(&aether_core::SessionId::new(SESSION_ID).unwrap()),
        )
        .expect("max_seq");
    assert_eq!(max_seq, Some(12_000), "已清理低 seq 不影响 max_seq");

    let error = fixture
        .session_backend
        .messages_page(&MessagesPageRequest {
            session_id: SESSION_ID.to_owned(),
            last_seq: Some(1_000),
            limit: Some(500),
        })
        .expect_err("缺口 11000 > 10k 必须拒绝自动补发");
    assert_eq!(error.code.as_str(), "readback_gap_too_large");
    assert!(error.message.contains("重开会话"), "{}", error.message);

    let page = fixture
        .session_backend
        .messages_page(&MessagesPageRequest {
            session_id: SESSION_ID.to_owned(),
            last_seq: Some(11_500),
            limit: Some(500),
        })
        .expect("缺口 ≤10k 可补读");
    assert_eq!(page["complete"], true);
    assert!(
        page.get("messages").is_none(),
        "补读分支不返回 messages 基线"
    );
    let seqs: Vec<i64> = page["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["seq"].as_i64().expect("seq"))
        .collect();
    assert_eq!(
        seqs.first().copied(),
        Some(11_501),
        "已清理 delta 缺口按「同构丢弃」语义呈现（补读从下一条保留事件继续）"
    );
    assert_eq!(seqs.last().copied(), Some(12_000));
    write_evidence(
        "dod2_readback_boundary",
        &json!({
            "max_seq": max_seq,
            "gap_error": error.code.as_str(),
            "backfill_complete": page["complete"],
            "backfill_first_seq": seqs.first(),
            "backfill_last_seq": seqs.last(),
        }),
    );
    fixture.shutdown();
}

// ===== DoD3：先归档后删除；归档失败跳过清理 =====

#[test]
fn archive_before_delete_and_failure_skips_cleanup() {
    let fixture = Fixture::open();
    let statements = vec![
        event_sql(
            "01J0000000000000000000E0001",
            SESSION_ID,
            1,
            "message.delta",
            &delta_payload(1),
            OLD_TS,
        ),
        event_sql(
            "01J0000000000000000000E0002",
            SESSION_ID,
            2,
            "usage",
            &usage_payload(),
            OLD_TS,
        ),
        event_sql(
            "01J0000000000000000000E0003",
            SESSION_ID,
            3,
            "log",
            "{\"level\":\"info\",\"message\":\"x\"}",
            OLD_TS,
        ),
    ];
    seed_session_and_events(&fixture, &statements);
    fixture.enable_retention(30);

    // 失败注入：归档目录被同名文件占位 → create_dir_all 失败 → 不删除任何行。
    std::fs::write(fixture.archives_dir(), b"block").expect("占位归档目录");
    let failed = fixture.run_cleanup();
    match &failed.outcome {
        RetentionOutcome::Failed { error } => {
            assert!(error.contains("创建归档目录失败"), "{error}");
        }
        other => panic!("必须归档失败：{other:?}"),
    }
    assert_eq!(fixture.remaining_event_ids().len(), 3, "失败不得删除任何行");
    assert_eq!(
        fixture.retention_status()["archive_failures_total"],
        json!(1)
    );
    assert!(fixture.audit_rows().iter().any(|(_, action, result, _)| {
        action == AUDIT_ACTION_RETENTION_CLEANED && result.as_deref() == Some("failed")
    }));

    // 解除注入：先归档（校验通过）后删除；归档内容 == 被删行。
    std::fs::remove_file(fixture.archives_dir()).expect("解除占位");
    let report = fixture.run_cleanup();
    assert_eq!(report.result(), "success");
    assert!(fixture.remaining_event_ids().is_empty());
    let mut archives: Vec<PathBuf> = std::fs::read_dir(fixture.archives_dir())
        .expect("归档目录")
        .flatten()
        .map(|entry| entry.path())
        .collect();
    archives.sort();
    assert_eq!(archives.len(), 1);
    let verification = verify_archive_file(&archives[0]).expect("归档校验");
    assert!(
        verification.matches,
        "归档产物完整（manifest 行数/哈希一致）"
    );
    assert_eq!(verification.rows, 3);
    assert_eq!(verification.manifest.source, "auto-retention");
    // 归档产物复制到证据目录（verify-archive.mjs 独立校验）。
    if let Some(dir) = std::env::var_os("AETHER_M5_04_EVIDENCE_DIR") {
        let dir = PathBuf::from(dir);
        if std::fs::create_dir_all(&dir).is_ok() {
            let _ = std::fs::copy(&archives[0], dir.join("dod3_retention_archive.jsonl"));
        }
    }
    write_evidence(
        "dod3_archive_before_delete",
        &json!({
            "failed_injection": failed.result(),
            "archive_path": archives[0].to_string_lossy(),
            "archive_rows": verification.rows,
            "retention": fixture.retention_status(),
        }),
    );
    fixture.shutdown();
}

// ===== DoD4：阈值联动（参数注入）=====

#[test]
fn growth_threshold_auto_arms_retention() {
    let fixture = Fixture::open();
    let task = fixture.task();
    // 参数注入：仅增长档可达（容量两档设为不可达的极大值）。
    task.set_thresholds(RetentionThresholds::with_limits(
        u64::MAX / 2,
        u64::MAX,
        100 * 1024 * 1024,
    ));
    let mb = 1024 * 1024u64;
    task.push_sample(NOW_MS - 28 * DAY_MS, 10 * mb);
    task.push_sample(NOW_MS - 21 * DAY_MS, 110 * mb);
    task.push_sample(NOW_MS - 14 * DAY_MS, 210 * mb);
    task.push_sample(NOW_MS - 7 * DAY_MS, 310 * mb);
    task.push_sample(NOW_MS, 410 * mb);

    // 警告档不自动启用：确认 2GB 档仅提示（此处阈值调小以断言 level 语义）。
    let decided = fixture.runtime.block_on(task.evaluate_auto_arm());
    assert_eq!(decided, Ok(Some(ArmReason::Growth)));
    assert_eq!(
        fixture.settings_get(EVENTS_RETENTION_ENABLED_KEY),
        json!(true)
    );
    assert!(fixture.audit_rows().iter().any(|(_, action, result, _)| {
        action == AUDIT_ACTION_RETENTION_ARMED && result.as_deref() == Some("success")
    }));
    let status = fixture.retention_status();
    assert_eq!(status["auto_arm"]["armed"], true);
    assert_eq!(status["auto_arm"]["reason"], "growth");
    assert!(status["growth"]["bytes_per_month"].as_i64().unwrap() > 100 * 1024 * 1024);

    // 再次评估：已启用 → 不重复触发（每进程至多一次）。
    assert_eq!(fixture.runtime.block_on(task.evaluate_auto_arm()), Ok(None));

    // `backup_list.retention` 段（UI 与阈值联动展示）。
    let list = fixture.diagnostics.backup_list().expect("清单");
    assert_eq!(list["retention"]["enabled"], true);
    assert!(list["retention"]["capacity"]["level"].is_string());
    write_evidence(
        "dod4_growth_arm",
        &json!({
            "auto_arm": status["auto_arm"],
            "growth": status["growth"],
            "retention_segment": list["retention"],
        }),
    );
    fixture.shutdown();
}

#[test]
fn size_threshold_auto_arms_and_warn_level_does_not() {
    // 场景 A：仅警告档（2GB）→ 不自动启用。
    let fixture = Fixture::open();
    let capacity = CapacityConfig::default();
    let task = fixture.task();
    task.set_thresholds(RetentionThresholds::with_limits(1, u64::MAX, u64::MAX));
    assert_eq!(fixture.runtime.block_on(task.evaluate_auto_arm()), Ok(None));
    assert_eq!(
        fixture.settings_get(EVENTS_RETENTION_ENABLED_KEY),
        json!(false)
    );
    let status = fixture.retention_status();
    assert_eq!(
        status["capacity"]["level"], "warn",
        "≥2GB 仅为警告档（提示可开启）"
    );
    assert_eq!(status["auto_arm"]["armed"], false);
    assert_eq!(capacity.level(3 * 1024 * 1024 * 1024), "warn");
    fixture.shutdown();

    // 场景 B：强提示档（≥5GB）→ 自动置 true（size）。
    let fixture = Fixture::open();
    let task = fixture.task();
    task.set_thresholds(RetentionThresholds::with_limits(1, 1, u64::MAX));
    assert_eq!(
        fixture.runtime.block_on(task.evaluate_auto_arm()),
        Ok(Some(ArmReason::Size))
    );
    assert_eq!(
        fixture.settings_get(EVENTS_RETENTION_ENABLED_KEY),
        json!(true)
    );
    let status = fixture.retention_status();
    assert_eq!(status["capacity"]["level"], "critical");
    assert_eq!(status["auto_arm"]["reason"], "size");
    write_evidence(
        "dod4_size_arm",
        &json!({ "auto_arm": status["auto_arm"], "capacity": status["capacity"] }),
    );
    fixture.shutdown();
}

// ===== DoD5：空间回收证据（db+wal 前后值）=====

#[test]
fn space_reclaim_evidence_recorded() {
    let fixture = Fixture::open();
    let mut statements = Vec::new();
    for index in 1..=600 {
        statements.push(event_sql(
            &format!("01J{index:023}"),
            SESSION_ID,
            index,
            "message.delta",
            &delta_payload(index),
            OLD_TS,
        ));
    }
    seed_session_and_events(&fixture, &statements);
    // 产生 WAL 增量（未 checkpoint 的写入）。
    fixture
        .runtime
        .block_on(fixture.boot.write.execute(StoreCommand::UpsertSetting {
            key: "retention.evidence.probe".to_owned(),
            value: "true".to_owned(),
            updated_at: 1,
        }))
        .expect("写入设置（WAL 增量）");
    fixture.enable_retention(30);

    let before = fixture.db_wal_bytes();
    let started = std::time::Instant::now();
    let report = fixture.run_cleanup();
    let elapsed = started.elapsed();
    assert_eq!(report.result(), "success");
    let after = fixture.db_wal_bytes();
    let status = fixture.retention_status();
    let recorded = status["last_freed_bytes"].as_u64().expect("释放空间记录");
    let actual_delta = (before.0 + before.1).saturating_sub(after.0 + after.1);
    // 清理后 checkpoint 截断 WAL；记录值 = 实际前后差值（同一次运行内采样）。
    assert_eq!(recorded, actual_delta);
    assert_eq!(status["last_cleaned_rows"], json!(600));
    assert!(fixture.remaining_event_ids().is_empty());

    write_evidence(
        "dod5_space_reclaim",
        &json!({
            "db_wal_before": before,
            "db_wal_after": after,
            "freed_bytes_recorded": recorded,
            "cleanup_elapsed_ms": elapsed.as_millis(),
            "retention": status,
        }),
    );
    fixture.shutdown();
}

// ===== DoD6：诊断包 retention/export 段 =====

#[test]
fn diagnostics_bundle_exposes_retention_and_export_metrics() {
    let fixture = Fixture::open();
    let statements = vec![
        event_sql(
            "01J0000000000000000000E0001",
            SESSION_ID,
            1,
            "message.delta",
            &delta_payload(1),
            OLD_TS,
        ),
        event_sql(
            "01J0000000000000000000E0002",
            SESSION_ID,
            2,
            "usage",
            &usage_payload(),
            OLD_TS,
        ),
    ];
    seed_session_and_events(&fixture, &statements);
    fixture.enable_retention(30);
    assert_eq!(fixture.run_cleanup().result(), "success");

    // 触发一次导出，填充 export 指标。
    let request = SessionExportRequest {
        session_id: SESSION_ID.to_owned(),
        target_dir: None,
    };
    let canonical = request.canonical_target_dir().expect("目标校验");
    let exported = fixture
        .archive
        .session_export(&request, canonical.as_deref())
        .expect("会话导出");
    assert!(exported["export"]["rows"].as_u64().unwrap() >= 1);
    assert!(fixture.audit_rows().iter().any(|(_, action, result, _)| {
        action == AUDIT_ACTION_SESSION_EXPORTED && result.as_deref() == Some("success")
    }));

    let target = fixture.temp.path().join("diag");
    std::fs::create_dir_all(&target).expect("诊断目录");
    let request = ExportDiagnosticsRequest {
        target_dir: long_path(&target),
    };
    let canonical = request.canonical_target_dir().expect("目标校验");
    let response = fixture
        .diagnostics
        .export_diagnostics(&request, &canonical)
        .expect("诊断导出");
    let text = std::fs::read_to_string(response["path"].as_str().expect("路径")).expect("读取");
    let bundle: Value = serde_json::from_str(&text).expect("解析");
    assert_eq!(bundle["retention"]["last_cleaned_rows"], json!(2));
    assert_eq!(bundle["retention"]["last_result"], "success");
    assert_eq!(bundle["retention"]["archive_failures_total"], json!(0));
    assert_eq!(bundle["export"]["sha256_ok"], true);
    assert!(bundle["export"]["rows"].as_u64().unwrap() >= 1);
    assert!(bundle["export"]["bytes"].as_u64().unwrap() > 0);
    let sections = response["sections"].as_array().expect("sections");
    assert!(sections.iter().any(|section| section == "retention"));
    assert!(sections.iter().any(|section| section == "export"));
    write_evidence(
        "dod6_diagnostics",
        &json!({
            "retention": bundle["retention"],
            "export": bundle["export"],
            "sections": sections,
        }),
    );
    fixture.shutdown();
}
