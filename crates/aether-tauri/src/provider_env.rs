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
}

impl ProviderEnvHook {
    #[must_use]
    pub fn new(data_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            data_dir,
            reads: OnceLock::new(),
            secrets: OnceLock::new(),
        })
    }

    /// 存储/密钥存储就绪后接线（幂等：仅首次生效）。
    pub fn wire(&self, reads: ReadPool, secrets: Option<Arc<dyn SecretStore>>) {
        let _ = self.reads.set(reads);
        let _ = self.secrets.set(secrets);
    }

    /// Codex 隔离 home（消费映射命中时注入 `CODEX_HOME`）。
    fn codex_home(&self) -> PathBuf {
        self.data_dir.join("runtime-homes").join("codex")
    }

    async fn resolve(&self, runtime_id: &str) -> Vec<(String, String)> {
        let Some(expected_type) = expected_provider_type(runtime_id) else {
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
        // 选择规则：启用 + 类型映射命中 → `(updated_at, id)` 最大者（确定性）。
        let selected = providers
            .iter()
            .filter(|provider| provider.enabled && provider.provider_type == expected_type)
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
                if let Some(base_url) = base_url.filter(|value| !value.trim().is_empty()) {
                    envs.push((ENV_ANTHROPIC_BASE_URL.to_owned(), base_url));
                }
                envs
            }
            "codex" => {
                let mut envs = vec![
                    (ENV_OPENAI_API_KEY.to_owned(), key),
                    (ENV_AETHER_PROVIDER_ID.to_owned(), provider_id),
                    (ENV_AETHER_PROVIDER_ENV_KEY.to_owned(), ENV_OPENAI_API_KEY.to_owned()),
                ];
                if let Some(base_url) = base_url.filter(|value| !value.trim().is_empty()) {
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
                    (ENV_AETHER_PROVIDER_ENV_KEY.to_owned(), ENV_DEEPSEEK_API_KEY.to_owned()),
                ];
                if let Some(base_url) = base_url.filter(|value| !value.trim().is_empty()) {
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
                        envs.push((
                            ENV_AETHER_PROVIDER_MODEL.to_owned(),
                            first.model_id.clone(),
                        ));
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

impl SpawnEnvSource for ProviderEnvHook {
    fn spawn_env<'a>(&'a self, runtime_id: &'a str) -> SpawnEnvFuture<'a> {
        Box::pin(async move { self.resolve(runtime_id).await })
    }
}
