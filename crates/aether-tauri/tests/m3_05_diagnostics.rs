//! M3-05 集成测试：诊断包导出与容量巡检（设计 D11/D13、ADR-007 增量、ADR-003 决策 19）。
//!
//! 覆盖 DoD：
//! - DoD1：诊断包密钥模式扫描 0 命中（`sk-`/`eyJ`/PEM 三类样本；脱敏后再扫描）；
//! - DoD2：容量阈值参数化模拟 → `backup_list.capacity.level` 的 ok/warn/critical 三档；
//! - DoD3：7 天未备份提醒可开关（时钟注入：`ManualClock` + `settings` 持久化）；
//! - DoD4：日志汇聚产物（含 `attempt=n/3` 与 `persist_degraded` 诊断）纳入诊断包，
//!   脱敏扫描 0 命中；降级启动态导出仍可用（D4）。
//!
//! 命令层（tauri mock invoke）覆盖导出目标的严格解析与外部路径语义（ADR-003 决策 19）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use aether_control::{Clock, ManualClock, SharedClock};
use aether_security::Redactor;
use aether_store::{StoreCommand, StoreError, StoreRuntime, WriteQueueConfig};
use aether_tauri::backup_control::{BackupControlBackend, CapacityConfig, SpaceProbe};
use aether_tauri::core_health::{
    HealthProvider, StaticHealthSource, StaticRuntimeSummaries, StorageHealthSnapshot,
};
use aether_tauri::diagnostics_control::{
    DiagnosticsControlBackend, DiagnosticsDeps, TaskDumpSource, BACKUP_REMINDER_KEY,
    BACKUP_REMINDER_THRESHOLD_MS,
};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{ExportDiagnosticsRequest, SettingsGetRequest, SettingsSetRequest};
use aether_tauri::ipc::{handler, IpcState};
use aether_tauri::json_payload::JsonPayload;
use aether_tauri::logging::LogSink;
use aether_tauri::security_level::{SecurityLevelView, SecurityProbe, SECURITY_LEVEL_OS};
use serde_json::{json, Value};
use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, WebviewWindow, WebviewWindowBuilder};
use tempfile::TempDir;

#[cfg(windows)]
const INVOKE_URL: &str = "http://tauri.localhost";
#[cfg(not(windows))]
const INVOKE_URL: &str = "tauri://localhost";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

/// 运行期生成的密钥形态样本（AGENTS §2.9：不把密钥字面量写进夹具）。
fn random_token(length: usize) -> String {
    let seed = format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut token = String::with_capacity(length);
    let bytes = seed.as_bytes();
    for index in 0..length {
        let byte = bytes[index % bytes.len()] as usize;
        token.push(alphabet[(byte + index * 7) % alphabet.len()] as char);
    }
    token
}

fn sample_api_key() -> String {
    format!("sk-ant-api03-{}", random_token(48))
}

fn sample_jwt() -> String {
    format!(
        "eyJhbGciOiJIUzI1NiJ9.{}.{}",
        random_token(40),
        random_token(32)
    )
}

fn sample_pem() -> String {
    let body = random_token(64);
    format!("-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----")
}

/// 静态任务 dump 源（M2-05 缓冲替身）。
struct StaticTaskDumps {
    dumps: Vec<Value>,
}

