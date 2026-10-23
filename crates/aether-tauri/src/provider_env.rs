//! 供应商消费映射 spawn 环境注入（M7-06/ADR-024 §2.1-1 / 附录 A-1）。
//!
//! 在适配器 spawn 前解析本运行时「所选 provider」并一次性注入环境变量：
//! - 运行时 → 供应商类型映射：`claude-code` → `anthropic`、`codex` → `openai`、
//!   `deepseek-harness` → `deepseek`（ADR-024 A-1）；
//! - 选择规则（实现口径，常量级；入任务证据）：满足映射的**启用** provider 中取
//!   `(updated_at, id)` 最大者——确定性；无命中 → 不注入（运行时按官方默认）；
//! - 密钥值经 `aether-security` 读取（D10；只经 env 一次性注入，不落盘、不日志）；
//! - 注入变量登记（任务证据）：
//!   - Claude Code：`ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL`（A-1 表内）；
//!   - Codex：`OPENAI_API_KEY` + `CODEX_HOME`（隔离，物化 `config.toml` 由适配器承担）
//!     + `AETHER_PROVIDER_ID` / `AETHER_PROVIDER_BASE_URL` / `AETHER_PROVIDER_ENV_KEY`；
//!   - DSH：`DEEPSEEK_API_KEY` + `AETHER_PROVIDER_ID` / `AETHER_PROVIDER_BASE_URL` /
//!     `AETHER_PROVIDER_ENV_KEY` / `AETHER_PROVIDER_MODELS`（profile provider 分层配置
//!     由适配器物化）。
//!
//! 失败隔离：未接线（存储降级启动）/ 读取失败 / 无密钥 → 记诊断告警并返回空集合，
//! **不阻断适配器启动**（`aether-adapters::SpawnEnvSource` 约定）。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use aether_adapters::supervisor::{SpawnEnvFuture, SpawnEnvSource};
use aether_security::{KeychainRef, SecretStore};
use aether_store::ReadPool;

use crate::credential_broker::BrokerInfo;

/// Claude Code 密钥环境变量（ADR-024 A-1）。
pub const ENV_ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
/// Claude Code Base URL 环境变量（ADR-024 A-1）。
pub const ENV_ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";
/// Codex 密钥环境变量（ADR-024 A-1：`OPENAI_API_KEY`）。
pub const ENV_OPENAI_API_KEY: &str = "OPENAI_API_KEY";
/// DSH 密钥环境变量（ADR-024 A-1：`DEEPSEEK_API_KEY` 等）。
pub const ENV_DEEPSEEK_API_KEY: &str = "DEEPSEEK_API_KEY";
/// Codex 隔离 home（物化 `config.toml` 于其内；ADR-024 §2.1-1）。
pub const ENV_CODEX_HOME: &str = "CODEX_HOME";
/// 消费映射辅助（非密钥）：供应商 id（Codex `model_providers` 键 / DSH provider 键）。
pub const ENV_AETHER_PROVIDER_ID: &str = "AETHER_PROVIDER_ID";
/// 消费映射辅助（非密钥）：供应商 Base URL。
pub const ENV_AETHER_PROVIDER_BASE_URL: &str = "AETHER_PROVIDER_BASE_URL";
/// 消费映射辅助（非密钥）：密钥环境变量名（Codex `env_key` / DSH `apiKeyEnv`）。
pub const ENV_AETHER_PROVIDER_ENV_KEY: &str = "AETHER_PROVIDER_ENV_KEY";
/// 消费映射辅助（非密钥）：模型清单 JSON（DSH provider 条目 `models`）。
pub const ENV_AETHER_PROVIDER_MODELS: &str = "AETHER_PROVIDER_MODELS";
/// ADR-028：本地凭据 broker 地址 + token（JSON `{endpoint,token}`；不含供应商密钥）。
pub const ENV_CREDENTIAL_BROKER: &str = "AETHER_CREDENTIAL_BROKER";
/// 消费映射辅助（非密钥）：默认模型 id（DSH acp 行 `model`；实测：缺省模型不设置时
/// DSH 不注册该 provider 的模型选项，M7-06 证据）。
pub const ENV_AETHER_PROVIDER_MODEL: &str = "AETHER_PROVIDER_MODEL";

