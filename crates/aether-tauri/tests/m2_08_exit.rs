//! M2-08 DoD2 集成：T11 退出可靠性（设计 D2/D5 / 附录 D）。
//!
//! 场景（真实存储 + 真实管线 + 真实监督器 + 无响应适配器夹具）：
//! 1. 按应用启动路径组装核心：`boot_supervisor` → `run_supervisor_startup`（孤儿清理 +
//!    预热 + 心跳监控）→ `boot_core_health_with_slot`（库打开 + 管线）→
//!    [`aether_tauri::shutdown::AppShutdown`]（与 `build_backend` 同构）；
//! 2. 适配器无响应（`--mode deaf`：hello + `initialize` 后不再响应 health/shutdown）；
//! 3. 触发退出：走生产同步桥 `AppShutdown::run_blocking`（Tauri `ExitRequested` 回调
//!    线程路径），测量墙钟：
//!    - **≤10s** 完成「广播 shutdown → 适配器终止段 → 存储侧五步」（T11）；
//!    - 适配器进程无残留（进程快照断言）；
//!    - Windows 机制序列断言 `TerminateJobObject` 优先（`taskkill /T /F` 兜底未触发）；
//!    - 存储五步顺序与 D2 一致（M2-06 集成）且 `-wal` 归零。
//!
//! 环境：夹具路径经 `AETHER_FIXTURE_BIN` 注入（`AETHER_REQUIRE_FIXTURE=1` 强制）；
//! 运行：`cargo test -p aether-tauri --test m2_08_exit -- --nocapture`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec};
use aether_tauri::core_health::{self, StaticRuntimeSummaries};
use aether_tauri::runtime_control::{boot_supervisor, run_supervisor_startup};
use aether_tauri::shutdown::AppShutdown;
use serde_json::json;
use tempfile::TempDir;

fn fixture_binary() -> Option<PathBuf> {
    match std::env::var_os("AETHER_FIXTURE_BIN") {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            if std::env::var("AETHER_REQUIRE_FIXTURE").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_FIXTURE=1 但 AETHER_FIXTURE_BIN 未设置");
            }
            eprintln!("SKIP：AETHER_FIXTURE_BIN 未设置（运行 pnpm verify:m2-08 后执行）");
            None
        }
    }
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")),
            Err(_) => false,
        }
    }
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

