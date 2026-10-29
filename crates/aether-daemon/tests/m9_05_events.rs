//! M9-05 进程级事件桥（P2 计划 M9-05 DoD-1；ADR-029 D-029-5 / §4.4）。
//!
//! 启动真实 `aetherd --stdio`（独立临时数据目录），预置 runtime/session 后经
//! `session_rename` 产生落盘事件，断言：
//! - stdout 出现 JSON-RPC notification `aether.event`（无 `id`，params 9 字段）；
//! - `session_id`/`run_id`/`runtime_id`/`seq`/`ts`/`type`/`payload` 逐字段正确；
//! - 同会话 `seq` 单调唯一（1/2/3），DB `UNIQUE(session_id, seq)` 兜底；
//! - 事件桥停止后 daemon 干净退出（不重启）。

// 集成测试文件级豁免（断言失败即 panic）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::time::Instant;

use harness::{Daemon, TestDirs, FRAME_TIMEOUT};
use serde_json::{json, Value};

const SESSION_ID: &str = "01J00000000000000000000S05";

/// 预置 `runtimes`/`sessions` 行（事件管线不依赖适配器；rename 只读写会话行）。
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
             VALUES (?1, 'mock', 'seed', 'idle', 1, 1)",
            [SESSION_ID],
        )
        .expect("session 行");
}

/// 读取下一条 `aether.event`（先查 `request` 跳过的通知，再等 stdout）。
fn next_event(daemon: &mut Daemon) -> Value {
    if let Some(index) = daemon
        .notifications
        .iter()
        .position(|frame| frame.get("method").and_then(Value::as_str) == Some("aether.event"))
    {
        return daemon.notifications.remove(index);
    }
    let deadline = Instant::now() + FRAME_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "等待 aether.event 超时");
        let frame = daemon.try_next_frame(remaining).expect("aether.event 帧");
        if frame.get("method").and_then(Value::as_str) == Some("aether.event") {
            return frame;
        }
        daemon.notifications.push(frame);
    }
}

fn assert_envelope(event: &Value, seq: u64, title: &str) {
    assert_eq!(event["jsonrpc"], json!("2.0"));
    assert_eq!(event["method"], json!("aether.event"));
    assert!(event.get("id").is_none(), "通知不得携带 id");
    let params = event["params"].as_object().expect("params 对象");
    assert_eq!(params.len(), 9, "EventEnvelope 必须恰好 9 字段：{params:?}");
    for field in aether_core::ENVELOPE_FIELDS {
        assert!(params.contains_key(field), "缺少信封字段 {field}");
    }
    assert_eq!(params["v"], json!(aether_core::EVENT_ENVELOPE_VERSION));
    assert!(
        params["id"].as_str().is_some_and(|id| !id.is_empty()),
        "事件 id 必须为非空字符串"
    );
    assert_eq!(params["session_id"], json!(SESSION_ID));
    assert_eq!(params["run_id"], Value::Null);
    assert_eq!(params["runtime_id"], json!("mock"));
    assert_eq!(params["seq"], json!(seq));
    assert!(params["ts"].as_i64().unwrap_or_default() > 0, "ts 必须为正");
    assert_eq!(params["type"], json!("session.updated"));
    assert_eq!(params["payload"]["session_id"], json!(SESSION_ID));
    assert_eq!(params["payload"]["changed_fields"], json!(["title"]));
    assert_eq!(params["payload"]["title"], json!(title));
}

#[test]
fn daemon_pushes_aether_event_after_log_with_monotonic_seq() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    seed_database(&dirs.data_dir());
    let mut daemon = Daemon::spawn(&dirs);
    daemon.initialize(1);
    let health = daemon.wait_ready();
    assert_eq!(health["result"]["storage_state"], json!("normal"));
    // 事件桥在预热完成（phase=ready 通知）前安装；等待 ready 消除订阅竞态。
    assert!(
        daemon.wait_for_status("ready", FRAME_TIMEOUT).is_some(),
        "必须收到 ready 阶段通知"
    );

    for (index, title) in ["M9-05 A", "M9-05 B", "M9-05 C"].iter().enumerate() {
        let response = daemon.request(
            100 + index as i64,
            "session_rename",
            json!({"session_id": SESSION_ID, "title": title}),
        );
        assert!(
            response.get("result").is_some(),
            "rename 必须成功：{response}"
        );
        let event = next_event(&mut daemon);
        assert_envelope(&event, index as u64 + 1, title);
    }

    // stdout 只允许 JSONL（事件桥不得输出调试文本）。
    for line in daemon.stdout_lines() {
        let value: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("stdout 非 JSON 行（{error}）：{line}"));
        assert_eq!(value["jsonrpc"], json!("2.0"));
    }

    let code = daemon.shutdown();
    assert_eq!(code, 0, "事件桥停止后 daemon 必须干净退出");

    let connection = rusqlite::Connection::open(dirs.data_dir().join("aether.db")).expect("直连");
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1",
            [SESSION_ID],
            |row| row.get(0),
        )
        .expect("事件计数");
    assert_eq!(count, 3, "先日志后广播：3 次 rename 必须 3 条事件");
    let max_seq: i64 = connection
        .query_row(
            "SELECT MAX(seq) FROM events WHERE session_id = ?1",
            [SESSION_ID],
            |row| row.get(0),
        )
        .expect("最大 seq");
    assert_eq!(max_seq, 3, "seq 会话内单调唯一");
    let duplicate = connection.execute(
        "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
         VALUES ('01J00000000000000000000D05', ?1, NULL, 'mock', 1, 'log', '{}', 1, 1)",
        [SESSION_ID],
    );
    assert!(
        duplicate.is_err(),
        "DB UNIQUE(session_id, seq) 必须兜底拒绝重复 seq"
    );
    drop(connection);

    // DoD-5：daemon 重启后 `session_list` + `messages_page` 同步；无重复/无空洞。
    let mut restarted = Daemon::spawn(&dirs);
    restarted.initialize(200);
    restarted.wait_ready();
    let sessions = restarted.request(201, "session_list", json!({}));
    let list = sessions["result"]
        .as_array()
        .unwrap_or_else(|| panic!("session_list result 必须是数组：{sessions}"));
    let row = list
        .iter()
        .find(|session| session["id"] == json!(SESSION_ID))
        .unwrap_or_else(|| panic!("重启后必须能列出会话：{sessions}"));
    assert_eq!(row["title"], json!("M9-05 C"), "重启后标题为最后落盘值");

    let page = restarted.request(
        202,
        "messages_page",
        json!({"session_id": SESSION_ID, "last_seq": 0, "limit": 10}),
    );
    let events = page["result"]["events"]
        .as_array()
        .unwrap_or_else(|| panic!("messages_page.events 必须是数组：{page}"));
    let seqs: Vec<u64> = events
        .iter()
        .map(|event| event["seq"].as_u64().unwrap_or_default())
        .collect();
    assert_eq!(seqs, vec![1, 2, 3], "重启后补读无重复/无空洞");
    assert_eq!(page["result"]["complete"], json!(true));

    let resumed = restarted.request(
        203,
        "messages_page",
        json!({"session_id": SESSION_ID, "last_seq": 2, "limit": 10}),
    );
    let resumed_events = resumed["result"]["events"].as_array().expect("补读页");
    let resumed_seqs: Vec<u64> = resumed_events
        .iter()
        .map(|event| event["seq"].as_u64().unwrap_or_default())
        .collect();
    assert_eq!(resumed_seqs, vec![3], "last_seq 续读只返回缺口之后的事件");
    assert_eq!(restarted.shutdown(), 0);
}
