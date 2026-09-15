//! 降级路径演练（M1-07 DoD3）：模拟 OS 凭据库不可用 → A3 加密文件启动。
//!
//! 用法：
//!   `cargo run -p aether-security --example degraded_drill -- --data-dir <DIR>`
//! 口令来源：环境变量 `AETHER_M1_07_DRILL_PASSPHRASE`（仅驻留进程，不落盘、不回显）。
//! 密钥来源：环境变量 `AETHER_M1_07_DRILL_TOKEN`（可选；验证脚本注入真实形态样本，
//! 未提供时进程内随机生成）。该值只写入加密文件，禁止回显。
//! 断言（由 scripts/test/m1-07-verify.mjs 校验）：
//!   - stdout 含 `安全级别：降级`；
//!   - 写入 → 解析（含重启后重新打开）→ 删除链路通过；
//!   - stdout / secrets.enc 均不含明文密钥。

use std::path::PathBuf;
use std::process::ExitCode;

use aether_security::{
    AdapterSecretPlan, FileCryptoParams, KeychainRef, SecretValue, SecurityConfig, SecurityLevel,
    SecurityManager,
};

const PASSPHRASE_ENV: &str = "AETHER_M1_07_DRILL_PASSPHRASE";
const TOKEN_ENV: &str = "M1_07_DRILL_TOKEN";
const TOKEN_SOURCE_ENV: &str = "AETHER_M1_07_DRILL_TOKEN";
const DRILL_SERVICE: &str = "adapter-drill";
const DRILL_KEY: &str = "api-token";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("[degraded-drill] FAIL: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let mut data_dir: Option<PathBuf> = None;
    let mut fast_kdf = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => {
                data_dir = Some(PathBuf::from(args.next().ok_or("--data-dir 缺少取值")?));
            }
            "--fast-kdf" => fast_kdf = true,
            other => return Err(format!("未知参数：{other}")),
        }
    }
    let data_dir = data_dir.ok_or("缺少 --data-dir")?;
    std::fs::create_dir_all(&data_dir).map_err(|err| format!("创建数据目录失败：{err}"))?;

    let passphrase = std::env::var(PASSPHRASE_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("缺少环境变量 {PASSPHRASE_ENV}（演练口令不回显、不落盘）"))?;
    let passphrase = SecretValue::new(passphrase);

    let token = match std::env::var(TOKEN_SOURCE_ENV) {
        Ok(value) if !value.is_empty() => SecretValue::new(value),
        _ => SecretValue::new(format!("drill-token-{}", random_hex(24)?)),
    };

    let secrets_file = data_dir.join("secrets.enc");
    println!("[degraded-drill] 数据目录：{}", data_dir.display());
    println!("[degraded-drill] 凭据库探测：模拟不可用（演练开关 force_degraded）");

    let params = if fast_kdf {
        FileCryptoParams::test_only_fast()
    } else {
        FileCryptoParams::a3()
    };
    let config = SecurityConfig::new(secrets_file.clone())
        .with_passphrase(passphrase.clone())
        .with_crypto_params(params)
        .force_degraded();

    let manager = SecurityManager::initialize(&config).map_err(|err| err.to_string())?;
    if manager.level() != SecurityLevel::Degraded {
        return Err(format!("预期降级，实际级别：{:?}", manager.level()));
    }
    // DoD3 要求的 UI 明示文案（由 SecurityStatus 统一提供，UI 不得静默降级）。
    println!("[degraded-drill] {}", manager.status().label_zh());
    println!("[degraded-drill] {}", manager.status().banner_zh());

    let reference = KeychainRef::new(DRILL_SERVICE, DRILL_KEY).map_err(|e| e.to_string())?;
    manager
        .store()
        .set(&reference, &token)
        .map_err(|err| format!("写入密钥失败：{err}"))?;
    // 第二个条目用于验证「删除」链路；主条目保留在文件中，供脚本扫描明文泄漏。
    let refresh_reference =
        KeychainRef::new(DRILL_SERVICE, "refresh-token").map_err(|e| e.to_string())?;
    let refresh = SecretValue::new(format!("drill-refresh-{}", random_hex(16)?));
    manager
        .store()
        .set(&refresh_reference, &refresh)
        .map_err(|err| format!("写入第二密钥失败：{err}"))?;
    println!("[degraded-drill] 写入引用 {reference}：ok（值不回显）");

    let plan = AdapterSecretPlan::new(DRILL_SERVICE).with_var(TOKEN_ENV, reference.clone());
    let env = manager
        .resolve_env(&plan)
        .map_err(|err| format!("解析注入失败：{err}"))?;
    let resolved = env.get(TOKEN_ENV).ok_or("注入项缺失")?;
    if !resolved.constant_time_eq(&token) {
        return Err("解析值与写入值不一致".into());
    }
    println!("[degraded-drill] 解析注入 env {TOKEN_ENV}：ok（值不回显）");

    // 模拟重启：重新打开同一文件并再次解析。
    drop(env);
    drop(manager);
    let manager = SecurityManager::initialize(&config).map_err(|err| err.to_string())?;
    let plan = AdapterSecretPlan::new(DRILL_SERVICE).with_var(TOKEN_ENV, reference.clone());
    let env = manager
        .resolve_env(&plan)
        .map_err(|err| format!("重启后解析注入失败：{err}"))?;
    let resolved = env.get(TOKEN_ENV).ok_or("重启后注入项缺失")?;
    if !resolved.constant_time_eq(&token) {
        return Err("重启后解析值与写入值不一致".into());
    }
    manager
        .store()
        .delete(&refresh_reference)
        .map_err(|err| format!("删除密钥失败：{err}"))?;
    println!("[degraded-drill] 重启后解析 + 删除：ok");
    println!("[degraded-drill] PASS");
    Ok(())
}

fn random_hex(byte_len: usize) -> Result<String, String> {
    let mut buf = vec![0u8; byte_len];
    getrandom::getrandom(&mut buf).map_err(|err| format!("随机数失败：{err}"))?;
    let mut out = String::with_capacity(byte_len * 2);
    for byte in buf {
        out.push_str(&format!("{byte:02x}"));
    }
    Ok(out)
}