#[test]
fn t11_exit_with_unresponsive_adapter_bounded_no_residual() {
    let Some(fixture) = fixture_binary() else {
        return;
    };
    let dir = TempDir::new().expect("临时目录");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();

    // ===== 1. 与 build_backend 同构的组装（监督器 → 启动序列 → 核心健康 → 退出编排） =====
    let ledger_path = dir.path().join("adapters.json");
    let spec = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Mock Fixture", fixture)
            .official(true)
            .with_args(["--mode", "deaf", "--seconds", "300"]),
    );
    let supervisor = Arc::new(boot_supervisor(vec![spec], Some(&ledger_path)).expect("监督器构造"));
    let startup = run_supervisor_startup(&supervisor, &handle).expect("启动序列（清理 + 预热）");
    assert_eq!(startup.warmups.len(), 1);
    let adapter_pid = runtime
        .block_on(async { supervisor.get("mock").unwrap().current_pid().await })
        .expect("无响应适配器 pid");
    assert!(
        pid_alive(adapter_pid),
        "deaf 夹具必须完成握手/initialize 并存活（Ready）"
    );

    let (core, storage_slot) = core_health::boot_core_health_with_slot(
        dir.path(),
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("核心健康源（存储 + 管线）启动");
    let pipeline = core.pipeline().map(|pipeline| (**pipeline).clone());
    let orchestrator = AppShutdown::new(
        pipeline,
        Some(Arc::clone(&supervisor)),
        Some(storage_slot),
        startup.monitors,
        handle.clone(),
    );
    assert_eq!(
        orchestrator.budget(),
        Duration::from_secs(10),
        "T11 预算 = 10s"
    );

    // ===== 2. 触发退出（生产路径：run_blocking，Tauri ExitRequested 回调线程同路径） =====
    let started = Instant::now();
    let report = std::thread::spawn({
        let orchestrator = Arc::clone(&orchestrator);
        move || orchestrator.run_blocking()
    })
    .join()
    .expect("退出线程不得 panic");
    let elapsed = started.elapsed();

    // ===== 3. T11 断言 =====
    assert!(
        elapsed <= Duration::from_secs(10),
        "T11：适配器无响应下退出必须 ≤10s（实测 {elapsed:?}）"
    );
    assert!(
        report.within_budget && !report.deadline_expired,
        "退出序列必须在预算内完成：{report:?}"
    );
    assert!(report.pipeline_shutdown_ok, "广播 shutdown 必须成功");

    let adapter = report.adapters.first().expect("适配器终止报告");
    assert_eq!(adapter.runtime_id, "mock");
    assert!(adapter.exited, "终止序列结束后适配器必须退出：{adapter:?}");
    assert_eq!(
        adapter.steps.first().map(String::as_str),
        Some("shutdown_rpc")
    );

    #[cfg(windows)]
    {
        // Windows：优雅 `taskkill /T`（控制台进程）不生效 → 强制步骤必须为
        // `TerminateJobObject`（首选整树回收），不需要兜底 `taskkill /T /F`。
        assert!(
            adapter
                .mechanisms
                .iter()
                .any(|m| m == "terminate_job_object"),
            "Windows 强杀必须经 TerminateJobObject（优先）：{:?}",
            adapter.mechanisms
        );
        assert!(
            !adapter
                .mechanisms
                .iter()
                .any(|m| m == "taskkill_tree_force"),
            "TerminateJobObject 成功时不得使用 taskkill /T /F 兜底：{:?}",
            adapter.mechanisms
        );
        for mechanism in &adapter.mechanisms {
            assert!(
                matches!(
                    mechanism.as_str(),
                    "shutdown_rpc"
                        | "taskkill_tree"
                        | "terminate_job_object"
                        | "taskkill_tree_force"
                ),
                "Windows 机制必须来自 D5 词典：{mechanism}"
            );
        }
    }
    #[cfg(unix)]
    {
        for mechanism in &adapter.mechanisms {
            assert!(
                matches!(
                    mechanism.as_str(),
                    "shutdown_rpc" | "sigterm_pgid" | "sigkill_pgid" | "sigkill_pgid_retry"
                ),
                "Unix 机制必须来自 D5 词典：{mechanism}"
            );
        }
    }

    // 存储侧五步（M2-06 语义在退出路径复现）。
    let storage = report.storage.as_ref().expect("存储关闭报告");
    assert!(storage.d2_order, "存储五步顺序必须与 D2 一致：{storage:?}");
    assert!(storage.drained, "写队列必须在 drain 上限内完成");
    assert_eq!(storage.wal_bytes_after, 0, "退出后 -wal 必须为 0 字节");

    // 进程快照：适配器无残留。
    assert!(
        !pid_alive(adapter_pid),
        "退出后适配器 PID {adapter_pid} 不得残留"
    );

    // ===== 4. 证据行（脚本解析 + 归档） =====
    println!(
        "AETHER_M2_08_T11 {}",
        json!({
            "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "budget_ms": 10_000,
            "within_budget": report.within_budget,
            "deadline_expired": report.deadline_expired,
            "adapter_pid": adapter_pid,
            "adapter_residual": pid_alive(adapter_pid),
            "adapter": adapter,
            "storage": {
                "d2_order": storage.d2_order,
                "drained": storage.drained,
                "wal_bytes_after": storage.wal_bytes_after,
                "duration_ms": storage.duration_ms,
            },
            "platform": std::env::consts::OS,
        })
    );
}
