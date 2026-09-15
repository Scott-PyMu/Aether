//! M1-07 DoD3 集成验证：A3 降级加密文件的启动、持久化、防篡改与错误路径。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use aether_security::{
    AdapterSecretPlan, EncryptedFileStore, FileCryptoParams, KeychainRef, SecretStore, SecretValue,
    SecurityConfig, SecurityLevel, SecurityManager, A3_M_COST_KIB, A3_P_COST, A3_T_COST,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

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

fn fast_params() -> FileCryptoParams {
    FileCryptoParams::test_only_fast()
}

#[test]
fn a3_parameters_are_frozen() {
    assert_eq!(A3_M_COST_KIB, 64 * 1024);
    assert_eq!(A3_T_COST, 3);
    assert_eq!(A3_P_COST, 1);
    assert_eq!(FileCryptoParams::a3(), FileCryptoParams::default());
    assert_ne!(FileCryptoParams::test_only_fast(), FileCryptoParams::a3());
}

#[test]
fn degraded_startup_publishes_banner_and_roundtrips() {
    let dir = temp_dir("degraded");
    let path = dir.join("secrets.enc");
    let passphrase = SecretValue::new("drill-passphrase-value");
    let config = SecurityConfig::new(path.clone())
        .with_passphrase(passphrase.clone())
        .with_crypto_params(fast_params())
        .force_degraded();

    let manager = SecurityManager::initialize(&config).unwrap();
    assert_eq!(manager.level(), SecurityLevel::Degraded);
    assert_eq!(
        manager.status().label_zh(),
        "安全级别：降级",
        "DoD3 冻结文案"
    );
    assert!(manager.status().banner_zh().contains("安全级别：降级"));
    assert!(manager.status().detail().contains("演练"));

    let reference = KeychainRef::new("adapter-drill", "api-token").unwrap();
    let secret = SecretValue::new("drill-secret-value-1234567890");
    manager.store().set(&reference, &secret).unwrap();

    let resolved = manager.resolve(&reference).unwrap();
    assert!(resolved.constant_time_eq(&secret));

    let plan = AdapterSecretPlan::new("adapter-drill").with_var("DRILL_TOKEN", reference.clone());
    let env = manager.resolve_env(&plan).unwrap();
    assert!(env.get("DRILL_TOKEN").unwrap().constant_time_eq(&secret));

    // 落盘文件必须是加密信封：无明文密钥、无口令。
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains(secret.expose()), "secrets.enc 不得含明文密钥");
    assert!(!raw.contains(passphrase.expose()), "secrets.enc 不得含口令");
    let envelope: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(envelope["v"], 1);
    assert_eq!(envelope["kdf"], "argon2id");
    assert_eq!(envelope["m_cost_kib"], fast_params().m_cost_kib);

    // 删除后读取必须 NotFound。
    manager.store().delete(&reference).unwrap();
    assert!(manager.resolve(&reference).unwrap_err().is_not_found());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn degraded_restart_reopens_same_file() {
    let dir = temp_dir("restart");
    let path = dir.join("secrets.enc");
    let passphrase = SecretValue::new("restart-passphrase");
    let config = SecurityConfig::new(path)
        .with_passphrase(passphrase)
        .with_crypto_params(fast_params())
        .force_degraded();

    let reference = KeychainRef::new("adapter-drill", "api-token").unwrap();
    let secret = SecretValue::new("restart-secret-value");
    {
        let manager = SecurityManager::initialize(&config).unwrap();
        manager.store().set(&reference, &secret).unwrap();
    }
    let manager = SecurityManager::initialize(&config).unwrap();
    let resolved = manager.resolve(&reference).unwrap();
    assert!(resolved.constant_time_eq(&secret));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wrong_passphrase_is_rejected() {
    let dir = temp_dir("wrong-pass");
    let path = dir.join("secrets.enc");
    let reference = KeychainRef::new("adapter-drill", "api-token").unwrap();
    {
        let store = EncryptedFileStore::create(
            &path,
            &SecretValue::new("correct-passphrase"),
            fast_params(),
        )
        .unwrap();
        store
            .set(&reference, &SecretValue::new("some-secret-value"))
            .unwrap();
    }
    let err = EncryptedFileStore::open(&path, &SecretValue::new("wrong-passphrase")).unwrap_err();
    assert!(matches!(err, aether_security::SecretError::Decrypt));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tampered_ciphertext_is_rejected() {
    let dir = temp_dir("tamper");
    let path = dir.join("secrets.enc");
    let passphrase = SecretValue::new("tamper-passphrase");
    let reference = KeychainRef::new("adapter-drill", "api-token").unwrap();
    {
        let store = EncryptedFileStore::create(&path, &passphrase, fast_params()).unwrap();
        store
            .set(&reference, &SecretValue::new("tamper-secret-value"))
            .unwrap();
    }

    let raw = std::fs::read(&path).unwrap();
    let mut envelope: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let ciphertext = envelope["ciphertext"].as_str().unwrap().to_string();
    let mut bytes = BASE64.decode(ciphertext).unwrap();
    bytes[0] ^= 0x01;
    envelope["ciphertext"] = serde_json::Value::String(BASE64.encode(bytes));
    std::fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();

    let err = EncryptedFileStore::open(&path, &passphrase).unwrap_err();
    assert!(matches!(err, aether_security::SecretError::Decrypt));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_reference_and_missing_file_fail_closed() {
    let dir = temp_dir("missing");
    let path = dir.join("secrets.enc");
    let passphrase = SecretValue::new("missing-passphrase");
    let store = EncryptedFileStore::create(&path, &passphrase, fast_params()).unwrap();
    let reference = KeychainRef::new("adapter-drill", "absent").unwrap();
    assert!(store.get(&reference).unwrap_err().is_not_found());
    assert!(store.delete(&reference).unwrap_err().is_not_found());

    let absent_path = dir.join("absent.enc");
    assert!(EncryptedFileStore::open(&absent_path, &passphrase).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn encrypted_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = temp_dir("perms");
    let path = dir.join("secrets.enc");
    let _store =
        EncryptedFileStore::create(&path, &SecretValue::new("perms-passphrase"), fast_params())
            .unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "secrets.enc 必须仅属主可读写");
    let _ = std::fs::remove_dir_all(&dir);
}
