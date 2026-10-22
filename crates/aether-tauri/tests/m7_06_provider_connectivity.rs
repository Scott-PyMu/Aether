//! M7-06/ADR-024：供应商连通性测试五路径与模型获取集成断言（DoD5）。
//!
//! 通过真实 [`ProviderControl`] 命令路径（真实存储 + 内存密钥存储 + 本地 HTTP 桩）：
//! - 五路径：成功 / 密钥无效（401/403 → `provider_auth_failed`）/ 网络不可达
//!   （`provider_unreachable`）/ 超时（`provider_unreachable` + 超时文案）/
//!   供应商错误（其余 4xx/5xx → `provider_error`，message 含状态码）；
//! - 无密钥不发请求（`invalid_value`，field=`api_key_ref`；本地桩零连接）；
//! - `provider_models_fetch`：归一化（trim/去重/空串剔除/长度 ≤128）+ 幂等 upsert
//!   （新增 / 已有仅刷新 display_name）；
//! - 响应与日志不含密钥本体（响应 JSON 扫描）。
//!
//! 运行：`cargo test -p aether-tauri --test m7_06_provider_connectivity`。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aether_security::{KeychainRef, SecretError, SecretStore, SecretValue, SecurityLevel};
use aether_store::{
    ProviderRecord, StoreCommand, StoreOutcome, StoreRuntime, WriteQueueConfig,
};
use aether_tauri::ipc::dto::{ProviderModelsFetchRequest, ProviderTestRequest};
use aether_tauri::provider_control::{provider_key_ref_uri, ProviderControl};
use serde_json::Value;
use tempfile::TempDir;

const UNIT_KEY: &str = "unit-m7-06-secret-key";

/// 内存密钥存储（D10 语义同款：按引用读写）。
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

