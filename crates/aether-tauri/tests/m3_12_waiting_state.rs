//! M3-12 生产组合路径集成 E2E：会话等待态写入（ADR-011 / D9）。
//!
//! 组合面（与生产组合根一致，M3-08 关闭后的口径）：
//! 工作区基准（`workspace_set` 换根） + 执行器权限网关（`PermissionServiceGate`）
//! + 真实 `PermissionService`（票据/超时/审计） + `SessionManager`（等待态唯一写者）
//! + Mock 适配器进程（`permission-loop:` 回环触发真实工作区 ask）。
//!
//! 断言（DoD6）：
//! - 真实工作区 `fs.write` ask → 会话 `running → waiting_permission`；
//! - 决议（allow once / deny）→ 合法回程 `waiting_permission → running`；
//! - `session.status_changed` 的 `from/to` 精确匹配；事件序列全部命中 19 边白名单；
//! - 回环零直通（执行器探针计数，复用 M2-10/M3-08 口径）。
//!
//! 验证层级 = 集成（真实适配器进程 + 核心组合）；真实 WebView 内联回环 E2E 归 M4-05。
//!
//! 运行：`AETHER_MOCK_ADAPTER=<Bun 编译产物> cargo test -p aether-tauri --test
//! m3_12_waiting_state`（由 `scripts/test/m3-12/verify-m3-12.mjs` 构建并设置；
//! 未设置且未要求时显式跳过，`AETHER_REQUIRE_MOCK_ADAPTER=1` 时缺路径直接失败）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, Supervisor};
use aether_control::{
    EventPipeline, LifecycleConfig, PermissionConfig, PermissionService, SessionManager,
    SystemClock,
};
use aether_core::{session_transition_allowed, SessionId, SessionStatus};
use aether_security::PolicyEngine;
use aether_store::{ReadPool, WriteQueue};
use aether_tauri::adapter_executor::AdapterRunExecutor;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    PermissionDecision as DtoPermissionDecision, PermissionResolveRequest, SessionCreateRequest,
    SessionSendRequest, WorkspaceSetRequest,
};
use aether_tauri::permission_loop::PermissionServiceGate;
use aether_tauri::runtime_control::{boot_supervisor, run_supervisor_startup};
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
                "SKIP：AETHER_MOCK_ADAPTER 未设置（运行 pnpm verify:m3-12 构建 Mock 后执行）"
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
    println!("[m3-12] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M3_12_EVIDENCE_DIR") else {
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

struct Harness {
    #[allow(dead_code)]
    dir: TempDir,
    workspace: TempDir,
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    pipeline: EventPipeline,
    reads: ReadPool,
    #[allow(dead_code)]
    write: WriteQueue,
    service: PermissionService,
    manager: SessionManager,
    executor: Arc<AdapterRunExecutor>,
    backend: Arc<SessionBackend>,
    supervisor: Arc<Supervisor>,
}

fn harness(binary: &Path) -> Harness {
    let dir = TempDir::new().expect("临时数据目录");
    let workspace = TempDir::new().expect("临时工作区");
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
    // D9 初始基准 = 数据目录（M3-03 口径）；workspace_set 后换根到工作区。
    let service = PermissionService::new(
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
        Arc::new(SystemClock),
        PolicyEngine::new(dir.path()).expect("策略引擎（数据目录基准）"),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
    );
    let spec = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Mock", binary.to_path_buf())
            .official(true)
            .with_args(["--stream-deltas", "4", "--stream-interval-ms", "1"]),
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
    let gate = PermissionServiceGate::new(service.clone());
    let executor = Arc::new(AdapterRunExecutor::new(
        Arc::clone(&supervisor),
        pipeline.clone(),
        reads.clone(),
        write.clone(),
        handle.clone(),
        Some(gate),
    ));
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        executor.clone(),
    );
    // M3-12 两阶段装配（与组合根 `lib.rs` 同口径）：权限服务 → 观察者 → 管理器。
    service.set_pending_observer(Arc::new(manager.clone()));
    let backend = Arc::new(
        SessionBackend::new(
            Arc::new(NotImplementedBackend),
            Some(manager.clone()),
            Some(executor.clone()),
            Some(reads.clone()),
            Some(Arc::clone(&supervisor)),
            handle,
        )
        .with_permissions(service.clone())
        .with_workspace_store(write.clone()),
    );
    Harness {
        dir,
        workspace,
        runtime,
        slot,
        pipeline,
        reads,
        write,
        service,
        manager,
        executor,
        backend,
        supervisor,
    }
}

