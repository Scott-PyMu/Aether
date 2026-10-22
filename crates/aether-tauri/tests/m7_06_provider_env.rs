//! M7-06/ADR-024：供应商消费映射 spawn 环境注入（`ProviderEnvHook`）集成断言（DoD1/DoD2）。
//!
//! - 运行时 → 供应商类型映射与选择规则（启用 + `(updated_at, id)` 最大者）；
//! - Claude Code：`ANTHROPIC_API_KEY` + `ANTHROPIC_BASE_URL`；
//! - Codex：`OPENAI_API_KEY` + 隔离 `CODEX_HOME` + `AETHER_PROVIDER_*`；
//! - DSH：`DEEPSEEK_API_KEY` + `AETHER_PROVIDER_MODELS`（enabled、字典序、上限）；
//! - 无匹配 / 未启用 / 密钥不可读 → 空集合（失败隔离，不阻断启动）；
//! - 非映射运行时（mock）→ 空集合。
//!
//! 运行：`cargo test -p aether-tauri --test m7_06_provider_env`。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use aether_adapters::supervisor::SpawnEnvSource;
use aether_security::{KeychainRef, SecretError, SecretStore, SecretValue, SecurityLevel};
use aether_store::{
    ProviderModelRecord, ProviderRecord, StoreCommand, StoreRuntime, WriteQueueConfig,
};
use aether_tauri::provider_env::{
    ProviderEnvHook, ENV_AETHER_PROVIDER_BASE_URL, ENV_AETHER_PROVIDER_ENV_KEY,
    ENV_AETHER_PROVIDER_ID, ENV_AETHER_PROVIDER_MODEL, ENV_AETHER_PROVIDER_MODELS,
    ENV_ANTHROPIC_API_KEY,
    ENV_ANTHROPIC_BASE_URL, ENV_CODEX_HOME, ENV_DEEPSEEK_API_KEY, ENV_OPENAI_API_KEY,
};
use tempfile::TempDir;

