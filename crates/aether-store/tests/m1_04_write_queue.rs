//! M1-04 DoD2/DoD3：group commit 批量参数生效、L1/L2 背压分级与 `storage_backpressure` 准入接口。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use aether_core::SessionId;
use aether_store::{BatchTrigger, Store, StoreError, StoreRuntime, WriteQueueConfig};

fn open_runtime(dir: &tempfile::TempDir, config: WriteQueueConfig) -> StoreRuntime {
    let handle = tokio::runtime::Handle::current();
    StoreRuntime::open(common::db_path(dir), config, &handle).unwrap()
}

// ===== DoD2：默认批量参数（16ms / ≥256 条 / mpsc 4096 / 4 读连接） =====

#[test]
fn dod2_default_batch_parameters_match_design() {
    let config = WriteQueueConfig::default();
    assert_eq!(config.capacity, 4_096, "D3：mpsc::channel(4096)");
    assert_eq!(config.max_batch_entries, 256, "D3：积压 ≥256 条提交");
    assert_eq!(
        config.flush_interval,
        Duration::from_millis(16),
        "D3：16ms 提交窗口"
    );
    assert_eq!(config.l1_threshold, 1_024, "D8：L1 >1024");
    assert_eq!(config.l2_threshold, 4_096, "D8：L2 >4096");
    assert_eq!(config.read_connections, 4, "D3：4 读连接");
    config.validate().expect("默认配置必须合法");
}

// ===== DoD2：≥256 条触发提交 =====

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod2_group_commit_triggers_on_256_entries() {
    let dir = common::temp_dir("batch-count");
    // 刷新窗口放大到 500ms：若批量未按条数触发，本用例会显著变慢并失败。
    let config = WriteQueueConfig {
        flush_interval: Duration::from_millis(500),
        ..WriteQueueConfig::default()
    };
    let runtime = open_runtime(&dir, config);
    let queue = runtime.queue().clone();

    let total = 512u64;
    let mut set = tokio::task::JoinSet::new();
    for seq in 0..total {
        let queue = queue.clone();
        set.spawn(async move {
            queue
                .append_events(vec![common::delta_event("sess-batch", seq)])
                .await
        });
    }
    let mut receipts = Vec::new();
    while let Some(joined) = set.join_next().await {
        receipts.push(joined.unwrap().unwrap());
    }
    assert_eq!(receipts.len(), usize::try_from(total).unwrap());

    let count_batches: Vec<_> = receipts
        .iter()
        .filter(|receipt| receipt.trigger == BatchTrigger::Count)
        .collect();
    assert!(
        !count_batches.is_empty(),
        "必须出现「≥256 条」触发的提交批次；实际触发方式: {:?}",
        receipts.iter().map(|r| r.trigger).collect::<Vec<_>>()
    );
    assert!(
        count_batches
            .iter()
            .all(|receipt| receipt.batch_entries >= 256),
        "Count 触发的批次条数必须 ≥256"
    );
    assert!(
        receipts.iter().all(|receipt| receipt.commit_ms < 500),
        "单事务必须 <500ms（D3 长事务禁止）"
    );

    let session = SessionId::new("sess-batch").unwrap();
    assert_eq!(runtime.reads().event_count(&session).await.unwrap(), total);
    let metrics = queue.metrics();
    assert_eq!(metrics.committed_entries, total);
    assert_eq!(metrics.failed_entries, 0);
    assert_eq!(metrics.max_batch_entries, 256, "单批最大条数应为 256");
    assert_eq!(metrics.depth, 0, "全部提交后队列深度必须归零");

    runtime.shutdown().await.unwrap();
}

