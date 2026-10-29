//! Keychain 启动 preflight（M9-09 DoD-8；ADR-029 §5.4）。
//!
//! 目的：旧 Tauri 构建写入的 `keychain://aether/provider/<id>` 条目，在 Electron
//! 签名身份下可能触发 ACL 重授权或不可读。启动时**只读探测**这些条目：
//! - 可读 → `readable`（无需提示）；
//! - 不存在 → `missing`（不是失败：用户可能从未保存密钥）；
//! - 凭据库不可用/被拒 → `unavailable` → `mode=reauth`（S-13 重授权/口令降级）。
//!
//! **绝不删除旧条目**（AGENTS §2.9 / UI-UX S-13）；本模块只调用 `SecretStore::get`。

use aether_security::{KeychainRef, SecretError, SecretStore};
use serde::Serialize;

/// 探测范围（S-13 `data-scope`）。
pub const SCOPE_PROVIDER: &str = "provider";
/// 全部条目可读/不存在。
pub const MODE_OK: &str = "ok";
/// 至少一条条目不可读 → 需要重新授权/口令降级。
pub const MODE_REAUTH: &str = "reauth";

/// 单条探测结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeychainPreflightEntry {
    /// `keychain://aether/<service>/<key>`（引用不是密钥本体）。
    pub reference: String,
    /// `readable` / `missing` / `unavailable`。
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 启动 preflight 报告（只含引用/状态，不含密钥值）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeychainPreflightReport {
    pub scope: String,
    /// `ok` / `reauth`
    pub mode: String,
    pub readable: usize,
    pub missing: usize,
    pub unavailable: usize,
    pub entries: Vec<KeychainPreflightEntry>,
}

impl KeychainPreflightReport {
    /// 是否需要 S-13 重授权/口令降级。
    #[must_use]
    pub fn needs_reauth(&self) -> bool {
        self.mode == MODE_REAUTH
    }
}

/// 对给定 provider id 集合做只读 preflight。
///
/// 去重并按字典序探测，保证证据可复跑；空集合返回 `mode=ok`（不作为失败）。
#[must_use]
pub fn run(store: &dyn SecretStore, provider_ids: &[String]) -> KeychainPreflightReport {
    let mut ids: Vec<&String> = provider_ids.iter().collect();
    ids.sort();
    ids.dedup();

    let mut entries = Vec::with_capacity(ids.len());
    let mut readable = 0usize;
    let mut missing = 0usize;
    let mut unavailable = 0usize;

    for provider_id in ids {
        let reference = match KeychainRef::new("provider", provider_id.clone()) {
            Ok(reference) => reference,
            Err(error) => {
                unavailable = unavailable.saturating_add(1);
                entries.push(KeychainPreflightEntry {
                    reference: format!("keychain://aether/provider/{provider_id}"),
                    status: "unavailable".to_owned(),
                    detail: Some(error.to_string()),
                });
                continue;
            }
        };
        let uri = reference.to_uri();
        match store.get(&reference) {
            Ok(_) => {
                readable = readable.saturating_add(1);
                entries.push(KeychainPreflightEntry {
                    reference: uri,
                    status: "readable".to_owned(),
                    detail: None,
                });
            }
            Err(SecretError::NotFound { .. }) => {
                missing = missing.saturating_add(1);
                entries.push(KeychainPreflightEntry {
                    reference: uri,
                    status: "missing".to_owned(),
                    detail: None,
                });
            }
            Err(error) => {
                unavailable = unavailable.saturating_add(1);
                entries.push(KeychainPreflightEntry {
                    reference: uri,
                    status: "unavailable".to_owned(),
                    detail: Some(error.to_string()),
                });
            }
        }
    }

    KeychainPreflightReport {
        scope: SCOPE_PROVIDER.to_owned(),
        mode: if unavailable > 0 {
            MODE_REAUTH.to_owned()
        } else {
            MODE_OK.to_owned()
        },
        readable,
        missing,
        unavailable,
        entries,
    }
}

