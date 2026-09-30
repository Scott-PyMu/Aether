//! M3-11 集成测试：模型与供应商配置命令后端（ADR-010 决策 3/4；D3/D7/D10）。
//!
//! 覆盖实施计划 M3-11 DoD1–4 的 IPC 后端面：
//! - DoD1：迁移 0003 已应用（`schema_version = 3`；表/约束/播种行为见
//!   `aether-store/tests/m3_11_providers.rs`）；供应商写路径经单写队列（D3）；
//! - DoD2：命令矩阵（未知成员 / 非法 type / base_url 格式 / custom 缺 base_url /
//!   `api_key` >8192 / 重复 model_id）→ 结构化错误且不落库；
//! - DoD3：内置 `provider_delete` → `builtin_provider_undeletable`；内置可停用；
//!   `provider_not_found` / `provider_model_not_found` 分支；
//! - DoD4：`api_key` → `aether-security` 写密钥 → 响应/落库 `api_key_ref`；
//!   三态（缺省/空串/覆盖）；`providers_list` 含引用不含明文；keychain 删除限自身
//!   命名空间（共享引用不删；不可用忽略 + 不阻断）；IPC 响应/诊断导出明文 0 命中
//!   （`sk-`/`eyJ`/PEM 扫描）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aether_security::{
    KeychainRef, Redactor, SecretError, SecretStore, SecretValue, SecurityLevel,
};
use aether_store::{
    ProviderRecord, ReadPool, StoreCommand, StoreRuntime, WriteQueue, WriteQueueConfig,
};
use aether_tauri::core_health::{
    HealthProvider, StaticHealthSource, StaticRuntimeSummaries, StorageHealthSnapshot,
};
use aether_tauri::diagnostics_control::{DiagnosticsControlBackend, DiagnosticsDeps};
use aether_tauri::ipc::backend::{IpcBackend, NotImplementedBackend};
use aether_tauri::ipc::dto::{
    ExportDiagnosticsRequest, ProviderCreateRequest, ProviderDeleteRequest,
    ProviderModelAddRequest, ProviderModelToggleRequest, ProviderToggleRequest, ProviderType,
    ProviderUpdateRequest, ProvidersListRequest,
};
use aether_tauri::ipc::error::IpcErrorCode;
use aether_tauri::ipc::validate::parse_strict;
use aether_tauri::provider_control::{provider_key_ref, ProviderControl};
use aether_tauri::security_level::{SecurityLevelView, SecurityProbe, SECURITY_LEVEL_OS};
use aether_tauri::session_backend::SessionBackend;
use serde_json::{json, Value};
use tempfile::TempDir;

const BUILTIN_ANTHROPIC: &str = "01J00000000000000000000B01";
const BUILTIN_OPENAI: &str = "01J00000000000000000000B02";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 运行期生成的密钥形态样本（AGENTS §2.9：不把密钥字面量写进夹具）。
fn random_token(length: usize) -> String {
    let seed = format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut token = String::with_capacity(length);
    let bytes = seed.as_bytes();
    for index in 0..length {
        let byte = bytes[index % bytes.len()] as usize;
        token.push(alphabet[(byte + index * 7) % alphabet.len()] as char);
    }
    token
}

fn sample_api_key() -> String {
    format!("sk-ant-api03-{}", random_token(48))
}

fn sample_jwt() -> String {
    format!(
        "eyJhbGciOiJIUzI1NiJ9.{}.{}",
        random_token(40),
        random_token(32)
    )
}

fn sample_pem() -> String {
    let body = random_token(64);
    format!("-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----")
}

/// 证据归档（`AETHER_M3_11_EVIDENCE_DIR`；供 Gate 3 逐条出示）。
fn evidence(name: &str, value: &Value) {
    println!("[m3-11] 证据 {name} = {value}");
    let Some(dir) = std::env::var_os("AETHER_M3_11_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(text) = serde_json::to_string_pretty(value) else {
        return;
    };
    let _ = std::fs::write(dir.join(format!("{name}.json")), text);
}

/// CI 的 `%TEMP%` 可能含 8.3 短名；导出目标先 canonicalize 并去掉 `\\?\` 前缀
/// （同 `m3_05_diagnostics.rs` 口径）。
fn long_path(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).expect("canonicalize 夹具路径");
    let text = canonical.to_string_lossy().to_string();
    #[cfg(windows)]
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        return stripped.to_string();
    }
    text
}

