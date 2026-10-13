//! M5-05 DoD5 基准：导出期间写队列 P95 不劣化 + 内存曲线受控（release + ignored）。
//!
//! 机制：≥30MB 事件库上执行全库归档（`archive_export`，流式 1MiB 缓冲），同时以写
//! 队列驱动 2000 条事件写入；断言导出期间写事务延迟 P95 <50ms、单事务 <500ms
//! （D3 口径），并采样导出窗口内进程 RSS 增量（受控上限常量级记录：<256MB）。
//!
//! 由 `scripts/test/m5-05/verify-m5-05.mjs` 以 release 运行：
//!   `cargo test --release -p aether-tauri --test m5_05_export_bench -- --ignored --nocapture`
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_store::Store;
use aether_tauri::archive_control::ArchiveControlBackend;
use aether_tauri::core_health::{boot_core_full, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::ArchiveExportRequest;
use sysinfo::{Pid, ProcessRefreshKind, System};

const EVENTS: usize = 30_000;
const WRITES: usize = 2_000;
/// RSS 增量上限（常量级记录；1MiB 有界缓冲流式路径不应随库体积线性增长）。
const RSS_DELTA_LIMIT_BYTES: u64 = 256 * 1024 * 1024;

fn seed_database(path: &Path) {
    let store = Store::open(path).unwrap();
    {
        let connection = store.connection();
        connection
            .execute_batch(
                "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
                 VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                 VALUES ('01J0000000000000000000000A', 'mock', '基准', 'idle', 10, 10)",
                [],
            )
            .unwrap();
        let sql = format!(
            "WITH RECURSIVE seqs(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seqs WHERE i < {EVENTS}) \
             INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
             SELECT printf('01J%023d', i), '01J0000000000000000000000A', NULL, 'mock', i, 'message.delta', \
                    '{{\"message_id\":\"01J000000000000000000000M1\",\"text\":\"' || substr(hex(zeroblob(500)), 1, 1000) || '\"}}', \
                    1700000000000, 1 \
             FROM seqs;"
        );
        connection.execute_batch(&sql).unwrap();
    }
}

fn percentile_95(samples: &[Duration]) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = ((sorted.len() as f64) * 0.95).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "基准：由 verify-m5-05 以 --release 运行"]
async fn export_does_not_degrade_write_latency_and_bounds_memory() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("aether.db");
    seed_database(&db_path);
    let db_bytes = std::fs::metadata(&db_path).unwrap().len();
    assert!(db_bytes > 20 * 1024 * 1024, "基准库应具备可观测体积");

    let handle = tokio::runtime::Handle::current();
    let boot = boot_core_full(
        dir.path(),
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .unwrap();
    let backend = Arc::new(ArchiveControlBackend::new(
        Arc::new(NotImplementedBackend),
        dir.path().to_path_buf(),
        Some(boot.reads.clone()),
        Some(boot.write.clone()),
        handle.clone(),
        Arc::new(aether_control::SystemClock),
    ));

    // RSS 采样线程（导出窗口内每 25ms 采样一次）。
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let baseline = current_rss();
    let sampler = {
        let stop = Arc::clone(&stop);
        let peak = Arc::clone(&peak);
        std::thread::spawn(move || {
            let mut system = System::new();
            let pid = Pid::from_u32(std::process::id());
            let refresh = ProcessRefreshKind::new().with_memory();
            while !stop.load(Ordering::SeqCst) {
                system.refresh_processes_specifics(refresh);
                if let Some(process) = system.process(pid) {
                    peak.fetch_max(process.memory(), Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        })
    };

    // 导出（后台阻塞线程）与写入（异步）并发。
    let export_task = {
        let backend = Arc::clone(&backend);
        let request = ArchiveExportRequest { target_dir: None };
        let canonical = request.canonical_target_dir().unwrap();
        tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let response = backend
                .archive_export(&request, canonical.as_deref())
                .expect("归档导出成功");
            (response, started.elapsed())
        })
    };
    let queue = boot.write.clone();
    let mut set = tokio::task::JoinSet::new();
    for job in 0..20u64 {
        let queue = queue.clone();
        set.spawn(async move {
            let mut receipts = Vec::new();
            for index in 0..(WRITES as u64 / 20) {
                let seq = EVENTS as u64 + job * 100 + index + 1;
                if let Ok(receipt) = queue
                    .append_events(vec![aether_core::EventEnvelope::from_json_str(&format!(
                        r#"{{"v":1,"id":"01JB{seq:022}","session_id":"01J0000000000000000000000A","run_id":null,"runtime_id":"mock","seq":{seq},"ts":1800000000000,"type":"message.delta","payload":{{"message_id":"01J000000000000000000000M1","text":"w{seq}"}}}}"#
                    )).unwrap()])
                    .await
                {
                    receipts.push(receipt);
                }
            }
            receipts
        });
    }
    let mut receipts = Vec::new();
    while let Some(joined) = set.join_next().await {
        receipts.extend(joined.unwrap());
    }
    let (response, export_elapsed) = export_task.await.unwrap();
    stop.store(true, Ordering::SeqCst);
    let _ = sampler.join();
    let peak_rss = peak.load(Ordering::SeqCst);
    let rss_delta = peak_rss.saturating_sub(baseline);

    let commit_samples: BTreeSet<(i64, u64, usize)> = receipts
        .iter()
        .map(|receipt| {
            (
                receipt.committed_at_ms,
                receipt.commit_ms,
                receipt.batch_entries,
            )
        })
        .collect();
    let commit_ms: Vec<Duration> = commit_samples
        .iter()
        .map(|(_, commit_ms, _)| Duration::from_millis(*commit_ms))
        .collect();
    let commit_p95 = percentile_95(&commit_ms);
    let metrics = queue.metrics();

    let rows = response["export"]["rows"].as_u64().unwrap();
    assert!(rows >= EVENTS as u64, "导出必须覆盖基准库（rows={rows}）");
    assert_eq!(metrics.failed_entries, 0, "导出期间不得有写失败");
    assert!(
        commit_p95 < Duration::from_millis(50),
        "导出期间写事务延迟 P95 必须 <50ms（D3）：{commit_p95:?}"
    );
    assert!(
        commit_ms
            .iter()
            .all(|sample| *sample < Duration::from_millis(500)),
        "单事务必须 <500ms（D3）"
    );
    assert!(
        rss_delta < RSS_DELTA_LIMIT_BYTES,
        "导出内存增量必须受控（<256MB）：实际 {rss_delta} 字节"
    );

    println!(
        "M5-05 DoD5 基准：库 {db_bytes} 字节 / 导出 {} 字节耗时 {:?}；并发写入 {} 条（{} 事务批次）；\
         事务延迟 P95={commit_p95:?} max={:?}；RSS baseline={baseline} peak={peak_rss} delta={rss_delta} 字节",
        response["export"]["bytes"],
        export_elapsed,
        WRITES,
        commit_ms.len(),
        commit_ms.iter().max().unwrap(),
    );

    let _ = boot.pipeline.shutdown().await;
    if let Some(storage) = boot.storage.take() {
        let _ = storage.shutdown().await;
    }
}

fn current_rss() -> u64 {
    let mut system = System::new();
    let pid = Pid::from_u32(std::process::id());
    system.refresh_processes_specifics(ProcessRefreshKind::new().with_memory());
    system
        .process(pid)
        .map(|process| process.memory())
        .unwrap_or(0)
}
