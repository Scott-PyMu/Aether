//! M3-07 集成测试：审计最小集（设计 D9 / SE-03 / ADR-003/ADR-004）。
//!
//! 覆盖：
//! - DoD1（注入动作 ↔ 审计条数一一对应，脚本比对）：会话生命周期 `create → send →
//!   dispose` 每个边界动作与每次状态转移各 1 条审计（与 `session.*` 事件逐条对应）；
//!   权限决议策略直决 allow/deny 各 1 条、ask→决议与 ask→超时各 2 条。
//! - DoD3（字段齐备）：三类审计行 `actor` / `resource` / `result` / `ts` 均非空，
//!   且语义正确（会话/权限审计的 resource 指向对象，result 为动作结果）。
//!
//! DoD2（应用层无 UPDATE/DELETE 路径）为静态审计：由
//! `scripts/test/m3-07/verify-m3-07.mjs` 扫描源码 + `StoreCommand` 枚举断言。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aether_control::{LifecycleConfig, PermissionConfig};
use aether_core::{PermissionDecision, PermissionScope, RunStatus, SessionStatus};
use aether_store::AuditLogRecord;
use m2_support::{
    build_manager, build_permission_service, create_session, manual_clock, permission_request,
    system_clock, wait_for, wait_run_status, wait_session_status, ScriptedExecutor, TestCore,
};
use serde_json::{json, Value};