/// 内存密钥存储替身（记录写入/删除；可注入 `set`/`delete` 失败）。
#[derive(Default)]
struct MemorySecretStore {
    entries: Mutex<BTreeMap<String, String>>,
    fail_set: AtomicBool,
    fail_delete: AtomicBool,
}

impl MemorySecretStore {
    fn contains(&self, reference: &KeychainRef) -> bool {
        match self.entries.lock() {
            Ok(guard) => guard.contains_key(&reference.to_uri()),
            Err(poisoned) => poisoned.into_inner().contains_key(&reference.to_uri()),
        }
    }

    fn len(&self) -> usize {
        match self.entries.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    fn insert_shared(&self, uri: &str, value: &str) {
        match self.entries.lock() {
            Ok(mut guard) => {
                guard.insert(uri.to_owned(), value.to_owned());
            }
            Err(poisoned) => {
                poisoned
                    .into_inner()
                    .insert(uri.to_owned(), value.to_owned());
            }
        }
    }
}

impl SecretStore for MemorySecretStore {
    fn level(&self) -> SecurityLevel {
        SecurityLevel::OsKeychain
    }

    fn get(&self, reference: &KeychainRef) -> Result<SecretValue, SecretError> {
        let guard = match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .get(&reference.to_uri())
            .map(|value| SecretValue::new(value.clone()))
            .ok_or_else(|| SecretError::NotFound {
                reference: reference.clone(),
            })
    }

    fn set(&self, reference: &KeychainRef, value: &SecretValue) -> Result<(), SecretError> {
        if self.fail_set.load(Ordering::SeqCst) {
            return Err(SecretError::KeychainUnavailable("注入失败".to_owned()));
        }
        match self.entries.lock() {
            Ok(mut guard) => {
                guard.insert(reference.to_uri(), value.expose().to_owned());
            }
            Err(poisoned) => {
                poisoned
                    .into_inner()
                    .insert(reference.to_uri(), value.expose().to_owned());
            }
        }
        Ok(())
    }

    fn delete(&self, reference: &KeychainRef) -> Result<(), SecretError> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(SecretError::KeychainUnavailable("注入失败".to_owned()));
        }
        let removed = match self.entries.lock() {
            Ok(mut guard) => guard.remove(&reference.to_uri()),
            Err(poisoned) => poisoned.into_inner().remove(&reference.to_uri()),
        };
        if removed.is_some() {
            Ok(())
        } else {
            Err(SecretError::NotFound {
                reference: reference.clone(),
            })
        }
    }
}

/// 固定安全级别探针（避免测试触碰真实 OS 凭据库）。
struct StaticSecurityProbe;

impl SecurityProbe for StaticSecurityProbe {
    fn status(&self) -> SecurityLevelView {
        SecurityLevelView {
            level: SECURITY_LEVEL_OS.to_owned(),
            detail: "注入：自检通过".to_owned(),
        }
    }
}

struct Harness {
    dir: TempDir,
    runtime: tokio::runtime::Runtime,
    reads: ReadPool,
    write: WriteQueue,
    secrets: Arc<MemorySecretStore>,
    backend: Arc<SessionBackend>,
    storage: Option<StoreRuntime>,
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

impl Harness {
    /// 打开存储 + 供应商后端（内存密钥存储）。
    fn open() -> Self {
        let dir = TempDir::new().expect("临时数据目录");
        let runtime = new_runtime();
        let handle = runtime.handle().clone();
        let storage = StoreRuntime::open(
            dir.path().join("aether.db"),
            WriteQueueConfig::default(),
            &handle,
        )
        .expect("打开存储运行时");
        let reads = storage.reads().clone();
        let write = storage.queue().clone();
        let secrets = Arc::new(MemorySecretStore::default());
        let backend = Arc::new(
            SessionBackend::new(
                Arc::new(NotImplementedBackend),
                None,
                None,
                Some(reads.clone()),
                None,
                handle.clone(),
            )
            .with_workspace_store(write.clone())
            .with_providers(ProviderControl::new(
                reads.clone(),
                write.clone(),
                Some(Arc::clone(&secrets) as Arc<dyn SecretStore>),
                handle,
            )),
        );
        Self {
            dir,
            runtime,
            reads,
            write,
            secrets,
            backend,
            storage: Some(storage),
        }
    }

