//! M10-09 标准性能复测（10 并发、600s、5.22GiB generation）。
//!
//! 默认 `#[ignore]`，由 `pnpm m10-09:bench` / `pnpm verify:m10-09:perf` 显式执行：
//! `AETHER_M10_09_DATA_DIR=dist-out/m10-09/generation \
//!  AETHER_M10_09_SECONDS=600 \
//!  cargo test -p aether-store --test m10_09_bench -- --ignored --nocapture`
//!
//! 测试口径：产品 `StoreRuntime::open_generation`（单写任务 + 每片连接）、
//! 10 个并发写会话、同一时段 `events_page` 读；写入/读取均记录 P50/P95/max 与失败数。
//! 读只查询本 run 新写入的合法 `log` 事件（`seq > 初始 max`）：M10-08 标准数据集的
//! 合成 payload 不满足 `EventPayload` 严格 schema，不能用于产品解码路径压测。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::time::{Duration, Instant};

use aether_core::{
    EventEnvelope, EventId, EventPayload, LogLevel, LogPayload, RuntimeId, SessionId,
    EVENT_ENVELOPE_VERSION,
};
use aether_store::{read_layout, StoreRuntime, WriteQueueConfig};

fn percentile(sorted: &[f64], percent: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * percent).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[test]
#[ignore = "M10-09 standard 600s benchmark; run via pnpm m10-09:bench"]
fn ten_concurrent_writes_and_events_page_for_600s() {
    let data_dir = std::env::var("AETHER_M10_09_DATA_DIR")
        .unwrap_or_else(|_| "dist-out/m10-09/generation".to_owned());
    let seconds: u64 = std::env::var("AETHER_M10_09_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(600);
    let out = std::env::var("AETHER_M10_09_OUT")
        .unwrap_or_else(|_| "dist-out/m10-09/bench-600s.json".to_owned());
    let layout = read_layout(Path::new(&data_dir)).expect("generation layout");
    let main_path = layout.main_db().to_path_buf();

    let sessions: Vec<String> = {
        let connection = rusqlite::Connection::open_with_flags(
            &main_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open main readonly");
        let mut statement = connection
            .prepare("SELECT id FROM sessions ORDER BY id LIMIT 10")
            .expect("prepare sessions");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query sessions");
        rows.map(|row| row.expect("session id")).collect()
    };
    assert_eq!(sessions.len(), 10, "数据集必须至少 10 个会话");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio");
    let storage =
        StoreRuntime::open_generation(layout, WriteQueueConfig::default(), runtime.handle())
            .expect("open generation runtime");
    let queue = storage.queue().clone();
    let reads = storage.reads().clone();

    let mut write_samples: Vec<f64> = Vec::new();
    let mut read_samples: Vec<f64> = Vec::new();
    let mut failures = 0u64;
    let mut first_write_error: Option<String> = None;
    let mut first_read_error: Option<String> = None;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);

    runtime.block_on(async {
        // 先记录每个会话的初始 max(seq)：写入从 max+1 开始，读只读这些新写入的
        // 合法 log 事件（M10-08 数据集的合成 payload 不满足 EventPayload 严格 schema，
        // 不能用于 events_page 解码；历史 payload 校验由 store 单测覆盖）。
        let mut initial_max: Vec<(SessionId, u64)> = Vec::with_capacity(sessions.len());
        for session_id in &sessions {
            let session_id = SessionId::new(session_id.clone()).expect("session id");
            let max = reads
                .max_seq(&session_id)
                .await
                .expect("max seq")
                .unwrap_or(0);
            initial_max.push((session_id, max));
        }

        let mut write_tasks = tokio::task::JoinSet::new();
        for (session_index, (session_id, start_seq)) in initial_max.iter().enumerate() {
            let queue = queue.clone();
            let session_id = session_id.clone();
            let mut seq = *start_seq;
            write_tasks.spawn(async move {
                let mut samples = Vec::new();
                let mut failures = 0u64;
                let mut first_error: Option<String> = None;
                loop {
                    if Instant::now() >= deadline {
                        break;
                    }
                    seq += 1;
                    let event = EventEnvelope {
                        v: EVENT_ENVELOPE_VERSION,
                        id: EventId::new(ulid::Ulid::new().to_string()).expect("event id"),
                        session_id: session_id.clone(),
                        run_id: None,
                        runtime_id: RuntimeId::new("mock").expect("runtime id"),
                        seq,
                        ts: i64::try_from(seq).unwrap_or(i64::MAX),
                        payload: EventPayload::Log(LogPayload {
                            level: LogLevel::Info,
                            message: format!("bench-{session_index}-{seq}"),
                        }),
                    };
                    let begin = Instant::now();
                    match queue.append_events(vec![event]).await {
                        Ok(_) => samples.push(begin.elapsed().as_secs_f64() * 1000.0),
                        Err(error) => {
                            failures = failures.saturating_add(1);
                            if first_error.is_none() {
                                first_error = Some(format!("{error:?}"));
                            }
                        }
                    }
                }
                (samples, failures, first_error)
            });
        }

        // 同一时段 `events_page` 读（10 个会话轮询；只读 seq > 初始 max 的新事件）。
        let reads_for_task = reads.clone();
        let sessions_for_read = initial_max;
        let read_task = tokio::task::spawn(async move {
            let mut samples = Vec::new();
            let mut failures = 0u64;
            let mut first_error: Option<String> = None;
            let mut cursor = 0usize;
            while Instant::now() < deadline {
                let (session, after_seq) =
                    &sessions_for_read[cursor % sessions_for_read.len()];
                cursor = cursor.saturating_add(1);
                let begin = Instant::now();
                match reads_for_task
                    .events_page(session, Some(*after_seq), 500)
                    .await
                {
                    Ok(_) => samples.push(begin.elapsed().as_secs_f64() * 1000.0),
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        if first_error.is_none() {
                            first_error = Some(format!("{error:?}"));
                        }
                    }
                }
            }
            (samples, failures, first_error)
        });

        while let Some(joined) = write_tasks.join_next().await {
            match joined {
                Ok((mut samples, failed, first_error)) => {
                    write_samples.append(&mut samples);
                    failures = failures.saturating_add(failed);
                    if first_write_error.is_none() {
                        first_write_error = first_error;
                    }
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    if first_write_error.is_none() {
                        first_write_error = Some(format!("join error: {error}"));
                    }
                }
            }
        }
        match read_task.await {
            Ok((mut samples, failed, first_error)) => {
                read_samples.append(&mut samples);
                failures = failures.saturating_add(failed);
                first_read_error = first_error;
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                first_read_error = Some(format!("join error: {error}"));
            }
        }
        storage.shutdown().await.expect("shutdown");
    });

    write_samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    read_samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let write_p50 = percentile(&write_samples, 0.50);
    let write_p95 = percentile(&write_samples, 0.95);
    let write_max = write_samples.last().copied().unwrap_or(0.0);
    let read_p50 = percentile(&read_samples, 0.50);
    let read_p95 = percentile(&read_samples, 0.95);
    let read_max = read_samples.last().copied().unwrap_or(0.0);
    let main_events: i64 = {
        let connection = rusqlite::Connection::open(&main_path).expect("main after");
        connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("main events")
    };
    let elapsed = started.elapsed().as_secs_f64();
    let result = serde_json::json!({
        "task": "M10-09",
        "profile": "debug",
        "concurrency": 10,
        "duration_seconds": elapsed,
        "write_samples": write_samples.len(),
        "write_p50_ms": write_p50,
        "write_p95_ms": write_p95,
        "write_max_ms": write_max,
        "read_samples": read_samples.len(),
        "read_p50_ms": read_p50,
        "read_p95_ms": read_p95,
        "read_max_ms": read_max,
        "failures": failures,
        "first_write_error": first_write_error,
        "first_read_error": first_read_error,
        "main_events": main_events,
        "clean_shutdown": true,
        "dataset": {
            "generation": read_layout(Path::new(&data_dir)).expect("layout after").generation().to_owned(),
            "data_dir": data_dir,
            "seconds_requested": seconds
        }
    });
    if let Some(parent) = std::path::Path::new(&out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, serde_json::to_vec_pretty(&result).expect("json")).expect("write result");
    println!(
        "M10-09_BENCH {}",
        serde_json::to_string(&result).expect("json line")
    );

    assert_eq!(
        failures, 0,
        "写入/读取失败数必须为 0；write={first_write_error:?} read={first_read_error:?}"
    );
    assert_eq!(main_events, 0, "主库 events 必须为空");
    assert!(write_p95 <= 50.0, "写 P95 {write_p95:.3}ms > 50ms");
    assert!(read_p95 < 150.0, "events_page P95 {read_p95:.3}ms >= 150ms");
}
