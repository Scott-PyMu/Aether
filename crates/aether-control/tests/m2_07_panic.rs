//! M2-07 DoD1 集成测试：会话执行任务 panic 隔离（设计 D2）。
//!
//! 断言口径：
//! - 任务 panic 由 `JoinError` 捕获记录，**仅该会话** run 标 `failed`（`task_panic`）；
//! - 其余会话事件流连续（广播订阅 + 补读回读均无缺口）；
//! - panic 会话可继续发送（回 `idle` 后续聊）；
//! - panic 后若等待队列已提升，重新 spawn 该会话任务继续执行（run 串行不变）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_control::{ExecutorFuture, ExecutorOutcome, LifecycleConfig, RunExecutor, RunRequest};
use aether_core::{RunStatus, SessionStatus};
use tokio::sync::Semaphore;

use m2_support::{
    build_manager, create_session, manual_clock, message_count, wait_for, wait_run_status,
    wait_session_status, TestCore,
};

/// 选择性 panic 执行器：`panic_text` 对应的 run 首次执行 panic，其余返回 `Completed`。
///
/// 按**输入文本**选择 panic 目标（而非「全局第 N 次调用」）：会话任务经 `JoinSet`
/// 并发调度，跨会话的 execute 调用顺序不确定，全局计数会把 panic 打到其他会话
/// （CI annotation：`m2_07_panic.rs:142 panic 会话 run 必须标 failed` 的根因）。
struct PanicExecutor {
    calls: Mutex<Vec<RunRequest>>,
    panic_text: String,
    panicked: AtomicBool,
    /// 可选门控：执行前等待许可（控制 panic 发生的时点）。
    permits: Option<Arc<Semaphore>>,
}

impl PanicExecutor {
    fn new(panic_text: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            panic_text: panic_text.to_owned(),
            panicked: AtomicBool::new(false),
            permits: None,
        })
    }

    fn gated(panic_text: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            panic_text: panic_text.to_owned(),
            panicked: AtomicBool::new(false),
            permits: Some(Arc::new(Semaphore::new(0))),
        })
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn run_ids(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.run_id.to_string())
            .collect()
    }

    fn release(&self, count: usize) {
        if let Some(permits) = &self.permits {
            permits.add_permits(count);
        }
    }
}

impl RunExecutor for PanicExecutor {
    fn execute(&self, request: RunRequest) -> ExecutorFuture<'_> {
        self.calls.lock().unwrap().push(request.clone());
        let permits = self.permits.clone();
        let should_panic =
            request.text == self.panic_text && !self.panicked.swap(true, Ordering::SeqCst);
        let run_label = request.run_id.to_string();
        Box::pin(async move {
            if let Some(permits) = permits {
                let _permit = permits.acquire().await;
            }
            if should_panic {
                // 在 future poll 期间 panic：由 JoinSet 的 JoinError 捕获，不传染其他任务。
                panic!("注入 panic（M2-07 DoD1）：run {run_label}");
            }
            ExecutorOutcome::Completed {
                assistant_text: Some(format!("completed after panic isolation: {run_label}")),
                usage: None,
            }
        })
    }
}

