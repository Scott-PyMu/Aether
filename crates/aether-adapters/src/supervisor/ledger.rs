//! D5 PID 台账（评审修订 #5）：防 PID 复用误杀的启动清理。
//!
//! 台账文件：`~/.aether/run/adapters.json`（D5），每条记录
//! `{adapter_id, pid, start_time_epoch, launch_token, cmdline_hash}`；spawn 时由监督器
//! 注入 `--launch-token=<ULID>` 并登记。
//!
//! 启动清理**必须同时满足三条件**才处置（任一不满足 → 仅记录日志，绝不 kill）：
//! ① pid 存活；② OS 报告的启动时间与台账 `start_time_epoch` 一致；③ 命令行包含
//! 台账 `launch_token`。PID 已不存在时仅移除陈旧记录（不涉及 kill）。
//!
//! 存活/启动时间/命令行的 OS 读取经 [`ProcessProbe`] 抽象：生产用 `sysinfo`
//! （Win `GetProcessTimes`、Linux `/proc/<pid>/stat`、macOS `kinfo_proc` 的等价封装），
//! 单测用固定夹具。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, System, UpdateKind};

/// 台账相对数据目录的路径（D5）。
pub const LEDGER_RELATIVE_PATH: &str = ".aether/run/adapters.json";
/// 台账文件名校验用（防误读其它文件）。
pub const LEDGER_FILE_NAME: &str = "adapters.json";

/// 台账记录（字段与 D5 冻结口径一致，不得增删语义）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerRecord {
    /// 适配器 id（`runtimes.id`）。
    pub adapter_id: String,
    /// 适配器进程 PID。
    pub pid: u32,
    /// OS 报告的进程启动时间（epoch 秒；Windows `GetProcessTimes` 口径）。
    pub start_time_epoch: u64,
    /// spawn 注入的启动令牌（ULID 字符串；同时出现在命令行中）。
    pub launch_token: String,
    /// 命令行哈希（FNV-1a 64，十六进制；完整性诊断用）。
    pub cmdline_hash: String,
}

/// 进程事实（来自 OS 的只读快照）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessFacts {
    /// PID 是否存活（能被 OS 查询到）。
    pub alive: bool,
    /// 进程启动时间（epoch 秒）；不可读时为 `None`。
    pub start_time_epoch: Option<u64>,
    /// 命令行参数（含 argv[0]）。
    pub cmdline: Vec<String>,
}

/// 进程事实读取抽象（生产：[`SysinfoProbe`]；测试：夹具探针）。
pub trait ProcessProbe: Send + Sync {
    fn facts(&self, pid: u32) -> ProcessFacts;
}

/// 基于 `sysinfo` 的跨平台探针。
#[derive(Debug)]
pub struct SysinfoProbe {
    system: Mutex<System>,
    refresh: ProcessRefreshKind,
}

impl SysinfoProbe {
    pub fn new() -> Self {
        Self {
            system: Mutex::new(System::new()),
            refresh: ProcessRefreshKind::new().with_cmd(UpdateKind::Always),
        }
    }
}

impl Default for SysinfoProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessProbe for SysinfoProbe {
    fn facts(&self, pid: u32) -> ProcessFacts {
        let mut system = match self.system.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        system.refresh_processes_specifics(self.refresh);
        match system.process(Pid::from_u32(pid)) {
            Some(process) => ProcessFacts {
                alive: true,
                start_time_epoch: Some(process.start_time()),
                cmdline: process.cmd().to_vec(),
            },
            None => ProcessFacts::default(),
        }
    }
}

/// 进程树回收抽象（Windows `taskkill /T /F` 兜底路径；集成测试断言真实回收）。
pub trait TreeKiller: Send + Sync {
    fn kill_tree(&self, pid: u32) -> Result<(), String>;
}

