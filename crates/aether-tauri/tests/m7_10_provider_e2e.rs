//! ADR-028 v0.9 / M7-10：三官方运行时多供应商应用级 E2E。
//!
//! 链路：命令层（`session_create` / `session_send` / `run_retry` / `session_resume` /
//! `session_interrupt`）+ 真 Credential Broker + 真 `ProviderEnvHook` + 真
//! `provider.catalog.get` + 三个真适配器进程 + 假 CLI/ACP + 本地合成密钥。
//!
//! 覆盖：
//! - 三运行时 per-session provider + model 必填；两会话并发、两 provider、不同
//!   key/base_url；Codex `CODEX_HOME` / DSH profile 隔离；
//! - 旧会话补选（三运行时分别）：`session_send.provider_id/model` → 适配器准备
//!   （Claude per-run env / Codex 复制 thread 状态 / DSH 复制 profile 状态）→
//!   持久化 → 后续 send 可用、原生上下文保留；已选再传 → `invalid_value`；
//! - 补选并发 `session_busy`（补选专用文案）、只读（provider 停用）与中断可用；
//! - 密钥轮换（两次 run 之间改 key，第二次取新值）；
//! - `SessionSummary` provider/model 状态与目录查询非机密。
//!
//! 运行：`AETHER_M7_10_REQUIRE=1 AETHER_CLAUDE_ADAPTER=... AETHER_CODEX_ADAPTER=...
//! AETHER_DSH_ADAPTER=... cargo test -p aether-tauri --test m7_10_provider_e2e`
//! （由 `pnpm verify:m7-10` 构建适配器包并编排）。未设置且未要求时显式 SKIP。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aether_adapters::supervisor::{RuntimeManifest, RuntimeSpec, Supervisor};
use aether_control::{LifecycleConfig, SessionManager, SystemClock};
use aether_core::{RunId, RunStatus, Session, SessionId, SessionStatus, TokenUsage};
use aether_security::{KeychainRef, SecretError, SecretStore, SecretValue, SecurityLevel};
use aether_store::{ProviderModelRecord, ProviderRecord, StoreCommand};
use aether_tauri::adapter_executor::AdapterRunExecutor;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use aether_tauri::credential_broker;
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{SessionCreateRequest, SessionIdRequest, SessionSendRequest};
use aether_tauri::ipc::error::IpcError;
use aether_tauri::provider_catalog::ProviderCatalog;
use aether_tauri::provider_env::ProviderEnvHook;
use aether_tauri::runtime_control::{boot_supervisor, run_supervisor_startup};
use aether_tauri::session_backend::SessionBackend;
use serde_json::{json, Value};
use tempfile::TempDir;

const PROVIDER_A: &str = "01J0000000000000000000PA1";
const PROVIDER_B: &str = "01J0000000000000000000PB1";
const MODEL_A: &str = "model-a";
const MODEL_B: &str = "model-b";
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
struct MemorySecretStore {
    map: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl MemorySecretStore {
    fn put(&self, provider_id: &str, value: &str) {
        let reference = KeychainRef::new("provider", provider_id).expect("引用");
        self.map
            .lock()
            .expect("锁")
            .insert(reference.to_uri(), value.to_owned());
    }
}

impl SecretStore for MemorySecretStore {
    fn level(&self) -> SecurityLevel {
        SecurityLevel::OsKeychain
    }

    fn get(&self, reference: &KeychainRef) -> Result<SecretValue, SecretError> {
        self.map
            .lock()
            .expect("锁")
            .get(&reference.to_uri())
            .map(|value| SecretValue::new(value.clone()))
            .ok_or_else(|| SecretError::NotFound {
                reference: reference.clone(),
            })
    }

    fn set(&self, reference: &KeychainRef, value: &SecretValue) -> Result<(), SecretError> {
        self.map
            .lock()
            .expect("锁")
            .insert(reference.to_uri(), value.expose().to_owned());
        Ok(())
    }

