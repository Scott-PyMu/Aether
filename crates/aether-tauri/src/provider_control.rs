//! 模型与供应商配置命令后端（M3-11；ADR-010 决策 3/4；设计 D3/D7/D10）。
//!
//! 覆盖 D7 命令面供应商族：
//! - `providers_list`：供应商 + 模型清单（`api_key_ref` 引用呈现；**不含 `api_key` 本体**）；
//! - `provider_create` / `provider_update` / `provider_delete` / `provider_toggle`：
//!   供应商增改删与启停（内置不可删：`builtin_provider_undeletable`）；
//! - `provider_model_add` / `provider_model_toggle`：模型增改与启停（无删除命令）。
//!
//! 密钥写入路径（ADR-010 决策 3，D10）：表单 `api_key` 明文仅经本地 IPC 传输，核心经
//! [`aether_security::SecretStore`]（OS 凭据库；A3 降级走加密文件，M1-07 口径）写入
//! `keychain://aether/provider/<provider_id>`，命令响应只返回/落库 `api_key_ref`；
//! `api_key` 本体不落库、不进响应/日志/诊断包/导出（本模块不把明文写入任何 tracing 字段）。
//!
//! 单写者约束（AGENTS §2.4）：全部写路径经单写队列（[`WriteQueue::execute`]）。
//! 写命令的事务实现见 `aether_store::ops`（本层不复刻 SQL）。

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aether_security::{KeychainRef, SecretStore, SecretValue};
use aether_store::{
    ProviderModelRecord, ProviderRecord, ReadPool, StoreCommand, StoreError, StoreOutcome,
    WriteQueue,
};
use serde_json::{json, Value};

use crate::ipc::dto::{
    ProviderCreateRequest, ProviderDeleteRequest, ProviderModelAddRequest,
    ProviderModelToggleRequest, ProviderToggleRequest, ProviderUpdateRequest, ProvidersListRequest,
};
use crate::ipc::error::IpcError;

/// 供应商命令硬超时（本地存储写 + keyring 调用；无网络路径）。
pub const PROVIDER_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// 降级加密文件路径（A3；正常路径不创建：仅 keyring 不可用且提供口令时挂载）。
pub const SECRETS_FILE_NAME: &str = "secrets.enc";
/// 降级口令环境钩子（P0 无口令 UI；测试/演练专用，登记于 M3-11 证据）。
///
/// 未设置时：keyring 不可用 → 安全存储不挂载，密钥字段命令返回 `internal` 诊断
/// （ADR-010 §6-2：不得明文落库；配置页其余功能可用）。
pub const SECRETS_PASSPHRASE_ENV: &str = "AETHER_SECRETS_PASSPHRASE";

/// 供应商密钥引用命名空间：`keychain://aether/provider/<provider_id>`（ADR-010 决策 3）。
pub fn provider_key_ref(provider_id: &str) -> Result<KeychainRef, IpcError> {
    KeychainRef::new("provider", provider_id)
        .map_err(|error| IpcError::internal(format!("供应商密钥引用生成失败：{error}")))
}

/// 供应商密钥引用 URI（响应/落库用的 `api_key_ref` 形态）。
pub fn provider_key_ref_uri(provider_id: &str) -> Result<String, IpcError> {
    Ok(provider_key_ref(provider_id)?.to_uri())
}

/// 启动时选择密钥存储（D10/A3）：keyring 自检通过 → OS 凭据库；否则在提供降级口令
/// （[`SECRETS_PASSPHRASE_ENV`]）时挂载 A3 加密文件；两者皆不可用 → `None`
/// （密钥字段命令回诊断错误，明文不落库）。
pub fn boot_secret_store(data_dir: &Path) -> Option<Arc<dyn SecretStore>> {
    let keyring = aether_security::KeyringStore::new();
    if aether_security::self_check(&keyring).is_ok() {
        tracing::info!("安全存储已挂载：OS 凭据库（供应商密钥写入路径，D10）");
        return Some(Arc::new(keyring));
    }
    let passphrase = std::env::var(SECRETS_PASSPHRASE_ENV)
        .ok()
        .filter(|value| !value.is_empty())?;
    let file = aether_security::EncryptedFileStore::open_or_create(
        &data_dir.join(SECRETS_FILE_NAME),
        &SecretValue::new(passphrase),
        aether_security::FileCryptoParams::a3(),
    )
    .ok()?;
    if aether_security::self_check(&file).is_ok() {
        tracing::info!("安全存储已挂载：A3 降级加密文件（供应商密钥写入路径）");
        Some(Arc::new(file))
    } else {
        tracing::warn!("A3 降级加密文件自检失败：供应商密钥字段命令回诊断错误（不落库）");
        None
    }
}