impl Harness {
    fn bind(&self, root: &Path) -> Value {
        let canonical =
            aether_tauri::ipc::path::validate_workspace_root(root.to_string_lossy().as_ref())
                .expect("工作区根校验");
        self.backend
            .workspace_set(
                &WorkspaceSetRequest {
                    workspace_id: None,
                    root_path: Some(root.to_string_lossy().to_string()),
                },
                Some(&canonical),
            )
            .expect("workspace_set")
    }

    fn create_session(&self) -> SessionId {
        let value = self
            .backend
            .session_create(&SessionCreateRequest {
                runtime_id: "mock".to_owned(),
                title: "M3-12 E2E".to_owned(),
                workspace_id: None,
                model: None,
                thinking_depth: None,
            })
            .expect("session_create");
        SessionId::new(value["id"].as_str().expect("会话 id")).expect("会话 id 形态")
    }

    fn send(&self, session_id: &SessionId, text: &str, client_msg_id: &str) -> String {
        let ack = self
            .backend
            .session_send(&SessionSendRequest {
                session_id: session_id.as_str().to_owned(),
                text: text.to_owned(),
                client_msg_id: client_msg_id.to_owned(),
                thinking_depth: None,
            })
            .expect("session_send");
        ack["run_id"].as_str().expect("run_id").to_owned()
    }

    fn session_status(&self, session_id: &SessionId) -> Option<SessionStatus> {
        self.runtime
            .block_on(self.manager.session_status(session_id))
            .ok()
    }

    fn wait_session_status(&self, session_id: &SessionId, expected: SessionStatus) -> bool {
        wait_until(Duration::from_secs(30), || {
            self.session_status(session_id) == Some(expected)
        })
    }

    fn run_status(&self, run_id: &str) -> Option<aether_core::RunStatus> {
        let run_id = aether_core::RunId::new(run_id.to_owned()).ok()?;
        self.runtime
            .block_on(self.reads.run(&run_id))
            .ok()
            .flatten()
            .map(|run| run.status)
    }

    fn wait_run_finished(&self, run_id: &str) -> aether_core::RunStatus {
        assert!(
            wait_until(Duration::from_secs(30), || {
                matches!(
                    self.run_status(run_id),
                    Some(aether_core::RunStatus::Succeeded)
                        | Some(aether_core::RunStatus::Failed)
                        | Some(aether_core::RunStatus::Cancelled)
                        | Some(aether_core::RunStatus::Timeout)
                )
            }),
            "run {run_id} 必须到达终态"
        );
        self.run_status(run_id).expect("终态")
    }

    fn pending_for(&self, session_id: &SessionId) -> Vec<aether_security::ApprovalTicket> {
        self.service.pending_list(Some(session_id))
    }

    fn wait_pending(&self, session_id: &SessionId) -> aether_security::ApprovalTicket {
        assert!(
            wait_until(Duration::from_secs(30), || {
                !self.pending_for(session_id).is_empty()
            }),
            "审批票据必须进入 pending"
        );
        self.pending_for(session_id).remove(0)
    }

    fn resolve(
        &self,
        request_id: &str,
        decision: DtoPermissionDecision,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.permission_resolve(&PermissionResolveRequest {
            request_id: request_id.to_owned(),
            decision,
        })
    }

