//! M7-08 存储层：工作区管理写路径与计数读路径（ADR-026 §2.1-1/4）。
//!
//! 覆盖：
//! - `workspaces_list` 计数口径（`active` = 在途会话 ∪ queued/running run；`total` 含终态；
//!   未分配会话归入合成组）；
//! - 工作区改名（仅 name；`root_path` 不可变）、删除（引用检查在事务内；存在引用 →
//!   `WorkspaceInUse` 且**不删除**）、无引用删除成功；
//! - 会话改派原子写（`workspace_id` + `config` 基准快照 + `system_prompt`）；
//! - `workspaces.memory_files` P1 恒空数组（ADR-026 §2.1-4）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aether_core::{
    PermissionMode, Run, RunId, RunStatus, Runtime, RuntimeId, RuntimeStatus, Session, SessionId,
    SessionStatus, TokenUsage, Workspace, WorkspaceId, THINKING_DEPTH_DEFAULT,
};
use aether_store::{
    StoreCommand, StoreOutcome, StoreRuntime, WorkspaceSessionCount, WriteQueueConfig,
};
use tokio::runtime::Handle;

fn runtime_row() -> Runtime {
    Runtime {
        id: RuntimeId::new("mock").unwrap(),
        name: "Mock".to_owned(),
        kind: "mock".to_owned(),
        version: "0.1.0".to_owned(),
        protocol: "1.0".to_owned(),
        capabilities: vec![],
        endpoint: None,
        config: serde_json::json!({}),
        status: RuntimeStatus::Ready,
        status_reason: None,
        last_seen_at: None,
        created_at: 1,
        updated_at: 1,
    }
}

fn workspace_row(id: &str, name: &str, root: &str) -> Workspace {
    Workspace {
        id: WorkspaceId::new(id).unwrap(),
        name: name.to_owned(),
        root_path: root.to_owned(),
        memory_files: Vec::new(),
        created_at: 10,
        updated_at: 20,
    }
}

fn session_row(id: &str, workspace_id: Option<&str>, status: SessionStatus) -> Session {
    Session {
        id: SessionId::new(id).unwrap(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: workspace_id.map(|value| WorkspaceId::new(value).unwrap()),
        parent_session_id: None,
        title: "t".to_owned(),
        status,
        model: None,
        thinking_depth: THINKING_DEPTH_DEFAULT,
        permission_mode: PermissionMode::default(),
        preset_id: None,
        system_prompt: None,
        config: serde_json::json!({}),
        token_usage: TokenUsage::default(),
        created_at: 1,
        updated_at: 1,
        closed_at: None,
    }
}

fn run_row(id: &str, session_id: &str, status: RunStatus) -> Run {
    Run {
        id: RunId::new(id).unwrap(),
        session_id: SessionId::new(session_id).unwrap(),
        status,
        input_message_id: None,
        thinking_depth: None,
        error: None,
        started_at: 1,
        finished_at: None,
    }
}

fn open_runtime(path: &std::path::Path) -> StoreRuntime {
    StoreRuntime::open(path, WriteQueueConfig::default(), &Handle::current()).unwrap()
}

async fn seed(runtime: &StoreRuntime) {
    runtime
        .queue()
        .execute(StoreCommand::EnsureRuntime {
            runtime: runtime_row(),
        })
        .await
        .unwrap();
    for workspace in [
        workspace_row("01J0000000000000000000W01", "A", "C:/ws/a"),
        workspace_row("01J0000000000000000000W02", "B", "C:/ws/b"),
    ] {
        runtime
            .queue()
            .execute(StoreCommand::UpsertWorkspace { workspace })
            .await
            .unwrap();
    }
}

/// DoD2/DoD5：计数口径（active/total + 未分配合成组）。
#[tokio::test(flavor = "multi_thread")]
async fn workspace_session_counts_follow_in_flight_definition() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&dir.path().join("aether.db"));
    seed(&runtime).await;

    // A：running（active）+ idle（total）；B：idle（total）；未分配：queued run 计 active。
    for session in [
        session_row(
            "01J00000000000000000000S1",
            Some("01J0000000000000000000W01"),
            SessionStatus::Running,
        ),
        session_row(
            "01J00000000000000000000S2",
            Some("01J0000000000000000000W01"),
            SessionStatus::Idle,
        ),
        session_row(
            "01J00000000000000000000S3",
            Some("01J0000000000000000000W02"),
            SessionStatus::Idle,
        ),
        session_row("01J00000000000000000000S4", None, SessionStatus::Idle),
        session_row("01J00000000000000000000S5", None, SessionStatus::Completed),
    ] {
        runtime
            .queue()
            .execute(StoreCommand::InsertSession { session })
            .await
            .unwrap();
    }
    runtime
        .queue()
        .execute(StoreCommand::InsertRun {
            run: run_row(
                "01J00000000000000000000R1",
                "01J00000000000000000000S4",
                RunStatus::Queued,
            ),
        })
        .await
        .unwrap();

    let counts = runtime.reads().workspace_session_counts().await.unwrap();
    assert_eq!(
        counts.by_workspace.get("01J0000000000000000000W01"),
        Some(&WorkspaceSessionCount {
            active: 1,
            total: 2
        }),
        "A：1 running + 1 idle"
    );
    assert_eq!(
        counts.by_workspace.get("01J0000000000000000000W02"),
        Some(&WorkspaceSessionCount {
            active: 0,
            total: 1
        }),
        "B：仅 idle（不计在途）"
    );
    assert_eq!(
        counts.unassigned,
        WorkspaceSessionCount {
            active: 1,
            total: 2
        },
        "未分配：queued run 计在途；completed 仅计总数"
    );
    assert_eq!(
        runtime.reads().workspaces().await.unwrap().len(),
        2,
        "合成组不建 workspaces 行"
    );
}

