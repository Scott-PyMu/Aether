//! M1-10 资源告警集成（env 阈值钩子 + 真实内存分配；M4-01「核心 OOM / 输出洪水」复用）。
//!
//! 链路：低阈值（`AETHER_TEST_RSS_THRESHOLD_MB=50`）+ 夹具真实分配 160MiB
//! → 持续 `AETHER_TEST_SUSTAIN_SECS=1` 触发 RSS 告警 → 同一超限区间不重复告警（告警去重/限流）
//! → 夹具释放内存后回落复位 → 再次分配后二次告警（证明复位生效）→ 全程不自动杀进程（D5）。
//!
//! 环境变量仅在本测试进程内显式设置；未设置时生产路径严格等于 D5 默认。
//!
//! 运行：`cargo test -p aether-adapters --test m1_10_resources`
//! （夹具 bin 由 Cargo 提供；跨平台可跑，CI 三平台矩阵同步执行。）

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{
    AdapterLedger, AdmissionPolicy, HeartbeatConfig, ResourceConfig, ResourceLimitKind,
    RuntimeManifest, RuntimeSpec, StartOutcome, Supervisor, SupervisorConfig, SysinfoProbe,
    SysinfoSampler, SystemTreeKiller, TerminationBudget, ENV_CPU_THRESHOLD_PCT,
    ENV_RSS_THRESHOLD_MB, ENV_SUSTAIN_SECS,
};
use common::{fixture_binary, unique_temp_dir, RecordingObserver};

const RSS_THRESHOLD_MB: u64 = 50;
const SUSTAIN_SECS: u64 = 1;
const ALLOC_MB: &str = "160";

fn manifest(args: &[&str]) -> RuntimeManifest {
    RuntimeManifest::new("mock", "Resource Fixture", fixture_binary())
        .official(true)
        .with_args(args.iter().map(|arg| (*arg).to_string()))
}

fn test_config() -> SupervisorConfig {
    SupervisorConfig {
        handshake_timeout: Duration::from_secs(3),
        initialize_timeout: Duration::from_secs(3),
        heartbeat: HeartbeatConfig {
            interval: Duration::from_millis(200),
            timeout: Duration::from_millis(150),
            max_consecutive_failures: 3,
        },
        termination: TerminationBudget {
            shutdown_rpc: Duration::from_millis(300),
            graceful: Duration::from_millis(1_000),
            force: Duration::from_millis(1_000),
            fallback: Duration::from_millis(1_000),
        },
        backoff_override: Some(Duration::from_millis(20)),
    }
}

