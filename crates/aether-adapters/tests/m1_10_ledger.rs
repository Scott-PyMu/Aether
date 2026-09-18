//! M1-10 DoD④ 集成：PID 台账三条件（真实进程夹具）。
//!
//! 覆盖：
//! - 三条件全命中（存活 + 启动时间一致 + `launch_token` 命中）→ 整树回收 + 记录移除；
//! - 存活但启动时间不一致 → 不杀，仅记录（记录保留）；
//! - 存活但命令行不含 `launch_token` → 不杀，仅记录（记录保留）；
//! - PID 已不存在 → 移除陈旧记录且不触发 kill。
//!
//! 运行：`cargo test -p aether-adapters --test m1_10_ledger`（夹具 bin 由 Cargo 提供）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{
    AdapterLedger, AdmissionPolicy, LedgerRecord, LedgerVerdict, ProcessProbe, RuntimeManifest,
    RuntimeSpec, Supervisor, SupervisorConfig, SysinfoProbe, SystemTreeKiller, TreeKiller,
};
use common::{fixture_binary, kill_fixture, pid_alive, unique_temp_dir, RecordingObserver};

const TOKEN: &str = "01JLEDGER00000000000000AB1";

#[derive(Default)]
struct RecordingKiller {
    calls: AtomicU32,
}

