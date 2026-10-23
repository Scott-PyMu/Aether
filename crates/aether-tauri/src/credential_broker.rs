//! ADR-028：本地凭据 broker（D10；M7-10）。
//!
//! 设计要点：
//! - 仅本机命名管道（Windows）/ Unix domain socket（macOS/Linux），不监听网络端口；
//! - 适配器 spawn 时只拿到 broker 地址 + 一次性 token（`AETHER_CREDENTIAL_BROKER`），
//!   不拿到任何供应商密钥；
//! - 适配器按 `provider_id + protocol` 请求；broker 从 `ProviderEnvHook` 读取
//!   OS 凭据库/A3 加密文件后只返回该一家供应商的凭据；
//! - 请求/响应均为单行 JSON；不记录请求体或密钥，审计只记录 provider/protocol/结果。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::provider_env::ProviderEnvHook;

/// broker 地址 + 一次性 token（注入适配器 env；token 不是供应商密钥）。
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrokerInfo {
    pub endpoint: String,
    pub token: String,
}

#[derive(Deserialize)]
struct BrokerRequest {
    token: String,
    provider_id: String,
    protocol: String,
    /// ADR-028 v0.9：`runtime_id` **必填**（仅作标签与范围收窄，不构成强认证）。
    runtime_id: String,
}

#[derive(Serialize)]
struct BrokerResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

const BROKER_TIMEOUT: Duration = Duration::from_secs(5);
const BROKER_MAX_LINE_BYTES: u64 = 64 * 1024;
/// ADR-028：仅官方运行时 label 可请求；token 仍为唯一持有凭证。
const OFFICIAL_RUNTIMES: [&str; 3] = ["claude-code", "codex", "deepseek-harness"];

/// 启动 broker；失败时返回 `None`（适配器回退到旧的单供应商 env 路径，不阻断启动）。
pub fn start(
    hook: Arc<ProviderEnvHook>,
    data_dir: &Path,
    handle: &tokio::runtime::Handle,
) -> Option<BrokerInfo> {
    let token = format!("{}{}", ulid::Ulid::new(), ulid::Ulid::new());
    let endpoint = match endpoint_path(data_dir) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            tracing::warn!(error = %error, "凭据 broker 路径不可用：不启动（回退单供应商 env）");
            return None;
        }
    };
    let listener = match handle.block_on(create_listener(&endpoint)) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!(error = %error, "凭据 broker 创建失败：不启动（回退单供应商 env）");
            return None;
        }
    };
    let info = BrokerInfo {
        endpoint: endpoint.clone(),
        token: token.clone(),
    };
    handle.spawn(serve(listener, hook, token, endpoint.clone()));
    tracing::info!(endpoint = %endpoint, "凭据 broker 已启动（本地通道；token 不落盘）");
    Some(info)
}

fn endpoint_path(data_dir: &Path) -> std::io::Result<String> {
    #[cfg(windows)]
    {
        let _ = data_dir;
        Ok(format!(r"\\.\pipe\aether-credential-{}", ulid::Ulid::new()))
    }
    #[cfg(unix)]
    {
        std::fs::create_dir_all(data_dir)?;
        // best-effort：清理超过 1 小时的旧 socket（随机名；避免崩溃残留持续堆积）。
        if let Ok(entries) = std::fs::read_dir(data_dir) {
            let now = std::time::SystemTime::now();
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with("credential-") || !name.ends_with(".sock") {
                    continue;
                }
                let stale = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .is_some_and(|age| age.as_secs() > 3600);
                if stale {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let path = data_dir.join(format!("credential-{}.sock", ulid::Ulid::new()));
        Ok(path.to_string_lossy().into_owned())
    }
}

#[cfg(windows)]
type Listener = tokio::net::windows::named_pipe::NamedPipeServer;

#[cfg(windows)]
async fn create_listener(endpoint: &str) -> std::io::Result<Listener> {
    use tokio::net::windows::named_pipe::ServerOptions;
    ServerOptions::new()
        .first_pipe_instance(true)
        // 仅本机可连接（ADR-028 §2.3；管道继承进程当前用户默认 DACL）。
        .reject_remote_clients(true)
        .create(endpoint)
}

#[cfg(windows)]
async fn accept(listener: &mut Listener, endpoint: &str) -> std::io::Result<Listener> {
    use tokio::net::windows::named_pipe::ServerOptions;
    listener.connect().await?;
    let next = ServerOptions::new()
        .reject_remote_clients(true)
        .create(endpoint)?;
    Ok(std::mem::replace(listener, next))
}

#[cfg(unix)]
type Listener = tokio::net::UnixListener;

#[cfg(unix)]
async fn create_listener(endpoint: &str) -> std::io::Result<Listener> {
    use std::os::unix::fs::PermissionsExt;
    let listener = tokio::net::UnixListener::bind(endpoint)?;
    let _ = std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600));
    Ok(listener)
}