#[tokio::test]
async fn rss_breach_alerts_dedups_resets_then_realerts_without_kill() {
    // 1) 显式设置测试阈值（本测试二进制独立进程；先于 Supervisor 构造）。
    std::env::set_var(ENV_RSS_THRESHOLD_MB, RSS_THRESHOLD_MB.to_string());
    std::env::set_var(ENV_CPU_THRESHOLD_PCT, "100000");
    std::env::set_var(ENV_SUSTAIN_SECS, SUSTAIN_SECS.to_string());

    let config = ResourceConfig::from_env();
    assert_eq!(
        config.rss_limit_bytes,
        RSS_THRESHOLD_MB * 1024 * 1024,
        "env 覆盖应生效（50MiB）"
    );
    assert_eq!(config.breach_sustain, Duration::from_secs(SUSTAIN_SECS));

    // 2) 启动夹具：160MiB 真实分配 → 3s 后释放 → 再 3s 后重新分配。
    let observer = Arc::new(RecordingObserver::new());
    let dir = unique_temp_dir("m1-10-resources");
    let ledger = Arc::new(tokio::sync::Mutex::new(
        AdapterLedger::load(dir.join("adapters.json")).unwrap(),
    ));
    let spec = RuntimeSpec::with_fresh_token(manifest(&[
        "--mode",
        "deaf",
        "--seconds",
        "30",
        "--mb",
        ALLOC_MB,
        "--release-after-secs",
        "3",
        "--realloc-after-secs",
        "3",
    ]));
    let supervisor = Supervisor::new(
        vec![spec],
        test_config(),
        AdmissionPolicy::official(),
        observer.clone(),
        Arc::clone(&ledger),
        Arc::new(SysinfoProbe::new()),
        Arc::new(SystemTreeKiller),
    )
    .unwrap();

    let outcomes = supervisor.warmup_all().await;
    assert_eq!(outcomes[0].1, StartOutcome::Ready, "夹具应完成预热");
    let runtime = supervisor.get("mock").expect("注册表命中");
    let pid = runtime.current_pid().await.expect("运行中 PID");

    // 3) 告警：低阈值 + 真实 RSS 超限持续 ≥1s。
    let first_deadline = Instant::now() + Duration::from_secs(12);
    while observer.snapshot().alerts.is_empty() {
        assert!(Instant::now() < first_deadline, "等待首次 RSS 告警超时");
        let _ = runtime.sample_resources().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let first = observer.snapshot().alerts[0].clone();
    assert_eq!(first.limit, ResourceLimitKind::Rss);
    assert_eq!(first.pid, pid);
    assert!(
        first.rss_bytes > RSS_THRESHOLD_MB * 1024 * 1024,
        "告警采样 RSS 必须高于低阈值：{}",
        first.rss_bytes
    );
    // D5：仅告警，不自动杀。
    assert!(
        runtime.is_running().await,
        "告警后进程必须仍在运行（D5：不自动杀）"
    );
    assert!(common::pid_alive(pid), "PID {pid} 不应被告警清理");
    println!(
        "[m1-10-resources] 告警#1：rss={}MiB sustained={}ms 阈值={}MiB（limit=rss）；进程存活（D5 不自动杀）",
        first.rss_bytes / (1024 * 1024),
        first.sustained_ms,
        RSS_THRESHOLD_MB
    );

    // 4) 告警去重/限流：仍在超限区间内继续采样，不产生重复告警。
    for _ in 0..3 {
        let _ = runtime.sample_resources().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(
        observer.snapshot().alerts.len(),
        1,
        "同一超限区间只告警一次（告警限流）"
    );
    assert!(runtime.is_running().await);
    println!("[m1-10-resources] 持续超限 3 次采样未重复告警（告警限流）");

    // 5) 回落复位：夹具释放内存，监督器喂入低于阈值的采样。
    let sampler = SysinfoSampler::new();
    let drop_deadline = Instant::now() + Duration::from_secs(15);
    let mut dropped = false;
    let mut low_rss = 0_u64;
    while Instant::now() < drop_deadline {
        let _ = runtime.sample_resources().await;
        let rss = sampler
            .sample(pid)
            .map(|sample| sample.rss_bytes)
            .unwrap_or(0);
        if rss < RSS_THRESHOLD_MB * 1024 * 1024 / 2 {
            dropped = true;
            low_rss = rss;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(dropped, "夹具释放内存后 RSS 应真实回落");
    assert_eq!(observer.snapshot().alerts.len(), 1, "回落过程不产生新告警");
    println!(
        "[m1-10-resources] RSS 回落至 {}MiB（< 阈值/2）→ 监视器复位",
        low_rss / (1024 * 1024)
    );

    // 6) 复位后二次超限：再次分配 → 二次告警（证明 reported 复位生效）。
    let second_deadline = Instant::now() + Duration::from_secs(15);
    while observer.snapshot().alerts.len() < 2 {
        assert!(Instant::now() < second_deadline, "复位后二次 RSS 告警超时");
        let _ = runtime.sample_resources().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let alerts = observer.snapshot().alerts;
    assert_eq!(alerts.len(), 2, "复位后应恰好出现第二次告警");
    assert_eq!(alerts[1].limit, ResourceLimitKind::Rss);
    assert!(alerts[1].rss_bytes > RSS_THRESHOLD_MB * 1024 * 1024);
    assert!(
        runtime.is_running().await,
        "二次告警后进程仍须在运行（D5：不自动杀）"
    );
    println!(
        "[m1-10-resources] 告警#2：rss={}MiB sustained={}ms（复位后二次触发）；进程存活",
        alerts[1].rss_bytes / (1024 * 1024),
        alerts[1].sustained_ms
    );
    println!("[m1-10-resources] 链路证据：告警 → 限流 → 回落复位 → 二次告警 → 全程不杀");

    supervisor.shutdown_all().await;
}
