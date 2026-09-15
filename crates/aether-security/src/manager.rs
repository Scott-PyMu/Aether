//! 安全级别判定、降级挂载与适配器密钥注入（设计 D10 / A3）。
//!
//! 启动顺序（[`SecurityManager::initialize`]）：
//! 1) OS 凭据库写→读→删自检；通过 → 安全级别：系统凭据库；
//! 2) 自检失败 → 必须提供口令，挂载 A3 加密文件（并在自检后标记降级）；
//! 3) 降级状态由 [`SecurityStatus`] 显式暴露，UI 必须展示 [`SecurityStatus::label_zh`]。

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::process::Command;

use crate::encrypted_file::{EncryptedFileStore, FileCryptoParams};
use crate::error::SecretError;
use crate::keyring_store::KeyringStore;
use crate::reference::KeychainRef;
use crate::store::{self_check, SecretStore, SecretValue, SecurityLevel};

/// 子进程 env 白名单（D10「子进程 env 白名单化」）。
///
/// 适配器 spawn 时只允许携带这些基础变量；密钥由
/// [`SecretEnv::apply_to_command`] 显式附加。
pub const BASE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "TEMP",
    "TMP",
    "TMPDIR",
    "HOME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "SHELL",
    "USER",
    "USERNAME",
    "LOGNAME",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "PROCESSOR_ARCHITECTURE",
];

/// 安全子系统启动配置。
pub struct SecurityConfig {
    /// 降级加密文件路径（正常路径不创建该文件）。
    pub secrets_file: PathBuf,
    /// 降级路径口令；正常路径忽略。
    pub passphrase: Option<SecretValue>,
    /// 加密文件 KDF 参数；默认 A3 冻结参数。
    pub crypto_params: FileCryptoParams,
    /// 演练/测试专用：跳过凭据库探测，强制走降级路径。
    ///
    /// 生产代码不得置为 `true`；只能降低安全级别、不能提升。
    pub force_keyring_unavailable: bool,
}

impl SecurityConfig {
    /// 以降级文件路径构造默认配置。
    #[must_use]
    pub fn new(secrets_file: PathBuf) -> Self {
        Self {
            secrets_file,
            passphrase: None,
            crypto_params: FileCryptoParams::a3(),
            force_keyring_unavailable: false,
        }
    }

    /// 设置降级口令。
    #[must_use]
    pub fn with_passphrase(mut self, passphrase: SecretValue) -> Self {
        self.passphrase = Some(passphrase);
        self
    }

    /// 覆盖 KDF 参数（仅测试/演练使用弱参数）。
    #[must_use]
    pub fn with_crypto_params(mut self, params: FileCryptoParams) -> Self {
        self.crypto_params = params;
        self
    }

    /// 强制降级（演练「凭据库不可用」路径）。
    #[must_use]
    pub fn force_degraded(mut self) -> Self {
        self.force_keyring_unavailable = true;
        self
    }
}

/// 安全级别状态（UI 展示与诊断导出共用）。
#[derive(Debug, Clone)]
pub struct SecurityStatus {
    level: SecurityLevel,
    detail: String,
}

impl SecurityStatus {
    fn os_keychain() -> Self {
        Self {
            level: SecurityLevel::OsKeychain,
            detail: "OS 凭据库自检通过".into(),
        }
    }

    fn degraded(detail: String) -> Self {
        Self {
            level: SecurityLevel::Degraded,
            detail,
        }
    }

    /// 安全级别枚举。
    #[must_use]
    pub fn level(&self) -> SecurityLevel {
        self.level
    }

    /// UI 展示文案（降级时为「安全级别：降级」）。
    #[must_use]
    pub fn label_zh(&self) -> &'static str {
        self.level.label_zh()
    }

    /// 降级/正常的补充说明（非机密）。
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// 供启动横幅使用的完整文案。
    #[must_use]
    pub fn banner_zh(&self) -> String {
        match self.level {
            SecurityLevel::OsKeychain => self.label_zh().to_string(),
            SecurityLevel::Degraded => format!(
                "{}（OS 凭据库不可用，密钥改存本地加密文件）",
                self.label_zh()
            ),
        }
    }
}