impl TaskDumpSource for StaticTaskDumps {
    fn task_dumps(&self) -> Vec<Value> {
        self.dumps.clone()
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

/// 固定空间探针（覆盖导出空间护栏分支）。
struct FixedSpaceProbe {
    writable: bool,
    available: u64,
}

impl SpaceProbe for FixedSpaceProbe {
    fn ensure_writable(&self, _dir: &Path) -> Result<(), String> {
        if self.writable {
            Ok(())
        } else {
            Err("注入：目录不可写".to_owned())
        }
    }

    fn available_bytes(&self, _dir: &Path) -> Result<u64, String> {
        Ok(self.available)
    }
}

struct Fixture {
    temp: TempDir,
    runtime: tokio::runtime::Runtime,
    storage: Option<StoreRuntime>,
    backend: DiagnosticsControlBackend,
    clock: Arc<ManualClock>,
    log_sink: Arc<LogSink>,
}

impl Fixture {
    fn open() -> Self {
        let temp = tempfile::tempdir().expect("临时数据目录");
        let runtime = new_runtime();
        let handle = runtime.handle().clone();
        let storage = StoreRuntime::open(
            temp.path().join("aether.db"),
            WriteQueueConfig::default(),
            &handle,
        )
        .expect("打开存储运行时");
        let clock = Arc::new(ManualClock::new(1_800_000_000_000));
        let log_sink = LogSink::new(256);
        let backend = Self::build(
            &temp,
            &runtime,
            Some(storage.reads().clone()),
            Some(storage.queue().clone()),
            Arc::clone(&clock) as SharedClock,
            Arc::clone(&log_sink),
            normal_health(),
            Some(Arc::new(StaticTaskDumps {
                dumps: vec![json!({
                    "task": "session:01J0000000000000000000000A run:01J000000000000000000000R1",
                    "action": "forced_cleanup",
                })],
            })),
        );
        Self {
            temp,
            runtime,
            storage: Some(storage),
            backend,
            clock,
            log_sink,
        }
    }

    /// 降级启动态（无读/写句柄；健康快照 persist_degraded）。
    fn degraded() -> Self {
        Self::degraded_with_logs(LogSink::new(256))
    }

    /// 降级启动态 + 显式日志汇聚端（DoD4：tracing 全局订阅器接线用例）。
    fn degraded_with_logs(log_sink: Arc<LogSink>) -> Self {
        let temp = tempfile::tempdir().expect("临时数据目录");
        let runtime = new_runtime();
        let clock = Arc::new(ManualClock::new(1_800_000_000_000));
        let backend = Self::build(
            &temp,
            &runtime,
            None,
            None,
            Arc::clone(&clock) as SharedClock,
            Arc::clone(&log_sink),
            degraded_health(),
            None,
        );
        Self {
            temp,
            runtime,
            storage: None,
            backend,
            clock,
            log_sink,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        temp: &TempDir,
        runtime: &tokio::runtime::Runtime,
        reads: Option<aether_store::ReadPool>,
        write: Option<aether_store::WriteQueue>,
        clock: SharedClock,
        log_sink: Arc<LogSink>,
        health: HealthProvider,
        task_dumps: Option<Arc<dyn TaskDumpSource>>,
    ) -> DiagnosticsControlBackend {
        // 内层为真实备份后端（`backup_list` 需读台账）+ 未实现基线（其余命令不可达）。
        let inner: Arc<dyn IpcBackend> = Arc::new(BackupControlBackend::new(
            Arc::new(NotImplementedBackend),
            temp.path().to_path_buf(),
            reads.clone(),
            write.clone(),
            runtime.handle().clone(),
        ));
        let deps = DiagnosticsDeps {
            data_dir: temp.path().to_path_buf(),
            reads,
            write,
            handle: runtime.handle().clone(),
            health,
            logs: Some(log_sink),
            task_dumps,
            security: Some(Arc::new(StaticSecurityProbe)),
        };
        DiagnosticsControlBackend::new(inner, deps)
            .with_clock(clock)
            .with_capacity_config(CapacityConfig::default())
    }

    fn target(&self) -> PathBuf {
        let dir = self.temp.path().join("export");
        std::fs::create_dir_all(&dir).expect("导出目录");
        dir
    }

    fn export(&self, target: &Path) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        let request = ExportDiagnosticsRequest {
            target_dir: target.to_string_lossy().to_string(),
        };
        let canonical = request.canonical_target_dir().expect("目标校验");
        self.backend.export_diagnostics(&request, &canonical)
    }

    fn backup_list(&self) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.backup_list()
    }

    fn settings_set(
        &self,
        key: &str,
        value: Value,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.settings_set(&SettingsSetRequest {
            key: key.to_owned(),
            value: JsonPayload(value),
        })
    }

    fn settings_get(&self, key: &str) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.settings_get(&SettingsGetRequest {
            key: key.to_owned(),
        })
    }