/// 触发消费映射隔离的运行时 → 供应商类型映射（ADR-024 A-1）。
pub fn expected_provider_type(runtime_id: &str) -> Option<&'static str> {
    match runtime_id {
        "claude-code" => Some("anthropic"),
        "codex" => Some("openai"),
        "deepseek-harness" => Some("deepseek"),
        _ => None,
    }
}

/// ADR-028：运行时 → 协议映射（runtime → protocol 白名单/兼容表）。
///
/// 表为补偿措施，不改变“应用级共享 token”的信任模型；`None` = 非官方运行时。
#[must_use]
pub fn runtime_protocol(runtime_id: &str) -> Option<&'static str> {
    match runtime_id {
        "claude-code" => Some("anthropic"),
        "codex" => Some("openai"),
        "deepseek-harness" => Some("dsh"),
        _ => None,
    }
}

/// ADR-028：读取 provider 的协议端点（`{}`/缺失 → `None`，由调用方回退 `base_url`）。
fn protocol_endpoint(provider: &aether_store::ProviderRecord, protocol: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(&provider.endpoints).ok()?;
    parsed
        .get(protocol)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// ADR-028：运行时协议可用性（显式端点 / base_url / 官方默认可解析）。
pub(crate) fn provider_supports_protocol(
    provider: &aether_store::ProviderRecord,
    runtime_id: &str,
) -> bool {
    let Some(protocol) = runtime_protocol(runtime_id) else {
        return false;
    };
    resolve_protocol_base_url(provider, protocol).is_ok()
}

/// DSH provider 条目模型清单上限（env 体积护栏；常量级调参记任务证据）。
pub const DSH_MODELS_ENV_CAP: usize = 100;

/// 供应商消费映射 spawn 环境注入器（[`aether_adapters::supervisor::SpawnEnvSource`] 实现）。
///
/// 构造于监督器之前（注入点在启动序列尾段才会实际触发）；存储/密钥存储就绪后经
/// [`ProviderEnvHook::wire`] 注入——未接线时一律不注入（安全缺省）。
pub struct ProviderEnvHook {
    data_dir: PathBuf,
    reads: OnceLock<ReadPool>,
    secrets: OnceLock<Option<Arc<dyn SecretStore>>>,
    /// ADR-028：凭据 broker 地址/token（broker 启动后接线；未接线 = 旧单供应商路径）。
    broker: OnceLock<BrokerInfo>,
}

impl ProviderEnvHook {
    #[must_use]
    pub fn new(data_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            data_dir,
            reads: OnceLock::new(),
            secrets: OnceLock::new(),
            broker: OnceLock::new(),
        })
    }

    /// 存储/密钥存储就绪后接线（幂等：仅首次生效）。
    pub fn wire(&self, reads: ReadPool, secrets: Option<Arc<dyn SecretStore>>) {
        let _ = self.reads.set(reads);
        let _ = self.secrets.set(secrets);
    }

    /// ADR-028：broker 启动后注入地址/token（幂等：仅首次生效）。
    pub fn set_broker_info(&self, info: BrokerInfo) {
        let _ = self.broker.set(info);
    }

    /// Codex 隔离 home（消费映射命中时注入 `CODEX_HOME`）。
    fn codex_home(&self) -> PathBuf {
        self.data_dir.join("runtime-homes").join("codex")
    }

    async fn resolve(&self, runtime_id: &str) -> Vec<(String, String)> {
        let Some(_expected_type) = expected_provider_type(runtime_id) else {
            return Vec::new();
        };
        let Some(reads) = self.reads.get() else {
            tracing::warn!(
                runtime_id = %runtime_id,
                "供应商消费映射未接线（存储未就绪）：spawn 不注入（按官方默认）"
            );
            return Vec::new();
        };
        let providers = match reads.providers().await {
            Ok(providers) => providers,
            Err(error) => {
                tracing::warn!(error = %error, "供应商读取失败：spawn 不注入（按官方默认）");
                return Vec::new();
            }
        };
        // 选择规则（ADR-028 过渡）：启用 + 该运行时协议端点可用（显式 endpoints 或类型回退）
        // → `(updated_at, id)` 最大者（确定性）。显式 provider_id 的路由由 Broker 阶段替代。
        let selected = providers
            .iter()
            .filter(|provider| provider.enabled && provider_supports_protocol(provider, runtime_id))
            .max_by(|left, right| {
                (left.updated_at, left.id.as_str()).cmp(&(right.updated_at, right.id.as_str()))
            });
        let Some(provider) = selected else {
            return Vec::new();
        };
        let Some(api_key_ref) = provider
            .api_key_ref
            .as_deref()
            .filter(|reference| !reference.is_empty())
        else {
            tracing::warn!(
                provider_id = %provider.id,
                "所选供应商未配置密钥：spawn 不注入（按官方默认）"
            );
            return Vec::new();
        };
        let reference = match KeychainRef::parse(api_key_ref) {
            Ok(reference) => reference,
            Err(error) => {
                tracing::warn!(
                    provider_id = %provider.id,
                    error = %error,
                    "api_key_ref 不可解析：spawn 不注入（按官方默认）"
                );
                return Vec::new();
            }
        };
        let Some(Some(store)) = self.secrets.get() else {
            tracing::warn!(
                provider_id = %provider.id,
                "安全存储未挂载：spawn 不注入（按官方默认）"
            );
            return Vec::new();
        };
        let store = Arc::clone(store);
        let secret = match tokio::task::spawn_blocking(move || store.get(&reference)).await {
            Ok(Ok(secret)) => secret,
            Ok(Err(error)) => {
                tracing::warn!(
                    provider_id = %provider.id,
                    error = %error,
                    "供应商密钥读取失败：spawn 不注入（按官方默认）"
                );
                return Vec::new();
            }
            Err(error) => {
                tracing::warn!(
                    provider_id = %provider.id,
                    error = %error,
                    "供应商密钥读取任务失败：spawn 不注入（按官方默认）"
                );
                return Vec::new();
            }
        };
        let key = secret.expose().to_owned();
        let base_url = provider.base_url.clone();
        let provider_id = provider.id.clone();

        match runtime_id {
            "claude-code" => {
                let mut envs = vec![(ENV_ANTHROPIC_API_KEY.to_owned(), key)];
                if let Some(base_url) = protocol_endpoint(provider, "anthropic")
                    .or(base_url)
                    .filter(|value| !value.trim().is_empty())
                {
                    envs.push((ENV_ANTHROPIC_BASE_URL.to_owned(), base_url));
                }
                envs
            }
            "codex" => {
                let mut envs = vec![
                    (ENV_OPENAI_API_KEY.to_owned(), key),
                    (ENV_AETHER_PROVIDER_ID.to_owned(), provider_id),
                    (
                        ENV_AETHER_PROVIDER_ENV_KEY.to_owned(),
                        ENV_OPENAI_API_KEY.to_owned(),
                    ),
                ];
                if let Some(base_url) = protocol_endpoint(provider, "openai")
                    .or(base_url)
                    .filter(|value| !value.trim().is_empty())
                {
                    envs.push((ENV_AETHER_PROVIDER_BASE_URL.to_owned(), base_url));
                }
                let home = self.codex_home();
                if let Err(error) = std::fs::create_dir_all(&home) {
                    tracing::warn!(
                        dir = %home.display(),
                        error = %error,
                        "Codex 隔离 home 创建失败（适配器物化 config.toml 将失败）"
                    );
                }
                envs.push((
                    ENV_CODEX_HOME.to_owned(),
                    home.to_string_lossy().into_owned(),
                ));
                envs
            }
            "deepseek-harness" => {
                let mut envs = vec![
                    (ENV_DEEPSEEK_API_KEY.to_owned(), key),
                    (ENV_AETHER_PROVIDER_ID.to_owned(), provider_id.clone()),
                    (
                        ENV_AETHER_PROVIDER_ENV_KEY.to_owned(),
                        ENV_DEEPSEEK_API_KEY.to_owned(),
                    ),
                ];
                if let Some(base_url) = protocol_endpoint(provider, "dsh")
                    .or(base_url)
                    .filter(|value| !value.trim().is_empty())
                {
                    envs.push((ENV_AETHER_PROVIDER_BASE_URL.to_owned(), base_url));
                }
                // 模型清单（enabled；DSH provider 条目 `models`）——上限护栏，
                // 保持确定性顺序（model_id 字典序）；首个模型作为 acp 行缺省
                // （实测：acp 缺省模型缺失时 DSH 不注册 provider 模型选项）。
                if let Ok(models) = reads.provider_models().await {
                    let mut items: Vec<&aether_store::ProviderModelRecord> = models
                        .iter()
                        .filter(|model| model.provider_id == provider_id && model.enabled)
                        .collect();
                    items.sort_by(|left, right| left.model_id.cmp(&right.model_id));
                    items.truncate(DSH_MODELS_ENV_CAP);
                    if let Some(first) = items.first() {
                        envs.push((ENV_AETHER_PROVIDER_MODEL.to_owned(), first.model_id.clone()));
                    }
                    let payload: Vec<serde_json::Value> = items
                        .iter()
                        .map(|model| {
                            serde_json::json!({
                                "id": model.model_id,
                                "name": model.display_name,
                            })
                        })
                        .collect();
                    if let Ok(text) = serde_json::to_string(&payload) {
                        envs.push((ENV_AETHER_PROVIDER_MODELS.to_owned(), text));
                    }
                }
                envs
            }
            _ => Vec::new(),
        }
    }
}