/// 密钥解析与适配器注入门面。
pub struct SecurityManager {
    store: Box<dyn SecretStore>,
    status: SecurityStatus,
}

impl SecurityManager {
    /// 启动初始化：凭据库自检 →（失败时）A3 加密文件挂载 + 自检。
    pub fn initialize(config: &SecurityConfig) -> Result<Self, SecretError> {
        if !config.force_keyring_unavailable {
            let keyring = KeyringStore::new();
            match self_check(&keyring) {
                Ok(()) => {
                    return Ok(Self {
                        store: Box::new(keyring),
                        status: SecurityStatus::os_keychain(),
                    });
                }
                Err(reason) => return Self::degraded(config, Some(reason)),
            }
        }
        Self::degraded(config, None)
    }

    fn degraded(config: &SecurityConfig, reason: Option<SecretError>) -> Result<Self, SecretError> {
        let passphrase = config
            .passphrase
            .as_ref()
            .ok_or(SecretError::PassphraseRequired)?;
        let file = EncryptedFileStore::open_or_create(
            &config.secrets_file,
            passphrase,
            config.crypto_params,
        )?;
        self_check(&file)?;
        let detail = match reason {
            Some(err) => err.to_string(),
            None => "演练：模拟 OS 凭据库不可用".to_string(),
        };
        Ok(Self {
            store: Box::new(file),
            status: SecurityStatus::degraded(detail),
        })
    }

    /// 当前安全级别状态。
    #[must_use]
    pub fn status(&self) -> &SecurityStatus {
        &self.status
    }

    /// 当前安全级别。
    #[must_use]
    pub fn level(&self) -> SecurityLevel {
        self.status.level
    }

    /// 底层存储（权限网关等内部的受控访问点）。
    #[must_use]
    pub fn store(&self) -> &dyn SecretStore {
        self.store.as_ref()
    }

    /// 解析单个引用。
    pub fn resolve(&self, reference: &KeychainRef) -> Result<SecretValue, SecretError> {
        self.store.get(reference)
    }

    /// 按适配器裁剪后解析注入项（只解析计划内引用）。
    pub fn resolve_env(&self, plan: &AdapterSecretPlan) -> Result<SecretEnv, SecretError> {
        resolve_env(self.store.as_ref(), plan)
    }
}

/// 一条注入项：环境变量名 → 密钥引用。
#[derive(Debug, Clone)]
pub struct SecretEnvVar {
    /// 环境变量名（如 `ANTHROPIC_API_KEY`）。
    pub name: String,
    /// 密钥引用。
    pub reference: KeychainRef,
}

/// 某个适配器的注入计划——解析范围只含本适配器条目（D10：按适配器裁剪）。
#[derive(Debug, Clone)]
pub struct AdapterSecretPlan {
    adapter_id: String,
    vars: Vec<SecretEnvVar>,
}

impl AdapterSecretPlan {
    /// 新建某适配器的注入计划。
    #[must_use]
    pub fn new(adapter_id: impl Into<String>) -> Self {
        Self {
            adapter_id: adapter_id.into(),
            vars: Vec::new(),
        }
    }

    /// 追加一条注入项。
    #[must_use]
    pub fn with_var(mut self, name: impl Into<String>, reference: KeychainRef) -> Self {
        self.vars.push(SecretEnvVar {
            name: name.into(),
            reference,
        });
        self
    }

    /// 适配器 ID。
    #[must_use]
    pub fn adapter_id(&self) -> &str {
        &self.adapter_id
    }

    /// 注入项清单。
    #[must_use]
    pub fn vars(&self) -> &[SecretEnvVar] {
        &self.vars
    }
}

/// 解析完成的注入环境（值受 `SecretValue` 保护，`Debug` 只显示变量名）。
pub struct SecretEnv {
    adapter_id: String,
    vars: BTreeMap<String, SecretValue>,
}

impl SecretEnv {
    /// 适配器 ID。
    #[must_use]
    pub fn adapter_id(&self) -> &str {
        &self.adapter_id
    }

    /// 取某变量的密钥值。
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&SecretValue> {
        self.vars.get(name)
    }

