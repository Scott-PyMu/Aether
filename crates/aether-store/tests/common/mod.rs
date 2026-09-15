//! M1-03 测试公共辅助（各 DoD 测试文件共用）。
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

use aether_store::Store;
use tempfile::TempDir;

/// 新建测试临时目录（Drop 时自动清理）。
pub fn temp_dir(prefix: &str) -> TempDir {
    tempfile::Builder::new()
        .prefix(&format!("aether-m1-03-{prefix}-"))
        .tempdir()
        .unwrap()
}

/// 临时目录内的库文件路径。
pub fn db_path(dir: &TempDir) -> PathBuf {
    dir.path().join("aether.db")
}

/// 打开临时库并返回（目录句柄, Store）。
pub fn open_temp_store(prefix: &str) -> (TempDir, Store) {
    let dir = temp_dir(prefix);
    let store = Store::open(db_path(&dir)).unwrap();
    (dir, store)
}

/// 读取 SQLite 页大小（DB 头偏移 16..18；值 1 代表 65536）。
pub fn page_size(bytes: &[u8]) -> usize {
    assert!(bytes.len() >= 100, "库文件头不完整");
    let raw = u16::from_be_bytes([bytes[16], bytes[17]]);
    if raw == 1 {
        65_536
    } else {
        raw as usize
    }
}

/// 破坏库文件最后一页的页类型字节（0xFF 为非法页类型），用于构造损坏库样本。
///
/// 前提：调用方已完成 `wal_checkpoint(TRUNCATE)` 并关闭写连接。
pub fn corrupt_last_page(path: &Path) -> usize {
    let mut bytes = fs::read(path).unwrap();
    let page = page_size(&bytes);
    assert!(bytes.len() >= page * 2, "库文件至少需要两页");
    let last_page_start = bytes.len() - page;
    bytes[last_page_start] = 0xFF;
    fs::write(path, &bytes).unwrap();
    last_page_start
}

/// 文件 sha256（十六进制）。
pub fn sha256_file(path: &Path) -> String {
    aether_store::checksum(&fs::read(path).unwrap())
}
