//! M9-04 JSON-RPC 路由矩阵（P2 计划 M9-04 DoD-1/2/4；ADR-029 §4.6）。
//!
//! 断言：
//! - 59 条 daemon 命令经 JSON-RPC 可达，未知字段在命令体之前被 `-32602` 拒绝；
//! - 4 条壳本地命令**不进入** daemon（`-32601` + `reason=shell_local`）；
//! - 63 条之外的未知方法 `-32601`；
//! - 预算越界/游标篡改只失败本请求，daemon 不重启（随后 `health` 仍可用）。

// 集成测试文件级豁免（断言失败即 panic）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use harness::{Daemon, TestDirs};
use serde_json::{json, Value};

const ULID: &str = "01J8ZQ5R0N7W9Y8X6V4T2S0K1M";

fn daemon_commands() -> Vec<&'static str> {
    aether_app::COMMANDS
        .iter()
        .copied()
        .filter(|method| !aether_app::SHELL_COMMANDS.contains(method))
        .collect()
}

#[test]
fn all_63_commands_are_routed_at_the_rpc_layer() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn(&dirs);
    daemon.initialize(1);
    let health = daemon.wait_ready();
    assert_eq!(health["result"]["storage_state"], json!("normal"));

    let daemon_commands = daemon_commands();
    assert_eq!(daemon_commands.len(), 59, "daemon 路由必须 59 条");
    let mut id = 100;
    for method in &daemon_commands {
        let response = daemon.request(id, method, json!({ "__m9_04_unknown_member": true }));
        id += 1;
        if *method == "runtimes_list" {
            assert!(
                response.get("result").is_some(),
                "runtimes_list 迁移前语义：忽略参数并执行：{response}"
            );
            continue;
        }
        assert_eq!(
            response["error"]["code"],
            json!(-32602),
            "{method} 必须由严格解析拒绝：{response}"
        );
        assert_eq!(
            response["error"]["data"]["aether_code"],
            json!("unknown_field"),
            "{method} 错误码：{response}"
        );
        assert_eq!(
            response["error"]["data"]["details"]["field"],
            json!("__m9_04_unknown_member"),
            "{method} 字段名：{response}"
        );
    }

    // 4 条壳本地命令不进入 daemon（59/4 路由口径）。
    for method in aether_app::SHELL_COMMANDS {
        let response = daemon.request(id, method, Value::Null);
        id += 1;
        assert_eq!(
            response["error"]["code"],
            json!(-32601),
            "{method}：{response}"
        );
        assert_eq!(
            response["error"]["data"]["reason"],
            json!("shell_local"),
            "{method} 必须标记 shell_local：{response}"
        );
    }

    // 未知方法。
    let response = daemon.request(id, "__m9_04_unknown_method", Value::Null);
    assert_eq!(response["error"]["code"], json!(-32601));
    assert_eq!(
        response["error"]["data"]["aether_code"],
        json!("not_implemented")
    );

    let code = daemon.shutdown();
    assert_eq!(code, 0, "路由矩阵后必须干净退出");
}

#[test]
fn budget_and_cursor_failures_do_not_restart_the_daemon() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn(&dirs);
    daemon.initialize(1);
    daemon.wait_ready();

    for (id, params, code) in [
        (
            201,
            json!({ "session_id": ULID, "max_bytes": 1 }),
            "out_of_range",
        ),
        (
            202,
            json!({ "session_id": ULID, "max_bytes": 1572865 }),
            "out_of_range",
        ),
        (
            203,
            json!({ "session_id": ULID, "next_cursor": "not*base64url" }),
            "invalid_value",
        ),
        (
            204,
            json!({ "session_id": ULID, "last_seq": 1, "next_cursor": "opaque" }),
            "invalid_value",
        ),
    ] {
        let response = daemon.request(id, "messages_page", params);
        assert_eq!(
            response["error"]["code"],
            json!(-32602),
            "预算/游标错误必须是 invalid_params：{response}"
        );
        assert_eq!(
            response["error"]["data"]["aether_code"],
            json!(code),
            "预算/游标错误码：{response}"
        );
    }
    for (id, params, code) in [
        (301, json!({ "max_bytes": 65535 }), "out_of_range"),
        (302, json!({ "max_bytes": 1572865 }), "out_of_range"),
        (
            303,
            json!({ "next_cursor": "not*base64url" }),
            "invalid_value",
        ),
        (
            304,
            json!({ "offset": 1, "next_cursor": "opaque" }),
            "invalid_value",
        ),
    ] {
        let response = daemon.request(id, "audit_query", params);
        assert_eq!(
            response["error"]["code"],
            json!(-32602),
            "审计预算/游标错误必须是 invalid_params：{response}"
        );
        assert_eq!(
            response["error"]["data"]["aether_code"],
            json!(code),
            "审计预算/游标错误码：{response}"
        );
    }

    // 只失败本请求：daemon 仍可服务（未重启/未断开）。
    let health = daemon.request(400, "health", Value::Null);
    assert_eq!(health["result"]["storage_state"], json!("normal"));
    assert!(
        daemon.stderr_text().contains("aetherd 启动"),
        "stderr 日志仍在（同一进程）"
    );
    let code = daemon.shutdown();
    assert_eq!(code, 0);
}
