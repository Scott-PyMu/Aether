//! `keychain://` 引用解析（设计 D10）。
//!
//! 引用语法冻结为：`keychain://aether/<service>/<key>`。
//! 配置文件只允许存引用，密钥值一律不落配置（AGENTS.md §2.9）。
//!
//! 安全注意：引用本身不是密钥，但其**非法输入**可能误把密钥值写进引用，
//! 因此解析错误信息不回显原始 URI。

use std::fmt;

use crate::error::SecretError;

/// 引用 scheme。
pub const KEYCHAIN_SCHEME: &str = "keychain";
/// 应用命名空间（`keychain://aether/...`）。
pub const KEYCHAIN_NAMESPACE: &str = "aether";
/// 完整前缀：`keychain://aether/`。
pub const KEYCHAIN_URI_PREFIX: &str = "keychain://aether/";
/// 单个路径段的最大长度（service / key）。
pub const MAX_SEGMENT_LEN: usize = 128;

/// 已校验的密钥引用：`keychain://aether/<service>/<key>`。
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeychainRef {
    service: String,
    key: String,
}

impl KeychainRef {
    /// 直接由 service / key 构造（等价于解析结果）。
    pub fn new(service: impl Into<String>, key: impl Into<String>) -> Result<Self, SecretError> {
        let service = service.into();
        let key = key.into();
        validate_segment("service", &service)?;
        validate_segment("key", &key)?;
        Ok(Self { service, key })
    }

    /// 解析 `keychain://aether/<service>/<key>`。
    ///
    /// 严格模式：scheme 与命名空间必须全小写精确匹配；不接受查询串、片段、
    /// 用户信息或百分号编码；必须恰为两段非空路径。
    pub fn parse(uri: &str) -> Result<Self, SecretError> {
        let rest = uri.strip_prefix(KEYCHAIN_URI_PREFIX).ok_or_else(|| {
            SecretError::InvalidReference(
                "引用必须以 keychain://aether/<service>/<key> 开头".into(),
            )
        })?;
        let mut segments = rest.split('/');
        let service = segments.next().unwrap_or("");
        let key = segments.next().unwrap_or("");
        if segments.next().is_some() {
            return Err(SecretError::InvalidReference(
                "引用路径段数必须恰为 service/key 两段".into(),
            ));
        }
        Self::new(service, key)
    }

    /// service 段（如适配器 ID）。
    #[must_use]
    pub fn service(&self) -> &str {
        &self.service
    }

    /// key 段（如 `api-key`）。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// 还原为标准引用字符串。
    #[must_use]
    pub fn to_uri(&self) -> String {
        format!("{KEYCHAIN_URI_PREFIX}{}/{}", self.service, self.key)
    }
}

impl fmt::Display for KeychainRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_uri())
    }
}

// 引用不是密钥，Debug 可展示完整 URI（与 SecretValue 相反）。
impl fmt::Debug for KeychainRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeychainRef({self})")
    }
}

fn validate_segment(kind: &'static str, value: &str) -> Result<(), SecretError> {
    if value.is_empty() {
        return Err(SecretError::InvalidReference(format!("{kind} 不能为空")));
    }
    if value.len() > MAX_SEGMENT_LEN {
        return Err(SecretError::InvalidReference(format!(
            "{kind} 超过 {MAX_SEGMENT_LEN} 字符上限"
        )));
    }
    if value == "." || value == ".." {
        return Err(SecretError::InvalidReference(format!(
            "{kind} 不允许为相对路径段"
        )));
    }
    let ok = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        return Err(SecretError::InvalidReference(format!(
            "{kind} 只允许 ASCII 字母/数字与 - _ ."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{KeychainRef, KEYCHAIN_URI_PREFIX, MAX_SEGMENT_LEN};

    #[test]
    fn parses_canonical_reference() {
        let r = KeychainRef::parse("keychain://aether/adapter-codex/api-key").unwrap();
        assert_eq!(r.service(), "adapter-codex");
        assert_eq!(r.key(), "api-key");
        assert_eq!(
            r.to_uri(),
            format!("{KEYCHAIN_URI_PREFIX}adapter-codex/api-key")
        );
    }

    #[test]
    fn constructs_and_roundtrips() {
        let r = KeychainRef::new("openai", "default").unwrap();
        assert_eq!(KeychainRef::parse(&r.to_uri()).unwrap(), r);
        assert_eq!(r.to_string(), "keychain://aether/openai/default");
    }

    #[test]
    fn rejects_malformed_references() {
        let cases = [
            "",
            "https://aether/x/y",
            "keychain://other/x/y",
            "keychain://aether",
            "keychain://aether/",
            "keychain://aether/only-service",
            "keychain://aether/svc/key/extra",
            "keychain://aether//key",
            "keychain://aether/svc/",
            "keychain://aether/svc/key?x=1",
            "keychain://aether/svc/..",
            "KEYCHAIN://aether/svc/key",
            "keychain://aether/svc/sk%2Dlive",
        ];
        for case in cases {
            assert!(
                KeychainRef::parse(case).is_err(),
                "应当拒绝的引用被接受：{case}"
            );
        }
    }

    #[test]
    fn rejects_illegal_characters_and_length() {
        assert!(KeychainRef::new("svc", "bad key").is_err());
        assert!(KeychainRef::new("svc", "bad/key").is_err());
        assert!(KeychainRef::new("svc", "中文").is_err());
        assert!(KeychainRef::new("svc", "x".repeat(MAX_SEGMENT_LEN + 1)).is_err());
        assert!(KeychainRef::new("", "key").is_err());
    }

    #[test]
    fn parse_error_never_echoes_input() {
        // 非法字符 '%'，且输入含密钥形态片段。
        let uri = "keychain://aether/svc/unit%2Dsecret-value";
        let err = KeychainRef::parse(uri).unwrap_err();
        assert!(!format!("{err}").contains("unit%2Dsecret"));
        assert!(!format!("{err}").contains("%2D"));
    }
}
