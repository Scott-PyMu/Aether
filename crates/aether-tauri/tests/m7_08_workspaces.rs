//! M7-08 集成测试：工作区管理与多工作区（ADR-026 §2.1；R5/R15 不变量 11）。
//!
//! 覆盖 DoD1–DoD6 的可执行验证面：
//! - DoD1：五命令参数/语义（`workspaces_list` / `workspace_create` /
//!   `workspace_update` / `workspace_delete` / `session_workspace_assign`），写路径经
//!   单写队列（`StoreCommand` 单事务）；
//! - DoD2：权限基准**按会话创建固定**——`workspace_set` 换根只对新会话生效；
//!   **改派后原权限基准仍生效**（A 基线：写 B → ask；写 A → allow）；记忆注入源切换；
//! - DoD3：删除安全（存在引用 → `workspace_in_use`；只删行、不删磁盘文件；审计）；
//! - DoD4：`workspaces.memory_files` P1 恒空数组；
//! - DoD5：合成组「默认（数据目录）」与计数（在途/总数）；
//! - DoD6：M3-08 `workspace_set` 新口径（换根仅新会话生效）。
//!
//! 运行：`cargo test -p aether-tauri --test m7_08_workspaces`
//! （不依赖真实适配器进程：监督器运行时保持 cold，仅验证命令/存储/权限判定面）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec};
use aether_control::{
    EventPipeline, ExecutorFuture, ExecutorOutcome, LifecycleConfig, PermissionConfig,
    PermissionResolution, PermissionService, RunExecutor, RunRequest, SessionManager, SystemClock,
};
use aether_core::{
    PermissionDecision, Run, RunId, RunStatus, Session, SessionId, SessionStatus, WorkspaceId,
};
use aether_security::PolicyEngine;
use aether_store::{ReadPool, StoreCommand, WriteQueue};
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    SessionCreateRequest, SessionWorkspaceAssignRequest, WorkspaceCreateRequest,
    WorkspaceDeleteRequest, WorkspaceSetRequest, WorkspaceUpdateRequest, WorkspacesListRequest,
};
use aether_tauri::runtime_control::boot_supervisor;
use aether_tauri::session_backend::SessionBackend;
use serde_json::Value;
use tempfile::TempDir;

// ===== 测试执行器（不 spawn 适配器；改派测试不发送消息）=====

struct NoopExecutor;

impl RunExecutor for NoopExecutor {
    fn execute(&self, _request: RunRequest) -> ExecutorFuture<'_> {
        Box::pin(async { ExecutorOutcome::Cancelled { reason: None } })
    }
}

struct Harness {
    dir: TempDir,
    workspace_a: TempDir,
    workspace_b: TempDir,
    workspace_c: TempDir,
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    pipeline: EventPipeline,
    reads: ReadPool,
    write: WriteQueue,
    service: PermissionService,
    backend: Arc<SessionBackend>,
}