    /// 未挂载密钥存储的变体（keyring 不可用且无 A3 降级口令）。
    fn open_without_secrets() -> Self {
        let mut harness = Self::open();
        let handle = harness.runtime.handle().clone();
        let backend = Arc::new(
            SessionBackend::new(
                Arc::new(NotImplementedBackend),
                None,
                None,
                Some(harness.reads.clone()),
                None,
                handle.clone(),
            )
            .with_workspace_store(harness.write.clone())
            .with_providers(ProviderControl::new(
                harness.reads.clone(),
                harness.write.clone(),
                None,
                handle,
            )),
        );
        harness.backend = backend;
        harness
    }

    fn list(&self) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.providers_list(&ProvidersListRequest {})
    }

    fn create(
        &self,
        name: &str,
        provider_type: ProviderType,
        base_url: Option<&str>,
        api_key: Option<&str>,
        enabled: bool,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.provider_create(&ProviderCreateRequest {
            name: name.to_owned(),
            provider_type,
            base_url: base_url.map(str::to_owned),
            api_key: api_key.map(str::to_owned),
            enabled,
        })
    }

    fn update(
        &self,
        id: &str,
        name: &str,
        base_url: Option<&str>,
        api_key: Option<&str>,
        enabled: bool,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.provider_update(&ProviderUpdateRequest {
            id: id.to_owned(),
            name: name.to_owned(),
            base_url: base_url.map(str::to_owned),
            api_key: api_key.map(str::to_owned),
            enabled,
        })
    }

    fn remove(&self, id: &str) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend
            .provider_delete(&ProviderDeleteRequest { id: id.to_owned() })
    }

    fn toggle(&self, id: &str, enabled: bool) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.provider_toggle(&ProviderToggleRequest {
            id: id.to_owned(),
            enabled,
        })
    }

    fn model_add(
        &self,
        provider_id: &str,
        model_id: &str,
        display_name: &str,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend.provider_model_add(&ProviderModelAddRequest {
            provider_id: provider_id.to_owned(),
            model_id: model_id.to_owned(),
            display_name: display_name.to_owned(),
        })
    }

    fn model_toggle(
        &self,
        provider_id: &str,
        model_id: &str,
        enabled: bool,
    ) -> Result<Value, aether_tauri::ipc::error::IpcError> {
        self.backend
            .provider_model_toggle(&ProviderModelToggleRequest {
                provider_id: provider_id.to_owned(),
                model_id: model_id.to_owned(),
                enabled,
            })
    }

    /// 直接登记供应商行（共享/自定义引用用例；绕过命令层）。
    fn insert_provider_direct(&self, provider: ProviderRecord) {
        self.runtime
            .block_on(
                self.write
                    .execute(StoreCommand::InsertProvider { provider }),
            )
            .expect("直接登记供应商行");
    }

    fn shutdown_storage(&mut self) {
        let storage = self.storage.take().expect("存储运行时句柄");
        self.runtime
            .block_on(storage.shutdown())
            .expect("存储关闭序列");
    }

    fn reopen(&mut self) {
        self.shutdown_storage();
        let dir = &self.dir;
        let handle = self.runtime.handle().clone();
        let storage = StoreRuntime::open(
            dir.path().join("aether.db"),
            WriteQueueConfig::default(),
            &handle,
        )
        .expect("重开存储运行时");
        let reads = storage.reads().clone();
        let write = storage.queue().clone();
        let secrets = Arc::clone(&self.secrets);
        let backend = Arc::new(
            SessionBackend::new(
                Arc::new(NotImplementedBackend),
                None,
                None,
                Some(reads.clone()),
                None,
                handle.clone(),
            )
            .with_workspace_store(write.clone())
            .with_providers(ProviderControl::new(
                reads.clone(),
                write.clone(),
                Some(secrets as Arc<dyn SecretStore>),
                handle,
            )),
        );
        self.reads = reads;
        self.write = write;
        self.backend = backend;
        self.storage = Some(storage);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(storage) = self.storage.take() {
            let _ = self.runtime.block_on(storage.shutdown());
        }
    }
}

