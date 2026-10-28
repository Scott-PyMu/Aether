//! M6-03 集成测试：完整审计查询与导出（设计 D9/SE-03、ADR-023、ADR-018）。
//!
//! 覆盖：
//! - DoD1 记录完备性：工具调用终态（`tool.call_completed` / `tool.call_failed`）经真实
//!   Mock 适配器全链路 → `tool.call` 审计行 1:1（协议上报面 100%；脚本比对）；
//! - DoD2 查询：过滤（会话/时间/actor/action/resource）+ 分页 + `ts DESC, id DESC`
//!   稳定排序（经 [`ArchiveControlBackend`] 的 IPC 面）；
//! - DoD3 导出：JSONL + manifest 完整校验、脱敏 0 命中、外部路径空间护栏、失败不留
//!   半成品（共享原子写管道，M5-05 同口径）；
//! - DoD5 保留联动：审计行不被 M5-04 保留策略触碰。
//!
//! 环境（DoD1 E2E）：真实 Mock 适配器进程经 `AETHER_MOCK_ADAPTER` 指定；未设置且未
//! 要求时显式跳过，`AETHER_REQUIRE_MOCK_ADAPTER=1` 时缺路径直接失败。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, Supervisor};
use aether_control::{LifecycleConfig, ManualClock, SessionManager, SystemClock};
use aether_core::{EventEnvelope, EventPayload, RunStatus, SessionId};
use aether_store::archive::{
    export_audit_archive, verify_archive_file, ARCHIVE_SOURCE_AUDIT_EXPORT,
};
use aether_store::{AuditLogRecord, AuditQuery, ReadPool, Store, StoreCommand, StoreError};
use aether_tauri::adapter_executor::{
    AdapterRunExecutor, ToolCallAuditSink, AUDIT_ACTION_TOOL_CALL,
};
use aether_tauri::archive_control::ArchiveControlBackend;
use aether_tauri::backup_control::SpaceProbe;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    AuditExportRequest, AuditQueryRequest, SessionCreateRequest, SessionSendRequest,
};
use aether_tauri::runtime_control::{boot_supervisor, run_supervisor_startup};
use aether_tauri::session_backend::SessionBackend;
use serde_json::{json, Value};
use tempfile::TempDir;

const SESSION_A: &str = "01J0000000000000000000000A";
const SESSION_B: &str = "01J0000000000000000000000B";

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

/// Windows CI 的 TEMP 常带 8.3 短名（如 `RUNNER~1`），而导出目标校验拒绝短名形态
/// （ADR-018/D13 守卫）。测试夹具统一走 canonicalize 后剥离 `\\?\` 的长名形态，
/// 与 m5_02/m5_03/m5_04/m5_05 的 `long_path` 口径一致。
fn long_path(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).expect("canonicalize 夹具路径");
    let text = canonical.to_string_lossy().to_string();
    #[cfg(windows)]
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        return stripped.to_string();
    }
    text
}

fn mock_binary() -> Option<PathBuf> {
    match std::env::var_os("AETHER_MOCK_ADAPTER") {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            if std::env::var("AETHER_REQUIRE_MOCK_ADAPTER").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_MOCK_ADAPTER=1 但 AETHER_MOCK_ADAPTER 未设置");
            }
            eprintln!(
                "SKIP：AETHER_MOCK_ADAPTER 未设置（运行 pnpm verify:m6-03 构建 Mock 后执行）"
            );
            None
        }
    }
}

fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

/// 运行期生成密钥样本（AGENTS §2.9：不把密钥形态字面量写进测试夹具）。
fn sample_api_key() -> String {
    format!("sk-ant-api03-{}", "B".repeat(48))
}

