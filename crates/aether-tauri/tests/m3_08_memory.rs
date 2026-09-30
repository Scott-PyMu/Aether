//! M3-08 集成测试：工作区记忆（设计 D14/D9；ADR-004 决策 3；v1.18 网关接线追记）。
//!
//! 覆盖（实施计划 v1.18 §4 M3-08 七项 DoD 的集成面）：
//! - DoD1：`workspace_set` 绑定 → 新会话按工作区注入记忆（优先级 + 32KB 上限文本由
//!   核心 `aether_control::memory` 组合，落 `sessions.system_prompt`）并经 `session.create`
//!   下发适配器（Mock session-log 观测）；
//! - DoD2：`memory.read/append/write` 经线协议工具调用上报 → `tool.call_started` →
//!   权限回环 → `tool.call_completed/failed`（真实 Mock 进程）；
//! - DoD3：工作区外 deny（越权不执行、不产生文件）、记忆白名单 allow（策略直决）；
//! - DoD4：原子写中断（终止适配器进程）→ 目标文件无半写（保持原内容）；
//! - DoD5：跨会话：会话 A 写入 → 新会话 B 注入读到更新内容；
//! - DoD6：外部修改后再写入返回 `memory_conflict` 且不覆盖；
//! - DoD7：执行器权限网关接线（`AdapterRunExecutor` 挂真实 `PermissionService`）——
//!   `workspace_set` 后权限基准与工作区同源；权限回环零直通（探针计数聚合）。
//!
//! 运行：`AETHER_MOCK_ADAPTER=<Bun 编译产物> cargo test -p aether-tauri --test m3_08_memory`
//! （由 `scripts/test/m3-08/verify-m3-08.mjs` 构建并设置；未设置且未要求时显式跳过，
//! `AETHER_REQUIRE_MOCK_ADAPTER=1` 时缺路径直接失败）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, Supervisor};
use aether_control::{
    EventPipeline, LifecycleConfig, PermissionConfig, PermissionService, SessionManager,
    SystemClock,
};
use aether_core::{RunStatus, Session, SessionId};
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
use aether_tauri::session_backend::{SessionBackend, WorkspaceBinding};
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
                "SKIP：AETHER_MOCK_ADAPTER 未设置（运行 pnpm verify:m3-08 构建 Mock 后执行）"
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
    println!("[m3-08] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M3_08_EVIDENCE_DIR") else {
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
    workspace2: TempDir,
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    pipeline: EventPipeline,
    reads: ReadPool,
    write: WriteQueue,
    service: PermissionService,
    #[allow(dead_code)]
    manager: SessionManager,
    executor: Arc<AdapterRunExecutor>,
    backend: Arc<SessionBackend>,
    supervisor: Arc<Supervisor>,
    session_log: PathBuf,
}

fn harness(binary: &Path) -> Harness {
    let dir = TempDir::new().expect("临时数据目录");
    let workspace = TempDir::new().expect("临时工作区 1");
    let workspace2 = TempDir::new().expect("临时工作区 2");
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
    let session_log = dir.path().join("mock-session-log.jsonl");
    let spec = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Mock", binary.to_path_buf())
            .official(true)
            .with_args([
                "--stream-deltas",
                "4",
                "--stream-interval-ms",
                "1",
                "--session-log",
                session_log.to_string_lossy().as_ref(),
            ]),
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
        workspace2,
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
        session_log,
    }
}

