//! ADR-028 v0.9 / M7-10：spawn 环境注入边界（三官方运行时）。
//!
//! M7-06 的 legacy 单供应商自动选择已被 ADR-028 v0.9 取代：P1 三官方运行时**永不**
//! 从 spawn env 获取供应商密钥；只注入 Credential Broker 地址/token，密钥按会话/按 run
//! 由 broker 现取。本测试锁定该边界（防止回退单供应商 env）。
//!
//! 运行：`cargo test -p aether-tauri --test m7_06_provider_env`。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use aether_adapters::supervisor::SpawnEnvSource;
use aether_security::{KeychainRef, SecretError, SecretStore, SecretValue, SecurityLevel};
use aether_store::{
    ProviderModelRecord, ProviderRecord, StoreCommand, StoreRuntime, WriteQueueConfig,
};
use aether_tauri::credential_broker::BrokerInfo;
use aether_tauri::provider_env::{
    ProviderEnvHook, ENV_ANTHROPIC_API_KEY, ENV_CODEX_HOME, ENV_CREDENTIAL_BROKER,
    ENV_DEEPSEEK_API_KEY, ENV_OPENAI_API_KEY,
};
use tempfile::TempDir;

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

fn insert_provider(
    runtime: &tokio::runtime::Runtime,
    storage: &StoreRuntime,
    secrets: &MemorySecretStore,
    id: &str,
    provider_type: &str,
    base_url: &str,
    key: &str,
) {
    let reference = KeychainRef::new("provider", id).expect("引用");
    secrets.put(id, key);
    runtime
        .block_on(storage.queue().execute(StoreCommand::InsertProvider {
            provider: ProviderRecord {
                id: id.to_owned(),
                name: format!("供应商 {id}"),
                provider_type: provider_type.to_owned(),
                base_url: Some(base_url.to_owned()),
                endpoints: "{}".to_owned(),
                api_key_ref: Some(reference.to_uri()),
                enabled: true,
                is_builtin: false,
                created_at: 1,
                updated_at: 10,
            },
        }))
        .expect("登记供应商");
}

#[test]
fn official_runtimes_never_receive_legacy_provider_env_even_with_providers() {
    let dir = TempDir::new().expect("临时目录");
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    let storage = StoreRuntime::open(
        dir.path().join("aether.db"),
        WriteQueueConfig::default(),
        &handle,
    )
    .expect("存储运行时");
    let secrets = MemorySecretStore::default();
    insert_provider(
        &runtime,
        &storage,
        &secrets,
        "01J0000000000000000000E01",
        "deepseek",
        "https://api.deepseek.com",
        "deepseek-test-key",
    );
    let models = [(
        "01J0000000000000000000M01",
        "01J0000000000000000000E01",
        "m-a",
    )];
    for (id, provider_id, model_id) in models {
        runtime
            .block_on(storage.queue().execute(StoreCommand::InsertProviderModel {
                model: ProviderModelRecord {
                    id: id.to_owned(),
                    provider_id: provider_id.to_owned(),
                    model_id: model_id.to_owned(),
                    display_name: model_id.to_owned(),
                    enabled: true,
                    created_at: 1,
                },
            }))
            .expect("登记模型");
    }
    let hook = ProviderEnvHook::new(dir.path().to_path_buf());
    hook.wire(
        storage.reads().clone(),
        Some(Arc::new(secrets) as Arc<dyn SecretStore>),
    );
    for runtime_id in ["claude-code", "codex", "deepseek-harness"] {
        let envs = runtime.block_on(hook.spawn_env(runtime_id));
        assert!(
            envs.is_empty(),
            "{runtime_id} 不得获得 legacy 供应商 env（ADR-028 v0.9）：{envs:?}"
        );
        assert!(envs.iter().all(|(key, _)| key != ENV_ANTHROPIC_API_KEY));
        assert!(envs.iter().all(|(key, _)| key != ENV_OPENAI_API_KEY));
        assert!(envs.iter().all(|(key, _)| key != ENV_DEEPSEEK_API_KEY));
        assert!(envs.iter().all(|(key, _)| key != ENV_CODEX_HOME));
    }
    assert!(runtime.block_on(hook.spawn_env("mock")).is_empty());
}

#[test]
fn broker_env_is_injected_without_provider_keys() {
    let dir = TempDir::new().expect("临时目录");
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    let storage = StoreRuntime::open(
        dir.path().join("aether.db"),
        WriteQueueConfig::default(),
        &handle,
    )
    .expect("存储运行时");
    let hook = ProviderEnvHook::new(dir.path().to_path_buf());
    hook.wire(storage.reads().clone(), None);
    hook.set_broker_info(BrokerInfo {
        endpoint: "test-endpoint".to_owned(),
        token: "test-token".to_owned(),
    });
    let envs = runtime.block_on(hook.spawn_env("claude-code"));
    assert_eq!(envs.len(), 1, "只允许 broker env：{envs:?}");
    assert_eq!(envs[0].0, ENV_CREDENTIAL_BROKER);
    assert!(!envs[0].1.contains("deepseek-test-key"));
}
