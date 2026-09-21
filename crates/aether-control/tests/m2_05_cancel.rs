//! M2-05 集成测试：取消树（interrupt/dispose/父取消级联）+ 10s 任务看门狗
//! （dump + 强制清理）+ 权限等待可取消。
//!
//! 设计依据：D8「取消树：会话级 CancellationToken，interrupt/dispose/父会话取消级联；
//! 权限等待可取消」；失败场景表「取消风暴 → 10s 未退出的会话任务强制清理并记 dump」。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::time::{Duration, Instant};

use aether_control::{LifecycleConfig, PermissionConfig, TASK_DUMP_ACTION_FORCED_CLEANUP};
use aether_core::{PermissionDecision, RunStatus, SessionStatus};
use m2_support::{
    build_manager, build_permission_service, create_session, insert_session_row, manual_clock,
    permission_request, system_clock, wait_for, wait_permissions_pending, wait_run_status,
    wait_session_status, CancellableExecutor, TestCore, UnresponsiveExecutor,
};
use serde_json::json;

const CHILD_SESSION: &str = "01J00000000000000000000C1";
const GRANDCHILD_SESSION: &str = "01J00000000000000000000G1";

fn delta_envelope(session_id: &str, run_id: &str, text: &str) -> serde_json::Value {
    json!({
        "v": 1,
        "id": "01J00000000000000000000D1",
        "session_id": session_id,
        "run_id": run_id,
        "runtime_id": "mock",
        "seq": 0,
        "ts": 1_700_000_000_000i64,
        "type": "message.delta",
        "payload": {"message_id": "01J00000000000000000000M1", "text": text},
    })
}

