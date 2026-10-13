//! M6-01 tauri 命令面集成：`session_permission_mode_set` 与 `rules_*` 五命令
//! （ADR-018 附录 A-1 校验矩阵、`rule_not_found`、规则变更刷新判定快照 + 审计动作）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use aether_control::{
    EventPipeline, ManualClock, PermissionConfig, PermissionService, PipelineConfig,
    StartupSelfCheckReport, StoreEventSource, StoreJournal,
};
use aether_core::{
    PermissionDecision, PermissionMode, Runtime, RuntimeId, RuntimeStatus, Session, SessionId,
    SessionStatus, TokenUsage,
};
use aether_security::PolicyEngine;
use aether_store::{StoreCommand, StoreRuntime, WriteQueueConfig};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    RuleAddRequest, RuleDeleteRequest, RuleToggleRequest, RuleUpdateRequest,
    SessionPermissionModeSetRequest,
};
use aether_tauri::session_backend::SessionBackend;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::runtime::Handle;

const SESSION_A: &str = "01J0000000000000000000000A";

struct Harness {
    _temp: TempDir,
    storage: StoreRuntime,
    pipeline: EventPipeline,
    workspace: TempDir,
    service: PermissionService,
    backend: Arc<dyn IpcBackend>,
}

impl Harness {
    async fn open() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("aether.db");
        let storage =
            StoreRuntime::open(&db_path, WriteQueueConfig::default(), &Handle::current()).unwrap();
        let pipeline = EventPipeline::start(
            PipelineConfig {
                persist_retry_delay: Duration::ZERO,
                ..PipelineConfig::default()
            },
            Arc::new(StoreJournal::new(storage.queue().clone())),
            Arc::new(StoreEventSource::new(storage.reads().clone())),
            &StartupSelfCheckReport::passing(4 * 1024 * 1024 * 1024),
            &Handle::current(),
        )
        .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let service = PermissionService::new(
            PermissionConfig {
                wait_timeout: None,
                ..PermissionConfig::default()
            },
            Arc::new(ManualClock::new(1_700_000_000_000)),
            PolicyEngine::new(workspace.path()).unwrap(),
            storage.queue().clone(),
            storage.reads().clone(),
            pipeline.clone(),
        );
        let backend: Arc<dyn IpcBackend> = Arc::new(
            SessionBackend::new(
                Arc::new(NotImplementedBackend),
                None,
                None,
                Some(storage.reads().clone()),
                None,
                Handle::current(),
            )
            .with_permissions(service.clone())
            .with_workspace_store(storage.queue().clone()),
        );
        Self {
            _temp: temp,
            storage,
            pipeline,
            workspace,
            service,
            backend,
        }
    }

    async fn insert_session(&self, session_id: &str) {
        let runtime = Runtime {
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
            created_at: 1,
            updated_at: 1,
        };
        self.storage
            .queue()
            .execute(StoreCommand::EnsureRuntime { runtime })
            .await
            .unwrap();
        let session = Session {
            id: SessionId::new(session_id).unwrap(),
            runtime_id: RuntimeId::new("mock").unwrap(),
            workspace_id: None,
            parent_session_id: None,
            title: "m6-01".to_owned(),
            status: SessionStatus::Idle,
            model: None,
            thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
            permission_mode: PermissionMode::WorkspaceWrite,
            system_prompt: None,
            config: json!({}),
            token_usage: TokenUsage::default(),
            created_at: 1,
            updated_at: 1,
            closed_at: None,
        };
        self.storage
            .queue()
            .execute(StoreCommand::InsertSession { session })
            .await
            .unwrap();
    }

    async fn shutdown(self) {
        self.pipeline.shutdown().await.unwrap();
        self.storage.shutdown().await.unwrap();
    }
}

fn rule_add_request(name: &str, target: Option<&str>) -> RuleAddRequest {
    RuleAddRequest {
        name: name.to_owned(),
        resource: "fs.write".to_owned(),
        action: "write".to_owned(),
        target_kind: if target.is_some() { "path" } else { "any" }.to_owned(),
        target: target.map(str::to_owned),
        decision: "deny".to_owned(),
        priority: Some(5),
    }
}