impl Harness {
    fn bind(&self, root: &Path) -> Value {
        // 与命令层同口径：canonicalize + 同步盘拒绝 + verbatim 剥离（`validate_workspace_root`）。
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

    fn create_session(&self) -> Session {
        self.create_session_with(None)
    }

    fn create_session_with(&self, workspace_id: Option<String>) -> Session {
        let value = self
            .backend
            .session_create(&SessionCreateRequest {
                runtime_id: "mock".to_owned(),
                title: "M3-08".to_owned(),
                workspace_id,
                model: None,
                thinking_depth: None,
            })
            .expect("session_create");
        serde_json::from_value(value).expect("Session 反序列化")
    }

    fn send(&self, session_id: &str, text: &str, client_msg_id: &str) -> String {
        let ack = self
            .backend
            .session_send(&SessionSendRequest {
                session_id: session_id.to_owned(),
                text: text.to_owned(),
                client_msg_id: client_msg_id.to_owned(),
                thinking_depth: None,
            })
            .expect("session_send");
        ack["run_id"].as_str().expect("run_id").to_owned()
    }

    fn run_status(&self, run_id: &str) -> Option<RunStatus> {
        let run_id = aether_core::RunId::new(run_id.to_owned()).ok()?;
        self.runtime
            .block_on(self.reads.run(&run_id))
            .ok()
            .flatten()
            .map(|run| run.status)
    }

    fn wait_run_finished(&self, run_id: &str) -> RunStatus {
        assert!(
            wait_until(Duration::from_secs(30), || {
                matches!(
                    self.run_status(run_id),
                    Some(RunStatus::Succeeded)
                        | Some(RunStatus::Failed)
                        | Some(RunStatus::Cancelled)
                        | Some(RunStatus::Timeout)
                )
            }),
            "run {run_id} 必须到达终态"
        );
        self.run_status(run_id).expect("终态")
    }

    fn pending_for(&self, session_id: &SessionId) -> Vec<aether_security::ApprovalTicket> {
        self.service.pending_list(Some(session_id))
    }

    fn session_events(&self, session_id: &SessionId) -> Vec<aether_core::EventEnvelope> {
        self.runtime
            .block_on(self.pipeline.readback(session_id, 0))
            .expect("补读")
            .events
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

    fn shutdown(self) {
        self.runtime.block_on(self.supervisor.shutdown_all());
        self.runtime.block_on(self.pipeline.shutdown()).ok();
        if let Some(storage) = self.slot.take() {
            self.runtime.block_on(storage.shutdown()).ok();
        }
        let _ = &self.dir;
    }
}

fn seed_memory(root: &Path, name: &str, content: &str) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, content).expect("写入记忆文件");
    path
}

// ===== DoD1/DoD5/DoD7：workspace_set → 注入 + 权限基准同源 + 旧会话不迁移 =====

