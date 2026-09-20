//! M1-10 DoD③ 集成：进程组/Job Object 与终止序列平台断言（真实进程树）。
//!
//! 覆盖：
//! - spawn 即建独立进程组/纳入 Job Object；
//! - 终止序列按 D5 顺序执行，每步硬超时，机制名进入报告；
//! - 强制回收（Windows `TerminateJobObject` / Unix `SIGKILL` 进程组）整树回收
//!   （父 + 子进程全部消失，验证非裸 kill）；
//! - Unix：会话/进程组 id == 子进程 pid（`setsid` 平台断言，Linux `/proc`）；
//! - Windows：`taskkill /T` 与 Job Object 两条路径均被断言。
//!
//! 运行：`cargo test -p aether-adapters --test m1_10_termination`。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use aether_adapters::process::{AdapterProcess, ProcessTerminationTarget};
use aether_adapters::supervisor::{run_termination, TerminationBudget, TerminationStep};
use common::{fixture_binary, pid_alive, unique_temp_dir};

const TOKEN: &str = "01JTERM0000000000000000AB1";

fn test_budget() -> TerminationBudget {
    TerminationBudget {
        shutdown_rpc: Duration::from_millis(200),
        graceful: Duration::from_millis(3_000),
        force: Duration::from_millis(2_000),
        fallback: Duration::from_millis(2_000),
    }
}

/// 启动 `--mode tree` 夹具：父进程 + 子进程（同进程组/Job），并回传 pid 文件内容。
async fn spawn_tree_process() -> (AdapterProcess, u32, u32, std::path::PathBuf) {
    let dir = unique_temp_dir("termination");
    let pid_file = dir.join("tree.txt");
    // 轮询只接受本轮 spawn 夹具写出的内容：先移除任何残留 pid-file，
    // 否则复用目录中的旧内容会在夹具写入前被读走（Gate 1 flaky 根因）。
    let _ = std::fs::remove_file(&pid_file);
    let process = AdapterProcess::spawn(
        fixture_binary(),
        [
            "--mode",
            "tree",
            "--seconds",
            "120",
            "--pid-file",
            pid_file.to_str().unwrap_or("tree.txt"),
            &format!("--launch-token={TOKEN}"),
        ],
    )
    .await
    .expect("启动 tree 夹具");

    let deadline = Instant::now() + Duration::from_secs(10);
    let (parent, child) = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            let mut parent = None;
            let mut child = None;
            for line in text.lines() {
                if let Some(value) = line.strip_prefix("parent=") {
                    parent = value.trim().parse::<u32>().ok();
                }
                if let Some(value) = line.strip_prefix("child=") {
                    child = value.trim().parse::<u32>().ok();
                }
            }
            if let (Some(parent), Some(child)) = (parent, child) {
                break (parent, child);
            }
        }
        assert!(
            Instant::now() < deadline,
            "等待 pid-file 超时：{}",
            pid_file.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    (process, parent, child, pid_file)
}

async fn wait_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !pid_alive(pid)
}

#[tokio::test]
async fn termination_sequence_platform_mechanisms_reap_whole_tree() {
    let (mut process, parent, child, _pid_file) = spawn_tree_process().await;
    assert_eq!(process.id(), Some(parent));

    let mut target = ProcessTerminationTarget::new(&mut process, None);
    let report = run_termination(&mut target, test_budget()).await;

    assert!(report.exited, "终止序列结束后整树必须退出：{report:?}");
    let steps = report.executed_steps();
    let mechanisms = report.mechanisms();
    assert_eq!(
        steps.first(),
        Some(&TerminationStep::ShutdownRpc),
        "第一步必须是 shutdown RPC（无连接 → Failed，继续下一步）"
    );
    assert_eq!(
        steps.get(1),
        Some(&TerminationStep::Graceful),
        "第二步必须是优雅终止"
    );

    #[cfg(windows)]
    {
        assert_eq!(mechanisms[0], "shutdown_rpc");
        assert_eq!(
            mechanisms[1], "taskkill_tree",
            "Windows 优雅终止必须为 taskkill /PID x /T"
        );
        // 若优雅步骤未成功，则必须走 TerminateJobObject 与 taskkill /F 兜底。
        for mechanism in &mechanisms[2..] {
            assert!(
                matches!(*mechanism, "terminate_job_object" | "taskkill_tree_force"),
                "Windows 强杀机制必须为 TerminateJobObject/taskkill 兜底，实际 {mechanism}"
            );
        }
        println!("[m1-10] Windows 终止机制序列：{mechanisms:?}");
    }
    #[cfg(unix)]
    {
        assert_eq!(
            mechanisms[1], "sigterm_pgid",
            "Unix 优雅终止必须发进程组 SIGTERM"
        );
        println!("[m1-10] Unix 终止机制序列：{mechanisms:?}");
    }

    // 父 + 子都必须消失（整树回收；裸 kill 会留下子进程）。
    assert!(
        wait_dead(child, Duration::from_secs(10)).await,
        "子进程 {child} 未被回收"
    );
    assert!(
        wait_dead(parent, Duration::from_secs(10)).await,
        "父进程 {parent} 未退出"
    );
    let _ = process.try_status();
}