/// DoD4：规则五命令全链路 + 审计动作 + 快照刷新；`rule_not_found` 稳定码。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rules_commands_full_cycle() {
    let harness = Harness::open().await;
    let target = harness
        .workspace
        .path()
        .join("rule-target.txt")
        .to_string_lossy()
        .to_string();

    // 空清单。
    let value = harness.backend.rules_list().unwrap();
    assert_eq!(value, Value::Array(vec![]));

    // 新增（path 目标 canonicalize）。
    let added = harness
        .backend
        .rule_add(&rule_add_request("deny-target", Some(&target)))
        .unwrap();
    let rule_id = added["id"].as_str().unwrap().to_owned();
    assert_eq!(added["decision"], "deny");
    assert_eq!(added["enabled"], true);
    assert_eq!(added["priority"], 5);
    assert!(added["target"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));

    // 清单 + 快照刷新（命令后规则立即参与判定）。
    let listed = harness.backend.rules_list().unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(harness.service.rules_snapshot().len(), 1);

    // 启停。
    let toggled = harness
        .backend
        .rule_toggle(&RuleToggleRequest {
            rule_id: rule_id.clone(),
            enabled: false,
        })
        .unwrap();
    assert_eq!(toggled["enabled"], false);
    assert_eq!(
        harness.backend.rules_list().unwrap().as_array().unwrap()[0]["enabled"],
        false
    );

    // 更新（name + decision + priority；resource 不变可不带 action）。
    let updated = harness
        .backend
        .rule_update(&RuleUpdateRequest {
            rule_id: rule_id.clone(),
            name: Some("deny-target-v2".to_owned()),
            resource: None,
            action: None,
            target_kind: None,
            target: None,
            decision: Some("allow".to_owned()),
            priority: Some(9),
        })
        .unwrap();
    assert_eq!(updated["name"], "deny-target-v2");
    assert_eq!(updated["decision"], "allow");
    assert_eq!(updated["priority"], 9);

    // resource 变更未带 action → invalid_value。
    let error = harness
        .backend
        .rule_update(&RuleUpdateRequest {
            rule_id: rule_id.clone(),
            name: None,
            resource: Some("exec".to_owned()),
            action: None,
            target_kind: None,
            target: None,
            decision: None,
            priority: None,
        })
        .unwrap_err();
    assert_eq!(error.code.as_str(), "invalid_value");

    // 删除 + 审计 + 不存在 → rule_not_found。
    harness
        .backend
        .rule_delete(&RuleDeleteRequest {
            rule_id: rule_id.clone(),
        })
        .unwrap();
    assert_eq!(harness.backend.rules_list().unwrap(), Value::Array(vec![]));
    let error = harness
        .backend
        .rule_delete(&RuleDeleteRequest {
            rule_id: rule_id.clone(),
        })
        .unwrap_err();
    assert_eq!(error.code.as_str(), "rule_not_found");
    let error = harness
        .backend
        .rule_toggle(&RuleToggleRequest {
            rule_id: "01J0000000000000000000ZZZZ".to_owned(),
            enabled: true,
        })
        .unwrap_err();
    assert_eq!(error.code.as_str(), "rule_not_found");

    // 审计动作齐备（rule.created/updated/toggled/deleted）。
    let records = harness.storage.reads().audit_log(100).await.unwrap();
    let actions: Vec<&str> = records.iter().map(|r| r.action.as_str()).collect();
    for expected in [
        "rule.created",
        "rule.updated",
        "rule.toggled",
        "rule.deleted",
    ] {
        assert!(
            actions.contains(&expected),
            "缺少审计动作 {expected}: {actions:?}"
        );
    }
    harness.shutdown().await;
}

/// DoD4：非法规则拒绝且不落库（枚举/组合/形态矩阵）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rule_add_rejects_invalid_forms_without_persisting() {
    let harness = Harness::open().await;

    // resource × target_kind 不兼容（DTO 层 invalid_enum + field=target_kind）。
    let mut request = rule_add_request("bad-combo", None);
    request.target_kind = "host".to_owned();
    let error = harness.backend.rule_add(&request).unwrap_err();
    assert_eq!(error.code.as_str(), "invalid_enum");
    assert_eq!(error.field.as_deref(), Some("target_kind"));

    // action 与 resource 不匹配（后端 invalid_value）。
    let mut request = rule_add_request("bad-action", None);
    request.action = "read".to_owned();
    let error = harness.backend.rule_add(&request).unwrap_err();
    assert_eq!(error.code.as_str(), "invalid_value");

    // path 目标形态非法（UNC → canonicalize_free_path 拒绝）。
    let mut request = rule_add_request("bad-path", Some(r"\\server\share\secret.txt"));
    request.target_kind = "path".to_owned();
    let error = harness.backend.rule_add(&request).unwrap_err();
    assert_eq!(error.code.as_str(), "invalid_value");

    // command 目标含空白（形态非法）。
    let mut request = rule_add_request("bad-command", Some("rm -rf"));
    request.resource = "exec".to_owned();
    request.action = "exec".to_owned();
    request.target_kind = "command".to_owned();
    let error = harness.backend.rule_add(&request).unwrap_err();
    assert_eq!(error.code.as_str(), "invalid_value");

    // 全部拒绝：清单与判定快照均保持空（不落库）。
    assert_eq!(harness.backend.rules_list().unwrap(), Value::Array(vec![]));
    assert!(harness.service.rules_snapshot().is_empty());
    harness.shutdown().await;
}

