//! M2-04 集成：背压分级与 journal 补读（设计 D8、评审 #4/#9；ADR-003/ADR-004）。
//!
//! 覆盖 DoD：
//!   1) L2：写队列 >4096 → 新 run 拒绝（`storage_backpressure`），已有 run 不受影响（故障注入）；
//!   2) L3：控制投递 >会话数×5000 → 熔断 + 30s 后自动解除；重启后从 journal 恢复投递、
//!      零丢失（集成）；
//!   3) 订阅者人为减速：心跳与中断请求响应 ≤2s（reader 不被阻塞证明）；
//!   4) `Lagged(k)` → 补读最终一致（集成）；
//!   5) 存储侧背压例外：暂停 ≤2s → 熔断隔离（`degraded + status_reason=storage_backpressure`）
//!      → 队列回落 ≤1024 持续 30s 自动解除；与 `persist_degraded` 修复路径严格区分。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
mod m2_support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_control::{
    BackpressureConfig, BackpressureController, Clock, ControlReadGate, DeliveryPoll,
    IsolationReason, IsolationSink, LifecycleConfig, ManualClock, PipelineConfig, PressurePhase,
    StorageState,
};
use aether_core::{EventEnvelope, RunStatus, RuntimeId, SessionId};
use aether_store::WriteQueueConfig;
use tokio::runtime::Handle;

use common::{log_envelope, log_event, start_pipeline_with, FakeJournal, SESSION_A};
use m2_support::{
    build_manager, create_session, message_count, system_clock, wait_run_status, ScriptedExecutor,
    TestCore,
};

/// 记录型隔离出口（故障注入替身；生产实现见 `aether-tauri::isolation`）。
#[derive(Default)]
struct RecordingSink {
    isolations: Mutex<Vec<(String, String, String)>>,
    releases: Mutex<Vec<String>>,
    release_ok: AtomicU64,
}

impl RecordingSink {
    fn isolations(&self) -> Vec<(String, String, String)> {
        self.isolations.lock().unwrap().clone()
    }

    fn releases(&self) -> Vec<String> {
        self.releases.lock().unwrap().clone()
    }
}

impl IsolationSink for RecordingSink {
    fn isolate(&self, runtime_id: &RuntimeId, reason: IsolationReason, detail: &str) -> bool {
        self.isolations.lock().unwrap().push((
            runtime_id.as_str().to_owned(),
            reason.detail_code().to_owned(),
            detail.to_owned(),
        ));
        true
    }

    fn release(&self, runtime_id: &RuntimeId) -> bool {
        self.releases
            .lock()
            .unwrap()
            .push(runtime_id.as_str().to_owned());
        self.release_ok.load(Ordering::Relaxed) == 0
    }
}

fn runtime_id() -> RuntimeId {
    RuntimeId::new("mock").unwrap()
}

fn session_id() -> SessionId {
    SessionId::new(SESSION_A).unwrap()
}

/// 直接写入写队列的批量事件（故障注入：绕过管线制造可控积压）。
fn backlog_events(session: &str, start: u64, count: u64) -> Vec<EventEnvelope> {
    (start..start + count)
        .map(|seq| log_envelope(&format!("01J000000000000000000D{seq:04}"), session, seq))
        .collect()
}

// ===== DoD1：L2 写队列 >4096 → 新 run 拒绝；已有 run 不受影响 =====

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod1_l2_rejects_new_run_and_existing_run_continues() {
    // 写队列注入 300ms 提交延迟：单作业 4200 条制造 >4096 积压。
    let core = TestCore::open_with(
        WriteQueueConfig {
            commit_delay: Duration::from_millis(300),
            ..WriteQueueConfig::default()
        },
        PipelineConfig::default(),
    )
    .await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let session = create_session(&manager, "m2-04-dod1").await;

    // 已有 run：执行器阻塞，run.started 已落盘。
    let ack = manager.send(&session.id, "第一条", "dod1-1").await.unwrap();
    assert!(!ack.queued);
    assert!(
        m2_support::wait_session_status(&manager, &session.id, aether_core::SessionStatus::Running)
            .await,
        "已有 run 必须处于执行态"
    );

    // 故障注入：4200 条积压 → 深度 >4096（D8 L2）。
    let queue = core.write();
    let submission = {
        let queue = queue.clone();
        tokio::spawn(async move {
            queue
                .append_events(backlog_events("01J0000000000000000000ZZ", 0, 4_200))
                .await
        })
    };
    assert!(
        common::wait_for(|| queue.depth() > 4_096, Duration::from_secs(5)).await,
        "积压必须超过 L2 阈值（当前深度 {}）",
        queue.depth()
    );
    assert_eq!(
        queue.pressure_level(),
        Some(aether_store::QueuePressureLevel::L2)
    );

    // 新 run 拒绝：storage_backpressure，且拒绝先于持久化（无孤儿消息）。
    let error = manager
        .send(&session.id, "第二条", "dod1-2")
        .await
        .expect_err("L2 积压期必须拒绝新 run");
    assert_eq!(error.code(), "storage_backpressure");
    assert_eq!(
        message_count(&core.reads(), &session.id).await,
        1,
        "被拒消息不得落库"
    );

    // 已有 run 不受影响：放行执行器，积压 drain 后 run 正常完成。
    executor.release(1);
    submission.await.unwrap().unwrap();
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Succeeded).await,
        "已有 run 必须正常完成"
    );
    assert!(
        common::wait_for(|| queue.depth() == 0, Duration::from_secs(5)).await,
        "队列必须回落到 0（临时高水位不改变存储状态；当前 {}）",
        queue.depth()
    );
    assert!(
        manager.send(&session.id, "第三条", "dod1-3").await.is_ok(),
        "回落恢复后新 run 可受理"
    );
}

