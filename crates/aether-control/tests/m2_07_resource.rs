//! M2-07 DoD3 集成测试：核心 RSS 巡检（设计 D2 缓解措施：2GB 告警、2.5GB 强制 delta 限流）。
//!
//! 断言口径：
//! - 阈值参数化注入（固定采样替身 + 自定义阈值）→ 告警事件（`core_rss_alert`）与
//!   强制 delta 限流（`core_rss_throttle`）**依次触发**；同一段越限不重复告警；
//! - 回落更新等级并可再次告警；`health().resource_pressure` 同步呈现；
//! - 限流生效：新 delta 缓冲窗口放宽（默认 16ms → 256ms；在途缓冲 deadline 同步放宽）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_control::{
    EventPipeline, PipelineConfig, ResourcePatrol, ResourcePatrolConfig, ResourcePressure,
    RssSampler, RSS_ALERT_EVENT_CODE, RSS_THROTTLE_EVENT_CODE,
};
use aether_core::{SessionId, EVENT_ENVELOPE_VERSION};
use serde_json::json;
use tokio::runtime::Handle;

use m2_support::{
    build_manager, create_session, manual_clock, start_pipeline_with, wait_for, TestCore,
};

/// 固定值 RSS 采样替身（故障注入）。
struct FixedSampler(Mutex<Option<u64>>);

impl FixedSampler {
    fn new(value: Option<u64>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(value)))
    }

    fn set(&self, value: Option<u64>) {
        match self.0.lock() {
            Ok(mut guard) => *guard = value,
            Err(poisoned) => *poisoned.into_inner() = value,
        }
    }
}

impl RssSampler for FixedSampler {
    fn sample_rss_bytes(&self) -> Option<u64> {
        match self.0.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

fn patrol_fixture(sampler: Arc<FixedSampler>) -> ResourcePatrol {
    ResourcePatrol::new(
        ResourcePatrolConfig {
            interval: Duration::from_millis(10),
            alert_bytes: 1_000,
            throttle_bytes: 2_000,
        },
        sampler,
    )
}

/// 事件类型/错误码收集（补读回读）。
async fn readback_codes(pipeline: &EventPipeline, session_id: &SessionId) -> Vec<String> {
    let frame = pipeline.readback(session_id, 0).await.unwrap();
    frame
        .events
        .iter()
        .filter_map(|event| match &event.payload {
            aether_core::EventPayload::Error(info) => Some(info.code.clone()),
            _ => None,
        })
        .collect()
}

/// DoD3：告警事件与 delta 限流依次触发；去重、回落与再告警。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod3_rss_patrol_escalates_alert_then_throttle_with_reset() {
    let core = TestCore::open().await;
    let manager = build_manager(
        &core,
        manual_clock(1_000),
        m2_support::ScriptedExecutor::new(0),
        aether_control::LifecycleConfig::default(),
    );
    // 建立会话与 `default_origin`（告警事件承载会话）。
    let session = create_session(&manager, "rss-patrol").await;

    let sampler = FixedSampler::new(Some(1_500));
    let patrol = patrol_fixture(Arc::clone(&sampler));

    // 1) 1.5KB ≥ alert(1KB) → Alert + core_rss_alert 事件。
    let change = patrol.patrol_once(&core.pipeline).await;
    assert_eq!(change, Some(ResourcePressure::Alert));
    let health = core.pipeline.health();
    assert_eq!(health.resource_pressure, ResourcePressure::Alert);
    assert_eq!(health.resource_alert_events, 1);
    assert_eq!(health.resource_throttle_events, 0);
    assert_eq!(
        readback_codes(&core.pipeline, &session.id).await,
        vec![RSS_ALERT_EVENT_CODE.to_owned()]
    );

    // 2) 同一段越限不重复告警。
    assert_eq!(patrol.patrol_once(&core.pipeline).await, None);
    assert_eq!(core.pipeline.health().resource_alert_events, 1);

    // 3) 3KB ≥ throttle(2KB) → Throttled + core_rss_throttle 事件（依次触发）。
    sampler.set(Some(3_000));
    assert_eq!(
        patrol.patrol_once(&core.pipeline).await,
        Some(ResourcePressure::Throttled)
    );
    let health = core.pipeline.health();
    assert_eq!(health.resource_pressure, ResourcePressure::Throttled);
    assert_eq!(health.resource_throttle_events, 1);
    assert_eq!(
        readback_codes(&core.pipeline, &session.id).await,
        vec![
            RSS_ALERT_EVENT_CODE.to_owned(),
            RSS_THROTTLE_EVENT_CODE.to_owned()
        ]
    );

    // 4) 回落：1500 → Alert（无新事件）；100 → Normal（无新事件）。
    sampler.set(Some(1_500));
    assert_eq!(
        patrol.patrol_once(&core.pipeline).await,
        Some(ResourcePressure::Alert)
    );
    assert_eq!(
        core.pipeline.health().resource_pressure,
        ResourcePressure::Alert
    );
    sampler.set(Some(100));
    assert_eq!(
        patrol.patrol_once(&core.pipeline).await,
        Some(ResourcePressure::Normal)
    );
    assert_eq!(
        core.pipeline.health().resource_pressure,
        ResourcePressure::Normal
    );
    assert_eq!(
        readback_codes(&core.pipeline, &session.id).await.len(),
        2,
        "回落不产生事件"
    );

    // 5) 再次越限 → 重新告警/限流（同一进程内可重复上报）。
    sampler.set(Some(1_500));
    assert_eq!(
        patrol.patrol_once(&core.pipeline).await,
        Some(ResourcePressure::Alert)
    );
    sampler.set(Some(3_000));
    assert_eq!(
        patrol.patrol_once(&core.pipeline).await,
        Some(ResourcePressure::Throttled)
    );
    let health = core.pipeline.health();
    assert_eq!(health.resource_alert_events, 2, "回落后再告警");
    assert_eq!(health.resource_throttle_events, 2);

    // 6) 采样不可用 → None（不改变等级）。
    sampler.set(None);
    assert_eq!(patrol.patrol_once(&core.pipeline).await, None);
    assert_eq!(
        core.pipeline.health().resource_pressure,
        ResourcePressure::Throttled
    );

    let snapshot = patrol.snapshot();
    assert_eq!(snapshot.pressure, ResourcePressure::Throttled);
    assert_eq!(snapshot.alert_events, 2);
    assert_eq!(snapshot.throttle_events, 2);
    assert_eq!(snapshot.samples, 8);

    println!(
        "[m2-07 DoD3] RSS 巡检：alert_events={} throttle_events={} samples={}；\
         事件序列={:?}",
        snapshot.alert_events,
        snapshot.throttle_events,
        snapshot.samples,
        readback_codes(&core.pipeline, &session.id).await
    );
}

/// DoD3 限流生效：Throttled 后新 delta 缓冲窗口放宽（16ms → 256ms）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dod3_throttle_relaxes_delta_flush_window() {
    let dir = tempfile::tempdir().unwrap();
    let config = PipelineConfig {
        persist_retry_delay: Duration::ZERO,
        ..PipelineConfig::default()
    };
    let storage = aether_store::StoreRuntime::open(
        dir.path().join("aether.db"),
        aether_store::WriteQueueConfig::default(),
        &Handle::current(),
    )
    .unwrap();
    let pipeline = start_pipeline_with(&storage, config);
    let mut events = pipeline.subscribe();

