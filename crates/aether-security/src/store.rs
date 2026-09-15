//! 密钥存储抽象与启动自检（设计 D10 / A3）。
//!
//! 约定：
//! - 密钥值只存在于 [`SecretValue`]（内存驻留、`Debug` 恒为脱敏、随 `Drop` 清零）；
//! - 任何存储后端必须实现 [`SecretStore`]，上层只依赖该 trait（D10 失效条件中
//!   预留的 `SecretProvider` 替换位）；
//! - 启动自检固定执行「写入 → 读取 → 删除（并确认删除）」链路。

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

use crate::error::SecretError;
use crate::reference::KeychainRef;

/// 安全级别（决定 UI 展示，见 [`SecurityLevel::label_zh`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecurityLevel {
    /// 密钥存 OS 凭据库（正常路径）。
    OsKeychain,
    /// OS 凭据库不可用，密钥存 A3 加密文件（必须明示，不得静默）。
    Degraded,
}

impl SecurityLevel {
    /// UI 展示文案（M1-07 DoD3 冻结口径）。
    #[must_use]
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::OsKeychain => "安全级别：系统凭据库",
            Self::Degraded => "安全级别：降级",
        }
    }

    /// 是否处于降级路径。
    #[must_use]
    pub fn is_degraded(self) -> bool {
        matches!(self, Self::Degraded)
    }
}

/// 密钥值包装：内存驻留、随 `Drop` 归零、`Debug` 永不输出内容。
#[derive(Clone)]
pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    /// 包装明文值（调用方应确保来源同样受控）。
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// 取出明文引用——只在真正使用密钥的一瞬间调用（注入 env / 解密校验）。
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    /// 明文长度（会泄露长度信息，仅用于合法性检查）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 是否为空串。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 常量时间比较（长度不同直接判否；长度本身不视为机密）。
    #[must_use]
    pub fn constant_time_eq(&self, other: &Self) -> bool {
        constant_time_eq(self.expose().as_bytes(), other.expose().as_bytes())
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue([REDACTED])")
    }
}

// 仅用于「加密文件」的明文载荷序列化；禁止用于日志 / 配置 / 诊断导出。
impl Serialize for SecretValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.expose())
    }
}

impl<'de> Deserialize<'de> for SecretValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::new(String::deserialize(deserializer)?))
    }
}

/// 密钥存储后端抽象。
pub trait SecretStore: Send + Sync {
    /// 后端对应的安全级别。
    fn level(&self) -> SecurityLevel;
    /// 读取密钥。
    fn get(&self, reference: &KeychainRef) -> Result<SecretValue, SecretError>;
    /// 写入（覆盖）密钥。
    fn set(&self, reference: &KeychainRef, value: &SecretValue) -> Result<(), SecretError>;
    /// 删除密钥；不存在时返回 [`SecretError::NotFound`]。
    fn delete(&self, reference: &KeychainRef) -> Result<(), SecretError>;
}

/// 自检项 service 段。
pub const SELF_CHECK_SERVICE: &str = "aether-selfcheck";
/// 自检项 key 段。
pub const SELF_CHECK_KEY: &str = "startup-probe";

/// 自检使用的固定引用。
pub fn self_check_reference() -> Result<KeychainRef, SecretError> {
    KeychainRef::new(SELF_CHECK_SERVICE, SELF_CHECK_KEY)
}

/// 启动自检：写入随机探针 → 读取比对 → 删除 → 确认删除。
///
/// 任一步骤失败即返回错误（失败时尽力清理探针，清理失败不掩盖原始错误）。
pub fn self_check(store: &dyn SecretStore) -> Result<(), SecretError> {
    let reference = self_check_reference()?;
    let probe = SecretValue::new(format!("selfcheck-{}", random_hex(16)?));

    // 上次异常退出可能残留探测项：尽力清理后再写入。
    let _ = store.delete(&reference);

    store.set(&reference, &probe)?;

    let read = match store.get(&reference) {
        Ok(value) => value,
        Err(err) => {
            let _ = store.delete(&reference);
            return Err(err);
        }
    };
    let matched = read.constant_time_eq(&probe);
    drop(read);

    let deleted = store.delete(&reference);
    if !matched {
        return Err(SecretError::SelfCheck("写入后读取的值不一致"));
    }
    deleted?;

    match store.get(&reference) {
        Err(err) if err.is_not_found() => Ok(()),
        Ok(_) => Err(SecretError::SelfCheck("删除后仍能读取（存在残留）")),
        Err(err) => Err(err),
    }
}

