//! M7-06/ADR-024 DoD3：三运行时「配置供应商 → run 真实使用该 base URL/模型」证据
//! （opt-in 真实冒烟；缺凭证时由 `scripts/test/m7-06/verify-m7-06.mjs` 打印显式 SKIP）。
//!
//! 链路（与生产同口径）：临时库登记供应商（`base_url` = 本地记录型代理；`api_key_ref`
//! → 内存安全存储中的**测试密钥**）→ `ProviderEnvHook` 在 spawn 时注入 env →
//! 适配器物化消费（Claude env / Codex `config.toml` / DSH provider 分层配置）→
//! 真实运行时执行 run（代理转发到真实上游，认证重写为上游 token）→
//! 断言：① 代理收到请求；② 运行时携带的凭据 == 核心注入的测试密钥（key_match）；
//! ③ 请求体模型 == 会话模型；④ run 终态 completed 且助手文本命中标记。
//!
//! 运行（由 verify 脚本编排；单独运行示例）：
//!   AETHER_M7_06_REAL=1 \
//!   AETHER_M7_06_CLAUDE_UPSTREAM=... AETHER_M7_06_CLAUDE_TOKEN=... AETHER_M7_06_CLAUDE_MODEL=... \
//!   AETHER_CLAUDE_ADAPTER=<bundle.exe> \
//!   cargo test -p aether-tauri --test m7_06_consumption -- --ignored --nocapture real_claude_code
//!
//! 标记：`#[ignore]`（opt-in）；**禁止静默跳过**——验证脚本缺凭证时必须打印 SKIP 行。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec};
use aether_control::{LifecycleConfig, SessionCreateOptions, SessionManager, SystemClock};
use aether_core::{RunStatus, Runtime, RuntimeId, RuntimeStatus};
use aether_security::{KeychainRef, SecretError, SecretStore, SecretValue, SecurityLevel};
use aether_store::{
    ProviderModelRecord, ProviderRecord, StoreCommand,
};
use aether_tauri::adapter_executor::AdapterRunExecutor;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::provider_env::ProviderEnvHook;
use aether_tauri::runtime_control::{boot_supervisor, run_supervisor_startup};
use serde_json::Value;
use tempfile::TempDir;

const PROXY_SCRIPT: &str = "scripts/test/m7-06/recording-proxy.mjs";

/// 内存密钥存储（核心注入路径的密钥来源）。
#[derive(Default)]
struct MemorySecretStore {
    map: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl SecretStore for MemorySecretStore {
    fn level(&self) -> SecurityLevel {
        SecurityLevel::OsKeychain
    }

    fn get(&self, reference: &KeychainRef) -> Result<SecretValue, SecretError> {
        self.map
            .lock()
            .unwrap()
            .get(&reference.to_uri())
            .map(|value| SecretValue::new(value.clone()))
            .ok_or_else(|| SecretError::NotFound {
                reference: reference.clone(),
            })
    }

    fn set(&self, reference: &KeychainRef, value: &SecretValue) -> Result<(), SecretError> {
        self.map
            .lock()
            .unwrap()
            .insert(reference.to_uri(), value.expose().to_string());
        Ok(())
    }

    fn delete(&self, reference: &KeychainRef) -> Result<(), SecretError> {
        self.map
            .lock()
            .unwrap()
            .remove(&reference.to_uri())
            .map(|_| ())
            .ok_or_else(|| SecretError::NotFound {
                reference: reference.clone(),
            })
    }
}

/// 记录型代理子进程（stdout 首行 JSON 就绪）。
struct Proxy {
    child: Child,
    port: u16,
    log_path: PathBuf,
}

impl Proxy {
    fn start(runtime: &str, upstream: &str, token: &str, expect_key: &str, log: PathBuf) -> Self {
        // 代理只转发到**源**（路径前缀由 provider.base_url 保留，运行时请求原样透传）。
        let parsed = url::Url::parse(upstream).expect("上游 URL");
        let origin = match parsed.port() {
            Some(port) => format!("{}://{}:{port}", parsed.scheme(), parsed.host_str().unwrap_or("")),
            None => format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or("")),
        };
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(PROXY_SCRIPT);
        let mut child = Command::new("node")
            .arg(script)
            .args([
                "--listen",
                "0",
                "--runtime",
                runtime,
                "--upstream",
                &origin,
                "--token",
                token,
                "--expect-key",
                expect_key,
                "--log",
            ])
            .arg(&log)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("启动记录型代理（需要 node）");
        let stdout = child.stdout.take().expect("代理 stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("读取代理就绪行");
        let value: Value = serde_json::from_str(line.trim()).expect("代理就绪 JSON");
        assert_eq!(value["ready"], true, "代理未就绪：{line}");
        let port = u16::try_from(value["port"].as_u64().expect("port")).expect("port 范围");
        Self {
            child,
            port,
            log_path: log,
        }
    }

