//! M5-05 集成测试：归档导出与会话 JSONL（设计 D13/D10/D11、ADR-021、ADR-018）。
//!
//! 覆盖：
//! - DoD1 Schema 一致性：JSONL 字段与附录 B（事件信封 9 字段）/附录 E（跨层字段映射）
//!   逐字段断言；`events`/`messages` 双序列与补读水位语义按 ADR（`events.seq` 为补读
//!   水位、`messages.seq` 独立序列）；
//! - DoD2 完整性：导出行数/哈希与库内一致（[`verify_archive_file`]）；中断（kill）
//!   不产生半成品（临时文件 + rename；子进程强杀用例）——≥1GB 大库抽样见
//!   `m5_05_export_big.rs`（`#[ignore]`，由 verify-m5-05 以 `AETHER_M5_05_BIG=1` 启用）；
//! - DoD3 脱敏：运行期生成密钥样本扫描 0 命中；`api_key_ref` 原样导出（D10）；
//! - DoD4 外部路径：选择器 canonicalize + 可写 + 空间检查（M3-04 同口径），不足拒绝；
//! - DoD6（后端面）：单会话/全库入口可用；归档产物与 M5-04 自动归档链路同格式可消费。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_control::ManualClock;
use aether_store::archive::{
    export_full_archive, verify_archive_file, ARCHIVE_PARTIAL_INFIX, ARCHIVE_SOURCE_ARCHIVE_EXPORT,
    ARCHIVE_SOURCE_SESSION_EXPORT,
};
use aether_store::{Store, StoreCommand};
use aether_tauri::archive_control::{
    ArchiveControlBackend, AUDIT_ACTION_ARCHIVE_EXPORTED, AUDIT_ACTION_SESSION_EXPORTED,
};
use aether_tauri::backup_control::SpaceProbe;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{ArchiveExportRequest, SessionExportRequest};
use aether_tauri::retention::RetentionTask;
use serde_json::{json, Value};
use tempfile::TempDir;

const SESSION_ID: &str = "01J0000000000000000000000A";
const CHILD_ENV: &str = "AETHER_M5_05_EXPORT_CHILD";

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

/// Windows CI 的 TEMP 常带 8.3 短名（如 `RUNNER~1`），而导出目标校验拒绝短名形态
/// （ADR-018/D13 守卫）。测试夹具统一走 canonicalize 后剥离 `\\?\` 的长名形态，
/// 与 m5_02/m5_03/m5_04 等 m5 测试的 `long_path` 口径一致。
fn long_path(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).expect("canonicalize 夹具路径");
    let text = canonical.to_string_lossy().to_string();
    #[cfg(windows)]
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        return stripped.to_string();
    }
    text
}

