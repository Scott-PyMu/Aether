//! ADR-028 v0.9：`provider.catalog.get` 只读目录数据源（adapter → core，D6）。
//!
//! 仅返回非机密字段：provider id/name/type/enabled/endpoints/models；**不返回**
//! `api_key`、`api_key_ref`、token，也不返回派生认证方式。目录变更后下一次查询即生效。
//! 不新增 IPC 命令、事件或错误码；命令面保持 63。

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use aether_adapters::connection::{CatalogError, ProviderCatalogSource};
use aether_store::ReadPool;
use serde_json::{json, Value};

/// 目录查询数据源（存储就绪前返回 `core_not_ready`；启动尾段接线）。
pub struct ProviderCatalog {
    reads: OnceLock<ReadPool>,
}

impl ProviderCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self {
            reads: OnceLock::new(),
        }
    }

    /// 存储就绪后接线（幂等：仅首次生效）。
    pub fn wire(&self, reads: ReadPool) {
        let _ = self.reads.set(reads);
    }
}

impl Default for ProviderCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderCatalogSource for ProviderCatalog {
    fn catalog<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CatalogError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(reads) = self.reads.get() else {
                return Err(CatalogError::internal("供应商目录未接线（存储未就绪）"));
            };
            let providers = reads
                .providers()
                .await
                .map_err(|error| CatalogError::internal(format!("供应商目录读取失败：{error}")))?;
            let models = reads.provider_models().await.map_err(|error| {
                CatalogError::internal(format!("供应商模型目录读取失败：{error}"))
            })?;
            let items: Vec<Value> = providers
                .iter()
                .map(|provider| {
                    let endpoints: Value =
                        serde_json::from_str(&provider.endpoints).unwrap_or_else(|_| json!({}));
                    let own_models: Vec<Value> = models
                        .iter()
                        .filter(|model| model.provider_id == provider.id)
                        .map(|model| {
                            json!({
                                "id": model.model_id,
                                "display_name": model.display_name,
                                "enabled": model.enabled,
                            })
                        })
                        .collect();
                    json!({
                        "id": provider.id,
                        "name": provider.name,
                        "type": provider.provider_type,
                        "enabled": provider.enabled,
                        "endpoints": endpoints,
                        "models": own_models,
                    })
                })
                .collect();
            Ok(json!({ "providers": items }))
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use aether_adapters::connection::ProviderCatalogSource;
    use aether_store::{
        ProviderModelRecord, ProviderRecord, StoreCommand, StoreRuntime, WriteQueueConfig,
    };

    #[test]
    fn catalog_is_non_secret_and_errors_before_wiring() {
        let catalog = ProviderCatalog::new();
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(catalog.catalog())
            .unwrap_err();
        assert_eq!(error.aether_code, "core_not_ready");

        let dir = tempfile::TempDir::new().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let storage = StoreRuntime::open(
            dir.path().join("aether.db"),
            WriteQueueConfig::default(),
            runtime.handle(),
        )
        .unwrap();
        let provider_id = "01J0000000000000000000CAT1";
        runtime
            .block_on(storage.queue().execute(StoreCommand::InsertProvider {
                provider: ProviderRecord {
                    id: provider_id.to_owned(),
                    name: "DeepSeek".to_owned(),
                    provider_type: "deepseek".to_owned(),
                    base_url: Some("https://api.deepseek.com".to_owned()),
                    endpoints: r#"{"anthropic":"https://api.deepseek.com/anthropic"}"#.to_owned(),
                    api_key_ref: Some("keychain://aether/provider/secret-reference".to_owned()),
                    enabled: true,
                    is_builtin: false,
                    created_at: 1,
                    updated_at: 1,
                },
            }))
            .unwrap();
        runtime
            .block_on(storage.queue().execute(StoreCommand::InsertProviderModel {
                model: ProviderModelRecord {
                    id: "01J0000000000000000000CAT2".to_owned(),
                    provider_id: provider_id.to_owned(),
                    model_id: "deepseek-flash".to_owned(),
                    display_name: "DeepSeek Flash".to_owned(),
                    enabled: true,
                    created_at: 1,
                },
            }))
            .unwrap();
        catalog.wire(storage.reads().clone());
        let value = runtime.block_on(catalog.catalog()).unwrap();
        let text = value.to_string();
        assert!(text.contains("deepseek-flash"));
        assert!(text.contains("anthropic"));
        assert!(!text.contains("api_key"));
        assert!(!text.contains("secret-reference"));
        assert!(!text.contains("token"));
    }
}