/// 供应商命令执行体（由 [`crate::session_backend::SessionBackend`] 持有并转发）。
pub struct ProviderControl {
    reads: ReadPool,
    write: WriteQueue,
    /// 密钥存储（`None` = keyring 不可用且 A3 降级不可用：密钥字段命令回诊断错误）。
    secrets: Option<Arc<dyn SecretStore>>,
    handle: tokio::runtime::Handle,
    timeout: Duration,
}

impl ProviderControl {
    pub fn new(
        reads: ReadPool,
        write: WriteQueue,
        secrets: Option<Arc<dyn SecretStore>>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            reads,
            write,
            secrets,
            handle,
            timeout: PROVIDER_COMMAND_TIMEOUT,
        }
    }

    /// 覆盖命令超时（测试/故障注入）。
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 同步桥接：spawn 到核心运行时并等待（与 `SessionBackend::call` 同口径）。
    fn call<T, F>(&self, future: F) -> Result<T, IpcError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, IpcError>> + Send + 'static,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.handle.spawn(async move {
            let _ = sender.send(future.await);
        });
        receiver.recv_timeout(self.timeout).map_err(|_| {
            IpcError::internal(format!(
                "供应商命令超时（>{:?}，核心未在预算内返回）",
                self.timeout
            ))
        })?
    }

    /// `providers_list`：供应商 + 模型清单（按 `created_at` 升序）。
    pub fn providers_list(&self, _request: &ProvidersListRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        self.call(async move {
            let providers = reads
                .providers()
                .await
                .map_err(|error| IpcError::internal(format!("供应商清单读取失败：{error}")))?;
            let models = reads
                .provider_models()
                .await
                .map_err(|error| IpcError::internal(format!("供应商模型读取失败：{error}")))?;
            let items: Vec<Value> = providers
                .iter()
                .map(|provider| {
                    let own: Vec<ProviderModelRecord> = models
                        .iter()
                        .filter(|model| model.provider_id == provider.id)
                        .cloned()
                        .collect();
                    provider_json(provider, &own)
                })
                .collect();
            Ok(json!({ "providers": items }))
        })
    }

    /// `provider_create`：新建供应商（`api_key` 非空 → 写密钥 → `api_key_ref`）。
    pub fn provider_create(&self, request: &ProviderCreateRequest) -> Result<Value, IpcError> {
        let write = self.write.clone();
        let secrets = self.secrets.clone();
        let id = ulid::Ulid::new().to_string();
        let name = request.name.clone();
        let provider_type = request.provider_type;
        let base_url = normalize_base_url(request.base_url.as_deref());
        let api_key = request.api_key.clone();
        let enabled = request.enabled;
        self.call(async move {
            // 密钥先写（失败 → 命令整体失败、不落库，ADR-010 附录 B.1）。
            let api_key_ref = match api_key.as_deref() {
                Some(value) if !value.is_empty() => {
                    let store = secrets.as_ref().ok_or_else(secrets_unavailable)?;
                    let reference = provider_key_ref(&id)?;
                    store
                        .set(&reference, &SecretValue::new(value))
                        .map_err(|error| {
                            IpcError::internal(format!("供应商密钥写入失败：{error}"))
                        })?;
                    Some(reference.to_uri())
                }
                _ => None,
            };
            let now = now_ms();
            let provider = ProviderRecord {
                id: id.clone(),
                name,
                provider_type: provider_type_str(provider_type).to_owned(),
                base_url,
                api_key_ref: api_key_ref.clone(),
                enabled,
                is_builtin: false,
                created_at: now,
                updated_at: now,
            };
            let outcome = write
                .execute(StoreCommand::InsertProvider {
                    provider: provider.clone(),
                })
                .await;
            match outcome {
                Ok(StoreOutcome::Applied { .. }) => Ok(provider_json(&provider, &[])),
                Ok(other) => {
                    // 落库未按预期成功：回滚已写密钥（尽力而为），不泄露明文。
                    rollback_secret(secrets.as_ref(), &id, api_key_ref.is_some());
                    Err(IpcError::internal(format!(
                        "供应商落库返回了非预期结果：{other:?}"
                    )))
                }
                Err(error) => {
                    rollback_secret(secrets.as_ref(), &id, api_key_ref.is_some());
                    Err(IpcError::internal(format!("供应商落库失败：{error}")))
                }
            }
        })
    }

    /// `provider_update`：整体更新（`type` 不可改；`api_key` 三态；`base_url`
    /// 缺省=不变、空串=清除）。
    pub fn provider_update(&self, request: &ProviderUpdateRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let write = self.write.clone();
        let secrets = self.secrets.clone();
        let id = request.id.clone();
        let name = request.name.clone();
        let base_url = request.base_url.clone();
        let api_key = request.api_key.clone();
        let enabled = request.enabled;
        self.call(async move {
            let existing = reads
                .provider(&id)
                .await
                .map_err(|error| IpcError::internal(format!("供应商读取失败：{error}")))?
                .ok_or_else(|| IpcError::provider_not_found(&id))?;

            // `api_key` 三态：缺省=不变、空串=清除（含 keychain 自身命名空间条目）、
            // 非空=覆盖（明文写 keyring/A3，响应只回引用）。
            let api_key_ref = match api_key.as_deref() {
                None => existing.api_key_ref.clone(),
                Some("") => {
                    delete_own_key(secrets.as_ref(), &id, existing.api_key_ref.as_deref());
                    None
                }
                Some(value) => {
                    let store = secrets.as_ref().ok_or_else(secrets_unavailable)?;
                    let reference = provider_key_ref(&id)?;
                    store
                        .set(&reference, &SecretValue::new(value))
                        .map_err(|error| {
                            IpcError::internal(format!("供应商密钥写入失败：{error}"))
                        })?;
                    Some(reference.to_uri())
                }
            };
            // `base_url`：缺省=不变、空串=清除、非空=设置（格式已在命令层校验）。
            let base_url = match base_url {
                None => existing.base_url.clone(),
                Some(value) if value.is_empty() => None,
                Some(value) => Some(value),
            };
            let updated = ProviderRecord {
                id: id.clone(),
                name,
                provider_type: existing.provider_type.clone(),
                base_url,
                api_key_ref,
                enabled,
                is_builtin: existing.is_builtin,
                created_at: existing.created_at,
                updated_at: now_ms(),
            };
            let outcome = write
                .execute(StoreCommand::UpdateProvider {
                    provider: updated.clone(),
                })
                .await
                .map_err(|error| IpcError::internal(format!("供应商更新落库失败：{error}")))?;
            if let StoreOutcome::Applied { affected } = outcome {
                if affected == 0 {
                    return Err(IpcError::internal("供应商更新未影响任何行（不变量破坏）"));
                }
            }
            let models = reads
                .provider_models()
                .await
                .map_err(|error| IpcError::internal(format!("供应商模型读取失败：{error}")))?
                .into_iter()
                .filter(|model| model.provider_id == id)
                .collect::<Vec<_>>();
            Ok(provider_json(&updated, &models))
        })
    }

    /// `provider_delete`：删除供应商（内置硬拒绝；级联模型；keychain 删除限自身命名空间）。
    pub fn provider_delete(&self, request: &ProviderDeleteRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let write = self.write.clone();
        let secrets = self.secrets.clone();
        let id = request.id.clone();
        self.call(async move {
            let existing = reads
                .provider(&id)
                .await
                .map_err(|error| IpcError::internal(format!("供应商读取失败：{error}")))?
                .ok_or_else(|| IpcError::provider_not_found(&id))?;
            if existing.is_builtin {
                return Err(IpcError::builtin_provider_undeletable(&id));
            }
            let outcome = write
                .execute(StoreCommand::DeleteProvider { id: id.clone() })
                .await
                .map_err(|error| IpcError::internal(format!("供应商删除落库失败：{error}")))?;
            let deleted = match outcome {
                StoreOutcome::Applied { affected } => affected > 0,
                other => {
                    return Err(IpcError::internal(format!(
                        "供应商删除返回了非预期结果：{other:?}"
                    )))
                }
            };
            // keychain 条目删除规则（ADR-010 决策 3）：仅当 `api_key_ref` 命中自身
            // 命名空间才删除；共享/自定义引用不删；不可用/不存在 → 忽略 + 诊断，不阻断。
            delete_own_key(secrets.as_ref(), &id, existing.api_key_ref.as_deref());
            Ok(json!({ "deleted": deleted }))
        })
    }

    /// `provider_toggle`：快速启用/停用（内置可停用）。
    pub fn provider_toggle(&self, request: &ProviderToggleRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let write = self.write.clone();
        let id = request.id.clone();
        let enabled = request.enabled;
        self.call(async move {
            let existing = reads
                .provider(&id)
                .await
                .map_err(|error| IpcError::internal(format!("供应商读取失败：{error}")))?
                .ok_or_else(|| IpcError::provider_not_found(&id))?;
            let outcome = write
                .execute(StoreCommand::SetProviderEnabled {
                    id: id.clone(),
                    enabled,
                    updated_at: now_ms(),
                })
                .await
                .map_err(|error| IpcError::internal(format!("供应商启停落库失败：{error}")))?;
            if let StoreOutcome::Applied { affected } = outcome {
                if affected == 0 {
                    return Err(IpcError::internal("供应商启停未影响任何行（不变量破坏）"));
                }
            }
            Ok(json!({ "id": existing.id, "enabled": enabled }))
        })
    }

    /// `provider_model_add`：新增模型（默认启用；重复 → `invalid_value`）。
    pub fn provider_model_add(&self, request: &ProviderModelAddRequest) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let write = self.write.clone();
        let provider_id = request.provider_id.clone();
        let model_id = request.model_id.clone();
        let display_name = request.display_name.clone();
        self.call(async move {
            reads
                .provider(&provider_id)
                .await
                .map_err(|error| IpcError::internal(format!("供应商读取失败：{error}")))?
                .ok_or_else(|| IpcError::provider_not_found(&provider_id))?;
            let model = ProviderModelRecord {
                id: ulid::Ulid::new().to_string(),
                provider_id: provider_id.clone(),
                model_id: model_id.clone(),
                display_name,
                enabled: true,
                created_at: now_ms(),
            };
            let outcome = write
                .execute(StoreCommand::InsertProviderModel {
                    model: model.clone(),
                })
                .await;
            match outcome {
                Ok(StoreOutcome::Applied { affected }) if affected > 0 => Ok(model_json(&model)),
                Ok(other) => Err(IpcError::internal(format!(
                    "供应商模型落库返回了非预期结果：{other:?}"
                ))),
                Err(StoreError::DuplicateProviderModel {
                    provider_id,
                    model_id,
                }) => Err(IpcError::invalid_value(format!(
                    "模型已存在（provider_id={provider_id}, model_id={model_id}）；不重复添加"
                ))),
                Err(error) => Err(IpcError::internal(format!("供应商模型落库失败：{error}"))),
            }
        })
    }

    /// `provider_model_toggle`：模型启用/停用（不存在 → `provider_model_not_found`）。
    pub fn provider_model_toggle(
        &self,
        request: &ProviderModelToggleRequest,
    ) -> Result<Value, IpcError> {
        let reads = self.reads.clone();
        let write = self.write.clone();
        let provider_id = request.provider_id.clone();
        let model_id = request.model_id.clone();
        let enabled = request.enabled;
        self.call(async move {
            reads
                .provider(&provider_id)
                .await
                .map_err(|error| IpcError::internal(format!("供应商读取失败：{error}")))?
                .ok_or_else(|| IpcError::provider_not_found(&provider_id))?;
            reads
                .provider_model(&provider_id, &model_id)
                .await
                .map_err(|error| IpcError::internal(format!("供应商模型读取失败：{error}")))?
                .ok_or_else(|| IpcError::provider_model_not_found(&provider_id, &model_id))?;
            let outcome = write
                .execute(StoreCommand::SetProviderModelEnabled {
                    provider_id: provider_id.clone(),
                    model_id: model_id.clone(),
                    enabled,
                })
                .await
                .map_err(|error| IpcError::internal(format!("模型启停落库失败：{error}")))?;
            if let StoreOutcome::Applied { affected } = outcome {
                if affected == 0 {
                    return Err(IpcError::internal("模型启停未影响任何行（不变量破坏）"));
                }
            }
            Ok(json!({
                "provider_id": provider_id,
                "model_id": model_id,
                "enabled": enabled,
            }))
        })
    }
}