/// 本地 HTTP 桩（预置响应；记录连接数与末次请求）。
struct HttpStub {
    base_url: String,
    connections: Arc<AtomicUsize>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HttpStub {
    /// 启动桩：每个连接回放 `response`；`delay` 前不响应（超时路径）。
    fn start(status: u16, body: &'static str, delay: Duration, max_requests: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("本地地址");
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);
        let handle = std::thread::spawn(move || {
            listener
                .set_nonblocking(false)
                .expect("阻塞模式本地桩");
            for _ in 0..max_requests {
                let accepted = listener.accept();
                let Ok((mut stream, _)) = accepted else {
                    break;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buffer = [0u8; 8192];
                let _ = stream.read(&mut buffer);
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                let reason = match status {
                    200 => "OK",
                    401 => "Unauthorized",
                    403 => "Forbidden",
                    500 => "Internal Server Error",
                    _ => "Status",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            connections,
            handle: Some(handle),
        }
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for HttpStub {
    fn drop(&mut self) {
        // 释放监听（连接一次后线程自退；未连接时通过连接空请求唤醒）。
        if let Some(handle) = self.handle.take() {
            let addr = self.base_url.trim_start_matches("http://").to_owned();
            // 最多触发一次连接以结束 accept（不影响最大请求数断言路径的调用方）。
            if let Ok(stream) = TcpStream::connect_timeout(
                &addr.parse().expect("本地地址解析"),
                Duration::from_millis(200),
            ) {
                drop(stream);
            }
            let _ = handle.join();
        }
    }
}

/// 测试夹具：临时库 + 内存密钥 + ProviderControl。
struct Fixture {
    #[allow(dead_code)]
    dir: TempDir,
    runtime: tokio::runtime::Runtime,
    control: ProviderControl,
    #[allow(dead_code)]
    storage: StoreRuntime,
    #[allow(dead_code)]
    secrets: Arc<MemorySecretStore>,
}

impl Fixture {
    fn open(with_key: bool) -> Self {
        let dir = TempDir::new().expect("临时目录");
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();
        let storage = StoreRuntime::open(
            dir.path().join("aether.db"),
            WriteQueueConfig::default(),
            &handle,
        )
        .expect("存储运行时");
        let reads = storage.reads().clone();
        let write = storage.queue().clone();
        let secrets = Arc::new(MemorySecretStore::default());
        let provider_id = "01J00000000000000000000C01";
        if with_key {
            let reference = KeychainRef::new("provider", provider_id).expect("引用");
            secrets
                .set(&reference, &SecretValue::new(UNIT_KEY))
                .expect("写入密钥");
        }
        let api_key_ref = if with_key {
            Some(provider_key_ref_uri(provider_id).expect("引用 URI"))
        } else {
            None
        };
        let provider = ProviderRecord {
            id: provider_id.to_owned(),
            name: "M7-06 桩供应商".to_owned(),
            provider_type: "custom".to_owned(),
            base_url: None,
            endpoints: "{}".to_owned(),
            api_key_ref,
            enabled: true,
            is_builtin: false,
            created_at: 1,
            updated_at: 1,
        };
        runtime
            .block_on(write.execute(StoreCommand::InsertProvider { provider }))
            .expect("登记供应商");
        let control = ProviderControl::new(
            reads,
            write,
            Some(Arc::clone(&secrets) as Arc<dyn SecretStore>),
            handle,
        )
        .with_network_timeouts(Duration::from_millis(500), Duration::from_millis(1_500));
        Self {
            dir,
            runtime,
            control,
            storage,
            secrets,
        }
    }

    fn set_base_url(&self, base_url: &str) {
        let reads = self.storage.reads().clone();
        let mut provider = self
            .runtime
            .block_on(reads.provider("01J00000000000000000000C01"))
            .expect("读取供应商")
            .expect("供应商存在");
        provider.base_url = Some(base_url.to_owned());
        provider.updated_at = 2;
        let outcome = self
            .runtime
            .block_on(self.storage.queue().execute(StoreCommand::UpdateProvider { provider }))
            .expect("更新供应商");
        assert!(matches!(outcome, StoreOutcome::Applied { affected } if affected == 1));
    }

    fn test(&self) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.control.provider_test(&ProviderTestRequest {
            provider_id: "01J00000000000000000000C01".to_owned(),
        })
    }

    fn fetch(&self) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.control
            .provider_models_fetch(&ProviderModelsFetchRequest {
                provider_id: "01J00000000000000000000C01".to_owned(),
            })
    }
}

fn error_code(error: &aether_tauri::ipc::error::IpcError) -> String {
    error.code.as_str().to_owned()
}

#[test]
fn success_path_returns_ok_latency_and_models_count() {
    let fixture = Fixture::open(true);
    let stub = HttpStub::start(
        200,
        r#"{"data":[{"id":"m-1"},{"id":"m-2"},{"id":"m-1"}]}"#,
        Duration::ZERO,
        1,
    );
    fixture.set_base_url(&stub.base_url);
    let result = fixture.test().expect("成功路径");
    assert_eq!(result["ok"], true);
    assert_eq!(result["status"], 200);
    assert_eq!(result["models_count"], 2, "归一化去重后计数：{result}");
    assert!(result["latency_ms"].is_u64());
    assert_eq!(stub.connections(), 1);
    // D10：响应不含密钥本体。
    assert!(!result.to_string().contains(UNIT_KEY));
}

#[test]
fn missing_api_key_ref_returns_invalid_value_without_request() {
    let fixture = Fixture::open(false);
    // 桩监听但必须零连接（无密钥不发请求）。
    let stub = HttpStub::start(200, "{}", Duration::ZERO, 1);
    fixture.set_base_url(&stub.base_url);
    let error = fixture.test().expect_err("无密钥必须拒绝");
    assert_eq!(error_code(&error), "invalid_value");
    assert_eq!(error.field.as_deref(), Some("api_key_ref"));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(stub.connections(), 0, "无密钥不得发任何请求");
}

#[test]
fn auth_failure_maps_to_provider_auth_failed() {
    let fixture = Fixture::open(true);
    for status in [401u16, 403] {
        let stub = HttpStub::start(status, "{}", Duration::ZERO, 1);
        fixture.set_base_url(&stub.base_url);
        let error = fixture.test().expect_err("认证失败");
        assert_eq!(error_code(&error), "provider_auth_failed");
        assert!(error.message.contains(&status.to_string()), "{}", error.message);
    }
}

#[test]
fn vendor_error_maps_to_provider_error_with_status() {
    let fixture = Fixture::open(true);
    let stub = HttpStub::start(500, r#"{"error":"boom"}"#, Duration::ZERO, 1);
    fixture.set_base_url(&stub.base_url);
    let error = fixture.test().expect_err("供应商错误");
    assert_eq!(error_code(&error), "provider_error");
    assert!(error.message.contains("500"), "{}", error.message);
}

#[test]
fn unreachable_endpoint_maps_to_provider_unreachable() {
    let fixture = Fixture::open(true);
    // 占位端口（未监听）。
    let listener = TcpListener::bind("127.0.0.1:0").expect("占位端口");
    let addr = listener.local_addr().expect("地址");
    drop(listener);
    fixture.set_base_url(&format!("http://{addr}"));
    let error = fixture.test().expect_err("不可达");
    assert_eq!(error_code(&error), "provider_unreachable");
}

#[test]
fn timeout_maps_to_provider_unreachable_with_timeout_text() {
    let fixture = Fixture::open(true);
    // 接受连接但不响应（> 注入的总超时 1.5s）。
    let stub = HttpStub::start(200, "{}", Duration::from_secs(3), 1);
    fixture.set_base_url(&stub.base_url);
    let error = fixture.test().expect_err("超时");
    assert_eq!(error_code(&error), "provider_unreachable");
    assert!(error.message.contains("超时"), "{}", error.message);
}

#[test]
fn models_fetch_normalizes_and_upserts_idempotently() {
    let fixture = Fixture::open(true);
    let stub = HttpStub::start(
        200,
        r#"{"data":[
            {"id":" m-1 "},
            {"id":"m-1"},
            {"id":""},
            {"id":"m-2"},
            {"id":"   "}
        ]}"#,
        Duration::ZERO,
        2,
    );
    fixture.set_base_url(&stub.base_url);

    let first = fixture.fetch().expect("首次获取");
    assert_eq!(first["fetched"], 2, "{first}");
    assert_eq!(first["added"], 2, "{first}");
    assert_eq!(first["refreshed"], 0, "{first}");

    // 幂等重跑：不再新增（display_name 相同 → 不刷新计数；行数不变）。
    let second = fixture.fetch().expect("重复获取");
    assert_eq!(second["fetched"], 2, "{second}");
    assert_eq!(second["added"], 0, "{second}");
    assert_eq!(second["refreshed"], 2, "{second}");

    let models = fixture
        .runtime
        .block_on(fixture.storage.reads().provider_models())
        .expect("模型清单");
    let mut ids: Vec<&str> = models
        .iter()
        .map(|model| model.model_id.as_str())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["m-1", "m-2"]);
    assert!(!first.to_string().contains(UNIT_KEY));
}

#[test]
fn models_fetch_skips_overlong_and_behavior_without_key() {
    let fixture = Fixture::open(true);
    let long_id = format!(r#"{{"id":"{}"}}"#, "x".repeat(129));
    let body: &'static str = Box::leak(
        format!(r#"{{"data":[{{"id":"ok-1"}},{long_id}]}}"#).into_boxed_str(),
    );
    let stub = HttpStub::start(200, body, Duration::ZERO, 1);
    fixture.set_base_url(&stub.base_url);
    let result = fixture.fetch().expect("获取");
    assert_eq!(result["fetched"], 1, "超长模型 id 必须被跳过：{result}");
    assert_eq!(result["added"], 1);
    assert_eq!(stub.connections(), 1);

    // 无密钥的 fetch：不进网络（invalid_value）。
    let none = Fixture::open(false);
    let stub2 = HttpStub::start(200, "{}", Duration::ZERO, 1);
    none.set_base_url(&stub2.base_url);
    let error = none.fetch().expect_err("无密钥");
    assert_eq!(error_code(&error), "invalid_value");
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(stub2.connections(), 0);
}

/// opt-in 真实网络（TLS 路径）：真实 HTTPS 端点的 `/models` 探测。
///
/// 目的：验证 rustls + 平台根证书验证器链路可用（真实 TLS 握手 + HTTP 往返）。
/// 口径：探针端点（如网关 Anthropic 面 `{base}/models`）返回 404 → 期望
/// `provider_error`（含状态码）；若得到 `provider_unreachable` 说明 TLS/网络链路
/// 故障 → 失败。缺凭证 → 打印显式 SKIP 并返回（禁止静默跳过）。
///
/// 环境：`AETHER_M7_06_REAL=1` + `AETHER_M7_06_TLS_BASE_URL`（完整 https base，含路径）+
/// `AETHER_M7_06_TLS_TOKEN`。
#[test]
#[ignore = "opt-in 真实网络（AETHER_M7_06_REAL=1）"]
fn real_https_tls_path_provider_test() {
    if std::env::var("AETHER_M7_06_REAL").as_deref() != Ok("1") {
        eprintln!("SKIP：AETHER_M7_06_REAL 未设置（opt-in 真实网络）");
        return;
    }
    let base = match std::env::var("AETHER_M7_06_TLS_BASE_URL") {
        Ok(value) if value.starts_with("https://") => value,
        _ => {
            eprintln!("SKIP：AETHER_M7_06_TLS_BASE_URL 未提供（要求 https:// 完整 base）");
            return;
        }
    };
    let token = match std::env::var("AETHER_M7_06_TLS_TOKEN") {
        Ok(value) if !value.is_empty() => value,
        _ => {
            eprintln!("SKIP：AETHER_M7_06_TLS_TOKEN 未提供");
            return;
        }
    };
    let fixture = Fixture::open(true);
    // 覆盖测试密钥为该 token（TLS 探针走真实端点）。
    let reference = KeychainRef::new("provider", "01J00000000000000000000C01").expect("引用");
    fixture
        .secrets
        .set(&reference, &SecretValue::new(token))
        .expect("写入探针密钥");
    fixture.set_base_url(&base);
    let result = fixture.test();
    match result {
        Ok(value) => {
            eprintln!("[m7-06 tls] 端点 /models 成功：{value}");
        }
        Err(error) => {
            let code = error_code(&error);
            assert_ne!(
                code, "provider_unreachable",
                "TLS/网络链路必须可用（不得为 unreachable）：{error}"
            );
            assert_eq!(code, "provider_error", "探针端点 404 应映射 provider_error：{error}");
            assert!(error.message.contains("404"), "{}", error.message);
            eprintln!("[m7-06 tls] TLS 链路可用（404 → provider_error，含状态码）：{error}");
        }
    }
}