#[test]
fn workspace_set_injects_memory_and_swaps_permission_root() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    seed_memory(h.workspace.path(), "AETHER.md", "AETHER 约定");
    seed_memory(h.workspace.path(), "CLAUDE.md", "CLAUDE 约定");

    // 绑定前：策略基准 = 数据目录（M3-03 兜底）。
    assert_eq!(
        h.service.workspace_root_path(),
        h.dir.path().canonicalize().unwrap()
    );
    let bound = h.bind(h.workspace.path());
    let workspace_id = bound["workspace_id"].as_str().unwrap().to_owned();
    let expected_root = aether_tauri::ipc::path::validate_workspace_root(
        h.workspace.path().to_string_lossy().as_ref(),
    )
    .unwrap();
    assert_eq!(
        bound["root_path"].as_str().unwrap(),
        expected_root.to_string_lossy(),
        "绑定回执 root_path 为命令层 canonical（verbatim 已剥离）"
    );
    // 权限基准与工作区同源（canonical）。
    assert_eq!(
        h.service.workspace_root_path(),
        h.workspace.path().canonicalize().unwrap(),
        "workspace_set 后权限基准必须换根到工作区"
    );

    // 新会话：workspace_id 落库 + 记忆注入（优先级 AETHER.md > CLAUDE.md）。
    let session = h.create_session();
    assert_eq!(
        session.workspace_id.as_ref().map(|id| id.as_str()),
        Some(workspace_id.as_str())
    );
    let prompt = session.system_prompt.clone().expect("系统提示必须注入");
    assert!(prompt.contains("工作区约定"), "{prompt}");
    assert!(prompt.contains("来源：AETHER.md"), "{prompt}");
    assert!(prompt.contains("AETHER 约定"), "{prompt}");
    assert!(!prompt.contains("CLAUDE 约定"), "低优先级文件不得混入");

    // AGENTS.md 出现后优先级翻转（重新创建会话读取最新优先级）。
    seed_memory(h.workspace.path(), "AGENTS.md", "AGENTS 约定");
    let session2 = h.create_session();
    let prompt2 = session2.system_prompt.clone().expect("注入");
    assert!(prompt2.contains("来源：AGENTS.md"), "{prompt2}");
    assert!(prompt2.contains("AGENTS 约定"), "{prompt2}");

    // 显式 workspace_id 形式；未知 id 拒绝且不落库。
    let explicit = h.create_session_with(Some(workspace_id.clone()));
    assert_eq!(
        explicit.workspace_id.as_ref().map(|id| id.as_str()),
        Some(workspace_id.as_str())
    );
    let error = h
        .backend
        .session_create(&SessionCreateRequest {
            runtime_id: "mock".to_owned(),
            title: "未知工作区".to_owned(),
            workspace_id: Some("01J8ZQ5R0N7W9Y8X6V4T2S0K1Z".to_owned()),
            model: None,
            thinking_depth: None,
        })
        .expect_err("未知 workspace_id 必须拒绝");
    assert_eq!(
        error.code,
        aether_tauri::ipc::error::IpcErrorCode::InvalidValue
    );

    // 切换工作区：仅新会话使用新工作区；旧会话不迁移（P0/ADR-004 决策 3）。
    seed_memory(h.workspace2.path(), "AGENTS.md", "工作区 2 约定");
    let bound2 = h.bind(h.workspace2.path());
    let workspace_id2 = bound2["workspace_id"].as_str().unwrap().to_owned();
    assert_ne!(workspace_id, workspace_id2);
    let session3 = h.create_session();
    assert_eq!(
        session3.workspace_id.as_ref().map(|id| id.as_str()),
        Some(workspace_id2.as_str())
    );
    assert!(
        session3
            .system_prompt
            .as_deref()
            .unwrap_or("")
            .contains("工作区 2 约定"),
        "新会话必须注入新工作区记忆"
    );
    let old = h
        .runtime
        .block_on(h.reads.session(&session.id))
        .unwrap()
        .expect("旧会话行");
    assert_eq!(
        old.workspace_id.as_ref().map(|id| id.as_str()),
        Some(workspace_id.as_str()),
        "旧会话 workspace_id 不迁移"
    );
    assert_eq!(
        old.system_prompt, session.system_prompt,
        "旧会话注入文本不变"
    );

    evidence(
        "dod1_workspace_binding",
        &json!({
            "task": "M3-08 DoD1/DoD7 工作区绑定 → 记忆注入 + 权限基准同源 + 旧会话不迁移",
            "workspace_id": workspace_id,
            "workspace_id_2": workspace_id2,
            "session_prompt_head": prompt.lines().next(),
            "new_session_prompt_head": session3.system_prompt.as_deref().unwrap_or("").lines().next(),
            "policy_root_after_bind": h.service.workspace_root_path().to_string_lossy(),
        }),
    );
    h.shutdown();
}

// ===== DoD2/DoD3/DoD7：记忆工具回环 + 零直通 + 越权 deny =====

