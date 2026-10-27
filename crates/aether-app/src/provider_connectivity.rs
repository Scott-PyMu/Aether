//! 供应商连通性测试 HTTP 客户端（M7-06/ADR-024 §2.1-3/§2.1-4）。
//!
//! 边界（ADR-024）：
//! - 请求总超时 **10s**、连接超时 **5s**（R9；常量入任务证据，调整按常量级调参登记）；
//! - 出站不做限制（R6，风险接受留痕随 ADR-024 归档）；
//! - 五路径错误映射：成功 / `provider_auth_failed`（401/403）/
//!   `provider_unreachable`（网络不可达 / DNS 失败 / 超时）/ `provider_error`（其余
//!   4xx/5xx，message 含状态码）/ 请求未发出（无密钥，由命令层先行 `invalid_value`）；
//! - 响应与日志不含密钥本体（D10：本模块不把任何头部/密钥写入日志或错误文本）。
//!
//! 端点口径（ADR-024 §2.1-2/实现要点 2）：`GET {base_url}/models`；无 `base_url` 时
//! 按供应商类型使用官方默认端点。

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ipc::error::IpcError;

/// 请求总超时（ADR-024 A-5；常量）。
pub const PROVIDER_HTTP_TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
/// 连接超时（ADR-024 A-5；常量）。
pub const PROVIDER_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 供应商类型默认端点（无 `base_url` 时；ADR-024 实现要点 2）。
const DEFAULT_ANTHROPIC_BASE: &str = "https://api.anthropic.com/v1";
const DEFAULT_OPENAI_BASE: &str = "https://api.openai.com/v1";
const DEFAULT_DEEPSEEK_BASE: &str = "https://api.deepseek.com";
const DEFAULT_GOOGLE_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

/// 归一化后的模型 id 长度上限（ADR-024 §2.1-4：长度 ≤128 校验；口径同 IPC `model`）。
pub const MAX_FETCHED_MODEL_CHARS: usize = 128;

/// 连通性测试结果（`provider_test` 成功回执；ADR-024 §2.1-3「含延迟与模型数」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectivityOutcome {
    /// HTTP 状态码（2xx）。
    pub status: u16,
    /// 请求耗时（毫秒）。
    pub latency_ms: u64,
    /// 响应中解析出的模型 id（可能为空——2xx 即视为连通成功）。
    pub models: Vec<String>,
}

/// 构造 `GET {base_url}/models` 端点；`base_url` 缺省按类型默认。
pub fn models_endpoint(provider_type: &str, base_url: Option<&str>) -> Result<String, IpcError> {
    let base = match (provider_type, base_url) {
        (_, Some(base_url)) if !base_url.trim().is_empty() => base_url.trim().to_owned(),
        ("anthropic", _) => DEFAULT_ANTHROPIC_BASE.to_owned(),
        ("openai", _) => DEFAULT_OPENAI_BASE.to_owned(),
        ("deepseek", _) => DEFAULT_DEEPSEEK_BASE.to_owned(),
        ("google", _) => DEFAULT_GOOGLE_BASE.to_owned(),
        (other, _) => {
            return Err(IpcError::invalid_value(format!(
                "供应商类型 {other} 无默认端点；请先配置 base_url"
            )))
        }
    };
    Ok(format!("{}/models", base.trim_end_matches('/')))
}

/// 发起连通性测试（阻塞；调用方经 `spawn_blocking` 执行）。
///
/// 错误映射见模块文档；错误文本不回显 URL 凭据（密钥只经请求头，不回显任何头部值）。
pub fn test_connection(
    provider_type: &str,
    base_url: Option<&str>,
    api_key: &str,
) -> Result<ConnectivityOutcome, IpcError> {
    test_connection_with_timeouts(
        provider_type,
        base_url,
        api_key,
        PROVIDER_HTTP_CONNECT_TIMEOUT,
        PROVIDER_HTTP_TOTAL_TIMEOUT,
    )
}

/// 注入超时的实现（单测/故障注入口径；生产常量见 [`PROVIDER_HTTP_TOTAL_TIMEOUT`]）。
pub fn test_connection_with_timeouts(
    provider_type: &str,
    base_url: Option<&str>,
    api_key: &str,
    connect_timeout: Duration,
    total_timeout: Duration,
) -> Result<ConnectivityOutcome, IpcError> {
    let endpoint = models_endpoint(provider_type, base_url)?;
    // TLS：rustls + 平台根证书验证器（OS 信任库；`webpki-roots` 被许可策略排除，
    // 见 `Cargo.toml` 注释）；加密提供者显式指定 ring（与构建特性同源）。
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(connect_timeout))
        .timeout_global(Some(total_timeout))
        .tls_config(tls)
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let mut request = agent.get(&endpoint);
    request = match provider_type {
        "anthropic" => request
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01"),
        "google" => request.header("x-goog-api-key", api_key),
        // openai / deepseek / custom：OpenAI 兼容 Bearer（ADR-024 A-1 同类口径）。
        _ => request.header("Authorization", &format!("Bearer {api_key}")),
    };

    let started = Instant::now();
    let response = request.call();
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(status)) => {
            return Err(map_status_error(status));
        }
        Err(error) => {
            return Err(IpcError::provider_unreachable(&classify_transport_error(&error)));
        }
    };

    let status = response.status().as_u16();
    let body = match response.into_body().read_to_string() {
        Ok(body) => body,
        Err(error) => {
            return Err(IpcError::provider_unreachable(&format!(
                "响应读取失败：{error}"
            )));
        }
    };
    Ok(ConnectivityOutcome {
        status,
        latency_ms,
        models: parse_model_ids(&body),
    })
}