/// 供应商类型枚举 → 落库字符串（snake_case，与 ADR-010 附录 A 注释一致）。
fn provider_type_str(provider_type: crate::ipc::dto::ProviderType) -> &'static str {
    use crate::ipc::dto::ProviderType;
    match provider_type {
        ProviderType::Anthropic => "anthropic",
        ProviderType::Openai => "openai",
        ProviderType::Deepseek => "deepseek",
        ProviderType::Google => "google",
        ProviderType::Custom => "custom",
    }
}

/// `base_url` 缺省/空串统一为 `None`（创建路径；更新路径三态单独处理）。
fn normalize_base_url(value: Option<&str>) -> Option<String> {
    value.filter(|text| !text.is_empty()).map(str::to_owned)
}

/// 供应商清单元素形状（ADR-010 附录 B.1；**不含 `api_key` 本体**）。
fn provider_json(provider: &ProviderRecord, models: &[ProviderModelRecord]) -> Value {
    json!({
        "id": provider.id,
        "name": provider.name,
        "type": provider.provider_type,
        "base_url": provider.base_url,
        "api_key_ref": provider.api_key_ref,
        "enabled": provider.enabled,
        "is_builtin": provider.is_builtin,
        "created_at": provider.created_at,
        "updated_at": provider.updated_at,
        "models": models.iter().map(model_json).collect::<Vec<_>>(),
    })
}

