// 注册清单规范化形态与摘要（ADR-015 §2.2#1/#2；build.rs 与运行期共用）。
//
// 本文件同时被 `crates/aether-tauri/build.rs`（`include!`）与库
// （`lib.rs` 的 `pub mod registry_identity`）使用，因此**只允许依赖** `std` /
// `serde_json` / `sha2`，禁止引用 crate 内部模块；文件头不得使用 `//!` 内层文档
// 注释（`include!` 到 build.rs 时位于项之后，会触发 E0753）。
//
// 规范化形态口径（M7-01 DoD3 登记）：
// - 键序固定：对象键按 UTF-8 字节字典序排序（与 `serde_json` 的
//   `preserve_order` 特性无关，显式排序）；
// - 紧凑 JSON：无多余空白/换行（LF/BOM 与格式差异不影响摘要）；
// - 数组顺序保持；标量按 `serde_json` 文本序列化（UTF-8）。
//
// 摘要算法：sha256 小写十六进制，前缀 `sha256:`（与迁移 checksum 同算法）。

use std::io::Read;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// 规范化 JSON 文本（递归键序排序 + 紧凑序列化）。
pub fn canonical_json(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                match serde_json::to_string(&Value::String((*key).clone())) {
                    Ok(text) => out.push_str(&text),
                    Err(_) => out.push_str("\"<key>\""),
                }
                out.push(':');
                if let Some(entry) = map.get(*key) {
                    canonical_json(entry, out);
                }
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                canonical_json(item, out);
            }
            out.push(']');
        }
        other => match serde_json::to_string(other) {
            Ok(text) => out.push_str(&text),
            Err(_) => out.push_str("null"),
        },
    }
}

/// 字节序列的 sha256（`sha256:<hex>`；与迁移 checksum 同算法口径）。
pub fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("sha256:{hex}")
}

/// 清单文档（已解析 `Value`）的规范化摘要。
pub fn manifest_digest(value: &Value) -> String {
    let mut text = String::new();
    canonical_json(value, &mut text);
    sha256_digest(text.as_bytes())
}

/// 程序文件的 sha256 摘要（流式读取，避免大文件整体驻留内存）。
pub fn program_digest(path: &std::path::Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(format!("sha256:{hex}"))
}