impl TreeKiller for RecordingKiller {
    fn kill_tree(&self, _pid: u32) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn probe_until_registered(
    probe: &SysinfoProbe,
    pid: u32,
) -> aether_adapters::supervisor::ProcessFacts {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let facts = probe.facts(pid);
        if facts.alive
            && facts.start_time_epoch.is_some()
            && facts.cmdline.iter().any(|arg| arg.contains(TOKEN))
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

#[test]
fn three_conditions_all_matched_reclaims_real_process() {
    let dir = unique_temp_dir("ledger-reclaim");
    let mut child = common::spawn_fixture(&[
        "--mode",
        "sleep",
        "--seconds",
        "120",
        &format!("--launch-token={TOKEN}"),
    ]);
    let pid = child.id();
    let probe = SysinfoProbe::new();
    let facts = probe_until_registered(&probe, pid);

    let ledger_path = dir.join("adapters.json");
    {
        let mut ledger = AdapterLedger::load(&ledger_path).unwrap();
        ledger
            .record_launch(LedgerRecord {
                adapter_id: "mock".to_owned(),
                pid,
                start_time_epoch: facts.start_time_epoch.unwrap_or_default(),
                launch_token: TOKEN.to_owned(),
                cmdline_hash: aether_adapters::supervisor::cmdline_hash(&facts.cmdline),
            })
            .unwrap();
    }

    let mut ledger = AdapterLedger::load(&ledger_path).unwrap();
    let report = ledger.cleanup(&probe, &SystemTreeKiller).unwrap();
    assert_eq!(report.actions.len(), 1);
    assert_eq!(report.actions[0].verdict, LedgerVerdict::Reclaim);
    assert!(report.actions[0].killed, "三条件全命中必须整树回收");
    assert!(ledger.get("mock").is_none(), "回收后台账记录必须移除");
    // 进程（整树）应在有界时间内消失。
    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_alive(pid), "回收后 PID {pid} 不应存活");
    kill_fixture(&mut child);
}

#[test]
fn alive_process_with_start_time_mismatch_is_never_killed() {
    let dir = unique_temp_dir("ledger-time-mismatch");
    let mut child = common::spawn_fixture(&[
        "--mode",
        "sleep",
        "--seconds",
        "120",
        &format!("--launch-token={TOKEN}"),
    ]);
    let pid = child.id();
    let probe = SysinfoProbe::new();
    let facts = probe_until_registered(&probe, pid);

    let ledger_path = dir.join("adapters.json");
    let killer = RecordingKiller::default();
    {
        let mut ledger = AdapterLedger::load(&ledger_path).unwrap();
        ledger
            .record_launch(LedgerRecord {
                adapter_id: "mock".to_owned(),
                pid,
                // 故意偏移：模拟 PID 复用（OS 报告的启动时间与台账不一致）。
                start_time_epoch: facts.start_time_epoch.unwrap_or_default() + 1_000,
                launch_token: TOKEN.to_owned(),
                cmdline_hash: aether_adapters::supervisor::cmdline_hash(&facts.cmdline),
            })
            .unwrap();
    }

    let mut ledger = AdapterLedger::load(&ledger_path).unwrap();
    let report = ledger.cleanup(&probe, &killer).unwrap();
    assert!(matches!(
        report.actions[0].verdict,
        LedgerVerdict::StartTimeMismatch { .. }
    ));
    assert!(!report.actions[0].killed);
    assert_eq!(killer.calls.load(Ordering::SeqCst), 0, "绝不触发 kill");
    assert!(ledger.get("mock").is_some(), "可疑记录必须保留");
    assert!(pid_alive(pid), "存活进程不得被误杀");
    kill_fixture(&mut child);
}

#[test]
fn alive_process_without_launch_token_in_cmdline_is_never_killed() {
    let dir = unique_temp_dir("ledger-token-mismatch");
    let mut child = common::spawn_fixture(&[
        "--mode",
        "sleep",
        "--seconds",
        "120",
        &format!("--launch-token={TOKEN}"),
    ]);
    let pid = child.id();
    let probe = SysinfoProbe::new();
    let facts = probe_until_registered(&probe, pid);

    let ledger_path = dir.join("adapters.json");
    let killer = RecordingKiller::default();
    let other_token = "01JOTHER00000000000000000";
    {
        let mut ledger = AdapterLedger::load(&ledger_path).unwrap();
        ledger
            .record_launch(LedgerRecord {
                adapter_id: "mock".to_owned(),
                pid,
                start_time_epoch: facts.start_time_epoch.unwrap_or_default(),
                // 台账令牌与命令行中的不一致 → 条件③不满足。
                launch_token: other_token.to_owned(),
                cmdline_hash: aether_adapters::supervisor::cmdline_hash(&facts.cmdline),
            })
            .unwrap();
    }

    let mut ledger = AdapterLedger::load(&ledger_path).unwrap();
    let report = ledger.cleanup(&probe, &killer).unwrap();
    assert_eq!(
        report.actions[0].verdict,
        LedgerVerdict::LaunchTokenMismatch {
            launch_token: other_token.to_owned()
        }
    );
    assert!(!report.actions[0].killed);
    assert_eq!(killer.calls.load(Ordering::SeqCst), 0, "绝不触发 kill");
    assert!(ledger.get("mock").is_some(), "未验证记录必须保留");
    assert!(pid_alive(pid), "存活进程不得被误杀");
    kill_fixture(&mut child);
}

#[test]
fn dead_pid_record_is_removed_without_kill() {
    let dir = unique_temp_dir("ledger-stale");
    let mut child = common::spawn_fixture(&["--mode", "stderr-crash", "--lines", "1"]);
    let pid = child.id();
    let _ = child.wait();

    let ledger_path = dir.join("adapters.json");
    let killer = RecordingKiller::default();
    {
        let mut ledger = AdapterLedger::load(&ledger_path).unwrap();
        ledger
            .record_launch(LedgerRecord {
                adapter_id: "mock".to_owned(),
                pid,
                start_time_epoch: 0,
                launch_token: TOKEN.to_owned(),
                cmdline_hash: "0000000000000000".to_owned(),
            })
            .unwrap();
    }

    let probe = SysinfoProbe::new();
    let mut ledger = AdapterLedger::load(&ledger_path).unwrap();
    let report = ledger.cleanup(&probe, &killer).unwrap();
    assert_eq!(report.actions[0].verdict, LedgerVerdict::Stale);
    assert!(!report.actions[0].killed);
    assert_eq!(killer.calls.load(Ordering::SeqCst), 0);
    assert!(ledger.get("mock").is_none(), "陈旧记录必须移除");
}

#[test]
fn supervisor_cleanup_orphans_reclaims_and_audits() {
    let dir = unique_temp_dir("ledger-supervisor");
    let mut child = common::spawn_fixture(&[
        "--mode",
        "sleep",
        "--seconds",
        "120",
        &format!("--launch-token={TOKEN}"),
    ]);
    let pid = child.id();
    let probe = Arc::new(SysinfoProbe::new());
    let facts = probe_until_registered(probe.as_ref(), pid);

    let ledger_path = dir.join("adapters.json");
    let ledger = Arc::new(tokio::sync::Mutex::new(
        AdapterLedger::load(&ledger_path).unwrap(),
    ));
    {
        let mut guard = ledger.try_lock().unwrap();
        guard
            .record_launch(LedgerRecord {
                adapter_id: "mock".to_owned(),
                pid,
                start_time_epoch: facts.start_time_epoch.unwrap_or_default(),
                launch_token: TOKEN.to_owned(),
                cmdline_hash: aether_adapters::supervisor::cmdline_hash(&facts.cmdline),
            })
            .unwrap();
    }

    let observer = Arc::new(RecordingObserver::new());
    let spec = RuntimeSpec::with_fresh_token(
        RuntimeManifest::new("mock", "Mock", fixture_binary()).official(true),
    );
    let supervisor = Supervisor::new(
        vec![spec],
        SupervisorConfig::d5(),
        AdmissionPolicy::official(),
        observer.clone(),
        Arc::clone(&ledger),
        probe,
        Arc::new(SystemTreeKiller),
    )
    .unwrap();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let report = runtime.block_on(supervisor.cleanup_orphans()).unwrap();
    assert_eq!(report.reclaimed().count(), 1);
    assert_eq!(
        observer.snapshot().audits[0].kind(),
        aether_adapters::supervisor::AuditKind::LedgerReclaimed
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_alive(pid));
    kill_fixture(&mut child);
}
