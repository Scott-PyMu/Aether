//! M2-06 DoD 集成测试：关闭序列五步顺序（shadow 日志）、退出后 `-wal` 0 字节与
//! 无残留句柄（平台文件语义断言）、读锁下运行期强制 checkpoint 的退避重试与诊断。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::mpsc;
use std::time::Duration;

use aether_core::SessionId;
use aether_store::{
    CheckpointConfig, ReadPool, ReadPoolCloseReport, ShutdownConfig, ShutdownStep, StoreRuntime,
    WriteQueueConfig,
};
use tokio::runtime::Handle;

fn open_runtime(config: WriteQueueConfig) -> (tempfile::TempDir, StoreRuntime) {
    let dir = common::temp_dir("m2-06");
    let runtime = StoreRuntime::open(common::db_path(&dir), config, &Handle::current()).unwrap();
    (dir, runtime)
}

/// 快捷写入 `count` 条事件（单会话 seq 连续）。
async fn write_events(runtime: &StoreRuntime, session: &str, count: usize) {
    runtime
        .queue()
        .append_events(common::delta_events(session, 0, count))
        .await
        .unwrap();
}

// ===== DoD1：顺序断言（shadow 日志）：五步顺序与 D2 完全一致 =====

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod1_shutdown_sequence_matches_d2_order() {
    let (_dir, runtime) = open_runtime(WriteQueueConfig::default());
    write_events(&runtime, "sess-order", 500).await;

    let session = SessionId::new("sess-order").unwrap();
    assert_eq!(
        runtime.reads().event_count(&session).await.unwrap(),
        500,
        "drain 前数据已全部落盘"
    );
    assert!(runtime.wal_size_bytes() > 0, "写入后 WAL 必须非空");

    let report = runtime.shutdown().await.unwrap();

    assert_eq!(
        report.order(),
        ShutdownStep::D2_ORDER.to_vec(),
        "shadow 日志顺序必须与 D2 五步一致：drain → 关读连接 → checkpoint(TRUNCATE) → 关写连接 → 退出"
    );
    assert!(report.matches_d2_order());
    assert!(report.drained, "写队列必须在 drain 上限内完成");
    assert!(!report.drain_timed_out);
    assert_eq!(
        report.read_close,
        ReadPoolCloseReport {
            connections: 4,
            closed: 4,
            timed_out: false
        },
        "4 个读连接必须全部关闭"
    );
    let checkpoint = report.checkpoint.as_ref().expect("checkpoint 必须执行");
    assert!(checkpoint.succeeded, "读连接已关闭，checkpoint 必须成功");
    assert!(!checkpoint.busy);
    assert_eq!(report.step(ShutdownStep::Exit).unwrap().code, "exit");

    // shadow 日志逐步骤证据（--nocapture）。
    println!("[m2-06 DoD1] shadow 日志：drain_write_queue → close_read_connections → wal_checkpoint_truncate → close_write_connection → exit");
    for record in &report.steps {
        println!(
            "[m2-06 DoD1]   {} at={}ms duration={}ms detail={}",
            record.code, record.at_ms, record.duration_ms, record.detail
        );
    }
    println!(
        "[m2-06 DoD1] checkpoint 诊断：{}",
        checkpoint.diagnostic_summary()
    );
}

// ===== DoD2：退出后 `-wal` 为 0 字节、无残留句柄（平台工具断言） =====

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod2_wal_zero_and_no_residual_handles_after_shutdown() {
    let (_dir, runtime) = open_runtime(WriteQueueConfig::default());
    write_events(&runtime, "sess-wal", 500).await;

    let db = runtime.path().to_path_buf();
    let wal = runtime.wal_path();
    let shm = runtime.shm_path();
    assert!(runtime.wal_size_bytes() > 0, "写入后 -wal 必须存在且非空");

    let report = runtime.shutdown().await.unwrap();
    assert!(report.checkpoint.as_ref().unwrap().succeeded);

    let wal_size = std::fs::metadata(&wal).map(|meta| meta.len()).unwrap_or(0);
    let shm_size = std::fs::metadata(&shm).map(|meta| meta.len()).unwrap_or(0);
    assert_eq!(wal_size, 0, "-wal 必须为 0 字节（退出后）");
    assert_eq!(shm_size, 0, "-shm 不得残留（最后连接关闭后由 SQLite 删除）");

    // 平台断言（Windows）：SQLite 打开库文件时不共享 DELETE，重命名/删除成功
    // 即证明无残留文件句柄；Unix 下同样验证文件系统语义。
    let renamed = db.with_file_name("aether.renamed.db");
    std::fs::rename(&db, &renamed)
        .expect("关停后库文件必须可重命名（存在残留句柄时 Windows 会拒绝）");
    std::fs::rename(&renamed, &db).expect("必须可改回原名");
    std::fs::remove_file(&db).expect("关停后库文件必须可删除（无残留句柄）");

    println!(
        "[m2-06 DoD2] 关停后：-wal={} 字节、-shm={} 字节（文件存在性：wal={} shm={}）；重命名/改回/删除全部成功（无残留句柄）",
        wal_size,
        shm_size,
        wal.exists(),
        shm.exists()
    );
}