/// DoD1：迁移 0003 已应用；内置 4 条播种形状；写路径经单写队列。
#[test]
fn dod1_builtin_seed_list_shape_and_write_queue() {
    let h = Harness::open();
    let summary = h.runtime.block_on(h.reads.store_summary()).expect("库摘要");
    assert_eq!(
        summary.schema_version,
        Some(3),
        "迁移必须到 0003（ADR-010）"
    );

    let list = h.list().expect("供应商清单");
    let providers = list["providers"].as_array().expect("providers 数组");
    assert_eq!(providers.len(), 4, "内置 4 条播种");
    for provider in providers {
        assert_eq!(provider["enabled"], false, "内置默认停用");
        assert_eq!(provider["is_builtin"], true);
        assert_eq!(provider["api_key_ref"], Value::Null);
        assert_eq!(provider["models"].as_array().map(Vec::len), Some(0));
    }
    assert_eq!(providers[0]["id"], BUILTIN_ANTHROPIC);
    assert_eq!(providers[0]["type"], "anthropic");
    assert_eq!(providers[0]["base_url"], "https://api.anthropic.com");

    // 创建经单写队列（提交计数 +1）。
    let committed_before = h.write.metrics().committed_entries;
    let created = h
        .create(
            "自定义",
            ProviderType::Custom,
            Some("https://api.example.com"),
            None,
            true,
        )
        .expect("创建供应商");
    assert_eq!(
        h.write.metrics().committed_entries,
        committed_before + 1,
        "供应商写路径必须经单写队列（D3）"
    );
    let keys: Vec<&str> = created
        .as_object()
        .expect("对象形状")
        .keys()
        .map(String::as_str)
        .collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        vec![
            "api_key_ref",
            "base_url",
            "created_at",
            "enabled",
            "id",
            "is_builtin",
            "models",
            "name",
            "type",
            "updated_at",
        ],
        "响应形状 = ADR-010 附录 B.1 列表元素"
    );
    assert_eq!(created["api_key_ref"], Value::Null, "无密钥 → 无引用");
    assert_eq!(created["models"].as_array().map(Vec::len), Some(0));

    let list = h.list().unwrap();
    assert_eq!(list["providers"].as_array().map(Vec::len), Some(5));

    evidence(
        "dod1_seed_list",
        &json!({
            "schema_version": summary.schema_version,
            "builtin_count": 4,
            "builtin_enabled": false,
            "committed_delta": 1,
            "created_id": created["id"],
        }),
    );
}

