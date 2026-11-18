//! M10-14 / ADR-040 D-040-3：运行期 7 天调度、`None` 基线、20 次、维护/导入延后、时钟跳变。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_app::core_health::{
    boot_core_full_with_options, CoreBootOptions, SchedulerConfig, SchedulerHooks,
    StaticRuntimeSummaries,
};
use aether_app::ipc::{MaintenanceCell, MaintenanceState};
use aether_app::storage_check::ImportActivity;
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

fn set_sidecar(layout: &StorageLayout, last_full: Option<i64>, clean_count: u32) {
    let mut sidecar = read_clean_shutdown(layout).expect("sidecar");
    sidecar.last_full_check_at = last_full;
    sidecar.clean_start_count = clean_count;
    write_clean_shutdown(layout, &sidecar).expect("sidecar");
}

fn scheduler(clock: Arc<AtomicI64>) -> SchedulerConfig {
    SchedulerConfig {
        enabled: true,
        poll_interval: Duration::from_millis(20),
        periodic_timeout: Duration::from_secs(10),
        restart_backoff: [
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::from_millis(10),
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
    clock: Arc<AtomicI64>,
    hooks: SchedulerHooks,
) -> aether_app::core_health::CoreBoot {
    boot_core_full_with_options(
        dir,
        runtime.handle(),
        Arc::new(StaticRuntimeSummaries::unwired()),
        CoreBootOptions {
            scheduler: scheduler(clock),
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

fn wait_check_completed(cell: &aether_app::core_health::StorageCheckCell, timeout: Duration) {
    wait_until(timeout, || {
        let snapshot = cell.snapshot();
        snapshot.state == "ok" && snapshot.mode == "background" && snapshot.full_coverage
    });
}

#[test]
fn runtime_scheduler_triggers_on_7_days_and_does_not_repeat() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    let t0 = 1_760_000_000_000i64;
    set_sidecar(&layout, Some(t0), 0);
    let clock = Arc::new(AtomicI64::new(t0));
    let hooks = SchedulerHooks::default();
    let runs = Arc::clone(&hooks.periodic_runs);
    let ticks = Arc::clone(&hooks.scheduler_ticks);
    let boot = boot(dir.path(), &runtime, Arc::clone(&clock), hooks);
    wait_until(Duration::from_secs(2), || ticks.load(Ordering::Acquire) > 0);

    // 未到 7 天：不触发。
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(runs.load(Ordering::Acquire), 0, "7 天前不得触发");

    clock.store(t0 + 7 * DAY_MS + 1000, Ordering::Release);
    wait_check_completed(&boot.storage_check, Duration::from_secs(5));
    assert_eq!(runs.load(Ordering::Acquire), 1, "7 天到期恰好触发一次");
    let snapshot = boot.storage_check.snapshot();
    assert_eq!(snapshot.mode, "background");
    assert_eq!(snapshot.state, "ok");
    assert!(snapshot.full_coverage);

    // 成功后 last_full 更新为当前时钟；继续等待不重复触发。
    let storage = boot.storage.take().expect("storage");
    assert_eq!(storage.clean_start_count(), 0);
    assert!(storage.last_full_check_at().is_some());
    runtime.block_on(storage.shutdown()).expect("shutdown");
    let sidecar = read_clean_shutdown(&layout).expect("sidecar");
    assert_eq!(sidecar.last_full_check_at, Some(t0 + 7 * DAY_MS + 1000));
    assert_eq!(sidecar.clean_start_count, 0);
}

#[test]
fn none_baseline_schedules_after_any_startup_path() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    set_sidecar(&layout, None, 0);
    let clock = Arc::new(AtomicI64::new(1_760_000_000_000));
    let hooks = SchedulerHooks::default();
    let boot = boot(dir.path(), &runtime, Arc::clone(&clock), hooks);

    wait_check_completed(&boot.storage_check, Duration::from_secs(5));
    let storage = boot.storage.take().expect("storage");
    assert!(
        storage.last_full_check_at().is_some(),
        "None 基线成功后必须写入 last_full_check_at"
    );
    runtime.block_on(storage.shutdown()).expect("shutdown");
    let sidecar = read_clean_shutdown(&layout).expect("sidecar");
    assert!(sidecar.last_full_check_at.is_some());
}

#[test]
fn maintenance_window_defers_runtime_check() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    let t0 = 1_760_000_000_000i64;
    set_sidecar(&layout, Some(t0), 0);
    let clock = Arc::new(AtomicI64::new(t0));
    let hooks = SchedulerHooks::default();
    let runs = Arc::clone(&hooks.periodic_runs);
    let ticks = Arc::clone(&hooks.scheduler_ticks);
    let boot = boot(dir.path(), &runtime, Arc::clone(&clock), hooks);
    wait_until(Duration::from_secs(2), || ticks.load(Ordering::Acquire) > 0);

    let maintenance = Arc::new(MaintenanceCell::new());
    maintenance.set(Some(MaintenanceState {
        kind: "backup".to_owned(),
        reason: "evidence".to_owned(),
        started_at: t0,
        progress: None,
    }));
    boot.storage_check
        .install_maintenance(Arc::clone(&maintenance)
            as Arc<dyn aether_app::storage_check::MaintenanceActivitySource>);
    clock.store(t0 + 8 * DAY_MS, Ordering::Release);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(runs.load(Ordering::Acquire), 0, "维护期间必须延后");

    maintenance.set(None);
    wait_check_completed(&boot.storage_check, Duration::from_secs(5));

    let storage = boot.storage.take().expect("storage");
    runtime.block_on(storage.shutdown()).expect("shutdown");
}

#[test]
fn import_window_defers_runtime_check() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    let t0 = 1_760_000_000_000i64;
    set_sidecar(&layout, Some(t0), 0);
    let clock = Arc::new(AtomicI64::new(t0));
    let hooks = SchedulerHooks::default();
    let runs = Arc::clone(&hooks.periodic_runs);
    let ticks = Arc::clone(&hooks.scheduler_ticks);
    let boot = boot(dir.path(), &runtime, Arc::clone(&clock), hooks);
    wait_until(Duration::from_secs(2), || ticks.load(Ordering::Acquire) > 0);

    let import = Arc::new(ImportActivity::new());
    boot.storage_check.install_import_activity(
        Arc::clone(&import) as Arc<dyn aether_app::storage_check::ImportActivitySource>
    );
    import.set(true);
    clock.store(t0 + 8 * DAY_MS, Ordering::Release);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(runs.load(Ordering::Acquire), 0, "导入窗口内必须延后");

    import.clear();
    wait_check_completed(&boot.storage_check, Duration::from_secs(5));

    let storage = boot.storage.take().expect("storage");
    runtime.block_on(storage.shutdown()).expect("shutdown");
}

#[test]
fn clock_forward_jump_is_conservatively_due_once() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    let t0 = 1_760_000_000_000i64;
    set_sidecar(&layout, Some(t0), 0);
    let clock = Arc::new(AtomicI64::new(t0));
    let hooks = SchedulerHooks::default();
    let ticks = Arc::clone(&hooks.scheduler_ticks);
    let boot = boot(dir.path(), &runtime, Arc::clone(&clock), hooks);
    wait_until(Duration::from_secs(2), || ticks.load(Ordering::Acquire) > 0);

    // 前进 >1 天但 <7 天：按时钟跳变保守触发一次。
    clock.store(t0 + DAY_MS + 60_000, Ordering::Release);
    wait_check_completed(&boot.storage_check, Duration::from_secs(5));

    let storage = boot.storage.take().expect("storage");
    runtime.block_on(storage.shutdown()).expect("shutdown");
}

