//! M3-10 集成测试：思考深度（会话级参数；ADR-010 决策 2/4）。
//!
//! 覆盖（实施计划 v1.17 §4 M3-10 DoD2/3/4 的集成面；真实 Mock 适配器进程）：
//! - DoD2 透传：`session_create`/`session_send` 带 `thinking_depth` → 适配器经
//!   `session.create`/`session.send` 收到（Mock session-log 回显断言）；缺省 = 2；
//!   `session_send` 覆盖仅本次 run（会话级值不被回写）；`runs.thinking_depth` 落
//!   生效值；`sessions.thinking_depth` 恢复/回显一致；
//! - DoD3 能力门：同步判定路径（runtime ready 且未声明 `thinking_depth`）→ 会话落
//!   缺省 2 + 响应 `warnings[0].code="thinking_depth_unsupported"`（非阻断，run 正常
//!   终态）；延迟判定路径（runtime 未 ready 时接受请求）→ 无响应警告、run 启动后
//!   以 `SessionSummary.thinking_depth` 回显生效值 2、字段不透传；
//! - DoD4 重放：`run_retry` 按会话级值恢复（忽略原覆盖），run 启动时重新执行能力门
//!   判定；
//! - DoD1/DoD5 的存储与校验矩阵分别由 `aether-store --test m3_10_thinking` 与
//!   `ipc_validation` 覆盖。
//!
//! 运行：`AETHER_MOCK_ADAPTER=<Bun 编译产物> cargo test -p aether-tauri --test m3_10_thinking`
//! （由 `scripts/test/m3-10/verify-m3-10.mjs` 构建并设置；未设置且未要求时显式跳过，
//! `AETHER_REQUIRE_MOCK_ADAPTER=1` 时缺路径直接失败）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, Supervisor};
use aether_control::{EventPipeline, LifecycleConfig, SessionManager, SystemClock};
use aether_core::{RunStatus, RuntimeStatus, Session, SessionId, THINKING_DEPTH_DEFAULT};
use aether_store::ReadPool;
use aether_tauri::adapter_executor::AdapterRunExecutor;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    RunRetryRequest, SessionCreateRequest, SessionIdRequest, SessionListRequest, SessionSendRequest,
};
use aether_tauri::runtime_control::boot_supervisor;
use aether_tauri::session_backend::SessionBackend;
use serde_json::{json, Value};
use tempfile::TempDir;

fn mock_binary() -> Option<PathBuf> {
    match std::env::var_os("AETHER_MOCK_ADAPTER") {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            if std::env::var("AETHER_REQUIRE_MOCK_ADAPTER").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_MOCK_ADAPTER=1 但 AETHER_MOCK_ADAPTER 未设置");
            }
            eprintln!(
                "SKIP：AETHER_MOCK_ADAPTER 未设置（运行 pnpm verify:m3-10 构建 Mock 后执行）"
            );
            None
        }
    }
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

fn evidence(name: &str, value: &Value) {
    println!("[m3-10] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M3_10_EVIDENCE_DIR") else {
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

/// 支持 `thinking_depth` 的运行时 id。
const RUNTIME_SUPPORTED: &str = "mock";
/// 未声明 `thinking_depth` 的运行时 id（`--no-thinking-depth`）。
const RUNTIME_UNSUPPORTED: &str = "mock-nodepth";

struct Harness {
    #[allow(dead_code)]
    dir: TempDir,
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    pipeline: EventPipeline,
    reads: ReadPool,
    backend: Arc<SessionBackend>,
    supervisor: Arc<Supervisor>,
    session_log: PathBuf,
}

fn harness(binary: &Path) -> Harness {
    let dir = TempDir::new().expect("临时数据目录");
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
    let session_log = dir.path().join("mock-session-log.jsonl");
    let supported_args = vec![
        "--stream-deltas".to_owned(),
        "2".to_owned(),
        "--stream-interval-ms".to_owned(),
        "1".to_owned(),
        "--session-log".to_owned(),
        session_log.to_string_lossy().to_string(),
    ];
    let mut unsupported_args = supported_args.clone();
    unsupported_args.push("--no-thinking-depth".to_owned());
    let supported = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new(RUNTIME_SUPPORTED, "Mock", binary.to_path_buf())
            .official(true)
            .with_args(supported_args),
    );
    let unsupported = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new(RUNTIME_UNSUPPORTED, "MockNoDepth", binary.to_path_buf())
            .official(true)
            .with_args(unsupported_args),
    );
    let supervisor = Arc::new(
        boot_supervisor(vec![supported, unsupported], Some(&dir.path().join("adapters.json")))
            .expect("构造监督器"),
    );
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
    let backend = Arc::new(SessionBackend::new(
        Arc::new(NotImplementedBackend),
        Some(manager),
        Some(executor),
        Some(reads.clone()),
        Some(Arc::clone(&supervisor)),
        handle,
    ));
    Harness {
        dir,
        runtime,
        slot,
        pipeline,
        reads,
        backend,
        supervisor,
        session_log,
    }
}