/// ADR-028：broker 返回给适配器的单供应商凭据（只在当前 run 使用）。
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderCredential {
    pub base_url: Option<String>,
    pub api_key: String,
    pub auth_env: String,
}

/// `protocol` → 适配器子进程认证 env 名（派生字段，不进入目录响应）。
#[must_use]
pub fn auth_env_for(protocol: &str) -> Option<&'static str> {
    match protocol {
        "anthropic" => Some(ENV_ANTHROPIC_API_KEY),
        "openai" => Some(ENV_OPENAI_API_KEY),
        "dsh" => Some(ENV_DEEPSEEK_API_KEY),
        _ => None,
    }
}

/// ADR-028：协议端点解析（显式 endpoints 优先；其次 `base_url`；最后官方默认）。
///
/// - `base_url` 先去除尾部 `/`；
/// - DeepSeek 的 Anthropic 补全仅在路径未以 `/anthropic` 结尾时追加；
/// - 内置 OpenAI 历史 `base_url` 缺 `/v1` 且无显式 `endpoints.openai` → 补全；
/// - 其他类型（如 Google）没有统一官方默认：必须显式配置端点（含 `base_url`）。
pub fn resolve_protocol_base_url(
    provider: &aether_store::ProviderRecord,
    protocol: &str,
) -> Result<Option<String>, String> {
    if let Some(endpoint) = protocol_endpoint(provider, protocol) {
        return Ok(Some(endpoint.trim_end_matches('/').to_owned()));
    }
    let base = provider
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_end_matches('/').to_owned());
    // `base_url` 按类型/协议回退（其他类型如 Google 必须显式 `endpoints.<protocol>`，
    // 避免把 Gemini 原生根误当作 Anthropic/OpenAI 兼容端点）。
    let base_usable = match provider.provider_type.as_str() {
        "custom" => true,
        _ => matches!(
            (protocol, provider.provider_type.as_str()),
            ("anthropic", "anthropic" | "deepseek")
                | ("openai", "openai" | "deepseek")
                | ("dsh", "deepseek")
        ),
    };
    let base = match base {
        Some(_base) if !base_usable => None,
        Some(base) if protocol == "anthropic" && provider.provider_type == "deepseek" => {
            if base.ends_with("/anthropic") {
                Some(base)
            } else {
                Some(format!("{base}/anthropic"))
            }
        }
        Some(base) if protocol == "openai" => {
            // 内置 OpenAI 历史值缺 `/v1`（0003 seed 不改）：解析层补全，避免把主机根
            // 当作完整端点（ADR-028 §2.1-3）。
            if provider.provider_type == "openai"
                && base == "https://api.openai.com"
                && protocol_endpoint(provider, "openai").is_none()
            {
                Some(format!("{base}/v1"))
            } else {
                Some(base)
            }
        }
        other => other,
    };
    if base.is_some() {
        return Ok(base);
    }
    let default = match (protocol, provider.provider_type.as_str()) {
        ("anthropic", "anthropic") => Some("https://api.anthropic.com"),
        ("anthropic", "deepseek") => Some("https://api.deepseek.com/anthropic"),
        ("openai", "openai") => Some("https://api.openai.com/v1"),
        ("openai", "deepseek") => Some("https://api.deepseek.com"),
        ("dsh", "deepseek") => Some("https://api.deepseek.com"),
        _ => None,
    };
    match default {
        Some(default) => Ok(Some(default.to_owned())),
        None => Err(format!(
            "供应商 {} 未配置协议 {protocol} 的端点（其他类型必须显式配置 base_url/endpoints）",
            provider.id
        )),
    }
}