/// 种子数据：1 会话 + 2 消息 + 事件（控制/白名单混合，seq 连续）+ 1 run + 1 供应商引用。
fn seed_database(path: &Path) {
    let store = Store::open(path).expect("打开/迁移数据库");
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
                "INSERT INTO sessions (id, runtime_id, title, status, model, system_prompt, config, token_usage, created_at, updated_at) \
                 VALUES (?1, 'mock', '归档会话', 'idle', 'deepseek/deepseek-chat', '你是助手', '{\"native_id\":\"n-1\",\"api_key_ref\":\"keychain://aether/provider/01J000000000000000000000V1\"}', '{\"input_tokens\":3}', 10, 12)",
                [SESSION_ID],
            )
            .expect("插入会话");
        connection
            .execute(
                "INSERT INTO messages (id, session_id, role, content, seq, created_at) \
                 VALUES ('01J000000000000000000000M1', ?1, 'user', '你好', 1, 11), \
                        ('01J000000000000000000000M2', ?1, 'assistant', '回复', 2, 12)",
                [SESSION_ID],
            )
            .expect("插入消息");
        let control_events = [
            ("session.created", "{\"title\":\"归档会话\"}", 10),
            (
                "run.started",
                "{\"run_id\":\"01J000000000000000000000R1\"}",
                11,
            ),
            (
                "message.delta",
                "{\"message_id\":\"01J000000000000000000000M2\",\"text\":\"chunk\"}",
                12,
            ),
            ("usage", "{\"input_tokens\":3}", 13),
            ("log", "{\"level\":\"info\",\"message\":\"m5-05\"}", 14),
            (
                "message.completed",
                "{\"message_id\":\"01J000000000000000000000M2\"}",
                15,
            ),
            (
                "tool.call_started",
                "{\"tool\":\"read_file\",\"call_id\":\"c1\"}",
                16,
            ),
            (
                "tool.call_completed",
                "{\"tool\":\"read_file\",\"call_id\":\"c1\"}",
                17,
            ),
            (
                "permission.requested",
                "{\"request_id\":\"01J000000000000000000000P1\"}",
                18,
            ),
            ("runtime.status_changed", "{\"status\":\"ready\"}", 19),
            ("error", "{\"code\":\"x\",\"message\":\"recoverable\"}", 20),
        ];
        for (index, (event_type, payload, ts)) in control_events.iter().enumerate() {
            let payload = payload.replace('\'', "''");
            let sql = format!(
                "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
                 VALUES ('01J0000000000000000000E{index:04}', '{SESSION_ID}', NULL, 'mock', {seq}, '{event_type}', '{payload}', {ts}, 1)",
                index = index,
                seq = index + 1,
            );
            connection.execute(&sql, []).expect("插入事件");
        }
        connection
            .execute(
                "INSERT INTO runs (id, session_id, status, input_message_id, thinking_depth, started_at, finished_at) \
                 VALUES ('01J000000000000000000000R1', ?1, 'succeeded', '01J000000000000000000000M1', 2, 11, 15)",
                [SESSION_ID],
            )
            .expect("插入 run");
        connection
            .execute(
                "INSERT INTO providers (id, name, type, base_url, api_key_ref, enabled, is_builtin, created_at, updated_at) \
                 VALUES ('01J000000000000000000000V1', 'DeepSeek', 'deepseek', 'https://api.deepseek.com', 'keychain://aether/provider/01J000000000000000000000V1', 1, 0, 10, 10)",
                [],
            )
            .expect("插入供应商");
    }
}

struct Fixture {
    temp: TempDir,
    runtime: tokio::runtime::Runtime,
    boot: CoreBoot,
    archive: Arc<ArchiveControlBackend>,
}

impl Fixture {
    fn open() -> Self {
        Self::open_with_probe(None)
    }

    fn open_with_probe(probe: Option<Arc<dyn SpaceProbe>>) -> Self {
        let temp = tempfile::tempdir().expect("临时数据目录");
        seed_database(&temp.path().join("aether.db"));
        let runtime = new_runtime();
        let handle = runtime.handle().clone();
        let boot = boot_core_full(
            temp.path(),
            &handle,
            Arc::new(StaticRuntimeSummaries::unwired()),
        )
        .expect("启动核心（存储 + 管线）");
        let clock = Arc::new(ManualClock::new(1_800_000_000_000));
        let mut backend = ArchiveControlBackend::new(
            Arc::new(NotImplementedBackend),
            temp.path().to_path_buf(),
            Some(boot.reads.clone()),
            Some(boot.write.clone()),
            handle,
            clock.clone(),
        );
        if let Some(probe) = probe {
            backend = backend.with_space_probe(probe);
        }
        Self {
            temp,
            runtime,
            boot,
            archive: Arc::new(backend),
        }
    }

    fn archives_dir(&self) -> PathBuf {
        self.temp.path().join("archives")
    }