impl Harness {
    /// 按需启动运行时（未预热运行时用于延迟能力门用例；ADR-010 判定时机）。
    fn start_runtime(&self, runtime_id: &str) -> bool {
        let Some(runtime) = self.supervisor.get(runtime_id) else {
            return false;
        };
        if self.runtime.block_on(runtime.status()) == RuntimeStatus::Ready {
            return true;
        }
        let outcome = self.runtime.block_on(runtime.start());
        if !outcome.is_ready() {
            return false;
        }
        wait_until(Duration::from_secs(30), || {
            self.runtime.block_on(runtime.status()) == RuntimeStatus::Ready
        })
    }

    fn create(
        &self,
        runtime_id: &str,
        thinking_depth: Option<i64>,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.session_create(&SessionCreateRequest {
            runtime_id: runtime_id.to_owned(),
            title: "M3-10 思考深度".to_owned(),
            workspace_id: None,
            model: None,
            thinking_depth,
        })
    }

    fn send(
        &self,
        session_id: &str,
        text: &str,
        client_msg_id: &str,
        thinking_depth: Option<i64>,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.session_send(&SessionSendRequest {
            session_id: session_id.to_owned(),
            text: text.to_owned(),
            client_msg_id: client_msg_id.to_owned(),
            thinking_depth,
        })
    }