fn plain(path: &Path) -> String {
    let text = std::fs::canonicalize(path)
        .expect("canonicalize")
        .to_string_lossy()
        .to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

/// 不做 canonicalize 的展示路径（合成组 root_path 原样返回 `data_dir`；
/// CI 临时目录可能含 `RUNNER~1` 8.3 短名，canonicalize 会改写为长名）。
fn plain_raw(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

/// 目标文件可能尚不存在：canonicalize 父目录后拼接（测试夹具口径同 M3-08
/// `outside_target`；规避 CI 临时目录 8.3 短名别名）。
fn plain_join(path: &Path) -> String {
    let parent = path.parent().unwrap_or(path);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string());
    let text = std::fs::canonicalize(parent)
        .expect("canonicalize parent")
        .join(file_name.unwrap_or_default())
        .to_string_lossy()
        .to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

fn harness() -> Harness {
    let dir = TempDir::new().expect("数据目录");
    let workspace_a = TempDir::new().expect("工作区 A");
    let workspace_b = TempDir::new().expect("工作区 B");
    let workspace_c = TempDir::new().expect("工作区 C");
    std::fs::write(workspace_a.path().join("AGENTS.md"), "A 区约定").unwrap();
    std::fs::write(workspace_b.path().join("AGENTS.md"), "B 区约定").unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio");
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
    .expect("核心");
    let service = PermissionService::new(
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
        Arc::new(SystemClock),
        PolicyEngine::new(dir.path()).expect("数据目录基准"),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
    );
    // 监督器：运行时保持 cold（不预热、不 spawn；仅用于 session_create 的注册表查找）。
    let program = dir.path().join("mock-adapter.exe");
    std::fs::write(&program, b"stub").unwrap();
    let spec = RuntimeSpec::with_fresh_token(RuntimeManifest::new("mock", "Mock", program));
    let supervisor = Arc::new(
        boot_supervisor(vec![spec], Some(&dir.path().join("adapters.json"))).expect("监督器"),
    );
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        Arc::new(NoopExecutor),
    );
    let backend = Arc::new(
        SessionBackend::new(
            Arc::new(NotImplementedBackend) as Arc<dyn IpcBackend>,
            Some(manager),
            None,
            Some(reads.clone()),
            Some(supervisor),
            handle,
        )
        .with_permissions(service.clone())
        .with_workspace_store(write.clone())
        .with_data_dir(dir.path()),
    );
    Harness {
        dir,
        workspace_a,
        workspace_b,
        workspace_c,
        runtime,
        slot,
        pipeline,
        reads,
        write,
        service,
        backend,
    }
}

impl Harness {
    fn set_workspace(&self, root: &Path) -> Value {
        let root_text = plain(root);
        let canonical =
            aether_tauri::ipc::path::validate_workspace_root(&root_text).expect("工作区根校验");
        self.backend
            .workspace_set(
                &WorkspaceSetRequest {
                    workspace_id: None,
                    root_path: Some(root_text),
                },
                Some(&canonical),
            )
            .expect("workspace_set")
    }

    fn create_workspace(
        &self,
        name: &str,
        root: &Path,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        let request = WorkspaceCreateRequest {
            name: name.to_owned(),
            root_path: plain(root),
        };
        let canonical = request.canonical_root_path().expect("root 校验");
        self.backend.workspace_create(&request, &canonical)
    }

    fn create_session(&self, workspace_id: Option<String>) -> Session {
        let value = self
            .backend
            .session_create(&SessionCreateRequest {
                runtime_id: Some("mock".to_owned()),
                title: "M7-08".to_owned(),
                workspace_id,
                model: None,
                thinking_depth: None,
                permission_mode: None,
                preset_id: None,
                provider_id: None,
            })
            .expect("session_create");
        serde_json::from_value(value).expect("Session 反序列化")
    }

    fn list(&self) -> Value {
        self.backend
            .workspaces_list(&WorkspacesListRequest {})
            .expect("workspaces_list")
    }

    fn assign(
        &self,
        session_id: &str,
        workspace_id: &str,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend
            .session_workspace_assign(&SessionWorkspaceAssignRequest {
                session_id: session_id.to_owned(),
                workspace_id: workspace_id.to_owned(),
            })
    }

    fn permission(
        &self,
        request_id: &str,
        session_id: &SessionId,
        resource: &str,
        action: &str,
        target: &str,
    ) -> aether_control::PermissionRequest {
        aether_control::PermissionRequest {
            request_id: request_id.to_owned(),
            session_id: Some(session_id.clone()),
            runtime_id: None,
            resource: resource.to_owned(),
            action: action.to_owned(),
            target: Some(target.to_owned()),
            content_bytes: Some(8),
        }
    }

    /// 直接请求权限（无需审批的路径；ask 路径必须经 [`Self::ask_and_deny`]）。
    fn request_now(
        &self,
        request_id: &str,
        session_id: &SessionId,
        resource: &str,
        action: &str,
        target: &Path,
    ) -> PermissionResolution {
        let target = plain_join(target);
        let request = self.permission(request_id, session_id, resource, action, &target);
        self.runtime
            .block_on(self.service.request(request))
            .expect("permission request")
    }

    /// 请求一个 ask（进入 pending）后 deny，返回决议与 pending 是否存在过。
    fn ask_and_deny(
        &self,
        request_id: &str,
        session_id: &SessionId,
        resource: &str,
        action: &str,
        target: &Path,
    ) -> PermissionResolution {
        let target = plain_join(target);
        let request = self.permission(request_id, session_id, resource, action, &target);
        let service = self.service.clone();
        let task = self
            .runtime
            .spawn(async move { service.request(request).await.expect("request") });
        let deadline = Instant::now() + Duration::from_secs(10);
        let ticket = loop {
            let pending = self.service.pending_list(None);
            if let Some(ticket) = pending.first() {
                break ticket.clone();
            }
            assert!(Instant::now() < deadline, "ask 未进入 pending");
            std::thread::sleep(Duration::from_millis(5));
        };
        self.runtime
            .block_on(
                self.service
                    .resolve(&ticket.request_id, PermissionDecision::Deny, None),
            )
            .expect("resolve");
        self.runtime.block_on(task).expect("ask task")
    }

    fn audit_actions(&self) -> Vec<String> {
        self.runtime
            .block_on(self.reads.audit_log(2_000))
            .expect("audit")
            .into_iter()
            .map(|record| record.action)
            .collect()
    }

    fn session(&self, session_id: &str) -> Session {
        self.runtime
            .block_on(
                self.reads
                    .session(&SessionId::new(session_id.to_owned()).unwrap()),
            )
            .unwrap()
            .expect("会话存在")
    }

    fn set_session_status(&self, session_id: &str, status: SessionStatus) {
        self.runtime
            .block_on(self.write.execute(StoreCommand::UpdateSessionStatus {
                session_id: SessionId::new(session_id.to_owned()).unwrap(),
                status,
                updated_at: 2,
                closed_at: None,
            }))
            .unwrap();
    }

    fn insert_run(&self, run_id: &str, session_id: &str, status: RunStatus) {
        self.runtime
            .block_on(self.write.execute(StoreCommand::InsertRun {
                run: Run {
                    id: RunId::new(run_id.to_owned()).unwrap(),
                    session_id: SessionId::new(session_id.to_owned()).unwrap(),
                    status,
                    input_message_id: None,
                    thinking_depth: None,
                    error: None,
                    started_at: 1,
                    finished_at: None,
                },
            }))
            .unwrap();
    }
}

/// DoD5：合成组「默认（数据目录）」+ 计数（在途/总数）；工作区创建/改名/查无错误码。
#[test]
fn list_create_update_and_synthetic_default_group() {
    let harness = harness();
    std::fs::write(harness.workspace_c.path().join("x.txt"), b"x").unwrap();

    let listed = harness.list();
    assert_eq!(listed["workspaces"].as_array().map(Vec::len), Some(0));
    assert_eq!(listed["unassigned"]["name"], "默认（数据目录）");
    assert_eq!(
        listed["unassigned"]["root_path"].as_str(),
        Some(plain_raw(harness.dir.path()).as_str()),
        "合成组展示数据目录根路径（不建 workspaces 行）"
    );

    let created = harness
        .create_workspace("工作区 C", harness.workspace_c.path())
        .expect("workspace_create");
    let workspace_id = created["workspace_id"].as_str().expect("id").to_owned();
    assert_eq!(created["name"], "工作区 C");
    assert_eq!(created["active_sessions"], 0);
    assert_eq!(created["total_sessions"], 0);
    assert_eq!(created["memory_files"], serde_json::json!([]));

    // 同根路径重复创建 → invalid_value（提示使用既有工作区）。
    let duplicate = harness
        .create_workspace("重复", harness.workspace_c.path())
        .expect_err("同根路径必须拒绝");
    assert_eq!(duplicate.code.as_str(), "invalid_value");
    assert!(duplicate.message.contains("已存在"));

    // 改名（仅名称）：root_path 不变。
    let renamed = harness
        .backend
        .workspace_update(&WorkspaceUpdateRequest {
            workspace_id: workspace_id.clone(),
            name: "C 改名".to_owned(),
        })
        .expect("workspace_update");
    assert_eq!(renamed["name"], "C 改名");
    assert_eq!(renamed["root_path"].as_str(), created["root_path"].as_str());

    // 查无 → workspace_not_found。
    let missing = harness
        .backend
        .workspace_update(&WorkspaceUpdateRequest {
            workspace_id: "01J0000000000000000000W99".to_owned(),
            name: "X".to_owned(),
        })
        .expect_err("查无");
    assert_eq!(missing.code.as_str(), "workspace_not_found");

    // 新会话（未绑定工作区）→ 合成组计数 +1（total）。
    let session = harness.create_session(None);
    let listed = harness.list();
    assert_eq!(
        listed["unassigned"]["total_sessions"].as_u64(),
        Some(1),
        "未分配会话归入合成组"
    );
    assert_eq!(listed["unassigned"]["active_sessions"].as_u64(), Some(0));
    assert_eq!(session.workspace_id, None);

    // 审计：workspace.created / workspace.updated。
    let actions = harness.audit_actions();
    assert!(actions.contains(&"workspace.created".to_owned()));
    assert!(actions.contains(&"workspace.updated".to_owned()));
}

/// DoD3：存在引用 → `workspace_in_use`（不删行）；改派后删除成功；磁盘文件保留。
#[test]
fn delete_refuses_in_use_and_never_touches_disk() {
    let harness = harness();
    let marker = harness.workspace_c.path().join("keep.txt");
    std::fs::write(&marker, b"must-survive").unwrap();
    let created = harness
        .create_workspace("C", harness.workspace_c.path())
        .expect("create");
    let workspace_id = created["workspace_id"].as_str().unwrap().to_owned();
    let session = harness.create_session(Some(workspace_id.clone()));

    let error = harness
        .backend
        .workspace_delete(&WorkspaceDeleteRequest {
            workspace_id: workspace_id.clone(),
        })
        .expect_err("存在引用必须拒绝");
    assert_eq!(error.code.as_str(), "workspace_in_use");
    assert!(error.message.contains("1 个会话"), "消息含引用计数");
    assert_eq!(harness.list()["workspaces"].as_array().unwrap().len(), 1);

    // 改派到 B → A（C）无引用 → 删除成功；磁盘文件保留。
    let workspace_b = harness
        .create_workspace("B", harness.workspace_b.path())
        .expect("create B");
    let workspace_b_id = workspace_b["workspace_id"].as_str().unwrap().to_owned();
    let assigned = harness
        .assign(session.id.as_str(), &workspace_b_id)
        .expect("assign");
    assert_eq!(assigned["changed"], true);
    let deleted = harness
        .backend
        .workspace_delete(&WorkspaceDeleteRequest {
            workspace_id: workspace_id.clone(),
        })
        .expect("delete");
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["disk_untouched"], true);
    assert!(marker.is_file(), "只删行、不删磁盘文件（ADR-026 §2.1-1）");
    assert_eq!(harness.list()["workspaces"].as_array().unwrap().len(), 1);

    // 查无 → workspace_not_found。
    let missing = harness
        .backend
        .workspace_delete(&WorkspaceDeleteRequest { workspace_id })
        .expect_err("查无");
    assert_eq!(missing.code.as_str(), "workspace_not_found");

    let actions = harness.audit_actions();
    assert!(actions.contains(&"workspace.deleted".to_owned()));
    assert!(actions.contains(&"session.workspace_assigned".to_owned()));
}