/// 内存密钥存储。
#[derive(Default)]
struct MemorySecretStore {
    map: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl MemorySecretStore {
    fn put(&self, provider_id: &str, value: &str) {
        let reference = KeychainRef::new("provider", provider_id).expect("引用");
        self.map
            .lock()
            .unwrap()
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

struct Fixture {
    #[allow(dead_code)]
    dir: TempDir,
    runtime: tokio::runtime::Runtime,
    storage: StoreRuntime,
    secrets: Arc<MemorySecretStore>,
    hook: Arc<ProviderEnvHook>,
}

impl Fixture {
    fn open() -> Self {
        let dir = TempDir::new().expect("临时目录");
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();
        let storage = StoreRuntime::open(
            dir.path().join("aether.db"),
            WriteQueueConfig::default(),
            &handle,
        )
        .expect("存储运行时");
        let secrets = Arc::new(MemorySecretStore::default());
        let hook = ProviderEnvHook::new(dir.path().to_path_buf());
        hook.wire(
            storage.reads().clone(),
            Some(Arc::clone(&secrets) as Arc<dyn SecretStore>),
        );
        Self {
            dir,
            runtime,
            storage,
            secrets,
            hook,
        }
    }

    /// 登记供应商（`(updated_at, id)` 决定选中；`api_key_ref` 指向自身命名空间引用）。
    fn insert_provider(
        &self,
        id: &str,
        provider_type: &str,
        base_url: Option<&str>,
        enabled: bool,
        updated_at: i64,
        key: Option<&str>,
    ) {
        self.insert_provider_with_endpoints(
            id,
            provider_type,
            base_url,
            "{}",
            enabled,
            updated_at,
            key,
        );
    }

    /// ADR-028：带显式协议端点的供应商登记。
    #[allow(clippy::too_many_arguments)]
    fn insert_provider_with_endpoints(
        &self,
        id: &str,
        provider_type: &str,
        base_url: Option<&str>,
        endpoints: &str,
        enabled: bool,
        updated_at: i64,
        key: Option<&str>,
    ) {
        let api_key_ref = key.map(|_| format!("keychain://aether/provider/{id}"));
        if let Some(value) = key {
            self.secrets.put(id, value);
        }
        let provider = ProviderRecord {
            id: id.to_owned(),
            name: format!("供应商 {id}"),
            provider_type: provider_type.to_owned(),
            base_url: base_url.map(str::to_owned),
            endpoints: endpoints.to_owned(),
            api_key_ref,
            enabled,
            is_builtin: false,
            created_at: 1,
            updated_at,
        };
        self.runtime
            .block_on(
                self.storage
                    .queue()
                    .execute(StoreCommand::InsertProvider { provider }),
            )
            .expect("登记供应商");
    }

    fn insert_model(&self, provider_id: &str, model_id: &str, enabled: bool) {
        let model = ProviderModelRecord {
            id: format!("{provider_id}-{model_id}"),
            provider_id: provider_id.to_owned(),
            model_id: model_id.to_owned(),
            display_name: model_id.to_owned(),
            enabled,
            created_at: 1,
        };
        self.runtime
            .block_on(
                self.storage
                    .queue()
                    .execute(StoreCommand::InsertProviderModel { model }),
            )
            .expect("登记模型");
    }

    fn spawn_env(&self, runtime_id: &str) -> Vec<(String, String)> {
        self.runtime.block_on(self.hook.spawn_env(runtime_id))
    }
}

fn get(envs: &[(String, String)], name: &str) -> Option<String> {
    envs.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
}

#[test]
fn claude_code_selection_prefers_latest_enabled_provider_and_injects_env() {
    let fixture = Fixture::open();
    fixture.insert_provider(
        "01J00000000000000000000E01",
        "anthropic",
        Some("https://old.example.com/v1"),
        true,
        10,
        Some("key-old"),
    );
    fixture.insert_provider(
        "01J00000000000000000000E02",
        "anthropic",
        Some("https://new.example.com/v1"),
        true,
        20,
        Some("key-new"),
    );
    fixture.insert_provider(
        "01J00000000000000000000E03",
        "anthropic",
        Some("https://disabled.example.com/v1"),
        false,
        99,
        Some("key-disabled"),
    );
    let envs = fixture.spawn_env("claude-code");
    assert_eq!(
        get(&envs, ENV_ANTHROPIC_API_KEY).as_deref(),
        Some("key-new"),
        "必须选择启用且 updated_at 最大的 provider：{envs:?}"
    );
    assert_eq!(
        get(&envs, ENV_ANTHROPIC_BASE_URL).as_deref(),
        Some("https://new.example.com/v1")
    );
    // 不注入其他运行时的密钥变量。
    assert!(get(&envs, ENV_OPENAI_API_KEY).is_none());
    assert!(get(&envs, ENV_DEEPSEEK_API_KEY).is_none());
}

#[test]
fn claude_code_can_use_deepseek_provider_via_anthropic_endpoint() {
    let fixture = Fixture::open();
    fixture.insert_provider_with_endpoints(
        "01J00000000000000000000E09",
        "deepseek",
        Some("https://api.deepseek.com"),
        r#"{"anthropic":"https://api.deepseek.com/anthropic","openai":"https://api.deepseek.com"}"#,
        true,
        30,
        Some("deepseek-key"),
    );
    let envs = fixture.spawn_env("claude-code");
    assert_eq!(
        get(&envs, ENV_ANTHROPIC_API_KEY).as_deref(),
        Some("deepseek-key")
    );
    assert_eq!(
        get(&envs, ENV_ANTHROPIC_BASE_URL).as_deref(),
        Some("https://api.deepseek.com/anthropic"),
        "Claude Code 必须使用 DeepSeek 的 Anthropic 兼容端点：{envs:?}"
    );
    assert!(get(&envs, ENV_OPENAI_API_KEY).is_none());
}

#[test]
fn codex_injects_isolated_home_and_provider_metadata() {
    let fixture = Fixture::open();
    let provider_id = "01J00000000000000000000E04";
    fixture.insert_provider(
        provider_id,
        "openai",
        Some("https://gateway.example.com/v1"),
        true,
        5,
        Some("key-codex"),
    );
    let envs = fixture.spawn_env("codex");
    assert_eq!(get(&envs, ENV_OPENAI_API_KEY).as_deref(), Some("key-codex"));
    assert_eq!(
        get(&envs, ENV_AETHER_PROVIDER_ID).as_deref(),
        Some(provider_id)
    );
    assert_eq!(
        get(&envs, ENV_AETHER_PROVIDER_BASE_URL).as_deref(),
        Some("https://gateway.example.com/v1")
    );
    assert_eq!(
        get(&envs, ENV_AETHER_PROVIDER_ENV_KEY).as_deref(),
        Some(ENV_OPENAI_API_KEY)
    );
    let codex_home = get(&envs, ENV_CODEX_HOME).expect("CODEX_HOME 隔离注入");
    assert!(
        codex_home.contains("runtime-homes"),
        "CODEX_HOME 必须指向数据目录隔离子目录：{codex_home}"
    );
    assert!(std::path::Path::new(&codex_home).is_dir(), "隔离目录必须已创建");
}

#[test]
fn dsh_injects_sorted_enabled_models_only() {
    let fixture = Fixture::open();
    let provider_id = "01J00000000000000000000E05";
    fixture.insert_provider(
        provider_id,
        "deepseek",
        Some("https://api.deepseek.com"),
        true,
        5,
        Some("key-dsh"),
    );
    fixture.insert_model(provider_id, "m-b", true);
    fixture.insert_model(provider_id, "m-a", true);
    fixture.insert_model(provider_id, "m-disabled", false);
    let envs = fixture.spawn_env("deepseek-harness");
    assert_eq!(get(&envs, ENV_DEEPSEEK_API_KEY).as_deref(), Some("key-dsh"));
    let models: serde_json::Value =
        serde_json::from_str(&get(&envs, ENV_AETHER_PROVIDER_MODELS).expect("模型清单"))
            .expect("模型 JSON");
    let ids: Vec<&str> = models
        .as_array()
        .expect("数组")
        .iter()
        .map(|item| item["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, vec!["m-a", "m-b"], "enabled 模型按字典序（不含停用）");
    // 实测（M7-06 证据）：acp 行缺省模型缺失时 DSH 不注册 provider 模型选项 →
    // 注入 enabled 模型首个（字典序）。
    assert_eq!(
        get(&envs, ENV_AETHER_PROVIDER_MODEL).as_deref(),
        Some("m-a"),
        "缺省模型必须为 enabled 首个（字典序）"
    );
}

#[test]
fn no_match_or_disabled_or_unreadable_key_yields_no_injection() {
    let fixture = Fixture::open();
    // 类型不匹配（deepseek provider 不影响 claude-code 选择）。
    fixture.insert_provider(
        "01J00000000000000000000E06",
        "deepseek",
        Some("https://api.deepseek.com"),
        true,
        9,
        Some("key-ds"),
    );
    assert!(fixture.spawn_env("claude-code").is_empty());

    // 匹配但停用 → 空。
    fixture.insert_provider(
        "01J00000000000000000000E07",
        "anthropic",
        Some("https://x.example.com"),
        false,
        9,
        Some("key-x"),
    );
    assert!(fixture.spawn_env("claude-code").is_empty());

    // 匹配、启用，但密钥不可读（引用存在、条目缺失）→ 空（失败隔离）。
    let provider_id = "01J00000000000000000000E08";
    let provider = ProviderRecord {
        id: provider_id.to_owned(),
        name: "缺失密钥".to_owned(),
        provider_type: "anthropic".to_owned(),
        base_url: Some("https://missing.example.com".to_owned()),
        endpoints: "{}".to_owned(),
        api_key_ref: Some(format!("keychain://aether/provider/{provider_id}")),
        enabled: true,
        is_builtin: false,
        created_at: 1,
        updated_at: 9,
    };
    fixture
        .runtime
        .block_on(
            fixture
                .storage
                .queue()
                .execute(StoreCommand::InsertProvider { provider }),
        )
        .expect("登记供应商");
    assert!(fixture.spawn_env("claude-code").is_empty());

    // 非映射运行时 → 空。
    assert!(fixture.spawn_env("mock").is_empty());
}

#[test]
fn provider_without_base_url_injects_key_only() {
    let fixture = Fixture::open();
    fixture.insert_provider(
        "01J00000000000000000000E09",
        "anthropic",
        None,
        true,
        5,
        Some("key-no-base"),
    );
    let envs = fixture.spawn_env("claude-code");
    assert_eq!(
        get(&envs, ENV_ANTHROPIC_API_KEY).as_deref(),
        Some("key-no-base")
    );
    assert!(get(&envs, ENV_ANTHROPIC_BASE_URL).is_none(), "无 base_url 不注入");
}
