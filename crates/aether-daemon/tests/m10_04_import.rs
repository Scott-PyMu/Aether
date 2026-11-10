//! M10-04 进程级 E2E：`aetherd` 真实二进制上的导出 → 导入 → 重启切换 → 再导出。
//!
//! 覆盖：
//! - `session_export` → `session_import`（异步 job）→ `import_status` awaiting_restart；
//! - daemon 正常重启后启动序列完成 staging + 原子切换；
//! - 重启后 `session_list`/`messages_page`/`session_export` 可见导入数据；
//! - 校验失败（篡改 manifest sha256）作业 failed，既有会话不变。
//!
//! 进程级用例串行执行（harness `serial_guard`）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::path::Path;
use std::time::{Duration, Instant};

use aether_store::archive::verify_archive_file;
use harness::{Daemon, TestDirs, FRAME_TIMEOUT};
use serde_json::{json, Value};

const SESSION_ID: &str = "01J00000000000000000000M04";
const USER_MESSAGE: &str = "01J0000000000000000000M041";
const ASSISTANT_MESSAGE: &str = "01J0000000000000000000M042";
const EVENT_ID: &str = "01J0000000000000000000M04E";

fn seed_database(data_dir: &Path) {
    std::fs::create_dir_all(data_dir).expect("数据目录");
    let db = data_dir.join("aether.db");
    {
        let store = aether_store::Store::open(&db).expect("建库/迁移");
        drop(store);
    }
    let connection = rusqlite::Connection::open(&db).expect("直连");
    connection
        .execute_batch(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1);",
        )
        .expect("runtime");
    connection
        .execute(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES (?1, 'mock', 'E2E 源会话', 'idle', 1, 1)",
            [SESSION_ID],
        )
        .expect("session");
    connection
        .execute(
            "INSERT INTO messages (id, session_id, role, content, seq, created_at) \
             VALUES (?1, ?3, 'user', '你好', 1, 2), (?2, ?3, 'assistant', '回复', 2, 3)",
            rusqlite::params![USER_MESSAGE, ASSISTANT_MESSAGE, SESSION_ID],
        )
        .expect("messages");
    connection
        .execute(
            "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
             VALUES (?1, ?2, NULL, 'mock', 1, 'log', '{\"level\":\"info\",\"message\":\"seed\"}', 1, 1)",
            rusqlite::params![EVENT_ID, SESSION_ID],
        )
        .expect("event");
}

