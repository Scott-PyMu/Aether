//! M9-06 DoD-5：`permission.pending` 通知桥（仅提示；轮询/决议路径不变）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_app::event_bridge::{EventFeed, SinkError};
use aether_app::permission_notice::{
    PendingCountSource, PermissionNotice, PermissionNoticeBridge, PermissionNoticeHandle,
    PermissionNoticeSink, PERMISSION_PENDING_EVENT_TYPES,
};
use aether_control::PipelineError;
use aether_core::{
    EventEnvelope, EventId, EventPayload, EventType, PermissionDecision, PermissionRequestId,
    PermissionRequestedPayload, PermissionResolvedPayload, RuntimeId, SessionId,
    SessionUpdatedPayload, ENVELOPE_FIELDS,
};
use tokio::sync::broadcast;

const SESSION: &str = "01J00000000000000000000S06";
const REQUEST: &str = "01J00000000000000000000P06";

struct MockFeed {
    sender: broadcast::Sender<EventEnvelope>,
}

impl MockFeed {
    fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    fn publish(&self, event: EventEnvelope) {
        let _ = self.sender.send(event);
    }
}

impl EventFeed for MockFeed {
    fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.sender.subscribe()
    }

    fn readback<'a>(
        &'a self,
        _session_id: &'a SessionId,
        _last_seq: u64,
    ) -> aether_app::event_bridge::ReadbackFuture<'a> {
        Box::pin(async { Err(PipelineError::PipelineClosed) })
    }
}

#[derive(Default)]
struct FakeCount {
    value: AtomicUsize,
    fail: AtomicBool,
}

impl FakeCount {
    fn set(&self, count: usize) {
        self.value.store(count, Ordering::SeqCst);
    }
}

impl PendingCountSource for FakeCount {
    fn pending_count(&self) -> Result<usize, String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("injected count failure".to_owned());
        }
        Ok(self.value.load(Ordering::SeqCst))
    }
}

struct RecordingSink {
    notices: Mutex<Vec<PermissionNotice>>,
    fail: AtomicBool,
}

impl RecordingSink {
    fn new() -> Self {
        Self {
            notices: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
        }
    }

    fn snapshot(&self) -> Vec<PermissionNotice> {
        self.notices.lock().unwrap().clone()
    }
}

impl PermissionNoticeSink for RecordingSink {
    fn send(&self, notice: &PermissionNotice) -> Result<(), SinkError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(SinkError::new("injected sink failure"));
        }
        self.notices.lock().unwrap().push(notice.clone());
        Ok(())
    }
}

fn session() -> SessionId {
    SessionId::new(SESSION).expect("session id")
}

fn runtime_id() -> RuntimeId {
    RuntimeId::new("mock").expect("runtime id")
}

fn permission_requested(seq: u64) -> EventEnvelope {
    EventEnvelope {
        v: aether_core::EVENT_ENVELOPE_VERSION,
        id: EventId::new(format!("01J0000000000000000000{seq:04}")).expect("event id"),
        session_id: session(),
        run_id: None,
        runtime_id: runtime_id(),
        seq,
        ts: 1_700_000_000_000 + i64::try_from(seq).unwrap_or(i64::MAX),
        payload: EventPayload::PermissionRequested(PermissionRequestedPayload {
            request_id: PermissionRequestId::new(REQUEST).expect("request id"),
            resource: "fs.write".to_owned(),
            action: "write".to_owned(),
            target: Some("C:\\outside\\notes.md".to_owned()),
        }),
    }
}

fn permission_resolved(seq: u64) -> EventEnvelope {
    EventEnvelope {
        v: aether_core::EVENT_ENVELOPE_VERSION,
        id: EventId::new(format!("01J0000000000000000000{seq:04}")).expect("event id"),
        session_id: session(),
        run_id: None,
        runtime_id: runtime_id(),
        seq,
        ts: 1_700_000_000_000 + i64::try_from(seq).unwrap_or(i64::MAX),
        payload: EventPayload::PermissionResolved(PermissionResolvedPayload {
            request_id: PermissionRequestId::new(REQUEST).expect("request id"),
            decision: PermissionDecision::Deny,
            scope: None,
        }),
    }
}

fn session_updated(seq: u64) -> EventEnvelope {
    EventEnvelope {
        v: aether_core::EVENT_ENVELOPE_VERSION,
        id: EventId::new(format!("01J0000000000000000000{seq:04}")).expect("event id"),
        session_id: session(),
        run_id: None,
        runtime_id: runtime_id(),
        seq,
        ts: 1_700_000_000_000 + i64::try_from(seq).unwrap_or(i64::MAX),
        payload: EventPayload::SessionUpdated(SessionUpdatedPayload {
            session_id: session(),
            changed_fields: vec!["title".to_owned()],
            title: Some("M9-06".to_owned()),
        }),
    }
}