// ===== DoD2：L3 控制投递熔断 → 30s 解除 → journal 恢复投递、零丢失 =====

#[tokio::test]
async fn dod2_l3_circuit_release_and_journal_zero_loss() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline_with(&journal, PipelineConfig::default());
    let clock = Arc::new(ManualClock::new(1_000_000));
    let sink = Arc::new(RecordingSink::default());
    let config = BackpressureConfig {
        delivery_per_session_items: 4,
        delivery_max_bytes: 4 * 1024,
        ..BackpressureConfig::default()
    };
    let controller = BackpressureController::start(
        config,
        clock.clone(),
        Arc::clone(&sink) as Arc<dyn IsolationSink>,
        &pipeline,
        &Handle::current(),
    )
    .unwrap();
    controller.register_session(&runtime_id(), &session_id());

    // 6 条已落盘事件；投递泵摄入后超限（会话数 1 × 4 条）。
    for seq in 1..=6u64 {
        pipeline
            .submit(log_event(
                &format!("01J000000000000000000E{seq:04}"),
                SESSION_A,
            ))
            .await
            .unwrap();
    }
    assert!(
        common::wait_for(
            || controller.metrics().delivery_overflows >= 1,
            Duration::from_secs(5)
        )
        .await,
        "投递超限必须被观察"
    );
    assert_eq!(controller.metrics().delivery_circuits_open, 1);
    let isolations = sink.isolations();
    assert_eq!(isolations.len(), 1);
    assert_eq!(isolations[0].1, "delivery_backlog", "L3 熔断来源");

    // 熔断期准入拒绝（拒绝新会话/新 run）。
    assert_eq!(
        controller.admission(&runtime_id()).unwrap_err().code(),
        "storage_backpressure"
    );

    // 零丢失：补读锚点 = 已确认 seq + 1，从 journal 补齐全部事件。
    let from_seq = match controller.poll(&runtime_id()) {
        DeliveryPoll::ReadbackRequired { from_seq, .. } => from_seq,
        other => panic!("必须切换 journal 补读模式: {other:?}"),
    };
    let frame = pipeline
        .readback(&session_id(), from_seq - 1)
        .await
        .unwrap();
    let seqs: Vec<u64> = frame.events.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, (from_seq..=6).collect::<Vec<_>>(), "补读零缺口");
    assert!(frame.complete);
    controller.ack_readback(&runtime_id(), &session_id(), 6);

    // 30s 后自动解除（重启适配器），准入恢复。
    clock.advance(30_000);
    controller.observe(clock.now_ms(), 0, StorageState::Normal);
    assert_eq!(sink.releases().len(), 1, "30s 后必须自动解除");
    assert!(controller.admission(&runtime_id()).is_ok());
    assert_eq!(controller.metrics().delivery_restarts, 1);

    // 重启后从 journal 恢复投递：新事件正常投递（不再补读）。
    pipeline
        .submit(log_event("01J000000000000000000E0007", SESSION_A))
        .await
        .unwrap();
    assert!(
        common::wait_for(
            || matches!(
                controller.poll(&runtime_id()),
                DeliveryPoll::Events(ref events) if events.iter().any(|event| event.seq == 7)
            ),
            Duration::from_secs(5)
        )
        .await,
        "解除后新事件必须恢复投递"
    );
    controller.shutdown().await;
}

