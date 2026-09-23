//! M3-01：`aether://event` 事件桥集成测试（DoD4：慢消费不触发全局背压）。
//!
//! 覆盖：
//! 1. 落盘事件按 `seq` 顺序转发；`AetherEvent`（T14 绑定类型）与核心
//!    `EventEnvelope` 的 JSON 形状一一对应（序列化对照断言）；
//! 2. 慢消费注入：桥接消费者阻塞/龟速时，提交与 `health`/`readback` 不被反压
//!    （D8：下游消费者永不反压 reader；`Lagged` 仅计数，UI 侧按 `seq` 补读）；
//! 3. 生产接线 `spawn_app`（Tauri `emit` 出口）冒烟：mock 应用下正常转发。

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_control::StorageState;
use aether_core::SessionId;
use aether_tauri::bindings::AetherEvent;
use aether_tauri::core_health::{boot_core_health_with_slot, StaticRuntimeSummaries};
use aether_tauri::event_bridge::{forward, spawn_app, tauri_sink, BridgeMetrics, EventSink};
use aether_tauri::json_payload::JsonPayload;
use serde_json::{json, Value};
use tempfile::TempDir;

const SESSION: &str = "01J8ZQ5R0N7W9Y8X6V4T2S0K1M";

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn log_event(index: usize) -> Value {
    json!({
        "v": 1,
        "id": format!("01J{index:023}"),
        "session_id": SESSION,
        "run_id": null,
        "runtime_id": "mock",
        "seq": 0,
        "ts": 1_700_000_000_000i64,
        "type": "log",
        "payload": { "level": "info", "message": format!("bridge-{index}") },
    })
}

fn boot(dir: &TempDir, handle: &tokio::runtime::Handle) -> aether_control::EventPipeline {
    let (backend, _slot) = boot_core_health_with_slot(
        dir.path(),
        handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("启动核心（存储 + 管线）");
    (**backend.pipeline().expect("管线已接线")).clone()
}

fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

/// 记录出口。
#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<AetherEvent>>,
}