    /// 变量名清单。
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.vars.keys().map(String::as_str)
    }

    /// 变量数量。
    #[must_use]
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// 一次性写入子进程 env（spawn 前调用；不做任何全局缓存）。
    pub fn apply_to_command(&self, command: &mut Command) {
        for (name, value) in &self.vars {
            command.env(name, value.expose());
        }
    }
}

impl fmt::Debug for SecretEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretEnv")
            .field("adapter_id", &self.adapter_id)
            .field("vars", &self.vars.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// 解析注入计划：逐条读取，未定义的引用直接报错（fail-closed）。
pub fn resolve_env(
    store: &dyn SecretStore,
    plan: &AdapterSecretPlan,
) -> Result<SecretEnv, SecretError> {
    validate_adapter_id(plan.adapter_id())?;
    let mut vars = BTreeMap::new();
    for entry in plan.vars() {
        validate_env_name(&entry.name)?;
        let value = store.get(&entry.reference)?;
        vars.insert(entry.name.clone(), value);
    }
    Ok(SecretEnv {
        adapter_id: plan.adapter_id().to_string(),
        vars,
    })
}

/// 只透传白名单内的基础环境变量（D10：子进程 env 白名单化）。
pub fn apply_base_env_allowlist<I>(command: &mut Command, vars: I)
where
    I: IntoIterator<Item = (String, String)>,
{
    for (name, value) in vars {
        if BASE_ENV_ALLOWLIST
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&name))
        {
            command.env(name, value);
        }
    }
}

fn validate_env_name(name: &str) -> Result<(), SecretError> {
    if name.is_empty() || name.len() > 128 {
        return Err(SecretError::InvalidEnvName(name.to_string()));
    }
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if first_ok && rest_ok {
        Ok(())
    } else {
        Err(SecretError::InvalidEnvName(name.to_string()))
    }
}