    fn retry(&self, run_id: &str) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.run_retry(&RunRetryRequest {
            run_id: run_id.to_owned(),
        })
    }

    fn session_row(&self, session_id: &str) -> Session {
        let id = SessionId::new(session_id).expect("会话 id");
        self.runtime
            .block_on(self.reads.session(&id))
            .expect("会话读取")
            .expect("会话行存在")
    }

    fn run_row(&self, run_id: &str) -> aether_core::Run {
        let id = aether_core::RunId::new(run_id).expect("run id");
        self.runtime
            .block_on(self.reads.run(&id))
            .expect("run 读取")
            .expect("run 行存在")
    }

    fn session_list(&self, session_id: &str) -> Value {
        let request: SessionListRequest =
            serde_json::from_value(json!({})).expect("SessionListRequest 缺省成员合法");
        let list = self.backend.session_list(&request).expect("session_list");
        list.as_array()
            .expect("列表数组")
            .iter()
            .find(|item| item["id"].as_str() == Some(session_id))
            .cloned()
            .expect("会话出现在列表")
    }

    fn wait_run_terminal(&self, run_id: &str) -> RunStatus {
        assert!(
            wait_until(Duration::from_secs(30), || {
                matches!(
                    self.run_row(run_id).status,
                    RunStatus::Succeeded
                        | RunStatus::Failed
                        | RunStatus::Cancelled
                        | RunStatus::Timeout
                )
            }),
            "run {run_id} 必须到达终态"
        );
        self.run_row(run_id).status
    }

    /// 取消在途 run（`long` 流永不完成；中断 → cancelled，供重放用例）。
    fn cancel_run(&self, session_id: &str, run_id: &str) {
        let report = self
            .backend
            .session_interrupt(&SessionIdRequest {
                session_id: session_id.to_owned(),
            })
            .expect("session_interrupt");
        assert_eq!(
            report["interrupted_run"].as_str(),
            Some(run_id),
            "中断必须命中在途 run"
        );
        assert_eq!(self.wait_run_terminal(run_id), RunStatus::Cancelled);
    }

    fn session_log_records(&self) -> Vec<Value> {
        match std::fs::read_to_string(&self.session_log) {
            Ok(text) => text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn create_records(&self) -> Vec<Value> {
        self.session_log_records()
            .into_iter()
            .filter(|record| record["method"].as_str() == Some("session.create"))
            .collect()
    }

    fn send_records(&self) -> Vec<Value> {
        self.session_log_records()
            .into_iter()
            .filter(|record| record["method"].as_str() == Some("session.send"))
            .collect()
    }

    fn shutdown(self) {
        self.runtime.block_on(self.supervisor.shutdown_all());
        self.runtime.block_on(self.pipeline.shutdown()).ok();
        if let Some(storage) = self.slot.take() {
            self.runtime.block_on(storage.shutdown()).ok();
        }
        let _ = &self.dir;
    }
}

// ===== DoD2：透传（会话级 + run 覆盖 + 缺省）=====

#[test]
fn dod2_passthrough_default_override_and_echo() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    assert!(h.start_runtime(RUNTIME_SUPPORTED), "支持运行时必须 Ready");

    // 会话级 4：响应回显 + 落库 + 适配器 session.create 收到 4（无警告）。
    let created = h.create(RUNTIME_SUPPORTED, Some(4)).expect("session_create");
    assert_eq!(created["thinking_depth"], 4);
    assert!(created.get("warnings").is_none(), "支持运行时不得有警告");
    let session_id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(h.session_row(&session_id).thinking_depth, 4);

    // 本次 run 覆盖 1：适配器 session.send 收到 1，runs 落 1，会话级仍为 4。
    let ack = h
        .send(&session_id, "hello", "01J8ZQ5R0N7W9Y8X6V4T2S0K11", Some(1))
        .expect("session_send");
    let override_run = ack["run_id"].as_str().unwrap().to_owned();
    assert_eq!(h.wait_run_terminal(&override_run), RunStatus::Succeeded);
    assert_eq!(h.run_row(&override_run).thinking_depth, Some(1));
    assert_eq!(
        h.session_row(&session_id).thinking_depth,
        4,
        "run 覆盖不得回写会话级值"
    );

    // 缺省（无覆盖）：run 落会话级 4；适配器 session.send 不带字段（null）。
    let ack = h
        .send(&session_id, "again", "01J8ZQ5R0N7W9Y8X6V4T2S0K12", None)
        .expect("session_send(缺省)");
    let session_run = ack["run_id"].as_str().unwrap().to_owned();
    assert_eq!(h.wait_run_terminal(&session_run), RunStatus::Succeeded);
    assert_eq!(h.run_row(&session_run).thinking_depth, Some(4));

    // 新建缺省会话：响应 2；adapter create 收到 2；run 落 2。
    let default_created = h.create(RUNTIME_SUPPORTED, None).expect("session_create(缺省)");
    assert_eq!(default_created["thinking_depth"], 2);
    let default_session = default_created["id"].as_str().unwrap().to_owned();
    let ack = h
        .send(&default_session, "plain", "01J8ZQ5R0N7W9Y8X6V4T2S0K13", None)
        .expect("session_send(plain)");
    let plain_run = ack["run_id"].as_str().unwrap().to_owned();
    assert_eq!(h.wait_run_terminal(&plain_run), RunStatus::Succeeded);
    assert_eq!(h.run_row(&plain_run).thinking_depth, Some(2));
    assert_eq!(
        h.session_list(&default_session)["thinking_depth"],
        THINKING_DEPTH_DEFAULT,
        "session_list 回显会话级生效值"
    );

    // Mock 回显：create 记录 [4, 2]；send 记录 [1, null, null]。
    let create_depths: Vec<Value> = h
        .create_records()
        .iter()
        .map(|record| record["thinking_depth"].clone())
        .collect();
    assert_eq!(
        create_depths,
        vec![json!(4), json!(2)],
        "session.create 必须收到透传值（缺省 2）"
    );
    let send_depths: Vec<Value> = h
        .send_records()
        .iter()
        .map(|record| record["thinking_depth"].clone())
        .collect();
    assert_eq!(
        send_depths,
        vec![json!(1), json!(null), json!(null)],
        "session.send 仅覆盖时携带字段"
    );

    evidence(
        "dod2_passthrough",
        &json!({
            "task": "M3-10 DoD2 透传：会话级/覆盖/缺省 → 适配器回显 + 落库",
            "session_thinking_depth": 4,
            "override_run_depth": 1,
            "session_level_after_override": h.session_row(&session_id).thinking_depth,
            "default_run_depth": 2,
            "adapter_create_depths": create_depths,
            "adapter_send_depths": send_depths,
        }),
    );
    h.shutdown();
}

// ===== DoD3：同步能力门（runtime ready 且未声明能力）=====