/// 台账清理判定（三条件的显式结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerVerdict {
    /// 三条件全命中 → 允许整树回收。
    Reclaim,
    /// ① 不满足：PID 已不存在 → 移除陈旧记录（不 kill）。
    Stale,
    /// ② 不满足：启动时间不一致（疑似 PID 复用）→ 仅记录。
    StartTimeMismatch {
        recorded_epoch: u64,
        observed_epoch: Option<u64>,
    },
    /// ③ 不满足：命令行不含 `launch_token` → 仅记录。
    LaunchTokenMismatch { launch_token: String },
}

impl LedgerVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reclaim => "reclaim",
            Self::Stale => "stale",
            Self::StartTimeMismatch { .. } => "start_time_mismatch",
            Self::LaunchTokenMismatch { .. } => "launch_token_mismatch",
        }
    }

    /// 是否放行 kill（仅三条件全命中）。
    pub fn allows_kill(&self) -> bool {
        matches!(self, Self::Reclaim)
    }
}

/// 三条件判定（纯函数）：
/// ① pid 存活；② 启动时间一致；③ 命令行包含 `launch_token`。
pub fn evaluate(record: &LedgerRecord, facts: &ProcessFacts) -> LedgerVerdict {
    if !facts.alive {
        return LedgerVerdict::Stale;
    }
    match facts.start_time_epoch {
        Some(observed) if observed == record.start_time_epoch => {}
        other => {
            return LedgerVerdict::StartTimeMismatch {
                recorded_epoch: record.start_time_epoch,
                observed_epoch: other,
            }
        }
    }
    if !facts
        .cmdline
        .iter()
        .any(|arg| arg.contains(&record.launch_token))
    {
        return LedgerVerdict::LaunchTokenMismatch {
            launch_token: record.launch_token.clone(),
        };
    }
    LedgerVerdict::Reclaim
}

/// 命令行哈希（FNV-1a 64；诊断用，不参与三条件判定）。
pub fn cmdline_hash(cmdline: &[String]) -> String {
    let joined = cmdline.join("\u{1f}");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in joined.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 台账错误。
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("读取台账失败（{path}）：{source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("写入台账失败（{path}）：{source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("台账 JSON 非法（{path}）：{detail}")]
    Parse { path: String, detail: String },
}

/// 启动清理的逐条动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerAction {
    pub adapter_id: String,
    pub pid: u32,
    pub verdict: LedgerVerdict,
    /// 是否实际发起整树回收（仅 `Reclaim` 时为 true）。
    pub killed: bool,
    pub detail: String,
}

/// 清理报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub actions: Vec<LedgerAction>,
}

impl CleanupReport {
    pub fn reclaimed(&self) -> impl Iterator<Item = &LedgerAction> {
        self.actions.iter().filter(|action| action.killed)
    }

    pub fn skipped(&self) -> impl Iterator<Item = &LedgerAction> {
        self.actions
            .iter()
            .filter(|action| !action.killed && !action.verdict.allows_kill())
    }
}

/// 台账文件（内存副本 + 原子落盘）。
#[derive(Debug)]
pub struct AdapterLedger {
    path: PathBuf,
    records: Vec<LedgerRecord>,
}

