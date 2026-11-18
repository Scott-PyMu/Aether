//! M10-14 / ADR-040 D-040-3 §5：调度器监督/重启、超时/迟到结果、shutdown/app_exit、sidecar false。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_app::core_health::{
    boot_core_full_with_options, CoreBootOptions, SchedulerConfig, SchedulerHooks,
    StaticRuntimeSummaries,
};
use aether_app::shutdown::AppShutdown;
use aether_store::layout::initialize as initialize_layout;
use aether_store::{
    read_clean_shutdown, write_clean_shutdown, StorageLayout, StoreRuntime, WriteQueueConfig,
};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

fn clean_layout(dir: &Path, runtime: &tokio::runtime::Runtime) -> StorageLayout {
    let layout = initialize_layout(dir, "0.1.0", 1).expect("init");
    let storage = StoreRuntime::open_generation(
        layout.clone(),
        WriteQueueConfig::default(),
        runtime.handle(),
    )
    .expect("storage");
    storage.mark_background_check_completed();
    runtime.block_on(storage.shutdown()).expect("shutdown");
    layout
}

fn set_sidecar(layout: &StorageLayout, last_full: Option<i64>) {
    let mut sidecar = read_clean_shutdown(layout).expect("sidecar");
    sidecar.last_full_check_at = last_full;
    sidecar.clean_start_count = 0;
    write_clean_shutdown(layout, &sidecar).expect("sidecar");
}

fn scheduler(clock: Arc<AtomicI64>, periodic_timeout: Duration) -> SchedulerConfig {
    SchedulerConfig {
        enabled: true,
        poll_interval: Duration::from_millis(20),
        periodic_timeout,
        restart_backoff: [
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_millis(20),
        ],
        runtime_concurrency: 2,
        background_delay: Duration::from_secs(3600),
        now: Arc::new(move || clock.load(Ordering::Acquire)),
        ..SchedulerConfig::default()
    }
}

fn boot(
    dir: &Path,
    runtime: &tokio::runtime::Runtime,
    config: SchedulerConfig,
    hooks: SchedulerHooks,
) -> aether_app::core_health::CoreBoot {
    boot_core_full_with_options(
        dir,
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        CoreBootOptions {
            scheduler: config,
            hooks,
            ..CoreBootOptions::default()
        },
    )
    .expect("boot")
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "等待条件超时");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn scheduler_restarts_after_injected_exit() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    let t0 = 1_760_000_000_000i64;
    set_sidecar(&layout, Some(t0));
    let clock = Arc::new(AtomicI64::new(t0));
    let hooks = SchedulerHooks::default();
    hooks.fail_iterations.store(1, Ordering::Release);
    let boot = boot(
        dir.path(),
        &runtime,
        scheduler(Arc::clone(&clock), Duration::from_secs(5)),
        hooks,
    );

    clock.store(t0 + 8 * DAY_MS, Ordering::Release);
    wait_until(Duration::from_secs(5), || {
        let snapshot = boot.storage_check.snapshot();
        snapshot.state == "ok" && snapshot.mode == "background" && snapshot.full_coverage
    });
    let snapshot = boot.storage_check.snapshot();
    assert_eq!(
        snapshot.state, "ok",
        "监督重启后周期检查必须完成：{snapshot:?}"
    );

    let storage = boot.storage.take().expect("storage");
    runtime.block_on(storage.shutdown()).expect("shutdown");
}

#[test]
fn scheduler_supervision_failure_degrades() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    let t0 = 1_760_000_000_000i64;
    set_sidecar(&layout, Some(t0));
    let clock = Arc::new(AtomicI64::new(t0));
    let hooks = SchedulerHooks::default();
    hooks.always_fail.store(true, Ordering::Release);
    let boot = boot(
        dir.path(),
        &runtime,
        scheduler(Arc::clone(&clock), Duration::from_secs(5)),
        hooks,
    );

    clock.store(t0 + 8 * DAY_MS, Ordering::Release);
    wait_until(Duration::from_secs(5), || {
        boot.storage_check
            .snapshot()
            .error
            .as_deref()
            .is_some_and(|error| error.contains("storage_check_scheduler_supervision_failed"))
    });
    let snapshot = boot.storage_check.snapshot();
    assert_eq!(snapshot.state, "failed");
    assert_eq!(
        snapshot.write_gate, "open",
        "后台/调度器失败不关闭已打开写门（由 storage_state 降级停止写入）"
    );
    let storage = boot.storage.take().expect("storage");
    assert!(!storage.background_check_completed());
    runtime.block_on(storage.shutdown()).expect("shutdown");
}

