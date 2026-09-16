//! M1-04 DoD4：写入压测期间读延迟 P95 <10ms（WAL 生效证明：读写并发、读不被写阻塞）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use aether_core::SessionId;
use aether_store::{pragma, StoreRuntime, WriteQueueConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod4_read_latency_p95_under_write_load_is_below_10ms() {
    let dir = common::temp_dir("read-wal");
    // commit_delay 拉长写事务窗口，保证读写真实重叠（注入参数，验证读不被写阻塞）。
    let config = WriteQueueConfig {
        commit_delay: Duration::from_millis(20),
        ..WriteQueueConfig::default()
    };
    let runtime = StoreRuntime::open(
        common::db_path(&dir),
        config,
        &tokio::runtime::Handle::current(),
    )
    .unwrap();
    let queue = runtime.queue().clone();
    let reads = runtime.reads().clone();
    assert_eq!(reads.connection_count(), 4, "D3：4 个读连接");

    // 写负载：8 个并发任务 × 8 个作业 × 50 条 = 3200 条事件。
    let mut writers = tokio::task::JoinSet::new();
    for job in 0..64u64 {
        let queue = queue.clone();
        writers.spawn(async move {
            queue
                .append_events(common::delta_events("sess-rw", job * 50, 50))
                .await
        });
    }

    // 等待写积压出现后再开始读，确保读发生在写事务压力期间。
    let overlap_deadline = Instant::now() + Duration::from_secs(10);
    while queue.depth() == 0 && Instant::now() < overlap_deadline {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(queue.depth() > 0, "写负载必须先产生积压（读写重叠前提）");

    // 读负载：4 个并发读者 × 50 次读取，期间记录队列深度以证明读写重叠。
    let session = SessionId::new("sess-rw").unwrap();
    let mut readers = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let reads = reads.clone();
        let queue = queue.clone();
        let session = session.clone();
        readers.spawn(async move {
            let mut latencies = Vec::with_capacity(50);
            let mut max_depth = 0usize;
            for _ in 0..50 {
                let started = Instant::now();
                let count = reads.event_count(&session).await.unwrap();
                latencies.push(started.elapsed());
                max_depth = max_depth.max(queue.depth());
                std::hint::black_box(count);
            }
            (latencies, max_depth)
        });
    }

    let mut latencies: Vec<Duration> = Vec::with_capacity(200);
    let mut max_depth_during_reads = 0usize;
    while let Some(joined) = readers.join_next().await {
        let (mut round, observed) = joined.unwrap();
        latencies.append(&mut round);
        max_depth_during_reads = max_depth_during_reads.max(observed);
    }
    while let Some(joined) = writers.join_next().await {
        joined.unwrap().unwrap();
    }

    assert_eq!(latencies.len(), 200, "读样本数必须为 200");
    let p95 = common::percentile_95(&latencies);
    println!(
        "DoD4 读样本: n={} P95={p95:?} max={:?}（写压力期间观测队列深度={max_depth_during_reads}）",
        latencies.len(),
        latencies.iter().max().unwrap()
    );
    assert!(
        p95 < Duration::from_millis(10),
        "写入压测期间读延迟 P95 必须 <10ms，实际 {p95:?}"
    );
    assert!(
        max_depth_during_reads > 0,
        "读取必须与写积压重叠（WAL 读写并发）"
    );

    // WAL 生效证明：读连接上 journal_mode 仍为 wal；写连接（M1-03 PRAGMA）一致。
    let journal_mode = reads
        .with_connection(|conn| Ok(pragma::snapshot(conn)?.journal_mode))
        .await
        .unwrap();
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

    // 全量数据落盘且读得回来的（补读分页入口）。
    let count = reads.event_count(&session).await.unwrap();
    assert_eq!(count, 3_200);
    let page = reads.events_page(&session, None, 3_200).await.unwrap();
    assert_eq!(page.len(), 3_200);
    assert!(
        page.windows(2).all(|pair| pair[0].seq < pair[1].seq),
        "补读页必须按 seq 升序"
    );
    assert_eq!(page.first().unwrap().seq, 0);
    assert_eq!(page.last().unwrap().seq, 3_199);
    assert_eq!(reads.max_seq(&session).await.unwrap(), Some(3_199));

    // 断点续传语义：after_seq 为排他边界（Some(0) 从 seq=1 开始）。
    let resumed = reads.events_page(&session, Some(0), 3_200).await.unwrap();
    assert_eq!(resumed.len(), 3_199);
    assert_eq!(resumed.first().unwrap().seq, 1);

    // 空会话补读返回空页。
    let other = SessionId::new("sess-empty").unwrap();
    assert!(reads
        .events_page(&other, None, 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(reads.max_seq(&other).await.unwrap(), None);

    // 关停 drain：队列中无未提交项，写任务正常退出。
    assert_eq!(queue.metrics().depth, 0);
    runtime.shutdown().await.unwrap();
}
