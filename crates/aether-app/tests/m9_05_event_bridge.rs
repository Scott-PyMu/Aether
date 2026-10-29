//! M9-05 事件桥（P2 计划 M9-05 DoD-2/3/4；ADR-029 §4.4/§4.7）。
//!
//! 覆盖：
//! - 写失败 → 不广播、不进入事件桥（先日志后广播）；
//! - 10 会话 × 50 delta/s：控制事件 0 丢失、delta 可丢/可补、P95 <150ms；
//! - 控制队列满：丢 delta + journal 补读，不重启 daemon；
//! - 连续 3 次补读失败：重建事件订阅 + 审计（daemon 进程不重启）。
//!
//! 使用真实 `StoreRuntime`（SQLite 单写队列 + 读池）与真实 `EventPipeline`；
//! 仅传输出口为测试记录器/慢出口（故障注入）。

// 集成测试文件级豁免（断言失败即 panic）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_app::event_bridge::{
    DeliveryConfig, EventBridge, EventFeed, EventSink, ReadbackFuture, SinkError,
    ESCALATION_AUDIT_ACTION,
};
use aether_control::{
    EventPipeline, EventSource, JournalError, JournalFuture, JournalWriter, PipelineConfig,
    ReadbackFrame, SourceFuture, StartupSelfCheckReport, StoreEventSource, StoreJournal,
};
use aether_core::{
    EventEnvelope, EventId, EventPayload, MessageDeltaPayload, MessageId, RuntimeId, SessionId,
    SessionUpdatedPayload, EVENT_ENVELOPE_VERSION,
};
use aether_store::{StoreRuntime, WriteQueueConfig};
use serde_json::{json, Value};
use tokio::runtime::Handle;

const DATABASE_FILE: &str = "aether.db";
const SESSIONS: [&str; 10] = [
    "01J000000000000000000000A0",
    "01J000000000000000000000A1",
    "01J000000000000000000000A2",
    "01J000000000000000000000A3",
    "01J000000000000000000000A4",
    "01J000000000000000000000A5",
    "01J000000000000000000000A6",
    "01J000000000000000000000A7",
    "01J000000000000000000000A8",
    "01J000000000000000000000A9",
];

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn raw_event(id: &str, session: &str, event_type: &str, payload: Value) -> Value {
    json!({
        "v": EVENT_ENVELOPE_VERSION,
        "id": id,
        "session_id": session,
        "run_id": null,
        "runtime_id": "mock",
        "seq": 0,
        "ts": now_ms(),
        "type": event_type,
        "payload": payload,
    })
}

fn session_updated(id: &str, session: &str, title: &str) -> Value {
    raw_event(
        id,
        session,
        "session.updated",
        json!({"session_id": session, "changed_fields": ["title"], "title": title}),
    )
}

fn log_event(id: &str, session: &str) -> Value {
    raw_event(
        id,
        session,
        "log",
        json!({"level": "info", "message": format!("log {id}")}),
    )
}

fn delta_event(id: &str, session: &str, message_id: &str, text: &str) -> Value {
    raw_event(
        id,
        session,
        "message.delta",
        json!({"message_id": message_id, "text": text}),
    )
}

#[derive(Debug, Clone)]
struct Recorded {
    event: EventEnvelope,
    received_ms: i64,
}

/// 记录型出口（正常路径）。
#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<Recorded>>,
}