fn wait_import_status(
    daemon: &mut Daemon,
    job_id: &str,
    terminal: &[&str],
) -> Value {
    let deadline = Instant::now() + FRAME_TIMEOUT;
    let mut id = 7000;
    loop {
        let status = daemon.request(
            id,
            "import_status",
            json!({"job_id": job_id, "include_warnings": true}),
        );
        id += 1;
        let result = status.get("result").cloned().unwrap_or(Value::Null);
        let state = result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if terminal.contains(&state) {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "等待导入状态 {terminal:?} 超时；最后状态：{result}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn daemon_export_import_restart_roundtrip() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    seed_database(&dirs.data_dir());
    let exports = dirs.root.path().join("exports");
    std::fs::create_dir_all(&exports).expect("exports");

    let mut daemon = Daemon::spawn_with_env(&dirs.env(), &[]);
    daemon.initialize(1);
    let health = daemon.wait_ready();
    assert_eq!(health["result"]["storage_state"], json!("normal"));

    let export = daemon.request(
        100,
        "session_export",
        json!({"session_id": SESSION_ID, "target_dir": exports.to_string_lossy()}),
    );
    let export_path = export["result"]["export"]["path"]
        .as_str()
        .expect("导出路径")
        .to_owned();
    let exported = verify_archive_file(Path::new(&export_path)).expect("导出校验");
    assert!(exported.matches);

    // dry-run：只预览，不切换、不改变既有数据。
    let dry = daemon.request(
        1000,
        "session_import",
        json!({"source_path": export_path.clone(), "mode": "remap", "dry_run": true}),
    );
    let dry_id = dry["result"]["job_id"]
        .as_str()
        .expect("dry-run job_id")
        .to_owned();
    let dry_done = wait_import_status(&mut daemon, &dry_id, &["completed", "failed"]);
    assert_eq!(dry_done["status"], json!("completed"), "dry-run：{dry_done}");
    assert_eq!(dry_done["summary"]["sessions"], json!(1));
    let after_dry = daemon.request(1001, "session_list", json!({"limit": 100}));
    assert_eq!(
        after_dry["result"].as_array().map(Vec::len),
        Some(1),
        "dry-run 不得改变既有数据"
    );

    let started = daemon.request(
        101,
        "session_import",
        json!({"source_path": export_path, "mode": "remap"}),
    );
    let job_id = started["result"]["job_id"]
        .as_str()
        .expect("job_id")
        .to_owned();
    assert_eq!(started["result"]["status"], json!("staging"));

    let awaiting = wait_import_status(&mut daemon, &job_id, &["awaiting_restart", "failed"]);
    assert_eq!(
        awaiting["status"],
        json!("awaiting_restart"),
        "导入必须进入待重启：{awaiting}"
    );
    assert_eq!(awaiting["summary"]["sessions"], json!(1));
    assert_eq!(awaiting["summary"]["messages"], json!(2));

    // 维护窗口：变更类命令被拒绝（只读命令仍可用）。
    let blocked = daemon.request(
        102,
        "session_send",
        json!({
            "session_id": SESSION_ID,
            "text": "maintenance",
            "client_msg_id": "01J0000000000000000000M04C",
        }),
    );
    assert!(blocked.get("error").is_some(), "维护窗口必须拒绝 session_send：{blocked}");
    // awaiting_restart 不可取消（ADR-034 D-034-4）。
    let cancel = daemon.request(103, "import_cancel", json!({"job_id": job_id}));
    assert_eq!(
        cancel["error"]["data"]["business_code"],
        json!("import_job_not_active"),
        "awaiting_restart 取消必须拒绝：{cancel}"
    );

    assert_eq!(daemon.shutdown(), 0, "daemon 正常退出");
    let mut daemon = Daemon::spawn_with_env(&dirs.env(), &[]);
    daemon.initialize(1);
    daemon.wait_ready();

    let completed = wait_import_status(&mut daemon, &job_id, &["completed", "failed"]);
    assert_eq!(
        completed["status"],
        json!("completed"),
        "重启后导入必须完成：{completed}"
    );

    let sessions = daemon.request(200, "session_list", json!({"limit": 100}));
    let items = sessions["result"].as_array().expect("session_list 数组");
    assert_eq!(items.len(), 2, "源会话 + 导入副本");
    let imported = items
        .iter()
        .map(|item| item["id"].as_str().unwrap_or_default())
        .find(|id| *id != SESSION_ID)
        .expect("导入会话")
        .to_owned();
    let messages = daemon.request(
        201,
        "messages_page",
        json!({"session_id": imported, "limit": 100}),
    );
    assert_eq!(
        messages["result"]["messages"].as_array().map(Vec::len),
        Some(2),
        "导入消息必须可见：{messages}"
    );

    let export_dir = dirs.root.path().join("exports-2");
    std::fs::create_dir_all(&export_dir).expect("exports-2");
    let reexport = daemon.request(
        202,
        "session_export",
        json!({"session_id": imported, "target_dir": export_dir.to_string_lossy()}),
    );
    let reexport_path = reexport["result"]["export"]["path"]
        .as_str()
        .expect("再导出路径");
    let verified = verify_archive_file(Path::new(reexport_path)).expect("再导出校验");
    assert!(verified.matches);
    assert_eq!(verified.manifest.rows, exported.manifest.rows);

    // 审计（ADR-034 D-034-5）：import.completed + export.started/completed。
    let import_audit = daemon.request(
        203,
        "audit_query",
        json!({"action": "import.completed", "limit": 10}),
    );
    assert!(
        !import_audit["result"]["rows"]
            .as_array()
            .map(Vec::is_empty)
            .unwrap_or(true),
        "必须记录 import.completed：{import_audit}"
    );
    let export_audit = daemon.request(
        204,
        "audit_query",
        json!({"action": "export.started", "limit": 10}),
    );
    assert!(
        export_audit["result"]["rows"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
            >= 2,
        "必须记录每次导出的 export.started：{export_audit}"
    );

    assert_eq!(daemon.shutdown(), 0);
}

#[test]
fn daemon_checksum_failure_leaves_existing_data_unchanged() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    seed_database(&dirs.data_dir());
    let exports = dirs.root.path().join("exports");
    std::fs::create_dir_all(&exports).expect("exports");

    let mut daemon = Daemon::spawn_with_env(&dirs.env(), &[]);
    daemon.initialize(1);
    daemon.wait_ready();
    let export = daemon.request(
        300,
        "session_export",
        json!({"session_id": SESSION_ID, "target_dir": exports.to_string_lossy()}),
    );
    let export_path = export["result"]["export"]["path"]
        .as_str()
        .expect("导出路径")
        .to_owned();
    let text = std::fs::read_to_string(&export_path).expect("读导出");
    let mut lines = text.lines();
    let mut manifest: Value =
        serde_json::from_str(lines.next().expect("manifest")).expect("manifest json");
    manifest["sha256"] = json!("c".repeat(64));
    let payload: Vec<&str> = lines.collect();
    std::fs::write(
        &export_path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&manifest).expect("manifest"),
            payload.join("\n")
        ),
    )
    .expect("篡改");

    let started = daemon.request(
        301,
        "session_import",
        json!({"source_path": export_path}),
    );
    let job_id = started["result"]["job_id"]
        .as_str()
        .expect("job_id")
        .to_owned();
    let failed = wait_import_status(&mut daemon, &job_id, &["failed", "completed"]);
    assert_eq!(failed["status"], json!("failed"), "校验失败必须 failed：{failed}");
    assert_eq!(
        failed["error"]["code"],
        json!("import_checksum_mismatch"),
        "业务码：{failed}"
    );
    let sessions = daemon.request(302, "session_list", json!({"limit": 100}));
    assert_eq!(
        sessions["result"].as_array().map(Vec::len),
        Some(1),
        "失败不得改变既有数据"
    );
    assert_eq!(daemon.shutdown(), 0);
}
