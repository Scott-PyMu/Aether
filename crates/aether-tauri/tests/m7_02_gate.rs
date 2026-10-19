//! M7-02：并发闸门（ADR-022 附录 A-5/A-6；DoD3/DoD4）。
//!
//! 覆盖：
//! - 参数注入：`clamp(floor(free_ram_gb/1.5), 2, 10)`（8GB 模拟 = 7.0 → 4；16GB → 10）；
//! - 超限排队：全局 FIFO + `session_send` 响应 `queue: { position, limit }`；
//! - `health.concurrency_gate: { limit, active, queued }`（ADR-018 附录 A-5）；
//! - 不超卖：`active ≤ limit` 恒成立；run 终态按 FIFO 顺序推进（单写者）；
//! - 双层队列合成（A-6 必测矩阵）：等待槽提升、`queued` 时再发消息（至多 1 条）、
//!   `queued` run 被 interrupt（等待槽不连带清空）、dispose 全清；
//! - `run_retry` 同样受闸门约束（ADR-022 实现要点 3）；
//! - T3 口径 10 并发基线（16GB → limit 10 不排队）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_adapters::supervisor::Supervisor;
use aether_control::{
    ConcurrencyGateCell, ExecutorFuture, ExecutorOutcome, FixedFreeRam, LifecycleConfig,
    RunExecutor, RunRequest, SessionManager, SystemClock,
};
use aether_core::{RunId, RunStatus, SessionId, SessionStatus};
use aether_store::{ReadPool, WriteQueue};
use aether_tauri::core_health::{boot_core_full_with_gate, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::IpcBackend;
use aether_tauri::ipc::dto::{RunRetryRequest, SessionCreateRequest, SessionSendRequest};
use aether_tauri::runtime_control::{boot_supervisor, mock_spec};
use aether_tauri::session_backend::SessionBackend;
use serde_json::{json, Value};
use tempfile::TempDir;

/// 受控执行器：记录调用顺序；每个 run 持有独立放行通道（**逐 run 显式放行**，
/// 放行即完成——不共享许可，避免完成回调形成连锁推进，保证 FIFO 断言确定性）。
struct ControlledExecutor {
    calls: Mutex<Vec<RunRequest>>,
    waiters: Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>,
}

impl ControlledExecutor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            waiters: Mutex::new(HashMap::new()),
        })
    }

    fn calls(&self) -> Vec<RunRequest> {
        match self.calls.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn call_count(&self) -> usize {
        match self.calls.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    fn run_ids(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|call| call.run_id.to_string())
            .collect()
    }

    /// 放行指定 run（使其执行以 `Completed` 收口）。返回是否命中在途等待者。
    fn release_run(&self, run_id: &str) -> bool {
        let waiter = match self.waiters.lock() {
            Ok(mut guard) => guard.remove(run_id),
            Err(poisoned) => poisoned.into_inner().remove(run_id),
        };
        match waiter {
            Some(sender) => sender.send(()).is_ok(),
            None => false,
        }
    }
}

impl RunExecutor for ControlledExecutor {
    fn execute(&self, request: RunRequest) -> ExecutorFuture<'_> {
        let run_id = request.run_id.to_string();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        match self.calls.lock() {
            Ok(mut guard) => guard.push(request),
            Err(poisoned) => poisoned.into_inner().push(request),
        }
        match self.waiters.lock() {
            Ok(mut guard) => {
                guard.insert(run_id, sender);
            }
            Err(poisoned) => {
                poisoned.into_inner().insert(run_id, sender);
            }
        }
        Box::pin(async move {
            let _ = receiver.await;
            ExecutorOutcome::Completed {
                assistant_text: Some("ok".to_owned()),
                usage: None,
            }
        })
    }
}

struct Harness {
    #[allow(dead_code)]
    dir: TempDir,
    #[allow(dead_code)]
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    #[allow(dead_code)]
    pipeline: aether_control::EventPipeline,
    #[allow(dead_code)]
    reads: ReadPool,
    #[allow(dead_code)]
    write: WriteQueue,
    manager: SessionManager,
    backend: Arc<dyn IpcBackend>,
    executor: Arc<ControlledExecutor>,
    #[allow(dead_code)]
    gate_cell: ConcurrencyGateCell,
    #[allow(dead_code)]
    supervisor: Arc<Supervisor>,
}