impl RecordingSink {
    fn events(&self) -> Vec<Recorded> {
        match self.events.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn control_count(&self) -> usize {
        self.events()
            .iter()
            .filter(|record| record.event.event_type().as_str() == "session.updated")
            .count()
    }
}

impl EventSink for RecordingSink {
    fn send(&self, event: &EventEnvelope) -> Result<(), SinkError> {
        let record = Recorded {
            event: event.clone(),
            received_ms: now_ms(),
        };
        let mut guard = match self.events.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push(record);
        Ok(())
    }
}

/// 可控闸门出口（制造确定性队列压力；故障注入）。
///
/// `send` 在闸门关闭时阻塞（1ms 轮询），用于稳定触发「队列满 → 丢 delta / 补读」。
struct GatedSink {
    events: Mutex<Vec<EventEnvelope>>,
    open: AtomicBool,
}

impl GatedSink {
    fn new(open: bool) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            open: AtomicBool::new(open),
        }
    }

    fn open(&self) {
        self.open.store(true, Ordering::Release);
    }

    fn close(&self) {
        self.open.store(false, Ordering::Release);
    }

    fn events(&self) -> Vec<EventEnvelope> {
        match self.events.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl EventSink for GatedSink {
    fn send(&self, event: &EventEnvelope) -> Result<(), SinkError> {
        while !self.open.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut guard = match self.events.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push(event.clone());
        Ok(())
    }
}

/// 注入事件来源（10×50 速率测试；直接按 seq 发布，不受管线合并窗口影响）。
struct MockFeed {
    sender: tokio::sync::broadcast::Sender<EventEnvelope>,
    events: Mutex<Vec<EventEnvelope>>,
}

impl MockFeed {
    fn new() -> Self {
        let (sender, _receiver) = tokio::sync::broadcast::channel(8_192);
        Self {
            sender,
            events: Mutex::new(Vec::new()),
        }
    }

    fn publish(&self, event: EventEnvelope) {
        {
            let mut guard = match self.events.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.push(event.clone());
        }
        let _ = self.sender.send(event);
    }
}

impl EventFeed for MockFeed {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EventEnvelope> {
        self.sender.subscribe()
    }

    fn readback<'a>(&'a self, session_id: &'a SessionId, last_seq: u64) -> ReadbackFuture<'a> {
        Box::pin(async move {
            let events: Vec<EventEnvelope> = {
                let guard = match self.events.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard
                    .iter()
                    .filter(|event| event.session_id == *session_id && event.seq > last_seq)
                    .cloned()
                    .collect()
            };
            let max_seq = events.last().map(|event| event.seq);
            Ok(ReadbackFrame {
                session_id: session_id.clone(),
                last_seq,
                max_seq,
                complete: true,
                events,
            })
        })
    }
}

fn envelope(id: &str, session: &str, seq: u64, payload: EventPayload) -> EventEnvelope {
    EventEnvelope {
        v: EVENT_ENVELOPE_VERSION,
        id: EventId::new(id).expect("event id"),
        session_id: SessionId::new(session).expect("session"),
        run_id: None,
        runtime_id: RuntimeId::new("mock").expect("runtime"),
        seq,
        ts: now_ms(),
        payload,
    }
}

struct StoreFixture {
    store: StoreRuntime,
    pipeline: EventPipeline,
    _dir: tempfile::TempDir,
}

impl StoreFixture {
    fn open(handle: &Handle, source: Option<Arc<dyn EventSource>>) -> Self {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join(DATABASE_FILE);
        let store =
            StoreRuntime::open(&path, WriteQueueConfig::default(), handle).expect("打开存储运行时");
        let journal = Arc::new(StoreJournal::new(store.queue().clone()));
        let source =
            source.unwrap_or_else(|| Arc::new(StoreEventSource::new(store.reads().clone())));
        // 测试缩短 delta 合并窗口（生产 16ms 不变）：让 10×50 用例聚焦事件桥，
        // 避免 sequencer/合并窗口等待主导测量（离散步长不改变丢/补语义）。
        let pipeline = EventPipeline::start(
            PipelineConfig {
                delta_flush_interval: Duration::from_millis(1),
                persist_retry_delay: Duration::ZERO,
                ..PipelineConfig::default()
            },
            journal,
            source,
            &StartupSelfCheckReport::passing(4 * 1024 * 1024 * 1024),
            handle,
        )
        .expect("启动事件管线");
        Self {
            store,
            pipeline,
            _dir: dir,
        }
    }

    async fn shutdown(self) {
        let _ = self.pipeline.shutdown().await;
        let _ = self.store.shutdown().await;
    }
}

async fn wait_for(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    condition()
}

/// DoD-2：写失败 → 管线不广播，事件桥不产生 UI 事件（先日志后广播）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_failure_is_not_broadcast_to_the_event_bridge() {
    struct FailingJournal;
    impl JournalWriter for FailingJournal {
        fn append(&self, _events: Vec<EventEnvelope>) -> JournalFuture<'_> {
            Box::pin(async {
                Err(JournalError::TransactionFailed {
                    code: Some(13),
                    message: "database or disk is full".to_owned(),
                })
            })
        }
    }
    struct EmptySource;
    impl EventSource for EmptySource {
        fn max_seq(&self, _session_id: &SessionId) -> SourceFuture<'_, Option<u64>> {
            Box::pin(async { Ok(None) })
        }
        fn events_after(
            &self,
            _session_id: &SessionId,
            _after_seq: Option<u64>,
            _limit: usize,
        ) -> SourceFuture<'_, Vec<EventEnvelope>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    let handle = Handle::current();
    let pipeline = EventPipeline::start(
        PipelineConfig {
            persist_retry_delay: Duration::ZERO,
            ..PipelineConfig::default()
        },
        Arc::new(FailingJournal),
        Arc::new(EmptySource),
        &StartupSelfCheckReport::passing(4 * 1024 * 1024 * 1024),
        &handle,
    )
    .expect("管线启动");
    let sink = Arc::new(RecordingSink::default());
    let mut bridge = EventBridge::new(pipeline.clone(), sink.clone())
        .spawn(&handle)
        .expect("事件桥启动");

    let result = pipeline
        .submit(log_event("01J00000000000000000000F01", SESSIONS[0]))
        .await;
    assert!(result.is_err(), "写失败必须返回错误：{result:?}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        sink.events().is_empty(),
        "写失败不得广播到事件桥（先日志后广播）"
    );

    let _ = bridge.shutdown(Duration::from_secs(1)).await;
    let _ = pipeline.shutdown().await;
}

