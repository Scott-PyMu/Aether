//! `aetherd` 本地 HTTP/WS API 共享状态（M10-03；ADR-033 v1.1）。
//!
//! 本模块只承载**传输层**状态与配置：监听、鉴权 token、发现文件、速率/并发闸门、
//! 取消登记与 `health.api` 快照。业务命令仍全部委托
//! `aether-app::IpcState::dispatch`（ADR-033 D-033-5：不复制业务逻辑）。
//!
//! 硬约束：
//! - 默认仅绑定 loopback；远程绑定需另立 ADR（M10 不实现）；
//! - token 只驻留内存；日志/URL/响应不输出 token（轮换响应除外）；
//! - HTTP/WS 与 stdio 共用同一 `IpcState`，不持有独立会话/运行/权限状态；
//! - 关闭时先停止接受新连接，再等待在途请求，删除发现文件（不阻塞 daemon 8.2s 预算）。

pub mod http;

use std::collections::HashMap;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use aether_app::event_bridge::EventFeed;
use aether_app::ipc::{IpcError, IpcState};
use serde_json::{json, Value};
use tokio::sync::{watch, Semaphore};

/// 发现文件名（`<data_dir>/.aetherd-api.json`；ADR-033 D-033-2）。
pub const DISCOVERY_FILE_NAME: &str = ".aetherd-api.json";
/// 默认绑定地址（loopback）。
pub const DEFAULT_BIND: &str = "127.0.0.1";
/// 默认端口（0 = 随机空闲端口）。
pub const DEFAULT_PORT: u16 = 0;
/// HTTP body 上限（2 MiB；ADR-033 D-033-3）。
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// 最大并发连接/请求（ADR-033 D-033-2）。
pub const MAX_CONNECTIONS: usize = 64;
/// 每 token 每秒请求数（令牌桶）。
pub const RATE_LIMIT_PER_SEC: f64 = 100.0;
/// 令牌桶突发容量。
pub const RATE_LIMIT_BURST: f64 = 200.0;
/// WS 首帧鉴权超时（5s；ADR-033 D-033-2）。
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// WS 有界队列上限（4096；ADR-033 D-033-3）。
pub const WS_QUEUE_LIMIT: usize = 4_096;
/// WS 事件字节上限（32 MiB；ADR-033 D-033-3）。
pub const WS_BYTES_LIMIT: usize = 32 * 1024 * 1024;
/// 命令默认超时（M9 既有命令未单列超时；M10 登记表命令为 10s–30s）。
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;
/// HTTP 层额外超时（命令超时 + 5s；ADR-033 D-033-3）。
pub const HTTP_TIMEOUT_SLACK_MS: u64 = 5_000;
/// HTTP 监听失败退出码（`--http-only` / `AETHERD_HTTP_REQUIRED=1`；ADR-033 §6）。
pub const EXIT_HTTP_LISTENER_FAILED: u8 = 25;

/// 传输层业务码：请求超时（仅 HTTP/CLI，不进入 IPC 枚举；ADR-033 §6）。
pub const BUSINESS_REQUEST_TIMEOUT: &str = "request_timeout";

/// API 监听状态（`health.api`；ADR-033 D-033-6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiState {
    /// HTTP 未开启（`--stdio` 默认；降低本地攻击面）。
    Disabled,
    /// 正在监听。
    Listening { port: u16, since_ms: u64 },
    /// 监听失败（`--stdio --http` 非必需时不致命；`health.api.last_error` 可查）。
    Error { last_error: String },
}

impl ApiState {
    /// `health.api` JSON 形状（不返回 token）。
    pub fn to_json(&self) -> Value {
        match self {
            Self::Disabled => json!({"state": "disabled"}),
            Self::Listening { port, since_ms } => {
                json!({"state": "listening", "port": port, "since": since_ms})
            }
            Self::Error { last_error } => {
                json!({"state": "error", "last_error": last_error})
            }
        }
    }
}

/// HTTP 开关来源（CLI 参数 / 环境变量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMode {
    /// CLI 未显式指定：读 `AETHERD_HTTP`（缺省关闭）。
    Default,
    /// CLI 显式开启（`--http`）。
    Enabled,
    /// CLI 显式关闭（`--no-http`）。
    Disabled,
}

/// HTTP/WS API 配置（启动前解析；非法组合 → 参数错误）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiConfig {
    pub enabled: bool,
    pub required: bool,
    pub bind: String,
    pub port: u16,
    pub cors_origin: Option<String>,
    pub data_dir: PathBuf,
}