#[test]
fn clock_backward_jump_is_conservatively_due_once() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    let t0 = 1_760_000_000_000i64;
    set_sidecar(&layout, Some(t0), 0);
    let clock = Arc::new(AtomicI64::new(t0));
    let hooks = SchedulerHooks::default();
    let ticks = Arc::clone(&hooks.scheduler_ticks);
    let boot = boot(dir.path(), &runtime, Arc::clone(&clock), hooks);
    wait_until(Duration::from_secs(2), || ticks.load(Ordering::Acquire) > 0);

    clock.store(t0 - 10 * 60 * 1000, Ordering::Release);
    wait_check_completed(&boot.storage_check, Duration::from_secs(5));

    let storage = boot.storage.take().expect("storage");
    runtime.block_on(storage.shutdown()).expect("shutdown");
}

#[test]
fn twenty_clean_starts_triggers_periodic_check() {
    let dir = tempfile::tempdir().expect("temp");
    let runtime = runtime();
    let layout = clean_layout(dir.path(), &runtime);
    let t0 = 1_760_000_000_000i64;
    set_sidecar(&layout, Some(t0), 19);
    let clock = Arc::new(AtomicI64::new(t0));
    let hooks = SchedulerHooks::default();
    let boot = boot(dir.path(), &runtime, Arc::clone(&clock), hooks);

    wait_check_completed(&boot.storage_check, Duration::from_secs(5));
    let storage = boot.storage.take().expect("storage");
    assert_eq!(storage.clean_start_count(), 0, "成功后 20 次计数归零");
    runtime.block_on(storage.shutdown()).expect("shutdown");
    let sidecar = read_clean_shutdown(&layout).expect("sidecar");
    assert_eq!(sidecar.clean_start_count, 0);
}
