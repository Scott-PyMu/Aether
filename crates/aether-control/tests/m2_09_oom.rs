//! M2-09 DoD3：1.5GB 压力注入 → 告警 + 限流且不崩溃（与 M2-07 联动）。
//!
//! 与 M2-07 的差异与联动：
//! - M2-07 DoD3 用**固定采样替身**验证巡检状态机（阈值参数化注入、去重、回落）；
//! - 本测试做**真实内存压力**全链路：测试进程（= 核心）真实分配 1.5GiB →
//!   `ResourcePatrol::with_env`（同一巡检器、同一 env 阈值钩子）真实采样当前进程 RSS →
//!   告警事件（`core_rss_alert`）与限流事件（`core_rss_throttle`）经管线落盘 →
//!   delta 合并窗口放宽 → 释放后回落 Normal → 管线仍功能正常（不崩溃）。
//!
//! 环境变量注入（`AETHER_TEST_RSS_*`，M2-07 冻结的钩子）：alert=1024MiB、
//! throttle=1280MiB，均低于 1.5GiB 压力，保证真实越限。
//!
//! 注意：本测试二进制仅 1 个用例（env 进程级、分配 1.5GiB）；释放后 Windows
//! `VirtualFree(MEM_RELEASE)` / Unix `munmap` 归还物理页，回落断言确定。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod m2_support;

use std::time::Duration;

use aether_control::{
    EventPipeline, ResourcePatrol, ResourcePressure, RSS_ALERT_EVENT_CODE, RSS_THROTTLE_EVENT_CODE,
};
use aether_core::SessionId;
use serde_json::json;

use m2_support::{build_manager, create_session, manual_clock, ScriptedExecutor, TestCore};

/// 1.5GiB 压力总量（真实分配 + 逐字节触写，保证 RSS 物理提交）。
const PRESSURE_BYTES: usize = 1536 * 1024 * 1024;
/// 第一步分配：1.1GiB（介于 alert=1GiB 与 throttle=1.25GiB 之间，用于观察「依次触发」）。
const PRESSURE_STEP1_BYTES: usize = 1126 * 1024 * 1024;
/// env 注入：告警阈值（MiB）。
const ENV_ALERT_MIB: &str = "AETHER_TEST_RSS_ALERT_MB";
/// env 注入：限流阈值（MiB）。
const ENV_THROTTLE_MIB: &str = "AETHER_TEST_RSS_THROTTLE_MB";
/// env 注入：巡检周期（ms；本测试手动驱动，仅验证钩子）。
const ENV_INTERVAL_MS: &str = "AETHER_TEST_RSS_INTERVAL_MS";

