//! M3-06 集成测试：`run_retry` IPC 接线与 Mode R/N 重放（设计 D5/D7、ADR-004/ADR-005）。
//!
//! 宿主：`aether-adapter-fixture --mode session`（最小 D6 会话面；`session.create`
//! 携带 `native_id` → `resumed=true`），调用记录写入 `--session-log` 供断言：
//! - **Mode N（新会话重发）**：无 `native_id` 时 `session.create` → `resumed=false`；
//! - **Mode R（原生恢复）**：核心重启后重放，执行器以 `sessions.config.native_id`
//!   恢复适配器会话 → `resumed=true` 且 `session_id` 不变；
//! - `run_retry`：仅终态可重试；复用输入消息（不新增用户消息行）；新 run 编号、
//!   旧 run 保留审计；`run_not_found`/`run_not_retryable` 结构化错误。
//!
//! 运行：`AETHER_FIXTURE_BIN=<aether-adapter-fixture> cargo test -p aether-tauri
//! --test m3_06_run_retry`（`scripts/test/m3-06/verify-m3-06.mjs` 构建并设置；
//! 未设置且未要求时显式跳过，`AETHER_REQUIRE_FIXTURE=1` 时缺路径直接失败）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, Supervisor};
use aether_control::{LifecycleConfig, SessionManager, SystemClock};
use aether_core::{RunId, RunStatus, SessionId};
use aether_store::ReadPool;
use aether_tauri::adapter_executor::AdapterRunExecutor;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{RunRetryRequest, SessionCreateRequest, SessionSendRequest};
use aether_tauri::runtime_control::{boot_supervisor, run_supervisor_startup};
use aether_tauri::session_backend::SessionBackend;
use serde_json::Value;
use tempfile::TempDir;

