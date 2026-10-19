//! M7-06 供应商消费映射 spawn 环境注入点集成断言（ADR-024 附录 A-1）。
//!
//! 覆盖 DoD2 的注入链路前半段（本 crate 侧）：
//! - `RuntimeSpec::with_env_source` 提供 `SpawnEnvSource` → 适配器 spawn 时环境变量
//!   一次性进入子进程（夹具经 `--env-report` 回写报告供断言）；
//! - 来源返回空集合 = 不注入（无匹配 provider 路径）；
//! - 注入值可被后续进程读取（真实读取路径由 `aether-tauri` 实现承担，见 m7_06 证据）。
//!
//! 运行：`cargo test -p aether-adapters --test m7_06_provider_env`。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aether_adapters::supervisor::{
    AdapterLedger, AdmissionPolicy, RuntimeManifest, RuntimeSpec, SpawnEnvFuture, SpawnEnvSource,
    StartOutcome, Supervisor, SupervisorConfig, SysinfoProbe, SystemTreeKiller,
};
use common::{fixture_binary, unique_temp_dir, RecordingObserver};

/// 固定注入项来源（探针变量；非密钥）。
struct StaticEnvSource {
    pairs: Vec<(String, String)>,
    calls: std::sync::Mutex<Vec<String>>,
}

impl StaticEnvSource {
    fn new(pairs: Vec<(&str, &str)>) -> Self {
        Self {
            pairs: pairs
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        match self.calls.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl SpawnEnvSource for StaticEnvSource {
    fn spawn_env<'a>(&'a self, runtime_id: &'a str) -> SpawnEnvFuture<'a> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(runtime_id.to_owned());
        }
        let pairs = self.pairs.clone();
        Box::pin(async move { pairs })
    }
}

fn supervisor_for(spec: RuntimeSpec) -> (Supervisor, Arc<RecordingObserver>) {
    let dir = unique_temp_dir("m7-06-provider-env");
    let ledger = Arc::new(tokio::sync::Mutex::new(
        AdapterLedger::load(dir.join("adapters.json")).unwrap(),
    ));
    let observer = Arc::new(RecordingObserver::new());
    let config = SupervisorConfig {
        handshake_timeout: Duration::from_secs(3),
        initialize_timeout: Duration::from_secs(3),
        ..SupervisorConfig::d5()
    };
    let supervisor = Supervisor::new(
        vec![spec],
        config,
        AdmissionPolicy::official(),
        observer.clone(),
        ledger,
        Arc::new(SysinfoProbe::new()),
        Arc::new(SystemTreeKiller),
    )
    .unwrap();
    (supervisor, observer)
}

fn report_of(path: &std::path::Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).expect("env 报告文件");
    serde_json::from_str(text.trim()).expect("env 报告 JSON")
}

/// DoD2①：来源注入的 env 经 spawn 一次性进入子进程，且来源按运行时 id 被询问。
#[tokio::test]
async fn spawn_env_source_injects_into_adapter_process() {
    let dir = unique_temp_dir("m7-06-provider-env-inject");
    let report = dir.join("env-report.json");
    let source = Arc::new(StaticEnvSource::new(vec![(
        "AETHER_M7_06_PROBE",
        "probe-value-m7-06",
    )]));
    let manifest = RuntimeManifest::new("mock", "Mock Fixture", fixture_binary())
        .official(true)
        .with_args([
            "--mode",
            "session",
            "--env-report",
            report.to_str().unwrap(),
        ]);
    let spec = RuntimeSpec::with_fresh_token(manifest).with_env_source(source.clone());
    let (supervisor, _observer) = supervisor_for(spec);

    let outcome = supervisor.get("mock").unwrap().start().await;
    assert_eq!(outcome, StartOutcome::Ready, "注入不得阻断启动：{outcome:?}");

    let report = report_of(&report);
    assert_eq!(report["name"], "AETHER_M7_06_PROBE");
    assert_eq!(report["present"], true, "env 必须进入子进程：{report}");
    assert_eq!(report["value"], "probe-value-m7-06");
    assert_eq!(
        source.calls(),
        vec!["mock".to_owned()],
        "来源必须按运行时 id 调用一次"
    );

    // 清理：终止进程（避免测试退出后孤儿）。
    let _ = supervisor.get("mock").unwrap().disable(None).await;
}

/// DoD2②：来源返回空集合 = 不注入（无匹配 provider 的官方默认路径），启动不受影响。
#[tokio::test]
async fn empty_env_source_leaves_process_env_untouched() {
    let dir = unique_temp_dir("m7-06-provider-env-empty");
    let report = dir.join("env-report.json");
    let source = Arc::new(StaticEnvSource::new(Vec::new()));
    let manifest = RuntimeManifest::new("mock", "Mock Fixture", fixture_binary())
        .official(true)
        .with_args([
            "--mode",
            "session",
            "--env-report",
            report.to_str().unwrap(),
            "--env-name",
            "AETHER_M7_06_ABSENT",
        ]);
    let spec = RuntimeSpec::with_fresh_token(manifest).with_env_source(source);
    let (supervisor, _observer) = supervisor_for(spec);

    let outcome = supervisor.get("mock").unwrap().start().await;
    assert_eq!(outcome, StartOutcome::Ready, "{outcome:?}");
    let report = report_of(&report);
    assert_eq!(report["present"], false, "空来源不得注入：{report}");

    let _ = supervisor.get("mock").unwrap().disable(None).await;
}