/// 模型清单元素形状（ADR-010 附录 B.1）。
fn model_json(model: &ProviderModelRecord) -> Value {
    json!({
        "id": model.id,
        "model_id": model.model_id,
        "display_name": model.display_name,
        "enabled": model.enabled,
    })
}

/// 密钥存储未挂载（keyring 不可用且 A3 降级不可用；ADR-010 §6-2：不得明文落库）。
fn secrets_unavailable() -> IpcError {
    IpcError::internal(
        "安全存储不可用（OS 凭据库自检失败且未配置 A3 降级口令）；密钥不可写入、不得明文落库（D10）",
    )
}

/// 删除自身命名空间的 keychain 条目（仅当既有引用等于 `keychain://aether/provider/<id>`）。
///
/// 不可用/条目不存在 → 忽略并记诊断，**不阻断**命令本体（ADR-010 决策 3）。
fn delete_own_key(
    store: Option<&Arc<dyn SecretStore>>,
    provider_id: &str,
    existing_ref: Option<&str>,
) {
    let Some(store) = store else {
        tracing::warn!(
            provider_id = %provider_id,
            "安全存储未挂载：跳过 keychain 条目清理（不阻断命令；D10 不落明文）"
        );
        return;
    };
    let Ok(own_reference) = provider_key_ref(provider_id) else {
        return;
    };
    if existing_ref != Some(own_reference.to_uri().as_str()) {
        // 自定义/共享引用不删（避免破坏其他消费者）。
        return;
    }
    match store.delete(&own_reference) {
        Ok(()) => {}
        Err(error) if error.is_not_found() => {}
        Err(error) => {
            tracing::warn!(
                provider_id = %provider_id,
                error = %error,
                "keychain 条目清理失败（忽略并继续；命令本体不受影响）"
            );
        }
    }
}

/// 创建路径回滚：落库失败后删除刚写入的密钥（尽力而为；只在确有写入时调用）。
fn rollback_secret(store: Option<&Arc<dyn SecretStore>>, provider_id: &str, wrote: bool) {
    if !wrote {
        return;
    }
    let Some(store) = store else {
        return;
    };
    if let Ok(reference) = provider_key_ref(provider_id) {
        let _ = store.delete(&reference);
    }
}

fn now_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}