    fn delete(&self, reference: &KeychainRef) -> Result<(), SecretError> {
        self.map
            .lock()
            .expect("锁")
            .remove(&reference.to_uri())
            .map(|_| ())
            .ok_or_else(|| SecretError::NotFound {
                reference: reference.clone(),
            })
    }
}

struct Harness {
    #[allow(dead_code)]
    dir: TempDir,
    runtime: tokio::runtime::Runtime,
    #[allow(dead_code)]
    pipeline: aether_control::EventPipeline,
    reads: aether_store::ReadPool,
    write: aether_store::WriteQueue,
    secrets: Arc<MemorySecretStore>,
    backend: Arc<dyn IpcBackend>,
    supervisor: Arc<Supervisor>,
    #[allow(dead_code)]
    hook: Arc<ProviderEnvHook>,
    catalog: Arc<ProviderCatalog>,
    env_files: EnvFiles,
}

struct EnvFiles {
    claude: PathBuf,
    codex: PathBuf,
    dsh: PathBuf,
}

fn adapter_path(env: &str) -> Option<PathBuf> {
    match std::env::var(env) {
        Ok(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => {
            if std::env::var("AETHER_M7_10_REQUIRE").as_deref() == Ok("1") {
                panic!("AETHER_M7_10_REQUIRE=1 但 {env} 未设置");
            }
            None
        }
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio 运行时")
}

fn provider_record(id: &str, base: &str) -> ProviderRecord {
    ProviderRecord {
        id: id.to_owned(),
        name: format!("供应商 {id}"),
        provider_type: "deepseek".to_owned(),
        base_url: Some(base.to_owned()),
        endpoints: serde_json::json!({
            "anthropic": format!("{base}/anthropic"),
            "openai": format!("{base}/openai"),
            "dsh": format!("{base}/openai"),
        })
        .to_string(),
        api_key_ref: Some(format!("keychain://aether/provider/{id}")),
        enabled: true,
        is_builtin: false,
        created_at: 1,
        updated_at: 1,
    }
}

fn model_record(id: &str, provider_id: &str, model: &str) -> ProviderModelRecord {
    ProviderModelRecord {
        id: id.to_owned(),
        provider_id: provider_id.to_owned(),
        model_id: model.to_owned(),
        display_name: model.to_owned(),
        enabled: true,
        created_at: 1,
    }
}

fn harness() -> Option<Harness> {
    let claude = adapter_path("AETHER_CLAUDE_ADAPTER")?;
    let codex = adapter_path("AETHER_CODEX_ADAPTER")?;
    let dsh = adapter_path("AETHER_DSH_ADAPTER")?;
    let dir = TempDir::new().expect("临时目录");
    let root = repo_root();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("工作区");
    let claude_home = dir.path().join("fake-claude-home");
    let codex_home = dir.path().join("fake-codex-home");
    let dsh_home = dir.path().join("fake-dsh-home");
    std::fs::create_dir_all(&claude_home).expect("claude home");
    std::fs::create_dir_all(&codex_home).expect("codex home");
    std::fs::create_dir_all(&dsh_home).expect("dsh home");
    seed_old_state(&claude_home, &codex_home, &dsh_home);
    let env_files = EnvFiles {
        claude: dir.path().join("claude-env.jsonl"),
        codex: dir.path().join("codex-env.jsonl"),
        dsh: dir.path().join("dsh-env.jsonl"),
    };
    std::env::set_var("FAKE_CLAUDE_HOME", &claude_home);
    std::env::set_var("FAKE_CLAUDE_ENV_FILE", &env_files.claude);
    std::env::set_var("FAKE_CLAUDE_PID_FILE", dir.path().join("claude-pids.txt"));
    std::env::set_var("FAKE_CODEX_ENV_FILE", &env_files.codex);
    std::env::set_var("FAKE_CODEX_PID_FILE", dir.path().join("codex-pids.txt"));
    std::env::set_var("FAKE_DSH_ENV_FILE", &env_files.dsh);
    std::env::set_var("FAKE_DSH_PID_FILE", dir.path().join("dsh-pids.txt"));

    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let CoreBoot {
        reads,
        write,
        pipeline,
        ..
    } = boot_core_full(
        dir.path(),
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("核心启动");
    let secrets = Arc::new(MemorySecretStore::default());
    secrets.put(PROVIDER_A, "fake-key-a");
    secrets.put(PROVIDER_B, "fake-key-b");
    for (id, base) in [
        (PROVIDER_A, "https://a.example.com"),
        (PROVIDER_B, "https://b.example.com"),
    ] {
        runtime
            .block_on(write.execute(StoreCommand::InsertProvider {
                provider: provider_record(id, base),
            }))
            .expect("插入供应商");
    }
    for (id, provider, model) in [
        ("01J0000000000000000000MA1", PROVIDER_A, MODEL_A),
        ("01J0000000000000000000MB1", PROVIDER_B, MODEL_B),
    ] {
        runtime
            .block_on(write.execute(StoreCommand::InsertProviderModel {
                model: model_record(id, provider, model),
            }))
            .expect("插入模型");
    }

    let hook = ProviderEnvHook::new(dir.path().to_path_buf());
    hook.wire(
        reads.clone(),
        Some(Arc::clone(&secrets) as Arc<dyn SecretStore>),
    );
    let broker = credential_broker::start(hook.clone(), dir.path(), &handle).expect("broker");
    hook.set_broker_info(broker);
    let catalog = Arc::new(ProviderCatalog::new());
    catalog.wire(reads.clone());

    let node = std::env::var("AETHER_NODE_BIN").unwrap_or_else(|_| "node".to_owned());
    let fake_claude = root.join("scripts/test/m2-02/fake-claude/cli.mjs");
    let fake_codex = root.join("scripts/test/m2-11/fake-codex/cli.mjs");
    let fake_dsh = root.join("scripts/test/m2-11/fake-dsh/acp-server.mjs");
    let mut specs = Vec::new();
    let claude_manifest = RuntimeManifest::new("claude-code", "Claude Code", claude)
        .official(true)
        .with_args([
            "--claude-bin".to_owned(),
            node.clone(),
            "--claude-arg".to_owned(),
            fake_claude.display().to_string(),
            "--workspace".to_owned(),
            workspace.display().to_string(),
            "--tools".to_owned(),
            "none".to_owned(),
        ]);
    let codex_manifest = RuntimeManifest::new("codex", "Codex", codex)
        .official(true)
        .with_args([
            "--codex-bin".to_owned(),
            node.clone(),
            "--codex-arg".to_owned(),
            fake_codex.display().to_string(),
            "--codex-home".to_owned(),
            codex_home.display().to_string(),
            "--workspace".to_owned(),
            workspace.display().to_string(),
        ]);
    let dsh_manifest = RuntimeManifest::new("deepseek-harness", "DeepSeek Harness", dsh)
        .official(true)
        .with_args([
            "--dsh-bin".to_owned(),
            fake_dsh.display().to_string(),
            "--dsh-node".to_owned(),
            node,
            "--dsh-home".to_owned(),
            dsh_home.display().to_string(),
            "--dsh-version".to_owned(),
            "0.2.1-rc.2".to_owned(),
            "--dsh-version-pin".to_owned(),
            "0.2.1-rc.2".to_owned(),
            "--workspace".to_owned(),
            workspace.display().to_string(),
        ]);
    for manifest in [claude_manifest, codex_manifest, dsh_manifest] {
        let mut spec = RuntimeSpec::with_fresh_token(manifest);
        spec.env_source = Some(hook.clone());
        spec.catalog_source = Some(catalog.clone());
        specs.push(spec);
    }
    let supervisor = Arc::new(
        boot_supervisor(specs, Some(&dir.path().join("adapter-ledger.json"))).expect("监督器"),
    );
    let startup = run_supervisor_startup(&supervisor, &handle).expect("启动预热");
    for (runtime_id, outcome) in &startup.warmups {
        assert!(outcome.is_ready(), "{runtime_id} 未 Ready：{outcome:?}");
    }
    let executor = Arc::new(AdapterRunExecutor::new(
        Arc::clone(&supervisor),
        pipeline.clone(),
        reads.clone(),
        write.clone(),
        handle.clone(),
        None,
    ));
    // 上面 EventPipeline::new 可能不是实际签名；由编译器提示调整。
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        write.clone(),
        reads.clone(),
        pipeline.clone(),
        executor.clone(),
    );
    let backend: Arc<dyn IpcBackend> = Arc::new(
        SessionBackend::new(
            Arc::new(NotImplementedBackend),
            Some(manager),
            Some(executor),
            Some(reads.clone()),
            Some(Arc::clone(&supervisor)),
            handle,
        )
        .with_workspace_store(write.clone())
        .with_provider_env(hook.clone()),
    );
    Some(Harness {
        dir,
        runtime,
        pipeline,
        reads,
        write,
        secrets,
        backend,
        supervisor,
        hook,
        catalog,
        env_files,
    })
}

const CODEX_OLD_ALIAS: &str = "01J0000000000000000000C0DE";

impl Harness {
    fn create(&self, runtime_id: &str, provider: &str, model: &str) -> Result<Value, IpcError> {
        self.backend.session_create(&SessionCreateRequest {
            runtime_id: Some(runtime_id.to_owned()),
            title: format!("M7-10 {runtime_id}"),
            workspace_id: None,
            model: Some(model.to_owned()),
            provider_id: Some(provider.to_owned()),
            thinking_depth: None,
            permission_mode: None,
            preset_id: None,
        })
    }

    fn send(
        &self,
        session_id: &str,
        text: &str,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<Value, IpcError> {
        self.backend.session_send(&SessionSendRequest {
            session_id: session_id.to_owned(),
            text: text.to_owned(),
            client_msg_id: ulid::Ulid::new().to_string(),
            thinking_depth: None,
            references: Vec::new(),
            provider_id: provider.map(str::to_owned),
            model: model.map(str::to_owned),
        })
    }

    fn insert_old_session(&self, runtime_id: &str, native_id: &str) -> SessionId {
        let session_id = SessionId::new(ulid::Ulid::new().to_string()).expect("session id");
        let session = Session {
            id: session_id.clone(),
            runtime_id: aether_core::RuntimeId::new(runtime_id).expect("runtime"),
            workspace_id: None,
            parent_session_id: None,
            title: format!("旧会话 {runtime_id}"),
            status: SessionStatus::Idle,
            model: None,
            thinking_depth: aether_core::THINKING_DEPTH_DEFAULT,
            permission_mode: aether_core::PermissionMode::default(),
            preset_id: None,
            system_prompt: None,
            config: json!({ "native_id": native_id }),
            token_usage: TokenUsage::default(),
            created_at: 1,
            updated_at: 1,
            closed_at: None,
        };
        self.runtime
            .block_on(self.write.execute(StoreCommand::InsertSession { session }))
            .expect("插入旧会话");
        session_id
    }

    fn wait_run(&self, run_id: &str) -> RunStatus {
        let run_id = RunId::new(run_id.to_owned()).expect("run id");
        let deadline = Instant::now() + RUN_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(run) = self.runtime.block_on(self.reads.run(&run_id)).expect("run") {
                if matches!(
                    run.status,
                    RunStatus::Succeeded
                        | RunStatus::Failed
                        | RunStatus::Cancelled
                        | RunStatus::Timeout
                ) {
                    return run.status;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("run {run_id:?} 未在 {RUN_TIMEOUT:?} 内终态");
    }

    fn assistant_text(&self, session_id: &str) -> String {
        let session_id = SessionId::new(session_id.to_owned()).expect("session id");
        let messages = self
            .runtime
            .block_on(self.reads.messages_page(&session_id, None, 500))
            .expect("消息读取");
        messages
            .into_iter()
            .filter(|message| message.role == aether_core::MessageRole::Assistant)
            .map(|message| message.content)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn stored_session(&self, session_id: &str) -> Session {
        let session_id = SessionId::new(session_id.to_owned()).expect("session id");
        self.runtime
            .block_on(self.reads.session(&session_id))
            .expect("会话读取")
            .expect("会话存在")
    }

    fn disable_provider(&self, provider_id: &str, enabled: bool) {
        self.runtime
            .block_on(self.write.execute(StoreCommand::SetProviderEnabled {
                id: provider_id.to_owned(),
                enabled,
                updated_at: 2,
            }))
            .expect("供应商启停");
    }

    fn disable_model(&self, provider_id: &str, model: &str, enabled: bool) {
        self.runtime
            .block_on(self.write.execute(StoreCommand::SetProviderModelEnabled {
                provider_id: provider_id.to_owned(),
                model_id: model.to_owned(),
                enabled,
            }))
            .expect("模型启停");
    }

    fn env_records(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }
}

fn seed_old_state(claude_home: &Path, codex_home: &Path, dsh_home: &Path) {
    let claude_id = "11111111-1111-4111-8111-111111111111";
    let claude_dir = claude_home.join("sessions");
    std::fs::create_dir_all(&claude_dir).expect("claude sessions");
    std::fs::write(
        claude_dir.join(format!("{claude_id}.json")),
        serde_json::json!({ "sessionId": claude_id, "memory": ["CLAUDE-LEGACY-TOKEN"] })
            .to_string(),
    )
    .expect("claude session");
    let codex_id = "thr_m710_legacy";
    std::fs::write(
        codex_home.join("fake-codex-sessions.json"),
        serde_json::json!({ "threads": { codex_id: { "memory": "CODEX-LEGACY-TOKEN" } } })
            .to_string(),
    )
    .expect("codex session");
    std::fs::create_dir_all(codex_home.join("aether-bridge")).expect("codex state dir");
    let mut alias_sessions = serde_json::Map::new();
    alias_sessions.insert(
        CODEX_OLD_ALIAS.to_owned(),
        serde_json::json!({ "thread_id": codex_id, "updated_at": 1 }),
    );
    std::fs::write(
        codex_home.join("aether-bridge/sessions.json"),
        serde_json::json!({ "v": 1, "sessions": alias_sessions }).to_string(),
    )
    .expect("codex alias");
    let dsh_id = "acp-m710-legacy";
    std::fs::write(
        dsh_home.join("fake-dsh-sessions.json"),
        serde_json::json!({ "sessions": { dsh_id: { "memory": "DSH-LEGACY-TOKEN" } } }).to_string(),
    )
    .expect("dsh session");
}

fn run_e2e() {
    let Some(h) = harness() else {
        eprintln!(
            "SKIP：AETHER_CLAUDE_ADAPTER/AETHER_CODEX_ADAPTER/AETHER_DSH_ADAPTER 未设置（verify:m7-10 构建后执行）"
        );
        return;
    };

    // 1) 三运行时正向：两会话并发、两 provider、不同 key/base_url。
    for runtime_id in ["claude-code", "codex", "deepseek-harness"] {
        let first = h.create(runtime_id, PROVIDER_A, MODEL_A).expect("创建 A");
        let second = h.create(runtime_id, PROVIDER_B, MODEL_B).expect("创建 B");
        assert_eq!(first["provider_state"], "active");
        assert_eq!(first["model_state"], "active");
        let first_id = first["id"].as_str().expect("session id");
        let second_id = second["id"].as_str().expect("session id");
        let ack_a = h.send(first_id, "chat", None, None).expect("并发发送 A");
        let ack_b = h.send(second_id, "chat", None, None).expect("并发发送 B");
        let status_a = h.wait_run(ack_a["run_id"].as_str().expect("run id"));
        if status_a != RunStatus::Succeeded {
            eprintln!(
                "{runtime_id} run A failed: {:?}",
                h.runtime.block_on(
                    h.reads
                        .run(&RunId::new(ack_a["run_id"].as_str().expect("run id").to_owned()).expect("run id"))
                )
            );
            eprintln!(
                "{runtime_id} events: {:?}",
                h.backend.messages_page(&aether_tauri::ipc::dto::MessagesPageRequest {
                    session_id: first_id.to_owned(),
                    last_seq: None,
                    limit: Some(200),
                    max_bytes: None,
                    next_cursor: None,
                })
            );
        }
        assert_eq!(status_a, RunStatus::Succeeded);
        assert_eq!(
            h.wait_run(ack_b["run_id"].as_str().expect("run id")),
            RunStatus::Succeeded
        );
        let text = h.assistant_text(first_id);
        assert!(!text.is_empty(), "{runtime_id} 正向 run 必须有助手文本");
    }

    // 正向 env/config 隔离断言。
    let claude_records = Harness::env_records(&h.env_files.claude);
    assert!(claude_records
        .iter()
        .any(|record| record["ANTHROPIC_API_KEY"] == "fake-key-a"));
    assert!(claude_records
        .iter()
        .any(|record| record["ANTHROPIC_API_KEY"] == "fake-key-b"));
    assert!(claude_records
        .iter()
        .any(|record| record["ANTHROPIC_BASE_URL"] == "https://a.example.com/anthropic"));
    assert!(claude_records
        .iter()
        .any(|record| record["ANTHROPIC_BASE_URL"] == "https://b.example.com/anthropic"));
    for record in &claude_records {
        assert!(record["AETHER_CREDENTIAL_BROKER"].is_null());
    }
    let codex_records = Harness::env_records(&h.env_files.codex);
    let codex_homes: Vec<&str> = codex_records
        .iter()
        .filter_map(|record| record["CODEX_HOME"].as_str())
        .collect();
    assert!(codex_homes.len() >= 2);
    assert!(codex_homes.windows(2).any(|pair| pair[0] != pair[1]));
    for home in &codex_homes {
        let config =
            std::fs::read_to_string(Path::new(home).join("config.toml")).unwrap_or_default();
        assert!(!config.contains("fake-key-a") && !config.contains("fake-key-b"));
    }
    let dsh_records = Harness::env_records(&h.env_files.dsh);
    let dsh_homes: Vec<&str> = dsh_records
        .iter()
        .filter_map(|record| record["DSH_HOME"].as_str())
        .collect();
    assert!(dsh_homes.len() >= 3);
    assert!(
        dsh_homes
            .iter()
            .filter(|home| home.contains("aether-dsh-sessions"))
            .count()
            >= 2
    );
    for record in &dsh_records {
        assert!(record["AETHER_CREDENTIAL_BROKER"].is_null());
    }

    // 2) 旧会话补选（三运行时；原生上下文保留；持久化；已选再传拒绝）。
    let old_cases = [
        (
            "claude-code",
            "11111111-1111-4111-8111-111111111111",
            "CLAUDE-LEGACY-TOKEN",
        ),
        ("codex", CODEX_OLD_ALIAS, "CODEX-LEGACY-TOKEN"),
        ("deepseek-harness", "acp-m710-legacy", "DSH-LEGACY-TOKEN"),
    ];
    for (runtime_id, native_id, token) in old_cases {
        let session_id = h.insert_old_session(runtime_id, native_id);
        let sid = session_id.as_str();
        // 无 provider 正常 send（无字段）→ invalid_value("请选择供应商")。
        let error = h.send(sid, "recall", None, None).expect_err("旧会话未补选");
        assert_eq!(
            error.code,
            aether_tauri::ipc::error::IpcErrorCode::InvalidValue
        );
        assert!(error.message.contains("请选择供应商"));
        let ack = h
            .send(sid, "recall", Some(PROVIDER_A), Some(MODEL_A))
            .expect("旧会话补选发送");
        assert_eq!(
            h.wait_run(ack["run_id"].as_str().expect("run id")),
            RunStatus::Succeeded
        );
        let text = h.assistant_text(sid);
        assert!(
            text.contains(token),
            "{runtime_id} 原生上下文必须保留：{text}"
        );
        let stored = h.stored_session(sid);
        assert_eq!(
            stored
                .config
                .get("aether")
                .and_then(|value| value.get("provider_id"))
                .and_then(Value::as_str),
            Some(PROVIDER_A)
        );
        assert_eq!(stored.model.as_deref(), Some(MODEL_A));
        // 已选再传 provider/model → invalid_value。
        let error = h
            .send(sid, "recall", Some(PROVIDER_B), None)
            .expect_err("禁止切换 provider");
        assert!(error.message.contains("禁止") || error.message.contains("provider"));
        let error = h
            .send(sid, "recall", None, Some(MODEL_B))
            .expect_err("禁止改 model");
        assert!(error.message.contains("禁止"));
        // 后续 send 不带字段可用（持久化生效）。
        let ack = h.send(sid, "recall", None, None).expect("补选后发送");
        assert_eq!(
            h.wait_run(ack["run_id"].as_str().expect("run id")),
            RunStatus::Succeeded
        );
    }

    // 3) 密钥轮换：两次 run 之间改 key，第二次取到新值（Claude）。
    let rotation = h
        .create("claude-code", PROVIDER_A, MODEL_A)
        .expect("轮换会话");
    let rotation_id = rotation["id"].as_str().expect("session id");
    let ack = h.send(rotation_id, "chat", None, None).expect("轮换 run 1");
    assert_eq!(
        h.wait_run(ack["run_id"].as_str().expect("run id")),
        RunStatus::Succeeded
    );
    h.secrets.put(PROVIDER_A, "fake-key-a-rotated");
    let ack = h.send(rotation_id, "chat", None, None).expect("轮换 run 2");
    assert_eq!(
        h.wait_run(ack["run_id"].as_str().expect("run id")),
        RunStatus::Succeeded
    );
    let records = Harness::env_records(&h.env_files.claude);
    assert!(records
        .iter()
        .any(|record| record["ANTHROPIC_API_KEY"] == "fake-key-a-rotated"));
    h.secrets.put(PROVIDER_A, "fake-key-a");

    // 4) 补选并发 `session_busy`（模型失效 + 在途 run）与只读/中断。
    let busy = h
        .create("claude-code", PROVIDER_A, MODEL_A)
        .expect("busy 会话");
    let busy_id = busy["id"].as_str().expect("session id");
    let slow = h.send(busy_id, "slow", None, None).expect("在途 run");
    let slow_run = slow["run_id"].as_str().expect("run id").to_owned();
    h.disable_model(PROVIDER_A, MODEL_A, false);
    let error = h
        .send(busy_id, "chat", None, Some(MODEL_B))
        .expect_err("补选并发");
    assert!(
        error
            .message
            .contains("当前任务仍在运行，暂不能补选供应商/模型"),
        "补选 session_busy 必须使用上下文文案：{}",
        error.message
    );
    assert!(error.message.contains("session_busy"));
    // 中断可用。
    let interrupt = h
        .backend
        .session_interrupt(&SessionIdRequest {
            session_id: busy_id.to_owned(),
        })
        .expect("只读/在途中断可用");
    assert!(interrupt["interrupted_run"].is_string());
    assert_eq!(h.wait_run(&slow_run), RunStatus::Cancelled);
    // 模型已停用 → model_state=invalid；补选 model-b（PROVIDER_A 无 model-b → 拒绝）。
    h.disable_model(PROVIDER_A, MODEL_A, true);
    // provider 失效只读：停用 PROVIDER_A，messages_page 可读、send/retry/resume 拒绝。
    let read_only = h
        .create("claude-code", PROVIDER_A, MODEL_A)
        .expect("只读会话");
    let read_only_id = read_only["id"].as_str().expect("session id");
    let slow = h.send(read_only_id, "slow", None, None).expect("只读在途");
    let slow_run = slow["run_id"].as_str().expect("run id").to_owned();
    h.disable_provider(PROVIDER_A, false);
    let error = h
        .send(read_only_id, "chat", None, None)
        .expect_err("只读 send");
    assert!(
        error.message.contains("只读"),
        "必须只读拒绝：{}",
        error.message
    );
    let error = h
        .backend
        .run_retry(&aether_tauri::ipc::dto::RunRetryRequest {
            run_id: slow_run.clone(),
        })
        .expect_err("只读 retry");
    assert!(error.message.contains("只读") || error.message.contains("run"));
    let error = h
        .backend
        .session_resume(&SessionIdRequest {
            session_id: read_only_id.to_owned(),
        })
        .expect_err("只读 resume");
    assert!(error.message.contains("只读"));
    let page = h
        .backend
        .messages_page(&aether_tauri::ipc::dto::MessagesPageRequest {
            session_id: read_only_id.to_owned(),
            last_seq: None,
            limit: Some(50),
            max_bytes: None,
            next_cursor: None,
        })
        .expect("只读历史可读");
    assert!(page.get("events").is_some());
    // 中断在只读后仍可用（run 仍在途）。
    let interrupted = h
        .backend
        .session_interrupt(&SessionIdRequest {
            session_id: read_only_id.to_owned(),
        })
        .expect("只读中断可用");
    assert!(interrupted["interrupted_run"].is_string());
    assert_eq!(h.wait_run(&slow_run), RunStatus::Cancelled);
    h.disable_provider(PROVIDER_A, true);

    // 5) 目录查询非机密（adapter → core 请求经真连接；此处经 core 源直读断言）。
    let catalog = h
        .runtime
        .block_on(aether_adapters::connection::ProviderCatalogSource::catalog(
            h.catalog.as_ref(),
        ))
        .expect("目录查询");
    let text = catalog.to_string();
    assert!(text.contains(PROVIDER_A));
    assert!(!text.contains("api_key"));
    assert!(!text.contains("fake-key"));

    h.runtime.block_on(h.supervisor.shutdown_all());
}

#[test]
fn m7_10_provider_e2e() {
    run_e2e();
}
