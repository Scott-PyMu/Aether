//! M9-09 DoD-8：旧 Tauri Keychain 条目在真实 OS 凭据库中的可读性/保留性。
//!
//! 旧 Tauri 构建与 Electron 构建使用同一 keyring 命名空间
//! `aether/provider/<id>`；本测试在真实 OS 凭据库写入一个“旧条目”，执行
//! preflight，断言：
//! - 条目可读（Windows Credential Manager / macOS Keychain / Linux Secret Service）；
//! - preflight **不删除**旧条目（再次读取仍成功）；
//! - 测试结束清理测试条目。
//!
//! 凭据库不可用（CI 无 Secret Service）时跳过；macOS 的“新签名身份 ACL”
//! 仍须在 M9-11 签名产物上复跑（DoD-8/G5 条件）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use aether_app::keychain_preflight::{run, MODE_OK};
use aether_security::{self_check, KeychainRef, KeyringStore, SecretStore, SecretValue};

#[test]
fn legacy_keyring_entry_is_readable_and_preserved() {
    let store = KeyringStore::new();
    if self_check(&store).is_err() {
        eprintln!("SKIP：OS 凭据库不可用（无 Secret Service / Keychain 权限）");
        return;
    }

    let reference =
        KeychainRef::new("provider", "m9-09-legacy-entry").expect("测试引用应为合法 keychain URI");
    store
        .set(&reference, &SecretValue::new("m9-09-legacy-value"))
        .expect("写入旧条目模拟数据");

    let report = run(&store, &["m9-09-legacy-entry".to_owned()]);
    assert_eq!(report.mode, MODE_OK, "旧条目必须可读：{report:?}");
    assert_eq!(report.readable, 1);
    assert_eq!(report.unavailable, 0);

    let preserved = store.get(&reference).expect("preflight 不得删除旧条目");
    assert_eq!(preserved.expose(), "m9-09-legacy-value");

    let _ = store.delete(&reference);
}