impl ProviderEnvHook {
    /// ADR-028：broker 是否已接线（未接线时 multi_provider 会话创建必须失败
    /// `core_not_ready`，不得回退单供应商 env）。
    #[must_use]
    pub fn broker_available(&self) -> bool {
        self.broker.get().is_some()
    }

    /// ADR-028：`session_create` 的可解析性预检（broker 可用、provider 存在/启用、
    /// 协议兼容、端点可解析、`api_key_ref` 存在且可读；读取后立即丢弃）。
    pub async fn preflight_credential(
        &self,
        provider_id: &str,
        protocol: &str,
    ) -> Result<(), String> {
        if !self.broker_available() {
            return Err(
                "凭据 broker 未接线（multi_provider 不可用；不回退单供应商 env）".to_owned(),
            );
        }
        self.resolve_credential(provider_id, protocol)
            .await
            .map(|_| ())
    }

    /// ADR-028：按 `provider_id + runtime_id` 读取单供应商凭据（broker 调用）。
    ///
    /// 校验 runtime → protocol 白名单与 provider → protocol 兼容性；不兼容 → 拒绝，
    /// 不返回 key。
    pub async fn resolve_credential_for_runtime(
        &self,
        provider_id: &str,
        runtime_id: &str,
        protocol: &str,
    ) -> Result<ProviderCredential, String> {
        let expected = runtime_protocol(runtime_id)
            .ok_or_else(|| format!("凭据 broker 请求方不是官方运行时：{runtime_id}"))?;
        if expected != protocol {
            return Err(format!(
                "运行时 {runtime_id} 与协议 {protocol} 不兼容（期望 {expected}）"
            ));
        }
        self.resolve_credential(provider_id, protocol).await
    }