// ===== DoD3：订阅者人为减速 → 心跳与中断请求 ≤2s（reader 不被阻塞） =====

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod3_slow_subscriber_does_not_block_reader_health_or_interrupt() {
    // 广播容量压缩到 32：慢订阅者快速 lag；reader（管线入口）不得被阻塞。
    let core = TestCore::open_with(
        WriteQueueConfig::default(),
        PipelineConfig {
            broadcast_capacity: 32,
            ..PipelineConfig::default()
        },
    )
    .await;
    let executor = ScriptedExecutor::new(0);
    let manager = build_manager(
        &core,
        system_clock(),
        executor.clone(),
        LifecycleConfig::default(),
    );
    let session = create_session(&manager, "m2-04-dod3").await;
    let ack = manager
        .send(&session.id, "慢消费者", "dod3-1")
        .await
        .unwrap();

    // 人为减速订阅者：只订阅不消费（永不 recv）。
    let _slow = core.pipeline.subscribe();
    // 背压控制器作为第二个消费者：同样不 poll（默认阈值不触发 L3）。
    let controller = BackpressureController::start(
        BackpressureConfig::default(),
        system_clock(),
        Arc::new(aether_control::NoopIsolationSink),
        &core.pipeline,
        &Handle::current(),
    )
    .unwrap();

    // 洪泛 400 条事件：逐条提交延迟（reader 不被下游消费者反压）。
    let mut worst = Duration::ZERO;
    let flood_start = Instant::now();
    for seq in 1..=400u64 {
        let started = Instant::now();
        core.pipeline
            .submit(log_event(
                &format!("01J000000000000000000F{seq:04}"),
                SESSION_A,
            ))
            .await
            .unwrap();
        worst = worst.max(started.elapsed());
    }
    assert!(
        worst <= Duration::from_secs(2),
        "reader 不得被慢消费者阻塞（最慢提交 {worst:?}，总耗时 {:?}）",
        flood_start.elapsed()
    );

    // 心跳（health 轮询）响应 ≤2s。
    let started = Instant::now();
    let health = core.pipeline.health();
    assert!(
        started.elapsed() <= Duration::from_secs(2),
        "health 必须 ≤2s 返回"
    );
    assert_eq!(health.storage_state_code(), "normal");
    // 控制器仍可正常观察投递（未消费积压仅占用内存，不反压）。
    assert!(controller.metrics().delivery_events > 0);

    // 中断请求响应 ≤2s 且生效。
    let started = Instant::now();
    let report = manager.interrupt(&session.id).await.unwrap();
    assert!(
        started.elapsed() <= Duration::from_secs(2),
        "interrupt 必须 ≤2s 返回（实测 {:?}）",
        started.elapsed()
    );
    assert_eq!(report.interrupted_run, Some(ack.run_id.clone()));
    assert!(
        wait_run_status(&manager, &ack.run_id, RunStatus::Cancelled).await,
        "中断必须生效"
    );
    controller.shutdown().await;
}

// ===== DoD4：Lagged(k) → 补读最终一致 =====

#[tokio::test]
async fn dod4_lagged_readback_is_eventually_consistent() {
    let journal = FakeJournal::new();
    let pipeline = start_pipeline_with(
        &journal,
        PipelineConfig {
            broadcast_capacity: 8,
            ..PipelineConfig::default()
        },
    );
    let clock = Arc::new(ManualClock::new(1_000_000));
    let sink = Arc::new(RecordingSink::default());
    // 投递泵人为减速（15ms/条）：60 条提交期间广播必然 lag。
    let controller = BackpressureController::start(
        BackpressureConfig {
            ingest_delay: Duration::from_millis(15),
            ..BackpressureConfig::default()
        },
        clock,
        Arc::clone(&sink) as Arc<dyn IsolationSink>,
        &pipeline,
        &Handle::current(),
    )
    .unwrap();
    controller.register_session(&runtime_id(), &session_id());

    for seq in 1..=60u64 {
        pipeline
            .submit(log_event(
                &format!("01J000000000000000000G{seq:04}"),
                SESSION_A,
            ))
            .await
            .unwrap();
    }
    assert!(
        common::wait_for(
            || controller.metrics().delivery_lagged_events > 0,
            Duration::from_secs(10)
        )
        .await,
        "慢投递泵必须触发广播 Lagged"
    );
    assert_eq!(
        controller.metrics().delivery_circuits_open,
        0,
        "Lagged 不熔断（仅切换补读）"
    );
    assert!(sink.isolations().is_empty());

    // 消费循环：补读 + 内存投递，直到 1..=60 全部到手（最终一致）。
    let mut collected: Vec<u64> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match controller.poll(&runtime_id()) {
            DeliveryPoll::ReadbackRequired { from_seq, .. } => {
                let frame = pipeline
                    .readback(&session_id(), from_seq - 1)
                    .await
                    .unwrap();
                if !frame.complete {
                    continue;
                }
                let max = frame
                    .events
                    .last()
                    .map(|event| event.seq)
                    .unwrap_or(from_seq - 1);
                collected.extend(frame.events.iter().map(|event| event.seq));
                controller.ack_readback(&runtime_id(), &session_id(), max);
            }
            DeliveryPoll::Events(events) => {
                let max = events.last().map(|event| event.seq).unwrap_or(0);
                collected.extend(events.iter().map(|event| event.seq));
                controller.ack(&runtime_id(), &session_id(), max);
            }
            DeliveryPoll::Empty => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        if collected.len() >= 60 {
            break;
        }
    }
    collected.sort_unstable();
    collected.dedup();
    assert_eq!(
        collected,
        (1..=60).collect::<Vec<u64>>(),
        "Lagged 后补读必须最终一致（无缺口）"
    );
    controller.shutdown().await;
}

