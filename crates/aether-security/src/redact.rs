//! 日志与诊断导出脱敏器（设计 D10）。
//!
//! 统一过滤：`sk-` 系 API Key、`eyJ` 系 JWT、PEM 私钥块，以及
//! Bearer / AWS / GitHub / Slack 等常见凭据形态。
//!
//! 约束：
//! - 输出中的替换标记**不含**任何触发前缀（如 `sk-` / `eyJ` / `PRIVATE KEY`），
//!   保证「脱敏后再扫描 0 命中」是可判定的；
//! - 脱敏器在日志写入与诊断导出两条链路前统一调用（M1-05 事件管线接入时复用本 API）。

use regex::Regex;
use serde_json::Value;

use crate::error::SecretError;

/// 命中类别（替换标记按类别区分，便于诊断定位而不泄露内容）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedactKind {
    /// `sk-` 前缀 API Key。
    ApiKey,
    /// `eyJ` 前缀 JWT。
    Jwt,
    /// PEM 私钥块（RSA/EC/OPENSSH/PKCS#8/PGP 等）。
    PemPrivateKey,
    /// `Bearer <token>`。
    BearerToken,
    /// AWS Access Key ID。
    AwsAccessKey,
    /// GitHub Token。
    GitHubToken,
    /// Slack Token。
    SlackToken,
}

impl RedactKind {
    /// 替换标记文本。
    #[must_use]
    pub fn marker(self) -> &'static str {
        match self {
            Self::ApiKey => "[REDACTED:api-key]",
            Self::Jwt => "[REDACTED:jwt]",
            Self::PemPrivateKey => "[REDACTED:pem-private-key]",
            Self::BearerToken => "[REDACTED:bearer-token]",
            Self::AwsAccessKey => "[REDACTED:aws-access-key]",
            Self::GitHubToken => "[REDACTED:github-token]",
            Self::SlackToken => "[REDACTED:slack-token]",
        }
    }
}

const PEM_PATTERN: &str = r"(?s)-----BEGIN[ A-Z0-9]*PRIVATE KEY[ A-Z0-9]*-----.*?(?:-----END[ A-Z0-9]*PRIVATE KEY[ A-Z0-9]*-----|$)";
const API_KEY_PATTERN: &str = r"sk-[A-Za-z0-9_-]{12,}";
const JWT_PATTERN: &str = r"eyJ[A-Za-z0-9_-]{6,}(?:\.[A-Za-z0-9_-]+)*";
const BEARER_PATTERN: &str = r"(?i)\bBearer\s+[A-Za-z0-9._~/+=-]{8,}";
const AWS_PATTERN: &str =
    r"\b(?:AKIA|ASIA|ABIA|ACCA|AGPA|AIDA|AIPA|ANPA|ANVA|APKA|AROA|ASCA)[A-Z0-9]{16}\b";
const GITHUB_PATTERN: &str =
    r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,}\b|\bgithub_pat_[A-Za-z0-9_]{22,}\b";
const SLACK_PATTERN: &str = r"\bxox[abpros]-[A-Za-z0-9-]{10,}\b";

const RULES: &[(&str, RedactKind)] = &[
    (PEM_PATTERN, RedactKind::PemPrivateKey),
    (API_KEY_PATTERN, RedactKind::ApiKey),
    (JWT_PATTERN, RedactKind::Jwt),
    (BEARER_PATTERN, RedactKind::BearerToken),
    (AWS_PATTERN, RedactKind::AwsAccessKey),
    (GITHUB_PATTERN, RedactKind::GitHubToken),
    (SLACK_PATTERN, RedactKind::SlackToken),
];