impl ApiConfig {
    /// 解析 CLI 开关 + 环境变量（ADR-033 D-033-1）。
    ///
    /// - `--http-only`：强制开启且 listener 失败致命（退出码 25）；
    /// - `AETHERD_HTTP=0` 与 `--http-only` 冲突 → `Err`（退出码 10）；
    /// - 非 loopback 绑定一律拒绝（M10 不实现远程绑定）。
    pub fn from_env(mode: HttpMode, http_only: bool, data_dir: PathBuf) -> Result<Self, String> {
        let env_http = std::env::var("AETHERD_HTTP").ok();
        let env_flag = match env_http.as_deref() {
            None => None,
            Some("1" | "true" | "TRUE") => Some(true),
            Some("0" | "false" | "FALSE") => Some(false),
            Some(other) => return Err(format!("AETHERD_HTTP 只能为 0/1（当前值：{other}）")),
        };

        if http_only && matches!(mode, HttpMode::Disabled) {
            return Err("--http-only 不能与 --no-http 同时使用".to_owned());
        }
        if http_only && env_flag == Some(false) {
            return Err("--http-only 与 AETHERD_HTTP=0 冲突（参数错误）".to_owned());
        }
        if matches!(mode, HttpMode::Enabled) && env_flag == Some(false) {
            return Err("--http 与 AETHERD_HTTP=0 冲突（参数错误）".to_owned());
        }

        let enabled = if http_only {
            true
        } else {
            match mode {
                HttpMode::Enabled => true,
                HttpMode::Disabled => false,
                HttpMode::Default => env_flag.unwrap_or(false),
            }
        };
        let required = http_only || std::env::var("AETHERD_HTTP_REQUIRED").as_deref() == Ok("1");

        let bind = std::env::var("AETHERD_HTTP_BIND")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BIND.to_owned());
        let ip: IpAddr = bind
            .parse()
            .map_err(|_| format!("AETHERD_HTTP_BIND 不是合法 IP 地址：{bind}"))?;
        if !ip.is_loopback() {
            if std::env::var("AETHERD_ALLOW_REMOTE").as_deref() != Ok("1") {
                return Err(format!(
                    "拒绝非 loopback 绑定 {bind}：M10 默认仅监听 127.0.0.1/::1（远程绑定需另立 ADR）"
                ));
            }
            return Err(format!(
                "远程绑定 {bind} 未实现：M10 仅支持 loopback（ADR-033 §7）"
            ));
        }

        let port = match std::env::var("AETHERD_HTTP_PORT") {
            Ok(raw) if !raw.trim().is_empty() => raw
                .parse::<u16>()
                .map_err(|_| format!("AETHERD_HTTP_PORT 必须是 0..=65535（当前值：{raw}）"))?,
            _ => DEFAULT_PORT,
        };
        let cors_origin = std::env::var("AETHERD_HTTP_CORS_ORIGIN")
            .ok()
            .filter(|value| !value.trim().is_empty());

        Ok(Self {
            enabled,
            required,
            bind,
            port,
            cors_origin,
            data_dir,
        })
    }

    /// 实际监听地址（IPv6 需要方括号）。
    pub fn socket_addr(&self) -> String {
        if self.bind.contains(':') {
            format!("[{}]:{}", self.bind, self.port)
        } else {
            format!("{}:{}", self.bind, self.port)
        }
    }
}

/// 发现文件内容（不序列化 token 到日志）。
#[derive(Debug, Clone)]
pub struct DiscoveryInfo {
    pub path: PathBuf,
    pub pid: u32,
    pub port: u16,
    pub bind: String,
    pub started_at: u64,
}

/// 取消登记：请求 id → 取消标志。
#[derive(Default)]
pub(crate) struct InFlight {
    canceled: AtomicBool,
}

impl InFlight {
    /// 是否已收到取消请求（响应阶段据此改为取消错误，不改变已落库状态）。
    pub(crate) fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::SeqCst)
    }
}

/// 简单令牌桶（每 token 100 req/s，突发 200；并发请求共用）。
struct RateBucket {
    tokens: f64,
    last: Instant,
}

impl RateBucket {
    fn new() -> Self {
        Self {
            tokens: RATE_LIMIT_BURST,
            last: Instant::now(),
        }
    }