/// DoD2：命令矩阵（DTO 层畸形样本结构化错误 + 后端重复 model_id 不落库）。
#[test]
fn dod2_command_matrix_structured_errors_without_persistence() {
    let h = Harness::open();
    let provider_count_before = h.list().unwrap()["providers"]
        .as_array()
        .map(Vec::len)
        .unwrap();

    // DTO 层（命令层执行 parse_strict 的同一入口）。
    let samples: Vec<(Value, &str, Option<&str>)> = vec![
        (
            json!({
                "name": "x", "type": "custom", "base_url": "https://a.example.com",
                "enabled": true, "unexpected": 1
            }),
            "unknown_field",
            Some("unexpected"),
        ),
        (
            json!({ "name": "x", "type": "mistral", "base_url": "https://a.example.com", "enabled": true }),
            "invalid_enum",
            None,
        ),
        (
            json!({ "name": "x", "type": "custom", "base_url": "ftp://a.example.com", "enabled": true }),
            "invalid_format",
            Some("base_url"),
        ),
        (
            json!({ "name": "x", "type": "custom", "enabled": true }),
            "missing_field",
            Some("base_url"),
        ),
        (
            json!({
                "name": "x", "type": "custom", "base_url": "https://a.example.com",
                "api_key": "k".repeat(8193), "enabled": true
            }),
            "too_large",
            Some("api_key"),
        ),
    ];
    let mut codes = Vec::new();
    for (payload, code, field) in samples {
        let error = parse_strict::<ProviderCreateRequest>(payload.clone())
            .expect_err(&format!("样本必须拒绝：{payload}"));
        assert_eq!(error.code.as_str(), code, "样本 {payload}：{error}");
        if let Some(field) = field {
            assert_eq!(error.field.as_deref(), Some(field), "样本 {payload}");
        }
        codes.push(error.code.as_str());
    }

    // 后端层：重复 `(provider_id, model_id)` → invalid_value 且不新增行。
    let created = h
        .create(
            "重复模型",
            ProviderType::Custom,
            Some("https://api.example.com"),
            None,
            true,
        )
        .unwrap();
    let provider_id = created["id"].as_str().unwrap().to_owned();
    h.model_add(&provider_id, "deepseek-v4-pro", "DeepSeek V4 Pro")
        .expect("首次添加");
    let duplicate = h
        .model_add(&provider_id, "deepseek-v4-pro", "重名")
        .expect_err("重复模型必须拒绝");
    assert_eq!(duplicate.code, IpcErrorCode::InvalidValue, "{duplicate}");
    let list = h.list().unwrap();
    let provider = list["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == provider_id.as_str())
        .expect("供应商存在");
    assert_eq!(
        provider["models"].as_array().map(Vec::len),
        Some(1),
        "重复添加不得新增模型行"
    );

    assert_eq!(
        h.list().unwrap()["providers"].as_array().map(Vec::len),
        Some(provider_count_before + 1),
        "畸形样本不得落库"
    );

    evidence(
        "dod2_command_matrix",
        &json!({
            "dto_error_codes": codes,
            "duplicate_model_code": duplicate.code.as_str(),
            "model_count_after_duplicate": 1,
        }),
    );
}

/// DoD3：内置删除拒绝 / 内置可停用 / not_found 分支。
#[test]
fn dod3_builtin_constraints_and_not_found_branches() {
    let h = Harness::open();

    let delete_error = h.remove(BUILTIN_ANTHROPIC).expect_err("内置删除必须拒绝");
    assert_eq!(
        delete_error.code,
        IpcErrorCode::BuiltinProviderUndeletable,
        "{delete_error}"
    );
    assert_eq!(delete_error.code.as_str(), "builtin_provider_undeletable");
    assert_eq!(
        h.list().unwrap()["providers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["id"] == BUILTIN_ANTHROPIC)
            .count(),
        1,
        "拒绝后内置供应商仍在"
    );

    // 内置可停用/启用。
    let toggled = h.toggle(BUILTIN_OPENAI, true).expect("内置可启用");
    assert_eq!(toggled["enabled"], true);
    let toggled = h.toggle(BUILTIN_OPENAI, false).expect("内置可停用");
    assert_eq!(toggled["enabled"], false);

    // not_found 分支。
    let missing = "01J00000000000000000000ZZZ";
    assert_eq!(
        h.remove(missing).unwrap_err().code,
        IpcErrorCode::ProviderNotFound
    );
    assert_eq!(
        h.toggle(missing, true).unwrap_err().code,
        IpcErrorCode::ProviderNotFound
    );
    assert_eq!(
        h.update(missing, "x", None, None, true).unwrap_err().code,
        IpcErrorCode::ProviderNotFound
    );
    assert_eq!(
        h.model_add(missing, "m", "M").unwrap_err().code,
        IpcErrorCode::ProviderNotFound
    );
    assert_eq!(
        h.model_toggle(missing, "m", true).unwrap_err().code,
        IpcErrorCode::ProviderNotFound
    );
    let created = h
        .create(
            "not-found 夹具",
            ProviderType::Custom,
            Some("https://api.example.com"),
            None,
            true,
        )
        .unwrap();
    let provider_id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(
        h.model_toggle(&provider_id, "absent-model", true)
            .unwrap_err()
            .code,
        IpcErrorCode::ProviderModelNotFound
    );

    evidence(
        "dod3_builtin_constraints",
        &json!({
            "builtin_delete_code": delete_error.code.as_str(),
            "builtin_toggle_allowed": true,
            "not_found_code": IpcErrorCode::ProviderNotFound.as_str(),
            "model_not_found_code": IpcErrorCode::ProviderModelNotFound.as_str(),
        }),
    );
}

/// DoD1（补充）：供应商配置跨重启保留（重开存储后列表一致）。
#[test]
fn dod1_providers_persist_across_restart() {
    let mut h = Harness::open();
    h.create(
        "跨重启",
        ProviderType::Deepseek,
        Some("https://api.deepseek.com"),
        None,
        true,
    )
    .expect("创建供应商");
    h.model_add(
        "01J00000000000000000000B03",
        "deepseek-v4-pro",
        "DeepSeek V4 Pro",
    )
    .expect("内置供应商可加模型");
    let before = h.list().expect("重启前清单");
    h.reopen();
    let after = h.list().expect("重启后清单");
    assert_eq!(after, before, "重开存储后供应商/模型列表必须一致");
}

/// DoD4：密钥写入/引用/三态/命名空间删除规则/明文扫描。
#[test]
fn dod4_key_reference_tri_state_and_namespace_rules() {
    let h = Harness::open();
    let secret = sample_api_key();

    // 创建携带明文 → 写密钥存储 + 返回引用；响应不含明文。
    let created = h
        .create(
            "带密钥",
            ProviderType::Anthropic,
            Some("https://api.anthropic.com"),
            Some(&secret),
            true,
        )
        .expect("创建携带密钥");
    let provider_id = created["id"].as_str().unwrap().to_owned();
    let expected_ref = format!("keychain://aether/provider/{provider_id}");
    assert_eq!(created["api_key_ref"], expected_ref);
    let own_reference = provider_key_ref(&provider_id).unwrap();
    assert!(h.secrets.contains(&own_reference), "密钥必须写入密钥存储");
    assert_eq!(h.secrets.len(), 1, "仅写入自身命名空间一条条目");
    assert!(
        !created.to_string().contains(&secret),
        "响应不得包含 api_key 明文"
    );

    // providers_list 含引用、不含明文。
    let list_text = h.list().unwrap().to_string();
    assert!(list_text.contains(&expected_ref));
    assert!(!list_text.contains(&secret));
    assert!(!list_text.contains("api_key\""), "不得返回 api_key 字段");

    // 三态一：缺省 = 不变（引用与密钥均保留）。
    let updated = h
        .update(
            &provider_id,
            "带密钥·改名",
            Some("https://api.anthropic.com"),
            None,
            true,
        )
        .unwrap();
    assert_eq!(updated["api_key_ref"], expected_ref);
    assert!(h.secrets.contains(&own_reference));

    // 三态二：空串 = 清除（引用置空 + 删除自身命名空间条目）。
    let cleared = h
        .update(&provider_id, "带密钥·清除", None, Some(""), true)
        .unwrap();
    assert_eq!(cleared["api_key_ref"], Value::Null);
    assert!(
        !h.secrets.contains(&own_reference),
        "空串必须删除 keychain 条目"
    );

    // 三态三：非空 = 覆盖（同一引用写入新值）。
    let replaced = sample_api_key();
    let overwritten = h
        .update(&provider_id, "带密钥·覆盖", None, Some(&replaced), true)
        .unwrap();
    assert_eq!(overwritten["api_key_ref"], expected_ref);
    let stored = h.secrets.get(&own_reference).expect("覆盖后的密钥");
    assert!(stored.constant_time_eq(&SecretValue::new(&replaced)));

    // 共享/自定义引用不删：直接登记一行，引用指向自身命名空间之外。
    let shared_uri = "keychain://aether/shared-key/default";
    let shared_id = "01J00000000000000000000SH1";
    h.secrets.insert_shared(shared_uri, "shared-value");
    h.insert_provider_direct(ProviderRecord {
        id: shared_id.to_owned(),
        name: "共享引用".to_owned(),
        provider_type: "custom".to_owned(),
        base_url: Some("https://api.example.com".to_owned()),
        api_key_ref: Some(shared_uri.to_owned()),
        enabled: true,
        is_builtin: false,
        created_at: 1,
        updated_at: 1,
    });
    h.remove(shared_id).expect("删除共享引用供应商");
    let shared_reference = KeychainRef::parse(shared_uri).unwrap();
    assert!(
        h.secrets.contains(&shared_reference),
        "共享引用不得被删除（仅自身命名空间才删）"
    );

    // keychain 不可用/条目不存在 → 忽略 + 不阻断（删除仍成功）。
    h.secrets.fail_delete.store(true, Ordering::SeqCst);
    let delete_with_unavailable = h.remove(&provider_id).expect("keychain 不可用不阻断删除");
    assert_eq!(delete_with_unavailable["deleted"], true);
    h.secrets.fail_delete.store(false, Ordering::SeqCst);

    // 明文扫描：IPC 响应（含覆盖写入的新值）0 命中。
    let redactor = Redactor::new().expect("脱敏器");
    for value in [
        &created,
        &updated,
        &cleared,
        &overwritten,
        &delete_with_unavailable,
    ] {
        let text = value.to_string();
        assert!(redactor.is_clean(&text), "响应存在密钥模式命中：{text}");
        assert!(!text.contains(&secret) && !text.contains(&replaced));
    }

    evidence(
        "dod4_key_ref",
        &json!({
            "api_key_ref": expected_ref,
            "plaintext_in_response": false,
            "list_contains_ref": true,
            "plaintext_scan_clean": true,
        }),
    );
    evidence(
        "dod4_key_tristate",
        &json!({
            "absent_keeps_ref": updated["api_key_ref"],
            "empty_clears": cleared["api_key_ref"],
            "overwrite_keeps_ref": overwritten["api_key_ref"],
            "shared_reference_preserved": true,
            "keychain_unavailable_does_not_block": true,
        }),
    );
}

/// DoD4：密钥存储未挂载（keyring + A3 均不可用）→ 密钥写入回诊断错误、不落库；
/// 不带密钥配置仍可用（ADR-010 §6-2）。
#[test]
fn dod4_absent_secret_store_blocks_key_writes_without_persistence() {
    let h = Harness::open_without_secrets();
    let secret = sample_api_key();
    let error = h
        .create(
            "无安全存储",
            ProviderType::Custom,
            Some("https://api.example.com"),
            Some(&secret),
            true,
        )
        .expect_err("无安全存储时携带密钥必须拒绝");
    assert_eq!(error.code, IpcErrorCode::Internal, "{error}");
    assert!(
        !error.message.contains(&secret),
        "错误消息不得携带明文：{error}"
    );
    let list = h.list().unwrap();
    assert_eq!(
        list["providers"].as_array().map(Vec::len),
        Some(4),
        "失败命令不得落库（仍为 4 条内置）"
    );

    let ok = h
        .create(
            "无密钥可建",
            ProviderType::Custom,
            Some("https://api.example.com"),
            None,
            true,
        )
        .expect("不携带密钥的配置仍可用");
    assert_eq!(ok["api_key_ref"], Value::Null);

    evidence(
        "dod4_no_secret_store",
        &json!({
            "create_with_key_code": error.code.as_str(),
            "providers_after_failure": 4,
            "create_without_key": true,
        }),
    );
}

/// DoD4：诊断导出包明文 0 命中（`sk-`/`eyJ`/PEM 模式扫描）。
#[test]
fn dod4_diagnostics_bundle_scans_clean_after_provider_key_write() {
    let h = Harness::open();
    let secret = sample_api_key();
    h.create(
        "诊断扫描",
        ProviderType::Anthropic,
        Some("https://api.anthropic.com"),
        Some(&secret),
        true,
    )
    .expect("创建携带密钥");

    let target = h.dir.path().join("export");
    std::fs::create_dir_all(&target).expect("导出目录");
    let health = HealthProvider::new(
        Arc::new(StaticHealthSource::new(StorageHealthSnapshot {
            storage_state: "normal".to_owned(),
            write_queue_depth: 0,
            degrade_trigger: None,
            degraded_since_ms: None,
            detail: None,
        })),
        Arc::new(StaticRuntimeSummaries::wired(Vec::new())),
    );
    let deps = DiagnosticsDeps {
        data_dir: h.dir.path().to_path_buf(),
        reads: Some(h.reads.clone()),
        write: Some(h.write.clone()),
        handle: h.runtime.handle().clone(),
        health,
        logs: None,
        task_dumps: None,
        security: Some(Arc::new(StaticSecurityProbe)),
    };
    let inner: Arc<dyn IpcBackend> = Arc::clone(&h.backend) as Arc<dyn IpcBackend>;
    let diagnostics = DiagnosticsControlBackend::new(inner, deps);
    let request = ExportDiagnosticsRequest {
        target_dir: long_path(&target),
    };
    let canonical = request.canonical_target_dir().expect("目标校验");
    let result = diagnostics
        .export_diagnostics(&request, &canonical)
        .expect("导出诊断包");
    let path = PathBuf::from(result["path"].as_str().expect("导出路径"));
    let text = std::fs::read_to_string(&path).expect("读取诊断包");
    let redactor = Redactor::new().expect("脱敏器");
    // 扫描器正控校准：`eyJ`（JWT）与 PEM 规则确实生效，再对诊断包做负向断言。
    let control = format!("{} {}", sample_jwt(), sample_pem());
    assert!(
        !redactor.is_clean(&control),
        "扫描器正控校准失败（JWT/PEM 规则未生效）"
    );
    assert!(
        !text.contains(&secret),
        "诊断包不得包含 api_key 明文（{path:?}）"
    );
    assert!(redactor.is_clean(&text), "诊断包存在密钥模式命中");
    assert!(!text.contains("\"api_key\""), "诊断包不得包含 api_key 字段");

    evidence(
        "dod4_diagnostics_scan",
        &json!({
            "bundle": path.file_name().map(|name| name.to_string_lossy().to_string()),
            "plaintext_hit": false,
            "redactor_clean": true,
        }),
    );
}