/// DoD2/DoD6（核心断言）：换根仅新会话生效；改派不重算基准；记忆注入源切换。
#[test]
fn reassign_switches_memory_but_keeps_creation_baseline() {
    let harness = harness();
    // 缺省工作区绑定 A（workspace_set）→ 新会话基准 = A。
    let bound = harness.set_workspace(harness.workspace_a.path());
    let workspace_a = bound["workspace_id"].as_str().unwrap().to_owned();
    let session = harness.create_session(None);
    assert_eq!(
        session.workspace_id.as_ref().map(WorkspaceId::as_str),
        Some(workspace_a.as_str())
    );
    assert!(
        session
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("A 区约定")),
        "会话创建注入 A 区记忆"
    );
    assert_eq!(
        harness.service.session_baseline_root(&session.id),
        Some(PathBuf::from(plain(harness.workspace_a.path()))),
        "创建时固定基准 = A"
    );
    assert_eq!(
        harness.session(session.id.as_str()).config["aether"]["workspace_baseline_root"],
        plain(harness.workspace_a.path()),
        "基准快照持久化到 sessions.config（跨重启不变）"
    );

    // 绑定 B（workspace_set 换根）→ 既有会话仍保持 A 基准（R15）；B 行已由绑定创建。
    let bound_b = harness.set_workspace(harness.workspace_b.path());
    let workspace_b_id = bound_b["workspace_id"].as_str().unwrap().to_owned();
    assert_eq!(
        harness.service.session_baseline_root(&session.id),
        Some(PathBuf::from(plain(harness.workspace_a.path()))),
        "workspace_set 换根不得重算既有会话基准"
    );

    // 改派到 B：记忆注入切换、权限基准不重算。
    let assigned = harness
        .assign(session.id.as_str(), &workspace_b_id)
        .expect("assign");
    assert_eq!(assigned["changed"], true);
    assert_eq!(assigned["baseline_unchanged"], true);
    assert_eq!(
        assigned["baseline_root"].as_str(),
        Some(plain(harness.workspace_a.path()).as_str()),
        "改派不重算基准（仍为创建时 A）"
    );
    assert_eq!(assigned["notice"], "权限基准不变，如需变更请新建会话");
    let after = harness.session(session.id.as_str());
    assert_eq!(
        after.workspace_id.as_ref().map(WorkspaceId::as_str),
        Some(workspace_b_id.as_str())
    );
    assert!(
        after
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("B 区约定") && !prompt.contains("A 区约定")),
        "记忆注入源即时切换（新 run 生效；既有预设块保留）"
    );
    assert_eq!(
        after.config["aether"]["workspace_baseline_root"],
        plain(harness.workspace_a.path()),
        "改派不改写基准快照"
    );

    // 权限判定：A 内写 = allow（基准内）；B 内写 = ask（基准外）→ deny。
    let allow = harness.request_now(
        "m7-08-a",
        &session.id,
        "fs.write",
        "write",
        &harness.workspace_a.path().join("inside-a.txt"),
    );
    assert!(allow.is_allowed(), "基准内写 allow（改派后原基准仍生效）");
    assert!(harness.service.pending_list(None).is_empty());
    let denied = harness.ask_and_deny(
        "m7-08-b",
        &session.id,
        "fs.write",
        "write",
        &harness.workspace_b.path().join("inside-b.txt"),
    );
    assert!(!denied.is_allowed(), "基准外写进入审批（deny）");

    // 新会话（C 绑定）基准 = C；旧会话仍 A（不因换根漂移）。
    harness.set_workspace(harness.workspace_c.path());
    let new_session = harness.create_session(None);
    assert_eq!(
        harness.service.session_baseline_root(&new_session.id),
        Some(PathBuf::from(plain(harness.workspace_c.path())))
    );
    assert_eq!(
        harness.service.session_baseline_root(&session.id),
        Some(PathBuf::from(plain(harness.workspace_a.path())))
    );
    let new_allow = harness.request_now(
        "m7-08-c",
        &new_session.id,
        "fs.write",
        "write",
        &harness.workspace_c.path().join("inside-c.txt"),
    );
    assert!(new_allow.is_allowed(), "新会话基准 = 换根后的 C");
    let old_ask = harness.ask_and_deny(
        "m7-08-c-old",
        &session.id,
        "fs.write",
        "write",
        &harness.workspace_c.path().join("inside-c-old.txt"),
    );
    assert!(!old_ask.is_allowed(), "旧会话不得静默切到新基准");

    // 跨重启：新权限服务（同一读池）从未登记该会话 → 从 sessions.config 快照懒加载 A。
    let restarted = PermissionService::new(
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
        Arc::new(SystemClock),
        PolicyEngine::new(harness.dir.path()).expect("数据目录基准"),
        harness.write.clone(),
        harness.reads.clone(),
        harness.pipeline.clone(),
    );
    let restart_allow = harness
        .runtime
        .block_on(restarted.request(harness.permission(
            "m7-08-restart-a",
            &session.id,
            "fs.write",
            "write",
            &plain_join(&harness.workspace_a.path().join("restart-a.txt")),
        )))
        .expect("restart request");
    assert!(
        restart_allow.is_allowed(),
        "重启后从 config 快照恢复创建时基准 A"
    );
}