    fn allow(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * RATE_LIMIT_PER_SEC).min(RATE_LIMIT_BURST);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// HTTP/WS API 共享状态（`Arc` 句柄；stdio/HTTP/WS/CLI 共用）。
pub struct ApiShared {
    state: RwLock<ApiState>,
    token: RwLock<Option<String>>,
    discovery: Mutex<Option<DiscoveryInfo>>,
    pipeline: OnceLock<Arc<dyn EventFeed>>,
    cors_origin: Option<String>,
    rate: Mutex<RateBucket>,
    in_flight: Mutex<HashMap<String, Arc<InFlight>>>,
    permits: Arc<Semaphore>,
    shutdown_tx: watch::Sender<bool>,
}

impl ApiShared {
    /// 构造禁用态（`--stdio` 默认；不产生发现文件）。
    pub fn disabled() -> Arc<Self> {
        Self::new(ApiState::Disabled, None, None)
    }

    /// 构造监听前/失败态（token 在开启时生成）。
    pub fn new(state: ApiState, token: Option<String>, cors_origin: Option<String>) -> Arc<Self> {
        let (shutdown_tx, _) = watch::channel(false);
        Arc::new(Self {
            state: RwLock::new(state),
            token: RwLock::new(token),
            discovery: Mutex::new(None),
            pipeline: OnceLock::new(),
            cors_origin,
            rate: Mutex::new(RateBucket::new()),
            in_flight: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            shutdown_tx,
        })
    }

    /// 生成 256-bit token 的待监听态。
    pub fn prepared(cors_origin: Option<String>) -> Result<Arc<Self>, String> {
        let token = generate_token()?;
        Ok(Self::new(ApiState::Disabled, Some(token), cors_origin))
    }