/// 组装闸门测试夹具（`free_ram_gb` 参数注入；health 走真实 `concurrency_gate` 数据源）。
fn harness(free_ram_gb: f64) -> Harness {
    let dir = TempDir::new().expect("临时目录");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio 运行时");
    let handle = runtime.handle().clone();
    let gate_cell = ConcurrencyGateCell::new();
    let CoreBoot {
        backend: core,
        storage: slot,
        reads,
        write,
        pipeline,
    } = boot_core_full_with_gate(
        dir.path(),
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
        Some(gate_cell.clone()),
    )
    .expect("启动核心（存储 + 管线）");
    let supervisor = Arc::new(
        boot_supervisor(
            vec![mock_spec("mock", "definitely-missing-adapter-binary")],
            Some(&dir.path().join("adapters.json")),
        )
        .expect("构造监督器"),
    );
    let executor = ControlledExecutor::new();
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        executor.clone(),
    )
    .with_free_ram(Arc::new(FixedFreeRam::new(free_ram_gb)))
    .with_concurrency_gate(gate_cell.clone());
    let backend: Arc<dyn IpcBackend> = Arc::new(SessionBackend::new(
        Arc::new(core),
        Some(manager.clone()),
        None,
        Some(reads.clone()),
        Some(Arc::clone(&supervisor)),
        handle,
    ));
    Harness {
        dir,
        runtime,
        slot,
        pipeline,
        reads,
        write,
        manager,
        backend,
        executor,
        gate_cell,
        supervisor,
    }
}

fn create_session(h: &Harness, title: &str) -> String {
    let created = h
        .backend
        .session_create(&SessionCreateRequest {
            runtime_id: Some("mock".to_owned()),
            title: title.to_owned(),
            workspace_id: None,
            model: None,
            thinking_depth: None,
            permission_mode: None,
        })
        .expect("创建会话");
    created["id"].as_str().expect("会话 id").to_owned()
}

fn send_message(h: &Harness, session_id: &str, client_id: &str) -> Value {
    h.backend
        .session_send(&SessionSendRequest {
            session_id: session_id.to_owned(),
            text: "hi".to_owned(),
            client_msg_id: client_id.to_owned(),
            thinking_depth: None,
        references: Vec::new(),
            })
        .expect("发送")
}

