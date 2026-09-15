//! 脱敏过滤器（M1-07 DoD2 证据链）：stdin → 脱敏 → stdout。
//!
//! 用法：
//!   `... --example redact_filter -- --mode text < log.txt`（逐字节文本脱敏）
//!   `... --example redact_filter -- --mode json < export.json`（诊断导出 JSON 递归脱敏）
//!
//! 退出码：0 = 成功；2 = 输入/参数错误。

use std::io::{self, Read, Write};
use std::process::ExitCode;

use aether_security::Redactor;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    Json,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("[redact-filter] FAIL: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let mut mode = Mode::Text;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" => {
                mode = match args.next().as_deref() {
                    Some("text") => Mode::Text,
                    Some("json") => Mode::Json,
                    _ => return Err("--mode 只接受 text|json".into()),
                };
            }
            other => return Err(format!("未知参数：{other}")),
        }
    }

    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|err| format!("读取 stdin 失败：{err}"))?;

    let redactor = Redactor::new().map_err(|err| err.to_string())?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match mode {
        Mode::Text => {
            let redacted = redactor.redact(&input);
            out.write_all(redacted.as_bytes())
                .map_err(|err| format!("写 stdout 失败：{err}"))?;
        }
        Mode::Json => {
            let value: serde_json::Value =
                serde_json::from_str(&input).map_err(|err| format!("JSON 解析失败：{err}"))?;
            let redacted = redactor.redact_json(&value);
            serde_json::to_writer_pretty(&mut out, &redacted)
                .map_err(|err| format!("写 stdout 失败：{err}"))?;
            out.write_all(b"\n")
                .map_err(|err| format!("写 stdout 失败：{err}"))?;
        }
    }
    Ok(())
}
