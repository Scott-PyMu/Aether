//! M3-12 集成测试：会话等待态写入（ADR-011 / D9）——置位/回程守卫、回程合法性
//! （allow/deny/超时/取消/run 失败）、重启 no-op、多会话并发等待。
//!
//! 验证口径（实施计划 v1.18 §4 M3-12 DoD1–4；生产组合路径集成 E2E 见
//! `crates/aether-tauri/tests/m3_12_waiting_state.rs`）：
//! - 状态仅由 [`SessionManager`] 落行并广播 `session.status_changed`（复用既有
//!   19 边状态机；不新增命令/事件/迁移）；
//! - `PermissionService` 只上报「同会话 pending 计数 0↔1」（`set_pending_observer`）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use aether_control::{ExecutorOutcome, LifecycleConfig, PermissionConfig};
use aether_core::{
    session_transition_allowed, ErrorInfo, PermissionDecision, PermissionScope, RunStatus,
    SessionId, SessionStatus,
};
use m2_support::{
    build_manager, build_permission_service, create_session, manual_clock, permission_request,
    system_clock, wait_for, wait_permissions_pending, wait_run_status, wait_session_status,
    CancellableExecutor, ScriptedExecutor, TestCore,
};

/// 会话 `session.status_changed` 转移序列（`from → to`，按落库顺序）。
async fn status_transitions(core: &TestCore, session_id: &SessionId) -> Vec<(String, String)> {
    core.pipeline
        .readback(session_id, 0)
        .await
        .unwrap()
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

/// 等待态定向边（`running ↔ waiting_permission`）计数。
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

/// 全部转移必须命中冻结的 19 边白名单（DoD1：不产生非法转移）。
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

/// 等待事件表达到预期转移数（行更新与事件落库之间存在窗口：`wait_session_status`
/// 以行为准可能在末条事件落库前返回）。
async fn wait_transitions_len(
    core: &TestCore,
    session_id: &SessionId,
    expected: usize,
) -> Vec<(String, String)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let transitions = status_transitions(core, session_id).await;
        if transitions.len() >= expected || tokio::time::Instant::now() >= deadline {
            return transitions;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// DoD1：置位/回程守卫——置位仅 `running` 生效、回程仅 `waiting_permission` 生效；
/// 非适用态 no-op；状态仅经 `SessionManager` 落行并广播（from/to 断言）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod1_mark_clear_guards_and_status_events() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let service = build_permission_service(
        &core,
        workspace.path(),
        system_clock(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    service.set_pending_observer(Arc::new(manager.clone()));
    let session = create_session(&manager, "m3-12-dod1").await;

    // 非适用态 no-op（idle）：不落行、不发事件。
    assert!(
        !manager.mark_waiting_permission(&session.id).await.unwrap(),
        "idle 置位必须 no-op"
    );
    assert!(
        !manager.clear_waiting_permission(&session.id).await.unwrap(),
        "idle 回程必须 no-op"
    );
    assert_eq!(
        manager.session_status(&session.id).await.unwrap(),
        SessionStatus::Idle
    );

    // running（run 在途）→ ask → 置位 `running → waiting_permission`。
    let run = manager.send(&session.id, "hold", "dod1-1").await.unwrap();
    assert!(
        wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
        "run 应派发"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Running).await,
        "run 在途会话应为 running"
    );

    let target = workspace
        .path()
        .join("note.txt")
        .to_string_lossy()
        .to_string();
    let request = permission_request(
        "dod1-req-1",
        Some(&session.id),
        "fs.write",
        "write",
        Some(&target),
        Some(10),
    );
    let waiter = {
        let service = service.clone();
        tokio::spawn(async move { service.request(request).await })
    };
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::WaitingPermission).await,
        "ask 后会话应置 waiting_permission"
    );
    // 已在等待 → 重复置位 no-op（不产生第二条事件）。
    assert!(
        !manager.mark_waiting_permission(&session.id).await.unwrap(),
        "waiting_permission 重复置位必须 no-op"
    );

    // allow 决议 → 回程 `waiting_permission → running`。
    service
        .resolve(
            "dod1-req-1",
            PermissionDecision::Allow,
            Some(PermissionScope::Once),
        )
        .await
        .unwrap();
    let resolution = waiter.await.unwrap().unwrap();
    assert!(resolution.is_allowed(), "allow 决议应回传适配器");
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Running).await,
        "决议后会话应回 running"
    );
    // 已在 running → 重复回程 no-op。
    assert!(
        !manager.clear_waiting_permission(&session.id).await.unwrap(),
        "running 回程必须 no-op"
    );

    // 放行 run → 终态 → 会话 idle。
    executor.release(1);
    assert!(
        wait_run_status(&manager, &run.run_id, RunStatus::Succeeded).await,
        "run 应成功终态"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "run 终态后会话回 idle"
    );

    // 非适用态 ask（idle）：观察者置位 no-op、决议回程 no-op。
    let request2 = permission_request(
        "dod1-req-2",
        Some(&session.id),
        "fs.write",
        "write",
        Some(&target),
        Some(10),
    );
    let waiter2 = {
        let service = service.clone();
        tokio::spawn(async move { service.request(request2).await })
    };
    assert!(
        wait_permissions_pending(&core.reads(), 1).await,
        "第二张票据应持久化 pending"
    );
    service
        .resolve("dod1-req-2", PermissionDecision::Deny, None)
        .await
        .unwrap();
    let resolution2 = waiter2.await.unwrap().unwrap();
    assert!(!resolution2.is_allowed(), "deny 决议应回传适配器");
    assert_eq!(
        manager.session_status(&session.id).await.unwrap(),
        SessionStatus::Idle,
        "非 running 的 ask 不得改写会话状态"
    );

    // 事件断言：等待态方向各恰好一次且 from/to 正确；全部转移合法。
    let transitions = status_transitions(&core, &session.id).await;
    assert_transitions_legal(&transitions);
    assert_eq!(
        waiting_edges(&transitions),
        vec![
            ("running".to_owned(), "waiting_permission".to_owned()),
            ("waiting_permission".to_owned(), "running".to_owned()),
        ],
        "等待态方向边恰好各一次；实际序列：{transitions:?}"
    );
    println!("[m3-12 DoD1] 状态转移序列 = {transitions:?}");

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD2：回程合法性（deny / 超时）——决议与超时路径均回 `running`；
/// 决议 × 超时并发不产生第二次回程（票据摘除唯一仲裁点）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod2_deny_and_timeout_return_to_running() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let clock = manual_clock(1_000_000);
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    service.set_pending_observer(Arc::new(manager.clone()));
    let session = create_session(&manager, "m3-12-dod2").await;
    let target = workspace
        .path()
        .join("note.txt")
        .to_string_lossy()
        .to_string();

    let run = manager.send(&session.id, "hold", "dod2-1").await.unwrap();
    assert!(
        wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
        "run 应派发"
    );

    // deny 路径。
    let deny_request = permission_request(
        "dod2-deny",
        Some(&session.id),
        "fs.write",
        "write",
        Some(&target),
        Some(10),
    );
    let deny_waiter = {
        let service = service.clone();
        tokio::spawn(async move { service.request(deny_request).await })
    };
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::WaitingPermission).await,
        "ask 后应置 waiting_permission"
    );
    service
        .resolve("dod2-deny", PermissionDecision::Deny, None)
        .await
        .unwrap();
    assert!(
        !deny_waiter.await.unwrap().unwrap().is_allowed(),
        "deny 决议应回传"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Running).await,
        "deny 后会话应回 running"
    );

    // 超时路径（300s 时钟注入）。
    let timeout_request = permission_request(
        "dod2-timeout",
        Some(&session.id),
        "fs.write",
        "write",
        Some(&target),
        Some(10),
    );
    let timeout_waiter = {
        let service = service.clone();
        tokio::spawn(async move { service.request(timeout_request).await })
    };
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::WaitingPermission).await,
        "ask 后应再次置 waiting_permission"
    );
    clock.advance(300_000);
    let timed_out = service.sweep_timeouts_once().await;
    assert_eq!(timed_out, vec!["dod2-timeout".to_owned()]);
    let timeout_resolution = timeout_waiter.await.unwrap().unwrap();
    assert!(
        timeout_resolution.timed_out && !timeout_resolution.is_allowed(),
        "超时应回传 deny（timed_out=true）"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Running).await,
        "超时 deny 后会话应回 running"
    );

    // 并发幂等：已决议/已超时的票据再次 sweep 不产生第二次回程。
    assert!(service.sweep_timeouts_once().await.is_empty());
    let transitions = status_transitions(&core, &session.id).await;
    assert_transitions_legal(&transitions);
    assert_eq!(
        waiting_edges(&transitions).len(),
        4,
        "两次 ask 各一次置位 + 一次回程；实际：{transitions:?}"
    );

    executor.release(1);
    assert!(
        wait_run_status(&manager, &run.run_id, RunStatus::Succeeded).await,
        "run 应成功终态"
    );
    println!("[m3-12 DoD2 deny/timeout] 状态转移序列 = {transitions:?}");

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD2：回程合法性（取消 + run 失败收口）——取消路径合法回程（`waiting_permission →
/// running → idle`）；run 失败收口后迟到决议不再产生回程/多余事件。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod2_cancel_and_run_failure_return_paths() {
    // 取消路径：request_cancellable 绑定在途 run 令牌 → 取消级联摘除票据。
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let executor = CancellableExecutor::new();
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let service = build_permission_service(
        &core,
        workspace.path(),
        system_clock(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    service.set_pending_observer(Arc::new(manager.clone()));
    let session = create_session(&manager, "m3-12-dod2-cancel").await;
    let target = workspace
        .path()
        .join("note.txt")
        .to_string_lossy()
        .to_string();

    let run = manager
        .send(&session.id, "hold", "dod2-cancel-1")
        .await
        .unwrap();
    assert!(
        wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
        "run 应派发"
    );
    let token = manager
        .active_run_cancel_token(&session.id)
        .await
        .unwrap()
        .expect("在途 run 应提供取消令牌");
    let request = permission_request(
        "dod2-cancel",
        Some(&session.id),
        "fs.write",
        "write",
        Some(&target),
        Some(10),
    );
    let waiter = {
        let service = service.clone();
        tokio::spawn(async move { service.request_cancellable(request, Some(&token)).await })
    };
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::WaitingPermission).await,
        "ask 后应置 waiting_permission"
    );

    let report = manager.interrupt(&session.id).await.unwrap();
    assert_eq!(report.interrupted_run, Some(run.run_id.clone()));
    let resolution = waiter.await.unwrap().unwrap();
    assert!(
        !resolution.is_allowed() && !resolution.timed_out,
        "取消路径应按 deny 收口（非超时）"
    );
    assert!(
        wait_run_status(&manager, &run.run_id, RunStatus::Cancelled).await,
        "被中断 run 应终态 cancelled"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "取消后会话必须回 idle"
    );
    let transitions = wait_transitions_len(&core, &session.id, 5).await;
    assert_transitions_legal(&transitions);
    assert_eq!(
        waiting_edges(&transitions).len(),
        2,
        "取消路径：一次置位 + 一次合法回程；实际：{transitions:?}"
    );
    println!("[m3-12 DoD2 cancel] 状态转移序列 = {transitions:?}");
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();

    // run 失败收口：失败路径经既有 settle（`waiting_permission → running → idle`）；
    // 迟到决议命中回程守卫（idle）→ no-op，不产生多余 `session.status_changed`。
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let executor = ScriptedExecutor::new(0);
    executor.set_outcome(ExecutorOutcome::Failed {
        error: ErrorInfo {
            code: "run_stream_timeout".to_owned(),
            message: "注入失败（run 失败收口路径）".to_owned(),
            recoverable: true,
        },
    });
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let service = build_permission_service(
        &core,
        workspace.path(),
        system_clock(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    service.set_pending_observer(Arc::new(manager.clone()));
    let session = create_session(&manager, "m3-12-dod2-failure").await;
    let target = workspace
        .path()
        .join("note.txt")
        .to_string_lossy()
        .to_string();

    let run = manager
        .send(&session.id, "hold", "dod2-fail-1")
        .await
        .unwrap();
    assert!(
        wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
        "run 应派发"
    );
    let request = permission_request(
        "dod2-fail",
        Some(&session.id),
        "fs.write",
        "write",
        Some(&target),
        Some(10),
    );
    let service_clone = service.clone();
    let _waiter = tokio::spawn(async move { service_clone.request(request).await });
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::WaitingPermission).await,
        "ask 后应置 waiting_permission"
    );
    executor.release(1);
    assert!(
        wait_run_status(&manager, &run.run_id, RunStatus::Failed).await,
        "注入失败 run 应终态 failed"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "失败收口后会话应回 idle"
    );
    let before = wait_transitions_len(&core, &session.id, 5).await;
    assert_eq!(before.len(), 5, "失败收口应产生完整转移序列：{before:?}");

    // 迟到决议：票据仍在 pending（无取消令牌），决议命中 idle 回程守卫 → no-op。
    // `resolve` 在返回前 await 观察者回调（`notify_pending_cleared`），故返回后
    // 事件表即为终态——无需轮询。
    service
        .resolve(
            "dod2-fail",
            PermissionDecision::Allow,
            Some(PermissionScope::Once),
        )
        .await
        .unwrap();
    let after = status_transitions(&core, &session.id).await;
    assert_eq!(
        before, after,
        "迟到决议不得产生新的 session.status_changed（回程守卫 no-op）"
    );
    assert_transitions_legal(&after);
    assert_eq!(
        waiting_edges(&after).len(),
        2,
        "失败收口一次置位 + 一次（waiting→running）收口；实际：{after:?}"
    );
    println!("[m3-12 DoD2 failure] 状态转移序列 = {after:?}");

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD3：重启语义——`restore_pending` 不标记等待态；恢复票据决议/超时后无回程、
/// 无多余 `session.status_changed`（新服务 + 新管理器读取同一库）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod3_restore_pending_never_marks_waiting_state() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let executor = ScriptedExecutor::new(4);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let clock = manual_clock(2_000_000);
    let service = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    service.set_pending_observer(Arc::new(manager.clone()));
    let session = create_session(&manager, "m3-12-dod3").await;
    let target = workspace
        .path()
        .join("note.txt")
        .to_string_lossy()
        .to_string();
    // 会话保持 idle（无在途 run）——重启恢复票据的决议/超时不得标记等待态。
    let baseline = status_transitions(&core, &session.id).await;
    assert_eq!(baseline.len(), 1, "仅创建时 creating → idle");

    let mut waiters = Vec::new();
    for request_id in ["dod3-resolve", "dod3-timeout"] {
        let service = service.clone();
        let request = permission_request(
            request_id,
            Some(&session.id),
            "fs.write",
            "write",
            Some(&target),
            Some(10),
        );
        waiters.push(tokio::spawn(async move { service.request(request).await }));
    }
    assert!(
        wait_permissions_pending(&core.reads(), 2).await,
        "两张票据应持久化 pending"
    );

    // 核心重启语义：新服务实例 + 新管理器（同一库、同一会话行）。
    let restarted = build_permission_service(
        &core,
        workspace.path(),
        clock.clone(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    let restarted_manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    restarted.set_pending_observer(Arc::new(restarted_manager.clone()));
    assert_eq!(
        restarted.restore_pending().await.unwrap(),
        2,
        "两张票据必须恢复（仅台账）"
    );
    assert_eq!(
        restarted_manager.session_status(&session.id).await.unwrap(),
        SessionStatus::Idle,
        "restore_pending 不得改写会话状态"
    );

    // 恢复票据决议（A）→ 回程 guard no-op。
    restarted
        .resolve(
            "dod3-resolve",
            PermissionDecision::Allow,
            Some(PermissionScope::Once),
        )
        .await
        .unwrap();
    // 恢复票据超时（B）→ 回程 guard no-op。
    clock.advance(300_000);
    let timed_out = restarted.sweep_timeouts_once().await;
    assert_eq!(timed_out, vec!["dod3-timeout".to_owned()]);

    let after = status_transitions(&core, &session.id).await;
    assert_eq!(
        baseline, after,
        "恢复票据的决议/超时不得产生 session.status_changed（no-op）"
    );
    assert!(
        waiting_edges(&after).is_empty(),
        "重启恢复路径不得出现 waiting_permission 边：{after:?}"
    );
    println!("[m3-12 DoD3] 重启 no-op；状态转移序列 = {after:?}");

    for waiter in waiters {
        waiter.abort();
        let _ = waiter.await;
    }
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD4：多会话并发等待——两个会话可同时处于 `waiting_permission`；
/// 同会话「≤1 个待审批」为展示口径（服务层并发 pending 能力保留，见 m2_03 T6）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod4_multiple_sessions_wait_concurrently() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let service = build_permission_service(
        &core,
        workspace.path(),
        system_clock(),
        PermissionConfig {
            wait_timeout: None,
            ..PermissionConfig::default()
        },
    );
    service.set_pending_observer(Arc::new(manager.clone()));

    let session_a = create_session(&manager, "m3-12-dod4-a").await;
    let session_b = create_session(&manager, "m3-12-dod4-b").await;
    let run_a = manager
        .send(&session_a.id, "hold-a", "dod4-a")
        .await
        .unwrap();
    let run_b = manager
        .send(&session_b.id, "hold-b", "dod4-b")
        .await
        .unwrap();
    assert!(
        wait_for(|| executor.call_count() == 2, Duration::from_secs(5)).await,
        "两个会话的 run 均应派发"
    );

    let target = workspace
        .path()
        .join("note.txt")
        .to_string_lossy()
        .to_string();
    let request_a = permission_request(
        "dod4-a",
        Some(&session_a.id),
        "fs.write",
        "write",
        Some(&target),
        Some(10),
    );
    let request_b = permission_request(
        "dod4-b",
        Some(&session_b.id),
        "fs.write",
        "write",
        Some(&target),
        Some(10),
    );
    let waiter_a = {
        let service = service.clone();
        tokio::spawn(async move { service.request(request_a).await })
    };
    let waiter_b = {
        let service = service.clone();
        tokio::spawn(async move { service.request(request_b).await })
    };

    assert!(
        wait_session_status(&manager, &session_a.id, SessionStatus::WaitingPermission).await,
        "会话 A 应置 waiting_permission"
    );
    assert!(
        wait_session_status(&manager, &session_b.id, SessionStatus::WaitingPermission).await,
        "会话 B 应同时置 waiting_permission（多会话并发等待）"
    );
    assert_eq!(service.pending_list(Some(&session_a.id)).len(), 1);
    assert_eq!(service.pending_list(Some(&session_b.id)).len(), 1);

    // 分别决议 → 各自回 running。
    service
        .resolve(
            "dod4-a",
            PermissionDecision::Allow,
            Some(PermissionScope::Once),
        )
        .await
        .unwrap();
    assert!(waiter_a.await.unwrap().unwrap().is_allowed());
    service
        .resolve("dod4-b", PermissionDecision::Deny, None)
        .await
        .unwrap();
    assert!(!waiter_b.await.unwrap().unwrap().is_allowed());
    assert!(
        wait_session_status(&manager, &session_a.id, SessionStatus::Running).await,
        "会话 A 决议后回 running"
    );
    assert!(
        wait_session_status(&manager, &session_b.id, SessionStatus::Running).await,
        "会话 B 决议后回 running"
    );

    executor.release(2);
    assert!(
        wait_run_status(&manager, &run_a.run_id, RunStatus::Succeeded).await
            && wait_run_status(&manager, &run_b.run_id, RunStatus::Succeeded).await,
        "两个 run 均应成功"
    );
    for session_id in [&session_a.id, &session_b.id] {
        let transitions = status_transitions(&core, session_id).await;
        assert_transitions_legal(&transitions);
        assert_eq!(
            waiting_edges(&transitions).len(),
            2,
            "每会话一次置位 + 一次回程"
        );
    }
    println!("[m3-12 DoD4] 两会话并发等待并各自合法回程");

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}