impl RecordingSink {
    fn count(&self) -> usize {
        self.events.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    fn events(&self) -> Vec<AetherEvent> {
        self.events
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, event: &AetherEvent) -> Result<(), String> {
        match self.events.lock() {
            Ok(mut guard) => {
                guard.push(event.clone());
                Ok(())
            }
            Err(_) => Err("记录器锁中毒".to_owned()),
        }
    }
}

/// 阻塞出口：首条事件阻塞直到测试放行（确定性慢消费注入）。
struct BlockingSink {
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl BlockingSink {
    fn new() -> Self {
        Self {
            gate: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
        }
    }
}

impl EventSink for BlockingSink {
    fn emit(&self, _event: &AetherEvent) -> Result<(), String> {
        let (lock, condvar) = &*self.gate;
        let mut released = lock.lock().map_err(|_| "门锁中毒".to_owned())?;
        while !*released {
            released = condvar.wait(released).map_err(|_| "门锁中毒".to_owned())?;
        }
        Ok(())
    }
}

fn synthetic_envelope(seq: u64) -> aether_core::EventEnvelope {
    aether_core::EventEnvelope {
        v: aether_core::EVENT_ENVELOPE_VERSION,
        id: aether_core::EventId::new(format!("01J{seq:023}")).expect("事件 id"),
        session_id: SessionId::new(SESSION).expect("会话 id"),
        run_id: None,
        runtime_id: aether_core::RuntimeId::new("mock").expect("运行时 id"),
        seq,
        ts: 1_700_000_000_000,
        payload: aether_core::EventPayload::parse(
            aether_core::EventType::Log,
            json!({ "level": "info", "message": format!("synthetic-{seq}") }),
        )
        .expect("payload 解析"),
    }
}

/// DoD4（基础）：事件按 seq 顺序转发；绑定类型与核心信封 JSON 形状一致。
#[test]
fn bridge_forwards_persisted_events_and_matches_envelope_shape() {
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let pipeline = boot(&dir, &handle);

    let sink = Arc::new(RecordingSink::default());
    let metrics = Arc::new(BridgeMetrics::default());
    let task = runtime.spawn(forward(
        pipeline.subscribe(),
        Arc::clone(&sink) as Arc<dyn EventSink>,
        Arc::clone(&metrics),
    ));

    runtime.block_on(async {
        for index in 1..=3 {
            pipeline.submit(log_event(index)).await.expect("提交事件");
        }
    });

    assert!(
        wait_until(Duration::from_secs(5), || sink.count() == 3),
        "3 条事件必须在 5s 内转发（实际 {}）",
        sink.count()
    );
    let forwarded = sink.events();
    assert!(
        forwarded.windows(2).all(|pair| pair[0].seq < pair[1].seq),
        "转发顺序必须与 seq 一致"
    );

    let session = SessionId::new(SESSION).expect("会话 id");
    let frame = runtime
        .block_on(pipeline.readback(&session, 0))
        .expect("补读");
    assert_eq!(frame.events.len(), 3);
    for (envelope, event) in frame.events.iter().zip(forwarded.iter()) {
        assert_eq!(
            serde_json::to_value(envelope).expect("信封序列化"),
            serde_json::to_value(event).expect("绑定事件序列化"),
            "AetherEvent 与 EventEnvelope 的 JSON 形状必须一一对应"
        );
    }

    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.forwarded, 3);
    assert_eq!(snapshot.failed, 0);
    assert_eq!(snapshot.lagged, 0);
    println!(
        "[m3-01-bridge] forwarded={} lagged={} failed={} envelope-equivalent=true",
        snapshot.forwarded, snapshot.lagged, snapshot.failed
    );
    task.abort();
}

/// DoD4：慢消费注入不反压提交/健康/读取（阻塞出口；放行后零丢失）。
#[test]
fn slow_bridge_consumer_does_not_backpressure_pipeline_or_health() {
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let pipeline = boot(&dir, &handle);

    let sink = Arc::new(BlockingSink::new());
    let gate = Arc::clone(&sink.gate);
    let metrics = Arc::new(BridgeMetrics::default());
    let task = runtime.spawn(forward(
        pipeline.subscribe(),
        Arc::clone(&sink) as Arc<dyn EventSink>,
        Arc::clone(&metrics),
    ));

    // 首条事件被阻塞期间持续提交；逐条测量提交耗时。
    let mut max_submit = Duration::ZERO;
    runtime.block_on(async {
        for index in 1..=100usize {
            let started = Instant::now();
            pipeline.submit(log_event(index)).await.expect("提交事件");
            max_submit = max_submit.max(started.elapsed());
        }
    });
    assert!(
        max_submit < Duration::from_millis(500),
        "慢消费不得反压提交（max={max_submit:?}）"
    );

    // 健康与读取在慢消费（出口阻塞）下仍可响应（口径 ≤2s；实际应为毫秒级）。
    let started = Instant::now();
    let health = pipeline.health();
    let health_latency = started.elapsed();
    assert!(
        health_latency < Duration::from_secs(2),
        "health 响应不得被慢消费阻塞（{health_latency:?}）"
    );
    assert_eq!(health.storage_state, StorageState::Normal);
    assert_eq!(health.persisted_events, 100);

    let session = SessionId::new(SESSION).expect("会话 id");
    let started = Instant::now();
    let frame = runtime
        .block_on(pipeline.readback(&session, 0))
        .expect("补读");
    let readback_latency = started.elapsed();
    assert!(
        readback_latency < Duration::from_secs(2),
        "readback 响应不得被慢消费阻塞"
    );
    assert_eq!(frame.events.len(), 100);
    println!(
        "[m3-01-bridge] slow-sink-blocked max_submit_ms={} health_ms={} readback_ms={} persisted={}",
        max_submit.as_millis(),
        health_latency.as_millis(),
        readback_latency.as_millis(),
        health.persisted_events
    );

    // 放行：桥接继续消费，容量内（100 < broadcast 4096）零丢失。
    {
        let (lock, condvar) = &*gate;
        let mut released = lock.lock().expect("门锁");
        *released = true;
        condvar.notify_all();
    }
    assert!(
        wait_until(Duration::from_secs(10), || {
            let snapshot = metrics.snapshot();
            snapshot.forwarded == 100
        }),
        "放行后必须补齐转发（实际 {:?}）",
        metrics.snapshot()
    );
    assert_eq!(metrics.snapshot().lagged, 0, "容量内不得丢帧");
    task.abort();
}

/// DoD4：广播落后（`Lagged`）只计诊断，转发循环不退出、不反压发送方。
#[test]
fn lagged_broadcast_is_counted_and_forwarding_continues() {
    let runtime = new_runtime();
    let (sender, receiver) = tokio::sync::broadcast::channel::<aether_core::EventEnvelope>(8);
    let sink = Arc::new(BlockingSink::new());
    let gate = Arc::clone(&sink.gate);
    let metrics = Arc::new(BridgeMetrics::default());
    let task = runtime.spawn(forward(
        receiver,
        Arc::clone(&sink) as Arc<dyn EventSink>,
        Arc::clone(&metrics),
    ));

    // 逐条 send 不阻塞（broadcast 语义）；总量 100 > 容量 8 → 必然落后。
    for seq in 1..=100u64 {
        sender.send(synthetic_envelope(seq)).expect("发送事件");
    }
    {
        let (lock, condvar) = &*gate;
        let mut released = lock.lock().expect("门锁");
        *released = true;
        condvar.notify_all();
    }
    assert!(
        wait_until(Duration::from_secs(5), || {
            let snapshot = metrics.snapshot();
            snapshot.lagged > 0
                && snapshot.forwarded >= 1
                && snapshot.forwarded + snapshot.lagged >= 100
        }),
        "落后必须计数且转发继续（实际 {:?}）",
        metrics.snapshot()
    );
    let snapshot = metrics.snapshot();
    assert!(
        snapshot.forwarded + snapshot.lagged >= 100,
        "落后后必须继续消费完剩余事件（forwarded={} lagged={}）",
        snapshot.forwarded,
        snapshot.lagged
    );
    println!(
        "[m3-01-bridge] lagged-sink forwarded={} lagged={} failed={}",
        snapshot.forwarded, snapshot.lagged, snapshot.failed
    );
    task.abort();
}

/// 生产接线冒烟：`spawn_app` 经 Tauri `emit` 出口转发（mock 应用）。
#[test]
fn spawn_app_forwards_via_tauri_event_channel() {
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let pipeline = boot(&dir, &handle);

    let app = tauri::test::mock_builder()
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .expect("构建 mock 应用");
    let metrics = spawn_app(app.handle(), &pipeline);
    runtime.block_on(async {
        pipeline.submit(log_event(1)).await.expect("提交事件");
    });
    assert!(
        wait_until(Duration::from_secs(5), || metrics.snapshot().forwarded >= 1),
        "spawn_app 必须转发事件（实际 {:?}）",
        metrics.snapshot()
    );
    println!(
        "[m3-01-bridge] spawn_app forwarded={} lagged={}",
        metrics.snapshot().forwarded,
        metrics.snapshot().lagged
    );

    // 生产出口的直接冒烟（不经管线）。
    let sink = tauri_sink(app.handle().clone());
    let event = AetherEvent {
        v: 1,
        id: format!("01J{:023}", 9),
        session_id: SESSION.to_owned(),
        run_id: None,
        runtime_id: "mock".to_owned(),
        seq: 9,
        ts: 1_700_000_000_000,
        event_type: "log".to_owned(),
        payload: JsonPayload(json!({ "level": "info", "message": "sink-smoke" })),
    };
    sink.emit(&event).expect("mock 应用 emit 必须成功");
}