fn client_id(label: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    format!("{label}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn request_id(label: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    format!("{label}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// 证据导出（`AETHER_M3_07_EVIDENCE_DIR` 存在时写 JSON；同时打印摘要行）。
fn write_evidence(name: &str, value: &Value) {
    println!("[m3-07] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M3_07_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{name}.json"));
    let Ok(text) = serde_json::to_string_pretty(value) else {
        return;
    };
    let _ = std::fs::write(&path, text);
}

fn row_json(record: &AuditLogRecord) -> Value {
    json!({
        "actor": record.actor,
        "action": record.action,
        "resource": record.resource,
        "result": record.result,
        "ts": record.ts,
        "session_id": record.session_id,
    })
}

/// DoD1/DoD3（会话生命周期）：每个注入动作与状态转移 ↔ 恰好 1 条审计。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_lifecycle_audits_are_one_to_one() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(8);
    let manager = build_manager(&core, system_clock(), executor, LifecycleConfig::default());

    // 注入动作：create → send（idle→running→idle）→ dispose（idle→completed）。
    let session = create_session(&manager, "m3-07-lifecycle").await;
    let ack = manager
        .send(&session.id, "审计最小集", &client_id("lifecycle"))
        .await
        .unwrap();
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Succeeded).await,
        "run 应到达终态"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "run 收口后会话应回 idle"
    );
    assert_eq!(
        manager.dispose(&session.id).await.unwrap(),
        SessionStatus::Completed
    );

    let rows = core.reads().audit_log(200).await.unwrap();
    let mine: Vec<&AuditLogRecord> = rows
        .iter()
        .filter(|record| record.session_id.as_deref() == Some(session.id.as_str()))
        .collect();
    let actual: Vec<(String, String)> = mine
        .iter()
        .map(|record| {
            (
                record.action.clone(),
                record.result.clone().unwrap_or_default(),
            )
        })
        .collect();
    let expected: Vec<(String, String)> = vec![
        ("session.created".to_owned(), "created".to_owned()),
        (
            "session.status_changed".to_owned(),
            "creating→idle".to_owned(),
        ),
        (
            "session.status_changed".to_owned(),
            "idle→running".to_owned(),
        ),
        (
            "session.status_changed".to_owned(),
            "running→idle".to_owned(),
        ),
        (
            "session.status_changed".to_owned(),
            "idle→completed".to_owned(),
        ),
        ("session.closed".to_owned(), "completed".to_owned()),
    ];
    assert_eq!(actual, expected, "注入动作与审计条数必须一一对应");

    // 与事件侧对照：session.status_changed 事件数 == 审计条数（逐条对应）。
    let readback = core.pipeline.readback(&session.id, 0).await.unwrap();
    let status_events = readback
        .events
        .iter()
        .filter(|event| event.event_type().as_str() == "session.status_changed")
        .count();
    let status_audits = mine
        .iter()
        .filter(|record| record.action == "session.status_changed")
        .count();
    assert_eq!(status_events, 4, "4 次状态转移事件");
    assert_eq!(status_audits, status_events, "状态转移审计与事件一一对应");
    let created_events = readback
        .events
        .iter()
        .filter(|event| event.event_type().as_str() == "session.created")
        .count();
    let closed_events = readback
        .events
        .iter()
        .filter(|event| event.event_type().as_str() == "session.closed")
        .count();
    assert_eq!(created_events, 1);
    assert_eq!(closed_events, 1);

    // DoD3：字段齐备（actor/resource/result/ts；resource 指向会话对象）。
    for record in &mine {
        assert!(!record.actor.is_empty(), "actor 必须齐备（user/system）");
        assert_eq!(
            record.resource.as_deref(),
            Some(format!("session:{}", session.id.as_str()).as_str())
        );
        assert!(record
            .result
            .as_deref()
            .is_some_and(|value| !value.is_empty()));
        assert!(record.ts > 0, "ts 必须为有效时间戳");
    }
    for record in mine
        .iter()
        .filter(|record| record.action == "session.status_changed")
    {
        assert_eq!(record.actor, "system", "状态转移审计 actor=system");
    }
    let created = mine
        .iter()
        .find(|record| record.action == "session.created")
        .expect("session.created 审计");
    assert_eq!(created.actor, "user", "创建动作 actor=user");
    let closed = mine
        .iter()
        .find(|record| record.action == "session.closed")
        .expect("session.closed 审计");
    assert_eq!(closed.actor, "user", "关闭动作 actor=user");

    write_evidence(
        "dod1_session_lifecycle",
        &json!({
            "task": "M3-07 DoD1 会话生命周期 1:1",
            "injected": ["session.create", "session.send", "session.dispose"],
            "expected": expected
                .iter()
                .map(|(action, result)| json!({"action": action, "result": result}))
                .collect::<Vec<_>>(),
            "actual": actual
                .iter()
                .map(|(action, result)| json!({"action": action, "result": result}))
                .collect::<Vec<_>>(),
            "status_events": status_events,
            "status_audits": status_audits,
            "rows": mine.iter().map(|record| row_json(record)).collect::<Vec<_>>(),
            "one_to_one": actual == expected,
        }),
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD1/DoD3（权限决议）：策略直决 / ask 决议 / ask 超时的审计条数一一对应。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_decision_audits_are_one_to_one() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    // 会话行（ask 审计带 session_id；`permissions` 行 FK 可空，但会话内回环有会话）。
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(&core, system_clock(), executor, LifecycleConfig::default());
    let session = create_session(&manager, "m3-07-permission").await;
    let session_id = session.id.clone();

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

    // ① 策略直决 deny：exec 恒 deny（1 条 permission.denied_by_policy）。
    let deny = service
        .request(permission_request(
            &request_id("exec"),
            Some(&session_id),
            "exec",
            "invoke",
            Some("anything"),
            None,
        ))
        .await
        .unwrap();
    assert!(!deny.is_allowed());

    // ② 策略直决 allow：工作区内 fs.read（1 条 permission.allowed_by_policy）。
    let inside = workspace.path().join("note.txt");
    std::fs::write(&inside, b"x").unwrap();
    let inside = inside.to_string_lossy().to_string();
    let allow = service
        .request(permission_request(
            &request_id("read-in"),
            Some(&session_id),
            "fs.read",
            "read",
            Some(&inside),
            None,
        ))
        .await
        .unwrap();
    assert!(allow.is_allowed());

    // ③ ask → 用户 once 允许（requested + resolved = 2 条）。
    let write_target = workspace
        .path()
        .join("new.txt")
        .to_string_lossy()
        .to_string();
    let request = permission_request(
        &request_id("write-ask"),
        Some(&session_id),
        "fs.write",
        "write",
        Some(&write_target),
        Some(10),
    );
    let service_clone = service.clone();
    let ask_task = tokio::spawn(async move { service_clone.request(request).await.unwrap() });
    assert!(
        wait_for(
            || !service.pending_list(Some(&session_id)).is_empty(),
            Duration::from_secs(3)
        )
        .await,
        "fs.write 工作区应进入待审批"
    );
    let ticket = service
        .pending_list(Some(&session_id))
        .first()
        .unwrap()
        .clone();
    service
        .resolve(
            &ticket.request_id,
            PermissionDecision::Allow,
            Some(PermissionScope::Once),
        )
        .await
        .unwrap();
    let resolution = ask_task.await.unwrap();
    assert!(resolution.is_allowed());

    // ④ ask → 300s 超时（时钟注入；requested + timeout = 2 条）。
    let timeout_target = workspace
        .path()
        .join("timeout.txt")
        .to_string_lossy()
        .to_string();
    let request = permission_request(
        &request_id("write-timeout"),
        Some(&session_id),
        "fs.write",
        "write",
        Some(&timeout_target),
        Some(10),
    );
    let service_clone = service.clone();
    let timeout_task = tokio::spawn(async move { service_clone.request(request).await.unwrap() });
    assert!(
        wait_for(
            || service.pending_list(Some(&session_id)).len() == 1,
            Duration::from_secs(3)
        )
        .await,
        "第二条 ask 应进入待审批"
    );
    // 300s 超时由 ManualClock 推进 + 巡检驱动。
    clock.advance(300_000);
    let timed_out = service.sweep_timeouts_once().await;
    assert_eq!(timed_out.len(), 1, "超时票据必须被摘除");
    let resolution = timeout_task.await.unwrap();
    assert!(
        resolution.timed_out && !resolution.is_allowed(),
        "超时按 deny"
    );

    // 审计比对：过滤本会话的权限审计（会话创建审计存在但不参与本断言）。
    let rows = core.reads().audit_log(200).await.unwrap();
    let permission_actions: Vec<(String, String)> = rows
        .iter()
        .filter(|record| record.action.starts_with("permission."))
        .map(|record| {
            (
                record.action.clone(),
                record.result.clone().unwrap_or_default(),
            )
        })
        .collect();
    let expected: Vec<(String, String)> = vec![
        ("permission.denied_by_policy".to_owned(), "deny".to_owned()),
        (
            "permission.allowed_by_policy".to_owned(),
            "allow".to_owned(),
        ),
        ("permission.requested".to_owned(), "pending".to_owned()),
        ("permission.resolved".to_owned(), "resolved".to_owned()),
        ("permission.requested".to_owned(), "pending".to_owned()),
        ("permission.timeout".to_owned(), "timeout".to_owned()),
    ];
    assert_eq!(
        permission_actions, expected,
        "权限决议注入与审计条数必须一一对应"
    );

    // DoD3：字段齐备（actor/resource/result/ts；resource = resource:action）。
    let permission_rows: Vec<&AuditLogRecord> = rows
        .iter()
        .filter(|record| record.action.starts_with("permission."))
        .collect();
    for record in &permission_rows {
        assert!(!record.actor.is_empty(), "actor 必须齐备");
        assert!(
            record
                .resource
                .as_deref()
                .is_some_and(|value| value.contains(':')),
            "resource 必须齐备（resource:action）"
        );
        assert!(record
            .result
            .as_deref()
            .is_some_and(|value| !value.is_empty()));
        assert!(record.ts > 0);
    }
    // ask 类审计带会话归属（会话生命周期/权限决议可关联）。
    let requested = permission_rows
        .iter()
        .find(|record| record.action == "permission.requested")
        .unwrap();
    assert_eq!(requested.session_id.as_deref(), Some(session_id.as_str()));

    write_evidence(
        "dod1_permission_decisions",
        &json!({
            "task": "M3-07 DoD1 权限决议 1:1",
            "injected": [
                "policy.deny",
                "policy.allow",
                "ask.resolved(once)",
                "ask.timeout",
            ],
            "expected": expected
                .iter()
                .map(|(action, result)| json!({"action": action, "result": result}))
                .collect::<Vec<_>>(),
            "actual": permission_actions
                .iter()
                .map(|(action, result)| json!({"action": action, "result": result}))
                .collect::<Vec<_>>(),
            "rows": permission_rows.iter().map(|record| row_json(record)).collect::<Vec<_>>(),
            "one_to_one": permission_actions == expected,
        }),
    );

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}