#[test]
fn dod3_sync_gate_warning_and_defaults() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    assert!(
        h.start_runtime(RUNTIME_UNSUPPORTED),
        "未支持运行时需 ready（同步判定路径）"
    );

    let created = h.create(RUNTIME_UNSUPPORTED, Some(3)).expect("session_create");
    // 同步判定路径：响应警告 + 生效值 2 + 请求值不透传。
    assert_eq!(created["thinking_depth"], 2, "未支持时必须落缺省 2");
    let warning = &created["warnings"][0];
    assert_eq!(warning["code"], "thinking_depth_unsupported");
    assert_eq!(warning["field"], "thinking_depth");
    assert_eq!(warning["runtime_id"], RUNTIME_UNSUPPORTED);
    let session_id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(h.session_row(&session_id).thinking_depth, 2);

    // 非阻断：run 正常终态；覆盖请求同样触发同步警告，run 落 2、会话值不变。
    let ack = h
        .send(&session_id, "hello", "01J8ZQ5R0N7W9Y8X6V4T2S0K21", Some(4))
        .expect("session_send");
    assert_eq!(
        ack["warnings"][0]["code"], "thinking_depth_unsupported",
        "同步判定路径响应必须携带警告"
    );
    let run_id = ack["run_id"].as_str().unwrap().to_owned();
    assert_eq!(h.wait_run_terminal(&run_id), RunStatus::Succeeded);
    assert_eq!(h.run_row(&run_id).thinking_depth, Some(2));
    assert_eq!(h.session_row(&session_id).thinking_depth, 2);

    // 适配器：create/send 均不携带 thinking_depth（字段不透传）。
    let create_records = h.create_records();
    assert_eq!(create_records.len(), 1);
    assert_eq!(create_records[0]["has_thinking_depth"], false);
    assert_eq!(create_records[0]["thinking_depth"], json!(null));
    let send_records = h.send_records();
    assert_eq!(send_records.len(), 1);
    assert_eq!(send_records[0]["has_thinking_depth"], false);

    evidence(
        "dod3_sync_gate",
        &json!({
            "task": "M3-10 DoD3 同步能力门：warnings + 落缺省 2 + 字段不透传 + run 正常终态",
            "warning": warning,
            "run_thinking_depth": 2,
            "session_thinking_depth": 2,
            "adapter_create_has_thinking_depth": create_records[0]["has_thinking_depth"],
        }),
    );
    h.shutdown();
}

// ===== DoD3：延迟判定路径（runtime 未 ready 时接受请求）=====

#[test]
fn dod3_delayed_gate_after_runtime_ready() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    // 不启动 `mock-nodepth`：`session_create` 在 cold 时接受请求（延迟判定）。
    assert_eq!(
        h.runtime
            .block_on(h.supervisor.get(RUNTIME_UNSUPPORTED).unwrap().status()),
        RuntimeStatus::Cold
    );

    let created = h.create(RUNTIME_UNSUPPORTED, Some(3)).expect("cold 时必须接受请求");
    assert!(
        created.get("warnings").is_none(),
        "延迟判定路径不得产生响应警告（ADR-010 v0.4）"
    );
    assert_eq!(created["thinking_depth"], 3, "请求值随会话落库保留");
    let session_id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(h.session_row(&session_id).thinking_depth, 3);

    // 首次 run：执行器启动运行时并判定能力 → 会话改写 2、run 落 2、字段不透传。
    let ack = h
        .send(&session_id, "hello", "01J8ZQ5R0N7W9Y8X6V4T2S0K31", None)
        .expect("session_send");
    let run_id = ack["run_id"].as_str().unwrap().to_owned();
    assert_eq!(h.wait_run_terminal(&run_id), RunStatus::Succeeded);
    assert_eq!(
        h.run_row(&run_id).thinking_depth,
        Some(THINKING_DEPTH_DEFAULT),
        "能力门生效值落 run 行"
    );
    assert_eq!(
        h.session_row(&session_id).thinking_depth,
        THINKING_DEPTH_DEFAULT,
        "延迟判定路径改写 sessions.thinking_depth=2（ADR-010）"
    );
    assert_eq!(
        h.session_list(&session_id)["thinking_depth"],
        THINKING_DEPTH_DEFAULT,
        "以 SessionSummary.thinking_depth 回显生效值"
    );
    let create_records = h.create_records();
    assert_eq!(create_records.len(), 1);
    assert_eq!(create_records[0]["has_thinking_depth"], false);

    evidence(
        "dod3_delayed_gate",
        &json!({
            "task": "M3-10 DoD3 延迟能力门：cold 接受请求 → run 启动判定 → 会话/run 落 2 + 字段不透传",
            "session_before": 3,
            "session_after": h.session_row(&session_id).thinking_depth,
            "run_effective": h.run_row(&run_id).thinking_depth,
            "session_summary_echo": h.session_list(&session_id)["thinking_depth"],
        }),
    );
    h.shutdown();
}