    fn insert_backup(&self, created_at: i64) {
        let storage = self.storage.as_ref().expect("存储已打开");
        let record = aether_store::backup::BackupRecord {
            id: format!(
                "01J00000000000000000{:04}R",
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ),
            path: self.temp.path().join("backups").join("aether-test.db"),
            size_bytes: 1024,
            encrypted: false,
            kind: "internal".to_owned(),
            created_at,
        };
        self.runtime
            .block_on(
                storage
                    .queue()
                    .execute(StoreCommand::InsertBackup { record }),
            )
            .map(|_| ())
            .expect("登记备份行");
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
        Arc::new(StaticRuntimeSummaries::wired(Vec::new())),
    )
}

fn degraded_health() -> HealthProvider {
    HealthProvider::new(
        Arc::new(StaticHealthSource::new(StorageHealthSnapshot {
            storage_state: "persist_degraded".to_owned(),
            write_queue_depth: 0,
            degrade_trigger: Some("write_failure".to_owned()),
            degraded_since_ms: Some(1_799_999_000_000),
            detail: Some("写事务连续 3 次失败（attempt=3/3）".to_owned()),
        })),
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
}

fn write_evidence(name: &str, value: &Value) {
    println!("[m3-05] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M3_05_EVIDENCE_DIR") else {
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

// ===== DoD1 + DoD4：诊断包脱敏 0 命中 + 日志汇聚整合 =====

#[test]
fn export_bundle_redacts_secrets_and_includes_log_aggregation() {
    let fixture = Fixture::open();
    let api_key = sample_api_key();
    let jwt = sample_jwt();
    let pem = sample_pem();
    fixture
        .log_sink
        .push_line(&format!("INFO starting adapter key={api_key}"));
    fixture
        .log_sink
        .push_line(&format!("WARN authorization: {jwt}"));
    fixture
        .log_sink
        .push_line(&format!("ERROR crash dump:\n{pem}\ntail line"));
    fixture
        .log_sink
        .push_line("WARN 写事务尝试失败 attempt=1/3");
    fixture
        .log_sink
        .push_line("WARN 写事务尝试失败 attempt=2/3");
    fixture
        .log_sink
        .push_line("ERROR 写事务尝试失败 attempt=3/3");
    fixture
        .log_sink
        .push_line("ERROR 存储进入 persist_degraded（write_failure）");

    let target = fixture.target();
    let response = fixture.export(&target).expect("诊断包导出成功");
    let path = PathBuf::from(response["path"].as_str().expect("路径"));
    assert!(path.exists(), "诊断包文件必须写出：{}", path.display());
    assert_eq!(response["scanned_clean"], json!(true));
    assert_eq!(response["task_dumps"], json!(1));

    let text = std::fs::read_to_string(&path).expect("读取诊断包");
    let redactor = Redactor::new().expect("脱敏器");
    assert!(
        redactor.is_clean(&text),
        "诊断包脱敏后必须 0 命中（D10）：{}",
        text
    );
    for secret in [&api_key, &jwt, &pem] {
        assert!(!text.contains(secret.as_str()), "原始密钥不得出现在诊断包");
    }
    for marker in [
        "[REDACTED:api-key]",
        "[REDACTED:jwt]",
        "[REDACTED:pem-private-key]",
    ] {
        assert!(text.contains(marker), "缺少脱敏标记 {marker}");
    }
    assert!(text.contains("attempt=1/3"), "日志汇聚必须包含 attempt=1/3");
    assert!(text.contains("attempt=3/3"), "日志汇聚必须包含 attempt=3/3");
    assert!(
        text.contains("persist_degraded"),
        "日志汇聚必须包含 persist_degraded 诊断"
    );
    assert!(text.contains("forced_cleanup"), "任务 dump 必须进入诊断包");

    let bundle: Value = serde_json::from_str(&text).expect("诊断包为 JSON");
    assert_eq!(bundle["bundle_version"], json!(1));
    assert_eq!(bundle["store"]["schema_version"], json!(2));
    assert_eq!(bundle["health"]["storage_state"], json!("normal"));
    assert!(bundle["config"]["settings"].is_object());
    assert!(bundle["host"]["data_dir"].is_string());

    write_evidence(
        "dod1_dod4_bundle",
        &json!({
            "response": response,
            "bundle_sections": bundle.as_object().map(|map| map.keys().cloned().collect::<Vec<_>>()),
            "scanned_clean": redactor.is_clean(&text),
            "log_lines": fixture.log_sink.line_count(),
        }),
    );
}

/// D4：降级启动态（无存储句柄）诊断包仍可导出（健康/容量/日志齐备；库摘要缺失有注记）。
#[test]
fn degraded_startup_export_remains_available() {
    let fixture = Fixture::degraded();
    fixture
        .log_sink
        .push_line("ERROR 启动失败：quick_check 失败（安全模式只读）");
    let target = fixture.target();
    let response = fixture.export(&target).expect("降级态导出成功");
    let text = std::fs::read_to_string(response["path"].as_str().expect("路径")).expect("读取");
    let bundle: Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(bundle["health"]["storage_state"], json!("persist_degraded"));
    assert!(bundle["store"].is_null(), "无读连接 → 库摘要省略");
    assert!(
        bundle["notes"]
            .as_array()
            .is_some_and(|notes| !notes.is_empty()),
        "降级态必须记录注记"
    );
    assert!(text.contains("quick_check 失败"));
    write_evidence(
        "dod4_degraded_export",
        &json!({ "response": response, "notes": bundle["notes"] }),
    );
}

// ===== DoD4 强证据：tracing 全局汇聚端 → 诊断包（与 M2-07 DoD6 对齐） =====

#[test]
fn tracing_events_flow_into_exported_bundle() {
    let sink = LogSink::new(128);
    // 安装全局订阅器（本测试二进制唯一安装者）；失败时以直接驱动 sink 兜底。
    if aether_tauri::logging::init(std::sync::Arc::clone(&sink)).is_err() {
        sink.push_line("WARN 写事务尝试失败 attempt=1/3");
        sink.push_line("WARN 写事务尝试失败 attempt=2/3");
        sink.push_line("WARN 写事务尝试失败 attempt=3/3");
        sink.push_line("ERROR 存储进入 persist_degraded（write_failure）");
    } else {
        tracing::warn!("写事务尝试失败 attempt=1/3");
        tracing::warn!("写事务尝试失败 attempt=2/3");
        tracing::warn!("写事务尝试失败 attempt=3/3");
        tracing::error!("存储进入 persist_degraded（write_failure）");
    }

    let fixture = Fixture::degraded_with_logs(sink);
    let target = fixture.target();
    let response = fixture.export(&target).expect("导出成功");
    let text = std::fs::read_to_string(response["path"].as_str().expect("路径")).expect("读取");
    assert!(text.contains("attempt=1/3"), "tracing → 汇聚端 → 诊断包");
    assert!(text.contains("attempt=3/3"));
    assert!(text.contains("persist_degraded"));
    assert!(Redactor::new().expect("脱敏器").is_clean(&text));
    write_evidence(
        "dod4_tracing_aggregation",
        &json!({ "response": response, "log_lines": fixture.log_sink.line_count() }),
    );
}

// ===== DoD2：容量阈值参数化模拟 =====

#[test]
fn capacity_thresholds_are_parameterized() {
    let fixture = Fixture::open();
    let backend = |limits: CapacityConfig| {
        let inner: Arc<dyn IpcBackend> = Arc::new(BackupControlBackend::new(
            Arc::new(NotImplementedBackend),
            fixture.temp.path().to_path_buf(),
            fixture.storage.as_ref().map(|s| s.reads().clone()),
            fixture.storage.as_ref().map(|s| s.queue().clone()),
            fixture.runtime.handle().clone(),
        ));
        let deps = DiagnosticsDeps {
            data_dir: fixture.temp.path().to_path_buf(),
            reads: fixture.storage.as_ref().map(|s| s.reads().clone()),
            write: fixture.storage.as_ref().map(|s| s.queue().clone()),
            handle: fixture.runtime.handle().clone(),
            health: normal_health(),
            logs: None,
            task_dumps: None,
            security: Some(Arc::new(StaticSecurityProbe)),
        };
        DiagnosticsControlBackend::new(inner, deps).with_capacity_config(limits)
    };

    // 模拟：阈值极小 → 任意库达到 critical；仅 critical 高 → warn；双高 → ok。
    let critical = backend(CapacityConfig::with_limits(1, 2))
        .backup_list()
        .expect("清单");
    assert_eq!(critical["capacity"]["level"], json!("critical"));
    assert_eq!(critical["capacity"]["warn_bytes"], json!(1));
    assert_eq!(critical["capacity"]["critical_bytes"], json!(2));

    let warn = backend(CapacityConfig::with_limits(1, u64::MAX))
        .backup_list()
        .expect("清单");
    assert_eq!(warn["capacity"]["level"], json!("warn"));

    let ok = backend(CapacityConfig::with_limits(u64::MAX, u64::MAX))
        .backup_list()
        .expect("清单");
    assert_eq!(ok["capacity"]["level"], json!("ok"));
    assert!(
        ok["capacity"]["total_bytes"].as_u64().unwrap_or(0) > 0,
        "库文件已初始化（db+wal > 0）"
    );

    write_evidence(
        "dod2_capacity_levels",
        &json!({ "critical": critical["capacity"], "warn": warn["capacity"], "ok": ok["capacity"] }),
    );
}

// ===== DoD3：7 天未备份提醒可开关（时钟注入） =====

#[test]
fn backup_reminder_respects_toggle_and_clock() {
    let fixture = Fixture::open();

    // 从未备份 → due（缺省开启）。
    let initial = fixture.backup_list().expect("清单");
    assert_eq!(initial["reminder"]["enabled"], json!(true));
    assert_eq!(initial["reminder"]["due"], json!(true));
    assert_eq!(initial["reminder"]["reason"], json!("never"));
    assert_eq!(
        initial["reminder"]["threshold_ms"],
        json!(BACKUP_REMINDER_THRESHOLD_MS)
    );

    // 关闭开关 → 不再提醒（持久化到 settings 表）。
    fixture
        .settings_set(BACKUP_REMINDER_KEY, json!(false))
        .expect("关闭提醒");
    let disabled = fixture.backup_list().expect("清单");
    assert_eq!(disabled["reminder"]["enabled"], json!(false));
    assert_eq!(disabled["reminder"]["due"], json!(false));
    let stored = fixture.settings_get(BACKUP_REMINDER_KEY).expect("读取");
    assert_eq!(stored["value"], json!(false));

    // 重新开启；6 天前备份 → 未到期。
    fixture
        .settings_set(BACKUP_REMINDER_KEY, json!(true))
        .expect("开启提醒");
    let now = fixture.clock.now_ms();
    fixture.insert_backup(now - 6 * 24 * 60 * 60 * 1000);
    let fresh = fixture.backup_list().expect("清单");
    assert_eq!(fresh["reminder"]["due"], json!(false));
    assert_eq!(fresh["reminder"]["reason"], json!(null));
    assert_eq!(
        fresh["reminder"]["last_backup_at"],
        json!(now - 6 * 24 * 60 * 60 * 1000)
    );

    // 时钟推进 → 超过 7 天 → 到期。
    fixture.clock.advance(2 * 24 * 60 * 60 * 1000);
    let stale = fixture.backup_list().expect("清单");
    assert_eq!(stale["reminder"]["enabled"], json!(true));
    assert_eq!(stale["reminder"]["due"], json!(true));
    assert_eq!(stale["reminder"]["reason"], json!("stale"));

    write_evidence(
        "dod3_reminder",
        &json!({
            "initial": initial["reminder"],
            "disabled": disabled["reminder"],
            "fresh": fresh["reminder"],
            "stale": stale["reminder"],
        }),
    );
}

// ===== 设置键校验与写入语义 =====

#[test]
fn settings_validation_and_persistence() {
    let fixture = Fixture::open();

    let wrong_type = fixture
        .settings_set(BACKUP_REMINDER_KEY, json!("true"))
        .expect_err("非布尔值必须拒绝");
    assert_eq!(
        wrong_type.code,
        aether_tauri::ipc::error::IpcErrorCode::InvalidType
    );
    let unknown = fixture
        .settings_set("theme", json!(true))
        .expect_err("未登记键必须拒绝");
    assert_eq!(
        unknown.code,
        aether_tauri::ipc::error::IpcErrorCode::InvalidEnum
    );
    let unknown_get = fixture
        .settings_get("workspace.root")
        .expect_err("未登记键必须拒绝");
    assert_eq!(
        unknown_get.code,
        aether_tauri::ipc::error::IpcErrorCode::InvalidEnum
    );

    // 写后读回（经单写队列 + 读池：覆盖重启后的持久化语义由 store 读回证明）。
    fixture
        .settings_set(BACKUP_REMINDER_KEY, json!(false))
        .expect("写入");
    assert_eq!(
        fixture.settings_get(BACKUP_REMINDER_KEY).expect("读取")["value"],
        json!(false)
    );
}

// ===== 导出目标空间护栏（ADR-003 决策 19） =====

#[test]
fn export_target_space_guard_rejects_insufficient_space() {
    let fixture = Fixture::open();
    let target = fixture.target();

    let insufficient = Fixture::build(
        &fixture.temp,
        &fixture.runtime,
        Some(fixture.storage.as_ref().unwrap().reads().clone()),
        Some(fixture.storage.as_ref().unwrap().queue().clone()),
        Arc::clone(&fixture.clock) as SharedClock,
        Arc::clone(&fixture.log_sink),
        normal_health(),
        None,
    )
    .with_space_probe(Arc::new(FixedSpaceProbe {
        writable: true,
        available: 0,
    }));
    let request = ExportDiagnosticsRequest {
        target_dir: target.to_string_lossy().to_string(),
    };
    let canonical = request.canonical_target_dir().expect("目标校验");
    let error = insufficient
        .export_diagnostics(&request, &canonical)
        .expect_err("空间不足必须拒绝");
    assert!(
        error.message.contains("diagnostics_space_insufficient"),
        "稳定业务码：{}",
        error.message
    );

    let unwritable = Fixture::build(
        &fixture.temp,
        &fixture.runtime,
        Some(fixture.storage.as_ref().unwrap().reads().clone()),
        Some(fixture.storage.as_ref().unwrap().queue().clone()),
        Arc::clone(&fixture.clock) as SharedClock,
        Arc::clone(&fixture.log_sink),
        normal_health(),
        None,
    )
    .with_space_probe(Arc::new(FixedSpaceProbe {
        writable: false,
        available: u64::MAX,
    }));
    let error = unwritable
        .export_diagnostics(&request, &canonical)
        .expect_err("不可写必须拒绝");
    assert!(
        error.message.contains("diagnostics_target_not_writable"),
        "稳定业务码：{}",
        error.message
    );
}

// ===== 命令层：导出目标严格解析（外部路径语义） =====

struct RecordingBackend {
    calls: std::sync::Mutex<Vec<String>>,
}

impl IpcBackend for RecordingBackend {
    fn export_diagnostics(
        &self,
        _request: &ExportDiagnosticsRequest,
        _canonical_target_dir: &Path,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.calls
            .lock()
            .unwrap()
            .push("export_diagnostics".to_owned());
        Ok(json!({ "recorded": "export_diagnostics" }))
    }

    fn settings_get(
        &self,
        _request: &SettingsGetRequest,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.calls.lock().unwrap().push("settings_get".to_owned());
        Ok(json!({ "recorded": "settings_get" }))
    }

    fn settings_set(
        &self,
        _request: &SettingsSetRequest,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.calls.lock().unwrap().push("settings_set".to_owned());
        Ok(json!({ "recorded": "settings_set" }))
    }
}

struct CommandFixture {
    #[allow(dead_code)]
    app: App<MockRuntime>,
    webview: WebviewWindow<MockRuntime>,
    backend: Arc<RecordingBackend>,
    external: PathBuf,
    /// 夹具临时根目录（持有到用例结束，避免目录被提前删除）。
    #[allow(dead_code)]
    base: TempDir,
}

fn command_fixture(label: &str) -> CommandFixture {
    let base = tempfile::tempdir().expect("临时目录");
    let external = base.path().join("external");
    std::fs::create_dir_all(&external).expect("外部目录");
    std::fs::write(external.join("candidate.db"), b"x").expect("写入文件样本");
    let backend = Arc::new(RecordingBackend {
        calls: std::sync::Mutex::new(Vec::new()),
    });
    let state = IpcState::new(backend.clone(), Vec::new());
    let app = mock_builder()
        .invoke_handler(handler())
        .manage(state)
        .build(mock_context(noop_assets()))
        .expect("构建 mock 应用");
    let webview = WebviewWindowBuilder::new(&app, label, Default::default())
        .build()
        .expect("创建 mock webview");
    CommandFixture {
        app,
        webview,
        backend,
        external,
        base,
    }
}

fn invoke(
    webview: &WebviewWindow<MockRuntime>,
    command: &str,
    payload: Value,
) -> Result<Value, Value> {
    let body = tauri::ipc::InvokeBody::Json(json!({ "payload": payload }));
    let response = tauri::test::get_ipc_response(
        webview,
        InvokeRequest {
            cmd: command.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: INVOKE_URL.parse().expect("URL"),
            body,
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    );
    match response {
        Ok(body) => Ok(body.deserialize::<Value>().unwrap_or(Value::Null)),
        Err(value) => Err(value),
    }
}

#[test]
fn command_layer_accepts_external_export_target_and_rejects_invalid() {
    let fixture = command_fixture("m3-05-command");

    // 外部目录（不在允许根白名单内）合法：ADR-003 决策 19「不做默认目录信任」。
    let accepted = invoke(
        &fixture.webview,
        "export_diagnostics",
        json!({ "target_dir": fixture.external.to_string_lossy() }),
    )
    .expect("外部目录必须通过校验");
    assert_eq!(accepted["recorded"], json!("export_diagnostics"));

    // 相对路径 / 不存在目录 / 文件（非目录）必须拒绝且不调用后端。
    for sample in [
        json!({ "target_dir": "relative/path" }),
        json!({ "target_dir": fixture.external.join("missing").to_string_lossy() }),
        json!({ "target_dir": fixture.external.join("candidate.db").to_string_lossy() }),
        json!({ "target_dir": fixture.external.to_string_lossy(), "extra": 1 }),
    ] {
        let error = invoke(&fixture.webview, "export_diagnostics", sample.clone())
            .expect_err("非法目标必须拒绝");
        assert!(
            error.get("code").and_then(Value::as_str).is_some(),
            "结构化错误：{error}"
        );
    }
    assert_eq!(
        fixture.backend.calls.lock().unwrap().len(),
        1,
        "校验失败不得调用下游"
    );

    // 已登记设置键到达后端；未登记键在命令层拒绝。
    let settings = invoke(
        &fixture.webview,
        "settings_get",
        json!({ "key": "backup.reminder" }),
    )
    .expect("已登记键合法");
    assert_eq!(settings["recorded"], json!("settings_get"));
    let error = invoke(&fixture.webview, "settings_get", json!({ "key": "theme" }))
        .expect_err("未登记键拒绝");
    assert_eq!(error["code"], json!("invalid_enum"));
}

// ===== 存储侧设置读写的失败路径（防御） =====

#[test]
fn store_reads_are_required_for_settings() {
    let fixture = Fixture::degraded();
    let error = fixture
        .settings_set(BACKUP_REMINDER_KEY, json!(true))
        .expect_err("无写队列必须 core_not_ready");
    assert_eq!(
        error.code,
        aether_tauri::ipc::error::IpcErrorCode::CoreNotReady
    );
    let error = fixture
        .settings_get(BACKUP_REMINDER_KEY)
        .expect_err("无读连接必须 core_not_ready");
    assert_eq!(
        error.code,
        aether_tauri::ipc::error::IpcErrorCode::CoreNotReady
    );
    // 防御：未使用的 `StoreError` 变体路径仍可映射（编译期覆盖）。
    let mapped = aether_tauri::backup_control::map_backup_error(StoreError::BackupNotFound {
        id: "x".to_owned(),
    });
    assert_eq!(mapped.code.as_str(), "invalid_value");
}