/// DoD-3：10 会话 × 50 delta/s；控制事件 0 丢失；P95 <150ms；delta 可丢并计数。
///
/// 使用注入事件来源（[`MockFeed`]）按真实节奏发布 500 delta/s，测量**事件桥**
/// 的排队/FIFO/延迟；真实 `EventPipeline` 的先日志后广播/补读由
/// `write_failure_*` / `control_overflow_*` / `three_consecutive_*` 覆盖。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_sessions_fifty_deltas_per_second_keep_all_control_events() {
    let handle = Handle::current();
    let feed = Arc::new(MockFeed::new());
    let sink = Arc::new(RecordingSink::default());
    let mut bridge = EventBridge::new_with_feed(feed.clone(), sink.clone())
        .spawn(&handle)
        .expect("事件桥启动");

    let started = std::time::Instant::now();
    let mut event_counter = 0_u64;
    for (index, session) in SESSIONS.iter().enumerate() {
        event_counter += 1;
        feed.publish(envelope(
            &format!("01J{event_counter:023}"),
            session,
            1,
            EventPayload::SessionUpdated(SessionUpdatedPayload {
                session_id: SessionId::new(*session).expect("session"),
                changed_fields: vec!["title".to_owned()],
                title: Some(format!("start-{index}")),
            }),
        ));
    }
    for tick in 0..50_u64 {
        for (index, session) in SESSIONS.iter().enumerate() {
            event_counter += 1;
            feed.publish(envelope(
                &format!("01J{event_counter:023}"),
                session,
                tick + 2,
                EventPayload::MessageDelta(MessageDeltaPayload {
                    message_id: MessageId::new(format!("01J00000000000000000000M{index}"))
                        .expect("message"),
                    text: format!("delta-{tick}"),
                }),
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for (index, session) in SESSIONS.iter().enumerate() {
        event_counter += 1;
        feed.publish(envelope(
            &format!("01J{event_counter:023}"),
            session,
            52,
            EventPayload::SessionUpdated(SessionUpdatedPayload {
                session_id: SessionId::new(*session).expect("session"),
                changed_fields: vec!["title".to_owned()],
                title: Some(format!("end-{index}")),
            }),
        ));
    }
    // 尾部事件（16ms 批处理边界）。
    tokio::time::sleep(Duration::from_millis(100)).await;

    let all_control_delivered = wait_for(
        || sink.control_count() >= SESSIONS.len() * 2,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        all_control_delivered,
        "控制事件必须 0 丢失：期望 {}，实际 {}",
        SESSIONS.len() * 2,
        sink.control_count()
    );

    let recorded = sink.events();
    let controls: Vec<_> = recorded
        .iter()
        .filter(|record| record.event.event_type().as_str() == "session.updated")
        .collect();
    assert_eq!(controls.len(), SESSIONS.len() * 2, "控制事件不得重复投递");
    let mut seen = std::collections::HashSet::new();
    for record in &controls {
        assert!(
            seen.insert((record.event.session_id.clone(), record.event.seq)),
            "重复的控制事件：{}#{}",
            record.event.session_id,
            record.event.seq
        );
    }
    // 每会话 seq 严格递增（乱序由 EventStore 处理；传输层保持单调）。
    let mut last_seq: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
    for record in &recorded {
        let session = record.event.session_id.as_str();
        let previous = last_seq.get(session).copied().unwrap_or(0);
        assert!(
            record.event.seq > previous,
            "会话 {session} seq 非单调：{} → {}",
            previous,
            record.event.seq
        );
        last_seq.insert(session, record.event.seq);
    }
    assert_eq!(
        recorded
            .iter()
            .filter(|record| record.event.event_type().as_str() == "message.delta")
            .count(),
        SESSIONS.len() * 50,
        "500 delta 必须全部投递（delta 可丢仅指队列满时）"
    );

    let mut latencies: Vec<u64> = recorded
        .iter()
        .map(|record| u64::try_from((record.received_ms - record.event.ts).max(0)).unwrap_or(0))
        .collect();
    latencies.sort_unstable();
    let p95_index = latencies
        .len()
        .saturating_mul(95)
        .div_ceil(100)
        .saturating_sub(1);
    let p95_ms = latencies.get(p95_index).copied().unwrap_or(0);
    assert!(p95_ms < 150, "P95 延迟必须 <150ms：{p95_ms}ms");
    let metrics = bridge.metrics();
    assert_eq!(metrics.connection_rebuilds, 0, "正常吞吐不得重建订阅");
    assert_eq!(metrics.sink_errors, 0);
    assert_eq!(metrics.dropped_delta, 0, "默认队列容量下不得丢 delta");

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    println!(
        "M9_05_10X50_REPORT {}",
        json!({
            "sessions": SESSIONS.len(),
            "deltas_per_second": 50,
            "elapsed_ms": elapsed_ms,
            "control_expected": SESSIONS.len() * 2,
            "control_received": controls.len(),
            "control_lost": 0,
            "deltas_received": SESSIONS.len() * 50,
            "dropped_delta": metrics.dropped_delta,
            "p95_ms": p95_ms,
        })
    );

    let _ = bridge.shutdown(Duration::from_secs(2)).await;
}

/// DoD-4：控制队列满 → 丢 delta + journal 补读；不重启 daemon（订阅不重建）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_overflow_uses_journal_readback_and_keeps_control_events() {
    let handle = Handle::current();
    let fixture = StoreFixture::open(&handle, None);
    let sink = Arc::new(GatedSink::new(false));
    let mut bridge = EventBridge::new(fixture.pipeline.clone(), sink.clone())
        .with_config(DeliveryConfig {
            queue_capacity: 2,
            ..DeliveryConfig::default()
        })
        .spawn(&handle)
        .expect("事件桥启动");

    // 先制造 delta/log 积压（可丢），再灌控制事件（不可丢 → 补读）。
    let mut joins = tokio::task::JoinSet::new();
    for index in 0..40 {
        let pipeline = fixture.pipeline.clone();
        let event = delta_event(
            &format!("01J{:023}", 900_000 + index),
            SESSIONS[0],
            &format!("01J{:023}", 930_000 + index),
            &format!("delta-{index}"),
        );
        joins.spawn(async move { pipeline.submit(event).await });
    }
    while let Some(result) = joins.join_next().await {
        result.expect("任务").expect("delta");
    }
    let mut joins = tokio::task::JoinSet::new();
    for index in 0..8 {
        let pipeline = fixture.pipeline.clone();
        let event = session_updated(
            &format!("01J{:023}", 910_000 + index),
            SESSIONS[0],
            &format!("control-{index}"),
        );
        joins.spawn(async move { pipeline.submit(event).await });
    }
    while let Some(result) = joins.join_next().await {
        result.expect("任务").expect("控制事件");
    }
    // 闸门关闭期间事件桥已进入「丢 delta / 控制补读」路径。
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before_open = bridge.metrics();
    assert!(
        before_open.dropped_delta > 0,
        "队列满必须丢弃可丢 delta/log 并计数：{before_open:?}"
    );
    sink.open();

    let all_control_delivered = wait_for(
        || {
            sink.events()
                .iter()
                .filter(|event| event.event_type().as_str() == "session.updated")
                .count()
                >= 8
        },
        Duration::from_secs(15),
    )
    .await;
    assert!(all_control_delivered, "控制事件必须经补读全部送达");

    let controls: Vec<_> = sink
        .events()
        .into_iter()
        .filter(|event| event.event_type().as_str() == "session.updated")
        .collect();
    assert_eq!(controls.len(), 8, "控制事件不得重复/丢失");
    let seqs: Vec<u64> = controls.iter().map(|event| event.seq).collect();
    // 先写入 40 条 log（seq 1..=40），控制事件从 41 开始。
    assert_eq!(seqs, (41..=48).collect::<Vec<_>>(), "控制事件按 seq 有序");

    let metrics = bridge.metrics();
    assert!(
        metrics.dropped_delta > 0,
        "队列满必须丢弃可丢 delta/log 并计数：{metrics:?}"
    );
    assert!(
        metrics.readback_events > 0,
        "控制事件队列满必须切 journal 补读：{metrics:?}"
    );
    assert_eq!(
        metrics.connection_rebuilds, 0,
        "补读成功不得重建订阅/重启 daemon"
    );

    let _ = bridge.shutdown(Duration::from_secs(2)).await;
    fixture.shutdown().await;
}

/// 可切换失败的读源（补读故障注入）。
struct ToggleFailSource {
    inner: StoreEventSource,
    fail: Arc<AtomicBool>,
}

impl EventSource for ToggleFailSource {
    fn max_seq(&self, session_id: &SessionId) -> SourceFuture<'_, Option<u64>> {
        if self.fail.load(Ordering::Acquire) {
            return Box::pin(async {
                Err(aether_control::SourceError::Unavailable {
                    message: "注入的补读失败".to_owned(),
                })
            });
        }
        self.inner.max_seq(session_id)
    }

    fn events_after(
        &self,
        session_id: &SessionId,
        after_seq: Option<u64>,
        limit: usize,
    ) -> SourceFuture<'_, Vec<EventEnvelope>> {
        if self.fail.load(Ordering::Acquire) {
            return Box::pin(async {
                Err(aether_control::SourceError::Unavailable {
                    message: "注入的补读失败".to_owned(),
                })
            });
        }
        self.inner.events_after(session_id, after_seq, limit)
    }
}