    fn export_session(&self, target: Option<&Path>) -> Value {
        let request = SessionExportRequest {
            session_id: SESSION_ID.to_owned(),
            target_dir: target.map(long_path),
        };
        let canonical = request.canonical_target_dir().expect("目标校验");
        self.archive
            .session_export(&request, canonical.as_deref())
            .expect("会话导出成功")
    }

    fn export_all(&self, target: Option<&Path>) -> Value {
        let request = ArchiveExportRequest {
            target_dir: target.map(long_path),
        };
        let canonical = request.canonical_target_dir().expect("目标校验");
        self.archive
            .archive_export(&request, canonical.as_deref())
            .expect("全库归档成功")
    }

    fn audit_actions(&self) -> Vec<(String, Option<String>, Option<String>)> {
        self.runtime
            .block_on(self.boot.reads.audit_log(100))
            .expect("审计查询")
            .into_iter()
            .map(|record| (record.action, record.result, record.resource))
            .collect()
    }

    fn shutdown(self) {
        let _ = self.runtime.block_on(self.boot.pipeline.shutdown());
        if let Some(storage) = self.boot.storage.take() {
            let _ = self.runtime.block_on(storage.shutdown());
        }
    }
}

/// 可切换的空间探针（故障注入）。
struct ToggleSpaceProbe {
    writable: AtomicBool,
    available: AtomicU64,
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

/// 运行期生成密钥样本（AGENTS §2.9：不把密钥形态字面量写进测试夹具）。
fn sample_api_key() -> String {
    format!("sk-ant-api03-{}", "A".repeat(48))
}

fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("读取归档")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("JSONL 行"))
        .collect()
}