/// 状态码 → 错误映射（401/403 → `provider_auth_failed`；其余 → `provider_error`）。
///
/// 4xx/5xx 默认由 ureq `http_status_as_error` 走 `Error::StatusCode`；本函数保证
/// 语义与 ADR-024 五路径表一致。
fn map_status_error(status: u16) -> IpcError {
    if status == 401 || status == 403 {
        IpcError::provider_auth_failed(&format!("服务端返回 {status}"))
    } else {
        IpcError::provider_error(Some(status), "供应商端点拒绝或异常（可稍后重试）")
    }
}

/// 传输层错误分类（不回显可能含凭据的细节；只保留类别）。
fn classify_transport_error(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Timeout(_) => "请求超时（总超时 10s / 连接 5s）".to_owned(),
        ureq::Error::ConnectionFailed => "连接失败（端点不可达或拒绝连接）".to_owned(),
        ureq::Error::HostNotFound => "DNS 解析失败（主机不存在）".to_owned(),
        ureq::Error::Io(io) => format!("网络 I/O 错误（{}）", io.kind()),
        ureq::Error::Tls(_) => "TLS 握手失败".to_owned(),
        other => format!("{other}"),
    }
}

/// 响应体 → 模型 id 列表（容忍 OpenAI/Anthropic `data[].id` 与 Google `models[].name`）。
///
/// 归一化（ADR-024 §2.1-4）：`trim`、空串剔除、去重（保序）、`models/` 前缀剥离；
/// 长度校验（≤128）由 [`normalize_model_ids`] 承担（供 `provider_models_fetch` 计数
/// `skipped`），本函数不做截断（连通性测试只计数量）。
pub fn parse_model_ids(body: &str) -> Vec<String> {
    normalize_model_ids(&parse_model_ids_raw(body)).0
}

/// 响应体 → 模型 id 原始列表（`models/` 前缀剥离；未做 trim/去重/长度校验）。
///
/// `provider_models_fetch` 用本函数保留原始形态，交由 [`normalize_model_ids`] 归一化并
/// 统计 `skipped`。
pub fn parse_model_ids_raw(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    if let Some(items) = value.get("data").and_then(|data| data.as_array()) {
        for item in items {
            if let Some(id) = item.get("id").and_then(|id| id.as_str()) {
                ids.push(id.to_owned());
            }
        }
    }
    if let Some(items) = value.get("models").and_then(|models| models.as_array()) {
        for item in items {
            let raw = item
                .get("id")
                .and_then(|id| id.as_str())
                .or_else(|| item.get("name").and_then(|name| name.as_str()));
            if let Some(raw) = raw {
                let trimmed = raw.trim();
                ids.push(
                    trimmed
                        .strip_prefix("models/")
                        .unwrap_or(trimmed)
                        .to_owned(),
                );
            }
        }
    }
    ids
}