fn write_evidence(name: &str, value: &Value) {
    println!("[m6-03] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M6_03_EVIDENCE_DIR") else {
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

// ===== 通用审计夹具（存储 + 归档后端） =====

struct AuditFixture {
    temp: TempDir,
    runtime: tokio::runtime::Runtime,
    boot: CoreBoot,
    archive: Arc<ArchiveControlBackend>,
}

impl AuditFixture {
    fn open() -> Self {
        Self::open_with_probe(None)
    }

    fn open_with_probe(probe: Option<Arc<dyn SpaceProbe>>) -> Self {
        let temp = tempfile::tempdir().expect("临时数据目录");
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
            clock,
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

    fn insert_audit(&self, record: AuditLogRecord) {
        self.runtime
            .block_on(
                self.boot
                    .write
                    .execute(StoreCommand::InsertAudit { record }),
            )
            .expect("插入审计行");
    }

    fn audit_rows(&self) -> Vec<AuditLogRecord> {
        self.runtime
            .block_on(self.boot.reads.audit_query(AuditQuery {
                limit: Some(500),
                ..AuditQuery::default()
            }))
            .expect("审计查询")
            .rows
    }

    fn query(&self, request: &AuditQueryRequest) -> Value {
        self.archive.audit_query(request).expect("audit_query 后端")
    }

    fn export(&self, request: &AuditExportRequest, target: Option<&Path>) -> Value {
        let canonical = request.canonical_target_dir().expect("目标校验");
        let _ = target;
        self.archive
            .audit_export(request, canonical.as_deref())
            .expect("audit_export 后端")
    }

    fn shutdown(self) {
        let _ = self.runtime.block_on(self.boot.pipeline.shutdown());
        if let Some(storage) = self.boot.storage.take() {
            let _ = self.runtime.block_on(storage.shutdown());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn audit_record(
    id: &str,
    session: Option<&str>,
    actor: &str,
    action: &str,
    resource: Option<&str>,
    detail: Option<&str>,
    result: &str,
    ts: i64,
) -> AuditLogRecord {
    AuditLogRecord {
        id: id.to_owned(),
        session_id: session.map(str::to_owned),
        runtime_id: Some("mock".to_owned()),
        actor: actor.to_owned(),
        action: action.to_owned(),
        resource: resource.map(str::to_owned),
        detail: detail.map(str::to_owned),
        result: Some(result.to_owned()),
        ts,
    }
}

// ===== DoD2：查询（过滤/分页/排序） =====

#[test]
fn audit_query_filters_pagination_and_stable_sort() {
    let fixture = AuditFixture::open();
    fixture.insert_audit(audit_record(
        "01J0000000000000000000Q01",
        Some(SESSION_A),
        "agent",
        "tool.call",
        Some("mock.echo"),
        Some("{\"tool_call_id\":\"c1\"}"),
        "success",
        100,
    ));
    fixture.insert_audit(audit_record(
        "01J0000000000000000000Q02",
        Some(SESSION_A),
        "agent",
        "tool.call",
        Some("mock.read_file"),
        Some("{\"tool_call_id\":\"c2\"}"),
        "failed",
        200,
    ));
    fixture.insert_audit(audit_record(
        "01J0000000000000000000Q03",
        Some(SESSION_B),
        "user",
        "session.created",
        Some("session:01J0000000000000000000000B"),
        None,
        "ok",
        300,
    ));

    // 空过滤：全部 + 稳定排序 ts DESC + has_more=false。
    let response = fixture.query(&AuditQueryRequest::default());
    assert_eq!(response["has_more"], false);
    assert_eq!(response["limit"], 100);
    assert_eq!(response["offset"], 0);
    let rows = response["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["id"], "01J0000000000000000000Q03", "ts 降序");
    assert_eq!(rows[1]["id"], "01J0000000000000000000Q02");
    assert_eq!(rows[2]["id"], "01J0000000000000000000Q01");

    // 组合过滤（actor + action）+ 分页（has_more 探测：命中 2，offset=1/limit=1 → false）。
    let response = fixture.query(&AuditQueryRequest {
        actor: Some("agent".to_owned()),
        action: Some("tool.call".to_owned()),
        limit: Some(1),
        offset: Some(1),
        ..AuditQueryRequest::default()
    });
    assert_eq!(response["has_more"], false);
    assert_eq!(response["limit"], 1);
    assert_eq!(response["offset"], 1);
    let rows = response["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "01J0000000000000000000Q01", "第二页");
    assert_eq!(rows[0]["detail"], "{\"tool_call_id\":\"c1\"}");

    // 首页 has_more=true（命中 2 > limit 1）。
    let response = fixture.query(&AuditQueryRequest {
        actor: Some("agent".to_owned()),
        action: Some("tool.call".to_owned()),
        limit: Some(1),
        ..AuditQueryRequest::default()
    });
    assert_eq!(response["has_more"], true);

    // 时间闭区间 + 会话过滤 + resource 过滤。
    let response = fixture.query(&AuditQueryRequest {
        session_id: Some(SESSION_A.to_owned()),
        resource: Some("mock.echo".to_owned()),
        from_ts: Some(50),
        to_ts: Some(150),
        ..AuditQueryRequest::default()
    });
    assert_eq!(response["rows"].as_array().map(Vec::len), Some(1));
    assert_eq!(response["rows"][0]["id"], "01J0000000000000000000Q01");

    write_evidence(
        "dod2_query",
        &json!({
            "rows": 3,
            "actor_action_rows": 2,
            "window_rows": 1,
            "has_more_probe": true,
            "order": ["01J0000000000000000000Q03", "01J0000000000000000000Q02", "01J0000000000000000000Q01"],
        }),
    );
    fixture.shutdown();
}

// ===== DoD3：导出（完整性/过滤/外部路径/脱敏/失败不留半成品） =====

#[test]
fn audit_export_is_verifiable_filtered_and_redacted() {
    let fixture = AuditFixture::open();
    let secret = sample_api_key();
    fixture.insert_audit(audit_record(
        "01J0000000000000000000R01",
        Some(SESSION_A),
        "agent",
        "tool.call",
        Some("mock.echo"),
        Some("{\"tool_call_id\":\"c1\",\"duration_ms\":12}"),
        "success",
        100,
    ));
    fixture.insert_audit(audit_record(
        "01J0000000000000000000R02",
        Some(SESSION_A),
        "agent",
        "tool.call",
        Some("mock.read_file"),
        Some(&format!(
            "{{\"tool_call_id\":\"c2\",\"error\":\"{secret}\"}}"
        )),
        "failed",
        200,
    ));
    fixture.insert_audit(audit_record(
        "01J0000000000000000000R03",
        Some(SESSION_B),
        "user",
        "session.created",
        None,
        None,
        "ok",
        300,
    ));

    // 全量导出（缺省 archives/）。
    let response = fixture.export(&AuditExportRequest::default(), None);
    let report = &response["export"];
    assert_eq!(report["source"], ARCHIVE_SOURCE_AUDIT_EXPORT);
    assert_eq!(report["rows"], 3);
    let path = PathBuf::from(report["path"].as_str().expect("路径"));
    assert!(
        path.starts_with(fixture.temp.path().join("archives")),
        "缺省目录 = 数据目录 archives/：{}",
        path.display()
    );
    let verification = verify_archive_file(&path).expect("校验归档");
    assert!(verification.matches, "manifest 行数/哈希与 payload 一致");
    assert_eq!(verification.manifest.source, ARCHIVE_SOURCE_AUDIT_EXPORT);
    assert_eq!(report["counts"]["audits"], 3);

    // 行形态：kind=audit + 白名单字段 + 脱敏 0 命中（D10；密钥不得落导出）。
    let text = std::fs::read_to_string(&path).expect("读取导出");
    assert!(
        !text.contains(&secret) && !text.contains("sk-"),
        "导出不得含密钥样本"
    );
    assert!(text.contains("[REDACTED:api-key]"), "命中样本必须被替换");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("JSONL 行"))
        .collect();
    assert_eq!(lines[0]["kind"], "manifest");
    assert_eq!(lines[0]["v"], 1);
    for line in &lines[1..] {
        assert_eq!(line["kind"], "audit");
        let object = line.as_object().expect("对象");
        for field in [
            "v",
            "kind",
            "id",
            "session_id",
            "runtime_id",
            "actor",
            "action",
            "resource",
            "detail",
            "result",
            "ts",
        ] {
            assert!(object.contains_key(field), "audit 行字段缺失：{field}");
        }
        assert_eq!(object.len(), 11, "audit 行 = 白名单 11 字段（无多余字段）");
    }
    // 排序：ts DESC（R03 → R02 → R01）。
    assert_eq!(lines[1]["id"], "01J0000000000000000000R03");
    assert_eq!(lines[3]["id"], "01J0000000000000000000R01");
    // detail 为 JSON 文本 → 按 JSON 输出；缺失 → null。
    assert_eq!(lines[3]["detail"]["tool_call_id"], "c1");
    assert!(lines[2]["detail"]["error"]
        .as_str()
        .unwrap_or_default()
        .contains("REDACTED"));
    assert!(lines[1]["detail"].is_null());

    // 过滤导出：仅 SESSION_A 的 tool.call（= 2 行）。
    let filtered = fixture.export(
        &AuditExportRequest {
            session_id: Some(SESSION_A.to_owned()),
            action: Some("tool.call".to_owned()),
            ..AuditExportRequest::default()
        },
        None,
    );
    assert_eq!(filtered["export"]["rows"], 2);
    assert_eq!(filtered["export"]["counts"]["audits"], 2);

    write_evidence(
        "dod3_export",
        &json!({
            "rows": report["rows"],
            "sha256": report["sha256"],
            "verified": verification.matches,
            "redacted": true,
            "filtered_rows": filtered["export"]["rows"],
        }),
    );
    fixture.shutdown();
}

/// 可切换空间探针（故障注入；与 M5-05 同口径）。
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

#[test]
fn audit_export_external_target_guards() {
    let probe = Arc::new(ToggleSpaceProbe {
        writable: AtomicBool::new(true),
        available: AtomicU64::new(u64::MAX),
    });
    let fixture = AuditFixture::open_with_probe(Some(probe.clone()));
    fixture.insert_audit(audit_record(
        "01J0000000000000000000S01",
        Some(SESSION_A),
        "agent",
        "tool.call",
        Some("mock.echo"),
        None,
        "success",
        100,
    ));
    let external = fixture.temp.path().join("external-audit");
    std::fs::create_dir_all(&external).expect("外部目录");

    // 可写 + 空间充足：产物落在外部目录。
    let request = AuditExportRequest {
        target_dir: Some(long_path(&external)),
        ..AuditExportRequest::default()
    };
    let response = fixture.export(&request, Some(&external));
    let path = PathBuf::from(response["export"]["path"].as_str().expect("路径"));
    let expected_dir = PathBuf::from(long_path(&external));
    assert_eq!(path.parent(), Some(expected_dir.as_path()));

    // 不可写 → archive_target_not_writable（不产生新产物）。
    probe.writable.store(false, Ordering::SeqCst);
    let error = fixture
        .archive
        .audit_export(&request, Some(&external))
        .expect_err("不可写必须拒绝");
    assert!(
        error.message.contains("archive_target_not_writable"),
        "{:?}",
        error.message
    );
    // 空间不足 → archive_space_insufficient。
    probe.writable.store(true, Ordering::SeqCst);
    probe.available.store(0, Ordering::SeqCst);
    let error = fixture
        .archive
        .audit_export(&request, Some(&external))
        .expect_err("空间不足必须拒绝");
    assert!(
        error.message.contains("archive_space_insufficient"),
        "{:?}",
        error.message
    );
    let finals: Vec<String> = std::fs::read_dir(&external)
        .expect("列出外部目录")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(finals.len(), 1, "失败路径不得新增产物：{finals:?}");

    write_evidence(
        "dod3_external_guards",
        &json!({
            "external_export": response["export"]["file_name"],
            "not_writable_code": "archive_target_not_writable",
            "insufficient_code": "archive_space_insufficient",
        }),
    );
    fixture.shutdown();
}

#[test]
fn audit_export_failure_leaves_no_partial_artifact() {
    let dir = tempfile::tempdir().expect("临时目录");
    let store = Store::open(dir.path().join("aether.db")).expect("打开库");
    store
        .connection()
        .execute(
            "INSERT INTO audit_log (id, session_id, runtime_id, actor, action, resource, detail, result, ts) \
             VALUES ('partial-1', NULL, 'mock', 'agent', 'tool.call', 'mock.echo', NULL, 'success', 1)",
            [],
        )
        .expect("种子审计行");
    let archives = dir.path().join("archives");
    std::fs::create_dir_all(&archives).expect("归档目录");
    // 注入脱敏阶段失败（逐行 sanitizer 返回 Err）→ 导出失败且无产物/暂存残留。
    let failing = |_value: &Value| -> Result<String, StoreError> {
        Err(StoreError::Internal {
            reason: "注入：脱敏阶段失败".to_owned(),
        })
    };
    let result = export_audit_archive(
        store.connection(),
        &archives,
        &AuditQuery::default(),
        1_800_000_000_000,
        &failing,
    );
    assert!(result.is_err(), "注入失败必须向上传播");
    let leftovers: Vec<String> = std::fs::read_dir(&archives)
        .expect("列出")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        leftovers.is_empty(),
        "失败路径不得产生半成品/暂存残留：{leftovers:?}"
    );
    write_evidence(
        "dod3_interrupt",
        &json!({ "no_partial_artifact": true, "leftovers": 0 }),
    );
}

// ===== DoD1：工具调用终态 → `tool.call` 审计行（含脱敏与幂等边界） =====

#[test]
fn tool_call_audit_sink_maps_terminal_events_and_redacts() {
    let temp = tempfile::tempdir().expect("临时数据目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let CoreBoot {
        storage: _slot,
        reads,
        write,
        pipeline,
        ..
    } = boot_core_full(
        temp.path(),
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("启动核心");
    let sink = ToolCallAuditSink::new(write.clone(), handle.clone());

    let session = SessionId::new(SESSION_A).unwrap();
    let run = aether_core::RunId::new("01J0000000000000000000000R").unwrap();
    let secret = sample_api_key();
    let envelope = |event: EventPayload, ts: i64| EventEnvelope {
        v: 1,
        id: aether_core::EventId::new("01J00000000000000000000E01").unwrap(),
        session_id: session.clone(),
        run_id: Some(run.clone()),
        runtime_id: aether_core::RuntimeId::new("mock").unwrap(),
        seq: 1,
        ts,
        payload: event,
    };

    // 非终态（started）不产生审计行。
    sink.audit_terminal(
        &session,
        &run,
        &envelope(
            EventPayload::ToolCallStarted(aether_core::ToolCallStartedPayload {
                tool_call_id: aether_core::ToolCallId::new("01J00000000000000000000C01").unwrap(),
                tool_name: "mock.echo".to_owned(),
                args: json!({ "path": "README.md" }),
            }),
            10,
        ),
    );
    // 终态 completed → success。
    sink.audit_terminal(
        &session,
        &run,
        &envelope(
            EventPayload::ToolCallCompleted(aether_core::ToolCallCompletedPayload {
                tool_call_id: aether_core::ToolCallId::new("01J00000000000000000000C01").unwrap(),
                tool_name: "mock.echo".to_owned(),
                duration_ms: 12,
            }),
            20,
        ),
    );
    // 终态 failed，error.message 携带密钥样本 → detail 脱敏 0 命中。
    sink.audit_terminal(
        &session,
        &run,
        &envelope(
            EventPayload::ToolCallFailed(aether_core::ToolCallFailedPayload {
                tool_call_id: aether_core::ToolCallId::new("01J00000000000000000000C02").unwrap(),
                tool_name: "mock.read_file".to_owned(),
                duration_ms: 7,
                error: aether_core::ErrorInfo {
                    code: "tool_execution_failed".to_owned(),
                    message: format!("read failed: {secret}"),
                    recoverable: true,
                },
            }),
            30,
        ),
    );

    assert!(
        wait_until(Duration::from_secs(10), || {
            let count = runtime
                .block_on(reads.audit_query(AuditQuery {
                    action: Some(AUDIT_ACTION_TOOL_CALL.to_owned()),
                    limit: Some(50),
                    ..AuditQuery::default()
                }))
                .map(|result| result.rows.len())
                .unwrap_or(0);
            count == 2
        }),
        "终态工具事件必须聚合为 2 条 tool.call 审计行"
    );
    let result = runtime
        .block_on(reads.audit_query(AuditQuery {
            action: Some(AUDIT_ACTION_TOOL_CALL.to_owned()),
            ..AuditQuery::default()
        }))
        .expect("审计查询");
    assert_eq!(result.rows.len(), 2);
    assert_eq!(sink.records(), 2);
    assert_eq!(sink.failures(), 0);

    let completed = result
        .rows
        .iter()
        .find(|row| row.result.as_deref() == Some("success"))
        .expect("success 行");
    assert_eq!(completed.actor, "agent");
    assert_eq!(completed.session_id.as_deref(), Some(SESSION_A));
    assert_eq!(completed.resource.as_deref(), Some("mock.echo"));
    assert_eq!(completed.runtime_id.as_deref(), Some("mock"));
    assert_eq!(completed.ts, 20);
    let detail: Value =
        serde_json::from_str(completed.detail.as_deref().expect("detail")).expect("detail JSON");
    assert_eq!(detail["tool_call_id"], "01J00000000000000000000C01");
    assert_eq!(detail["run_id"], "01J0000000000000000000000R");
    assert_eq!(detail["duration_ms"], 12);

    let failed = result
        .rows
        .iter()
        .find(|row| row.result.as_deref() == Some("failed"))
        .expect("failed 行");
    assert_eq!(failed.resource.as_deref(), Some("mock.read_file"));
    let detail_text = failed.detail.as_deref().expect("detail");
    assert!(
        !detail_text.contains(&secret) && !detail_text.contains("sk-"),
        "审计 detail 不得包含密钥样本：{detail_text}"
    );
    assert!(detail_text.contains("[REDACTED:api-key]"));
    let detail: Value = serde_json::from_str(detail_text).expect("detail JSON");
    assert_eq!(detail["error"]["code"], "tool_execution_failed");

    write_evidence(
        "dod1_sink",
        &json!({
            "records": sink.records(),
            "failures": sink.failures(),
            "redacted": true,
            "non_terminal_ignored": true,
        }),
    );
    let _ = runtime.block_on(pipeline.shutdown());
    let _ = &_slot;
}

// ===== DoD5：保留策略不触碰审计行 =====

#[test]
fn retention_policy_never_deletes_audit_rows() {
    let fixture = AuditFixture::open();
    fixture.insert_audit(audit_record(
        "01J0000000000000000000V01",
        Some(SESSION_A),
        "agent",
        "tool.call",
        Some("mock.echo"),
        None,
        "success",
        10,
    ));
    fixture.insert_audit(audit_record(
        "01J0000000000000000000V02",
        Some(SESSION_A),
        "user",
        "session.created",
        None,
        None,
        "ok",
        10,
    ));
    // 超窗白名单事件（log）→ 保留策略应清理（对照）；审计行必须完整保留。
    let raw_event = json!({
        "v": 1,
        "id": "01J00000000000000000000W01",
        "session_id": SESSION_A,
        "run_id": null,
        "runtime_id": "mock",
        "seq": 1,
        "ts": 10,
        "type": "log",
        "payload": {
            "level": "info",
            "message": "old"
        }
    });
    let outcome = fixture
        .runtime
        .block_on(fixture.boot.pipeline.submit(raw_event))
        .expect("提交超窗事件");
    assert!(outcome.is_persisted(), "超窗事件必须落盘：{outcome:?}");
    fixture
        .runtime
        .block_on(fixture.boot.write.execute(StoreCommand::UpsertSetting {
            key: "events.retention.enabled".to_owned(),
            value: "true".to_owned(),
            updated_at: 1,
        }))
        .expect("启用保留");

    let task = fixture.archive.retention_task().expect("保留任务已接线");
    let report = fixture.runtime.block_on(task.run_once());
    assert_eq!(report.result(), "success");

    let rows = fixture.audit_rows();
    let mut ids: Vec<&str> = rows
        .iter()
        .filter(|row| row.action == AUDIT_ACTION_TOOL_CALL || row.action == "session.created")
        .map(|row| row.id.as_str())
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec!["01J0000000000000000000V01", "01J0000000000000000000V02"],
        "审计行不得被保留策略静默删除"
    );
    // 对照：白名单事件已被清理。
    let remaining = fixture
        .runtime
        .block_on(
            fixture
                .boot
                .reads
                .events_page(&SessionId::new(SESSION_A).unwrap(), None, 50),
        )
        .expect("事件读取");
    assert!(
        remaining
            .iter()
            .all(|event| !matches!(event.payload, aether_core::EventPayload::Log(_))),
        "白名单超窗事件应被清理（对照）"
    );

    write_evidence(
        "dod5_retention",
        &json!({
            "audit_rows_preserved": ids.len(),
            "retention_result": report.result(),
        }),
    );
    fixture.shutdown();
}

// ===== DoD1（E2E）：真实 Mock 适配器 → 协议上报面 100% 审计 =====

struct E2eHarness {
    #[allow(dead_code)]
    dir: TempDir,
    #[allow(dead_code)]
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    reads: ReadPool,
    manager: SessionManager,
    backend: Arc<dyn IpcBackend>,
    supervisor: Arc<Supervisor>,
    executor: Arc<AdapterRunExecutor>,
}

fn harness(binary: &Path) -> E2eHarness {
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let CoreBoot {
        storage: slot,
        reads,
        write,
        pipeline,
        ..
    } = boot_core_full(
        dir.path(),
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("启动核心（存储 + 管线）");
    let spec = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Mock", binary.to_path_buf()).official(true),
    );
    let supervisor = Arc::new(
        boot_supervisor(vec![spec], Some(&dir.path().join("adapters.json"))).expect("构造监督器"),
    );
    let startup = run_supervisor_startup(&supervisor, &handle).expect("启动序列尾段");
    for (runtime_id, outcome) in &startup.warmups {
        assert!(
            outcome.is_ready(),
            "{runtime_id} 预热必须 Ready：{outcome:?}"
        );
    }
    let executor = Arc::new(AdapterRunExecutor::new(
        Arc::clone(&supervisor),
        pipeline.clone(),
        reads.clone(),
        write.clone(),
        handle.clone(),
        None,
    ));
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        executor.clone(),
    );
    let backend: Arc<dyn IpcBackend> = Arc::new(SessionBackend::new(
        Arc::new(NotImplementedBackend),
        Some(manager.clone()),
        Some(executor.clone()),
        Some(reads.clone()),
        Some(Arc::clone(&supervisor)),
        handle,
    ));
    E2eHarness {
        dir,
        runtime,
        slot,
        reads,
        manager,
        backend,
        supervisor,
        executor,
    }
}

#[test]
fn protocol_reported_tool_calls_are_fully_audited_end_to_end() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    let created = h
        .backend
        .session_create(&SessionCreateRequest {
            runtime_id: Some("mock".to_owned()),
            title: "M6-03 E2E".to_owned(),
            workspace_id: None,
            model: None,
            thinking_depth: None,
            permission_mode: None,
            preset_id: None,
            provider_id: None,
        })
        .expect("session_create");
    let session_id = created["id"].as_str().expect("会话 id").to_owned();
    let session = SessionId::new(session_id.clone()).unwrap();

    // 4 类场景（终态 success/failed 各 2）：正常完成 / 执行失败 / 预置许可 / 预置拒绝。
    let scenarios = [
        ("tool:normal", "succeeded"),
        ("tool:fail", "succeeded"),
        ("tool:permission-allow", "succeeded"),
        ("tool:permission-deny", "succeeded"),
    ];
    for (text, expected_status) in scenarios {
        let ack = h
            .backend
            .session_send(&SessionSendRequest {
                session_id: session_id.clone(),
                text: text.to_owned(),
                client_msg_id: ulid::Ulid::new().to_string(),
                thinking_depth: None,
                references: Vec::new(),
                provider_id: None,
                model: None,
            })
            .expect("session_send");
        let run_id = ack["run_id"].as_str().expect("run_id").to_owned();
        assert!(
            wait_until(Duration::from_secs(30), || {
                let run = h
                    .runtime
                    .block_on(
                        h.reads
                            .run(&aether_core::RunId::new(run_id.clone()).unwrap()),
                    )
                    .ok()
                    .flatten();
                run.map(|run| run.status)
                    == Some(match expected_status {
                        "succeeded" => RunStatus::Succeeded,
                        _ => RunStatus::Failed,
                    })
            }),
            "run({text}) 必须到达终态 {expected_status}"
        );
        // 同时等待（或确认）该 run 的终态工具事件审计行落库：以终态事件为锚逐场景推进。
    }

    // 完备性比对：`events` 表协议上报的终态工具事件 ↔ `tool.call` 审计行 1:1。
    assert!(
        wait_until(Duration::from_secs(30), || {
            let events = h
                .runtime
                .block_on(h.reads.events_page(&session, None, 1000))
                .unwrap_or_default();
            let terminal = terminal_tool_events(&events);
            let audits = h
                .runtime
                .block_on(h.reads.audit_query(AuditQuery {
                    session_id: Some(session_id.clone()),
                    action: Some(AUDIT_ACTION_TOOL_CALL.to_owned()),
                    limit: Some(500),
                    ..AuditQuery::default()
                }))
                .map(|result| result.rows.len())
                .unwrap_or(0);
            terminal.len() == 4 && audits == 4
        }),
        "终态工具事件与审计行必须在窗口内 1:1 到齐"
    );

    let events = h
        .runtime
        .block_on(h.reads.events_page(&session, None, 1000))
        .expect("事件读取");
    let terminal = terminal_tool_events(&events);
    let audit_result = h
        .runtime
        .block_on(h.reads.audit_query(AuditQuery {
            session_id: Some(session_id.clone()),
            action: Some(AUDIT_ACTION_TOOL_CALL.to_owned()),
            limit: Some(500),
            ..AuditQuery::default()
        }))
        .expect("审计查询");
    assert_eq!(terminal.len(), 4, "终态工具事件数：{terminal:?}");
    assert_eq!(
        audit_result.rows.len(),
        4,
        "tool.call 审计行数必须与协议上报一致"
    );

    let mut event_keys: Vec<(String, String)> = terminal
        .iter()
        .map(|(id, result)| (id.clone(), result.clone()))
        .collect();
    event_keys.sort();
    let mut audit_keys: Vec<(String, String)> = audit_result
        .rows
        .iter()
        .map(|row| {
            let detail: Value = row
                .detail
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok())
                .unwrap_or(Value::Null);
            (
                detail["tool_call_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                row.result.clone().unwrap_or_default(),
            )
        })
        .collect();
    audit_keys.sort();
    assert_eq!(
        audit_keys, event_keys,
        "协议上报面工具调用必须 100% 落审计（tool_call_id + result 1:1）"
    );
    for row in &audit_result.rows {
        assert_eq!(row.actor, "agent");
        assert_eq!(row.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(row.runtime_id.as_deref(), Some("mock"));
    }
    let successes = audit_result
        .rows
        .iter()
        .filter(|row| row.result.as_deref() == Some("success"))
        .count();
    assert_eq!(successes, 2, "success/failed 各 2（场景①/④ vs ②/⑤）");
    assert_eq!(h.executor.tool_audit().failures(), 0, "审计写入零失败");

    write_evidence(
        "dod1_completeness",
        &json!({
            "protocol_terminal_tool_events": terminal.len(),
            "audit_tool_call_rows": audit_result.rows.len(),
            "coverage": "100%",
            "successes": successes,
            "failures": h.executor.tool_audit().failures(),
        }),
    );

    // 会话生命周期/权限/运行器状态最小集回归（既有审计面不回归）。
    let lifecycle = h
        .runtime
        .block_on(h.reads.audit_query(AuditQuery {
            session_id: Some(session_id.clone()),
            ..AuditQuery::default()
        }))
        .expect("审计查询");
    assert!(
        lifecycle
            .rows
            .iter()
            .any(|row| row.action == "session.created"),
        "会话生命周期审计必须保留"
    );

    h.runtime.block_on(h.supervisor.shutdown_all());
    let _ = h.manager.clone();
    let _ = &h.slot;
    let _ = &h.dir;
}

/// 协议上报的终态工具事件（tool_call_id + result）。
fn terminal_tool_events(events: &[EventEnvelope]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCallCompleted(payload) => Some((
                payload.tool_call_id.as_str().to_owned(),
                "success".to_owned(),
            )),
            EventPayload::ToolCallFailed(payload) => Some((
                payload.tool_call_id.as_str().to_owned(),
                "failed".to_owned(),
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn audit_query_budget_truncates_cursor_resumes_and_tamper_is_rejected() {
    let fixture = AuditFixture::open();
    for index in 0..10 {
        fixture.insert_audit(audit_record(
            &format!("01J{index:023}"),
            Some(SESSION_A),
            "agent",
            "tool.call",
            Some("mock.echo"),
            Some(&format!("{{\"payload\":\"{}\"}}", "x".repeat(8 * 1024))),
            "success",
            1_000 + index,
        ));
    }
    let base = AuditQueryRequest {
        actor: Some("agent".to_owned()),
        max_bytes: Some(64 * 1024),
        limit: Some(500),
        ..AuditQueryRequest::default()
    };
    let first = fixture.query(&base);
    let first_rows = first["rows"].as_array().expect("rows");
    assert!(
        first_rows.len() < 10,
        "64KiB 预算必须截断 10×8KiB 审计行：{}",
        first_rows.len()
    );
    assert_eq!(first["has_more"], json!(true), "预算截断必须 has_more=true");
    let mut ids: Vec<String> = first_rows
        .iter()
        .map(|row| row["id"].as_str().expect("id").to_owned())
        .collect();
    let mut next = first["next_cursor"].as_str().map(str::to_owned);
    let mut pages = 1;
    while let Some(token) = next.take() {
        pages += 1;
        assert!(pages <= 10, "审计续读不得死循环（零推进）");
        let page = fixture.query(&AuditQueryRequest {
            actor: Some("agent".to_owned()),
            max_bytes: Some(64 * 1024),
            limit: Some(500),
            next_cursor: Some(token),
            ..AuditQueryRequest::default()
        });
        ids.extend(
            page["rows"]
                .as_array()
                .expect("rows")
                .iter()
                .map(|row| row["id"].as_str().expect("id").to_owned()),
        );
        next = page["next_cursor"].as_str().map(str::to_owned);
    }
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 10, "游标续读必须覆盖全部 10 行且无重复：{ids:?}");

    // 篡改过滤条件（actor）→ filter_hash 不匹配 → invalid_value。
    let token = aether_tauri::ipc::cursor::encode_cursor(
        aether_tauri::ipc::cursor::STREAM_AUDIT,
        0,
        "deadbeef",
    );
    let error = fixture
        .archive
        .audit_query(&AuditQueryRequest {
            actor: Some("agent".to_owned()),
            max_bytes: Some(64 * 1024),
            limit: Some(500),
            next_cursor: Some(token),
            ..AuditQueryRequest::default()
        })
        .expect_err("游标过滤条件不匹配必须拒绝");
    assert_eq!(error.code.as_str(), "invalid_value");
    assert_eq!(error.field.as_deref(), Some("next_cursor"));

    // 零推进：游标已指向末尾 → 空页 + 无 next_cursor。
    let fingerprint = base.filter_fingerprint();
    let end = aether_tauri::ipc::cursor::encode_cursor(
        aether_tauri::ipc::cursor::STREAM_AUDIT,
        10,
        &fingerprint,
    );
    let page = fixture.query(&AuditQueryRequest {
        actor: Some("agent".to_owned()),
        max_bytes: Some(64 * 1024),
        limit: Some(500),
        next_cursor: Some(end),
        ..AuditQueryRequest::default()
    });
    assert!(
        page["rows"].as_array().is_some_and(Vec::is_empty),
        "已到末尾必须空页：{page}"
    );
    assert!(page.get("next_cursor").is_none(), "空页不得返回游标");
    assert_eq!(page["has_more"], json!(false));
}
