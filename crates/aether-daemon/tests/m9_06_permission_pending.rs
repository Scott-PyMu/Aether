//! M9-06 进程级权限通道（P2 计划 M9-06 DoD-5；ADR-029 §4.4/§16.4）。
//!
//! 启动真实 `aetherd --stdio`（独立临时数据目录），预置一条 `permissions.status=pending`
//! 记录（模拟核心重启后 D9 待审批恢复），断言：
//! - `permissions_pending` 轮询是唯一待审批事实源（返回原 request_id）；
//! - `permission_resolve` 是唯一决议入口：决议后落库 + 审计 + `aether.event`；
//! - `permission.pending` 通知（`{count, session_id}`）随落盘事件刷新，**不携带决议字段**；
//! - 重复决议 → `invalid_value` + `business_code=permission_not_pending`；
//! - 通知桥/事件桥停止后 daemon 干净退出（不重启）。

// 集成测试文件级豁免（断言失败即 panic）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::time::Instant;

use harness::{Daemon, TestDirs, FRAME_TIMEOUT};
use serde_json::{json, Value};

const SESSION_ID: &str = "01J00000000000000000000S06";
const PERMISSION_ID: &str = "01J00000000000000000000T06";
const REQUEST_ID: &str = "01J00000000000000000000P06";

fn now_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// 预置 runtime/session + 一条 pending 权限记录（核心重启后 `restore_pending` 恢复）。
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
    connection
        .execute(
            "INSERT INTO permissions \
             (id, session_id, request_id, resource, action, target, decision, status, requested_at) \
             VALUES (?1, ?2, ?3, 'fs.write', 'write', 'C:\\outside\\notes.md', 'ask', 'pending', ?4)",
            rusqlite::params![PERMISSION_ID, SESSION_ID, REQUEST_ID, now_ms()],
        )
        .expect("pending 权限行");
    drop(connection);
}

/// 读取下一条指定方法的通知（先查 `request` 跳过的通知，再等 stdout）。
fn next_notification(daemon: &mut Daemon, method: &str) -> Value {
    if let Some(index) = daemon
        .notifications
        .iter()
        .position(|frame| frame.get("method").and_then(Value::as_str) == Some(method))
    {
        return daemon.notifications.remove(index);
    }
    let deadline = Instant::now() + FRAME_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "等待 {method} 通知超时");
        let frame = daemon.try_next_frame(remaining).expect("通知帧");
        if frame.get("method").and_then(Value::as_str) == Some(method) {
            return frame;
        }
        daemon.notifications.push(frame);
    }
}

#[test]
fn permission_pending_notification_and_single_resolution_path() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    seed_database(&dirs.data_dir());
    let mut daemon = Daemon::spawn(&dirs);
    daemon.initialize(1);
    let health = daemon.wait_ready();
    assert_eq!(health["result"]["storage_state"], json!("normal"));
    assert!(
        daemon.wait_for_status("ready", FRAME_TIMEOUT).is_some(),
        "必须收到 ready 阶段通知（通知桥随管线接线完成）"
    );

    // 轮询：重启后待审批恢复，request_id 原样。
    let pending = daemon.request(
        100,
        "permissions_pending",
        json!({ "session_id": SESSION_ID }),
    );
    let items = pending["result"]
        .as_array()
        .unwrap_or_else(|| panic!("permissions_pending 返回数组：{pending}"));
    assert_eq!(items.len(), 1, "pending 必须恢复且只有 1 条");
    assert_eq!(items[0]["request_id"], json!(REQUEST_ID));
    assert_eq!(items[0]["resource"], json!("fs.write"));
    assert_eq!(items[0]["timeout_ms"], json!(300_000), "D9：300s");

    // 唯一决议入口：permission_resolve（通知不参与决议）。
    let resolved = daemon.request(
        101,
        "permission_resolve",
        json!({ "request_id": REQUEST_ID, "decision": "deny" }),
    );
    assert_eq!(
        resolved["result"]["decision"],
        json!("deny"),
        "决议必须成功：{resolved}"
    );
    assert_eq!(resolved["result"]["ticket_id"], json!(PERMISSION_ID));

    // 落盘事件（先日志后广播）→ 通知刷新。
    let event = next_notification(&mut daemon, "aether.event");
    assert_eq!(event["params"]["type"], json!("permission.resolved"));
    assert_eq!(event["params"]["session_id"], json!(SESSION_ID));
    let notice = next_notification(&mut daemon, "permission.pending");
    let params = notice["params"].as_object().expect("通知 params 对象");
    assert_eq!(params.len(), 2, "只允许 count + session_id：{params:?}");
    assert_eq!(params["count"], json!(0));
    assert_eq!(params["session_id"], json!(SESSION_ID));
    for forbidden in ["request_id", "decision", "scope", "ticket_id"] {
        assert!(
            !params.contains_key(forbidden),
            "permission.pending 不得携带决议字段 {forbidden}"
        );
    }

    // 轮询再次校准：空清单。
    let pending = daemon.request(102, "permissions_pending", json!({}));
    assert_eq!(pending["result"], json!([]));

    // 重复决议：稳定业务码（不新增 32 枚举）。
    let duplicate = daemon.request(
        103,
        "permission_resolve",
        json!({ "request_id": REQUEST_ID, "decision": "once" }),
    );
    assert_eq!(duplicate["error"]["code"], json!(-32602));
    assert_eq!(
        duplicate["error"]["data"]["aether_code"],
        json!("invalid_value")
    );
    assert_eq!(
        duplicate["error"]["data"]["business_code"],
        json!("permission_not_pending")
    );

    assert_eq!(daemon.shutdown(), 0, "通知桥/事件桥停止后干净退出");

    // 审计与落库（daemon 退出后直连核查）。
    let connection = rusqlite::Connection::open(dirs.data_dir().join("aether.db")).expect("直连");
    let (status, decision): (String, String) = connection
        .query_row(
            "SELECT status, decision FROM permissions WHERE id = ?1",
            [PERMISSION_ID],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("权限行");
    assert_eq!(status, "resolved");
    assert_eq!(decision, "deny");
    let audit: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'permission.resolved' AND session_id = ?1",
            [SESSION_ID],
            |row| row.get(0),
        )
        .expect("审计计数");
    assert_eq!(audit, 1, "决议必须审计落库");
    let events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND type = 'permission.resolved'",
            [SESSION_ID],
            |row| row.get(0),
        )
        .expect("事件计数");
    assert_eq!(events, 1, "permission.resolved 必须先落盘后广播");
}