/// DoD4/DoD8⑧：`session_permission_mode_set` 命令（回显 + 落库 + 审计 + 错误路径）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_permission_mode_set_command() {
    let harness = Harness::open().await;
    harness.insert_session(SESSION_A).await;

    let value = harness
        .backend
        .session_permission_mode_set(&SessionPermissionModeSetRequest {
            session_id: SESSION_A.to_owned(),
            permission_mode: "danger_full_access".to_owned(),
        })
        .unwrap();
    assert_eq!(value["session_id"], SESSION_A);
    assert_eq!(value["permission_mode"], "danger_full_access");
    assert_eq!(value["previous_permission_mode"], "workspace_write");
    let row = harness
        .storage
        .reads()
        .session(&SessionId::new(SESSION_A).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.permission_mode, PermissionMode::DangerFullAccess);

    // 审计：session.permission_mode_changed（actor=user，resource=session）。
    let records = harness.storage.reads().audit_log(100).await.unwrap();
    let changed = records
        .iter()
        .find(|record| record.action == "session.permission_mode_changed")
        .expect("档位变更必须落审计");
    assert_eq!(changed.actor, "user");
    assert_eq!(changed.resource.as_deref(), Some("session"));
    let detail: Value = serde_json::from_str(changed.detail.as_deref().unwrap()).unwrap();
    assert_eq!(detail["from"], "workspace_write");
    assert_eq!(detail["to"], "danger_full_access");

    // 非法枚举 → invalid_enum。
    let error = harness
        .backend
        .session_permission_mode_set(&SessionPermissionModeSetRequest {
            session_id: SESSION_A.to_owned(),
            permission_mode: "full_access".to_owned(),
        })
        .unwrap_err();
    assert_eq!(error.code.as_str(), "invalid_enum");

    // 会话不存在 → invalid_value + 稳定业务码 session_not_found。
    let error = harness
        .backend
        .session_permission_mode_set(&SessionPermissionModeSetRequest {
            session_id: "01J0000000000000000000ZZZZ".to_owned(),
            permission_mode: "read_only".to_owned(),
        })
        .unwrap_err();
    assert_eq!(error.code.as_str(), "invalid_value");
    assert!(error.message.contains("session_id 不存在"));
    harness.shutdown().await;
}

/// DoD5：命令侧新增 deny 规则后，判定路径立即收紧（快照刷新断言）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rule_add_command_updates_decision_snapshot() {
    let harness = Harness::open().await;
    harness.insert_session(SESSION_A).await;
    let session_id = SessionId::new(SESSION_A).unwrap();
    let inside = harness
        .workspace
        .path()
        .join("snapshot.txt")
        .to_string_lossy()
        .to_string();

    // 缺省档区内写 before：allow。
    let before = harness
        .service
        .request(aether_control::PermissionRequest {
            request_id: "m6-01-snapshot-before".to_owned(),
            session_id: Some(session_id.clone()),
            runtime_id: Some(RuntimeId::new("mock").unwrap()),
            resource: "fs.write".to_owned(),
            action: "write".to_owned(),
            target: Some(inside.clone()),
            content_bytes: Some(5),
        })
        .await
        .unwrap();
    assert!(before.is_allowed());

    // 命令侧新增 deny any 规则。
    harness
        .backend
        .rule_add(&rule_add_request("tighten-all", None))
        .unwrap();

    // after：deny（规则快照已刷新）。
    let after = harness
        .service
        .request(aether_control::PermissionRequest {
            request_id: "m6-01-snapshot-after".to_owned(),
            session_id: Some(session_id),
            runtime_id: Some(RuntimeId::new("mock").unwrap()),
            resource: "fs.write".to_owned(),
            action: "write".to_owned(),
            target: Some(inside),
            content_bytes: Some(5),
        })
        .await
        .unwrap();
    assert_eq!(after.decision, PermissionDecision::Deny);
    harness.shutdown().await;
}