// ===== DoD4：重放按会话级值恢复 + run 启动重新判定 =====

#[test]
fn dod4_retry_uses_session_level_and_regates() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    assert!(h.start_runtime(RUNTIME_SUPPORTED), "支持运行时必须 Ready");

    // 会话级 3；原 run 覆盖 1（终态 cancelled）→ 重放须按会话级 3 恢复（忽略覆盖）。
    let created = h.create(RUNTIME_SUPPORTED, Some(3)).expect("session_create");
    let session_id = created["id"].as_str().unwrap().to_owned();
    let ack = h
        .send(&session_id, "long", "01J8ZQ5R0N7W9Y8X6V4T2S0K41", Some(1))
        .expect("session_send");
    let cancelled_run = ack["run_id"].as_str().unwrap().to_owned();
    h.cancel_run(&session_id, &cancelled_run);
    assert_eq!(h.run_row(&cancelled_run).thinking_depth, Some(1));

    let retried = h.retry(&cancelled_run).expect("run_retry");
    let retry_run = retried["run_id"].as_str().unwrap().to_owned();
    assert_ne!(retry_run, cancelled_run, "重放必须产生新 run");
    assert_eq!(
        h.run_row(&retry_run).thinking_depth,
        Some(3),
        "重放按会话级值恢复（Mode R/N，ADR-010）"
    );
    // 旧 run 保留审计（不再被改写）。
    assert_eq!(h.run_row(&cancelled_run).thinking_depth, Some(1));

    // 重放 run 的适配器请求不带覆盖字段（会话级默认由 session.create 承载）。
    assert!(
        wait_until(Duration::from_secs(20), || {
            h.send_records()
                .iter()
                .any(|record| record["client_msg_id"].as_str() == Some(retry_run.as_str()))
        }),
        "重放 run 必须到达适配器 session.send"
    );
    let retry_send = h
        .send_records()
        .into_iter()
        .find(|record| record["client_msg_id"].as_str() == Some(retry_run.as_str()))
        .expect("重放 send 记录");
    assert_eq!(retry_send["has_thinking_depth"], false);

    // 未支持运行时：会话级 3（延迟）→ 首次 run 判定为 2 → 取消 → 重放再判定为 2。
    let unsupported = h
        .create(RUNTIME_UNSUPPORTED, Some(3))
        .expect("session_create(cold)");
    let usize_session = unsupported["id"].as_str().unwrap().to_owned();
    let ack = h
        .send(&usize_session, "long", "01J8ZQ5R0N7W9Y8X6V4T2S0K42", None)
        .expect("session_send(long)");
    let long_run = ack["run_id"].as_str().unwrap().to_owned();
    // 等待执行器完成能力门（会话改写 2）再取消。
    assert!(wait_until(Duration::from_secs(20), || {
        h.session_row(&usize_session).thinking_depth == THINKING_DEPTH_DEFAULT
    }));
    h.cancel_run(&usize_session, &long_run);
    let retried = h.retry(&long_run).expect("run_retry(unsupported)");
    let retry_run = retried["run_id"].as_str().unwrap().to_owned();
    assert!(
        wait_until(Duration::from_secs(20), || {
            h.send_records()
                .iter()
                .any(|record| record["client_msg_id"].as_str() == Some(retry_run.as_str()))
        }),
        "未支持运行时的重放 run 必须到达适配器"
    );
    assert_eq!(
        h.run_row(&retry_run).thinking_depth,
        Some(THINKING_DEPTH_DEFAULT),
        "重放 run 启动时重新执行能力门判定（未支持 → 2）"
    );
    let unsupported_retry_send = h
        .send_records()
        .into_iter()
        .find(|record| record["client_msg_id"].as_str() == Some(retry_run.as_str()))
        .expect("未支持重放 send 记录");
    assert_eq!(
        unsupported_retry_send["has_thinking_depth"], false,
        "能力未声明时重放同样不透传"
    );
    h.cancel_run(&usize_session, &retry_run);

    evidence(
        "dod4_replay",
        &json!({
            "task": "M3-10 DoD4 重放按会话级值恢复 + run 启动重新判定",
            "cancelled_run_depth": 1,
            "retry_run_depth": 3,
            "session_level": 3,
            "unsupported_retry_run_depth": h.run_row(&retry_run).thinking_depth,
            "unsupported_session_depth": h.session_row(&usize_session).thinking_depth,
        }),
    );
    h.shutdown();
}