// ===== DoD5：存储侧背压例外（暂停 ≤2s → 隔离 → 30s 解除；persist_degraded 区分） =====

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod5_storage_pressure_isolates_then_release_and_persist_degraded_is_distinct() {
    let core = TestCore::open().await;
    let executor = ScriptedExecutor::new(4);
    let manager = build_manager(&core, system_clock(), executor, LifecycleConfig::default());
    let clock = Arc::new(ManualClock::new(1_000_000));
    let sink = Arc::new(RecordingSink::default());
    let controller = BackpressureController::new(
        BackpressureConfig::default(),
        clock.clone(),
        Arc::clone(&sink) as Arc<dyn IsolationSink>,
    )
    .unwrap();
    let manager = manager.with_backpressure(controller.clone());
    let session = create_session(&manager, "m2-04-dod5").await;
    controller.register_session(&session.runtime_id, &session.id);

    // 写队列临时高水位（故障注入）：控制读取暂停，剩余窗口 ≤2s。
    controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
    assert_eq!(controller.phase(&session.runtime_id), PressurePhase::Paused);
    match controller.control_read_gate(&session.runtime_id) {
        ControlReadGate::Paused { remaining_ms } => {
            assert!(remaining_ms <= 2_000, "单次暂停 ≤2s：{remaining_ms}");
        }
        other => panic!("压力下必须暂停：{other:?}"),
    }

    // 连续 3 次 2s 暂停超时 → 隔离（degraded + storage_backpressure）。
    for _ in 0..3 {
        clock.advance(2_000);
        controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
        if controller.phase(&session.runtime_id) == PressurePhase::Isolated {
            break;
        }
        controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
    }
    assert_eq!(
        controller.phase(&session.runtime_id),
        PressurePhase::Isolated
    );
    assert_eq!(controller.metrics().storage_isolations, 1);
    let isolations = sink.isolations();
    assert_eq!(isolations.len(), 1);
    assert_eq!(isolations[0].1, "storage_backpressure");
    assert!(isolations[0]
        .2
        .contains("status_reason=storage_backpressure"));

    // 隔离期拒绝新会话/新 run（SessionManager 接线生效）。
    let error = manager
        .send(&session.id, "隔离期", "dod5-1")
        .await
        .expect_err("隔离期必须拒绝新 run");
    assert_eq!(error.code(), "storage_backpressure");

    // 队列回落 ≤1024 持续 30s → 自动解除（仅队列维度）。
    controller.observe(clock.now_ms(), 1_024, StorageState::Normal);
    clock.advance(29_999);
    controller.observe(clock.now_ms(), 1_024, StorageState::Normal);
    assert!(sink.releases().is_empty(), "30s 前不得解除");
    clock.advance(1);
    controller.observe(clock.now_ms(), 1_024, StorageState::Normal);
    assert_eq!(sink.releases().len(), 1);
    assert_eq!(controller.phase(&session.runtime_id), PressurePhase::Normal);
    assert!(
        manager.send(&session.id, "解除后", "dod5-2").await.is_ok(),
        "解除后新 run 可受理"
    );

    // 再次隔离后进入 persist_degraded：不适用队列自动解除路径（D4 修复 + 重启 + 自检）。
    controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
    for _ in 0..3 {
        clock.advance(2_000);
        controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
        if controller.phase(&session.runtime_id) == PressurePhase::Isolated {
            break;
        }
        controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
    }
    assert_eq!(
        controller.phase(&session.runtime_id),
        PressurePhase::Isolated
    );
    let releases_before = sink.releases().len();
    controller.observe(clock.now_ms(), 0, StorageState::PersistDegraded);
    assert_eq!(
        controller.phase(&session.runtime_id),
        PressurePhase::PersistDegraded
    );
    for _ in 0..4 {
        clock.advance(30_000);
        controller.observe(clock.now_ms(), 0, StorageState::PersistDegraded);
    }
    assert_eq!(
        sink.releases().len(),
        releases_before,
        "persist_degraded 不得走队列自动解除"
    );
    let error = manager
        .send(&session.id, "降级期", "dod5-3")
        .await
        .expect_err("降级期拒绝新 run");
    assert_eq!(error.code(), "persist_degraded", "两态错误码严格区分");
}
