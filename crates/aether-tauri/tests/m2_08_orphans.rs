//! M2-08 DoD1 集成：强杀核心后重启的启动清理（D5 / 评审 #5）。
//!
//! 场景（真实进程，复用 M1-10 故障注入夹具）：
//! 1. 子进程扮演「核心」（`orphan_host_child`）：经真实监督器预热一个 `sleep` 夹具，
//!    台账落盘 `~/.aether/run/adapters.json` 格式（本次为临时目录注入），回报适配器 PID；
//! 2. **强杀核心**（父进程 `Child::kill`：Windows `TerminateProcess` / Unix `SIGKILL`）：
//!    Job Object 未设 kill-on-close（`kill_on_drop(false)`）且 Unix 为独立会话（setsid），
//!    适配器成为孤儿并保持存活；
//! 3. **重启**：走应用启动路径（`boot_supervisor` + `run_supervisor_startup`）执行
//!    启动清理——三条件全命中的孤儿被整树回收；PID 复用诱饵（启动时间不符）与
//!    令牌不符诱饵 **0 误杀**（仅记录，台账记录保留）。
//!
//! 环境：夹具路径经 `AETHER_FIXTURE_BIN` 注入（`scripts/test/m2-08/verify-m2-08.mjs`
//! 构建并设置；`AETHER_REQUIRE_FIXTURE=1` 时缺路径直接失败）。
//!
//! Windows 断言：孤儿存活于 Job Object（非沙箱、无 kill-on-close）→ 启动清理经
//! `taskkill /PID x /T /F` 整树回收；`TerminateJobObject` 优先级断言见 `m2_08_exit`
//! （无响应退出机制序列）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{
    AdapterLedger, LedgerRecord, LedgerVerdict, ProcessProbe, RuntimeManifest, RuntimeSpec,
    SysinfoProbe,
};
use aether_tauri::runtime_control::{boot_supervisor, run_supervisor_startup};
use serde_json::json;
use tempfile::TempDir;

/// 子进程宿主模式环境变量（值为状态目录）。
const HOST_ENV: &str = "AETHER_M2_08_ORPHAN_HOST";
/// 子进程宿主测试名（`--exact` 过滤）。
const HOST_TEST: &str = "orphan_host_child";

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

/// 启动常驻夹具进程（Unix 独立进程组，与生产适配器 `setsid` 模型一致）。
fn spawn_fixture(args: &[&str]) -> std::process::Child {
    let mut command = Command::new(fixture_binary().expect("夹具路径"));
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().expect("启动 aether-adapter-fixture")
}

