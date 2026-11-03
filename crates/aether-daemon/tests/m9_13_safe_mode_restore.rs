//! M9-13 DoD-4（ADR-029 v0.4 §3.5-6 / R10-A）：`quick_check` 失败安全模式下
//! `backup_restore` 维护例外。
//!
//! 场景：
//! - 真实库 + 数据 → 复制为外部 `.db` 候选；
//! - 损坏运行库末页 → daemon 打开后进入 `persist_degraded` 只读安全模式（不退出）；
//! - 安全模式调用 `backup_restore`（外部候选）→ 允许登记现场日志（`restart_required`）；
//! - 重启 daemon → 启动序列在无写者窗口执行恢复 → `quick_check`/数据一致/审计。
//!
//! 集成测试文件级豁免（断言失败即 panic）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::io::{Seek, SeekFrom, Write};

use aether_store::backup::{read_restore_journal, restore_journal_path};
use harness::{Daemon, TestDirs, FRAME_TIMEOUT};
use serde_json::{json, Value};

const SESSION_ID: &str = "01J00000000000000000013S01";
const MESSAGE_A: &str = "01J00000000000000000013M01";
const MESSAGE_B: &str = "01J00000000000000000013M02";

fn seed_database(data_dir: &std::path::Path) {
    std::fs::create_dir_all(data_dir).expect("数据目录");
    let db = data_dir.join("aether.db");
    {
        let store = aether_store::Store::open(&db).expect("建库/迁移");
        drop(store);
    }
    let connection = rusqlite::Connection::open(&db).expect("直连");
    connection
        .execute(
            "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
             VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1)",
            [],
        )
        .expect("runtime 行");
    connection
        .execute(
            "INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
             VALUES (?1, 'mock', 'M9-13 safe mode', 'idle', 1, 1)",
            [SESSION_ID],
        )
        .expect("session 行");
    for (id, role, content, seq) in [
        (MESSAGE_A, "user", "hello", 1_i64),
        (MESSAGE_B, "assistant", "world", 2_i64),
    ] {
        connection
            .execute(
                "INSERT INTO messages (id, session_id, role, content, seq, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)",
                rusqlite::params![id, SESSION_ID, role, content, seq],
            )
            .expect("message 行");
    }
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint");
}

fn corrupt_last_page(db: &std::path::Path) {
    let connection = rusqlite::Connection::open(db).expect("直连");
    connection
        .execute_batch(
            "CREATE TABLE corrupt_probe (x BLOB);\n\
             WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 120)\n\
             INSERT INTO corrupt_probe (x) SELECT randomblob(2000) FROM seq;\n\
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .expect("构造损坏样本");
    let page_size: i64 = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .expect("page_size");
    let page_count: i64 = connection
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .expect("page_count");
    assert!(page_count > 4, "样本库必须多页");
    let offset = u64::try_from((page_count - 1) * page_size).expect("offset");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(db)
        .expect("打开库文件");
    file.seek(SeekFrom::Start(offset)).expect("seek");
    file.write_all(&[0xFF; 256]).expect("破坏末页");
    file.flush().expect("flush");
    drop(connection);
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
}

fn health_until_ready(daemon: &mut Daemon) -> Value {
    let deadline = std::time::Instant::now() + FRAME_TIMEOUT;
    let mut id = 100;
    loop {
        let health = daemon.request(id, "health", Value::Null);
        id += 1;
        if health.get("result").is_some() {
            return health;
        }
        assert_eq!(
            health["error"]["data"]["aether_code"],
            json!("core_not_ready"),
            "预热期只允许 core_not_ready：{health}"
        );
        assert!(std::time::Instant::now() < deadline, "等待安全模式超时");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn safe_mode_allows_backup_restore_and_applies_it_on_next_start() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let db = dirs.data_dir().join("aether.db");
    seed_database(&dirs.data_dir());

    // 外部明文候选：恢复前先复制（候选自身必须 quick_check 通过）。
    let candidate = dirs.root.path().join("m9-13-candidate.db");
    std::fs::copy(&db, &candidate).expect("复制候选");
    corrupt_last_page(&db);

    // 1) 安全模式：quick_check 失败但进程不退出，health=persist_degraded。
    let mut daemon = Daemon::spawn(&dirs);
    let response = daemon.request(
        1,
        "initialize",
        harness::initialize_request(1)["params"].clone(),
    );
    assert!(
        response.get("result").is_some(),
        "完整性失败不得致命：{response}"
    );
    let health = health_until_ready(&mut daemon);
    assert_eq!(
        health["result"]["storage_state"],
        json!("persist_degraded"),
        "quick_check 失败 → 只读安全模式：{health}"
    );

    // 2) 维护例外：安全模式下仍可登记恢复（不写库，只写现场日志）。
    let restore = daemon.request(
        2,
        "backup_restore",
        json!({"source": {"external": {"path": candidate.to_string_lossy()}}}),
    );
    assert!(
        restore.get("result").is_some(),
        "安全模式必须允许 backup_restore 维护例外：{restore}"
    );
    assert_eq!(restore["result"]["restart_required"], json!(true));
    let journal = read_restore_journal(&db)
        .expect("读取现场日志")
        .expect("现场日志必须存在");
    assert_eq!(journal.status, "requested", "恢复请求必须登记为 requested");
    assert!(
        restore_journal_path(&db).exists(),
        "现场日志文件必须落盘：{}",
        restore_journal_path(&db).display()
    );

    // 3) 关闭并重启：启动序列执行恢复，随后正常打开（quick_check 通过）。
    assert_eq!(daemon.shutdown(), 0, "安全模式仍可干净退出");
    assert!(
        read_restore_journal(&db).expect("读取现场日志").is_some(),
        "重启前现场日志必须保留（由下次启动消费）"
    );

    let mut restarted = Daemon::spawn(&dirs);
    restarted.initialize(1);
    let health = restarted.wait_ready();
    assert_eq!(
        health["result"]["storage_state"],
        json!("normal"),
        "恢复后必须回到可写正常态：{health}"
    );
    assert!(
        read_restore_journal(&db).expect("读取现场日志").is_none(),
        "恢复成功后现场日志必须收尾"
    );

    let sessions = restarted.request(2, "session_list", json!({}));
    let list = sessions["result"]
        .as_array()
        .unwrap_or_else(|| panic!("session_list 数组：{sessions}"));
    assert_eq!(list.len(), 1, "旧会话必须保留：{sessions}");
    assert_eq!(list[0]["id"], json!(SESSION_ID));

    let messages = restarted.request(
        3,
        "messages_page",
        json!({"session_id": SESSION_ID, "limit": 50}),
    );
    let rows = messages["result"]["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("messages_page.messages 数组：{messages}"));
    assert_eq!(rows.len(), 2, "消息必须保留：{messages}");

    assert_eq!(restarted.shutdown(), 0);

    // 4) 审计：启动恢复写 `backup.restore`（D13 第 7 步）。
    let connection = rusqlite::Connection::open(&db).expect("直连");
    let audits: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'backup.restore'",
            [],
            |row| row.get(0),
        )
        .expect("审计计数");
    assert!(audits >= 1, "恢复成功必须写审计（实际 {audits}）");
    let integrity: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .expect("quick_check");
    assert_eq!(integrity, "ok", "恢复后 quick_check 必须通过");
}