/// DoD2：改派前置（在途 run / waiting_permission / running → session_busy）。
#[test]
fn assign_is_rejected_while_session_is_busy() {
    let harness = harness();
    let workspace_a = harness
        .create_workspace("A", harness.workspace_a.path())
        .expect("create A");
    let workspace_b = harness
        .create_workspace("B", harness.workspace_b.path())
        .expect("create B");
    let workspace_a_id = workspace_a["workspace_id"].as_str().unwrap().to_owned();
    let workspace_b_id = workspace_b["workspace_id"].as_str().unwrap().to_owned();
    let session = harness.create_session(Some(workspace_a_id.clone()));

    // running 状态 → busy。
    harness.set_session_status(session.id.as_str(), SessionStatus::Running);
    let busy = harness
        .assign(session.id.as_str(), &workspace_b_id)
        .expect_err("running 必须拒绝");
    assert_eq!(busy.code.as_str(), "invalid_value");
    assert!(busy.message.contains("session_busy"));

    // waiting_permission → busy。
    harness.set_session_status(session.id.as_str(), SessionStatus::WaitingPermission);
    let waiting = harness
        .assign(session.id.as_str(), &workspace_b_id)
        .expect_err("waiting_permission 必须拒绝");
    assert!(waiting.message.contains("session_busy"));

    // 空闲但存在 queued run → busy。
    harness.set_session_status(session.id.as_str(), SessionStatus::Idle);
    harness.insert_run(
        "01J00000000000000000000R1",
        session.id.as_str(),
        RunStatus::Queued,
    );
    let queued = harness
        .assign(session.id.as_str(), &workspace_b_id)
        .expect_err("queued run 必须拒绝");
    assert!(queued.message.contains("session_busy"));

    // run 终态 + idle → 允许；未改派时重复请求幂等（changed=false）。
    harness
        .runtime
        .block_on(harness.write.execute(StoreCommand::FinishRun {
            run_id: RunId::new("01J00000000000000000000R1".to_owned()).unwrap(),
            status: RunStatus::Cancelled,
            error: None,
            finished_at: 3,
        }))
        .unwrap();
    let unchanged = harness
        .assign(session.id.as_str(), &workspace_a_id)
        .expect("同工作区幂等");
    assert_eq!(unchanged["changed"], false);
    assert_eq!(unchanged["baseline_unchanged"], true);

    // 未知工作区/会话 → 结构化错误。
    let missing_workspace = harness
        .assign(session.id.as_str(), "01J0000000000000000000W99")
        .expect_err("未知工作区");
    assert_eq!(missing_workspace.code.as_str(), "workspace_not_found");
    let missing_session = harness
        .assign("01J00000000000000000000S99", &workspace_b_id)
        .expect_err("未知会话");
    assert!(missing_session.message.contains("session_not_found"));
}