/// 收割循环：持续驱动 `reap_run_tasks_once` 直到 `run_id` 达到期望终态。
///
/// 一轮收割可能**先**拿到其他会话的正常完成（`reaped > 0` 但不含 panic 任务）；
/// panic 任务进入 JoinSet 完成队列的时点由调度决定（CI 插桩/满载下晚于正常完成）。
/// 生产由后台看门狗周期收割；测试无后台任务，必须持续收割直至 panic 落终态，
/// 否则断言退化为调度时序赌博。
async fn reap_until_run_status(
    manager: &aether_control::SessionManager,
    run_id: &aether_core::RunId,
    expected: RunStatus,
    deadline: Duration,
) -> bool {
    let started = tokio::time::Instant::now();
    loop {
        let _ = manager.reap_run_tasks_once().await;
        if manager
            .run(run_id)
            .await
            .ok()
            .flatten()
            .map(|run| run.status)
            == Some(expected)
        {
            return true;
        }
        if started.elapsed() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// DoD1：panic 仅该会话 failed，其余会话事件流连续；panic 会话可继续发送。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod1_panic_isolates_session_and_other_streams_continue() {
    let core = TestCore::open().await;
    let executor = PanicExecutor::new("boom");
    let manager = build_manager(
        &core,
        manual_clock(1_000),
        Arc::clone(&executor) as Arc<dyn RunExecutor>,
        LifecycleConfig::default(),
    );
    let mut events = core.pipeline.subscribe();

    let session_a = create_session(&manager, "panic-a").await;
    let session_b = create_session(&manager, "panic-b").await;

    let ack_a = manager
        .send(&session_a.id, "boom", "client-a-1")
        .await
        .unwrap();
    let ack_b = manager
        .send(&session_b.id, "hello", "client-b-1")
        .await
        .unwrap();

    // 收割：panic 任务被 JoinError 捕获并隔离恢复（持续收割直至落终态）。
    assert!(
        reap_until_run_status(
            &manager,
            &ack_a.run_id,
            RunStatus::Failed,
            Duration::from_secs(10)
        )
        .await,
        "panic 会话 run 必须经 JoinError 收割路径标 failed（task_panic）"
    );
    assert!(
        wait_run_status(&manager, &ack_b.run_id, RunStatus::Succeeded).await,
        "其余会话 run 必须不受影响"
    );
    assert!(wait_session_status(&manager, &session_a.id, SessionStatus::Idle).await);
    assert!(wait_session_status(&manager, &session_b.id, SessionStatus::Idle).await);

    // 终态错误码 = task_panic（D2「JoinError 记录 + 会话标 failed」）。
    let run_a = manager.run(&ack_a.run_id).await.unwrap().unwrap();
    assert_eq!(run_a.status, RunStatus::Failed);
    assert_eq!(
        run_a.error.as_deref(),
        Some("task_panic"),
        "panic 终态错误码必须为 task_panic"
    );

    // 事件流断言：session B 的 seq 连续（无缺口）；两会话均收到终态事件。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut b_seqs = Vec::new();
    let mut a_failed = false;
    let mut b_completed = false;
    while (!a_failed || !b_completed) && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), events.recv()).await {
            Ok(Ok(envelope)) => {
                if envelope.session_id == session_a.id
                    && envelope.event_type().as_str() == "run.failed"
                {
                    a_failed = true;
                }
                if envelope.session_id == session_b.id {
                    if envelope.event_type().as_str() == "run.completed" {
                        b_completed = true;
                    }
                    b_seqs.push(envelope.seq);
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
            Err(_elapsed) => continue,
        }
    }
    assert!(a_failed, "panic 会话必须落 run.failed 事件");
    assert!(b_completed, "其余会话必须落 run.completed 事件");
    assert!(!b_seqs.is_empty(), "其余会话事件流必须连续可观测");
    let expected: Vec<u64> = (1..=b_seqs.len() as u64).collect();
    assert_eq!(b_seqs, expected, "其余会话 seq 必须无缺口");

    // 管线 sequencer 恢复（panic 隔离路径调用 restart_session）。
    let health = core.pipeline.health();
    assert!(
        health.sequencer_restarts >= 1,
        "panic 后必须执行 sequencer 恢复"
    );

    // panic 会话回 idle 后可继续发送并完成（DoD1 人工恢复口径：可重试/续聊）。
    let ack_a2 = manager
        .send(&session_a.id, "again", "client-a-2")
        .await
        .unwrap();
    assert!(
        wait_run_status(&manager, &ack_a2.run_id, RunStatus::Succeeded).await,
        "panic 后同会话必须可继续发送并完成"
    );
    assert_eq!(
        message_count(&core.reads(), &session_a.id).await,
        3,
        "用户消息 ×2 + panic 后助手续跑终稿 ×1"
    );
    assert_eq!(executor.call_count(), 3, "A 两次 + B 一次");

    println!(
        "[m2-07 DoD1] panic 隔离：A run={} → {}（task_panic）；B run={} → succeeded；\
         sequencer_restarts={}；A 续聊完成；B 事件流 seq 连续 {} 条",
        ack_a.run_id,
        run_a.status,
        ack_b.run_id,
        health.sequencer_restarts,
        b_seqs.len()
    );
}

/// DoD1 扩展：panic 时等待队列已提升 → 恢复后重新 spawn 继续执行（run 串行不变）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod1_panic_promotes_and_respawns_waiting_run() {
    let core = TestCore::open().await;
    let executor = PanicExecutor::gated("first");
    let manager = build_manager(
        &core,
        manual_clock(1_000),
        Arc::clone(&executor) as Arc<dyn RunExecutor>,
        LifecycleConfig::default(),
    );

    let session = create_session(&manager, "panic-queue").await;
    let ack_first = manager
        .send(&session.id, "first", "client-q-1")
        .await
        .unwrap();

    // 等第一条 run 进入执行器（持有许可前阻塞）。
    assert!(
        wait_for(|| executor.call_count() == 1, Duration::from_secs(5)).await,
        "第一条 run 必须进入执行器"
    );
    // 第二条进入等待队列（active 已被占用）。
    let ack_second = manager
        .send(&session.id, "second", "client-q-2")
        .await
        .unwrap();
    assert!(ack_second.queued, "第二条必须进入等待队列");

    // 放行第一条：panic → 收割恢复 → 提升并重新 spawn 执行第二条。
    executor.release(1);
    assert!(
        reap_until_run_status(
            &manager,
            &ack_first.run_id,
            RunStatus::Failed,
            Duration::from_secs(10)
        )
        .await,
        "panic run 必须经 JoinError 收割并标 failed（task_panic）"
    );
    // 放行提升后的第二条。
    executor.release(1);
    assert!(
        wait_run_status(&manager, &ack_second.run_id, RunStatus::Succeeded).await,
        "提升的等待 run 必须在重新 spawn 后完成"
    );
    assert!(wait_session_status(&manager, &session.id, SessionStatus::Idle).await);
    assert_eq!(executor.call_count(), 2, "两次执行（panic + 提升重跑）");

    let first = manager.run(&ack_first.run_id).await.unwrap().unwrap();
    assert_eq!(first.error.as_deref(), Some("task_panic"));
    let ids = executor.run_ids();
    assert_eq!(ids[0], ack_first.run_id.to_string());
    assert_eq!(ids[1], ack_second.run_id.to_string());

    println!(
        "[m2-07 DoD1+] panic 提升重跑：first={} failed(task_panic) → second={} succeeded",
        ack_first.run_id, ack_second.run_id
    );
}