#[test]
fn memory_tools_round_trip_through_executor_gate_zero_passthrough() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    let target = seed_memory(h.workspace.path(), "AGENTS.md", "初始内容");
    h.bind(h.workspace.path());
    let session = h.create_session();
    let session_id = session.id.clone();

    // `memory.write` → fs.write ask（工作区内非白名单？AGENTS.md 为白名单 → 策略 allow，
    // 但工作区记忆白名单是 allow；此处断言「经回环」：策略直决同样经 `permission.resolve`）。
    let run_id = h.send(
        session_id.as_str(),
        &format!("memory.write|{}|新写入内容", target.display()),
        "01J8ZQ5R0N7W9Y8X6V4T2S0K1A",
    );
    assert!(
        wait_until(Duration::from_secs(30), || {
            matches!(
                h.run_status(&run_id),
                Some(RunStatus::Succeeded) | Some(RunStatus::Failed) | Some(RunStatus::Cancelled)
            )
        }),
        "run 必须收口"
    );
    assert_eq!(h.run_status(&run_id), Some(RunStatus::Succeeded));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "新写入内容",
        "允许后执行真实原子写"
    );

    // 事件序列（归属重写）：tool.call_started → tool.call_completed；核心管线只承载
    // 核心拥有的事件——`permission.resolved` 为适配器侧事件（回环证据在探针/审计/
    // 待审批路径），不进入核心事件流（M2-10 口径）。转发提交与 run 终态落库异步，
    // 断言前轮询等待事件到齐。
    let tool_types: Vec<String> = {
        let mut types = Vec::new();
        assert!(
            wait_until(Duration::from_secs(10), || {
                types = h
                    .session_events(&session_id)
                    .iter()
                    .filter(|event| {
                        event.run_id.as_ref().map(|id| id.as_str()) == Some(run_id.as_str())
                    })
                    .map(|event| event.event_type().as_str().to_owned())
                    .filter(|kind| kind.starts_with("tool.") || kind.starts_with("permission."))
                    .collect();
                types == vec!["tool.call_started", "tool.call_completed"]
            }),
            "工具事件序列未在窗口内到齐：{types:?}"
        );
        types
    };
    assert_eq!(tool_types, vec!["tool.call_started", "tool.call_completed"]);
    // 零直通（聚合探针：1 请求 = 1 决议 = 1 下发）。
    let stats = h.executor.permission_loop_stats();
    assert!(
        stats.zero_passthrough,
        "执行器回环必须零直通：{}",
        stats.summary()
    );
    assert_eq!(stats.snapshot.requests_received, 1);
    assert_eq!(stats.snapshot.decisions, 1);
    assert_eq!(stats.snapshot.resolutions_sent, 1);

    // `memory.read`（fs.read，工作区内 allow）。
    let read_target = target.clone();
    let read_run = h.send(
        session_id.as_str(),
        &format!("memory.read|{}", read_target.display()),
        "01J8ZQ5R0N7W9Y8X6V4T2S0K1B",
    );
    assert_eq!(h.wait_run_finished(&read_run), RunStatus::Succeeded);
    let stats = h.executor.permission_loop_stats();
    assert!(stats.zero_passthrough, "{}", stats.summary());
    assert_eq!(stats.snapshot.requests_received, 2);

    // `memory.write` 到工作区外：策略 deny（零直通，适配器不执行写）。
    let outside = h.dir.path().join("outside-memory.md");
    let deny_run = h.send(
        session_id.as_str(),
        &format!("memory.write|{}|越权内容", outside.display()),
        "01J8ZQ5R0N7W9Y8X6V4T2S0K1C",
    );
    assert_eq!(h.wait_run_finished(&deny_run), RunStatus::Succeeded);
    assert!(!outside.exists(), "越权拒绝必须不产生文件（适配器不执行）");
    // 转发提交异步：轮询等待 deny 的 tool.call_failed 到齐后再断言错误码。
    let mut deny_code: Option<String> = None;
    assert!(
        wait_until(Duration::from_secs(10), || {
            deny_code = h.session_events(&session_id).iter().find_map(|event| {
                if event.run_id.as_ref().map(|id| id.as_str()) == Some(deny_run.as_str())
                    && event.event_type() == aether_core::EventType::ToolCallFailed
                {
                    match &event.payload {
                        aether_core::EventPayload::ToolCallFailed(payload) => {
                            Some(payload.error.code.clone())
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            });
            deny_code.is_some()
        }),
        "缺少 tool.call_failed（deny 路径）"
    );
    assert_eq!(deny_code.as_deref(), Some("denied"));
    let stats = h.executor.permission_loop_stats();
    assert!(stats.zero_passthrough, "{}", stats.summary());
    assert_eq!(stats.snapshot.requests_received, 3);
    assert_eq!(stats.snapshot.decisions, 3);
    assert_eq!(stats.snapshot.resolutions_sent, 3);
    assert_eq!(
        h.service.pending_list(None).len(),
        0,
        "策略直决不产生 pending"
    );

    // 审计可查（策略直决路径：allow/deny 各一条；ask 路径见 ask 用例）。
    let actions: Vec<String> = h
        .runtime
        .block_on(h.reads.audit_log(200))
        .unwrap()
        .into_iter()
        .map(|record| record.action)
        .collect();
    for expected in [
        "permission.allowed_by_policy",
        "permission.denied_by_policy",
    ] {
        assert!(
            actions.iter().any(|action| action == expected),
            "{actions:?}"
        );
    }

    evidence(
        "dod2_tool_round_trip",
        &json!({
            "task": "M3-08 DoD2/DoD3/DoD7 记忆工具回环（零直通探针聚合）",
            "write_run": run_id,
            "read_run": read_run,
            "deny_run": deny_run,
            "tool_event_types": tool_types,
            "probe": stats.summary(),
            "outside_file_created": outside.exists(),
            "audit_actions": actions,
        }),
    );
    h.shutdown();
}

/// `fs.write` ask（工作区内非白名单）：pending → IPC 决议 → 适配器执行写（真实回环）。
#[test]
fn in_workspace_ask_requires_ipc_resolution_before_write() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    h.bind(h.workspace.path());
    let session = h.create_session();
    let session_id = session.id.clone();
    // 工作区内非记忆文件（NOTES.md）→ fs.write ask（策略矩阵）。
    let target = h.workspace.path().join("NOTES.md");
    let run_id = h.send(
        session_id.as_str(),
        &format!("memory.write|{}|审批后写入", target.display()),
        "01J8ZQ5R0N7W9Y8X6V4T2S0K1D",
    );
    assert!(
        wait_until(Duration::from_secs(10), || !h
            .pending_for(&session_id)
            .is_empty()),
        "工作区内非白名单写必须进入待审批"
    );
    let ticket = h.pending_for(&session_id)[0].clone();
    assert_eq!(ticket.resource, "fs.write");
    assert_eq!(ticket.action, "write");
    assert_eq!(
        ticket.target.as_deref(),
        Some(target.to_string_lossy().as_ref())
    );
    assert!(!target.exists(), "决议前不得执行写（零直通）");
    assert!(h.run_status(&run_id) != Some(RunStatus::Succeeded));

    let resolved = h
        .resolve(&ticket.request_id, DtoPermissionDecision::Once)
        .expect("permission_resolve");
    assert_eq!(resolved["decision"], "allow");
    assert_eq!(h.wait_run_finished(&run_id), RunStatus::Succeeded);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "审批后写入");
    assert!(h.pending_for(&session_id).is_empty());
    // 审计可查（ask 路径：requested + resolved）。
    let actions: Vec<String> = h
        .runtime
        .block_on(h.reads.audit_log(200))
        .unwrap()
        .into_iter()
        .map(|record| record.action)
        .collect();
    for expected in ["permission.requested", "permission.resolved"] {
        assert!(
            actions.iter().any(|action| action == expected),
            "{actions:?}"
        );
    }

    let stats = h.executor.permission_loop_stats();
    assert!(stats.zero_passthrough, "{}", stats.summary());
    assert_eq!(stats.snapshot.requests_received, 1);

    evidence(
        "dod7_ask_ipc_round_trip",
        &json!({
            "task": "M3-08 DoD7 工作区内 ask：pending → IPC 允许 → 适配器写入（零直通）",
            "request_id": ticket.request_id,
            "target": ticket.target,
            "probe": stats.summary(),
        }),
    );
    h.shutdown();
}