    // 先建立一个默认 origin（error 事件承载会话）。
    let session_id = SessionId::new("01J00000000000000000020RSS").unwrap();
    pipeline
        .submit(json!({
            "v": EVENT_ENVELOPE_VERSION,
            "id": "01J00000000000000000020R01",
            "session_id": session_id.as_str(),
            "run_id": null,
            "runtime_id": "mock",
            "seq": 0,
            "ts": 1,
            "type": "log",
            "payload": { "level": "info", "message": "rss patrol origin" }
        }))
        .await
        .unwrap();

    let sampler = FixedSampler::new(Some(3_000));
    let patrol = patrol_fixture(Arc::clone(&sampler));
    assert_eq!(
        patrol.patrol_once(&pipeline).await,
        Some(ResourcePressure::Throttled)
    );
    assert_eq!(pipeline.resource_pressure(), ResourcePressure::Throttled);

    // 限流后提交 delta：256ms 窗口内不得落盘（默认 16ms 会立即冲刷）。
    let delta = json!({
        "v": EVENT_ENVELOPE_VERSION,
        "id": "01J00000000000000000020D01",
        "session_id": session_id.as_str(),
        "run_id": null,
        "runtime_id": "mock",
        "seq": 0,
        "ts": 2,
        "type": "message.delta",
        "payload": { "message_id": "01J00000000000000000020M01", "text": "限流窗口内容" }
    });
    pipeline.submit(delta).await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        pipeline
            .readback(&session_id, 0)
            .await
            .unwrap()
            .events
            .len(),
        2,
        "限流窗口（256ms）内 delta 不得落盘（已有：origin log + throttle error）"
    );

    // 窗口到期后必须落盘（且收到已落盘广播）。
    assert!(
        wait_for(
            || {
                // 同步读取补读不可在闭包内 await；用健康计数代替。
                pipeline.health().delta_persisted_events >= 1
            },
            Duration::from_secs(2)
        )
        .await,
        "限流窗口到期必须冲刷 delta"
    );
    let frame = pipeline.readback(&session_id, 0).await.unwrap();
    assert_eq!(frame.events.len(), 3, "delta 合并后落盘（+1）");
    let mut broadcast_count = 0usize;
    while let Ok(envelope) = events.try_recv() {
        if envelope.event_type() == aether_core::EventType::MessageDelta {
            broadcast_count += 1;
        }
    }
    assert_eq!(broadcast_count, 1, "delta 落盘后广播一次（先日志后广播）");

    println!(
        "[m2-07 DoD3+] RSS 限流：resource_pressure={} delta 窗口放宽；120ms 未落盘、\
         窗口到期后落盘并广播（delta_persisted_events={}）",
        pipeline.health().resource_pressure.code(),
        pipeline.health().delta_persisted_events
    );
}
