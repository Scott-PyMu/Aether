//! M1-07 DoD1（keychain 自检）+ DoD3（降级接管）集成验证。
//!
//! 平台口径：
//! - Windows / macOS：OS 凭据库必须可用，自检必须通过（含删除后无残留）；
//! - 其他平台（无 Secret Service 的 Linux CI）：允许不可用，但必须自动走 A3 降级，
//!   且降级启动后安全级别为 `Degraded`。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use aether_security::{
    self_check, self_check_reference, FileCryptoParams, KeyringStore, SecretStore, SecretValue,
    SecurityConfig, SecurityLevel, SecurityManager,
};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("aether-m1-07-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn os_keychain_self_check_or_documented_fallback() {
    let store = KeyringStore::new();
    match self_check(&store) {
        Ok(()) => {
            println!("[m1-07] OS keychain 自检通过（写入→读取→删除）");
            let reference = self_check_reference().unwrap();
            let err = store.get(&reference).unwrap_err();
            assert!(err.is_not_found(), "自检后不得残留探测项：{err}");
        }
        Err(err) => {
            println!("[m1-07] OS keychain 不可用：{err}");
            if cfg!(any(target_os = "windows", target_os = "macos")) {
                panic!("Windows/macOS 上 OS 凭据库必须可用：{err}");
            }
            // Linux（无 Secret Service）：必须自动降级，不得拒绝启动。
            let dir = temp_dir("keychain-fallback");
            let config = SecurityConfig::new(dir.join("secrets.enc"))
                .with_passphrase(SecretValue::new("ci-fallback-passphrase"))
                .with_crypto_params(FileCryptoParams::test_only_fast());
            let manager = SecurityManager::initialize(&config).unwrap();
            assert_eq!(manager.level(), SecurityLevel::Degraded);
            assert_eq!(manager.status().label_zh(), "安全级别：降级");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