fn client_id(index: usize) -> String {
    format!("01J000000000000000000{:05}", index % 100000)
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

fn gate(h: &Harness) -> aether_control::ConcurrencyGateSnapshot {
    h.runtime.block_on(h.manager.concurrency_gate())
}

fn run_status(h: &Harness, run_id: &str) -> Option<RunStatus> {
    let run_id = RunId::new(run_id.to_owned()).expect("run id");
    h.runtime
        .block_on(h.manager.run(&run_id))
        .expect("读取 run")
        .map(|run| run.status)
}

/// DoD3/DoD4：参数注入（8GB 模拟 7.0 → 4）+ 超限排队 + 响应 `queue` +
/// `health.concurrency_gate` + 不超卖 + FIFO 逐项推进（逐 run 放行，确定性）。
#[test]
fn gate_injection_queueing_health_and_fifo_advance() {
    let h = harness(7.0);
    assert_eq!(
        h.runtime.block_on(h.manager.refresh_gate_limit()),
        Some(4),
        "8GB 模拟：clamp(floor(7/1.5),2,10) = 4"
    );

    let mut sessions = Vec::new();
    let mut acks = Vec::new();
    for index in 0..6 {
        let session_id = create_session(&h, &format!("gate-{index}"));
        let ack = send_message(&h, &session_id, &client_id(index));
        sessions.push(session_id);
        acks.push(ack);
    }
    // 前 4 个立即派发；第 5/6 个进入全局 FIFO（position 1/2，limit 4）。
    for ack in acks.iter().take(4) {
        assert!(ack.get("queue").is_none(), "前 4 个 run 不应排队：{ack}");
        assert_eq!(ack["queued"], false);
    }
    assert_eq!(acks[4]["queue"], json!({ "position": 1, "limit": 4 }));
    assert_eq!(acks[5]["queue"], json!({ "position": 2, "limit": 4 }));
    assert!(
        wait_until(Duration::from_secs(10), || h.executor.call_count() == 4),
        "不超卖：仅 4 个执行器调用（当前 {}）",
        h.executor.call_count()
    );

    // health.concurrency_gate（ADR-018 附录 A-5）。
    let health = h.backend.health().expect("health");
    assert_eq!(
        health["concurrency_gate"],
        json!({ "limit": 4, "active": 4, "queued": 2 })
    );

    // 放行 run#1 → 终态触发 FIFO 队首推进（单写者顺序；仍 4 active，不超卖）。
    let first_run = acks[0]["run_id"].as_str().expect("run id").to_owned();
    assert!(h.executor.release_run(&first_run), "run#1 应在途");
    assert!(
        wait_until(Duration::from_secs(10), || h.executor.call_count() == 5),
        "终态后应推进队首（当前 {}）",
        h.executor.call_count()
    );
    assert_eq!(
        h.executor.run_ids()[4],
        acks[4]["run_id"].as_str().expect("run id"),
        "推进的必须是 FIFO 队首（第 5 个 run）"
    );
    assert!(wait_until(Duration::from_secs(5), || run_status(&h, &first_run)
        == Some(RunStatus::Succeeded)));
    let gate_after = gate(&h);
    assert_eq!(gate_after.active, 4, "推进后仍不超卖");
    assert_eq!(gate_after.queued, 1);

    // 逐 run 放行剩余 → 全部收口，队空。
    for ack in acks.iter().skip(1) {
        let run_id = ack["run_id"].as_str().expect("run id").to_owned();
        assert!(
            wait_until(Duration::from_secs(10), || h.executor.release_run(&run_id)),
            "放行失败（run 未在途）：{run_id}"
        );
    }
    assert!(
        wait_until(Duration::from_secs(15), || gate(&h).active == 0),
        "全部 run 应收口（gate={:?}）",
        gate(&h)
    );
    assert_eq!(gate(&h).queued, 0);
    assert_eq!(h.executor.call_count(), 6, "6 个 run 均应派发");
    for ack in &acks {
        let run_id = ack["run_id"].as_str().expect("run id");
        assert!(
            wait_until(Duration::from_secs(5), || run_status(&h, run_id)
                == Some(RunStatus::Succeeded)),
            "run 应成功：{run_id}"
        );
    }
    for session_id in &sessions {
        let sid = SessionId::new(session_id.clone()).expect("session id");
        assert!(
            wait_until(Duration::from_secs(5), || h
                .runtime
                .block_on(h.manager.session_status(&sid))
                .ok()
                == Some(SessionStatus::Idle)),
            "会话应回 idle：{session_id}"
        );
    }
}

/// DoD3（A-6 必测矩阵）：`running` 时再发 1 条 → 等待槽（不发全局排队）；
/// 第 2 条 → `session_busy`；`queued` run 被 interrupt → 等待槽**不**连带清空，
/// run 从 FIFO 移除（槽位让给下一项），等待槽 run 晋升（饱和 → FIFO 尾部）。
#[test]
fn waiting_slot_and_queued_interrupt_follow_a6_matrix() {
    let h = harness(7.0);
    assert_eq!(h.runtime.block_on(h.manager.refresh_gate_limit()), Some(4));

    // 4 个 busy 会话占满闸门。
    let mut busy_runs = Vec::new();
    for index in 0..4 {
        let session_id = create_session(&h, &format!("busy-{index}"));
        let ack = send_message(&h, &session_id, &client_id(index));
        busy_runs.push(ack["run_id"].as_str().expect("run id").to_owned());
    }
    assert!(wait_until(Duration::from_secs(10), || h.executor.call_count() == 4));

    // 会话 A：第 1 条 → 闸门排队（position 1）；会话 B：第 1 条 → position 2。
    let session_a = create_session(&h, "session-a");
    let a1 = send_message(&h, &session_a, &client_id(100));
    assert_eq!(a1["queue"], json!({ "position": 1, "limit": 4 }));
    let session_b = create_session(&h, "session-b");
    let b1 = send_message(&h, &session_b, &client_id(101));
    assert_eq!(b1["queue"], json!({ "position": 2, "limit": 4 }));

    // 会话 A：第 2 条 → 等待槽（不发全局排队）；第 3 条 → session_busy。
    let a2 = send_message(&h, &session_a, &client_id(102));
    assert_eq!(a2["queued"], true);
    assert!(a2.get("queue").is_none(), "等待槽 run 不发全局排队：{a2}");
    let a2_run = a2["run_id"].as_str().expect("run id").to_owned();
    let error = h
        .backend
        .session_send(&SessionSendRequest {
            session_id: session_a.clone(),
            text: "third".to_owned(),
            client_msg_id: client_id(103),
            thinking_depth: None,
        references: Vec::new(),
            })
        .expect_err("等待槽已满");
    assert!(error.message.contains("session_busy"), "{}", error.message);

    // interrupt 会话 A（当前 run 为闸门排队）：a1 移除并取消；等待槽不连带清空（A-6）。
    let sid = SessionId::new(session_a.clone()).expect("id");
    let report = h.runtime.block_on(h.manager.interrupt(&sid)).expect("中断");
    assert_eq!(
        report.interrupted_run.as_ref().map(|run| run.to_string()),
        Some(a1["run_id"].as_str().expect("run id").to_owned())
    );
    assert_eq!(report.cancelled_waiting_run, None, "等待槽不连带清空（A-6）");
    assert_eq!(
        run_status(&h, a1["run_id"].as_str().expect("id")),
        Some(RunStatus::Cancelled)
    );
    assert_eq!(
        run_status(&h, &a2_run),
        Some(RunStatus::Queued),
        "a2 晋升为当前 run（闸门饱和 → 全局 FIFO 尾部）"
    );
    assert!(
        wait_until(Duration::from_secs(5), || gate(&h).queued == 2),
        "FIFO 应有 2 项（gate={:?}）",
        gate(&h)
    );

    // 放行 busy#1 → 队首 = B 的 b1（不是 A 的 a2：FIFO 顺序单写者）→ 再放行 busy#2
    // → a2 被推进。
    assert!(h.executor.release_run(&busy_runs[0]));
    assert!(
        wait_until(Duration::from_secs(10), || h.executor.call_count() == 5),
        "推进队首（当前 {}）",
        h.executor.call_count()
    );
    assert_eq!(
        h.executor.run_ids()[4],
        b1["run_id"].as_str().expect("run id"),
        "FIFO 顺序：B 先于 A"
    );
    assert_eq!(gate(&h).queued, 1);

    assert!(h.executor.release_run(&busy_runs[1]));
    assert!(
        wait_until(Duration::from_secs(10), || h.executor.call_count() == 6),
        "再推进（当前 {}）",
        h.executor.call_count()
    );
    assert_eq!(h.executor.run_ids()[5], a2_run, "等待槽提升的 run 随后推进");

    // 放行其余 → 全部收口。
    for run_id in [&busy_runs[2], &busy_runs[3]] {
        assert!(wait_until(Duration::from_secs(10), || h
            .executor
            .release_run(run_id)));
    }
    let b1_run = b1["run_id"].as_str().expect("run id").to_owned();
    assert!(wait_until(Duration::from_secs(10), || h
        .executor
        .release_run(&b1_run)));
    assert!(wait_until(Duration::from_secs(10), || h
        .executor
        .release_run(&a2_run)));
    assert!(
        wait_until(Duration::from_secs(10), || run_status(&h, &a2_run)
            == Some(RunStatus::Succeeded)),
        "等待槽提升的 run 最终执行成功"
    );
    assert!(wait_until(Duration::from_secs(5), || gate(&h).queued == 0));
}

/// DoD3（A-6）：dispose 全清——闸门排队 run 与 interrupt 后提升的 run 一并取消，
/// FIFO 归零（不残留、不超卖）。
#[test]
fn dispose_clears_queued_runs_and_fifo() {
    let h = harness(7.0);
    assert_eq!(h.runtime.block_on(h.manager.refresh_gate_limit()), Some(4));

    let mut busy_runs = Vec::new();
    for index in 0..4 {
        let session_id = create_session(&h, &format!("busy-{index}"));
        let ack = send_message(&h, &session_id, &client_id(index));
        busy_runs.push(ack["run_id"].as_str().expect("run id").to_owned());
    }
    assert!(wait_until(Duration::from_secs(10), || h.executor.call_count() == 4));

    let session_a = create_session(&h, "dispose-a");
    let a1 = send_message(&h, &session_a, &client_id(100));
    let a2 = send_message(&h, &session_a, &client_id(101)); // 等待槽
    assert_eq!(a2["queued"], true);
    assert_eq!(gate(&h).queued, 1);

    let sid = SessionId::new(session_a.clone()).expect("id");
    let status = h.runtime.block_on(h.manager.dispose(&sid)).expect("dispose");
    assert_eq!(status, SessionStatus::Cancelled);
    assert_eq!(
        run_status(&h, a1["run_id"].as_str().expect("id")),
        Some(RunStatus::Cancelled)
    );
    assert_eq!(
        run_status(&h, a2["run_id"].as_str().expect("id")),
        Some(RunStatus::Cancelled),
        "dispose 必须清除 interrupt 后提升的残余 run"
    );
    let gate_after = gate(&h);
    assert_eq!(gate_after.active, 4, "在途 run 不受 dispose 影响");
    assert_eq!(gate_after.queued, 0, "dispose 后 FIFO 不得残留");

    // 收尾：放行在途 run。
    for run_id in &busy_runs {
        assert!(wait_until(Duration::from_secs(10), || h
            .executor
            .release_run(run_id)));
    }
    assert!(wait_until(Duration::from_secs(10), || gate(&h).active == 0));
}

/// ADR-022 实现要点 3：`run_retry` 同样受闸门约束（超限 → 排队；队首推进仍按 FIFO）。
#[test]
fn run_retry_respects_gate() {
    let h = harness(7.0);
    assert_eq!(h.runtime.block_on(h.manager.refresh_gate_limit()), Some(4));

    let mut busy_runs = Vec::new();
    for index in 0..4 {
        let session_id = create_session(&h, &format!("busy-{index}"));
        let ack = send_message(&h, &session_id, &client_id(index));
        busy_runs.push(ack["run_id"].as_str().expect("run id").to_owned());
    }
    assert!(wait_until(Duration::from_secs(10), || h.executor.call_count() == 4));

    // 会话 C 的第 1 条（排队后被 interrupt 取消 → 可重试）。
    let session_c = create_session(&h, "retry-c");
    let c1 = send_message(&h, &session_c, &client_id(200));
    let c1_run = c1["run_id"].as_str().expect("id").to_owned();
    let sid = SessionId::new(session_c.clone()).expect("id");
    let report = h.runtime.block_on(h.manager.interrupt(&sid)).expect("中断");
    assert_eq!(
        report.interrupted_run.as_ref().map(|run| run.to_string()),
        Some(c1_run.clone())
    );
    assert_eq!(gate(&h).queued, 0);

    // 重放：闸门饱和 → 新 run 排队（响应 `queued=false`：未占会话等待槽，占当前 run 位）。
    let retry = h
        .backend
        .run_retry(&RunRetryRequest {
            run_id: c1_run.clone(),
        })
        .expect("重放");
    assert_eq!(retry["queued"], false);
    let retry_run = retry["run_id"].as_str().expect("run id").to_owned();
    assert_eq!(run_status(&h, &retry_run), Some(RunStatus::Queued));
    assert_eq!(gate(&h).queued, 1, "重放 run 进入全局 FIFO");

    // 放行 busy#1 → 队首（重放 run）推进。
    assert!(h.executor.release_run(&busy_runs[0]));
    assert!(
        wait_until(Duration::from_secs(10), || h.executor.call_count() == 5),
        "推进队首（当前 {}）",
        h.executor.call_count()
    );
    assert_eq!(h.executor.run_ids()[4], retry_run);

    // 收尾。
    for run_id in &busy_runs[1..] {
        assert!(wait_until(Duration::from_secs(10), || h
            .executor
            .release_run(run_id)));
    }
    assert!(wait_until(Duration::from_secs(10), || h
        .executor
        .release_run(&retry_run)));
}

/// DoD4（T3 口径）：16GB → limit 10；10 并发会话 × 1 run 全部立即派发（无排队）。
#[test]
fn ten_concurrent_runs_fit_default_cap() {
    let h = harness(16.0);
    assert_eq!(h.runtime.block_on(h.manager.refresh_gate_limit()), Some(10));

    let mut run_ids = Vec::new();
    for index in 0..10 {
        let session_id = create_session(&h, &format!("t3-{index}"));
        let ack = send_message(&h, &session_id, &client_id(index));
        assert!(ack.get("queue").is_none(), "第 {index} 个 run 不应排队：{ack}");
        run_ids.push(ack["run_id"].as_str().expect("id").to_owned());
    }
    assert!(
        wait_until(Duration::from_secs(10), || h.executor.call_count() == 10),
        "10 并发应全部派发（当前 {}）",
        h.executor.call_count()
    );
    let gate_before = gate(&h);
    assert_eq!(gate_before.limit, 10);
    assert_eq!(gate_before.active, 10);
    assert_eq!(gate_before.queued, 0);

    for run_id in &run_ids {
        assert!(wait_until(Duration::from_secs(10), || h
            .executor
            .release_run(run_id)));
    }
    assert!(
        wait_until(Duration::from_secs(15), || gate(&h).active == 0),
        "10 并发应收口（gate={:?}）",
        gate(&h)
    );
    for run_id in &run_ids {
        assert!(wait_until(Duration::from_secs(5), || run_status(&h, run_id)
            == Some(RunStatus::Succeeded)));
    }
}