/// DoD-4：连续 3 次补读失败 → 重建事件订阅 + 审计（daemon 不重启）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_consecutive_readback_failures_escalate_with_audit() {
    let handle = Handle::current();
    let fail = Arc::new(AtomicBool::new(false));
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join(DATABASE_FILE);
    let store =
        StoreRuntime::open(&path, WriteQueueConfig::default(), &handle).expect("存储运行时");
    let source = Arc::new(ToggleFailSource {
        inner: StoreEventSource::new(store.reads().clone()),
        fail: Arc::clone(&fail),
    });
    let pipeline = EventPipeline::start(
        PipelineConfig::default(),
        Arc::new(StoreJournal::new(store.queue().clone())),
        source,
        &StartupSelfCheckReport::passing(4 * 1024 * 1024 * 1024),
        &handle,
    )
    .expect("管线启动");
    let sink = Arc::new(GatedSink::new(true));
    let mut bridge = EventBridge::new(pipeline.clone(), sink.clone())
        .with_config(DeliveryConfig {
            queue_capacity: 2,
            ..DeliveryConfig::default()
        })
        .with_audit_write(store.queue().clone())
        .spawn(&handle)
        .expect("事件桥启动");

    // 先落一条控制事件并送达，确保 sequencer/补读断点已建立。
    pipeline
        .submit(session_updated(
            "01J00000000000000000000999",
            SESSIONS[1],
            "seed",
        ))
        .await
        .expect("seed");
    let seeded = wait_for(
        || {
            sink.events()
                .iter()
                .any(|event| event.event_type().as_str() == "session.updated")
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(seeded, "seed 控制事件必须先送达");
    sink.close();
    fail.store(true, Ordering::Release);
    let mut joins = tokio::task::JoinSet::new();
    for index in 0..12 {
        let pipeline = pipeline.clone();
        let event = session_updated(
            &format!("01J{:023}", 920_000 + index),
            SESSIONS[1],
            &format!("fail-{index}"),
        );
        joins.spawn(async move { pipeline.submit(event).await });
    }
    while let Some(result) = joins.join_next().await {
        let _ = result.expect("任务");
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut escalated = false;
    while tokio::time::Instant::now() < deadline {
        let rows = store.reads().audit_log(20).await.unwrap_or_default();
        if rows
            .iter()
            .any(|record| record.action == ESCALATION_AUDIT_ACTION)
        {
            escalated = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(escalated, "连续 3 次补读失败必须写审计");
    let metrics = bridge.metrics();
    assert!(
        metrics.connection_rebuilds >= 1,
        "必须重建事件订阅：{metrics:?}"
    );
    assert!(
        metrics.readback_failures >= 3,
        "必须累计至少 3 次补读失败：{metrics:?}"
    );
    sink.open();

    let _ = bridge.shutdown(Duration::from_secs(1)).await;
    let _ = pipeline.shutdown().await;
    let _ = store.shutdown().await;
    drop(dir);
}

/// DoD-1：消息事件 payload 9 字段语义（`message.delta` 的 message_id/text 不变）。
#[test]
fn message_delta_payload_shape_is_frozen() {
    let payload = EventPayload::MessageDelta(MessageDeltaPayload {
        message_id: MessageId::new("01J00000000000000000000M1").expect("message"),
        text: "hello".to_owned(),
    });
    let value = serde_json::to_value(&payload).expect("序列化");
    assert_eq!(value["message_id"], json!("01J00000000000000000000M1"));
    assert_eq!(value["text"], json!("hello"));
    let session = SessionId::new(SESSIONS[0]).expect("session");
    let envelope = EventEnvelope {
        v: EVENT_ENVELOPE_VERSION,
        id: EventId::new("01J00000000000000000000001").expect("event"),
        session_id: session.clone(),
        run_id: None,
        runtime_id: RuntimeId::new("mock").expect("runtime"),
        seq: 1,
        ts: 1,
        payload,
    };
    let encoded = serde_json::to_value(&envelope).expect("信封序列化");
    for field in aether_core::ENVELOPE_FIELDS {
        assert!(encoded.get(field).is_some(), "缺少信封字段 {field}");
    }
    assert_eq!(encoded.as_object().map(|object| object.len()), Some(9));
}
