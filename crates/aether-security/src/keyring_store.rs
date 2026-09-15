//! OS 凭据库后端（设计 D10：keyring）。
//!
//! 平台映射：Windows Credential Manager / macOS Keychain / Linux Secret Service。
//! keyring service 名称 = `aether/<引用 service>`，account 名称 = `<引用 key>`。

use keyring::Entry;

use crate::error::SecretError;
use crate::reference::KeychainRef;
use crate::store::{SecretStore, SecretValue, SecurityLevel};

/// 所有条目共享的 keyring service 前缀（应用命名空间）。
pub const KEYRING_SERVICE_PREFIX: &str = "aether";

/// OS 凭据库存储后端。
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringStore;

impl KeyringStore {
    /// 构造后端（不触发任何凭据库调用；可用性由[`crate::self_check`]判定）。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl SecretStore for KeyringStore {
    fn level(&self) -> SecurityLevel {
        SecurityLevel::OsKeychain
    }

    fn get(&self, reference: &KeychainRef) -> Result<SecretValue, SecretError> {
        match entry(reference)?.get_password() {
            Ok(secret) => Ok(SecretValue::new(secret)),
            Err(keyring::Error::NoEntry) => Err(SecretError::NotFound {
                reference: reference.clone(),
            }),
            Err(err) => Err(SecretError::KeychainUnavailable(err.to_string())),
        }
    }

    fn set(&self, reference: &KeychainRef, value: &SecretValue) -> Result<(), SecretError> {
        entry(reference)?
            .set_password(value.expose())
            .map_err(|err| SecretError::KeychainUnavailable(err.to_string()))
    }

    fn delete(&self, reference: &KeychainRef) -> Result<(), SecretError> {
        match entry(reference)?.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Err(SecretError::NotFound {
                reference: reference.clone(),
            }),
            Err(err) => Err(SecretError::KeychainUnavailable(err.to_string())),
        }
    }
}

fn entry(reference: &KeychainRef) -> Result<Entry, SecretError> {
    Entry::new(
        &format!("{KEYRING_SERVICE_PREFIX}/{}", reference.service()),
        reference.key(),
    )
    .map_err(|err| SecretError::KeychainUnavailable(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{entry, KEYRING_SERVICE_PREFIX};
    use crate::reference::KeychainRef;

    #[test]
    fn entry_mapping_is_namespaced() {
        let reference = KeychainRef::new("adapter-codex", "api-key").unwrap();
        assert!(entry(&reference).is_ok());
        assert_eq!(KEYRING_SERVICE_PREFIX, "aether");
    }
}