/// DoD3：改名只动 name；root_path 不可变；查无影响 0 行。
#[tokio::test(flavor = "multi_thread")]
async fn rename_workspace_only_changes_name() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&dir.path().join("aether.db"));
    seed(&runtime).await;

    let outcome = runtime
        .queue()
        .execute(StoreCommand::RenameWorkspace {
            id: WorkspaceId::new("01J0000000000000000000W01").unwrap(),
            name: "A2".to_owned(),
            updated_at: 99,
        })
        .await
        .unwrap();
    assert_eq!(outcome, StoreOutcome::Applied { affected: 1 });
    let workspace = runtime
        .reads()
        .workspace(&WorkspaceId::new("01J0000000000000000000W01").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(workspace.name, "A2");
    assert_eq!(
        workspace.root_path, "C:/ws/a",
        "root_path 不可变（防基准漂移）"
    );
    assert_eq!(workspace.updated_at, 99);
    assert_eq!(workspace.created_at, 10, "created_at 不变");
    assert!(
        workspace.memory_files.is_empty(),
        "P1：memory_files 恒空数组"
    );

    // 查无 → 影响 0 行（命令层映射 workspace_not_found）。
    let outcome = runtime
        .queue()
        .execute(StoreCommand::RenameWorkspace {
            id: WorkspaceId::new("01J0000000000000000000W09").unwrap(),
            name: "X".to_owned(),
            updated_at: 100,
        })
        .await
        .unwrap();
    assert_eq!(outcome, StoreOutcome::Applied { affected: 0 });
}

