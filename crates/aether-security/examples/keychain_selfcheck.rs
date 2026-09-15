//! 真实 OS 凭据库自检（M1-07 DoD1）。
//!
//! 用法：`cargo run -p aether-security --example keychain_selfcheck`
//! 退出码：0 = 自检通过；3 = 凭据库不可用（应走 A3 降级路径）；2 = 参数/内部错误。

use std::process::ExitCode;

use aether_security::{self_check, KeyringStore};

fn main() -> ExitCode {
    let store = KeyringStore::new();
    match self_check(&store) {
        Ok(()) => {
            println!("[m1-07] keychain 写→读→删自检：PASS");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("[m1-07] keychain 不可用：{err}");
            ExitCode::from(3)
        }
    }
}