/// DoD1：取消风暴——20 会话并发 dispose，全部任务 10s 内退出；取消级联零遗漏。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod1_cancel_storm_twenty_sessions_dispose_within_10s() {
    const SESSIONS: usize = 20;
    let core = TestCore::open().await;
    let executor = CancellableExecutor::new();
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );

    let mut session_ids = Vec::new();
    let mut run_ids = Vec::new();
    for index in 0..SESSIONS {
        let session = create_session(&manager, &format!("storm-{index}")).await;
        let ack = manager
            .send(&session.id, "storm", &format!("storm-{index}"))
            .await
            .unwrap();
        assert!(!ack.queued && !ack.duplicate);
        session_ids.push(session.id);
        run_ids.push(ack.run_id);
    }
    assert!(
        wait_for(
            || executor.call_count() == SESSIONS,
            Duration::from_secs(10)
        )
        .await,
        "20 个执行器应全部派发（当前 {}）",
        executor.call_count()
    );

    // 20 会话并发 dispose（取消风暴）。
    let started = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for session_id in &session_ids {
        let manager = manager.clone();
        let session_id = session_id.clone();
        set.spawn(async move { manager.dispose(&session_id).await });
    }
    let mut statuses = Vec::new();
    while let Some(result) = set.join_next().await {
        statuses.push(result.unwrap().unwrap());
    }
    let dispose_elapsed = started.elapsed();
    assert_eq!(statuses.len(), SESSIONS);
    assert!(
        statuses
            .iter()
            .all(|status| *status == SessionStatus::Cancelled),
        "有在途 run 的会话 dispose 应为 cancelled：{statuses:?}"
    );

    // 取消树级联：全部 run 令牌取消、run 行终态 cancelled、会话行终态。
    assert!(
        wait_for(
            || executor.cancelled_count() == SESSIONS,
            Duration::from_secs(5)
        )
        .await,
        "取消令牌必须级联到全部在途 run（当前 {}）",
        executor.cancelled_count()
    );
    for run_id in &run_ids {
        assert!(
            wait_run_status(&manager, run_id, RunStatus::Cancelled).await,
            "run 应 cancelled：{run_id}"
        );
    }
    for session_id in &session_ids {
        assert_eq!(
            manager.session_status(session_id).await.unwrap(),
            SessionStatus::Cancelled
        );
        assert!(
            manager
                .session_cancel_token(session_id)
                .await
                .unwrap()
                .is_cancelled(),
            "会话取消节点应已取消：{session_id}"
        );
    }

    // 全部任务 10s 内退出（取消响应型执行器：实测毫秒级），无强制清理 dump。
    let exit_started = Instant::now();
    let exited = wait_for(
        || {
            manager.sweep_tasks_once();
            manager.active_task_count() == 0
        },
        Duration::from_secs(10),
    )
    .await;
    let exit_elapsed = exit_started.elapsed();
    assert!(
        exited,
        "20 个会话任务必须在 10s 内退出（剩余 {}）",
        manager.active_task_count()
    );
    assert!(
        manager.task_dumps().is_empty(),
        "取消响应型执行器不得产生强制清理 dump：{:?}",
        manager.task_dumps()
    );
    assert!(
        dispose_elapsed < Duration::from_secs(10),
        "dispose 风暴耗时 {dispose_elapsed:?}"
    );
    println!(
        "[m2-05 DoD1] 20 会话并发 dispose：dispose 全部返回耗时 {dispose_elapsed:?}；\
         任务全部退出耗时 {exit_elapsed:?}；dump 数 = {}",
        manager.task_dumps().len()
    );
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD2（API-E2E）：interrupt → 会话回 idle 且可续聊；权限等待经取消树级联取消。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod2_interrupt_returns_idle_and_conversation_continues() {
    let core = TestCore::open().await;
    let workspace = tempfile::tempdir().unwrap();
    let executor = CancellableExecutor::new();
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let permissions = build_permission_service(
        &core,
        workspace.path(),
        system_clock(),
        PermissionConfig::default(),
    );
    let session = create_session(&manager, "interrupt-e2e").await;

    // 第 1 条：执行器在途（未放行），流式 delta 已产生。
    let first = manager.send(&session.id, "first", "e2e-1").await.unwrap();
    assert!(
        wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
        "第 1 条应派发执行"
    );
    let outcome = core
        .pipeline
        .submit(delta_envelope(
            session.id.as_str(),
            first.run_id.as_str(),
            "部分输出",
        ))
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            aether_control::SubmitOutcome::Persisted { .. }
                | aether_control::SubmitOutcome::Buffered
        ),
        "流式 delta 应被管线受理：{outcome:?}"
    );

    // 权限等待绑定在途 run 令牌（M2-05：权限等待可取消）。
    let run_token = manager
        .active_run_cancel_token(&session.id)
        .await
        .unwrap()
        .expect("在途 run 应提供取消令牌");
    let target = workspace.path().join("note.txt");
    let request = permission_request(
        "req-e2e-1",
        Some(&session.id),
        "fs.write",
        "write",
        target.to_str(),
        None,
    );
    let waiter = {
        let permissions = permissions.clone();
        let token = run_token.clone();
        tokio::spawn(async move { permissions.request_cancellable(request, Some(&token)).await })
    };
    assert!(
        wait_permissions_pending(&core.reads(), 1).await,
        "审批票据应 pending 持久化"
    );

    // interrupt：run 转 cancelled；权限等待 ≤2s 内被级联取消（deny）。
    let report = manager.interrupt(&session.id).await.unwrap();
    assert_eq!(report.interrupted_run, Some(first.run_id.clone()));
    assert_eq!(report.cancelled_waiting_run, None);
    let resolution = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("权限等待应被取消树唤醒（≤2s）")
        .unwrap()
        .unwrap();
    assert_eq!(resolution.decision, PermissionDecision::Deny);
    assert!(!resolution.timed_out, "取消路径不是 300s 超时路径");
    assert!(
        resolution.reason.contains("取消"),
        "取消原因应可审计：{}",
        resolution.reason
    );

    // 会话回 idle、run 终态 cancelled、被中断 run 无终稿事件。
    assert!(
        wait_run_status(&manager, &first.run_id, RunStatus::Cancelled).await,
        "在途 run 应 cancelled"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "interrupt 后会话必须回 idle"
    );
    let readback = core.pipeline.readback(&session.id, 0).await.unwrap();
    let first_events: Vec<_> = readback
        .events
        .iter()
        .filter(|event| event.run_id.as_ref() == Some(&first.run_id))
        .collect();
    assert!(
        first_events
            .iter()
            .any(|event| event.event_type().as_str() == "run.cancelled"),
        "run.cancelled 事件必须落库"
    );
    assert!(
        !first_events
            .iter()
            .any(|event| event.event_type().as_str() == "message.completed"),
        "被中断 run 不得出现终稿事件"
    );
    let cancelled_event = first_events
        .iter()
        .find(|event| event.event_type().as_str() == "run.cancelled")
        .unwrap();
    assert_eq!(
        cancelled_event.payload.to_value().unwrap()["reason"],
        "user_interrupt"
    );

    // 权限取消审计（deny + cancelled）。
    let audits = core.reads().audit_log(100).await.unwrap();
    assert!(
        audits.iter().any(|record| {
            record.action == "permission.cancelled" && record.result.as_deref() == Some("cancelled")
        }),
        "取消路径必须写审计：{:?}",
        audits
            .iter()
            .map(|record| (record.action.as_str(), record.result.as_deref()))
            .collect::<Vec<_>>()
    );

    // 可续聊：第 2 条消息受理 → 完成 → 会话回 idle。
    let second = manager.send(&session.id, "second", "e2e-2").await.unwrap();
    assert!(!second.queued && !second.duplicate);
    executor.release(1);
    assert!(
        wait_run_status(&manager, &second.run_id, RunStatus::Succeeded).await,
        "续聊 run 应成功"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "续聊后会话回 idle"
    );
    assert_eq!(
        m2_support::message_count(&core.reads(), &session.id).await,
        3,
        "用户消息 ×2 + 助手终稿 ×1（被中断 run 无终稿）"
    );
    println!(
        "[m2-05 DoD2] interrupt → idle → 续聊完成；权限等待取消决议：decision={:?} timed_out={} reason={}",
        resolution.decision, resolution.timed_out, resolution.reason
    );
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD2 补充：父会话 dispose 按 `parent_session_id` 级联关闭子/孙会话（D8 父取消）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parent_dispose_cascades_to_child_and_grandchild_sessions() {
    let core = TestCore::open().await;
    let executor = CancellableExecutor::new();
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let parent = create_session(&manager, "parent").await;
    // P0 无子会话创建 API：直接落库构造两层后代（M2-05 级联路径）。
    let child = insert_session_row(&core, CHILD_SESSION, Some(parent.id.as_str())).await;
    let grandchild = insert_session_row(&core, GRANDCHILD_SESSION, Some(child.id.as_str())).await;

    let child_ack = manager.send(&child.id, "child", "child-1").await.unwrap();
    let grandchild_ack = manager
        .send(&grandchild.id, "grandchild", "grandchild-1")
        .await
        .unwrap();
    assert!(
        wait_for(|| executor.call_count() == 2, Duration::from_secs(5)).await,
        "子/孙会话 run 应派发"
    );

    // 父空闲 → completed；后代有在途 run → cancelled（级联）。
    let status = manager.dispose(&parent.id).await.unwrap();
    assert_eq!(status, SessionStatus::Completed);
    assert!(
        wait_run_status(&manager, &child_ack.run_id, RunStatus::Cancelled).await,
        "子会话 run 应被级联取消"
    );
    assert!(
        wait_run_status(&manager, &grandchild_ack.run_id, RunStatus::Cancelled).await,
        "孙会话 run 应被级联取消"
    );
    assert_eq!(
        manager.session_status(&child.id).await.unwrap(),
        SessionStatus::Cancelled
    );
    assert_eq!(
        manager.session_status(&grandchild.id).await.unwrap(),
        SessionStatus::Cancelled
    );
    assert!(
        manager
            .session_cancel_token(&child.id)
            .await
            .unwrap()
            .is_cancelled(),
        "子会话取消节点应级联取消"
    );
    // 终态会话拒绝新 run（级联关闭后不可继续）。
    let error = manager
        .send(&child.id, "after", "child-2")
        .await
        .unwrap_err();
    assert_eq!(error.code(), "session_closed");
    println!(
        "[m2-05 DoD2-级联] parent=completed；child/grandchild run=cancelled；会话=terminal；取消节点已级联"
    );
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD2 补充（D9 交叉）：`waiting_permission` 下的取消路径必须走合法转移
/// （`waiting_permission → running → idle`；M2-10 接线审批等待状态后的前置保障）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_from_waiting_permission_settles_via_running() {
    const WP_SESSION: &str = "01J00000000000000000000W1";
    let core = TestCore::open().await;
    let executor = CancellableExecutor::new();
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    // P0 尚无写入 `waiting_permission` 的编排路径（归 M2-10/M3-03）；直接落库构造。
    let session = m2_support::insert_session_row_with_status(
        &core,
        WP_SESSION,
        None,
        SessionStatus::WaitingPermission,
    )
    .await;
    let ack = manager.send(&session.id, "wp", "wp-1").await.unwrap();
    assert!(
        wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
        "在途 run 应派发"
    );

    let report = manager.interrupt(&session.id).await.unwrap();
    assert_eq!(report.interrupted_run, Some(ack.run_id.clone()));
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Cancelled).await,
        "在途 run 应 cancelled"
    );
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "取消后会话必须回 idle（经合法转移路径）"
    );
    let readback = core.pipeline.readback(&session.id, 0).await.unwrap();
    let changes: Vec<(String, String)> = readback
        .events
        .iter()
        .filter(|event| event.event_type().as_str() == "session.status_changed")
        .map(|event| {
            let payload = event.payload.to_value().unwrap();
            (
                payload["from"].as_str().unwrap().to_owned(),
                payload["to"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        changes.contains(&("waiting_permission".to_owned(), "running".to_owned())),
        "等待审批取消必须先回 running（白名单约束）：{changes:?}"
    );
    assert!(
        changes.contains(&("running".to_owned(), "idle".to_owned())),
        "随后回 idle：{changes:?}"
    );
    println!("[m2-05 DoD2-waiting_permission] 状态转移序列 = {changes:?}");
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// DoD3：不响应任务注入 → 10s 记录任务 dump + 强制清理；dump 可导出（诊断包源）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod3_unresponsive_task_dumped_and_force_cleaned_after_10s() {
    let core = TestCore::open().await;
    let clock = manual_clock(1_000_000);
    let executor = UnresponsiveExecutor::new();
    let manager = build_manager(
        &core,
        clock.clone(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let session = create_session(&manager, "unresponsive").await;
    let ack = manager.send(&session.id, "hang", "hang-1").await.unwrap();
    assert!(
        wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
        "不响应执行器应被派发"
    );

    // 中断：run 行/事件落终态；执行器忽略取消 → 任务仍在册（看门狗计时开始）。
    let report = manager.interrupt(&session.id).await.unwrap();
    assert_eq!(report.interrupted_run, Some(ack.run_id.clone()));
    assert!(
        wait_session_status(&manager, &session.id, SessionStatus::Idle).await,
        "中断后会话回 idle"
    );
    assert_eq!(
        manager.active_task_count(),
        1,
        "不响应执行器 → 任务应仍在册（未被清理）"
    );
    assert!(manager.task_dumps().is_empty());

    // 取消后 9_999ms 不处置；10_000ms 记 dump + 强制清理（D8 阈值）。
    clock.advance(9_999);
    assert!(
        manager.sweep_tasks_once().is_empty(),
        "未到 10s 不得记录 dump/清理"
    );
    assert!(manager.task_dumps().is_empty());
    assert_eq!(manager.active_task_count(), 1);

    clock.advance(1);
    let dumps = manager.sweep_tasks_once();
    assert_eq!(dumps.len(), 1, "恰好 10s 必须记录任务 dump");
    let dump = &dumps[0];
    assert_eq!(
        dump.task,
        format!(
            "session:{} run:{}",
            session.id.as_str(),
            ack.run_id.as_str()
        )
    );
    assert_eq!(dump.session_id, session.id.as_str());
    assert_eq!(dump.run_id, ack.run_id.as_str());
    assert_eq!(dump.started_at_ms, 1_000_000);
    assert_eq!(dump.orphaned_at_ms, 1_000_000);
    assert_eq!(dump.dumped_at_ms, 1_010_000);
    assert_eq!(dump.elapsed_ms, 10_000);
    assert_eq!(dump.action, TASK_DUMP_ACTION_FORCED_CLEANUP);

    // 强制清理（abort）生效后任务出册；dump 保留在诊断缓冲（M3-05 诊断包消费）。
    let exited = wait_for(
        || {
            manager.sweep_tasks_once();
            manager.active_task_count() == 0
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(exited, "强制清理后任务必须出册");
    assert_eq!(manager.task_dumps(), dumps, "dump 不得因出册丢失/重复");
    assert!(
        manager.sweep_tasks_once().is_empty(),
        "重复巡检不得重复记录 dump"
    );
    let dump_json = serde_json::to_value(dump).unwrap();
    assert_eq!(dump_json["action"], "forced_cleanup");
    assert_eq!(dump_json["elapsed_ms"], 10_000);
    println!("[m2-05 DoD3] task dump（诊断包源）= {dump_json}");

    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}