/// DoD3：存在会话引用 → `WorkspaceInUse`（事务内复核；不删除）；引用清零后可删除。
#[tokio::test(flavor = "multi_thread")]
async fn delete_workspace_refuses_when_sessions_reference_it() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&dir.path().join("aether.db"));
    seed(&runtime).await;
    runtime
        .queue()
        .execute(StoreCommand::InsertSession {
            session: session_row(
                "01J00000000000000000000S1",
                Some("01J0000000000000000000W01"),
                SessionStatus::Idle,
            ),
        })
        .await
        .unwrap();

    let outcome = runtime
        .queue()
        .execute(StoreCommand::DeleteWorkspace {
            id: WorkspaceId::new("01J0000000000000000000W01").unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(outcome, StoreOutcome::WorkspaceInUse { sessions: 1 });
    assert!(
        runtime
            .reads()
            .workspace(&WorkspaceId::new("01J0000000000000000000W01").unwrap())
            .await
            .unwrap()
            .is_some(),
        "存在引用时不得删除"
    );

    // 改派到 B 后（引用转移），A 无引用 → 删除成功。
    runtime
        .queue()
        .execute(StoreCommand::AssignSessionWorkspace {
            session_id: SessionId::new("01J00000000000000000000S1").unwrap(),
            workspace_id: WorkspaceId::new("01J0000000000000000000W02").unwrap(),
            config: serde_json::json!({"aether": {"workspace_baseline_root": "C:/ws/a"}}),
            system_prompt: Some("prompt".to_owned()),
            updated_at: 200,
        })
        .await
        .unwrap();
    let outcome = runtime
        .queue()
        .execute(StoreCommand::DeleteWorkspace {
            id: WorkspaceId::new("01J0000000000000000000W01").unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(outcome, StoreOutcome::Applied { affected: 1 });
    assert!(runtime
        .reads()
        .workspace(&WorkspaceId::new("01J0000000000000000000W01").unwrap())
        .await
        .unwrap()
        .is_none());
    // 改派后的会话仍在（不级联删除会话）。
    let session = runtime
        .reads()
        .session(&SessionId::new("01J00000000000000000000S1").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        session.workspace_id,
        Some(WorkspaceId::new("01J0000000000000000000W02").unwrap())
    );
}

/// DoD2：改派原子写 `workspace_id` + `config`（基准快照）+ `system_prompt`。
#[tokio::test(flavor = "multi_thread")]
async fn assign_session_workspace_updates_all_three_fields_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = open_runtime(&dir.path().join("aether.db"));
    seed(&runtime).await;
    runtime
        .queue()
        .execute(StoreCommand::InsertSession {
            session: session_row(
                "01J00000000000000000000S1",
                Some("01J0000000000000000000W01"),
                SessionStatus::Idle,
            ),
        })
        .await
        .unwrap();

    let config = serde_json::json!({
        "native_id": "n-1",
        "aether": { "workspace_baseline_root": "C:/ws/a" }
    });
    let outcome = runtime
        .queue()
        .execute(StoreCommand::AssignSessionWorkspace {
            session_id: SessionId::new("01J00000000000000000000S1").unwrap(),
            workspace_id: WorkspaceId::new("01J0000000000000000000W02").unwrap(),
            config: config.clone(),
            system_prompt: Some(
                "# 工作区约定（记忆文件注入，优先级低于系统提示）\n来源：AGENTS.md\n\nB".to_owned(),
            ),
            updated_at: 300,
        })
        .await
        .unwrap();
    assert_eq!(outcome, StoreOutcome::Applied { affected: 1 });

    let session = runtime
        .reads()
        .session(&SessionId::new("01J00000000000000000000S1").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        session.workspace_id,
        Some(WorkspaceId::new("01J0000000000000000000W02").unwrap())
    );
    assert_eq!(
        session.config, config,
        "config 整体写入（基准快照 + 适配器键）"
    );
    assert_eq!(session.updated_at, 300);
    assert!(
        session
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("来源：AGENTS.md")),
        "记忆注入源切换后的提示词"
    );

    // 查无会话 → 影响 0 行。
    let outcome = runtime
        .queue()
        .execute(StoreCommand::AssignSessionWorkspace {
            session_id: SessionId::new("01J00000000000000000000S9").unwrap(),
            workspace_id: WorkspaceId::new("01J0000000000000000000W02").unwrap(),
            config: serde_json::json!({}),
            system_prompt: None,
            updated_at: 301,
        })
        .await
        .unwrap();
    assert_eq!(outcome, StoreOutcome::Applied { affected: 0 });
}

/// DoD1/DoD4：写入工作区行时 `memory_files` 保持 `[]`（P1 不启用）。
#[tokio::test(flavor = "multi_thread")]
async fn workspace_memory_files_stay_disabled_in_p1() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("aether.db");
    let runtime = open_runtime(&db);
    seed(&runtime).await;
    let workspace = runtime
        .reads()
        .workspace(&WorkspaceId::new("01J0000000000000000000W01").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(workspace.memory_files.is_empty());
    // 行内原始 JSON 也必须是 `[]`（不是 `null`/缺省文本）。
    let connection = rusqlite::Connection::open(&db).unwrap();
    let raw: String = connection
        .query_row(
            "SELECT memory_files FROM workspaces WHERE id = '01J0000000000000000000W01'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raw, "[]");
}