fn bridge(
    capacity: usize,
    count: Arc<FakeCount>,
    sink: Arc<RecordingSink>,
) -> (Arc<MockFeed>, PermissionNoticeHandle) {
    let handle = tokio::runtime::Handle::current();
    let feed = Arc::new(MockFeed::new(capacity));
    let notice = PermissionNoticeBridge::new_with_feed(
        feed.clone() as Arc<dyn EventFeed>,
        count as Arc<dyn PendingCountSource>,
        sink as Arc<dyn PermissionNoticeSink>,
    )
    .spawn(&handle);
    (feed, notice)
}

async fn wait_for(condition: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn tokio_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

#[test]
fn permission_events_trigger_hint_notices_only() {
    let rt = tokio_runtime();
    rt.block_on(async {
        let count = Arc::new(FakeCount::default());
        let sink = Arc::new(RecordingSink::new());
        let (feed, mut notice) = bridge(64, count.clone(), sink.clone());

        count.set(1);
        feed.publish(permission_requested(1));
        assert!(
            wait_for(|| sink.snapshot().len() == 1, Duration::from_secs(2)).await,
            "permission.requested 必须触发通知"
        );
        let notices = sink.snapshot();
        assert_eq!(notices[0].count, 1);
        assert_eq!(notices[0].session_id.as_deref(), Some(SESSION));
        assert_eq!(
            notices[0].to_params().as_object().unwrap().len(),
            2,
            "通知只允许 count + session_id"
        );

        // 非权限事件不触发通知（不改变提示/决议语义）。
        feed.publish(session_updated(2));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(sink.snapshot().len(), 1, "session.updated 不得触发通知");

        count.set(0);
        feed.publish(permission_resolved(3));
        assert!(
            wait_for(|| sink.snapshot().len() == 2, Duration::from_secs(2)).await,
            "permission.resolved 必须触发通知"
        );
        assert_eq!(sink.snapshot()[1].count, 0);

        // 附录 B 类型集合未变：通知只由这两类事件驱动。
        assert_eq!(PERMISSION_PENDING_EVENT_TYPES.len(), 2);
        assert_eq!(ENVELOPE_FIELDS.len(), 9);

        let metrics = notice.metrics();
        assert_eq!(metrics.sent, 2);
        assert_eq!(metrics.count_failures, 0);
        assert!(notice.shutdown(Duration::from_secs(1)).await);
    });
}

#[test]
fn lagged_broadcast_resyncs_with_global_count() {
    let rt = tokio_runtime();
    rt.block_on(async {
        let count = Arc::new(FakeCount::default());
        let sink = Arc::new(RecordingSink::new());
        // 容量 1：连续发布 5 条权限事件，桥必然 Lagged → 立即补发一次全局计数。
        let (feed, mut notice) = bridge(1, count.clone(), sink.clone());
        count.set(2);
        for seq in 1..=5 {
            feed.publish(permission_requested(seq));
        }
        assert!(
            wait_for(
                || sink
                    .snapshot()
                    .iter()
                    .any(|notice| notice.session_id.is_none()),
                Duration::from_secs(2)
            )
            .await,
            "lag 后必须补发全局计数（session_id 省略）"
        );
        assert!(notice.metrics().lagged_events > 0);
        assert!(notice.shutdown(Duration::from_secs(1)).await);
    });
}

#[test]
fn count_and_sink_failures_are_isolated() {
    let rt = tokio_runtime();
    rt.block_on(async {
        let count = Arc::new(FakeCount::default());
        let sink = Arc::new(RecordingSink::new());
        let (feed, mut notice) = bridge(64, count.clone(), sink.clone());

        // 计数失败：不发通知，计入诊断；轮询仍是事实源。
        count.fail.store(true, Ordering::SeqCst);
        feed.publish(permission_requested(1));
        assert!(
            wait_for(
                || notice.metrics().count_failures == 1,
                Duration::from_secs(2)
            )
            .await,
            "计数失败必须计数"
        );
        assert!(sink.snapshot().is_empty());

        // 出口失败：计入诊断，不 panic、不停止桥。
        count.fail.store(false, Ordering::SeqCst);
        count.set(1);
        sink.fail.store(true, Ordering::SeqCst);
        feed.publish(permission_requested(2));
        assert!(
            wait_for(|| notice.metrics().sink_errors == 1, Duration::from_secs(2)).await,
            "出口失败必须计数"
        );
        sink.fail.store(false, Ordering::SeqCst);
        feed.publish(permission_requested(3));
        assert!(
            wait_for(|| notice.metrics().sent == 1, Duration::from_secs(2)).await,
            "出口恢复后继续发通知"
        );
        assert!(notice.shutdown(Duration::from_secs(1)).await);
        // 事件类型集合仍为附录 B 现有值（通知不新增事件类型）。
        for event_type in [
            EventType::PermissionRequested,
            EventType::PermissionResolved,
        ] {
            assert!(PERMISSION_PENDING_EVENT_TYPES.contains(&event_type));
        }
    });
}