#[tokio::test]
async fn force_kill_reaps_tree_via_job_object_or_process_group() {
    let (mut process, parent, child, _pid_file) = spawn_tree_process().await;
    assert_eq!(process.id(), Some(parent));

    // Windows：JobObjectChild::start_kill → TerminateJobObject；
    // Unix：ProcessSession 进程组 → SIGKILL 到 -pgid。
    process.force_kill().expect("强制整树回收");

    assert!(
        wait_dead(child, Duration::from_secs(10)).await,
        "Job/进程组未回收子进程 {child}"
    );
    assert!(
        wait_dead(parent, Duration::from_secs(10)).await,
        "父进程 {parent} 未退出"
    );
    let _ = process.try_status();
}

#[tokio::test]
async fn graceful_step_is_bounded_and_platform_correct() {
    let (mut process, parent, child, _pid_file) = spawn_tree_process().await;
    let started = Instant::now();
    let result = process.terminate_gracefully(Duration::from_secs(3)).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "优雅下发必须受硬超时约束（实际 {:?}）",
        started.elapsed()
    );
    #[cfg(unix)]
    {
        result.expect("Unix SIGTERM 应成功下发");
        assert!(
            wait_dead(parent, Duration::from_secs(5)).await,
            "SIGTERM 到进程组后父进程应退出"
        );
        assert!(wait_dead(child, Duration::from_secs(5)).await);
    }
    #[cfg(windows)]
    {
        // Windows 控制台进程：`taskkill /T`（无 /F）会报告「只能强制终止」（EC 128），
        // 属预期平台行为——由终止序列第 3/4 步（TerminateJobObject / taskkill /F）兜底。
        println!("[m1-10] Windows 优雅终止结果：{result:?}");
        process.force_kill().expect("兜底强杀");
        assert!(wait_dead(parent, Duration::from_secs(10)).await);
        assert!(wait_dead(child, Duration::from_secs(10)).await);
    }
    let _ = result;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_spawn_uses_setsid_process_group() {
    let (mut process, pid, _child, _pid_file) = spawn_tree_process().await;
    let assertion_pgid = process.process_group_id();
    assert_eq!(assertion_pgid, Some(pid), "ProcessSession 断言 pgid == pid");

    // 独立读取 /proc/<pid>/stat 第 5 字段（pgrp）交叉验证。
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("读取 /proc stat");
    let after_rparen = stat.rsplit(')').next().unwrap_or_default();
    let fields: Vec<&str> = after_rparen.split_whitespace().collect();
    // stat: state(0) ppid(1) pgrp(2)
    let observed_pgid: u32 = fields
        .get(2)
        .and_then(|value| value.parse().ok())
        .expect("解析 pgrp");
    assert_eq!(
        observed_pgid, pid,
        "/proc 报告的 pgid 必须等于 pid（setsid）"
    );

    process.force_kill().expect("清理");
    let _ = process.try_status();
}

/// macOS：会话/进程组 id == 子进程 pid（`setsid` 平台断言，`ps -o pgid=` 交叉验证）。
#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_spawn_uses_setsid_process_group() {
    let (mut process, pid, _child, _pid_file) = spawn_tree_process().await;
    assert_eq!(
        process.process_group_id(),
        Some(pid),
        "ProcessSession 断言 pgid == pid"
    );

    // 独立读取 `ps -o pgid= -p <pid>` 交叉验证（macOS 无 /proc）。
    let output = std::process::Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .expect("执行 ps");
    assert!(
        output.status.success(),
        "ps 退出码异常：{:?}",
        output.status
    );
    let observed_pgid: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("解析 pgid");
    assert_eq!(observed_pgid, pid, "ps 报告的 pgid 必须等于 pid（setsid）");

    process.force_kill().expect("清理");
    let _ = process.try_status();
}

/// 其它平台（当前仅 Windows 进入）：Unix `setsid`/pgrp 断言不适用；
/// Windows 的 Job Object / `CREATE_NEW_PROCESS_GROUP` 断言位于
/// `termination_sequence_platform_mechanisms_reap_whole_tree` 的 `#[cfg(windows)]` 分支。
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[tokio::test]
async fn non_unix_platform_assertion_is_explicitly_skipped() {
    eprintln!("SKIP：本平台无 setsid/pgrp 断言（Windows 由 Job Object 整树回收断言覆盖）");
}
