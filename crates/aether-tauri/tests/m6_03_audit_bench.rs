//! M6-03 DoD2 基准：1e6 行审计表下已过滤分页查询 P95 ≤300ms（release + ignored）。
//!
//! 机制：构造 1,000,000 行 `audit_log`（actor 三分循环 / action 四类循环 /
//! session 集中分布 / `ts` 递增），经 [`ArchiveControlBackend::audit_query`] 执行
//! 40 次混合过滤分页查询（actor / session+时间窗 / action / 组合），断言 P95 ≤300ms
//! （ADR-023 §2.1-3 任务内定值；基准环境 16GB 记入 M6-03 证据）。
//!
//! 由 `scripts/test/m6-03/verify-m6-03.mjs` 以 release 运行：
//!   `cargo test --release -p aether-tauri --test m6_03_audit_bench -- --ignored --nocapture`
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_store::Store;
use aether_tauri::archive_control::ArchiveControlBackend;
use aether_tauri::core_health::{boot_core_full, StaticRuntimeSummaries};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::AuditQueryRequest;
use serde_json::json;

/// 审计行数（ADR-023：1e6 行下 P95 目标）。
const AUDIT_ROWS: usize = 1_000_000;
/// 查询样本数（混合过滤分页）。
const SAMPLES: usize = 40;
/// P95 阈值（ADR-023 §2.1-3 任务内定值）。
const P95_LIMIT: Duration = Duration::from_millis(300);

fn seed_database(path: &Path) {
    let store = Store::open(path).unwrap();
    let connection = store.connection();
    let sql = format!(
        "BEGIN;\
         WITH RECURSIVE seqs(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seqs WHERE i < {AUDIT_ROWS}) \
         INSERT INTO audit_log (id, session_id, runtime_id, actor, action, resource, detail, result, ts) \
         SELECT printf('01J%023d', i), \
                CASE WHEN i % 10 = 0 THEN NULL ELSE '01J0000000000000000000000A' END, \
                'mock', \
                CASE i % 3 WHEN 0 THEN 'user' WHEN 1 THEN 'agent' ELSE 'system' END, \
                CASE i % 4 WHEN 0 THEN 'tool.call' WHEN 1 THEN 'session.created' \
                           WHEN 2 THEN 'permission.requested' ELSE 'runtime.status_changed' END, \
                CASE i % 4 WHEN 0 THEN 'mock.echo' WHEN 1 THEN 'session:01J0000000000000000000000A' \
                           WHEN 2 THEN 'fs.write:notes.md' ELSE 'runtime:mock' END, \
                '{{\"n\":' || i || '}}', \
                CASE i % 2 WHEN 0 THEN 'success' ELSE 'failed' END, \
                1700000000000 + i \
         FROM seqs;\
         COMMIT;"
    );
    connection.execute_batch(&sql).unwrap();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count as usize, AUDIT_ROWS, "基准库行数");
}

fn percentile_95(samples: &[Duration]) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = ((sorted.len() as f64) * 0.95).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn percentile_50(samples: &[Duration]) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "基准：由 verify-m6-03 以 --release 运行"]
async fn one_million_row_audit_query_p95_under_300ms() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("aether.db");
    let started = Instant::now();
    seed_database(&db_path);
    let seed_elapsed = started.elapsed();
    let db_bytes = std::fs::metadata(&db_path).unwrap().len();

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

    // 混合过滤分页查询（等值 + 时间窗 + 组合；limit 100 缺省分页）。
    let cases: Vec<AuditQueryRequest> = vec![
        AuditQueryRequest {
            actor: Some("agent".to_owned()),
            ..AuditQueryRequest::default()
        },
        AuditQueryRequest {
            session_id: Some("01J0000000000000000000000A".to_owned()),
            from_ts: Some(1_700_500_000_000),
            to_ts: Some(1_700_500_100_000),
            ..AuditQueryRequest::default()
        },
        AuditQueryRequest {
            action: Some("tool.call".to_owned()),
            ..AuditQueryRequest::default()
        },
        AuditQueryRequest {
            actor: Some("system".to_owned()),
            action: Some("runtime.status_changed".to_owned()),
            offset: Some(1_000),
            ..AuditQueryRequest::default()
        },
        AuditQueryRequest {
            session_id: Some("01J0000000000000000000000A".to_owned()),
            actor: Some("user".to_owned()),
            limit: Some(500),
            ..AuditQueryRequest::default()
        },
    ];

    // 预热一次（连接建立/页缓存），不计入样本。
    let _ = backend.audit_query(&cases[0]).unwrap();

    let mut samples: Vec<Duration> = Vec::with_capacity(SAMPLES);
    let mut row_counts: Vec<usize> = Vec::with_capacity(SAMPLES);
    for index in 0..SAMPLES {
        let request = &cases[index % cases.len()];
        let started = Instant::now();
        let response = backend.audit_query(request).unwrap();
        let elapsed = started.elapsed();
        println!("[bench] case={} elapsed={elapsed:?}", index % cases.len());
        samples.push(elapsed);
        row_counts.push(response["rows"].as_array().map(Vec::len).unwrap_or(0));
        assert!(
            response["rows"].as_array().map(Vec::len).unwrap_or(0) <= 500,
            "分页上限"
        );
    }
    let p50 = percentile_50(&samples);
    let p95 = percentile_95(&samples);
    let max = samples.iter().max().copied().unwrap();

    println!(
        "M6-03 DoD2 基准：{} 行 / 库 {} 字节（seed {:?}）；{} 次混合过滤分页查询 \
         P50={p50:?} P95={p95:?} max={max:?}",
        AUDIT_ROWS,
        db_bytes,
        seed_elapsed,
        samples.len(),
    );
    if let Some(dir) = std::env::var_os("AETHER_M6_03_EVIDENCE_DIR") {
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        let evidence = json!({
            "rows": AUDIT_ROWS,
            "db_bytes": db_bytes,
            "seed_ms": seed_elapsed.as_millis(),
            "samples": samples.len(),
            "p50_ms": p50.as_millis(),
            "p95_ms": p95.as_millis(),
            "max_ms": max.as_millis(),
            "p95_limit_ms": P95_LIMIT.as_millis(),
            "page_rows": row_counts,
            "count_free_pagination": true,
        });
        let _ = std::fs::write(
            dir.join("dod2_bench.json"),
            serde_json::to_string_pretty(&evidence).unwrap(),
        );
    }
    assert!(
        p95 <= P95_LIMIT,
        "1e6 行已过滤分页查询 P95 必须 ≤300ms（ADR-023）：实际 {p95:?}"
    );

    let _ = boot.pipeline.shutdown().await;
    if let Some(storage) = boot.storage.take() {
        let _ = storage.shutdown().await;
    }
}