    /// ADR-028：按 `provider_id + protocol` 读取单供应商凭据（broker 调用）。
    ///
    /// 密钥只在此调用期间进入内存；调用方负责写入当前 CLI 子进程 env，不缓存/不落盘。
    pub async fn resolve_credential(
        &self,
        provider_id: &str,
        protocol: &str,
    ) -> Result<ProviderCredential, String> {
        let reads = self
            .reads
            .get()
            .ok_or_else(|| "凭据 broker 未接线（存储未就绪）".to_owned())?;
        let provider = reads
            .provider(provider_id)
            .await
            .map_err(|error| format!("供应商读取失败：{error}"))?
            .ok_or_else(|| format!("供应商不存在：{provider_id}"))?;
        if !provider.enabled {
            return Err(format!("供应商已停用：{provider_id}"));
        }
        let base_url = resolve_protocol_base_url(&provider, protocol)?;
        let auth_env = auth_env_for(protocol)
            .ok_or_else(|| format!("未知协议：{protocol}"))?
            .to_owned();
        let api_key_ref = provider
            .api_key_ref
            .as_deref()
            .filter(|reference| !reference.is_empty())
            .ok_or_else(|| format!("供应商未配置密钥：{provider_id}"))?;
        let reference = KeychainRef::parse(api_key_ref)
            .map_err(|error| format!("api_key_ref 不可解析：{error}"))?;
        let store = match self.secrets.get() {
            Some(Some(store)) => Arc::clone(store),
            _ => return Err("安全存储未挂载".to_owned()),
        };
        let secret = tokio::task::spawn_blocking(move || store.get(&reference))
            .await
            .map_err(|error| format!("密钥读取任务失败：{error}"))?
            .map_err(|error| format!("密钥读取失败：{error}"))?;
        Ok(ProviderCredential {
            base_url,
            api_key: secret.expose().to_owned(),
            auth_env,
        })
    }
}

