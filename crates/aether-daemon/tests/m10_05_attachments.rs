//! M10-05 进程级 E2E：`aetherd` 上的附件三命令 + 能力门（ADR-035）。
//!
//! 覆盖：
//! - `attachment_add`（真实文件/魔数/内容寻址）→ `attachment_list` → `attachment_remove`；
//! - 非法类型结构化拒绝（`attachment_type_unsupported`）；
//! - `session_send.attachments` 在未声明 `multimodal` 的运行时上拒绝，且不产生消息/run。
//!
//! 进程级用例串行执行（harness `serial_guard`）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::path::Path;

use harness::{Daemon, TestDirs};
use serde_json::{json, Value};

const SESSION_ID: &str = "01J00000000000000000000M05";

fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&[0, 0, 0, 13]);
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes
}

fn seed_database(data_dir: &Path) {
    std::fs::create_dir_all(data_dir).expect("数据目录");
    let db = data_dir.join("aether.db");
    let store = aether_store::Store::open(&db).expect("建库/迁移");
    store
        .execute_write(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1)",
        )
        .expect("runtime");
    store
        .execute_write(&format!(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES ('{SESSION_ID}', 'mock', 'E2E 附件会话', 'idle', 1, 1)"
        ))
        .expect("session");
}

#[test]
fn daemon_attachment_lifecycle_and_capability_gate() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    seed_database(&dirs.data_dir());
    let source = dirs.root.path().join("shot.png");
    std::fs::write(&source, png_header(64, 48)).expect("fixture");
    let text = dirs.root.path().join("note.txt");
    std::fs::write(&text, b"nope").expect("fixture");

    let mut daemon = Daemon::spawn_with_env(&dirs.env(), &[]);
    daemon.initialize(1);
    let health = daemon.wait_ready();
    assert_eq!(health["result"]["storage_state"], json!("normal"));

    let added: Value = daemon.request(
        100,
        "attachment_add",
        json!({
            "session_id": SESSION_ID,
            "source_path": source.to_string_lossy(),
            "kind": "image",
        }),
    );
    let summary = added
        .get("result")
        .unwrap_or_else(|| panic!("attachment_add 失败：{added}"));
    assert!(
        summary.get("storage_path").is_none(),
        "不得返回 storage_path"
    );
    let attachment_id = summary["attachment_id"]
        .as_str()
        .expect("attachment_id")
        .to_owned();
    let sha = summary["sha256"].as_str().expect("sha256").to_owned();
    assert_eq!(summary["mime"], "image/png");
    let stored = dirs
        .data_dir()
        .join("attachments")
        .join(&sha[..2])
        .join(format!("{sha}.png"));
    assert!(stored.is_file(), "内容寻址文件必须落盘");

    let listed = daemon.request(101, "attachment_list", json!({ "session_id": SESSION_ID }));
    let items = listed["result"]["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["attachment_id"], attachment_id.as_str());

    let rejected = daemon.request(
        102,
        "attachment_add",
        json!({
            "session_id": SESSION_ID,
            "source_path": text.to_string_lossy(),
            "kind": "image",
        }),
    );
    assert_eq!(
        rejected["error"]["data"]["business_code"],
        json!("attachment_type_unsupported"),
        "非法类型必须结构化拒绝：{rejected}"
    );

    // 运行时 mock 未声明 multimodal → 发送附件拒绝（不静默丢弃），不产生消息。
    let send = daemon.request(
        103,
        "session_send",
        json!({
            "session_id": SESSION_ID,
            "text": "看图",
            "client_msg_id": "01J0000000000000000000M051",
            "attachments": [attachment_id],
        }),
    );
    assert_eq!(
        send["error"]["data"]["business_code"],
        json!("attachment_type_unsupported"),
        "能力门必须拒绝：{send}"
    );
    let messages = daemon.request(
        104,
        "messages_page",
        json!({ "session_id": SESSION_ID, "limit": 10 }),
    );
    assert_eq!(
        messages["result"]["messages"].as_array().map(Vec::len),
        Some(0),
        "拒绝发送不得产生消息：{messages}"
    );

    let removed = daemon.request(
        105,
        "attachment_remove",
        json!({ "attachment_id": attachment_id }),
    );
    assert_eq!(removed["result"]["removed"], json!(true));
    assert!(!stored.exists(), "无引用文件必须清理");
    let again = daemon.request(
        106,
        "attachment_remove",
        json!({ "attachment_id": attachment_id }),
    );
    assert_eq!(again["result"]["removed"], json!(false));
}
