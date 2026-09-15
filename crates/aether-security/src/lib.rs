//! Aether 权限网关与密钥存储（设计 D9 / D10 / A3）。
//!
//! 依赖方向（AGENTS.md §2.1）：仅依赖 `aether-core`，禁止依赖其他内部 crate。
//! 本里程碑（M1-07）落地密钥侧：keyring 自检、`keychain://` 引用解析与注入、
//! 日志/诊断脱敏器、A3 降级加密文件；权限门自 M2-10 起落地。
//!
//! 安全边界（AGENTS.md §2.9）：
//! - 密钥值只经 [`SecretValue`] 传递（`Debug` 恒为 `[REDACTED]`、`Drop` 清零）；
//! - 引用 URI 可以进配置/日志（[`KeychainRef`]）；密钥值不可以；
//! - 适配器只按各自 [`AdapterSecretPlan`] 解析，互不可见。

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod encrypted_file;
mod error;
mod keyring_store;
mod manager;
mod redact;
mod reference;
mod store;

pub use encrypted_file::{
    EncryptedFileStore, FileCryptoParams, A3_M_COST_KIB, A3_P_COST, A3_T_COST,
};
pub use error::SecretError;
pub use keyring_store::{KeyringStore, KEYRING_SERVICE_PREFIX};
pub use manager::{
    apply_base_env_allowlist, resolve_env, AdapterSecretPlan, SecretEnv, SecretEnvVar,
    SecurityConfig, SecurityManager, SecurityStatus, BASE_ENV_ALLOWLIST,
};
pub use redact::{RedactKind, Redactor};
pub use reference::{
    KeychainRef, KEYCHAIN_NAMESPACE, KEYCHAIN_SCHEME, KEYCHAIN_URI_PREFIX, MAX_SEGMENT_LEN,
};
pub use store::{
    self_check, self_check_reference, SecretStore, SecretValue, SecurityLevel, SELF_CHECK_KEY,
    SELF_CHECK_SERVICE,
};

#[cfg(test)]
mod tests {
    #[test]
    fn security_depends_on_core_with_single_version_source() {
        assert_eq!(aether_core::version(), env!("CARGO_PKG_VERSION"));
    }
}