// ===== DoD6：冲突处理 =====

#[test]
fn memory_conflict_is_reported_and_does_not_overwrite() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    let target = seed_memory(h.workspace.path(), "AGENTS.md", "原始内容");
    h.bind(h.workspace.path());
    let session = h.create_session();
    let session_id = session.id.clone();
    let run_id = h.send(
        session_id.as_str(),
        &format!("memory.conflict|{}|本次写入", target.display()),
        "01J8ZQ5R0N7W9Y8X6V4T2S0K1E",
    );
    assert_eq!(h.wait_run_finished(&run_id), RunStatus::Succeeded);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "原始内容外部修改",
        "冲突时不得覆盖（仅保留外部修改内容）"
    );
    // 转发提交异步：轮询等待 tool.call_failed 到齐后再断言错误码。
    let mut failed_code: Option<String> = None;
    assert!(
        wait_until(Duration::from_secs(10), || {
            failed_code = h.session_events(&session_id).iter().find_map(|event| {
                if event.run_id.as_ref().map(|id| id.as_str()) == Some(run_id.as_str())
                    && event.event_type() == aether_core::EventType::ToolCallFailed
                {
                    match &event.payload {
                        aether_core::EventPayload::ToolCallFailed(payload) => {
                            Some(payload.error.code.clone())
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            });
            failed_code.is_some()
        }),
        "缺少 tool.call_failed(memory_conflict)"
    );
    assert_eq!(failed_code.as_deref(), Some("memory_conflict"));
    evidence(
        "dod6_memory_conflict",
        &json!({
            "task": "M3-08 DoD6 外部修改 → memory_conflict 且不覆盖",
            "run_id": run_id,
            "file_content": std::fs::read_to_string(&target).unwrap(),
        }),
    );
    h.shutdown();
}

// ===== DoD4：原子写中断（终止进程）不产生半写文件 =====

#[test]
fn atomic_write_interrupted_by_termination_leaves_target_intact() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    let target = seed_memory(h.workspace.path(), "AGENTS.md", "原始稳定内容");
    h.bind(h.workspace.path());
    let session = h.create_session();
    let session_id = session.id.clone();
    // 256KB、4KB/50ms 分块 → 有充足窗口在 rename 前终止进程。
    let run_id = h.send(
        session_id.as_str(),
        &format!("memory.slow|{}|262144", target.display()),
        "01J8ZQ5R0N7W9Y8X6V4T2S0K1F",
    );
    // 等待临时文件出现（分块写已开始）。
    let temp_seen = wait_until(Duration::from_secs(10), || {
        std::fs::read_dir(h.workspace.path())
            .map(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.file_name().to_string_lossy().contains(".aether-tmp-"))
            })
            .unwrap_or(false)
    });
    assert!(temp_seen, "分块慢写必须已创建临时文件");
    // 终止适配器（D2/D5 终止序列；Windows Job Object / 跨平台整树回收）。
    let runtime = h.supervisor.get("mock").expect("runtime");
    let report = h.runtime.block_on(runtime.shutdown());
    assert!(
        !report.executed_steps().is_empty() || report.exited,
        "终止报告形状异常：{report:?}"
    );
    // 目标文件未被半写覆盖（内容仍为原始值；临时文件可能残留但非目标文件）。
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "原始稳定内容",
        "写入中断不得产生半写目标文件"
    );
    let leftovers: Vec<String> = std::fs::read_dir(h.workspace.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    let half_written = leftovers
        .iter()
        .filter(|name| name.starts_with("AGENTS.md") && name.as_str() != "AGENTS.md")
        .count();
    assert_eq!(half_written, 0, "目标文件名不得出现半写变体：{leftovers:?}");
    evidence(
        "dod4_atomic_interrupt",
        &json!({
            "task": "M3-08 DoD4 原子写中断（终止适配器进程）→ 目标无半写",
            "run_id": run_id,
            "target_content": std::fs::read_to_string(&target).unwrap(),
            "workspace_files": leftovers,
        }),
    );
    h.shutdown();
}