#[cfg(unix)]
async fn accept(
    listener: &mut Listener,
    _endpoint: &str,
) -> std::io::Result<tokio::net::UnixStream> {
    listener.accept().await.map(|(stream, _)| stream)
}

async fn serve(
    mut listener: Listener,
    hook: Arc<ProviderEnvHook>,
    token: String,
    endpoint: String,
) {
    loop {
        match accept(&mut listener, &endpoint).await {
            Ok(stream) => {
                tokio::spawn(handle_connection(stream, Arc::clone(&hook), token.clone()));
            }
            Err(error) => {
                tracing::warn!(error = %error, "凭据 broker accept 失败；继续监听");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle_connection<S>(stream: S, hook: Arc<ProviderEnvHook>, token: String)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half.take(BROKER_MAX_LINE_BYTES));
    let mut line = String::new();
    let read = tokio::time::timeout(BROKER_TIMEOUT, reader.read_line(&mut line)).await;
    let response = match read {
        Ok(Ok(_)) => process_request(&line, &hook, &token).await,
        Ok(Err(error)) => BrokerResponse::error(format!("读取凭据请求失败：{error}")),
        Err(_) => BrokerResponse::error("凭据请求超时"),
    };
    if let Ok(text) = serde_json::to_string(&response) {
        let _ = tokio::time::timeout(BROKER_TIMEOUT, async {
            write_half.write_all(text.as_bytes()).await?;
            write_half.write_all(b"\n").await?;
            write_half.flush().await
        })
        .await;
    }
}

async fn process_request(line: &str, hook: &ProviderEnvHook, token: &str) -> BrokerResponse {
    let request: BrokerRequest = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(_) => return BrokerResponse::error("凭据请求格式无效"),
    };
    if request.token != token {
        return BrokerResponse::error("凭据 broker token 无效");
    }
    let runtime_id = request.runtime_id.as_str();
    if !OFFICIAL_RUNTIMES.contains(&runtime_id) {
        return BrokerResponse::error("凭据 broker 请求方不是官方运行时");
    }
    let result = hook
        .resolve_credential_for_runtime(&request.provider_id, runtime_id, &request.protocol)
        .await;
    match &result {
        Ok(_) => tracing::info!(
            provider_id = %request.provider_id,
            runtime_id = %runtime_id,
            protocol = %request.protocol,
            result = "ok",
            "凭据 broker 请求"
        ),
        Err(error) => tracing::warn!(
            provider_id = %request.provider_id,
            runtime_id = %runtime_id,
            protocol = %request.protocol,
            result = "error",
            error = %error,
            "凭据 broker 请求"
        ),
    }
    match result {
        Ok(credential) => BrokerResponse {
            ok: true,
            base_url: credential.base_url,
            api_key: Some(credential.api_key),
            auth_env: Some(credential.auth_env),
            error: None,
        },
        Err(error) => BrokerResponse::error(error),
    }
}

impl BrokerResponse {
    fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            base_url: None,
            api_key: None,
            auth_env: None,
            error: Some(message.into()),
        }
    }
}