/// 启动入口：密钥存储未挂载（keyring 不可用且无口令降级）也视为需要 S-13。
///
/// 这样无需 main 读取 `startup_get`（M9 保留命令不暴露），S-13 触发完全由 daemon
/// 的只读 preflight + `daemon.status` 通知承担。
#[must_use]
pub fn run_boot(
    store: Option<&dyn SecretStore>,
    provider_ids: &[String],
) -> KeychainPreflightReport {
    match store {
        Some(store) => run(store, provider_ids),
        None if provider_ids.is_empty() => KeychainPreflightReport {
            scope: SCOPE_PROVIDER.to_owned(),
            mode: MODE_OK.to_owned(),
            readable: 0,
            missing: 0,
            unavailable: 0,
            entries: Vec::new(),
        },
        None => KeychainPreflightReport {
            scope: SCOPE_PROVIDER.to_owned(),
            mode: MODE_REAUTH.to_owned(),
            readable: 0,
            missing: 0,
            unavailable: 1,
            entries: vec![KeychainPreflightEntry {
                reference: "keychain://aether/provider/<unmounted>".to_owned(),
                status: "unavailable".to_owned(),
                detail: Some(
                    "OS 凭据库不可用且未提供口令降级（密钥存储未挂载；旧条目未删除）"
                        .to_owned(),
                ),
            }],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_security::{SecretValue, SecurityLevel};
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeStore {
        values: HashMap<String, Result<String, &'static str>>,
        gets: Mutex<Vec<String>>,
        deletes: Mutex<Vec<String>>,
    }

    impl SecretStore for FakeStore {
        fn level(&self) -> SecurityLevel {
            SecurityLevel::OsKeychain
        }

        fn get(&self, reference: &KeychainRef) -> Result<SecretValue, SecretError> {
            self.gets
                .lock()
                .map(|mut guard| guard.push(reference.to_uri()))
                .ok();
            match self.values.get(&reference.to_uri()) {
                Some(Ok(value)) => Ok(SecretValue::new(value.clone())),
                Some(Err(detail)) => Err(SecretError::KeychainUnavailable((*detail).to_owned())),
                None => Err(SecretError::NotFound {
                    reference: reference.clone(),
                }),
            }
        }

        fn set(&self, _reference: &KeychainRef, _value: &SecretValue) -> Result<(), SecretError> {
            Err(SecretError::KeychainUnavailable("read-only test".to_owned()))
        }

        fn delete(&self, reference: &KeychainRef) -> Result<(), SecretError> {
            self.deletes
                .lock()
                .map(|mut guard| guard.push(reference.to_uri()))
                .ok();
            Ok(())
        }
    }

    #[test]
    fn readable_entries_need_no_prompt() {
        let mut store = FakeStore::default();
        store.values.insert(
            "keychain://aether/provider/openai".to_owned(),
            Ok("secret-value".to_owned()),
        );
        let report = run(&store, &["openai".to_owned(), "openai".to_owned()]);
        assert_eq!(report.mode, MODE_OK);
        assert_eq!(report.readable, 1);
        assert_eq!(report.missing, 0);
        assert_eq!(report.unavailable, 0);
        assert_eq!(report.entries.len(), 1, "重复 id 必须去重");
    }

    #[test]
    fn unavailable_entry_requests_reauth_without_deleting() {
        let mut store = FakeStore::default();
        store.values.insert(
            "keychain://aether/provider/codex".to_owned(),
            Err("ACL denied (test)"),
        );
        let report = run(&store, &["codex".to_owned()]);
        assert!(report.needs_reauth());
        assert_eq!(report.unavailable, 1);
        assert_eq!(
            store.deletes.lock().map(|guard| guard.len()).unwrap_or(0),
            0,
            "preflight 不得删除旧条目"
        );
    }

    #[test]
    fn missing_entry_is_not_a_failure() {
        let store = FakeStore::default();
        let report = run(&store, &["not-installed".to_owned()]);
        assert_eq!(report.mode, MODE_OK);
        assert_eq!(report.missing, 1);
        assert_eq!(report.unavailable, 0);
    }

    #[test]
    fn unmounted_store_requests_reauth_only_when_providers_exist() {
        let empty = run_boot(None, &[]);
        assert_eq!(empty.mode, MODE_OK, "全新安装无密钥条目时不得弹 S-13");

        let report = run_boot(None, &["openai".to_owned()]);
        assert!(report.needs_reauth());
        assert_eq!(report.unavailable, 1);
        assert!(report.entries[0].reference.contains("unmounted"));
    }

    #[test]
    fn report_never_contains_secret_values() {
        let mut store = FakeStore::default();
        store.values.insert(
            "keychain://aether/provider/openai".to_owned(),
            Ok("sk-live-should-never-appear".to_owned()),
        );
        let report = run(&store, &["openai".to_owned()]);
        let json = serde_json::to_string(&report).unwrap_or_default();
        assert!(!json.contains("sk-live-should-never-appear"));
        assert!(json.contains("keychain://aether/provider/openai"));
    }
}