// ===== DoD5：跨会话注入读到上一会话写入 =====

#[test]
fn cross_session_injection_reads_previous_write() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    let target = seed_memory(h.workspace.path(), "AGENTS.md", "基线\n");
    h.bind(h.workspace.path());

    let session_a = h.create_session();
    let run_a = h.send(
        session_a.id.as_str(),
        &format!(
            "memory.append|{}|会话 A 沉淀（跨会话可见）",
            target.display()
        ),
        "01J8ZQ5R0N7W9Y8X6V4T2S0K1G",
    );
    assert_eq!(h.wait_run_finished(&run_a), RunStatus::Succeeded);
    assert!(std::fs::read_to_string(&target)
        .unwrap()
        .contains("会话 A 沉淀"));

    // 新会话 B：核心读取更新后的文件并注入；发一次 run 触发适配器会话创建，
    // Mock 观测到注入文本（session-log）。
    let session_b = h.create_session();
    let prompt_b = session_b.system_prompt.clone().expect("会话 B 注入");
    assert!(
        prompt_b.contains("会话 A 沉淀（跨会话可见）"),
        "新会话注入必须包含上一会话写入：{prompt_b}"
    );
    let run_b = h.send(
        session_b.id.as_str(),
        "会话 B 触发适配器会话创建",
        "01J8ZQ5R0N7W9Y8X6V4T2S0K1H",
    );
    assert_eq!(h.wait_run_finished(&run_b), RunStatus::Succeeded);
    let records = h.session_log_records();
    let injected: Vec<&Value> = records
        .iter()
        .filter(|record| {
            record["method"] == "session.create" && record["has_system_prompt"] == true
        })
        .collect();
    assert!(
        injected.iter().any(|record| record["system_prompt"]
            .as_str()
            .unwrap_or("")
            .contains("会话 A 沉淀（跨会话可见）")),
        "适配器必须收到含更新内容的注入：{records:?}"
    );
    evidence(
        "dod5_cross_session_injection",
        &json!({
            "task": "M3-08 DoD5 会话 A 写入 → 新会话 B 注入读到更新",
            "session_a": session_a.id.as_str(),
            "session_b": session_b.id.as_str(),
            "session_b_prompt_has_update": prompt_b.contains("会话 A 沉淀（跨会话可见）"),
            "mock_injection_observed": injected.len(),
        }),
    );
    h.shutdown();
}