#[test]
fn periodic_timeout_discards_late_results() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    let t0 = 1_760_000_000_000i64;
    set_sidecar(&layout, Some(t0));
    let clock = Arc::new(AtomicI64::new(t0));
    let hooks = SchedulerHooks::default();
    hooks.periodic_delay_ms.store(200, Ordering::Release);
    let runs = Arc::clone(&hooks.periodic_runs);
    let boot = boot(
        dir.path(),
        &runtime,
        scheduler(Arc::clone(&clock), Duration::from_millis(50)),
        hooks,
    );

    clock.store(t0 + 8 * DAY_MS, Ordering::Release);
    wait_until(Duration::from_secs(5), || {
        boot.storage_check
            .snapshot()
            .error
            .as_deref()
            .is_some_and(|error| error == "storage_check_timeout")
    });
    assert_eq!(runs.load(Ordering::Acquire), 1, "超时检查只启动一次");
    assert!(
        !boot.storage_check.check_in_progress(),
        "超时必须释放 in-progress"
    );

    // 迟到结果（阻塞任务 200ms 后完成）不得更新 last_full/flag/health。
    std::thread::sleep(Duration::from_millis(300));
    let snapshot = boot.storage_check.snapshot();
    assert_eq!(snapshot.state, "failed");
    assert_eq!(snapshot.error.as_deref(), Some("storage_check_timeout"));
    let storage = boot.storage.take().expect("storage");
    assert_eq!(
        storage.last_full_check_at(),
        Some(t0),
        "迟到结果不得更新 last_full_check_at"
    );
    assert!(!storage.background_check_completed());
    runtime.block_on(storage.shutdown()).expect("shutdown");
}

#[test]
fn real_panic_is_supervised_and_restarted() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    let t0 = 1_760_000_000_000i64;
    set_sidecar(&layout, Some(t0));
    let clock = Arc::new(AtomicI64::new(t0));
    let mut hooks = SchedulerHooks::default();
    let panicked = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&panicked);
    hooks.panic_hook = Some(Arc::new(move || {
        if !flag.swap(true, Ordering::AcqRel) {
            panic!("M10-14 injected scheduler panic");
        }
    }));
    let boot = boot(
        dir.path(),
        &runtime,
        scheduler(Arc::clone(&clock), Duration::from_secs(5)),
        hooks,
    );

    clock.store(t0 + 8 * DAY_MS, Ordering::Release);
    wait_until(Duration::from_secs(5), || {
        let snapshot = boot.storage_check.snapshot();
        snapshot.state == "ok" && snapshot.mode == "background" && snapshot.full_coverage
    });
    assert!(
        panicked.load(Ordering::Acquire),
        "必须真实触发一次调度器 panic"
    );
    let snapshot = boot.storage_check.snapshot();
    assert_eq!(snapshot.state, "ok", "panic 后监督重启必须完成周期检查");

    let storage = boot.storage.take().expect("storage");
    runtime.block_on(storage.shutdown()).expect("shutdown");
}

#[test]
fn app_exit_cancels_check_and_skips_clean_sidecar() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    let t0 = 1_760_000_000_000i64;
    set_sidecar(&layout, Some(t0));
    let clock = Arc::new(AtomicI64::new(t0));
    let config = SchedulerConfig {
        enabled: false,
        ..scheduler(Arc::clone(&clock), Duration::from_secs(5))
    };
    let boot = boot(dir.path(), &runtime, config, SchedulerHooks::default());

    // 模拟检查进行中（阻塞任务无法取消，epoch/in-progress 由 shutdown 取消）。
    let epoch = boot.storage_check.begin_full_check().expect("begin check");
    assert!(boot.storage_check.check_in_progress());

    aether_store::invalidate_clean_shutdown(&layout).expect("invalidate sidecar");
    let orchestrator = AppShutdown::new(
        None,
        None,
        Some(Arc::clone(&boot.storage)),
        Vec::new(),
        runtime.handle().clone(),
    );
    orchestrator.install_check_grace(Duration::from_millis(50));
    let dummy = runtime.handle().spawn(async {});
    orchestrator.install_storage_check(Arc::clone(&boot.storage_check), dummy);
    let report = runtime.block_on(orchestrator.run());
    assert!(report.storage.is_some(), "存储关闭仍需执行");
    assert!(
        !boot.storage_check.check_in_progress(),
        "grace 超时后必须放弃 in-progress"
    );
    assert!(boot.storage_check.is_cancelled());
    assert!(
        read_clean_shutdown(&layout).is_none(),
        "检查未收口不得写 clean sidecar"
    );
    boot.storage_check.finish_full_check(epoch);
}

#[test]
fn shutdown_after_check_completed_writes_sidecar_normally() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    let t0 = 1_760_000_000_000i64;
    set_sidecar(&layout, Some(t0));
    let clock = Arc::new(AtomicI64::new(t0));
    let config = SchedulerConfig {
        enabled: false,
        ..scheduler(Arc::clone(&clock), Duration::from_secs(5))
    };
    let boot = boot(dir.path(), &runtime, config, SchedulerHooks::default());

    let orchestrator = AppShutdown::new(
        None,
        None,
        Some(Arc::clone(&boot.storage)),
        Vec::new(),
        runtime.handle().clone(),
    );
    orchestrator.install_check_grace(Duration::from_millis(50));
    let dummy = runtime.handle().spawn(async {});
    orchestrator.install_storage_check(Arc::clone(&boot.storage_check), dummy);
    let report = runtime.block_on(orchestrator.run());
    assert!(report.storage.is_some());
    assert!(
        read_clean_shutdown(&layout).is_some(),
        "无进行中检查时仍应正常写 clean sidecar"
    );
}