/// 脱敏器：编译期固定规则集，无状态、可跨线程共享。
pub struct Redactor {
    rules: Vec<(Regex, RedactKind)>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field(
                "rules",
                &self.rules.iter().map(|(_, k)| *k).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Redactor {
    /// 构造脱敏器（规则编译失败返回错误，绝不带病运行）。
    pub fn new() -> Result<Self, SecretError> {
        let mut rules = Vec::with_capacity(RULES.len());
        for (pattern, kind) in RULES {
            let regex = Regex::new(pattern).map_err(|_| SecretError::RedactorInit)?;
            rules.push((regex, *kind));
        }
        Ok(Self { rules })
    }

    /// 文本脱敏（日志行、stderr、事件载荷等）。
    #[must_use]
    pub fn redact(&self, input: &str) -> String {
        let mut out = input.to_string();
        for (regex, kind) in &self.rules {
            if regex.is_match(&out) {
                out = regex.replace_all(&out, kind.marker()).into_owned();
            }
        }
        out
    }

    /// JSON 递归脱敏（诊断导出、配置导出）。
    #[must_use]
    pub fn redact_json(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.redact(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.redact_json(item)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), self.redact_json(item)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// 是否已无任何命中（诊断导出前的守门断言；生产路径用 [`Self::redact`]）。
    #[must_use]
    pub fn is_clean(&self, input: &str) -> bool {
        self.rules.iter().all(|(regex, _)| !regex.is_match(input))
    }
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD};
    use base64::Engine as _;
    use serde_json::json;

    use super::{RedactKind, Redactor};

    // 样本一律运行期生成（AGENTS.md §2.9：不把密钥形态字面量写进测试夹具）。
    fn random_b64url(byte_len: usize) -> String {
        let mut bytes = vec![0u8; byte_len];
        getrandom::getrandom(&mut bytes).unwrap();
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Anthropic 形态：`sk-ant-api03-` + 95 字符 base64url。
    fn sample_api_key() -> String {
        format!("sk-ant-api03-{}", random_b64url(71))
    }

    /// 真实 JWT 结构：base64url(header).base64url(payload).base64url(signature)。
    fn sample_jwt() -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD
            .encode(format!(r#"{{"sub":"{}","iat":1757894400}}"#, random_b64url(12)).as_bytes());
        format!("{header}.{payload}.{}", random_b64url(32))
    }

    /// 真实 PEM 装甲：body 为运行期随机载荷、64 列折行。
    fn sample_pem(armor: &str) -> String {
        let mut material = vec![0u8; 1216];
        getrandom::getrandom(&mut material).unwrap();
        let encoded = BASE64.encode(material);
        let body = encoded
            .as_bytes()
            .chunks(64)
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        format!("-----BEGIN {armor}-----\n{body}\n-----END {armor}-----")
    }

    #[test]
    fn redacts_sk_family_keys() {
        let redactor = Redactor::new().unwrap();
        let text = format!("OPENAI_API_KEY={}", sample_api_key());
        let out = redactor.redact(&text);
        assert_eq!(out, "OPENAI_API_KEY=[REDACTED:api-key]");
        let anthropic = format!("(cwd: /tmp) export ANTHROPIC_API_KEY={}", sample_api_key());
        assert!(redactor.redact(&anthropic).contains("[REDACTED:api-key]"));
    }

    #[test]
    fn redacts_jwt() {
        let redactor = Redactor::new().unwrap();
        let out = redactor.redact(&format!("authorization: {}", sample_jwt()));
        assert_eq!(out, "authorization: [REDACTED:jwt]");
    }

    #[test]
    fn redacts_full_pem_block() {
        let redactor = Redactor::new().unwrap();
        let out = redactor.redact(&format!("crash dump:\n{}\ntail", sample_pem("PRIVATE KEY")));
        assert_eq!(out, "crash dump:\n[REDACTED:pem-private-key]\ntail");
    }

    #[test]
    fn redacts_truncated_pem_to_end_of_input() {
        let redactor = Redactor::new().unwrap();
        let full = sample_pem("OPENSSH PRIVATE KEY");
        let truncated = full.lines().take(5).collect::<Vec<_>>().join("\n");
        let out = redactor.redact(&truncated);
        assert_eq!(out, "[REDACTED:pem-private-key]");
    }

    #[test]
    fn redacts_other_credential_families() {
        let redactor = Redactor::new().unwrap();
        for (text, kind) in [
            (
                "Authorization: Bearer abcdefghijklmnop.qrstuvwx",
                RedactKind::BearerToken,
            ),
            ("AKIAIOSFODNN7EXAMPLE", RedactKind::AwsAccessKey),
            (
                "ghp_0123456789abcdefghijklmnopqrstuvwxyz",
                RedactKind::GitHubToken,
            ),
            ("xoxb-123456789012-abcdefghijkl", RedactKind::SlackToken),
        ] {
            let out = redactor.redact(text);
            assert!(out.contains(kind.marker()), "未命中 {kind:?}：{out}");
            assert!(redactor.is_clean(&out), "标记自身不得再命中：{out}");
        }
    }

    #[test]
    fn keeps_benign_text_untouched() {
        let redactor = Redactor::new().unwrap();
        let benign = "2026-09-15T00:00:00Z INFO adapter started pid=4242\n\
                      error: request failed status=503 retry=1\n\
                      note: this is a task, taking risk into account";
        assert_eq!(redactor.redact(benign), benign);
        assert!(redactor.is_clean(benign));
    }

    #[test]
    fn redaction_is_idempotent_and_clean() {
        let redactor = Redactor::new().unwrap();
        let text = format!("key={} jwt={}", sample_api_key(), sample_jwt());
        let once = redactor.redact(&text);
        let twice = redactor.redact(&once);
        assert_eq!(once, twice);
        assert!(redactor.is_clean(&once));
    }

    #[test]
    fn redacts_json_recursively() {
        let redactor = Redactor::new().unwrap();
        let bundle = json!({
            "app": {"name": "Aether"},
            "config": {"key": sample_api_key()},
            "logs": [format!("token={}", sample_jwt())],
            "count": 3
        });
        let out = redactor.redact_json(&bundle);
        let text = serde_json::to_string(&out).unwrap();
        assert!(redactor.is_clean(&text));
        assert_eq!(out["count"], json!(3));
        assert_eq!(out["app"]["name"], json!("Aether"));
    }
}
