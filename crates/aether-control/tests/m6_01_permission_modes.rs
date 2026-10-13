//! M6-01 集成测试：会话级三档权限矩阵（ADR-014 附录 B）、用户自定义规则层与
//! `always` 持久授权（ADR-019）。
//!
//! 口径：矩阵为设计变更驱动（ADR-014 附录 B 替换 P0 固定矩阵）；
//! 本文件为 M6-01 DoD2/3/5/6/8 的可执行验证（P0 回归由 `m2_03_permission.rs` 承接）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::time::Duration;

use aether_control::{PermissionConfig, PermissionService};
use aether_core::{PermissionDecision, PermissionMode, PermissionScope};
use aether_store::{PermissionRuleRecord, StoreCommand};
use m2_support::{
    build_manager, build_permission_service, create_session, insert_session_row, manual_clock,
    outside_target, permission_request, set_session_permission_mode, wait_for,
    wait_permissions_pending, ScriptedExecutor, TestCore,
};

fn request_id(label: &str) -> String {
    format!("m6-01-{label}")
}

async fn audit_entries(core: &TestCore) -> Vec<aether_store::AuditLogRecord> {
    core.reads().audit_log(10_000).await.unwrap()
}

async fn allowed_by_policy_count(core: &TestCore) -> usize {
    audit_entries(core)
        .await
        .into_iter()
        .filter(|record| record.action == "permission.allowed_by_policy")
        .count()
}

fn rule_record(id: &str, target: Option<String>, decision: &str) -> PermissionRuleRecord {
    PermissionRuleRecord {
        id: id.to_owned(),
        name: format!("rule-{id}"),
        resource: "fs.write".to_owned(),
        action: "write".to_owned(),
        target_kind: if target.is_some() { "path" } else { "any" }.to_owned(),
        target,
        decision: decision.to_owned(),
        priority: 10,
        enabled: true,
        created_at: 100,
        updated_at: 100,
    }
}

async fn install_rule(core: &TestCore, service: &PermissionService, rule: PermissionRuleRecord) {
    core.write()
        .execute(StoreCommand::InsertPermissionRule { rule })
        .await
        .unwrap();
    service.restore_rules().await.unwrap();
}