impl SpawnEnvSource for ProviderEnvHook {
    fn spawn_env<'a>(&'a self, runtime_id: &'a str) -> SpawnEnvFuture<'a> {
        Box::pin(async move {
            // ADR-028 v0.9：P1 三官方运行时永不进入 legacy 自动选择——无论 broker
            // 是否接线，都不再向官方适配器注入任何供应商密钥环境变量（broker 未接线
            // 时 multi_provider 会话创建直接 `core_not_ready`，不得回退单供应商 env）。
            let broker = self.broker.get();
            let mut envs = if runtime_protocol(runtime_id).is_some() {
                Vec::new()
            } else {
                self.resolve(runtime_id).await
            };
            if let Some(info) = broker {
                match serde_json::to_string(info) {
                    Ok(text) => envs.push((ENV_CREDENTIAL_BROKER.to_owned(), text)),
                    Err(error) => tracing::warn!(
                        error = %error,
                        "凭据 broker 信息序列化失败：不注入（适配器回退单供应商 env）"
                    ),
                }
            }
            envs
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn provider(
        provider_type: &str,
        base_url: Option<&str>,
        endpoints: &str,
        enabled: bool,
    ) -> aether_store::ProviderRecord {
        aether_store::ProviderRecord {
            id: "01J0000000000000000000T01".to_owned(),
            name: "测试供应商".to_owned(),
            provider_type: provider_type.to_owned(),
            base_url: base_url.map(str::to_owned),
            endpoints: endpoints.to_owned(),
            api_key_ref: None,
            enabled,
            is_builtin: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn runtime_protocol_table_is_static_and_official_only() {
        assert_eq!(runtime_protocol("claude-code"), Some("anthropic"));
        assert_eq!(runtime_protocol("codex"), Some("openai"));
        assert_eq!(runtime_protocol("deepseek-harness"), Some("dsh"));
        assert_eq!(runtime_protocol("mock"), None);
        assert_eq!(auth_env_for("anthropic"), Some(ENV_ANTHROPIC_API_KEY));
        assert_eq!(auth_env_for("openai"), Some(ENV_OPENAI_API_KEY));
        assert_eq!(auth_env_for("dsh"), Some(ENV_DEEPSEEK_API_KEY));
        assert_eq!(auth_env_for("unknown"), None);
    }

    #[test]
    fn endpoint_resolution_prefers_explicit_then_base_url_then_official_default() {
        // DeepSeek 显式 provider + {} endpoints → Claude Code Anthropic 默认端点。
        let deepseek = provider("deepseek", Some("https://api.deepseek.com"), "{}", true);
        assert_eq!(
            resolve_protocol_base_url(&deepseek, "anthropic").unwrap(),
            Some("https://api.deepseek.com/anthropic".to_owned())
        );
        // 显式端点优先于 base_url。
        let explicit = provider(
            "deepseek",
            Some("https://api.deepseek.com"),
            r#"{"anthropic":"https://gateway.example.com/anthropic/"}"#,
            true,
        );
        assert_eq!(
            resolve_protocol_base_url(&explicit, "anthropic").unwrap(),
            Some("https://gateway.example.com/anthropic".to_owned())
        );
        // 内置 OpenAI 历史值缺 /v1 且无显式端点 → 解析层补全。
        let openai = provider("openai", Some("https://api.openai.com"), "{}", true);
        assert_eq!(
            resolve_protocol_base_url(&openai, "openai").unwrap(),
            Some("https://api.openai.com/v1".to_owned())
        );
        // OpenAI 无 base_url → 官方默认。
        let openai_default = provider("openai", None, "{}", true);
        assert_eq!(
            resolve_protocol_base_url(&openai_default, "openai").unwrap(),
            Some("https://api.openai.com/v1".to_owned())
        );
        // 其他类型（Google）无统一默认 → 必须显式配置。
        let google = provider("google", None, "{}", true);
        assert!(resolve_protocol_base_url(&google, "anthropic").is_err());
        // Google 显式 OpenAI 兼容端点可解析。
        let google_openai = provider(
            "google",
            None,
            r#"{"openai":"https://generativelanguage.googleapis.com/v1beta/openai"}"#,
            true,
        );
        assert_eq!(
            resolve_protocol_base_url(&google_openai, "openai").unwrap(),
            Some("https://generativelanguage.googleapis.com/v1beta/openai".to_owned())
        );
        // provider → protocol 兼容性：Google 原生 base_url 不得当作 Anthropic 端点。
        let google_base = provider(
            "google",
            Some("https://generativelanguage.googleapis.com"),
            "{}",
            true,
        );
        assert!(!provider_supports_protocol(&google_base, "claude-code"));
    }

    #[test]
    fn spawn_env_with_broker_does_not_inject_any_provider_key() {
        let hook = ProviderEnvHook::new(std::path::PathBuf::from("."));
        hook.set_broker_info(BrokerInfo {
            endpoint: "test-endpoint".to_owned(),
            token: "test-token".to_owned(),
        });
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let envs = runtime.block_on(hook.spawn_env("claude-code"));
        assert_eq!(envs.len(), 1, "仅 broker env：{envs:?}");
        assert_eq!(envs[0].0, ENV_CREDENTIAL_BROKER);
        assert!(!envs[0].1.contains("test-key"));
    }
}