fn fixture_binary() -> Option<PathBuf> {
    match std::env::var_os("AETHER_FIXTURE_BIN") {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            if std::env::var("AETHER_REQUIRE_FIXTURE").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_FIXTURE=1 但 AETHER_FIXTURE_BIN 未设置");
            }
            eprintln!("SKIP：AETHER_FIXTURE_BIN 未设置（运行 pnpm verify:m3-06 构建夹具后执行）");
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

/// 核心会话链路（可关闭后在既有数据目录上重开，模拟核心重启）。
struct CoreHarness {
    boot: CoreBoot,
    #[allow(dead_code)]
    manager: SessionManager,
    #[allow(dead_code)]
    executor: Arc<AdapterRunExecutor>,
    backend: Arc<dyn IpcBackend>,
}

impl CoreHarness {
    fn open(
        runtime: &tokio::runtime::Runtime,
        dir: &Path,
        supervisor: Arc<Supervisor>,
        handle: &tokio::runtime::Handle,
    ) -> Self {
        let boot = boot_core_full(dir, handle, Arc::new(StaticRuntimeSummaries::unwired()))
            .expect("启动核心（存储 + 管线）");
        let executor = Arc::new(AdapterRunExecutor::new(
            Arc::clone(&supervisor),
            boot.pipeline.clone(),
            boot.reads.clone(),
            boot.write.clone(),
            handle.clone(),
            None,
        ));
        let manager = SessionManager::new(
            LifecycleConfig::default(),
            Arc::new(SystemClock),
            boot.write.clone(),
            boot.reads.clone(),
            boot.pipeline.clone(),
            executor.clone(),
        );
        runtime
            .block_on(manager.reconcile_interrupted_runs())
            .expect("重启状态重建");
        let backend: Arc<dyn IpcBackend> = Arc::new(SessionBackend::new(
            Arc::new(NotImplementedBackend),
            Some(manager.clone()),
            Some(executor.clone()),
            Some(boot.reads.clone()),
            Some(Arc::clone(&supervisor)),
            handle.clone(),
        ));
        Self {
            boot,
            manager,
            executor,
            backend,
        }
    }

    fn reads(&self) -> &ReadPool {
        &self.boot.reads
    }

    async fn shutdown(self) {
        let _ = self.boot.pipeline.shutdown().await;
        if let Some(runtime) = self.boot.storage.take() {
            let _ = runtime.shutdown().await;
        }
    }
}

fn session_log_lines(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
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

fn wait_run_status(
    runtime: &tokio::runtime::Runtime,
    reads: &ReadPool,
    run_id: &RunId,
    expected: RunStatus,
) -> bool {
    wait_until(Duration::from_secs(30), || {
        runtime
            .block_on(reads.run(run_id))
            .ok()
            .flatten()
            .map(|run| run.status)
            == Some(expected)
    })
}

#[test]
fn run_retry_uses_terminal_guard_and_replays_per_mode_r_and_n() {
    let Some(fixture) = fixture_binary() else {
        return;
    };
    let dir = TempDir::new().expect("临时目录");
    let log_path = dir.path().join("session-log.jsonl");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();

    // 官方白名单 id（fixture 二进制仅作会话宿主；manifest id 取白名单内取值）。
    let spec = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Fixture Session", fixture)
            .official(true)
            .with_args([
                "--mode".to_owned(),
                "session".to_owned(),
                "--session-log".to_owned(),
                log_path.to_string_lossy().to_string(),
            ]),
    );
    let supervisor = Arc::new(
        boot_supervisor(vec![spec], Some(&dir.path().join("adapters.json"))).expect("监督器"),
    );
    let startup = run_supervisor_startup(&supervisor, &handle).expect("启动序列尾段");
    for (runtime_id, outcome) in &startup.warmups {
        assert!(
            outcome.is_ready(),
            "{runtime_id} 预热必须 Ready：{outcome:?}"
        );
    }

    // ===== 1. Mode N（首次创建）：无 native_id → resumed=false =====
    let core = CoreHarness::open(&runtime, dir.path(), Arc::clone(&supervisor), &handle);
    let created = core
        .backend
        .session_create(&SessionCreateRequest {
            runtime_id: "mock".to_owned(),
            title: "M3-06 重放".to_owned(),
            workspace_id: None,
            model: None,
            thinking_depth: None,
        })
        .expect("session_create");
    let session_id = SessionId::new(created["id"].as_str().expect("会话 id").to_owned()).unwrap();

    let first = core
        .backend
        .session_send(&SessionSendRequest {
            session_id: session_id.as_str().to_owned(),
            text: "hello".to_owned(),
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K01".to_owned(),
            thinking_depth: None,
        })
        .expect("session_send");
    let first_run = RunId::new(first["run_id"].as_str().expect("run_id").to_owned()).unwrap();
    assert!(
        wait_run_status(&runtime, core.reads(), &first_run, RunStatus::Succeeded),
        "首 run 应成功"
    );
    let native_id = runtime
        .block_on(core.reads().session(&session_id))
        .expect("读会话")
        .and_then(|session| {
            session
                .config
                .get("native_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .expect("Mode R 恢复键必须写回 sessions.config.native_id");

    // ===== 2. 失败 run → run_retry（同核心生命周期内复用适配器会话） =====
    let failing = core
        .backend
        .session_send(&SessionSendRequest {
            session_id: session_id.as_str().to_owned(),
            text: "fail-once".to_owned(),
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K02".to_owned(),
            thinking_depth: None,
        })
        .expect("session_send(fail)");
    let failing_run = RunId::new(failing["run_id"].as_str().unwrap().to_owned()).unwrap();
    assert!(
        wait_run_status(&runtime, core.reads(), &failing_run, RunStatus::Failed),
        "注入失败 run 应为 failed"
    );

    // 未知 run → run_not_found（参数格式已过 DTO，终态/存在性由后端判定）。
    let missing = RunRetryRequest {
        run_id: "01J00000000000000000000MISS".to_owned(),
    };
    let error = core.backend.run_retry(&missing).unwrap_err();
    assert!(error.message.contains("run_not_found"), "{}", error.message);

    let retry = core
        .backend
        .run_retry(&RunRetryRequest {
            run_id: failing_run.as_str().to_owned(),
        })
        .expect("run_retry");
    assert_eq!(retry["session_id"].as_str(), Some(session_id.as_str()));
    assert_eq!(
        retry["input_message_id"].as_str(),
        failing["message_id"].as_str(),
        "重放复用原输入消息"
    );
    assert_eq!(retry["queued"], false);
    let retry_run = RunId::new(retry["run_id"].as_str().unwrap().to_owned()).unwrap();
    assert_ne!(retry_run, failing_run, "重放必须产生新 run");
    assert!(
        wait_run_status(&runtime, core.reads(), &retry_run, RunStatus::Succeeded),
        "重放 run 应成功"
    );
    // 旧 run 保留审计。
    let old = runtime
        .block_on(core.reads().run(&failing_run))
        .unwrap()
        .expect("旧 run 行");
    assert_eq!(old.status, RunStatus::Failed);
    assert_eq!(old.error.as_deref(), Some("fixture_fail"));

    // 适配器调用记录：仅 1 次 create（无新会话）；send 次数 = 首 run + 失败 run + 重放。
    let lines = session_log_lines(&log_path);
    let creates: Vec<&Value> = lines
        .iter()
        .filter(|line| line["method"] == "session.create")
        .collect();
    assert_eq!(creates.len(), 1, "同核心生命周期内重放不得新建适配器会话");
    assert_eq!(creates[0]["resumed"], false, "首次创建为 Mode N 语义");
    assert_eq!(creates[0]["session_id"].as_str(), Some(native_id.as_str()));
    let sends = lines
        .iter()
        .filter(|line| line["method"] == "session.send")
        .count();
    assert_eq!(sends, 3, "首 run + 失败 run + 重放各一次 send");

    // ===== 3. Mode R：核心重启后重放，以 native_id 恢复适配器会话 =====
    runtime.block_on(core.shutdown());
    let core2 = CoreHarness::open(&runtime, dir.path(), Arc::clone(&supervisor), &handle);
    let resumed = core2
        .backend
        .session_send(&SessionSendRequest {
            session_id: session_id.as_str().to_owned(),
            text: "again".to_owned(),
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K03".to_owned(),
            thinking_depth: None,
        })
        .expect("session_send(again)");
    let resumed_run = RunId::new(resumed["run_id"].as_str().unwrap().to_owned()).unwrap();
    assert!(
        wait_run_status(&runtime, core2.reads(), &resumed_run, RunStatus::Succeeded),
        "核心重启后的 run 应成功"
    );
    let lines = session_log_lines(&log_path);
    let resumed_creates: Vec<&Value> = lines
        .iter()
        .filter(|line| line["method"] == "session.create" && line["resumed"] == true)
        .collect();
    assert_eq!(
        resumed_creates.len(),
        1,
        "核心重启后必须经 native_id 恢复（Mode R）：{lines:?}"
    );
    assert_eq!(
        resumed_creates[0]["session_id"].as_str(),
        Some(native_id.as_str()),
        "Mode R 恢复的会话 id 必须等于 native_id"
    );

    // 核心重启后对旧失败 run 重放同样走 Mode R（恢复会话 + 重放输入）。
    let retry2 = core2
        .backend
        .run_retry(&RunRetryRequest {
            run_id: failing_run.as_str().to_owned(),
        })
        .expect("run_retry(core2)");
    let retry2_run = RunId::new(retry2["run_id"].as_str().unwrap().to_owned()).unwrap();
    assert!(
        wait_run_status(&runtime, core2.reads(), &retry2_run, RunStatus::Succeeded),
        "核心重启后的重放应成功"
    );

    // ===== 4. 运行中 run 拒绝重试（run_not_retryable） =====
    let long = core2
        .backend
        .session_send(&SessionSendRequest {
            session_id: session_id.as_str().to_owned(),
            text: "long".to_owned(),
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K04".to_owned(),
            thinking_depth: None,
        })
        .expect("session_send(long)");
    let long_run = RunId::new(long["run_id"].as_str().unwrap().to_owned()).unwrap();
    assert!(
        wait_run_status(&runtime, core2.reads(), &long_run, RunStatus::Running),
        "长 run 应在执行中"
    );
    let error = core2
        .backend
        .run_retry(&RunRetryRequest {
            run_id: long_run.as_str().to_owned(),
        })
        .unwrap_err();
    assert!(
        error.message.contains("run_not_retryable"),
        "{}",
        error.message
    );

    runtime.block_on(core2.shutdown());
    runtime.block_on(supervisor.shutdown_all());
}
