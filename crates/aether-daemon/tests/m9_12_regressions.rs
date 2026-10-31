//! M9-12 E2E 回归：生产装饰器链上的审计命令可达性。
//!
//! 背景：M9-04 的路由矩阵在脚本化 backend 上验证命令面；M9-12 的真实
//! `aetherd` Playwright E2E 发现 `SessionBackend`/`DiagnosticsControlBackend`
//! 未把 `audit_query`/`audit_export` 透传给内层 `ArchiveControlBackend`，
//! 生产链路因此命中 trait 默认 `not_implemented`。本测试锁定修复：
//! 真实 daemon + 真实存储 + 完整生产装饰器链，`audit_query` 必须返回
//! `{rows, limit, offset, has_more}`，不得是 `not_implemented`。
//!
//! 集成测试允许 panic（AGENTS §2.2 的 crate 规则针对生产代码）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use harness::{Daemon, TestDirs};
use serde_json::json;

#[test]
fn audit_query_is_reachable_through_production_decorator_chain() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn(&dirs);
    daemon.initialize(1);
    daemon.wait_ready();

    let response = daemon.request(2, "audit_query", json!({ "limit": 5 }));
    assert!(
        response.get("error").is_none(),
        "audit_query 不应在 SessionBackend/DiagnosticsControlBackend 上 not_implemented：{response}"
    );
    let result = response.get("result").expect("audit_query 必须返回 result");
    assert!(result["rows"].is_array(), "rows 必须是数组：{result}");
    assert!(result["limit"].is_number(), "limit 必须是数字：{result}");
    assert!(result["offset"].is_number(), "offset 必须是数字：{result}");
    assert!(
        result["has_more"].is_boolean(),
        "has_more 必须是布尔：{result}"
    );

    assert_eq!(daemon.shutdown(), 0, "daemon 必须干净退出");
}