/// 归一化模型 id（ADR-024 §2.1-4）：`trim` → 空串剔除 → 去重（保序）→ 长度校验。
///
/// 返回 `(有效 id 列表, 被跳过的条数)`；长度 > [`MAX_FETCHED_MODEL_CHARS`] 的条目
/// 计入 `skipped`（不使整体失败）。
pub fn normalize_model_ids(raw: &[String]) -> (Vec<String>, usize) {
    let mut seen = std::collections::HashSet::new();
    let mut valid = Vec::new();
    let mut skipped = 0usize;
    for item in raw {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            skipped += 1;
            continue;
        }
        if trimmed.chars().count() > MAX_FETCHED_MODEL_CHARS {
            skipped += 1;
            continue;
        }
        let normalized = trimmed.to_owned();
        if seen.insert(normalized.clone()) {
            valid.push(normalized);
        } else {
            skipped += 1;
        }
    }
    (valid, skipped)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// 单请求本地 HTTP 桩服务：按 `responder` 返回原始字节。
    fn spawn_stub_server(
        responder: impl Fn(String) -> Vec<u8> + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("本地地址");
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 8192];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let response = responder(request);
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        });
        (format!("http://{addr}"), handle)
    }

    fn http_response(status: u16, body: &str) -> Vec<u8> {
        let reason = match status {
            200 => "OK",
            401 => "Unauthorized",
            500 => "Internal Server Error",
            _ => "Status",
        };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn endpoint_follows_base_url_and_defaults() {
        assert_eq!(
            models_endpoint("anthropic", Some("https://env-aitoken.streamax.com/anth/v1")).unwrap(),
            "https://env-aitoken.streamax.com/anth/v1/models"
        );
        assert_eq!(
            models_endpoint("anthropic", Some("https://example.com/v1/")).unwrap(),
            "https://example.com/v1/models"
        );
        assert_eq!(
            models_endpoint("openai", None).unwrap(),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_endpoint("deepseek", None).unwrap(),
            "https://api.deepseek.com/models"
        );
        assert_eq!(
            models_endpoint("google", None).unwrap(),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
        let err = models_endpoint("custom", None).expect_err("custom 必须有 base_url");
        assert_eq!(err.code.as_str(), "invalid_value");
    }

    #[test]
    fn success_path_returns_models_and_latency() {
        let bodies = r#"{"data":[{"id":"m-1"},{"id":"m-2"}]}"#.to_owned();
        let (base, server) = spawn_stub_server(move |request| {
            assert!(request.starts_with("GET /models "), "{request}");
            assert!(
                request.to_ascii_lowercase().contains("authorization: bearer unit-key"),
                "必须携带注入密钥（Bearer）：{request}"
            );
            http_response(200, &bodies)
        });
        let outcome = test_connection_with_timeouts(
            "openai",
            Some(&base),
            "unit-key",
            Duration::from_secs(5),
            Duration::from_secs(10),
        )
        .expect("成功路径");
        assert_eq!(outcome.status, 200);
        assert_eq!(outcome.models, vec!["m-1".to_owned(), "m-2".to_owned()]);
        server.join().unwrap();
    }

    #[test]
    fn auth_failure_maps_to_provider_auth_failed() {
        let (base, server) = spawn_stub_server(|_| http_response(401, "{}"));
        let error = test_connection_with_timeouts(
            "openai",
            Some(&base),
            "bad-key",
            Duration::from_secs(5),
            Duration::from_secs(10),
        )
        .expect_err("401");
        assert_eq!(error.code.as_str(), "provider_auth_failed");
        assert!(error.message.contains("401"), "{}", error.message);
        server.join().unwrap();

        let (base, server) = spawn_stub_server(|_| http_response(403, "{}"));
        let error = test_connection_with_timeouts(
            "openai",
            Some(&base),
            "bad-key",
            Duration::from_secs(5),
            Duration::from_secs(10),
        )
        .expect_err("403");
        assert_eq!(error.code.as_str(), "provider_auth_failed");
        server.join().unwrap();
    }

    #[test]
    fn vendor_error_maps_to_provider_error_with_status() {
        let (base, server) = spawn_stub_server(|_| http_response(500, "{}"));
        let error = test_connection_with_timeouts(
            "openai",
            Some(&base),
            "key",
            Duration::from_secs(5),
            Duration::from_secs(10),
        )
        .expect_err("500");
        assert_eq!(error.code.as_str(), "provider_error");
        assert!(error.message.contains("500"), "{}", error.message);
        server.join().unwrap();
    }

    #[test]
    fn unreachable_endpoint_maps_to_provider_unreachable() {
        // 未监听端口（立即拒绝连接）。
        let listener = TcpListener::bind("127.0.0.1:0").expect("占位端口");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let error = test_connection_with_timeouts(
            "custom",
            Some(&format!("http://{addr}")),
            "key",
            Duration::from_millis(500),
            Duration::from_millis(1_500),
        )
        .expect_err("不可达");
        assert_eq!(error.code.as_str(), "provider_unreachable");
    }

    #[test]
    fn timeout_maps_to_provider_unreachable() {
        // 接受连接但永不响应 → 总超时。
        let listener = TcpListener::bind("127.0.0.1:0").expect("挂起端口");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(3));
                drop(stream);
            }
        });
        let error = test_connection_with_timeouts(
            "custom",
            Some(&format!("http://{addr}")),
            "key",
            Duration::from_millis(300),
            Duration::from_millis(700),
        )
        .expect_err("超时");
        assert_eq!(error.code.as_str(), "provider_unreachable");
        assert!(error.message.contains("超时"), "{}", error.message);
        handle.join().unwrap();
    }

    #[test]
    fn google_naming_and_normalization() {
        let body = r#"{"models":[{"name":"models/gemini-1"},{"name":" models/gemini-2 "},{"name":"models/gemini-1"}]}"#;
        let ids = parse_model_ids(body);
        assert_eq!(ids, vec!["gemini-1".to_owned(), "gemini-2".to_owned()]);
    }

    #[test]
    fn normalization_trims_dedupes_and_skips_invalid() {
        let raw = vec![
            " a ".to_owned(),
            "a".to_owned(),
            "".to_owned(),
            "   ".to_owned(),
            "x".repeat(MAX_FETCHED_MODEL_CHARS + 1),
            "b".to_owned(),
        ];
        let (valid, skipped) = normalize_model_ids(&raw);
        assert_eq!(valid, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(skipped, 4, "空串×2 + 重复×1 + 超长×1");
    }
}