/// DoD8①②③④：三档矩阵逐项（读写/exec/net/记忆）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod8_three_mode_matrix() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    let inside = workspace.path().join("inside.txt");
    std::fs::write(&inside, b"x").unwrap();
    let inside = inside.to_string_lossy().to_string();
    let outside_file = outside_target(&outside, "outside.txt");

    // ===== read_only：写全 deny（含记忆）、exec/net deny、区外读 allow =====
    let read_only = insert_session_row(&core, "01J00000000000000000000M01", None).await;
    set_session_permission_mode(&core, &read_only.id, PermissionMode::ReadOnly).await;
    for (resource, action, target, content) in [
        ("fs.write", "write", inside.as_str(), Some(10_u64)),
        ("fs.write", "write", outside_file.as_str(), Some(10)),
        ("exec", "exec", "npm", None),
        ("net", "connect", "example.com", None),
    ] {
        let resolution = service
            .request(permission_request(
                &request_id(&format!("ro-{resource}")),
                Some(&read_only.id),
                resource,
                action,
                Some(target),
                content,
            ))
            .await
            .unwrap();
        assert!(
            !resolution.is_allowed(),
            "read_only 档 {resource}:{action} 必须 deny"
        );
        assert!(
            service.pending_list(None).is_empty(),
            "deny 不得进入 pending"
        );
    }
    let read_outside = service
        .request(permission_request(
            &request_id("ro-read-out"),
            Some(&read_only.id),
            "fs.read",
            "read",
            Some(&outside_file),
            None,
        ))
        .await
        .unwrap();
    assert!(
        read_outside.is_allowed(),
        "read_only 档区外读 allow（不约束读）"
    );

    // ===== 缺省 workspace_write：区内写 allow；区外写 ask → once 授权 =====
    let default_mode = insert_session_row(&core, "01J00000000000000000000M02", None).await;
    let inside_write = service
        .request(permission_request(
            &request_id("ww-inside"),
            Some(&default_mode.id),
            "fs.write",
            "write",
            Some(&inside),
            Some(10),
        ))
        .await
        .unwrap();
    assert!(inside_write.is_allowed(), "缺省档区内写免逐次审批");
    assert!(service.pending_list(None).is_empty());

    let outside_b = outside_target(&outside, "outside-ask.txt");
    let service_clone = service.clone();
    let ask_target = outside_b.clone();
    let ask = tokio::spawn(async move {
        service_clone
            .request(permission_request(
                &request_id("ww-outside"),
                Some(&default_mode.id.clone()),
                "fs.write",
                "write",
                Some(&ask_target),
                Some(10),
            ))
            .await
            .unwrap()
    });
    assert!(
        wait_for(
            || !service.pending_list(None).is_empty(),
            Duration::from_secs(3)
        )
        .await,
        "缺省档区外写必须进入待审批（ask）"
    );
    let ticket = service.pending_list(None).first().unwrap().clone();
    service
        .resolve(
            &ticket.request_id,
            PermissionDecision::Allow,
            Some(PermissionScope::Once),
        )
        .await
        .unwrap();
    assert!(ask.await.unwrap().is_allowed(), "once 授权 allow");

    // ===== danger_full_access：区外读写/exec/net 放行且 100% 经网关 + 审计 =====
    let danger = insert_session_row(&core, "01J00000000000000000000M03", None).await;
    set_session_permission_mode(&core, &danger.id, PermissionMode::DangerFullAccess).await;
    let before_allowed = allowed_by_policy_count(&core).await;
    let exec = service
        .request(permission_request(
            &request_id("danger-exec"),
            Some(&danger.id),
            "exec",
            "exec",
            Some("npm --version"),
            None,
        ))
        .await
        .unwrap();
    assert!(exec.is_allowed(), "顶档 exec allow（仅门层）");
    let net = service
        .request(permission_request(
            &request_id("danger-net"),
            Some(&danger.id),
            "net",
            "connect",
            Some("example.com"),
            None,
        ))
        .await
        .unwrap();
    assert!(net.is_allowed(), "顶档 net allow");
    let write_out = service
        .request(permission_request(
            &request_id("danger-write"),
            Some(&danger.id),
            "fs.write",
            "write",
            Some(&outside_file),
            Some(5),
        ))
        .await
        .unwrap();
    assert!(write_out.is_allowed(), "顶档区外写 allow");
    let after_allowed = allowed_by_policy_count(&core).await;
    assert!(
        after_allowed >= before_allowed + 3,
        "顶档 allow 仍必须 100% 经网关并落审计（before={before_allowed} after={after_allowed}）"
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD8⑦/⑩：read_only 档记忆写入被拒 + 审计；判定入参 = 会话档位（按会话解析）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod8_read_only_memory_write_denied_with_audit() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    let session = insert_session_row(&core, "01J00000000000000000000M04", None).await;
    set_session_permission_mode(&core, &session.id, PermissionMode::ReadOnly).await;
    let memory = workspace.path().join("AGENTS.md");
    std::fs::write(&memory, b"x").unwrap();
    let resolution = service
        .request(permission_request(
            &request_id("ro-memory"),
            Some(&session.id),
            "fs.write",
            "write",
            Some(&memory.to_string_lossy()),
            Some(10),
        ))
        .await
        .unwrap();
    assert!(
        !resolution.is_allowed(),
        "read_only 档记忆写入 deny（不变量 10）"
    );
    let entries = audit_entries(&core).await;
    let denied = entries
        .iter()
        .find(|record| record.action == "permission.denied_by_policy")
        .expect("read_only 写拒绝必须落审计");
    assert!(denied
        .detail
        .as_deref()
        .is_some_and(|detail| detail.contains("read_only")));
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD8⑥：已 pending 票据不被档位变更追溯改写（按原决议收口）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod8_pending_ticket_not_rewritten_by_mode_change() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    let session = insert_session_row(&core, "01J00000000000000000000M05", None).await;
    let target = outside_target(&outside, "pending-target.txt");
    let service_clone = service.clone();
    let target_clone = target.clone();
    let session_clone = session.id.clone();
    let waiter = tokio::spawn(async move {
        service_clone
            .request(permission_request(
                &request_id("pending-mode-change"),
                Some(&session_clone),
                "fs.write",
                "write",
                Some(&target_clone),
                Some(5),
            ))
            .await
            .unwrap()
    });
    assert!(
        wait_for(
            || !service.pending_list(None).is_empty(),
            Duration::from_secs(3)
        )
        .await
    );
    // 档位降级：pending 票据不被追溯改写（UI 决议按原票据收口）。
    set_session_permission_mode(&core, &session.id, PermissionMode::ReadOnly).await;
    let ticket = service.pending_list(None).first().unwrap().clone();
    assert!(ticket.canonical_target.is_some());
    service
        .resolve(
            &ticket.request_id,
            PermissionDecision::Allow,
            Some(PermissionScope::Once),
        )
        .await
        .unwrap();
    let resolution = waiter.await.unwrap();
    assert!(
        resolution.is_allowed(),
        "已 pending 票据按原决议收口（不追溯改写）"
    );
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD8⑥ + DoD3：`always` 持久授权；降级 read_only 后既有授权不放大（不变量 9）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn always_grant_persists_and_downgrade_reevaluates() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    let session = insert_session_row(&core, "01J00000000000000000000M06", None).await;
    let target = outside_target(&outside, "always-target.txt");

    // 缺省档区外写 → ask → always 授权。
    let service_clone = service.clone();
    let target_clone = target.clone();
    let session_clone = session.id.clone();
    let waiter = tokio::spawn(async move {
        service_clone
            .request(permission_request(
                &request_id("always-1"),
                Some(&session_clone),
                "fs.write",
                "write",
                Some(&target_clone),
                Some(5),
            ))
            .await
            .unwrap()
    });
    assert!(
        wait_for(
            || !service.pending_list(None).is_empty(),
            Duration::from_secs(3)
        )
        .await
    );
    let ticket = service.pending_list(None).first().unwrap().clone();
    let canonical = ticket.canonical_target.clone().expect("canonical target");
    service
        .resolve(
            &ticket.request_id,
            PermissionDecision::Allow,
            Some(PermissionScope::Always),
        )
        .await
        .unwrap();
    assert!(waiter.await.unwrap().is_allowed());

    // 授权行落库形态：scope=always + target=canonical（ADR-019 附录 A-2）。
    let row = core.reads().permission(&ticket.id).await.unwrap().unwrap();
    assert_eq!(row.scope, Some(PermissionScope::Always));
    assert_eq!(row.target.as_deref(), Some(canonical.as_str()));

    // 同 target 后续请求直接 allow（授权命中）。
    let second = service
        .request(permission_request(
            &request_id("always-2"),
            Some(&session.id),
            "fs.write",
            "write",
            Some(&target),
            Some(5),
        ))
        .await
        .unwrap();
    assert!(second.is_allowed(), "always 授权命中免逐次审批");
    assert!(service.pending_list(None).is_empty());

    // 目标变更不命中（不同 canonical target → 重新 ask）。
    let other = outside_target(&outside, "other-target.txt");
    let service_clone = service.clone();
    let session_clone = session.id.clone();
    let other_clone = other.clone();
    let ask_other = tokio::spawn(async move {
        service_clone
            .request(permission_request(
                &request_id("always-other"),
                Some(&session_clone),
                "fs.write",
                "write",
                Some(&other_clone),
                Some(5),
            ))
            .await
            .unwrap()
    });
    assert!(
        wait_for(
            || !service.pending_list(None).is_empty(),
            Duration::from_secs(3)
        )
        .await,
        "target 变更不得命中既有授权"
    );
    let other_ticket = service.pending_list(None).first().unwrap().clone();
    service
        .resolve(&other_ticket.request_id, PermissionDecision::Deny, None)
        .await
        .unwrap();
    assert!(!ask_other.await.unwrap().is_allowed());

    // 降级 read_only：既有 always 授权按新档位重新求值 → 写请求判 deny（不变量 9）。
    set_session_permission_mode(&core, &session.id, PermissionMode::ReadOnly).await;
    let downgraded = service
        .request(permission_request(
            &request_id("always-downgraded"),
            Some(&session.id),
            "fs.write",
            "write",
            Some(&target),
            Some(5),
        ))
        .await
        .unwrap();
    assert!(
        !downgraded.is_allowed(),
        "read_only 下不因任何既有授权放行写请求（P1-1 断言）"
    );
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD3：核心重启后 `always` 授权按持久行重建匹配集（canonicalize 失败不命中）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn always_grant_rebuilt_after_restart() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    let session = insert_session_row(&core, "01J00000000000000000000M07", None).await;
    let target = outside_target(&outside, "restart-target.txt");
    let service_clone = service.clone();
    let target_clone = target.clone();
    let session_clone = session.id.clone();
    let waiter = tokio::spawn(async move {
        service_clone
            .request(permission_request(
                &request_id("restart-1"),
                Some(&session_clone),
                "fs.write",
                "write",
                Some(&target_clone),
                Some(5),
            ))
            .await
            .unwrap()
    });
    assert!(
        wait_for(
            || !service.pending_list(None).is_empty(),
            Duration::from_secs(3)
        )
        .await
    );
    let ticket = service.pending_list(None).first().unwrap().clone();
    service
        .resolve(
            &ticket.request_id,
            PermissionDecision::Allow,
            Some(PermissionScope::Always),
        )
        .await
        .unwrap();
    assert!(waiter.await.unwrap().is_allowed());

    // 重启（新服务实例，同一库）：restore_grants 重建匹配集。
    let restarted = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    let restored = restarted.restore_grants().await.unwrap();
    assert_eq!(restored, 1, "always 授权必须重建");
    let hit = restarted
        .request(permission_request(
            &request_id("restart-2"),
            Some(&session.id),
            "fs.write",
            "write",
            Some(&target),
            Some(5),
        ))
        .await
        .unwrap();
    assert!(hit.is_allowed(), "重启后 always 授权仍命中");
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD5：规则 deny 收紧矩阵 allow（区内写缺省 allow → deny + `resolver=rule:<id>`）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rule_deny_tightens_allow_and_audits_resolver() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    let session = insert_session_row(&core, "01J00000000000000000000M08", None).await;
    let rule_id = "01J000000000000000000000R1";
    install_rule(&core, &service, rule_record(rule_id, None, "deny")).await;
    let inside = workspace.path().join("tightened.txt");
    let resolution = service
        .request(permission_request(
            &request_id("rule-deny"),
            Some(&session.id),
            "fs.write",
            "write",
            Some(&inside.to_string_lossy()),
            Some(5),
        ))
        .await
        .unwrap();
    assert!(!resolution.is_allowed(), "deny 规则收紧矩阵 allow");
    let entries = audit_entries(&core).await;
    let denied = entries
        .iter()
        .find(|record| record.action == "permission.denied_by_policy")
        .expect("规则收紧必须落审计");
    let detail = denied.detail.as_deref().unwrap();
    assert!(
        detail.contains(&format!("rule:{rule_id}")),
        "审计必须携带 resolver=rule:<id>：{detail}"
    );
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD5：规则 allow 放行矩阵 ask（区外写免逐次审批）+ 硬边界不可放宽。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rule_allow_resolves_ask_and_cannot_widen_hard_boundary() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    let session = insert_session_row(&core, "01J00000000000000000000M09", None).await;
    let target = outside_target(&outside, "rule-allow.txt");
    let rule_id = "01J000000000000000000000R2";
    install_rule(
        &core,
        &service,
        rule_record(rule_id, Some(target.clone()), "allow"),
    )
    .await;
    let allowed = service
        .request(permission_request(
            &request_id("rule-allow"),
            Some(&session.id),
            "fs.write",
            "write",
            Some(&target),
            Some(5),
        ))
        .await
        .unwrap();
    assert!(allowed.is_allowed(), "allow 规则放行矩阵 ask");
    assert!(
        service.pending_list(None).is_empty(),
        "规则放行不得产生 pending"
    );
    let entries = audit_entries(&core).await;
    assert!(
        entries.iter().any(|record| {
            record.action == "permission.allowed_by_policy"
                && record
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains(&format!("rule:{rule_id}")))
        }),
        "规则放行审计必须携带 resolver=rule:<id>"
    );

    // 硬边界：read_only 档写请求不可被 allow 规则放宽（规则不参与矩阵 deny）。
    let read_only = insert_session_row(&core, "01J00000000000000000000M10", None).await;
    set_session_permission_mode(&core, &read_only.id, PermissionMode::ReadOnly).await;
    install_rule(
        &core,
        &service,
        rule_record("01J000000000000000000000R3", None, "allow"),
    )
    .await;
    let inside = workspace.path().join("cannot-widen.txt");
    let denied = service
        .request(permission_request(
            &request_id("rule-hard-boundary"),
            Some(&read_only.id),
            "fs.write",
            "write",
            Some(&inside.to_string_lossy()),
            Some(5),
        ))
        .await
        .unwrap();
    assert!(!denied.is_allowed(), "read_only 硬边界不可被规则放宽");
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD6：规则变更不影响已 pending 票据；新请求按新规则判定。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rule_change_does_not_affect_pending_ticket() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    let session = insert_session_row(&core, "01J00000000000000000000M11", None).await;
    let target = outside_target(&outside, "pending-rule.txt");
    let service_clone = service.clone();
    let target_clone = target.clone();
    let session_clone = session.id.clone();
    let waiter = tokio::spawn(async move {
        service_clone
            .request(permission_request(
                &request_id("pending-before-rule"),
                Some(&session_clone),
                "fs.write",
                "write",
                Some(&target_clone),
                Some(5),
            ))
            .await
            .unwrap()
    });
    assert!(
        wait_for(
            || !service.pending_list(None).is_empty(),
            Duration::from_secs(3)
        )
        .await
    );
    assert!(wait_permissions_pending(&core.reads(), 1).await);
    // 规则加入：pending 票据不受影响（决议仍按用户选择收口）。
    install_rule(
        &core,
        &service,
        rule_record("01J000000000000000000000R4", None, "deny"),
    )
    .await;
    let ticket = service.pending_list(None).first().unwrap().clone();
    service
        .resolve(
            &ticket.request_id,
            PermissionDecision::Allow,
            Some(PermissionScope::Once),
        )
        .await
        .unwrap();
    assert!(
        waiter.await.unwrap().is_allowed(),
        "pending 票据不受规则变更影响"
    );

    // 新请求（同 target）：deny 规则收紧 → deny。
    let after = service
        .request(permission_request(
            &request_id("after-rule"),
            Some(&session.id),
            "fs.write",
            "write",
            Some(&target),
            Some(5),
        ))
        .await
        .unwrap();
    assert!(!after.is_allowed(), "规则变更后新请求按新规则判定");
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD8④：会话创建即带档位（`create_session_with_options` 落库 + 判定入参）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_create_persists_permission_mode() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let clock = manual_clock(5_000_000);
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    let executor = ScriptedExecutor::new(8);
    let manager = build_manager(
        &core,
        clock.clone(),
        executor,
        aether_control::LifecycleConfig::default(),
    );
    let session = manager
        .create_session_with_options(
            m2_support::mock_runtime(),
            "m6-01-mode",
            aether_control::SessionCreateOptions {
                permission_mode: PermissionMode::DangerFullAccess,
                ..aether_control::SessionCreateOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(session.permission_mode, PermissionMode::DangerFullAccess);
    let row = core.reads().session(&session.id).await.unwrap().unwrap();
    assert_eq!(row.permission_mode, PermissionMode::DangerFullAccess);
    // 判定入参按会话解析：exec 在顶档 allow。
    let allowed = service
        .request(permission_request(
            &request_id("created-danger-exec"),
            Some(&session.id),
            "exec",
            "exec",
            Some("npm"),
            None,
        ))
        .await
        .unwrap();
    assert!(allowed.is_allowed(), "新会话档位进入判定入参");
    // 缺省（无会话/无档位）：workspace_write。
    let default_session = create_session(&manager, "m6-01-default").await;
    assert_eq!(
        default_session.permission_mode,
        PermissionMode::WorkspaceWrite
    );
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}