pub(crate) fn random_bytes(buf: &mut [u8]) -> Result<(), SecretError> {
    getrandom::getrandom(buf).map_err(|_| SecretError::Random)
}

pub(crate) fn random_hex(byte_len: usize) -> Result<String, SecretError> {
    use std::fmt::Write as _;

    let mut buf = Zeroizing::new(vec![0u8; byte_len]);
    random_bytes(&mut buf)?;
    let mut out = String::with_capacity(byte_len * 2);
    for byte in buf.iter() {
        // String 的 fmt::Write 不会失败。
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::{self_check, SecretStore, SecretValue, SecurityLevel};
    use crate::error::SecretError;
    use crate::reference::KeychainRef;

    #[derive(Default)]
    struct MemoryStore {
        map: Mutex<HashMap<String, String>>,
        fail_next_set: Mutex<bool>,
        mismatch_on_get: Mutex<bool>,
    }

    impl SecretStore for MemoryStore {
        fn level(&self) -> SecurityLevel {
            SecurityLevel::OsKeychain
        }

        fn get(&self, reference: &KeychainRef) -> Result<SecretValue, SecretError> {
            let map = self.map.lock().unwrap();
            match map.get(&reference.to_uri()) {
                Some(_) if *self.mismatch_on_get.lock().unwrap() => {
                    Ok(SecretValue::new("corrupted"))
                }
                Some(value) => Ok(SecretValue::new(value.clone())),
                None => Err(SecretError::NotFound {
                    reference: reference.clone(),
                }),
            }
        }

        fn set(&self, reference: &KeychainRef, value: &SecretValue) -> Result<(), SecretError> {
            if *self.fail_next_set.lock().unwrap() {
                return Err(SecretError::KeychainUnavailable("injected failure".into()));
            }
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

    #[test]
    fn self_check_passes_and_leaves_no_residue() {
        let store = MemoryStore::default();
        self_check(&store).unwrap();
        assert!(store.map.lock().unwrap().is_empty());
    }

    #[test]
    fn self_check_detects_mismatch_and_cleans_up() {
        let store = MemoryStore::default();
        *store.mismatch_on_get.lock().unwrap() = true;
        let err = self_check(&store).unwrap_err();
        assert!(matches!(err, SecretError::SelfCheck(_)));
        assert!(store.map.lock().unwrap().is_empty(), "失败后必须清理探针");
    }

    #[test]
    fn self_check_propagates_set_failure() {
        let store = MemoryStore::default();
        *store.fail_next_set.lock().unwrap() = true;
        let err = self_check(&store).unwrap_err();
        assert!(matches!(err, SecretError::KeychainUnavailable(_)));
    }

    #[test]
    fn secret_value_debug_and_zeroize_wrapper() {
        let value = SecretValue::new("unit-secret-value");
        assert_eq!(format!("{value:?}"), "SecretValue([REDACTED])");
        assert!(!format!("{value:?}").contains("unit-secret-value"));
        assert!(value.constant_time_eq(&SecretValue::new("unit-secret-value")));
        assert!(!value.constant_time_eq(&SecretValue::new("other")));
        assert!(!value.constant_time_eq(&SecretValue::new("unit-secret-value2")));
    }

    #[test]
    fn security_level_labels_are_frozen() {
        assert_eq!(SecurityLevel::Degraded.label_zh(), "安全级别：降级");
        assert_eq!(SecurityLevel::OsKeychain.label_zh(), "安全级别：系统凭据库");
        assert!(SecurityLevel::Degraded.is_degraded());
    }

    #[test]
    fn random_hex_is_hex_and_varies() {
        let a = super::random_hex(16).unwrap();
        let b = super::random_hex(16).unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