    /// 当前状态快照。
    pub fn state(&self) -> ApiState {
        match self.state.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// 更新状态。
    pub fn set_state(&self, state: ApiState) {
        match self.state.write() {
            Ok(mut guard) => *guard = state,
            Err(poisoned) => *poisoned.into_inner() = state,
        }
    }

    /// `health.api` 快照。
    pub fn health_json(&self) -> Value {
        self.state().to_json()
    }

    /// 校验 token（常量时间比较；token 缺失一律拒绝）。
    pub fn token_matches(&self, candidate: &str) -> bool {
        let guard = match self.token.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.as_deref() {
            Some(expected) => constant_time_eq(expected.as_bytes(), candidate.as_bytes()),
            None => false,
        }
    }

    /// 注入事件管线（预热完成后；WS 订阅/补读用）。
    pub fn install_pipeline(&self, pipeline: Arc<dyn EventFeed>) {
        let _ = self.pipeline.set(pipeline);
    }

    /// 事件管线（未预热完成 → `None`）。
    pub fn pipeline(&self) -> Option<Arc<dyn EventFeed>> {
        self.pipeline.get().cloned()
    }

    /// CORS 单一来源（仅测试；生产默认 `None` = 拒绝）。
    pub fn cors_origin(&self) -> Option<&str> {
        self.cors_origin.as_deref()
    }

    /// 并发闸门（最大 64；超过 → 429）。
    pub fn try_acquire(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        Arc::clone(&self.permits).try_acquire_owned().ok()
    }

    /// 速率限制（100 req/s，突发 200）。
    pub fn allow_rate(&self) -> bool {
        match self.rate.lock() {
            Ok(mut guard) => guard.allow(),
            Err(poisoned) => poisoned.into_inner().allow(),
        }
    }

    /// 登记在途请求（返回取消标志）。
    pub(crate) fn begin_request(&self, key: &str) -> Arc<InFlight> {
        let entry = Arc::new(InFlight::default());
        if key.is_empty() {
            return entry;
        }
        match self.in_flight.lock() {
            Ok(mut guard) => {
                guard.insert(key.to_owned(), Arc::clone(&entry));
            }
            Err(poisoned) => {
                poisoned
                    .into_inner()
                    .insert(key.to_owned(), Arc::clone(&entry));
            }
        }
        entry
    }

    /// 结束在途请求（幂等）。
    pub fn finish_request(&self, key: &str) {
        if key.is_empty() {
            return;
        }
        match self.in_flight.lock() {
            Ok(mut guard) => {
                guard.remove(key);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove(key);
            }
        }
    }

    /// 取消在途请求（未知 id → `false`；ADR-033 D-033-3）。
    pub fn cancel_request(&self, key: &str) -> bool {
        if key.is_empty() {
            return false;
        }
        let entry = match self.in_flight.lock() {
            Ok(mut guard) => guard.remove(key),
            Err(poisoned) => poisoned.into_inner().remove(key),
        };
        match entry {
            Some(entry) => {
                entry.canceled.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// 关闭信号（WS 1012 / HTTP graceful shutdown）。
    pub fn shutdown_sender(&self) -> watch::Sender<bool> {
        self.shutdown_tx.clone()
    }

    /// 关闭订阅端。
    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// 写入发现文件（仅 HTTP 开启时；`0600`/用户 DACL）。
    pub fn install_discovery(&self, info: DiscoveryInfo) -> Result<(), String> {
        let token = match self.token.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let token = token.ok_or_else(|| "API token 未初始化".to_owned())?;
        write_discovery(&info, &token)?;
        match self.discovery.lock() {
            Ok(mut guard) => *guard = Some(info),
            Err(poisoned) => *poisoned.into_inner() = Some(info),
        }
        Ok(())
    }

    /// 轮换 token（先原子写发现文件，成功后再替换内存 token；旧 token 立即失效）。
    pub fn rotate_token(&self) -> Result<String, String> {
        let info = match self.discovery.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let info = info.ok_or_else(|| "HTTP API 未监听，无法轮换 token".to_owned())?;
        let token = generate_token()?;
        write_discovery(&info, &token)?;
        match self.token.write() {
            Ok(mut guard) => *guard = Some(token.clone()),
            Err(poisoned) => *poisoned.into_inner() = Some(token.clone()),
        }
        Ok(token)
    }

    /// 删除发现文件（关闭/崩溃残留由下次启动覆盖）。
    pub fn remove_discovery(&self) {
        let info = match self.discovery.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(info) = info {
            let _ = std::fs::remove_file(&info.path);
        }
    }
}

/// 运行中的 HTTP API 句柄（stdio 与 `--http-only` 共用）。
pub struct ApiHandle {
    shared: Arc<ApiShared>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
    discovery_path: Option<PathBuf>,
}

impl ApiHandle {
    /// 共享状态（`health.api` / WS 管线注入）。
    pub fn shared(&self) -> Arc<ApiShared> {
        Arc::clone(&self.shared)
    }

    /// 关闭：停止接受（≤1s 由 server 实现）、等待在途（≤3s）、删除发现文件。
    pub fn shutdown(mut self, runtime: &tokio::runtime::Handle, budget: Duration) {
        // 先通知 WS 发送 1012，再触发 HTTP graceful shutdown。
        let _ = self.shared.shutdown_sender().send(true);
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        // 让 server task 完成 graceful shutdown（ADR：停止接受 ≤1s + 在途 ≤3s）。
        if let Some(mut join) = self.join.take() {
            let result = runtime.block_on(async { tokio::time::timeout(budget, &mut join).await });
            if result.is_err() {
                join.abort();
                tracing::warn!("HTTP/WS 关闭超出预算，已中止监听任务（daemon 退出优先）");
            }
        }
        self.shared.remove_discovery();
        if let Some(path) = &self.discovery_path {
            let _ = std::fs::remove_file(path);
        }
        self.shared.set_state(ApiState::Disabled);
    }
}

/// 启动 HTTP/WS 监听（绑定失败返回 `Err`；调用方决定是否致命）。
pub fn start(
    runtime: &tokio::runtime::Runtime,
    config: &ApiConfig,
    ipc: IpcState,
    shared: Arc<ApiShared>,
) -> Result<ApiHandle, String> {
    let addr = config.socket_addr();
    let attempts = if config.port == 0 { 3 } else { 1 };
    let mut last_error = String::new();
    let mut listener = None;
    for _ in 0..attempts {
        match runtime.block_on(tokio::net::TcpListener::bind(&addr)) {
            Ok(bound) => {
                listener = Some(bound);
                break;
            }
            Err(error) => last_error = format!("{addr}: {error}"),
        }
    }
    let listener = listener.ok_or_else(|| {
        if config.port == 0 {
            format!("随机端口绑定失败（已重试 3 次）：{last_error}")
        } else {
            format!("EADDRINUSE/绑定失败：{last_error}")
        }
    })?;
    let local = listener
        .local_addr()
        .map_err(|error| format!("读取监听地址失败：{error}"))?;
    let started_at = now_ms();
    let discovery_path = config.data_dir.join(DISCOVERY_FILE_NAME);
    let pid = std::process::id();
    let info = DiscoveryInfo {
        path: discovery_path.clone(),
        pid,
        port: local.port(),
        bind: config.bind.clone(),
        started_at,
    };
    shared.install_discovery(info)?;
    shared.set_state(ApiState::Listening {
        port: local.port(),
        since_ms: started_at,
    });

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_shared = Arc::clone(&shared);
    let join = runtime.spawn(http::serve(listener, ipc, server_shared, shutdown_rx));

    tracing::info!(
        bind = %config.bind,
        port = local.port(),
        discovery = %discovery_path.display(),
        "HTTP/WS API 已启动（loopback + token；stdio 通道不变）"
    );

    Ok(ApiHandle {
        shared,
        shutdown: Some(shutdown_tx),
        join: Some(join),
        discovery_path: Some(discovery_path),
    })
}

/// 业务分发 + `health.api` 注入（stdio 与 HTTP 共用同一口径）。
pub fn dispatch_command(
    state: &IpcState,
    api: &ApiShared,
    method: &str,
    params: Value,
) -> Result<Value, IpcError> {
    let mut value = state.dispatch(method, params)?;
    if method == "health" {
        if let Value::Object(object) = &mut value {
            object.insert("api".to_owned(), api.health_json());
        }
    }
    Ok(value)
}

/// 命令超时（毫秒；M9 既有命令缺省 30s；新命令接入时在此登记）。
pub fn command_timeout_ms(_method: &str) -> u64 {
    DEFAULT_COMMAND_TIMEOUT_MS
}

/// 生成 256-bit token（64 hex；`getrandom`）。
pub fn generate_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| format!("随机 token 生成失败：{error}"))?;
    Ok(hex::encode(bytes))
}

/// 常量时间字符串比较（避免 token 比较的早退侧信道）。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 写发现文件（Unix `0600`；Windows 继承当前用户数据目录 DACL）。
fn write_discovery(info: &DiscoveryInfo, token: &str) -> Result<(), String> {
    let value = json!({
        "v": 1,
        "pid": info.pid,
        "port": info.port,
        "bind": info.bind,
        "token": token,
        "started_at": info.started_at,
        "protocol": aether_rpc::PROTOCOL_VERSION,
    });
    let text = serde_json::to_vec_pretty(&value)
        .map_err(|error| format!("发现文件序列化失败：{error}"))?;
    if let Some(parent) = info.path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("数据目录不可创建（{}）：{error}", parent.display()))?;
    }
    write_private_file(&info.path, &text)
        .map_err(|error| format!("发现文件写入失败（{}）：{error}", info.path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    // 已存在文件不应用 `mode`（仅创建时生效）：写后强制 0600。
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Windows：文件继承数据目录（当前用户 profile）的默认 DACL；不额外放宽权限。
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

/// Unix epoch 毫秒。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_64_hex_and_constant_time_compare_rejects_mismatch() {
        let token = generate_token().expect("token");
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(constant_time_eq(token.as_bytes(), token.as_bytes()));
        assert!(!constant_time_eq(token.as_bytes(), b"short"));
        assert!(!constant_time_eq(b"aaaaaaaa", b"aaaaaaab"));
    }

    #[test]
    fn api_state_json_never_contains_token() {
        let shared = ApiShared::new(
            ApiState::Listening {
                port: 43123,
                since_ms: 1,
            },
            Some("secret".to_owned()),
            None,
        );
        let value = shared.health_json().to_string();
        assert!(value.contains("listening"));
        assert!(!value.contains("secret"));
    }

    #[test]
    fn rate_bucket_allows_burst_then_limits() {
        let mut bucket = RateBucket::new();
        let mut allowed = 0;
        for _ in 0..(RATE_LIMIT_BURST as usize + 1) {
            if bucket.allow() {
                allowed += 1;
            }
        }
        assert_eq!(allowed, RATE_LIMIT_BURST as usize);
    }

    #[test]
    fn config_rejects_remote_bind_and_http_only_conflict() {
        let dir = std::env::temp_dir();
        // 环境变量在测试进程中共享：仅在缺失时设置/读取，避免并发影响。
        std::env::set_var("AETHERD_HTTP_BIND", "0.0.0.0");
        let error = ApiConfig::from_env(HttpMode::Default, false, dir.clone());
        std::env::remove_var("AETHERD_HTTP_BIND");
        assert!(error.is_err(), "非 loopback 必须拒绝");

        std::env::set_var("AETHERD_HTTP", "0");
        let error = ApiConfig::from_env(HttpMode::Enabled, true, dir.clone());
        std::env::remove_var("AETHERD_HTTP");
        assert!(error.is_err(), "--http-only 与 AETHERD_HTTP=0 冲突");

        std::env::set_var("AETHERD_HTTP", "0");
        let error = ApiConfig::from_env(HttpMode::Enabled, false, dir);
        std::env::remove_var("AETHERD_HTTP");
        assert!(error.is_err(), "--http 与 AETHERD_HTTP=0 冲突");
    }
}