    fn status_transitions(&self, session_id: &SessionId) -> Vec<(String, String)> {
        self.runtime
            .block_on(self.pipeline.readback(session_id, 0))
            .expect("补读")
            .events
            .into_iter()
            .filter(|event| event.event_type().as_str() == "session.status_changed")
            .filter_map(|event| {
                let payload = event.payload.to_value().ok()?;
                Some((
                    payload["from"].as_str()?.to_owned(),
                    payload["to"].as_str()?.to_owned(),
                ))
            })
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

/// 等待态方向边（`running ↔ waiting_permission`）。
fn waiting_edges(transitions: &[(String, String)]) -> Vec<(String, String)> {
    transitions
        .iter()
        .filter(|(from, to)| {
            (from == "running" && to == "waiting_permission")
                || (from == "waiting_permission" && to == "running")
        })
        .cloned()
        .collect()
}

/// 全部转移必须命中冻结的 19 边白名单。
fn assert_transitions_legal(transitions: &[(String, String)]) {
    for (from, to) in transitions {
        let from = SessionStatus::from_str(from).unwrap();
        let to = SessionStatus::from_str(to).unwrap();
        assert!(
            session_transition_allowed(from, to),
            "非法转移：{from} → {to}（19 边白名单）"
        );
    }
}

/// DoD6：生产组合路径集成 E2E——真实工作区 ask → `running → waiting_permission` →
/// 决议合法回程（allow once + deny 两路径）。
#[test]
fn production_composition_real_workspace_ask_marks_waiting_and_returns() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    h.bind(h.workspace.path());
    assert_eq!(
        h.service.workspace_root_path(),
        h.workspace.path().canonicalize().unwrap(),
        "workspace_set 后权限基准必须换根到工作区"
    );
    let session = h.create_session();

    // ===== 路径 1：allow once =====
    let run_allow = h.send(&session, "permission-loop:notes.txt", "m3-12-allow-1");
    let ticket = h.wait_pending(&session);
    assert_eq!(ticket.resource, "fs.write");
    assert_eq!(ticket.action, "write");
    assert!(
        ticket
            .canonical_target
            .as_deref()
            .is_some_and(|target| target.contains("notes.txt")),
        "target 应为工作区内 canonical 路径：{:?}",
        ticket.canonical_target
    );
    assert!(
        h.wait_session_status(&session, SessionStatus::WaitingPermission),
        "真实工作区 ask 应置 waiting_permission（当前 {:?}）",
        h.session_status(&session)
    );
    h.resolve(&ticket.request_id, DtoPermissionDecision::Once)
        .expect("permission_resolve(once)");
    assert!(
        h.wait_session_status(&session, SessionStatus::Running),
        "决议后会话应合法回程 running"
    );
    let outcome = h.wait_run_finished(&run_allow);
    assert_eq!(
        outcome,
        aether_core::RunStatus::Succeeded,
        "allow 路径 run 应成功"
    );
    assert!(h.wait_session_status(&session, SessionStatus::Idle));

    // ===== 路径 2：deny =====
    let run_deny = h.send(&session, "permission-loop:denied.txt", "m3-12-deny-1");
    let denied = h.wait_pending(&session);
    assert!(
        h.wait_session_status(&session, SessionStatus::WaitingPermission),
        "第二次 ask 应再次置 waiting_permission"
    );
    h.resolve(&denied.request_id, DtoPermissionDecision::Deny)
        .expect("permission_resolve(deny)");
    assert!(
        h.wait_session_status(&session, SessionStatus::Running),
        "deny 后会话应合法回程 running"
    );
    let outcome = h.wait_run_finished(&run_deny);
    assert_eq!(
        outcome,
        aether_core::RunStatus::Succeeded,
        "deny 路径工具失败但 run 正常完成（Mock 收口语义）"
    );
    assert!(h.wait_session_status(&session, SessionStatus::Idle));

    // ===== 事件断言：from/to 精确匹配且全部合法 =====
    // 行更新先于事件落库：等待两条路径的 `running → idle` 收口事件全部在案（行状态
    // 已由上方断言确认）；仅影响证据完整性与方向边计数，不改变断言语义。
    assert!(
        wait_until(Duration::from_secs(5), || {
            h.status_transitions(&session)
                .iter()
                .filter(|(from, to)| from == "running" && to == "idle")
                .count()
                >= 2
        }),
        "两条路径的 run 收口事件均应落库"
    );
    let transitions = h.status_transitions(&session);
    assert_transitions_legal(&transitions);
    let edges = waiting_edges(&transitions);
    assert_eq!(
        edges,
        vec![
            ("running".to_owned(), "waiting_permission".to_owned()),
            ("waiting_permission".to_owned(), "running".to_owned()),
            ("running".to_owned(), "waiting_permission".to_owned()),
            ("waiting_permission".to_owned(), "running".to_owned()),
        ],
        "两条 ask 路径的等待态方向边必须精确匹配；实际：{transitions:?}"
    );

    // 回环零直通（执行器探针聚合；复用 M2-10/M3-08 口径）。
    let stats = h.executor.permission_loop_stats();
    assert!(
        stats.zero_passthrough,
        "权限回环必须零直通：{}",
        stats.summary()
    );
    assert!(
        stats.snapshot.requests_received >= 2,
        "至少两次真实回环请求：{}",
        stats.summary()
    );

    evidence(
        "dod6_production_composition",
        &json!({
            "session_id": session.as_str(),
            "transitions": transitions,
            "waiting_edges": edges,
            "permission_loop": stats.summary(),
            "workspace_root": h.service.workspace_root_path().to_string_lossy(),
            "layer": "L2 生产组合路径集成（Mock 适配器进程；真实 WebView 归 M4-05）",
        }),
    );
    println!(
        "[m3-12 DoD6] 生产组合路径：waiting_permission 置位/回程两次路径通过；转移序列 = {transitions:?}"
    );
    h.shutdown();
}
