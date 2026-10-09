//! M4-04：T1/T2/T3 验收运行器（设计附录 D）。
//!
//! - T1 会话创建时延：预热后连续创建 50 次；**P50 < 500ms（主指标）、P95 < 2s**；
//! - T2 事件回显时延：Mock 适配器 10 个并发会话（每会话 1 run）× 100 delta；
//!   P95 = 单 run 100 次样本分位；**P95 < 150ms**（P0 统一口径）；
//! - T3 并发控制事件不丢：10 个并发会话、每会话 1 个 run，全程录音（在线广播）
//!   与 journal（库内补读）比对；**控制事件 0 丢失；gap 均可补读**。
//!
//! 运行：`AETHER_MOCK_ADAPTER=<Bun 编译产物> cargo test -p aether-tauri --test
//! m4_04_t1_t3 -- --nocapture`（由 `scripts/test/m4-04/verify-m4-04.mjs` 构建并设置；
//! 未设置且未要求时显式跳过，`AETHER_REQUIRE_MOCK_ADAPTER=1` 时缺路径直接失败）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, Supervisor};
use aether_control::{EventPipeline, LifecycleConfig, SessionManager, SystemClock};
use aether_core::SessionId;
use aether_store::{ReadPool, WriteQueue};
use aether_tauri::adapter_executor::AdapterRunExecutor;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{SessionCreateRequest, SessionSendRequest};
use aether_tauri::runtime_control::{boot_supervisor, run_supervisor_startup};
use aether_tauri::session_backend::SessionBackend;
use serde_json::json;
use tempfile::TempDir;

const T2_SESSIONS: usize = 10;
const DELTAS_PER_RUN: usize = 100;
/// 显著大于合并窗口（16ms）的间隔：即使在 CI 负载抖动下也保证每条 delta 单独落盘
/// → 100 个可测样本/run（T2 口径「单 run 100 次样本分位」）。
const STREAM_INTERVAL_MS: u64 = 100;
const T2_P95_LIMIT_MS: u64 = 150;
const T1_P50_LIMIT_MS: u64 = 500;
const T1_P95_LIMIT_MS: u64 = 2_000;

fn mock_binary() -> Option<PathBuf> {
    match std::env::var_os("AETHER_MOCK_ADAPTER") {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            if std::env::var("AETHER_REQUIRE_MOCK_ADAPTER").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_MOCK_ADAPTER=1 但 AETHER_MOCK_ADAPTER 未设置");
            }
            eprintln!(
                "SKIP：AETHER_MOCK_ADAPTER 未设置（运行 pnpm verify:m4-04 构建 Mock 后执行）"
            );
            None
        }
    }
}

struct Harness {
    #[allow(dead_code)]
    dir: TempDir,
    #[allow(dead_code)]
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    slot: Arc<aether_tauri::shutdown::StorageSlot>,
    pipeline: EventPipeline,
    reads: ReadPool,
    #[allow(dead_code)]
    write: WriteQueue,
    #[allow(dead_code)]
    manager: SessionManager,
    backend: Arc<dyn IpcBackend>,
    supervisor: Arc<Supervisor>,
}

fn harness(binary: &Path) -> Harness {
    let dir = TempDir::new().expect("临时目录");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio 运行时");
    let handle = runtime.handle().clone();
    let CoreBoot {
        storage: slot,
        reads,
        write,
        pipeline,
        ..
    } = boot_core_full(
        dir.path(),
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("启动核心（存储 + 管线）");
    let spec = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Mock", binary.to_path_buf())
            .official(true)
            .with_args([
                "--stream-deltas",
                &DELTAS_PER_RUN.to_string(),
                "--stream-interval-ms",
                &STREAM_INTERVAL_MS.to_string(),
            ]),
    );
    let supervisor = Arc::new(
        boot_supervisor(vec![spec], Some(&dir.path().join("adapters.json"))).expect("构造监督器"),
    );
    let startup = run_supervisor_startup(&supervisor, &handle).expect("启动序列尾段");
    for (runtime_id, outcome) in &startup.warmups {
        assert!(
            outcome.is_ready(),
            "{runtime_id} 预热必须 Ready：{outcome:?}"
        );
    }
    let executor = Arc::new(AdapterRunExecutor::new(
        Arc::clone(&supervisor),
        pipeline.clone(),
        reads.clone(),
        write.clone(),
        handle.clone(),
        None,
    ));
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        executor.clone(),
    );
    let backend: Arc<dyn IpcBackend> = Arc::new(SessionBackend::new(
        Arc::new(NotImplementedBackend),
        Some(manager.clone()),
        Some(executor),
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
        supervisor,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟")
        .as_millis() as u64
}

fn percentile_ms(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p * sorted.len() as f64).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[idx]
}