impl AdapterLedger {
    /// 从磁盘加载（文件不存在视为空台账）。
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, LedgerError> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self {
                path,
                records: Vec::new(),
            });
        }
        let text = std::fs::read_to_string(&path).map_err(|source| LedgerError::Read {
            path: path.display().to_string(),
            source,
        })?;
        if text.trim().is_empty() {
            return Ok(Self {
                path,
                records: Vec::new(),
            });
        }
        let records: Vec<LedgerRecord> =
            serde_json::from_str(&text).map_err(|error| LedgerError::Parse {
                path: path.display().to_string(),
                detail: error.to_string(),
            })?;
        Ok(Self { path, records })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn records(&self) -> &[LedgerRecord] {
        &self.records
    }

    pub fn get(&self, adapter_id: &str) -> Option<&LedgerRecord> {
        self.records
            .iter()
            .find(|record| record.adapter_id == adapter_id)
    }

    /// 登记一次 spawn（同 id 覆盖）并立即落盘。
    pub fn record_launch(&mut self, record: LedgerRecord) -> Result<(), LedgerError> {
        self.records
            .retain(|existing| existing.adapter_id != record.adapter_id);
        self.records.push(record);
        self.save()
    }

    /// 移除记录（进程已退出/已回收）并立即落盘。
    pub fn remove(&mut self, adapter_id: &str) -> Result<Option<LedgerRecord>, LedgerError> {
        let mut removed = None;
        self.records.retain(|record| {
            if record.adapter_id == adapter_id {
                removed = Some(record.clone());
                false
            } else {
                true
            }
        });
        if removed.is_some() {
            self.save()?;
        }
        Ok(removed)
    }

    /// 原子落盘（tmp + rename，避免半写台账）。
    pub fn save(&self) -> Result<(), LedgerError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LedgerError::Write {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let json =
            serde_json::to_string_pretty(&self.records).map_err(|error| LedgerError::Parse {
                path: self.path.display().to_string(),
                detail: error.to_string(),
            })?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes()).map_err(|source| LedgerError::Write {
            path: tmp.display().to_string(),
            source,
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|source| LedgerError::Write {
            path: self.path.display().to_string(),
            source,
        })
    }

    /// 启动清理：逐条判定三条件。
    ///
    /// - 三条件全命中 → `killer.kill_tree` 整树回收 + 移除记录；
    /// - PID 已不存在 → 移除陈旧记录；
    /// - 其余 → 保留记录并写明原因（绝不 kill）。
    pub fn cleanup(
        &mut self,
        probe: &dyn ProcessProbe,
        killer: &dyn TreeKiller,
    ) -> Result<CleanupReport, LedgerError> {
        let records = self.records.clone();
        let mut report = CleanupReport::default();
        let mut changed = false;
        for record in &records {
            let facts = probe.facts(record.pid);
            let verdict = evaluate(record, &facts);
            match verdict.clone() {
                LedgerVerdict::Reclaim => {
                    let killed = match killer.kill_tree(record.pid) {
                        Ok(()) => true,
                        Err(detail) => {
                            report.actions.push(LedgerAction {
                                adapter_id: record.adapter_id.clone(),
                                pid: record.pid,
                                verdict: LedgerVerdict::Reclaim,
                                killed: false,
                                detail: format!("整树回收失败：{detail}"),
                            });
                            continue;
                        }
                    };
                    self.records
                        .retain(|existing| existing.adapter_id != record.adapter_id);
                    changed = true;
                    report.actions.push(LedgerAction {
                        adapter_id: record.adapter_id.clone(),
                        pid: record.pid,
                        verdict,
                        killed,
                        detail: "三条件全命中 → 整树回收".to_owned(),
                    });
                }
                LedgerVerdict::Stale => {
                    self.records
                        .retain(|existing| existing.adapter_id != record.adapter_id);
                    changed = true;
                    report.actions.push(LedgerAction {
                        adapter_id: record.adapter_id.clone(),
                        pid: record.pid,
                        verdict,
                        killed: false,
                        detail: "pid 已不存在 → 移除陈旧记录".to_owned(),
                    });
                }
                LedgerVerdict::StartTimeMismatch {
                    recorded_epoch,
                    observed_epoch,
                } => {
                    let detail = format!(
                        "启动时间不一致（台账 {recorded_epoch}，OS {observed_epoch:?}）→ 疑似 PID 复用，仅记录不 kill"
                    );
                    report.actions.push(LedgerAction {
                        adapter_id: record.adapter_id.clone(),
                        pid: record.pid,
                        verdict,
                        killed: false,
                        detail,
                    });
                }
                LedgerVerdict::LaunchTokenMismatch { launch_token } => {
                    let detail = format!(
                        "命令行不含 launch_token={launch_token} → 非本实例台账进程，仅记录不 kill"
                    );
                    report.actions.push(LedgerAction {
                        adapter_id: record.adapter_id.clone(),
                        pid: record.pid,
                        verdict,
                        killed: false,
                        detail,
                    });
                }
            }
        }
        if changed {
            self.save()?;
        }
        Ok(report)
    }
}