// ===== DoD2：16ms 定时触发提交（默认参数） =====

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod2_group_commit_triggers_on_flush_interval() {
    let dir = common::temp_dir("batch-timer");
    let runtime = open_runtime(&dir, WriteQueueConfig::default());
    let queue = runtime.queue().clone();

    let started = Instant::now();
    let receipt = queue
        .append_events(common::delta_events("sess-timer", 0, 3))
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(
        receipt.trigger,
        BatchTrigger::Timer,
        "3 条不足 256，应定时提交"
    );
    assert_eq!(receipt.batch_entries, 3);
    assert!(
        elapsed >= Duration::from_millis(10),
        "定时提交必须等待 16ms 窗口（实际 {elapsed:?}）"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "16ms 窗口不得显著超时（实际 {elapsed:?}）"
    );

    let session = SessionId::new("sess-timer").unwrap();
    assert_eq!(runtime.reads().event_count(&session).await.unwrap(), 3);
    let metrics = queue.metrics();
    assert_eq!(metrics.committed_batches, 1);
    assert_eq!(metrics.pressure_level, None);

    runtime.shutdown().await.unwrap();
}

// ===== DoD3：默认阈值（>1024 / >4096）行为验证 =====
//
// 单个 4200 条作业 + 注入 500ms 提交延迟：提交窗口内队列深度稳定 >4096，
// 直接以 D3/D8 默认阈值（1024/4096）验证 L1/L2 告警深度与 storage_backpressure 拒绝。

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod3_default_thresholds_trigger_l1_l2_and_reject_admission() {
    let dir = common::temp_dir("pressure-default");
    let config = WriteQueueConfig {
        commit_delay: Duration::from_millis(500),
        ..WriteQueueConfig::default()
    };
    let runtime = open_runtime(&dir, config);
    let queue = runtime.queue().clone();
    let mut alerts = queue.subscribe_alerts();

    let submission = {
        let queue = queue.clone();
        tokio::spawn(async move {
            queue
                .append_events(common::delta_events("sess-pressure-default", 0, 4_200))
                .await
        })
    };

    // 提交窗口内：准入必须拒绝（默认 L2 阈值 4096）。
    let admission_deadline = Instant::now() + Duration::from_secs(5);
    let rejection = loop {
        match queue.admission() {
            Err(error) => break error,
            Ok(()) => {
                assert!(
                    Instant::now() < admission_deadline,
                    "4200 条积压期间 admission 必须拒绝"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    };
    assert_eq!(rejection.code(), "storage_backpressure");
    match rejection {
        StoreError::StorageBackpressure { depth, threshold } => {
            assert!(depth > 4_096, "拒绝时必须携带真实深度: {depth}");
            assert_eq!(threshold, 4_096, "默认 L2 阈值必须为 4096");
        }
        other => panic!("错误类型不符: {other:?}"),
    }

    // 告警（广播缓冲）：默认阈值下 L1(1024)/L2(4096) 边沿必须出现。
    let mut seen_l1 = false;
    let mut seen_l2 = false;
    let alert_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !(seen_l1 && seen_l2) {
        let alert = tokio::time::timeout_at(alert_deadline, alerts.recv())
            .await
            .expect("等待默认阈值告警超时")
            .expect("告警通道不应关闭");
        if alert.phase != aether_store::QueuePressurePhase::Enter {
            continue;
        }
        match alert.level {
            aether_store::QueuePressureLevel::L1 => {
                assert!(alert.depth > 1_024);
                assert_eq!(alert.threshold, 1_024);
                seen_l1 = true;
            }
            aether_store::QueuePressureLevel::L2 => {
                assert!(alert.depth > 4_096);
                assert_eq!(alert.threshold, 4_096);
                seen_l2 = true;
            }
        }
    }

    // drain 后自动回落（仅队列维度）。
    submission.await.unwrap().unwrap();
    let drain_deadline = Instant::now() + Duration::from_secs(10);
    while queue.depth() > 0 && Instant::now() < drain_deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(queue.depth(), 0);
    assert!(queue.admission().is_ok());
    assert_eq!(queue.pressure_level(), None);
    let metrics = queue.metrics();
    assert_eq!(metrics.committed_entries, 4_200);
    assert_eq!(metrics.failed_entries, 0);
    assert_eq!(metrics.l1_alerts, 1);
    assert_eq!(metrics.l2_alerts, 1);

    runtime.shutdown().await.unwrap();
}

// ===== DoD3：L1/L2 告警与 storage_backpressure 准入 =====
//
// 阈值逻辑使用参数化配置验证（默认常量 1024/4096 由 dod2_default_batch_parameters_match_design
// 逐字断言）；`commit_delay` 为测试注入：制造可控积压，验证水位跨级与准入拒绝。

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod3_l1_l2_alerts_and_admission_backpressure() {
    let dir = common::temp_dir("pressure");
    let config = WriteQueueConfig {
        capacity: 2_048,
        max_batch_entries: 64,
        flush_interval: Duration::from_millis(50),
        l1_threshold: 256,
        l2_threshold: 1_024,
        read_connections: 4,
        commit_delay: Duration::from_millis(100),
    };
    let runtime = open_runtime(&dir, config);
    let queue = runtime.queue().clone();
    assert_eq!(queue.depth(), 0);

    let mut alerts = queue.subscribe_alerts();

    // 12 个作业 × 100 条 = 1200 条，全部立即入队（容量 2048 > 1200）。
    let mut set = tokio::task::JoinSet::new();
    for job in 0..12u64 {
        let queue = queue.clone();
        set.spawn(async move {
            queue
                .append_events(common::delta_events("sess-pressure", job * 100, 100))
                .await
        });
    }

    let mut seen_l1_enter = false;
    let mut seen_l2_enter = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !(seen_l1_enter && seen_l2_enter) {
        let alert = tokio::time::timeout_at(deadline, alerts.recv())
            .await
            .expect("等待 L1/L2 告警超时")
            .expect("告警通道不应关闭");
        match (alert.level, alert.phase) {
            (aether_store::QueuePressureLevel::L2, aether_store::QueuePressurePhase::Enter) => {
                assert!(alert.depth > 1_024, "L2 触发深度必须 > 阈值: {alert:?}");
                assert_eq!(alert.threshold, 1_024);
                seen_l2_enter = true;
            }
            (aether_store::QueuePressureLevel::L1, aether_store::QueuePressurePhase::Enter) => {
                assert!(alert.depth > 256, "L1 触发深度必须 > 阈值: {alert:?}");
                assert_eq!(alert.threshold, 256);
                seen_l1_enter = true;
            }
            _ => {}
        }
    }
    assert!(
        queue.pressure_level() == Some(aether_store::QueuePressureLevel::L2),
        "积压期背压电平应为 L2"
    );

    // 准入接口：积压 >L2 阈值时必须返回 storage_backpressure（供 M2-04 拒绝新 run）。
    let admission_deadline = Instant::now() + Duration::from_secs(3);
    let rejection = loop {
        match queue.admission() {
            Err(error) => break error,
            Ok(()) => {
                assert!(
                    Instant::now() < admission_deadline,
                    "积压期 admission 必须拒绝（storage_backpressure）"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    };
    assert_eq!(rejection.code(), "storage_backpressure");
    match rejection {
        StoreError::StorageBackpressure { depth, threshold } => {
            assert!(
                depth > threshold,
                "拒绝时必须携带真实深度: {depth} > {threshold}"
            );
            assert_eq!(threshold, 1_024);
        }
        other => panic!("错误类型不符: {other:?}"),
    }

    // 排空全部作业并要求自动回落（队列维度，不涉及持久化降级）。
    while let Some(joined) = set.join_next().await {
        joined.unwrap().unwrap();
    }
    let drain_deadline = Instant::now() + Duration::from_secs(10);
    while queue.depth() > 0 && Instant::now() < drain_deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(queue.depth(), 0, "队列必须回落到 0");
    assert!(queue.admission().is_ok(), "队列回落 ≤L2 后准入必须恢复");
    assert_eq!(queue.pressure_level(), None, "回落后背压电平必须归正常");

    let metrics = queue.metrics();
    assert_eq!(metrics.committed_entries, 1_200);
    assert_eq!(metrics.failed_entries, 0);
    assert!(metrics.l1_alerts >= 1, "L1 告警计数必须 >0: {metrics:?}");
    assert!(metrics.l2_alerts >= 1, "L2 告警计数必须 >0: {metrics:?}");

    let session = SessionId::new("sess-pressure").unwrap();
    assert_eq!(runtime.reads().event_count(&session).await.unwrap(), 1_200);

    runtime.shutdown().await.unwrap();
}

// ===== 写队列正确性：UNIQUE(session_id, seq) 兜底失败经队列回传 =====

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_seq_write_fails_with_sqlite_constraint_code() {
    let dir = common::temp_dir("dup-seq");
    let runtime = open_runtime(&dir, WriteQueueConfig::default());
    let queue = runtime.queue().clone();

    let first = common::delta_event("sess-dup", 0);
    queue.append_events(vec![first]).await.unwrap();

    // 不同 id、相同 (session_id, seq)：命中 0002 的唯一索引。
    let duplicate = aether_core::EventEnvelope::from_json_str(
        r#"{"v":1,"id":"sess-dup-evt-other","session_id":"sess-dup","run_id":null,"runtime_id":"mock","seq":0,"ts":1760000000000,"type":"message.delta","payload":{"message_id":"sess-dup-msg","text":"dup"}}"#,
    )
    .unwrap();
    let error = queue.append_events(vec![duplicate]).await.unwrap_err();
    assert!(
        matches!(error, StoreError::WriteTransactionFailed { .. }),
        "实际: {error:?}"
    );
    assert_eq!(error.code(), "write_transaction_failed");
    match error {
        StoreError::WriteTransactionFailed { code, message } => {
            assert_eq!(
                code,
                Some(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
                "必须保留 UNIQUE 约束的 SQLite 扩展码（message={message}）"
            );
        }
        other => panic!("错误类型不符: {other:?}"),
    }

    let metrics = queue.metrics();
    assert_eq!(metrics.committed_entries, 1);
    assert_eq!(metrics.failed_entries, 1);
    assert_eq!(metrics.depth, 0);

    let session = SessionId::new("sess-dup").unwrap();
    assert_eq!(runtime.reads().event_count(&session).await.unwrap(), 1);

    runtime.shutdown().await.unwrap();
}

// ===== 安全模式（quick_check 失败）拒绝启动写运行时 =====

fn build_corrupt_store(path: &Path) {
    {
        let store = Store::open(path).unwrap();
        store
            .connection()
            .execute_batch(
                "CREATE TABLE fragile_data (id INTEGER PRIMARY KEY, payload TEXT NOT NULL);",
            )
            .unwrap();
        for index in 0..800 {
            store
                .connection()
                .execute(
                    "INSERT INTO fragile_data (payload) VALUES (?1)",
                    [format!("row-{index:04}-{}", "x".repeat(180))],
                )
                .unwrap();
        }
        store
            .connection()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }
    common::corrupt_last_page(path);
}

#[test]
fn store_runtime_refuses_to_start_in_safe_mode() {
    let dir = common::temp_dir("runtime-safe-mode");
    let path = common::db_path(&dir);
    build_corrupt_store(&path);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = StoreRuntime::open(&path, WriteQueueConfig::default(), runtime.handle());
    match result {
        Ok(_) => panic!("损坏库必须拒绝启动写运行时（安全模式只读）"),
        Err(error) => {
            assert!(
                matches!(error, StoreError::SafeModeWriteRefused { .. }),
                "实际: {error:?}"
            );
            assert!(error.to_string().contains("安全模式"));
        }
    }

    // 只读入口（备份/导出）仍由 M1-03 Store 提供。
    let store = Store::open(&path).unwrap();
    assert!(store.is_safe_mode());
    assert!(store
        .export_readable(dir.path().join("export.jsonl"))
        .is_ok());
}