// ===== DoD3：读锁占用下强制 checkpoint 失败 → 退避重试并记录诊断（单测） =====

/// 在读连接池上开启一个读事务并阻塞持有（模拟读锁占用）；
/// 返回（释放信号, 持有任务句柄）。
async fn hold_read_lock(reads: &ReadPool) -> (mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
    let reads = reads.clone();
    let (locked_tx, locked_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder = tokio::spawn(async move {
        let _ = reads
            .with_connection(move |conn| {
                conn.execute_batch("BEGIN; SELECT COUNT(*) FROM events;")?;
                let _ = locked_tx.send(());
                let _ = release_rx.recv();
                conn.execute_batch("COMMIT;")?;
                Ok(())
            })
            .await;
    });
    tokio::task::spawn_blocking(move || locked_rx.recv().unwrap())
        .await
        .unwrap();
    (release_tx, holder)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod3_forced_checkpoint_retries_until_read_lock_released() {
    let (_dir, runtime) = open_runtime(WriteQueueConfig::default());
    write_events(&runtime, "sess-lock", 300).await;
    assert!(runtime.wal_size_bytes() > 0);

    let (release, holder) = hold_read_lock(runtime.reads()).await;

    // 强制 checkpoint 与持锁读并发：读锁未释放 → busy → 退避重试。
    let queue = runtime.queue().clone();
    let config = CheckpointConfig {
        max_attempts: 40,
        initial_backoff: Duration::from_millis(15),
        max_backoff: Duration::from_millis(25),
    };
    let checkpoint = tokio::spawn(async move { queue.checkpoint_truncate(config).await });
    tokio::time::sleep(Duration::from_millis(120)).await;
    release.send(()).unwrap();
    holder.await.unwrap();
    let report = checkpoint.await.unwrap().unwrap();

    assert!(
        report.succeeded,
        "读锁释放后 checkpoint 必须成功：{report:?}"
    );
    assert!(report.retried(), "必须发生退避重试");
    assert!(report.busy_attempts() >= 1, "必须记录读锁（busy）尝试");
    assert!(
        report
            .history
            .iter()
            .any(|attempt| attempt.busy && attempt.error.is_some()),
        "读锁尝试必须带诊断：{:?}",
        report.history
    );
    assert_eq!(runtime.wal_size_bytes(), 0, "TRUNCATE 后 WAL 必须归零");

    println!(
        "[m2-06 DoD3] 读锁释放后 checkpoint 成功：{}；busy 尝试={}；逐次记录={:?}",
        report.diagnostic_summary(),
        report.busy_attempts(),
        report
            .history
            .iter()
            .map(|attempt| (attempt.attempt, attempt.busy, attempt.waited_ms))
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod3_forced_checkpoint_exhausts_attempts_and_records_diagnostics() {
    let (_dir, runtime) = open_runtime(WriteQueueConfig::default());
    write_events(&runtime, "sess-lock2", 300).await;

    let (release, holder) = hold_read_lock(runtime.reads()).await;
    let config = CheckpointConfig {
        max_attempts: 3,
        initial_backoff: Duration::from_millis(5),
        max_backoff: Duration::from_millis(10),
    };
    let report = runtime
        .queue()
        .checkpoint_truncate(config.clone())
        .await
        .unwrap();
    // 先释放读锁（避免断言失败时阻塞线程悬留），再做报告断言。
    release.send(()).unwrap();
    holder.await.unwrap();

    assert!(!report.succeeded, "读锁持续占用时不得声称成功");
    assert_eq!(report.attempts, 3, "必须尝试满 max_attempts");
    assert_eq!(report.busy_attempts(), 3, "全部尝试均记录读锁诊断");
    assert!(report.busy);
    assert!(
        report.error.as_deref().unwrap_or_default().contains("BUSY"),
        "失败原因必须标记读锁：{:?}",
        report.error
    );
    assert!(
        report
            .history
            .iter()
            .skip(1)
            .all(|attempt| attempt.waited_ms > 0),
        "重试必须经历退避等待"
    );

    // 释放读锁 → 同一入口再次执行成功（证明失败源于读锁）。
    let again = runtime
        .queue()
        .checkpoint_truncate(CheckpointConfig::default())
        .await
        .unwrap();
    assert!(again.succeeded, "释放读锁后必须成功：{again:?}");
    assert_eq!(runtime.wal_size_bytes(), 0);

    println!(
        "[m2-06 DoD3] 读锁未释放：attempts={} busy={} 诊断={}；释放后重跑成功 attempts={}",
        report.attempts,
        report.busy_attempts(),
        report.diagnostic_summary(),
        again.attempts
    );
}

/// 运行期强制 checkpoint 触发口径（D3：WAL >256MB；测试用 0 字节阈值验证触发/不触发）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintenance_checkpoint_triggers_only_above_threshold() {
    let (_dir, runtime) = open_runtime(WriteQueueConfig::default());
    write_events(&runtime, "sess-maint", 200).await;
    assert!(runtime.wal_size_bytes() > 0);

    // 默认 256MB 阈值：不触发（返回值 None，WAL 保持非空）。
    assert!(
        runtime.maintenance_checkpoint().await.unwrap().is_none(),
        "WAL 未超过 256MB 不得触发"
    );
    assert!(runtime.wal_size_bytes() > 0);

    // 阈值 0：触发并成功 TRUNCATE。
    let report = runtime
        .checkpoint_if_wal_exceeds(0, CheckpointConfig::default())
        .await
        .unwrap()
        .expect("WAL > 0 字节阈值必须触发");
    assert!(report.succeeded);
    assert_eq!(runtime.wal_size_bytes(), 0, "强制 checkpoint 后 WAL 归零");

    // 已归零：不再触发。
    assert!(runtime
        .checkpoint_if_wal_exceeds(0, CheckpointConfig::default())
        .await
        .unwrap()
        .is_none());

    // 非法配置拒绝。
    let error = runtime
        .checkpoint_if_wal_exceeds(
            0,
            CheckpointConfig {
                max_attempts: 0,
                ..CheckpointConfig::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "invalid_checkpoint_config");

    println!("[m2-06 运行期] 强制 checkpoint：256MB 阈值不触发；阈值 0 触发并归零；非法配置拒绝");
}

// ===== drain 超时：放弃在途写入（无成功回执）仍完成五步序列 =====

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_timeout_abandons_pending_writes_and_still_completes_sequence() {
    // commit_delay 注入：写任务提交前阻塞，drain 标记无法在其上限内被处理。
    let (_dir, runtime) = open_runtime(WriteQueueConfig {
        commit_delay: Duration::from_millis(300),
        ..WriteQueueConfig::default()
    });
    let queue = runtime.queue().clone();
    let wal = runtime.wal_path();

    let mut set = tokio::task::JoinSet::new();
    for seq in 0..300u64 {
        let queue = queue.clone();
        set.spawn(async move {
            queue
                .append_events(vec![common::delta_event("sess-timeout", seq)])
                .await
        });
    }
    // 等写任务进入 commit_delay 窗口（批次已收集、尚未提交）。
    tokio::time::sleep(Duration::from_millis(60)).await;

    let report = runtime
        .shutdown_with(ShutdownConfig {
            drain_timeout: Duration::from_millis(10),
            read_close_timeout: Duration::from_secs(1),
            checkpoint: CheckpointConfig::default(),
        })
        .await
        .unwrap();

    assert!(report.drain_timed_out, "drain 必须按上限超时");
    assert!(!report.drained);
    assert!(
        report.matches_d2_order(),
        "超时路径仍必须完成五步序列：{:?}",
        report.order()
    );
    assert!(
        report
            .checkpoint
            .as_ref()
            .map(|c| c.succeeded)
            .unwrap_or(false),
        "临时写连接必须完成 checkpoint：{:?}",
        report.checkpoint
    );

    // 未落盘写入不得产生成功回执（放弃语义，无假成功）。
    let mut receipts = 0usize;
    while let Some(joined) = set.join_next().await {
        if joined.unwrap().is_ok() {
            receipts += 1;
        }
    }
    assert_eq!(receipts, 0, "drain 超时后不得有成功回执");
    let wal_size = std::fs::metadata(&wal).map(|meta| meta.len()).unwrap_or(0);
    assert_eq!(wal_size, 0, "临时写连接 checkpoint 后 WAL 必须归零");

    println!(
        "[m2-06 drain 超时] shadow: {}；成功回执={}；wal={} 字节",
        report.shadow_log(),
        receipts,
        wal_size
    );
}