fn create_session(h: &Harness, title: &str) -> String {
    let created = h
        .backend
        .session_create(&SessionCreateRequest {
            runtime_id: "mock".to_owned(),
            title: title.to_owned(),
            workspace_id: None,
            model: None,
            thinking_depth: None,
        })
        .expect("session_create");
    created["id"].as_str().expect("会话 id").to_owned()
}

/// T1：会话创建时延（预热后连续 50 次；P50 < 500ms，P95 < 2s）。
#[test]
fn t1_session_create_latency_p50_p95_within_budget() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);

    for index in 0..3 {
        let _ = create_session(&h, &format!("T1 预热 {index}"));
    }
    let mut samples: Vec<u64> = Vec::with_capacity(50);
    for index in 0..50 {
        let started = Instant::now();
        let _ = create_session(&h, &format!("T1 样本 {index}"));
        samples.push(started.elapsed().as_millis() as u64);
    }
    let mut sorted = samples.clone();
    sorted.sort_unstable();
    let p50 = percentile_ms(&sorted, 0.50);
    let p95 = percentile_ms(&sorted, 0.95);
    let max = sorted.last().copied().unwrap_or(0);

    assert!(
        p50 < T1_P50_LIMIT_MS,
        "T1 P50 必须 < {T1_P50_LIMIT_MS}ms（实测 {p50}ms）"
    );
    assert!(
        p95 < T1_P95_LIMIT_MS,
        "T1 P95 必须 < {T1_P95_LIMIT_MS}ms（实测 {p95}ms）"
    );

    println!(
        "AETHER_M4_04_T1 {}",
        json!({
            "samples": samples.len(),
            "warmup": 3,
            "p50_ms": p50,
            "p95_ms": p95,
            "max_ms": max,
            "p50_limit_ms": T1_P50_LIMIT_MS,
            "p95_limit_ms": T1_P95_LIMIT_MS,
            "pass": true,
        })
    );

    h.runtime.block_on(h.supervisor.shutdown_all());
}

/// 在线录音条目：{ session_id, id, type, ts(适配器发射时刻), received_ms(在线收到时刻) }。
#[derive(Clone, Debug)]
struct Recorded {
    session_id: String,
    id: String,
    event_type: String,
    ts: i64,
    received_ms: u64,
}