fn validate_adapter_id(adapter_id: &str) -> Result<(), SecretError> {
    if adapter_id.is_empty() || adapter_id.len() > 128 {
        return Err(SecretError::InvalidReference(
            "适配器 ID 不能为空且不超过 128 字符".into(),
        ));
    }
    let ok = adapter_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(SecretError::InvalidReference(
            "适配器 ID 只允许 ASCII 字母/数字与 - _ .".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::process::Command;
    use std::sync::Mutex;

    use super::{
        apply_base_env_allowlist, resolve_env, AdapterSecretPlan, SecurityConfig, SecurityLevel,
        SecurityManager, BASE_ENV_ALLOWLIST,
    };
    use crate::error::SecretError;
    use crate::reference::KeychainRef;
    use crate::store::{SecretStore, SecretValue};

    #[derive(Default)]
    struct MemoryStore {
        map: Mutex<HashMap<String, String>>,
    }

    impl SecretStore for MemoryStore {
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
            let removed = self.map.lock().unwrap().remove(&reference.to_uri());
            if removed.is_some() {
                Ok(())
            } else {
                Err(SecretError::NotFound {
                    reference: reference.clone(),
                })
            }
        }
    }

    fn store_with(entries: &[(&str, &str, &str)]) -> MemoryStore {
        let store = MemoryStore::default();
        for (service, key, value) in entries {
            let reference = KeychainRef::new(*service, *key).unwrap();
            store.set(&reference, &SecretValue::new(*value)).unwrap();
        }
        store
    }

    #[test]
    fn resolves_env_from_plan_only() {
        let store = store_with(&[
            ("adapter-a", "api-key", "unit-secret-value-adapter-a"),
            ("adapter-b", "api-key", "unit-secret-value-adapter-b"),
        ]);
        let plan_a = AdapterSecretPlan::new("adapter-a").with_var(
            "AETHER_A_TOKEN",
            KeychainRef::new("adapter-a", "api-key").unwrap(),
        );
        let plan_b = AdapterSecretPlan::new("adapter-b").with_var(
            "AETHER_B_TOKEN",
            KeychainRef::new("adapter-b", "api-key").unwrap(),
        );

        let env_a = resolve_env(&store, &plan_a).unwrap();
        let env_b = resolve_env(&store, &plan_b).unwrap();

        assert_eq!(env_a.adapter_id(), "adapter-a");
        assert_eq!(env_a.len(), 1);
        assert!(env_a.names().any(|name| name == "AETHER_A_TOKEN"));
        assert!(!env_a.names().any(|name| name == "AETHER_B_TOKEN"));
        assert_ne!(
            env_a.get("AETHER_A_TOKEN").unwrap().expose(),
            env_b.get("AETHER_B_TOKEN").unwrap().expose(),
            "适配器之间不得串用密钥"
        );
    }

    #[test]
    fn resolve_env_fails_closed_on_missing_reference() {
        let store = store_with(&[]);
        let plan = AdapterSecretPlan::new("adapter-a").with_var(
            "AETHER_A_TOKEN",
            KeychainRef::new("adapter-a", "absent").unwrap(),
        );
        let err = resolve_env(&store, &plan).unwrap_err();
        assert!(err.is_not_found());
    }

    #[test]
    fn rejects_illegal_env_names() {
        let store = store_with(&[("adapter-a", "api-key", "value")]);
        let reference = KeychainRef::new("adapter-a", "api-key").unwrap();
        for name in ["", "1KEY", "KEY-WITH-DASH", "KEY WITH SPACE"] {
            let plan = AdapterSecretPlan::new("adapter-a").with_var(name, reference.clone());
            assert!(matches!(
                resolve_env(&store, &plan).unwrap_err(),
                SecretError::InvalidEnvName(_)
            ));
        }
    }

    #[test]
    fn secret_env_debug_never_exposes_values() {
        let store = store_with(&[("adapter-a", "api-key", "unit-secret-value-debug")]);
        let plan = AdapterSecretPlan::new("adapter-a").with_var(
            "AETHER_A_TOKEN",
            KeychainRef::new("adapter-a", "api-key").unwrap(),
        );
        let env = resolve_env(&store, &plan).unwrap();
        let debug = format!("{env:?}");
        assert!(debug.contains("AETHER_A_TOKEN"));
        assert!(!debug.contains("unit-secret-value-debug"));
    }

    #[test]
    fn injects_env_into_spawned_child_process() {
        let secret = "tok-m1-07-inject-abcdefghijklmnop";
        let store = store_with(&[("adapter-a", "api-key", secret)]);
        let plan = AdapterSecretPlan::new("adapter-a").with_var(
            "AETHER_M1_07_TOKEN",
            KeychainRef::new("adapter-a", "api-key").unwrap(),
        );
        let env = resolve_env(&store, &plan).unwrap();

        let mut command = if cfg!(windows) {
            let mut command = Command::new("cmd");
            command.args(["/C", "echo %AETHER_M1_07_TOKEN%"]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", "printf %s \"$AETHER_M1_07_TOKEN\""]);
            command
        };
        env.apply_to_command(&mut command);
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), secret);
    }

    #[test]
    fn base_env_allowlist_filters_non_allowlisted_vars() {
        let mut command = Command::new("echo");
        apply_base_env_allowlist(
            &mut command,
            [
                ("PATH".to_string(), "/usr/bin".to_string()),
                (
                    "OPENAI_API_KEY".to_string(),
                    "SHOULD_NOT_PASS_VALUE".to_string(),
                ),
            ],
        );
        let envs: Vec<_> = command
            .get_envs()
            .map(|(key, _)| key.to_string_lossy().to_string())
            .collect();
        assert!(envs.iter().any(|name| name.eq_ignore_ascii_case("PATH")));
        assert!(!envs
            .iter()
            .any(|name| name.eq_ignore_ascii_case("OPENAI_API_KEY")));
        assert!(!BASE_ENV_ALLOWLIST.is_empty());
    }

    #[test]
    fn degraded_config_requires_passphrase() {
        let dir = std::env::temp_dir().join(format!("aether-m1-07-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = SecurityConfig::new(dir.join("secrets.enc"))
            .with_crypto_params(crate::encrypted_file::FileCryptoParams::test_only_fast())
            .force_degraded();
        let result = SecurityManager::initialize(&config);
        assert!(matches!(result, Err(SecretError::PassphraseRequired)));

        let config = config.with_passphrase(SecretValue::new("unit-passphrase"));
        let manager = SecurityManager::initialize(&config).unwrap();
        assert_eq!(manager.level(), SecurityLevel::Degraded);
        assert_eq!(manager.status().label_zh(), "安全级别：降级");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
