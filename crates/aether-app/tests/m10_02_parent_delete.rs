//! M10-02/ADR-032：父会话存在子会话时 `session_delete` 拒绝（真实 SessionBackend）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use aether_app::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_app::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_app::ipc::dto::SessionDeleteRequest;
use aether_app::ipc::BusinessCode;
use aether_app::session_backend::SessionBackend;
use aether_control::{
    ExecutorFuture, ExecutorOutcome, LifecycleConfig, RunExecutor, RunRequest, SessionManager,
    SystemClock,
};
use aether_core::{
    EventType, PermissionMode, Runtime, RuntimeId, RuntimeStatus, Session, SessionId,
    SessionStatus, TokenUsage,
};
use aether_store::StoreCommand;
use serde_json::json;
use tempfile::TempDir;

struct NoopExecutor;

impl RunExecutor for NoopExecutor {
    fn execute(&self, request: RunRequest) -> ExecutorFuture<'_> {
        Box::pin(async move {
            request.cancel.cancelled().await;
            ExecutorOutcome::Cancelled {
                reason: Some("noop".to_owned()),
            }
        })
    }
}

#[test]
fn parent_session_delete_is_rejected_when_children_exist() {
    let dir = TempDir::new().expect("tempdir");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()
        .expect("runtime");
    let handle = runtime.handle().clone();
    let CoreBoot {
        storage: _slot,
        reads,
        write,
        pipeline,
        ..
    } = boot_core_full(
        dir.path(),
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("boot core");
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline,
        Arc::new(NoopExecutor),
    );
    let backend = Arc::new(
        SessionBackend::new(
            Arc::new(NotImplementedBackend),
            Some(manager),
            None,
            Some(reads.clone()),
            None,
            handle.clone(),
        )
        .with_workspace_store(write.clone()),
    );

    let parent = SessionId::new("01JPARENTDELETE0000000000").unwrap();
    let child = SessionId::new("01JCHILDDELETE00000000000").unwrap();
    let now = 1_700_000_000_000i64;
    let runtime_row = Runtime {
        id: RuntimeId::new("mock").unwrap(),
        name: "Mock".to_owned(),
        kind: "mock".to_owned(),
        version: "0.1.0".to_owned(),
        protocol: "1.0".to_owned(),
        capabilities: Vec::new(),
        endpoint: None,
        config: json!({}),
        status: RuntimeStatus::Ready,
        status_reason: None,
        last_seen_at: None,
        created_at: now,
        updated_at: now,
    };
    let parent_session = Session {
        id: parent.clone(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: None,
        parent_session_id: None,
        title: "parent".to_owned(),
        status: SessionStatus::Idle,
        model: None,
        thinking_depth: 2,
        permission_mode: PermissionMode::WorkspaceWrite,
        preset_id: None,
        system_prompt: None,
        config: json!({}),
        token_usage: TokenUsage::default(),
        created_at: now,
        updated_at: now,
        closed_at: None,
    };
    let child_session = Session {
        id: child.clone(),
        runtime_id: RuntimeId::new("mock").unwrap(),
        workspace_id: None,
        parent_session_id: Some(parent.clone()),
        title: "child".to_owned(),
        status: SessionStatus::Idle,
        model: None,
        thinking_depth: 2,
        permission_mode: PermissionMode::WorkspaceWrite,
        preset_id: None,
        system_prompt: None,
        config: json!({}),
        token_usage: TokenUsage::default(),
        created_at: now,
        updated_at: now,
        closed_at: None,
    };
    runtime.block_on(async {
        write
            .execute(StoreCommand::EnsureRuntime {
                runtime: runtime_row,
            })
            .await
            .unwrap();
        write
            .execute(StoreCommand::InsertSession {
                session: parent_session,
            })
            .await
            .unwrap();
        write
            .execute(StoreCommand::InsertSession {
                session: child_session,
            })
            .await
            .unwrap();
    });

    let request = SessionDeleteRequest {
        session_id: parent.as_str().to_owned(),
    };
    let backend_for_call = Arc::clone(&backend);
    let error = runtime
        .block_on(async move {
            tokio::task::spawn_blocking(move || backend_for_call.session_delete(&request))
                .await
                .unwrap()
        })
        .expect_err("存在子会话必须拒绝删除");
    assert_eq!(
        error.business_code(),
        Some(BusinessCode::SessionHasChildren),
        "业务码必须是 session_has_children"
    );

    runtime.block_on(async {
        assert!(
            reads.session(&parent).await.unwrap().is_some(),
            "父会话不得被删除"
        );
        assert!(
            reads.session(&child).await.unwrap().is_some(),
            "子会话不得被级联删除"
        );
        let events = reads.events_latest(&parent, 20).await.unwrap();
        assert!(
            events
                .iter()
                .all(|event| event.payload.event_type() != EventType::SessionClosed),
            "拒绝删除不得产生 session.closed"
        );
        let audit = reads.audit_log(20).await.unwrap();
        assert!(
            audit.iter().any(|record| {
                record.action == "session.deleted"
                    && record.result.as_deref() == Some("denied")
                    && record
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.contains("has_children"))
            }),
            "应有 session.deleted denied + has_children 审计: {audit:?}"
        );
    });
}