/// T2 + T3：10 并发会话 × 1 run × 100 delta；回显 P95 < 150ms；控制事件 0 丢失。
#[test]
fn t2_t3_echo_latency_and_control_events_zero_loss() {
    let Some(binary) = mock_binary() else {
        return;
    };
    let h = harness(&binary);

    let sessions: Vec<String> = (0..T2_SESSIONS)
        .map(|index| create_session(&h, &format!("T2 会话 {index}")))
        .collect();

    // 在线录音：订阅广播（UI 事件桥同源）。
    let mut receiver = h.pipeline.subscribe();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let consumer = std::thread::spawn(move || {
        let mut recorded: Vec<Recorded> = Vec::new();
        let mut lagged: u64 = 0;
        let mut empty_after_stop = 0u32;
        loop {
            match receiver.try_recv() {
                Ok(envelope) => {
                    empty_after_stop = 0;
                    recorded.push(Recorded {
                        session_id: envelope.session_id.as_str().to_owned(),
                        id: envelope.id.as_str().to_owned(),
                        event_type: envelope.event_type().as_str().to_owned(),
                        ts: envelope.ts,
                        received_ms: now_ms(),
                    });
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(count)) => {
                    lagged += count;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                    if stop_flag.load(Ordering::SeqCst) {
                        empty_after_stop += 1;
                        if empty_after_stop > 20 {
                            break;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        (recorded, lagged)
    });

    // 并发发送 10 个 run（每会话 1 run）。
    let ack_started = Instant::now();
    let acks: Vec<(String, String)> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (index, session_id) in sessions.iter().enumerate() {
            let backend = Arc::clone(&h.backend);
            let session = session_id.clone();
            handles.push(scope.spawn(move || {
                let ack = backend
                    .session_send(&SessionSendRequest {
                        session_id: session,
                        text: format!("T2 负载 {index}"),
                        client_msg_id: format!("01J8ZQ5R0N7W9Y8X6V4T2S{index:05}"),
                        thinking_depth: None,
                    })
                    .expect("session_send");
                let run_id = ack["run_id"].as_str().expect("run_id").to_owned();
                (session_id.clone(), run_id)
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().expect("发送线程"))
            .collect()
    });
    let send_ack_ms = ack_started.elapsed().as_millis() as u64;

    // 等待全部 run 到达终态。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let done = acks.iter().all(|(_, run_id)| {
            let run = h
                .runtime
                .block_on(
                    h.reads
                        .run(&aether_core::RunId::new(run_id.clone()).expect("run id")),
                )
                .ok()
                .flatten();
            run.map(|run| {
                matches!(
                    run.status,
                    aether_core::RunStatus::Succeeded
                        | aether_core::RunStatus::Failed
                        | aether_core::RunStatus::Cancelled
                        | aether_core::RunStatus::Timeout
                )
            })
            .unwrap_or(false)
        });
        if done || Instant::now() >= deadline {
            assert!(done, "全部 run 必须在 60s 内到达终态");
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // 末事件广播余量（落库 → 广播链路的尾部窗口）。
    std::thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::SeqCst);
    let (recorded, lagged) = consumer.join().expect("录音线程");

    // ---- T2：回显时延（delta 样本；单 run P95）----
    let mut per_run_samples: Vec<(String, Vec<u64>)> = Vec::new();
    for (session_id, _run_id) in &acks {
        let samples: Vec<u64> = recorded
            .iter()
            .filter(|entry| entry.session_id == *session_id && entry.event_type == "message.delta")
            .map(|entry| entry.received_ms.saturating_sub(entry.ts as u64))
            .collect();
        per_run_samples.push((session_id.clone(), samples));
    }

    let mut per_run_p95: Vec<u64> = Vec::new();
    let mut all_samples: Vec<u64> = Vec::new();
    let mut min_sample_count = usize::MAX;
    for (session_id, samples) in &per_run_samples {
        assert!(
            !samples.is_empty(),
            "会话 {session_id} 必须收到 delta 回显样本"
        );
        min_sample_count = min_sample_count.min(samples.len());
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        per_run_p95.push(percentile_ms(&sorted, 0.95));
        all_samples.extend_from_slice(samples);
    }
    let mut all_sorted = all_samples.clone();
    all_sorted.sort_unstable();
    let overall_p95 = percentile_ms(&all_sorted, 0.95);
    let max_run_p95 = per_run_p95.iter().copied().max().unwrap_or(0);

    println!(
        "[m4-04 T2 诊断] per_run_samples={:?} lagged={lagged}",
        per_run_samples
            .iter()
            .map(|(session, samples)| format!(
                "{}:{}",
                &session[..8.min(session.len())],
                samples.len()
            ))
            .collect::<Vec<_>>()
    );

    assert!(
        all_samples.len() >= T2_SESSIONS * (DELTAS_PER_RUN / 2),
        "T2 样本量不足（合并窗口可能吞并样本）：{}",
        all_samples.len()
    );
    assert!(
        max_run_p95 < T2_P95_LIMIT_MS,
        "T2 单 run P95 必须 < {T2_P95_LIMIT_MS}ms（实测最大 {max_run_p95}ms）"
    );

    println!(
        "AETHER_M4_04_T2 {}",
        json!({
            "sessions": T2_SESSIONS,
            "deltas_per_run": DELTAS_PER_RUN,
            "interval_ms": STREAM_INTERVAL_MS,
            "samples_total": all_samples.len(),
            "min_samples_per_run": if min_sample_count == usize::MAX { 0 } else { min_sample_count },
            "per_run_p95_ms": per_run_p95,
            "max_run_p95_ms": max_run_p95,
            "overall_p95_ms": overall_p95,
            "limit_ms": T2_P95_LIMIT_MS,
            "send_ack_ms": send_ack_ms,
            "lagged": lagged,
            "pass": true,
        })
    );

    // ---- T3：控制事件 0 丢失 + 缺口可补读 ----
    let mut control_received: Vec<(String, String)> = Vec::new();
    for entry in &recorded {
        if entry.event_type != "message.delta" {
            control_received.push((entry.session_id.clone(), entry.id.clone()));
        }
    }

    let mut control_persisted = 0usize;
    let mut missing_in_db: Vec<String> = Vec::new();
    let mut seq_ok = true;
    let mut complete_ok = true;
    for (session_id, _run_id) in &acks {
        let session = SessionId::new(session_id.clone()).expect("会话 id");
        let frame = h
            .runtime
            .block_on(h.pipeline.readback(&session, 0))
            .expect("补读");
        complete_ok &= frame.complete;
        // seq 连续（1..=max，无重复无缺口）。
        let seqs: Vec<u64> = frame.events.iter().map(|event| event.seq).collect();
        for (index, seq) in seqs.iter().enumerate() {
            if *seq != (index as u64 + 1) {
                seq_ok = false;
                break;
            }
        }
        let persisted_ids: std::collections::HashSet<String> = frame
            .events
            .iter()
            .map(|event| event.id.as_str().to_owned())
            .collect();
        for event in &frame.events {
            if event.event_type() != aether_core::EventType::MessageDelta {
                control_persisted += 1;
            }
        }
        for (received_session, id) in &control_received {
            if received_session == session_id && !persisted_ids.contains(id) {
                missing_in_db.push(id.clone());
            }
        }
        // 缺口可补读：以已收最大 seq 之后再补读一次（若已到最新则 complete=true）。
        let max_received_seq = frame.events.last().map(|event| event.seq).unwrap_or(0);
        let tail = h
            .runtime
            .block_on(h.pipeline.readback(&session, max_received_seq))
            .expect("尾部补读");
        complete_ok &= tail.complete;
    }

    assert!(
        missing_in_db.is_empty(),
        "T3 在线收到的控制事件必须 100% 落库（丢失 {} 条，样本 {:?}）",
        missing_in_db.len(),
        &missing_in_db[..missing_in_db.len().min(5)]
    );
    assert!(seq_ok, "T3 库内 seq 必须连续无缺口");
    assert!(complete_ok, "T3 补读必须 complete（gap 均可补读）");
    assert!(
        control_received.len() >= T2_SESSIONS * 4,
        "T3 控制事件样本量不足：{}",
        control_received.len()
    );

    println!(
        "AETHER_M4_04_T3 {}",
        json!({
            "sessions": T2_SESSIONS,
            "control_events_received": control_received.len(),
            "control_events_persisted": control_persisted,
            "lost": missing_in_db.len(),
            "seq_contiguous": seq_ok,
            "backfill_complete": complete_ok,
            "lagged": lagged,
            "pass": true,
        })
    );

    h.runtime.block_on(h.supervisor.shutdown_all());
}
