//! M1-07 DoD2 集成验证：`sk-` / `eyJ` / PEM 三类真实样本在「日志 + 诊断导出」
//! 经脱敏器后 **0 命中**（扫描断言）。
//!
//! 样本策略（AGENTS.md §2.9 + 任务说明「必须真实模式」）：
//! - 三类样本均在**运行期生成**（随机载荷 + 真实结构/长度/字符集），仓库与夹具不落任何密钥；
//! - 先对原始样本断言扫描器可命中（防「扫描器失效导致的假阴性」），再对脱敏产物断言 0 命中。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aether_security::Redactor;
use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD};
use base64::Engine as _;
use regex::Regex;
use serde_json::json;

/// DoD 三类扫描模式（与脱敏实现分离的独立扫描器）。
const SCAN_API_KEY: &str = r"sk-[A-Za-z0-9_-]{8,}";
const SCAN_JWT: &str = r"eyJ[A-Za-z0-9_-]{8,}";
const SCAN_PEM: &str = r"-----BEGIN[ A-Z0-9]*PRIVATE KEY";

fn hits(text: &str, pattern: &str) -> usize {
    Regex::new(pattern).unwrap().find_iter(text).count()
}

fn random_b64url(byte_len: usize) -> String {
    let mut bytes = vec![0u8; byte_len];
    getrandom::getrandom(&mut bytes).unwrap();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Anthropic 形态：`sk-ant-api03-` + 95 字符 base64url（真实长度量级）。
fn sample_api_key() -> String {
    format!("sk-ant-api03-{}", random_b64url(71))
}

/// 真实 JWT 结构：base64url(header).base64url(payload).base64url(signature)。
fn sample_jwt() -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD
        .encode(format!(r#"{{"sub":"{}","iat":1757894400}}"#, random_b64url(12)).as_bytes());
    let signature = random_b64url(32);
    format!("{header}.{payload}.{signature}")
}

/// 真实 PEM 装甲：PKCS#8 / OpenSSH 等，body 按 64 列 base64 折行。
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

fn log_corpus(api_key: &str, jwt: &str, pem: &str, pem_crlf: &str) -> String {
    format!(
        "2026-09-15T10:00:00.123Z INFO runtime.spawn adapter=codex\n\
         2026-09-15T10:00:01.000Z DEBUG request headers authorization=Bearer {jwt}\n\
         2026-09-15T10:00:02.000Z ERROR provider init failed: credentials rejected (key={api_key})\n\
         2026-09-15T10:00:03.000Z WARN adapter started pid=4242 latency_ms=87\n\
         2026-09-15T10:00:04.000Z ERROR crash dump follows\r\n{pem_crlf}\r\n\
         2026-09-15T10:00:05.000Z INFO stdout scan begin\n{pem}\n2026-09-15T10:00:06.000Z INFO stdout scan end\n\
         2026-09-15T10:00:07.000Z INFO health check ok\n"
    )
}

fn diagnostics_export(api_key: &str, jwt: &str, pem: &str) -> serde_json::Value {
    json!({
        "app": {"name": "Aether", "version": "0.1.0"},
        "security": {
            "level": "安全级别：系统凭据库",
            "refs": ["keychain://aether/adapter-codex/api-key"],
        },
        "config": {
            "providers": {
                "anthropic": {
                    "api_key": api_key,
                    "base_url": "https://api.anthropic.com",
                }
            }
        },
        "logs": [
            format!("Authorization: Bearer {jwt}"),
            format!("private material:\n{pem}"),
        ],
        "db": {"size_bytes": 4096, "events": 120},
    })
}

#[test]
fn redaction_scan_zero_hits_on_logs_and_diagnostics_export() {
    let api_key = sample_api_key();
    let jwt = sample_jwt();
    let pem = sample_pem("PRIVATE KEY");
    let pem_crlf = sample_pem("OPENSSH PRIVATE KEY").replace('\n', "\r\n");
    let redactor = Redactor::new().unwrap();

    // 1) 原始日志：三类模式必须全部可被独立扫描器命中（防假阴性）。
    let corpus = log_corpus(&api_key, &jwt, &pem, &pem_crlf);
    assert!(hits(&corpus, SCAN_API_KEY) >= 1, "扫描器应命中 sk- 样本");
    assert!(hits(&corpus, SCAN_JWT) >= 1, "扫描器应命中 eyJ 样本");
    assert!(hits(&corpus, SCAN_PEM) >= 2, "扫描器应命中 PEM 样本");

    // 2) 脱敏后的日志：0 命中 + 原文中的密钥字节串必须消失。
    let redacted = redactor.redact(&corpus);
    assert_eq!(hits(&redacted, SCAN_API_KEY), 0, "sk- 在日志中必须 0 命中");
    assert_eq!(hits(&redacted, SCAN_JWT), 0, "eyJ 在日志中必须 0 命中");
    assert_eq!(hits(&redacted, SCAN_PEM), 0, "PEM 在日志中必须 0 命中");
    for sample in [&api_key, &jwt, &pem, &pem_crlf] {
        assert!(!redacted.contains(sample.as_str()), "脱敏产物仍含原始样本");
    }
    assert!(
        redacted.contains("adapter started pid=4242"),
        "普通日志行必须保留"
    );
    assert!(redacted.contains("health check ok"), "普通日志行必须保留");
    assert!(redacted.contains("[REDACTED:pem-private-key]"));
    assert_eq!(redactor.redact(&redacted), redacted, "脱敏必须幂等");

    // 3) 诊断导出：原始 JSON 可命中，脱敏后 0 命中且结构保留。
    let bundle = diagnostics_export(&api_key, &jwt, &pem);
    let raw_export = serde_json::to_string_pretty(&bundle).unwrap();
    assert!(hits(&raw_export, SCAN_API_KEY) >= 1);
    assert!(hits(&raw_export, SCAN_JWT) >= 1);
    assert!(hits(&raw_export, SCAN_PEM) >= 1);

    let redacted_export = redactor.redact_json(&bundle);
    let export_text = serde_json::to_string_pretty(&redacted_export).unwrap();
    assert_eq!(
        hits(&export_text, SCAN_API_KEY),
        0,
        "sk- 在诊断导出中必须 0 命中"
    );
    assert_eq!(
        hits(&export_text, SCAN_JWT),
        0,
        "eyJ 在诊断导出中必须 0 命中"
    );
    assert_eq!(
        hits(&export_text, SCAN_PEM),
        0,
        "PEM 在诊断导出中必须 0 命中"
    );
    assert!(!export_text.contains(&api_key));
    assert!(!export_text.contains(&jwt));
    assert!(!export_text.contains(&pem));
    assert_eq!(redacted_export["db"]["size_bytes"], json!(4096));
    assert_eq!(redacted_export["app"]["name"], json!("Aether"));
    assert_eq!(
        redacted_export["security"]["refs"][0],
        json!("keychain://aether/adapter-codex/api-key"),
        "引用本身不是密钥，允许保留"
    );
}

#[test]
fn truncated_pem_is_redacted_to_end() {
    // 崩溃日志中被截断的 PEM（无 END 行）：必须一直脱敏到末尾。
    let redactor = Redactor::new().unwrap();
    let pem = sample_pem("RSA PRIVATE KEY");
    let truncated = pem.lines().take(6).collect::<Vec<_>>().join("\n");
    let corpus = format!(
        "2026-09-15T10:00:00.000Z ERROR dump\n{truncated}\n2026-09-15T10:00:01.000Z INFO tail"
    );
    let redacted = redactor.redact(&corpus);
    assert_eq!(hits(&redacted, SCAN_PEM), 0);
    assert!(!redacted.contains(&pem));
}