    fn records(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn env_required(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => value,
        _ => panic!("opt-in 真实冒烟缺少环境变量 {name}（禁止静默跳过；由 verify:m7-06 编排）"),
    }
}

fn real_enabled() -> bool {
    std::env::var("AETHER_M7_06_REAL").as_deref() == Ok("1")
}

fn runtime_row(id: &str) -> Runtime {
    Runtime {
        id: RuntimeId::new(id).expect("runtime id"),
        name: id.to_owned(),
        kind: id.to_owned(),
        version: "0.1.0".to_owned(),
        protocol: "1.0".to_owned(),
        capabilities: Vec::new(),
        endpoint: None,
        config: serde_json::json!({}),
        status: RuntimeStatus::Ready,
        status_reason: None,
        last_seen_at: None,
        created_at: 1,
        updated_at: 1,
    }
}

struct Case<'a> {
    runtime_id: &'a str,
    provider_type: &'a str,
    adapter_env: &'a str,
    upstream_env: &'a str,
    token_env: &'a str,
    model_env: &'a str,
    prefix_env: &'a str,
    marker: &'a str,
}

const CASES: [Case<'static>; 3] = [
    Case {
        runtime_id: "claude-code",
        provider_type: "anthropic",
        adapter_env: "AETHER_CLAUDE_ADAPTER",
        upstream_env: "AETHER_M7_06_CLAUDE_UPSTREAM",
        token_env: "AETHER_M7_06_CLAUDE_TOKEN",
        model_env: "AETHER_M7_06_CLAUDE_MODEL",
        prefix_env: "AETHER_M7_06_CLAUDE_PREFIX",
        marker: "AETHER-M7-06-CLAUDE-OK",
    },
    Case {
        runtime_id: "codex",
        provider_type: "openai",
        adapter_env: "AETHER_CODEX_ADAPTER",
        upstream_env: "AETHER_M7_06_CODEX_UPSTREAM",
        token_env: "AETHER_M7_06_CODEX_TOKEN",
        model_env: "AETHER_M7_06_CODEX_MODEL",
        prefix_env: "AETHER_M7_06_CODEX_PREFIX",
        marker: "AETHER-M7-06-CODEX-OK",
    },
    Case {
        runtime_id: "deepseek-harness",
        provider_type: "deepseek",
        adapter_env: "AETHER_DSH_ADAPTER",
        upstream_env: "AETHER_M7_06_DSH_UPSTREAM",
        token_env: "AETHER_M7_06_DSH_TOKEN",
        model_env: "AETHER_M7_06_DSH_MODEL",
        prefix_env: "AETHER_M7_06_DSH_PREFIX",
        marker: "AETHER-M7-06-DSH-OK",
    },
];

/// 单运行时真实消费用例（由具体 `#[test]` 按运行时调用）。
fn run_case(runtime_id: &str) {
    if !real_enabled() {
        eprintln!("SKIP：AETHER_M7_06_REAL 未设置（opt-in 真实冒烟；由 verify:m7-06 编排）");
        return;
    }
    let case = CASES
        .iter()
        .find(|case| case.runtime_id == runtime_id)
        .expect("用例存在");
    let adapter = env_required(case.adapter_env);
    let upstream = env_required(case.upstream_env);
    let token = env_required(case.token_env);
    let model = env_required(case.model_env);
    // 上游 API 的路径前缀（如 Anthropic 网关 `/anth/v1`；OpenAI 兼容网关 `/open/v1`）。
    let prefix = std::env::var(case.prefix_env).unwrap_or_default();
    let prefix = prefix.trim_end_matches('/').to_owned();
    // 注入的测试密钥：形态按运行时惯例（Claude 侧 CLI 对密钥前缀有校验，需 `sk-ant-`）。
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let test_key = match case.provider_type {
        "anthropic" => format!("sk-ant-m7-06-{nonce}"),
        _ => format!("sk-m7-06-{runtime_id}-{nonce}"),
    };

    let dir = TempDir::new().expect("临时目录");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("工作区");
    // 证据目录：默认临时目录；`AETHER_M7_06_EVIDENCE_DIR` 可覆盖为持久目录（调试/归档）。
    let evidence_dir = std::env::var("AETHER_M7_06_EVIDENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dir.path().to_path_buf());
    std::fs::create_dir_all(&evidence_dir).expect("证据目录");
    let proxy_log = evidence_dir.join(format!("proxy-{runtime_id}.jsonl"));
    let proxy = Proxy::start(runtime_id, &upstream, &token, &test_key, proxy_log);

    let runtime_handle = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime_handle.handle().clone();
    let CoreBoot {
        storage,
        reads,
        write,
        pipeline,
        ..
    } = boot_core_full(dir.path(), &handle, Arc::new(StaticRuntimeSummaries::unwired()))
        .expect("启动核心（存储 + 管线）");

    // 供应厂商记录：base_url = 记录型代理；密钥 = 测试密钥（核心唯一注入源）。
    let provider_id = "01J00000000000000000000M71";
    let reference = KeychainRef::new("provider", provider_id).expect("引用");
    let secrets = Arc::new(MemorySecretStore::default());
    secrets
        .set(&reference, &SecretValue::new(test_key.clone()))
        .expect("写入测试密钥");
    let provider = ProviderRecord {
        id: provider_id.to_owned(),
        name: format!("M7-06 {runtime_id}"),
        provider_type: case.provider_type.to_owned(),
        base_url: Some(format!("http://127.0.0.1:{}{prefix}", proxy.port)),
        endpoints: "{}".to_owned(),
        api_key_ref: Some(reference.to_uri()),
        enabled: true,
        is_builtin: false,
        created_at: 1,
        updated_at: 1,
    };
    runtime_handle
        .block_on(write.execute(StoreCommand::InsertProvider { provider }))
        .expect("登记供应商");
    let provider_model = ProviderModelRecord {
        id: "01J00000000000000000000M72".to_owned(),
        provider_id: provider_id.to_owned(),
        model_id: model.clone(),
        display_name: model.clone(),
        enabled: true,
        created_at: 1,
    };
    runtime_handle
        .block_on(write.execute(StoreCommand::InsertProviderModel {
            model: provider_model,
        }))
        .expect("登记模型");

    // 监督器：真实适配器 bundle + 生产同款消费映射 hook。
    let mut args: Vec<String> = vec![
        "--workspace".to_owned(),
        workspace.to_string_lossy().into_owned(),
    ];
    let dsh_bin;
    let dsh_home;
    if runtime_id == "deepseek-harness" {
        dsh_bin = env_required("AETHER_M7_06_DSH_BIN");
        dsh_home = std::env::var("AETHER_M7_06_DSH_HOME")
            .unwrap_or_else(|_| dir.path().join("dsh-home").to_string_lossy().into_owned());
        std::fs::create_dir_all(&dsh_home).expect("DSH home");
        args.extend([
            "--dsh-bin".to_owned(),
            dsh_bin.clone(),
            "--dsh-node".to_owned(),
            std::env::var("AETHER_M7_06_DSH_NODE").unwrap_or_else(|_| "node".to_owned()),
            "--dsh-home".to_owned(),
            dsh_home.clone(),
        ]);
        if let Ok(version) = std::env::var("AETHER_M7_06_DSH_VERSION") {
            args.extend(["--dsh-version".to_owned(), version]);
        }
    }
    let manifest = RuntimeManifest::new(runtime_id, runtime_id, adapter)
        .official(true)
        .with_args(args);
    let hook = ProviderEnvHook::new(dir.path().to_path_buf());
    hook.wire(
        reads.clone(),
        Some(Arc::clone(&secrets) as Arc<dyn SecretStore>),
    );
    let spec = RuntimeSpec::with_fresh_token(manifest).with_env_source(hook);
    let supervisor = Arc::new(
        boot_supervisor(vec![spec], Some(&dir.path().join("adapters.json")))
            .expect("构造监督器"),
    );
    let startup = run_supervisor_startup(&supervisor, &handle).expect("启动序列尾段");
    for (id, outcome) in &startup.warmups {
        assert!(
            outcome.is_ready(),
            "适配器 {id} 预热必须 Ready（真实冒烟）：{outcome:?}"
        );
    }

    let executor = Arc::new(AdapterRunExecutor::new(
        Arc::clone(&supervisor),
        pipeline.clone(),
        reads.clone(),
        write.clone(),
        handle.clone(),
        None,
    ));
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        executor,
    );

    let session = runtime_handle
        .block_on(manager.create_session_with_options(
            runtime_row(runtime_id),
            &format!("m7-06-{runtime_id}"),
            SessionCreateOptions {
                model: Some(model.clone()),
                ..SessionCreateOptions::default()
            },
        ))
        .expect("创建会话");
    let session_id = session.id.clone();
    let prompt = format!(
        "Do not use any tools and do not read any files. Reply with exactly this token and nothing else: {}",
        case.marker
    );
    let ack = runtime_handle
        .block_on(manager.send_with_thinking_depth(
            &session_id,
            &prompt,
            "01J00000000000000000000M73",
            None,
        ))
        .expect("发送消息");
    let run_id = ack.run_id.clone();

    // 等待 run 终态（真实模型调用；预算 300s）。
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut status = None;
    while Instant::now() < deadline {
        if let Ok(Some(run)) = runtime_handle.block_on(reads.run(&run_id)) {
            if matches!(
                run.status,
                RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Timeout
            ) {
                status = Some(run);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let run = status.expect("run 必须在 300s 内终态（真实冒烟）");
    // 失败路径也打印代理记录与 run 错误（--nocapture 调试/证据）。
    eprintln!("run[{runtime_id}] status={:?} error={:?}", run.status, run.error);
    let records = proxy.records();
    for record in &records {
        eprintln!("proxy[{runtime_id}] {record}");
    }
    let messages = runtime_handle
        .block_on(reads.messages_page(&session_id, None, 100))
        .unwrap_or_default();
    // 无论成败先受控收口（终止适配器进程与存储），避免 panic 展开时残留子进程句柄。
    if let Some(runtime) = supervisor.get(runtime_id) {
        let _ = runtime_handle.block_on(runtime.disable(None));
    }
    if let Some(storage_runtime) = storage.take() {
        let _ = runtime_handle.block_on(storage_runtime.shutdown());
    }
    runtime_handle.shutdown_background();

    assert_eq!(
        run.status,
        RunStatus::Succeeded,
        "run 必须完成（真实冒烟）：{run:?}"
    );
    let assistant = messages
        .iter()
        .rev()
        .find(|message| message.role == aether_core::MessageRole::Assistant)
        .expect("助手终稿");
    assert!(
        assistant.content.contains(case.marker),
        "助手文本必须命中标记（{marker}）：{content}",
        marker = case.marker,
        content = assistant.content
    );

    // 代理证据：请求到达（base_url 真实使用）+ 凭据匹配（注入的测试密钥）+ 模型一致。
    let model_records: Vec<&Value> = records
        .iter()
        .filter(|record| record["key_match"].as_bool().is_some())
        .collect();
    assert!(
        !model_records.is_empty(),
        "代理必须收到运行时请求（base_url 真实使用）：{records:?}"
    );
    assert!(
        model_records
            .iter()
            .any(|record| record["key_match"] == Value::Bool(true)),
        "运行时必须携带核心注入的测试密钥（DoD2 注入路径）：{model_records:?}"
    );
    assert!(
        model_records
            .iter()
            .any(|record| record["model"].as_str() == Some(model.as_str())),
        "请求体模型必须为会话模型 {model}：{model_records:?}"
    );
}

#[test]
#[ignore = "opt-in 真实冒烟（AETHER_M7_06_REAL=1）"]
fn real_claude_code_consumption() {
    run_case("claude-code");
}

#[test]
#[ignore = "opt-in 真实冒烟（AETHER_M7_06_REAL=1）"]
fn real_codex_consumption() {
    run_case("codex");
}

#[test]
#[ignore = "opt-in 真实冒烟（AETHER_M7_06_REAL=1）"]
fn real_dsh_consumption() {
    run_case("deepseek-harness");
}