/// DoD1/DoD4：DTO 参数校验（名称形态、ULID、未知字段拒绝）。
#[test]
fn dto_validation_matrix_for_workspace_commands() {
    use aether_tauri::ipc::validate::CommandRequest;

    let long_name = "n".repeat(65);
    let cases: Vec<(&str, Result<(), aether_tauri::ipc::error::IpcError>)> = vec![
        (
            "too_large",
            WorkspaceCreateRequest {
                name: long_name,
                root_path: "C:/ws".to_owned(),
            }
            .validate(),
        ),
        (
            "invalid_format",
            WorkspaceCreateRequest {
                name: " 前后空白 ".to_owned(),
                root_path: "C:/ws".to_owned(),
            }
            .validate(),
        ),
        (
            "invalid_format",
            WorkspaceUpdateRequest {
                workspace_id: "not-a-ulid".to_owned(),
                name: "ok".to_owned(),
            }
            .validate(),
        ),
        (
            "invalid_format",
            SessionWorkspaceAssignRequest {
                session_id: "not-a-ulid".to_owned(),
                workspace_id: "01J0000000000000000000W01".to_owned(),
            }
            .validate(),
        ),
        (
            "invalid_format",
            WorkspaceDeleteRequest {
                workspace_id: "x".to_owned(),
            }
            .validate(),
        ),
    ];
    for (expected, validation) in cases {
        let error = validation.expect_err("必须拒绝");
        assert_eq!(error.code.as_str(), expected, "错误码 {}", error.message);
    }

    // 未知字段（serde deny_unknown_fields）→ unknown_field。
    let unknown = serde_json::from_value::<WorkspaceCreateRequest>(serde_json::json!({
        "name": "x",
        "root_path": "C:/ws",
        "unexpected": 1
    }))
    .expect_err("未知字段");
    assert!(unknown.to_string().contains("unknown field"));
}

/// DoD1：`workspaces_list` 计数随会话在途状态变化（active/total）。
#[test]
fn workspace_counts_reflect_active_sessions() {
    let harness = harness();
    let workspace_a = harness
        .create_workspace("A", harness.workspace_a.path())
        .expect("create A");
    let workspace_a_id = workspace_a["workspace_id"].as_str().unwrap().to_owned();
    let session = harness.create_session(Some(workspace_a_id.clone()));

    let listed = harness.list();
    let entry = listed["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["workspace_id"] == workspace_a_id)
        .cloned()
        .expect("工作区条目");
    assert_eq!(entry["active_sessions"].as_u64(), Some(0));
    assert_eq!(entry["total_sessions"].as_u64(), Some(1));

    harness.set_session_status(session.id.as_str(), SessionStatus::Running);
    let listed = harness.list();
    let entry = listed["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["workspace_id"] == workspace_a_id)
        .cloned()
        .unwrap();
    assert_eq!(entry["active_sessions"].as_u64(), Some(1));
}