/// 默认台账路径：`~/.aether/run/adapters.json`（D5）。
///
/// Windows 取 `USERPROFILE`、Unix 取 `HOME`；两者皆缺失时退回系统临时目录
/// （`<temp>/aether-run/adapters.json`），保证启动清理仍有落脚点。
pub fn default_ledger_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    match home {
        Some(home) => home.join(LEDGER_RELATIVE_PATH),
        None => std::env::temp_dir()
            .join("aether-run")
            .join(LEDGER_FILE_NAME),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn record() -> LedgerRecord {
        LedgerRecord {
            adapter_id: "mock".to_owned(),
            pid: 4242,
            start_time_epoch: 1_700_000_000,
            launch_token: "01JTOKEN0000000000000000AB".to_owned(),
            cmdline_hash: cmdline_hash(&["mock".to_owned(), "serve".to_owned()]),
        }
    }

    fn facts(alive: bool, start: Option<u64>, cmdline: &[&str]) -> ProcessFacts {
        ProcessFacts {
            alive,
            start_time_epoch: start,
            cmdline: cmdline.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    #[test]
    fn three_conditions_all_matched_allows_reclaim() {
        let verdict = evaluate(
            &record(),
            &facts(
                true,
                Some(1_700_000_000),
                &["mock", "--launch-token=01JTOKEN0000000000000000AB"],
            ),
        );
        assert_eq!(verdict, LedgerVerdict::Reclaim);
        assert!(verdict.allows_kill());
    }

    #[test]
    fn dead_pid_is_stale_without_kill() {
        let verdict = evaluate(&record(), &facts(false, None, &[]));
        assert_eq!(verdict, LedgerVerdict::Stale);
        assert!(!verdict.allows_kill());
    }

    #[test]
    fn start_time_mismatch_never_kills() {
        let verdict = evaluate(
            &record(),
            &facts(
                true,
                Some(1_700_000_123),
                &["mock", "--launch-token=01JTOKEN0000000000000000AB"],
            ),
        );
        match verdict {
            LedgerVerdict::StartTimeMismatch {
                recorded_epoch,
                observed_epoch,
            } => {
                assert_eq!(recorded_epoch, 1_700_000_000);
                assert_eq!(observed_epoch, Some(1_700_000_123));
            }
            other => panic!("应为启动时间不一致，实际 {other:?}"),
        }
        assert!(!verdict.allows_kill());
    }

    #[test]
    fn missing_start_time_never_kills() {
        let verdict = evaluate(
            &record(),
            &facts(
                true,
                None,
                &["mock", "--launch-token=01JTOKEN0000000000000000AB"],
            ),
        );
        assert!(matches!(verdict, LedgerVerdict::StartTimeMismatch { .. }));
        assert!(!verdict.allows_kill());
    }

    #[test]
    fn launch_token_mismatch_never_kills() {
        let verdict = evaluate(
            &record(),
            &facts(true, Some(1_700_000_000), &["mock", "serve"]),
        );
        assert_eq!(
            verdict,
            LedgerVerdict::LaunchTokenMismatch {
                launch_token: "01JTOKEN0000000000000000AB".to_owned()
            }
        );
        assert!(!verdict.allows_kill());
    }

    #[test]
    fn cmdline_hash_is_stable_and_sensitive() {
        let first = cmdline_hash(&["mock".to_owned(), "--launch-token=A".to_owned()]);
        let second = cmdline_hash(&["mock".to_owned(), "--launch-token=A".to_owned()]);
        let third = cmdline_hash(&["mock".to_owned(), "--launch-token=B".to_owned()]);
        assert_eq!(first, second);
        assert_ne!(first, third);
        assert_eq!(first.len(), 16);
    }

    #[derive(Default)]
    struct FixtureProbe {
        facts: HashMap<u32, ProcessFacts>,
    }

    impl ProcessProbe for FixtureProbe {
        fn facts(&self, pid: u32) -> ProcessFacts {
            self.facts.get(&pid).cloned().unwrap_or_default()
        }
    }

    #[derive(Default)]
    struct RecordingKiller {
        killed: Mutex<Vec<u32>>,
    }

    impl TreeKiller for RecordingKiller {
        fn kill_tree(&self, pid: u32) -> Result<(), String> {
            match self.killed.lock() {
                Ok(mut killed) => {
                    killed.push(pid);
                    Ok(())
                }
                Err(_) => Err("锁中毒".to_owned()),
            }
        }
    }

    fn temp_ledger(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join("aether-m1-10-tests")
            .join(format!("{tag}-{}.json", std::process::id()))
    }

    #[test]
    fn cleanup_reclaims_only_when_three_conditions_match() {
        let path = temp_ledger("reclaim");
        let mut ledger = AdapterLedger::load(&path).unwrap();
        let mut matching = record();
        matching.adapter_id = "matched".to_owned();
        matching.pid = 100;
        ledger.record_launch(matching).unwrap();
        let mut reused = record();
        reused.adapter_id = "reused".to_owned();
        reused.pid = 200;
        ledger.record_launch(reused).unwrap();
        let mut tokenless = record();
        tokenless.adapter_id = "tokenless".to_owned();
        tokenless.pid = 300;
        ledger.record_launch(tokenless).unwrap();
        let mut dead = record();
        dead.adapter_id = "dead".to_owned();
        dead.pid = 400;
        ledger.record_launch(dead).unwrap();

        let mut probe = FixtureProbe::default();
        probe.facts.insert(
            100,
            facts(
                true,
                Some(1_700_000_000),
                &["mock", "--launch-token=01JTOKEN0000000000000000AB"],
            ),
        );
        probe.facts.insert(
            200,
            facts(
                true,
                Some(1_700_000_999),
                &["mock", "--launch-token=01JTOKEN0000000000000000AB"],
            ),
        );
        probe
            .facts
            .insert(300, facts(true, Some(1_700_000_000), &["mock", "serve"]));
        // 400：不存在（Probe 默认值 alive=false）。
        let killer = RecordingKiller::default();
        let report = ledger.cleanup(&probe, &killer).unwrap();

        assert_eq!(
            killer.killed.lock().unwrap().as_slice(),
            [100],
            "只有三条件全命中的进程可被回收"
        );
        assert_eq!(report.reclaimed().count(), 1);
        let kept: Vec<&str> = ledger
            .records()
            .iter()
            .map(|record| record.adapter_id.as_str())
            .collect();
        assert!(kept.contains(&"reused"));
        assert!(kept.contains(&"tokenless"));
        assert!(!kept.contains(&"matched"));
        assert!(!kept.contains(&"dead"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ledger_round_trips_json() {
        let path = temp_ledger("roundtrip");
        {
            let mut ledger = AdapterLedger::load(&path).unwrap();
            ledger.record_launch(record()).unwrap();
        }
        let loaded = AdapterLedger::load(&path).unwrap();
        assert_eq!(loaded.records().len(), 1);
        assert_eq!(loaded.get("mock"), Some(&record()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ledger_rejects_unknown_fields() {
        let path = temp_ledger("unknown");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            &path,
            r#"[{"adapter_id":"mock","pid":1,"start_time_epoch":0,"launch_token":"t","cmdline_hash":"h","extra":1}]"#,
        );
        let error = AdapterLedger::load(&path).unwrap_err();
        assert!(matches!(error, LedgerError::Parse { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn default_ledger_path_matches_d5_layout() {
        let path = default_ledger_path();
        assert!(path.ends_with("adapters.json"));
        let text = path.to_string_lossy().replace('\\', "/");
        assert!(
            text.contains(".aether/run/adapters.json") || text.contains("aether-run/adapters.json"),
            "台账路径必须落在 .aether/run：{text}"
        );
    }
}
