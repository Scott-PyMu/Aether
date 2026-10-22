//! ADR-028 / M7-10：本地凭据 broker 集成断言。
//!
//! 覆盖：启动 broker → 适配器侧按 `provider_id + protocol` 请求 → 返回该供应商
//! 的端点/密钥/auth env；无效 token 拒绝；密钥不落盘/不日志（仅内存响应）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use aether_adapters::supervisor::SpawnEnvSource;
use aether_security::{KeychainRef, SecretError, SecretStore, SecretValue, SecurityLevel};
use aether_store::{ProviderRecord, StoreCommand, StoreRuntime, WriteQueueConfig};
use aether_tauri::credential_broker::{self, BrokerInfo};
use aether_tauri::provider_env::ProviderEnvHook;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

#[derive(Default)]
struct MemorySecretStore {
    map: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl MemorySecretStore {
    fn put(&self, reference: &KeychainRef, value: &str) {
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
            .insert(reference.to_uri(), value.expose().to_owned());
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

fn provider(provider_id: &str, keychain_ref: &str) -> ProviderRecord {
    ProviderRecord {
        id: provider_id.to_owned(),
        name: "DeepSeek (Claude Code)".to_owned(),
        provider_type: "deepseek".to_owned(),
        base_url: Some("https://api.deepseek.com".to_owned()),
        endpoints:
            r#"{"anthropic":"https://api.deepseek.com/anthropic","openai":"https://api.deepseek.com"}"#
                .to_owned(),
        api_key_ref: Some(keychain_ref.to_owned()),
        enabled: true,
        is_builtin: false,
        created_at: 1,
        updated_at: 10,
    }
}

async fn request<S>(stream: S, body: &str) -> String
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    write_half.write_all(body.as_bytes()).await.unwrap();
    write_half.write_all(b"\n").await.unwrap();
    write_half.flush().await.unwrap();
    let mut reader = BufReader::new(read_half.take(64 * 1024));
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    line
}

#[cfg(windows)]
async fn exchange(info: &BrokerInfo, body: &str) -> String {
    use tokio::net::windows::named_pipe::ClientOptions;
    let stream = ClientOptions::new()
        .open(&info.endpoint)
        .expect("连接 named pipe");
    request(stream, body).await
}

#[cfg(unix)]
async fn exchange(info: &BrokerInfo, body: &str) -> String {
    let stream = tokio::net::UnixStream::connect(&info.endpoint)
        .await
        .expect("连接 unix socket");
    request(stream, body).await
}

#[test]
fn broker_returns_provider_credential_and_rejects_bad_token() {
    let dir = tempfile::TempDir::new().expect("临时目录");
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    let storage = StoreRuntime::open(
        dir.path().join("aether.db"),
        WriteQueueConfig::default(),
        &handle,
    )
    .expect("存储运行时");
    let secrets = Arc::new(MemorySecretStore::default());
    let provider_id = "01J0000000000000000000M710";
    let reference = KeychainRef::new("provider", provider_id).expect("引用");
    secrets.put(&reference, "deepseek-test-key");
    runtime
        .block_on(storage.queue().execute(StoreCommand::InsertProvider {
            provider: provider(provider_id, &reference.to_uri()),
        }))
        .expect("登记供应商");

    let hook = ProviderEnvHook::new(dir.path().to_path_buf());
    hook.wire(
        storage.reads().clone(),
        Some(Arc::clone(&secrets) as Arc<dyn SecretStore>),
    );
    let info = credential_broker::start(hook.clone(), dir.path(), &handle).expect("broker 启动");
    hook.set_broker_info(info.clone());
    // broker 地址/token 随 spawn env 注入；token 不是供应商密钥。
    let spawned = runtime.block_on(hook.spawn_env("claude-code"));
    let broker_env = spawned
        .iter()
        .find(|(key, _)| key == "AETHER_CREDENTIAL_BROKER")
        .map(|(_, value)| value.clone())
        .expect("broker env 注入");
    let broker_info: serde_json::Value =
        serde_json::from_str(&broker_env).expect("broker env JSON");
    assert_eq!(
        broker_info["endpoint"].as_str(),
        Some(info.endpoint.as_str())
    );
    assert_eq!(broker_info["token"].as_str(), Some(info.token.as_str()));
    assert!(
        !broker_env.contains("deepseek-test-key"),
        "broker env 不得含供应商 key"
    );

    let valid = runtime.block_on(async {
        exchange(
            &info,
            &serde_json::json!({
                "token": info.token,
                "provider_id": provider_id,
                "protocol": "anthropic",
                "runtime_id": "claude-code",
            })
            .to_string(),
        )
        .await
    });
    let parsed: serde_json::Value = serde_json::from_str(valid.trim()).expect("响应 JSON");
    assert_eq!(parsed["ok"], true, "响应：{parsed}");
    assert_eq!(parsed["api_key"], "deepseek-test-key");
    assert_eq!(parsed["base_url"], "https://api.deepseek.com/anthropic");
    assert_eq!(parsed["auth_env"], "ANTHROPIC_API_KEY");

    let invalid = runtime.block_on(async {
        exchange(
            &info,
            &serde_json::json!({
                "token": "bad-token",
                "provider_id": provider_id,
                "protocol": "anthropic",
            })
            .to_string(),
        )
        .await
    });
    let parsed: serde_json::Value = serde_json::from_str(invalid.trim()).expect("响应 JSON");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["api_key"].as_str().is_none());

    let unofficial = runtime.block_on(async {
        exchange(
            &info,
            &serde_json::json!({
                "token": info.token,
                "provider_id": provider_id,
                "protocol": "anthropic",
                "runtime_id": "third-party",
            })
            .to_string(),
        )
        .await
    });
    let parsed: serde_json::Value = serde_json::from_str(unofficial.trim()).expect("响应 JSON");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["api_key"].as_str().is_none());
}