// ===== 启动恢复：最近绑定工作区在重启装配后恢复（M3-08） =====

#[test]
fn workspace_binding_is_restored_on_restart() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);
    seed_memory(h.workspace.path(), "AGENTS.md", "恢复约定");
    let bound = h.bind(h.workspace.path());
    let workspace_id = bound["workspace_id"].as_str().unwrap().to_owned();

    // 装配第二个后端（同一存储/权限服务），模拟重启后的启动恢复路径。
    let restored_backend = SessionBackend::new(
        Arc::new(NotImplementedBackend),
        None,
        None,
        Some(h.reads.clone()),
        Some(Arc::clone(&h.supervisor)),
        h.runtime.handle().clone(),
    )
    .with_permissions(h.service.clone())
    .with_workspace_store(h.write.clone());
    let binding: Option<WorkspaceBinding> = h
        .runtime
        .block_on(restored_backend.restore_workspace_binding())
        .expect("恢复工作区绑定");
    let binding = binding.expect("必须恢复最近绑定的工作区");
    assert_eq!(binding.id.as_str(), workspace_id);
    assert_eq!(
        h.service.workspace_root_path(),
        h.workspace.path().canonicalize().unwrap(),
        "恢复后权限基准与工作区同源"
    );

    // 恢复后新会话注入命中恢复的工作区。
    let session = h.create_session();
    assert_eq!(
        session.workspace_id.as_ref().map(|id| id.as_str()),
        Some(workspace_id.as_str())
    );
    assert!(session
        .system_prompt
        .as_deref()
        .unwrap_or("")
        .contains("恢复约定"));
    evidence(
        "dod7_restore_binding",
        &json!({
            "task": "M3-08 启动恢复：最近绑定工作区 → 权限基准同源 + 新会话注入",
            "workspace_id": workspace_id,
            "restored_root": binding.root_path.to_string_lossy(),
            "session_workspace_id": session.workspace_id.as_ref().map(|id| id.as_str()),
        }),
    );
    h.shutdown();
}