fn kill_fixture(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 等待 sysinfo 观测到夹具进程（启动时间与命令行可读）。
fn probe_until_registered(
    probe: &SysinfoProbe,
    pid: u32,
    token: &str,
) -> aether_adapters::supervisor::ProcessFacts {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let facts = probe.facts(pid);
        if facts.alive
            && facts.start_time_epoch.is_some()
            && facts.cmdline.iter().any(|arg| arg.contains(token))
        {
            return facts;
        }
        assert!(
            Instant::now() < deadline,
            "夹具进程未被 sysinfo 观测到：{facts:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn ledger_record(
    probe: &SysinfoProbe,
    adapter_id: &str,
    pid: u32,
    launch_token: &str,
) -> LedgerRecord {
    let facts = probe_until_registered(probe, pid, launch_token);
    LedgerRecord {
        adapter_id: adapter_id.to_owned(),
        pid,
        start_time_epoch: facts.start_time_epoch.unwrap_or_default(),
        launch_token: launch_token.to_owned(),
        cmdline_hash: aether_adapters::supervisor::cmdline_hash(&facts.cmdline),
    }
}

/// 子进程宿主：扮演「核心」，预热一个真实适配器后挂起等待被强杀。
///
/// 直接运行（无 `AETHER_M2_08_ORPHAN_HOST`）时立即返回，保证常规 `cargo test` 全绿。
#[test]
fn orphan_host_child() {
    let Ok(state_dir) = std::env::var(HOST_ENV) else {
        return;
    };
    let state_dir = PathBuf::from(state_dir);
    let fixture = fixture_binary().expect("宿主模式需要夹具路径");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let ledger_path = state_dir.join("adapters.json");
    let spec = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Mock Fixture", fixture)
            .official(true)
            // deaf 夹具完成 hello + initialize（进入 Ready）；`--survive-eof` 保证核心
            // 被强杀（stdin 写端关闭）后适配器仍存活为孤儿（真实卡死适配器语义）。
            .with_args(["--mode", "deaf", "--seconds", "300", "--survive-eof"]),
    );
    let supervisor =
        Arc::new(boot_supervisor(vec![spec], Some(&ledger_path)).expect("宿主监督器构造"));
    let startup = run_supervisor_startup(&supervisor, &handle).expect("宿主启动序列");
    assert_eq!(startup.warmups.len(), 1);
    assert_eq!(
        startup.warmups[0].1,
        aether_adapters::supervisor::StartOutcome::Ready,
        "宿主预热必须 Ready（{:?}）",
        startup.warmups[0].1
    );
    let adapter_pid = runtime
        .block_on(async { supervisor.get("mock").unwrap().current_pid().await })
        .expect("宿主适配器 pid");
    let payload = json!({
        "adapter_pid": adapter_pid,
        "ledger_path": ledger_path.to_string_lossy(),
        "host_pid": std::process::id(),
    });
    std::fs::write(state_dir.join("orphan.json"), payload.to_string()).expect("写宿主状态");
    println!("AETHER_M2_08_HOST_READY {payload}");

    // 挂起等待被强杀（模拟核心崩溃；不执行任何清理/终止序列）。
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// DoD1：强杀核心 → 重启（应用启动路径）→ token 命中孤儿被清理；PID 复用诱饵 0 误杀。
#[test]
fn force_kill_core_then_restart_cleanup_is_token_scoped() {
    if std::env::var(HOST_ENV).is_ok() {
        return; // 子进程宿主模式：不递归。
    }
    if fixture_binary().is_none() {
        return;
    }
    let dir = TempDir::new().expect("临时目录");
    let state_dir = dir.path().to_path_buf();
    let ledger_path = state_dir.join("adapters.json");

    // ===== 1. 启动「核心」子进程并等待夹具 Ready =====
    let exe = std::env::current_exe().expect("当前测试二进制");
    let mut host = Command::new(exe)
        .args(["--exact", HOST_TEST, "--nocapture", "--test-threads=1"])
        .env(HOST_ENV, &state_dir)
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("启动宿主子进程");
    let orphan_state = state_dir.join("orphan.json");
    let wait_deadline = Instant::now() + Duration::from_secs(60);
    while !orphan_state.is_file() {
        if Instant::now() >= wait_deadline {
            kill_fixture(&mut host);
            panic!("宿主子进程未在 60s 内就绪（{}）", orphan_state.display());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&orphan_state).expect("读宿主状态"))
            .expect("宿主状态 JSON");
    let orphan_pid =
        u32::try_from(state["adapter_pid"].as_u64().expect("adapter_pid")).expect("pid 范围");
    println!("AETHER_M2_08_DOD1_HOST_READY {}", state);

    // ===== 2. 强杀「核心」（不执行关闭序列）→ 适配器成为孤儿 =====
    host.kill().expect("强杀核心");
    let _ = host.wait();
    assert!(
        pid_alive(orphan_pid),
        "核心被强杀后适配器必须仍存活（Job Object 非沙箱/无 kill-on-close；Unix setsid）"
    );

    // ===== 3. 布置诱饵台账记录：PID 复用（启动时间不符）与令牌不符 =====
    let probe = SysinfoProbe::new();
    let mut reuse_decoy = spawn_fixture(&[
        "--mode",
        "sleep",
        "--seconds",
        "300",
        "--launch-token=01JM208REUSE0000000000001",
    ]);
    let reuse_pid = reuse_decoy.id();
    let mut reuse_record = ledger_record(
        &probe,
        "decoy-reuse",
        reuse_pid,
        "01JM208REUSE0000000000001",
    );
    // 启动时间偏移 1000s：模拟 PID 复用（OS 启动时间与台账不一致）→ 绝不 kill。
    reuse_record.start_time_epoch = reuse_record.start_time_epoch.saturating_add(1_000);

    let mut token_decoy = spawn_fixture(&[
        "--mode",
        "sleep",
        "--seconds",
        "300",
        "--launch-token=01JM208TOKEN0000000000001",
    ]);
    let token_pid = token_decoy.id();
    let mut token_record = ledger_record(
        &probe,
        "decoy-token",
        token_pid,
        "01JM208TOKEN0000000000001",
    );
    // 台账令牌与命令行不一致 → 条件③不满足 → 绝不 kill。
    token_record.launch_token = "01JM208OTHER0000000000001".to_owned();

    {
        let mut ledger = AdapterLedger::load(&ledger_path).expect("加载台账");
        assert_eq!(ledger.records().len(), 1, "核心遗留孤儿记录必须存在");
        assert_eq!(ledger.records()[0].pid, orphan_pid);
        ledger.record_launch(reuse_record).expect("写入诱饵 A");
        ledger.record_launch(token_record).expect("记录诱饵 B");
    }

    // ===== 4. 重启：应用启动路径（boot_supervisor + run_supervisor_startup） =====
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let supervisor = Arc::new(boot_supervisor(Vec::new(), Some(&ledger_path)).expect("新监督器"));
    let startup = run_supervisor_startup(&supervisor, &handle).expect("启动序列（含孤儿清理）");
    let report = &startup.cleanup;
    println!(
        "AETHER_M2_08_DOD1_CLEANUP {}",
        json!({
            "actions": report.actions.iter().map(|action| json!({
                "adapter_id": action.adapter_id,
                "pid": action.pid,
                "verdict": action.verdict.as_str(),
                "killed": action.killed,
                "detail": action.detail,
            })).collect::<Vec<_>>(),
            "reclaimed": report.reclaimed().count(),
            "skipped": report.skipped().count(),
        })
    );

    // 4.1 令牌命中的孤儿被整树回收（Windows：`taskkill /T /F`；Unix：`kill -KILL -<pgid>`）。
    let reclaimed = report.reclaimed().collect::<Vec<_>>();
    assert_eq!(reclaimed.len(), 1, "必须且仅回收 1 个孤儿：{report:?}");
    assert_eq!(reclaimed[0].pid, orphan_pid);
    assert_eq!(reclaimed[0].verdict, LedgerVerdict::Reclaim);
    let dead_deadline = Instant::now() + Duration::from_secs(10);
    while pid_alive(orphan_pid) && Instant::now() < dead_deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_alive(orphan_pid), "孤儿 PID {orphan_pid} 必须被回收");

    // 4.2 PID 复用诱饵：绝不 kill（0 误杀），台账记录保留。
    let actions = &report.actions;
    let reuse_action = actions
        .iter()
        .find(|action| action.adapter_id == "decoy-reuse")
        .expect("诱饵 A 处置记录");
    assert!(
        matches!(
            reuse_action.verdict,
            LedgerVerdict::StartTimeMismatch { .. }
        ),
        "启动时间不符必须判 StartTimeMismatch：{reuse_action:?}"
    );
    assert!(!reuse_action.killed, "PID 复用诱饵绝不允许被 kill");
    assert!(pid_alive(reuse_pid), "PID 复用诱饵必须存活（0 误杀）");

    // 4.3 令牌不符诱饵：绝不 kill，台账记录保留。
    let token_action = actions
        .iter()
        .find(|action| action.adapter_id == "decoy-token")
        .expect("诱饵 B 处置记录");
    assert!(
        matches!(
            token_action.verdict,
            LedgerVerdict::LaunchTokenMismatch { .. }
        ),
        "令牌不符必须判 LaunchTokenMismatch：{token_action:?}"
    );
    assert!(!token_action.killed, "令牌不符诱饵绝不允许被 kill");
    assert!(pid_alive(token_pid), "令牌不符诱饵必须存活（0 误杀）");

    // 4.4 台账收口：孤儿记录移除；可疑记录保留（下次启动复核）。
    let ledger = AdapterLedger::load(&ledger_path).expect("重读台账");
    assert!(ledger.get("mock").is_none(), "已回收孤儿记录必须移除");
    assert!(ledger.get("decoy-reuse").is_some(), "可疑记录必须保留");
    assert!(ledger.get("decoy-token").is_some(), "可疑记录必须保留");
    assert_eq!(actions.len(), 3, "处置条数必须与台账预期一一对应");

    println!(
        "AETHER_M2_08_DOD1 {}",
        json!({
            "orphan_pid": orphan_pid,
            "reclaimed": 1,
            "killed_decoys": 0,
            "reuse_decoy_alive": pid_alive(reuse_pid),
            "token_decoy_alive": pid_alive(token_pid),
            "platform": std::env::consts::OS,
        })
    );
    kill_fixture(&mut reuse_decoy);
    kill_fixture(&mut token_decoy);
}