/// 将归档产物复制到证据目录（verify-archive.mjs 独立校验；无环境变量时仅打印）。
fn copy_evidence_file(name: &str, path: &Path) {
    println!("[m5-05] 证据文件 {name} = {}", path.display());
    let Some(dir) = std::env::var_os("AETHER_M5_05_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::copy(path, dir.join(name));
}

/// 证据 JSON 归档（verify-m5-05.mjs 复核）。
fn write_evidence(name: &str, value: &Value) {
    println!("[m5-05] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M5_05_EVIDENCE_DIR") else {
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

// ===== DoD1：Schema 一致性（逐字段映射）=====

#[test]
fn session_export_field_mapping_and_sequences() {
    let fixture = Fixture::open();
    let response = fixture.export_session(None);
    let report = &response["export"];
    assert_eq!(report["source"], ARCHIVE_SOURCE_SESSION_EXPORT);
    assert_eq!(report["rows"], json!(14)); // 1 会话 + 2 消息 + 11 事件
    let path = PathBuf::from(report["path"].as_str().expect("路径"));
    assert!(path.exists());
    let verification = verify_archive_file(&path).expect("校验");
    assert!(verification.matches, "manifest 行数/哈希与 payload 一致");

    let lines = read_lines(&path);
    let manifest = &lines[0];
    assert_eq!(manifest["kind"], "manifest");
    assert_eq!(manifest["v"], 1);
    assert_eq!(manifest["session_id"], SESSION_ID);
    assert_eq!(manifest["rows"], json!(14));
    assert_eq!(manifest["sha256"], json!(report["sha256"]));

    // 附录 B：事件行 = 信封 9 字段（v/id/session_id/run_id/runtime_id/seq/ts/type/payload）+ kind。
    let envelope_fields = [
        "v",
        "id",
        "session_id",
        "run_id",
        "runtime_id",
        "seq",
        "ts",
        "type",
        "payload",
    ];
    let mut event_seqs = Vec::new();
    let mut event_types: Vec<String> = Vec::new();
    let mut message_rows: Vec<(i64, String, String)> = Vec::new();
    for line in &lines[1..] {
        let kind = line["kind"].as_str().expect("kind");
        let object = line.as_object().expect("对象");
        match kind {
            "session" => {
                assert_eq!(object["id"], SESSION_ID);
                assert_eq!(object["runtime_id"], "mock");
                assert_eq!(object["title"], "归档会话");
                assert_eq!(object["model"], "deepseek/deepseek-chat");
                assert_eq!(object["system_prompt"], "你是助手");
                assert_eq!(object["config"]["native_id"], "n-1");
                assert_eq!(object["token_usage"]["input_tokens"], 3);
            }
            "message" => {
                // 附录 E 跨层字段映射：messages.content 终稿 + seq/created_at(ts)。
                assert_eq!(
                    object.keys().cloned().collect::<Vec<_>>(),
                    vec![
                        "content",
                        "id",
                        "kind",
                        "role",
                        "seq",
                        "session_id",
                        "ts",
                        "v"
                    ],
                    "message 行字段集（ADR-021 A-2）"
                );
                assert_eq!(object["session_id"], SESSION_ID);
                message_rows.push((
                    object["seq"].as_i64().expect("seq"),
                    object["role"].as_str().expect("role").to_owned(),
                    object["content"].as_str().expect("content").to_owned(),
                ));
            }
            "event" => {
                for field in envelope_fields {
                    assert!(object.contains_key(field), "事件信封字段缺失：{field}");
                }
                assert_eq!(
                    object.len(),
                    envelope_fields.len() + 1,
                    "事件行 = 信封 9 字段 + kind（无多余字段）"
                );
                let event_type = object["type"].as_str().expect("type");
                event_types.push(event_type.to_owned());
                if event_type == "message.delta" {
                    assert_eq!(object["payload"]["text"], "chunk");
                }
                event_seqs.push(object["seq"].as_i64().expect("seq"));
            }
            other => panic!("未知 kind：{other}"),
        }
    }
    assert!(
        event_seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "events 按 seq 升序：{event_seqs:?}"
    );
    assert_eq!(event_seqs.len(), 11);
    assert_eq!(
        event_types,
        vec![
            "session.created",
            "run.started",
            "message.delta",
            "usage",
            "log",
            "message.completed",
            "tool.call_started",
            "tool.call_completed",
            "permission.requested",
            "runtime.status_changed",
            "error",
        ],
        "附录 B 事件类型原样导出"
    );
    assert_eq!(
        message_rows,
        vec![
            (1, "user".to_owned(), "你好".to_owned()),
            (2, "assistant".to_owned(), "回复".to_owned()),
        ],
        "messages.seq 独立序列（升序）+ 终稿内容"
    );
    // `events.seq` 为补读水位：导出中最大 seq 与会话库内一致。
    let max_seq = fixture
        .runtime
        .block_on(
            fixture
                .boot
                .reads
                .max_seq(&aether_core::SessionId::new(SESSION_ID).unwrap()),
        )
        .unwrap();
    assert_eq!(
        event_seqs.last().copied(),
        max_seq.map(|value| value as i64)
    );

    let actions = fixture.audit_actions();
    assert!(
        actions.iter().any(|(action, result, resource)| {
            action == AUDIT_ACTION_SESSION_EXPORTED
                && result.as_deref() == Some("success")
                && resource.as_deref() == Some(report["path"].as_str().unwrap())
        }),
        "审计 session.exported 必须落库：{actions:?}"
    );
    copy_evidence_file("dod1_session_archive.jsonl", &path);
    write_evidence(
        "dod1_field_mapping",
        &json!({
            "rows": report["rows"],
            "counts": report["counts"],
            "sha256": report["sha256"],
            "event_seqs": event_seqs,
            "message_rows": message_rows,
            "audit_success": true,
        }),
    );
    fixture.shutdown();
}

// ===== DoD2（kill 中断）：导出中断不留半成品 =====

fn child_slow_sanitize(
    value: &Value,
    counter: &AtomicUsize,
) -> Result<String, aether_store::StoreError> {
    let index = counter.fetch_add(1, Ordering::SeqCst);
    if index == 2 {
        // 模拟大库导出中途（父进程观察到暂存文件后强杀）。
        std::thread::sleep(Duration::from_secs(120));
    }
    serde_json::to_string(value).map_err(|error| aether_store::StoreError::Internal {
        reason: error.to_string(),
    })
}

/// 恒等脱敏（父测试重试路径；无密钥样本）。
fn identity_sanitize(value: &Value) -> Result<String, aether_store::StoreError> {
    serde_json::to_string(value).map_err(|error| aether_store::StoreError::Internal {
        reason: error.to_string(),
    })
}

/// 子进程入口（仅由父测试经 `--exact` 拉起）。
#[test]
fn m5_05_export_child_process() {
    if std::env::var(CHILD_ENV).ok().as_deref() != Some("1") {
        return;
    }
    let db = PathBuf::from(std::env::var("AETHER_M5_05_DB").expect("子进程 db"));
    let dir = PathBuf::from(std::env::var("AETHER_M5_05_DIR").expect("子进程归档目录"));
    let store = Store::open(&db).expect("子进程打开库");
    let counter = AtomicUsize::new(0);
    let sanitize = |value: &Value| child_slow_sanitize(value, &counter);
    let _ = export_full_archive(store.connection(), &dir, 1_800_000_000_000, &sanitize);
    std::process::exit(0);
}

#[test]
fn kill_during_export_leaves_no_half_artifact() {
    if std::env::var(CHILD_ENV).ok().as_deref() == Some("1") {
        return;
    }
    let dir = tempfile::TempDir::new().expect("临时目录");
    let db = dir.path().join("aether.db");
    seed_database(&db);
    let archives = dir.path().join("archives");
    std::fs::create_dir_all(&archives).expect("归档目录");

    let exe = std::env::current_exe().expect("当前测试可执行文件");
    let mut child = Command::new(exe)
        .args([
            "--exact",
            "m5_05_export_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .env("AETHER_M5_05_DB", &db)
        .env("AETHER_M5_05_DIR", &archives)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起导出子进程");

    // 等待 payload 暂存出现（导出已进入写出阶段）→ 强杀（Windows TerminateProcess / Unix SIGKILL）。
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut partials: Vec<PathBuf> = Vec::new();
    while Instant::now() < deadline {
        partials = archive_files_with(&archives, ARCHIVE_PARTIAL_INFIX);
        if !partials.is_empty() {
            break;
        }
        if let Some(status) = child.try_wait().expect("查询子进程") {
            panic!("导出子进程提前退出（{status:?}），无法覆盖中断窗口");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !partials.is_empty(),
        "中断窗口必须可观察（payload 暂存文件出现）"
    );
    child.kill().expect("强杀导出子进程");
    let _ = child.wait();

    // 中断后：无最终 `.jsonl` 产物（不产生半成品）；残留暂存可被清理。
    let finals = final_archives(&archives);
    assert!(finals.is_empty(), "中断不得产生最终产物：{finals:?}");
    let stale = aether_store::archive::cleanup_archive_temps(&archives);
    assert!(stale >= 1, "崩溃残留必须可被清理");
    assert!(archive_files_with(&archives, ARCHIVE_PARTIAL_INFIX).is_empty());

    // 恢复后再导出成功且可校验（同一目录，无残留干扰）。
    let store = Store::open(&db).expect("重开");
    let report = export_full_archive(
        store.connection(),
        &archives,
        1_800_000_000_001,
        &identity_sanitize,
    )
    .expect("中断后重试成功");
    let verification = verify_archive_file(&report.path).expect("校验");
    assert!(verification.matches);
    assert_eq!(verification.rows, 15); // 1 会话 + 2 消息 + 11 事件 + 1 run
    write_evidence(
        "dod2_kill_interrupt",
        &json!({
            "partial_observed": partials.len(),
            "no_final_artifact": true,
            "stale_cleaned": stale,
            "retry_rows": verification.rows,
            "retry_matches": verification.matches,
        }),
    );
}

fn archive_files_with(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = listing
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().contains(needle))
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files
}

/// 最终产物（文件名以 `.jsonl` 结尾；排除 `*.jsonl.archive-partial-*` 暂存）。
fn final_archives(dir: &Path) -> Vec<PathBuf> {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = listing
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files
}

// ===== DoD3：脱敏 0 命中 + api_key_ref 原样导出 =====

#[test]
fn export_redacts_secrets_and_keeps_api_key_refs() {
    let fixture = Fixture::open();
    // 会话内容中注入运行期生成密钥样本（模拟历史消息中出现的敏感串）。
    let key = sample_api_key();
    let message_id = "01J000000000000000000000M3";
    fixture
        .runtime
        .block_on(fixture.boot.write.execute(StoreCommand::InsertMessage {
            message: parse_message(message_id, &format!("token={key}")),
        }))
        .expect("注入消息");

    let response = fixture.export_session(None);
    let path = PathBuf::from(response["export"]["path"].as_str().expect("路径"));
    let text = std::fs::read_to_string(&path).expect("读取归档");
    assert!(
        !text.contains(&key) && !text.contains("sk-"),
        "归档不得包含密钥样本/前缀（D10）"
    );
    assert!(
        text.contains("[REDACTED:api-key]"),
        "命中的敏感串必须被脱敏标记替换"
    );

    // 全库归档同样 0 命中；`api_key_ref` 引用原样导出（引用非密钥本体）。
    let response = fixture.export_all(None);
    let path = PathBuf::from(response["export"]["path"].as_str().expect("路径"));
    let text = std::fs::read_to_string(&path).expect("读取归档");
    assert!(!text.contains("sk-"), "全库归档不得包含密钥前缀（D10）");
    assert!(
        text.contains("keychain://aether/provider/01J000000000000000000000V1"),
        "api_key_ref 引用必须原样导出（D10：引用非密钥本体）"
    );
    write_evidence(
        "dod3_redaction",
        &json!({
            "session_export_clean": true,
            "full_archive_clean": true,
            "api_key_ref_exported": true,
        }),
    );
    fixture.shutdown();
}

fn parse_message(id: &str, content: &str) -> aether_core::Message {
    aether_core::Message {
        id: aether_core::MessageId::new(id).unwrap(),
        session_id: aether_core::SessionId::new(SESSION_ID).unwrap(),
        run_id: None,
        client_msg_id: None,
        role: aether_core::MessageRole::User,
        content: content.to_owned(),
        content_parts: None,
        tool_calls: None,
        parent_message_id: None,
        seq: 0,
        created_at: 30,
    }
}

// ===== DoD4：外部路径与空间护栏 =====

#[test]
fn external_target_and_space_guards() {
    let probe = Arc::new(ToggleSpaceProbe {
        writable: AtomicBool::new(true),
        available: AtomicU64::new(u64::MAX),
    });
    let fixture = Fixture::open_with_probe(Some(probe.clone()));
    let external = fixture.temp.path().join("external-target");
    std::fs::create_dir_all(&external).expect("外部目录");

    // 外部目录：可写 + 空间充足 → 成功，产物落在外部目录。
    let response = fixture.export_all(Some(&external));
    assert_eq!(response["export"]["source"], ARCHIVE_SOURCE_ARCHIVE_EXPORT);
    let path = PathBuf::from(response["export"]["path"].as_str().expect("路径"));
    let expected_dir = PathBuf::from(long_path(&external));
    assert_eq!(path.parent(), Some(expected_dir.as_path()));

    // 不可写 → archive_target_not_writable。
    probe.writable.store(false, Ordering::SeqCst);
    let request = ArchiveExportRequest {
        target_dir: Some(long_path(&external)),
    };
    let canonical = request.canonical_target_dir().expect("目标校验");
    let error = fixture
        .archive
        .archive_export(&request, canonical.as_deref())
        .expect_err("不可写必须拒绝");
    assert!(
        error.message.contains("archive_target_not_writable"),
        "{:?}",
        error.message
    );

    // 空间不足 → archive_space_insufficient（不得产生半成品）。
    probe.writable.store(true, Ordering::SeqCst);
    probe.available.store(0, Ordering::SeqCst);
    let error = fixture
        .archive
        .archive_export(&request, canonical.as_deref())
        .expect_err("空间不足必须拒绝");
    assert!(
        error.message.contains("archive_space_insufficient"),
        "{:?}",
        error.message
    );
    assert_eq!(final_archives(&external).len(), 1, "失败路径不得新增产物");

    let actions = fixture.audit_actions();
    assert!(
        actions.iter().any(|(action, result, _)| {
            action == AUDIT_ACTION_ARCHIVE_EXPORTED && result.as_deref() == Some("failed")
        }),
        "失败审计必须落库：{actions:?}"
    );
    write_evidence(
        "dod4_external_and_space_guards",
        &json!({
            "external_export_ok": response["export"]["file_name"],
            "not_writable_code": "archive_target_not_writable",
            "insufficient_code": "archive_space_insufficient",
        }),
    );
    fixture.shutdown();
}

// ===== DoD6（后端面）：与 M5-04 自动归档链路同格式可消费 =====

#[test]
fn export_artifact_is_consumable_by_retention_archive_chain() {
    let fixture = Fixture::open();
    let export = fixture.export_all(None);
    let export_path = PathBuf::from(export["export"]["path"].as_str().expect("路径"));

    // 构造保留清理场景：启用保留策略（窗口 7 天）→ 自动归档链路产出一致格式产物。
    fixture
        .runtime
        .block_on(fixture.boot.write.execute(StoreCommand::UpsertSetting {
            key: "events.retention.enabled".to_owned(),
            value: "true".to_owned(),
            updated_at: 1,
        }))
        .expect("启用保留");
    let task: Arc<RetentionTask> = fixture.archive.retention_task().expect("保留任务已接线");
    let report = fixture.runtime.block_on(task.run_once());
    assert_eq!(report.result(), "success");
    // 白名单类型且超窗（种子 ts=10..20 远早于 ManualClock + 7 天）。
    let archives = archive_files_with(&fixture.archives_dir(), "events-archive-");
    assert_eq!(archives.len(), 1, "自动归档产物必须写出");

    // 同一校验器消费两种产物（同格式：manifest + kind 行）。
    let mut retention_verification = None;
    for path in [&export_path, &archives[0]] {
        let verification = verify_archive_file(path).expect("校验");
        assert!(verification.matches, "{}", path.display());
        assert_eq!(verification.manifest.v, 1);
        if path == &archives[0] {
            retention_verification = Some(verification);
        }
    }
    let retention_verification = retention_verification.expect("自动归档校验结果");
    let retention_lines = read_lines(&archives[0]);
    assert_eq!(retention_lines[0]["source"], "auto-retention");
    let cleaned_ids: Vec<&str> = retention_lines[1..]
        .iter()
        .filter(|line| line["kind"] == "event")
        .filter_map(|line| line["id"].as_str())
        .collect();
    assert!(
        cleaned_ids.len() == 3,
        "仅 message.delta/usage/log 三类超窗事件进入自动归档：{cleaned_ids:?}"
    );
    copy_evidence_file("dod6_full_archive.jsonl", &export_path);
    copy_evidence_file("dod6_retention_archive.jsonl", &archives[0]);
    write_evidence(
        "dod6_chain_consumable",
        &json!({
            "export_rows": export["export"]["rows"],
            "retention_rows": retention_verification.rows,
            "retention_source": retention_lines[0]["source"],
            "cleaned_ids": cleaned_ids,
        }),
    );
    fixture.shutdown();
}