/// 轮询等待 readback 事件数达到期望（异步轮询，避免 sync 闭包内 await）。
async fn wait_event_count(
    pipeline: &EventPipeline,
    session_id: &SessionId,
    expected: usize,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let count = pipeline
            .readback(session_id, 0)
            .await
            .map(|frame| frame.events.len())
            .unwrap_or(0);
        if count >= expected {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 轮询等待巡检等级（异步轮询；`patrol_once` 为 async）。
async fn wait_pressure_level(
    patrol: &ResourcePatrol,
    pipeline: &EventPipeline,
    expected: ResourcePressure,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let _ = patrol.patrol_once(pipeline).await;
        if pipeline.health().resource_pressure == expected {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_1_5gb_pressure_triggers_alert_throttle_and_no_crash() {
    // 阈值注入（`ResourcePatrolConfig::from_env` 消费；进程级 env，单用例安全）。
    std::env::set_var(ENV_ALERT_MIB, "1024");
    std::env::set_var(ENV_THROTTLE_MIB, "1280");
    std::env::set_var(ENV_INTERVAL_MS, "10");

    let core = TestCore::open().await;
    let manager = build_manager(
        &core,
        manual_clock(1_000),
        ScriptedExecutor::new(0),
        aether_control::LifecycleConfig::default(),
    );
    let session = create_session(&manager, "m2-09-oom").await;
    // create_session 落盘基线事件（session.created + session.status_changed）。
    let base_events = core
        .pipeline
        .readback(&session.id, 0)
        .await
        .unwrap()
        .events
        .len();
    assert!(base_events >= 2, "会话创建事件基线异常: {base_events}");

    // 1) env 阈值钩子生效（与 M2-07 同一 `ResourcePatrol::with_env` 巡检器）。
    let patrol = ResourcePatrol::with_env();
    assert_eq!(
        patrol.config().alert_bytes,
        1024 * 1024 * 1024,
        "env 告警阈值必须生效"
    );
    assert_eq!(
        patrol.config().throttle_bytes,
        1280 * 1024 * 1024,
        "env 限流阈值必须生效"
    );

    // 2) 真实 1.5GiB 压力（分两步增长，观察告警 → 限流依次触发）。
    //    第一步 1.1GiB：介于 alert(1GiB) 与 throttle(1.25GiB) 之间。
    let pressure_started = std::time::Instant::now();
    let mut pressure = vec![0xAB_u8; PRESSURE_STEP1_BYTES];
    pressure[0] = 0xAB;
    pressure[PRESSURE_STEP1_BYTES - 1] = 0xAB;
    println!(
        "[m2-09 DoD3] 第一步分配 {:.2} GiB 耗时 {}ms",
        PRESSURE_STEP1_BYTES as f64 / (1024.0 * 1024.0 * 1024.0),
        pressure_started.elapsed().as_millis()
    );
    // 让 sysinfo 采样观测到真实 RSS（页面提交 + 采样刷新）。
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 3) 告警事件：≥1GiB → Alert + core_rss_alert 落盘（先日志后广播）。
    let change = patrol.patrol_once(&core.pipeline).await;
    assert_eq!(
        change,
        Some(ResourcePressure::Alert),
        "1.1GiB ≥ 1GiB 必须告警"
    );
    let health = core.pipeline.health();
    assert_eq!(health.resource_pressure, ResourcePressure::Alert);
    assert_eq!(health.resource_alert_events, 1);
    assert!(
        wait_event_count(
            &core.pipeline,
            &session.id,
            base_events + 1,
            Duration::from_secs(5)
        )
        .await,
        "core_rss_alert 必须落盘"
    );
    let codes = readback_codes(&core.pipeline, &session.id).await;
    assert!(
        codes.contains(&RSS_ALERT_EVENT_CODE.to_owned()),
        "告警事件码必须在库: {codes:?}"
    );

    // 第二步增长到 1.5GiB：超过 throttle(1.25GiB)。
    pressure.resize(PRESSURE_BYTES, 0xAB);
    pressure[PRESSURE_BYTES - 1] = 0xAB;
    println!(
        "[m2-09 DoD3] 增长至 {:.2} GiB（{} 字节，累计耗时 {}ms）",
        PRESSURE_BYTES as f64 / (1024.0 * 1024.0 * 1024.0),
        PRESSURE_BYTES,
        pressure_started.elapsed().as_millis()
    );
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 4) 限流事件：≥1.25GiB → Throttled + core_rss_throttle 落盘（依次触发）。
    assert_eq!(
        patrol.patrol_once(&core.pipeline).await,
        Some(ResourcePressure::Throttled),
        "1.5GiB ≥ 1.25GiB 必须限流"
    );
    let health = core.pipeline.health();
    assert_eq!(health.resource_pressure, ResourcePressure::Throttled);
    assert_eq!(health.resource_throttle_events, 1);
    assert!(
        wait_event_count(
            &core.pipeline,
            &session.id,
            base_events + 2,
            Duration::from_secs(5)
        )
        .await,
        "core_rss_throttle 必须落盘"
    );
    let codes = readback_codes(&core.pipeline, &session.id).await;
    assert!(
        codes.contains(&RSS_THROTTLE_EVENT_CODE.to_owned()),
        "限流事件码必须在库: {codes:?}"
    );

    // 5) delta 限流生效：Throttled 后提交 delta，256ms 窗口内不落盘。
    let delta = json!({
        "v": aether_core::EVENT_ENVELOPE_VERSION,
        "id": "01J00000000000000000090D01",
        "session_id": session.id.as_str(),
        "run_id": null,
        "runtime_id": "mock",
        "seq": 0,
        "ts": 2,
        "type": "message.delta",
        "payload": { "message_id": "01J00000000000000000090M01", "text": "1.5GB 压力下限流窗口内容" }
    });
    core.pipeline.submit(delta).await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        core.pipeline
            .readback(&session.id, 0)
            .await
            .unwrap()
            .events
            .len(),
        base_events + 2,
        "限流窗口（256ms）内 delta 不得落盘（仅有会话基线 + alert + throttle 两条 error）"
    );
    assert!(
        wait_event_count(
            &core.pipeline,
            &session.id,
            base_events + 3,
            Duration::from_secs(3)
        )
        .await,
        "限流窗口到期必须冲刷 delta"
    );

    // 6) 不崩溃：压力存在期间管线仍可提交并落盘控制事件。
    let log = json!({
        "v": aether_core::EVENT_ENVELOPE_VERSION,
        "id": "01J00000000000000000090L01",
        "session_id": session.id.as_str(),
        "run_id": null,
        "runtime_id": "mock",
        "seq": 0,
        "ts": 3,
        "type": "log",
        "payload": { "level": "info", "message": "1.5GB 压力下控制事件仍可落盘" }
    });
    core.pipeline.submit(log).await.unwrap();
    assert!(
        wait_event_count(
            &core.pipeline,
            &session.id,
            base_events + 4,
            Duration::from_secs(5)
        )
        .await,
        "1.5GiB 压力下管线必须保持功能（不崩溃）"
    );

    // 7) 回落：释放压力后 RSS 归还，巡检回到 Normal（等待分配器/采样器延迟）。
    drop(pressure);
    println!("[m2-09 DoD3] 已释放 1.5GiB；等待 RSS 回落");
    let recovered = wait_pressure_level(
        &patrol,
        &core.pipeline,
        ResourcePressure::Normal,
        Duration::from_secs(15),
    )
    .await;
    if !recovered {
        eprintln!(
            "[m2-09 DoD3] RSS 未回落（分配器可能保留物理页）；当前巡检等级={:?}",
            core.pipeline.health().resource_pressure
        );
    }
    assert!(
        recovered,
        "释放 1.5GiB 后巡检必须回到 Normal（Windows VirtualFree / Unix munmap 归还物理页）"
    );
    assert_eq!(
        core.pipeline.health().resource_pressure,
        ResourcePressure::Normal
    );

    // 8) 回落后再验证管线完整可用（终局不崩溃证据）。
    let final_log = json!({
        "v": aether_core::EVENT_ENVELOPE_VERSION,
        "id": "01J00000000000000000090L02",
        "session_id": session.id.as_str(),
        "run_id": null,
        "runtime_id": "mock",
        "seq": 0,
        "ts": 4,
        "type": "log",
        "payload": { "level": "info", "message": "回落后续写正常" }
    });
    core.pipeline.submit(final_log).await.unwrap();
    assert!(
        wait_event_count(
            &core.pipeline,
            &session.id,
            base_events + 5,
            Duration::from_secs(5)
        )
        .await,
        "回落后的控制事件必须落盘"
    );

    let snapshot = patrol.snapshot();
    println!(
        "[m2-09 DoD3] 真实 1.5GiB 压力：alert_events={} throttle_events={} samples={}；\
         最终压力等级={}；管线存活且续写正常",
        snapshot.alert_events,
        snapshot.throttle_events,
        snapshot.samples,
        core.pipeline.health().resource_pressure.code(),
    );
    core.pipeline.shutdown().await.unwrap();
    core.storage.shutdown().await.unwrap();
}

/// 事件错误码收集（补读回读）。
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
