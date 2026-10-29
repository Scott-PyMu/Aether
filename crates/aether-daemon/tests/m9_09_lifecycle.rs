//! M9-09 DoD-2/4/5/6：daemon 锁生命周期、stdin EOF 与关闭预算（真实 `aetherd`）。
//!
//! 覆盖：
//! - daemon 持有 `<data_dir>/.aetherd.lock`（OS 独占）→ 第二个 daemon 退出码 20 +
//!   `business_code=data_dir_locked`；
//! - daemon `kill -9` → OS 释放锁 → 新 daemon 可正常启动（M9 不执行迁移）；
//! - Electron 崩溃（stdin EOF）→ daemon 优雅关闭并在 8.2s 内退出 + 锁释放；
//! - `shutdown` 立即回执，退出 ≤8.2s（ADR-029 §3.6）。
//!
//! 运行：`cargo test -p aether-daemon --test m9_09_lifecycle -j 1 -- --test-threads=1`。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::time::{Duration, Instant};

use aether_daemon::lock::DataDirLock;
use harness::{serial_guard, Daemon, TestDirs};
use serde_json::{json, Value};

const SHUTDOWN_BUDGET: Duration = Duration::from_millis(8_200);

fn assert_bootstrap_lock_failure(mut daemon: Daemon) {
    let frame = daemon
        .try_next_frame(Duration::from_secs(15))
        .expect("第二个 daemon 必须写出首帧错误");
    assert_eq!(frame["id"], Value::Null, "预初始化错误必须是 id:null：{frame}");
    assert_eq!(
        frame["error"]["data"]["business_code"],
        json!("data_dir_locked"),
        "业务码必须是 data_dir_locked：{frame}"
    );
    let exit = daemon.wait_exit(Duration::from_secs(15)).expect("第二个 daemon 必须退出");
    assert_eq!(exit, 20, "data_dir_locked 退出码必须 20");
}

#[test]
fn lock_is_held_and_second_daemon_fails_with_20() {
    let _guard = serial_guard();
    let dirs = TestDirs::new();
    let mut first = Daemon::spawn(&dirs);
    first.initialize(1);
    first.wait_ready();
    assert!(
        dirs.data_dir().join(".aetherd.lock").exists(),
        "daemon 运行期间必须持有锁文件"
    );

    let second = Daemon::spawn(&dirs);
    assert_bootstrap_lock_failure(second);

    // 第一个 daemon 仍可用（锁不是全局单例）。
    let health = first.request(2, "health", Value::Null);
    assert!(health.get("result").is_some(), "持锁 daemon 必须仍可用：{health}");
    assert_eq!(first.shutdown(), 0);
}

#[test]
fn kill_9_releases_lock_and_next_daemon_starts_without_migration() {
    let _guard = serial_guard();
    let dirs = TestDirs::new();
    let mut first = Daemon::spawn(&dirs);
    first.initialize(1);
    first.wait_ready();
    let db = dirs.data_dir().join("aether.db");
    assert!(db.exists(), "预热后数据库必须存在");

    first.kill_hard();
    // OS 释放句柄后锁可重新获取（不依赖存在性判断/陈旧锁清理）。
    let lock = DataDirLock::acquire(&dirs.data_dir()).expect("强杀后锁必须由 OS 释放");
    drop(lock);

    let mut second = Daemon::spawn(&dirs);
    let init = second.initialize(2);
    assert_eq!(
        init["result"]["protocolVersion"],
        json!("1.0"),
        "强杀后新 daemon 必须可握手：{init}"
    );
    let startup = second.request(3, "startup_get", Value::Null);
    assert_eq!(
        startup["result"]["phase"],
        json!("ready"),
        "M9 不迁移，启动门保持 ready：{startup}"
    );
    assert_eq!(second.shutdown(), 0);
}

#[test]
fn stdin_eof_gracefully_shuts_down_and_releases_lock_within_budget() {
    let _guard = serial_guard();
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn(&dirs);
    daemon.initialize(1);
    daemon.wait_ready();

    let started = Instant::now();
    daemon.close_stdin();
    let exit = daemon
        .wait_exit(SHUTDOWN_BUDGET)
        .expect("stdin EOF 后必须优雅退出（≤8.2s）");
    assert_eq!(exit, 0);
    assert!(
        started.elapsed() <= SHUTDOWN_BUDGET,
        "EOF 关闭超预算：{:?}",
        started.elapsed()
    );
    DataDirLock::acquire(&dirs.data_dir()).expect("EOF 关闭后锁必须释放");
}

#[test]
fn shutdown_ack_is_immediate_and_exit_within_8_2s() {
    let _guard = serial_guard();
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn(&dirs);
    daemon.initialize(1);
    daemon.wait_ready();

    let started = Instant::now();
    let response = daemon.request(9_001, "shutdown", Value::Null);
    let ack_ms = started.elapsed();
    assert_eq!(
        response["result"]["accepted"],
        json!(true),
        "shutdown 必须立即回执：{response}"
    );
    assert!(ack_ms < Duration::from_secs(1), "回执不得等待关闭序列（{ack_ms:?}）");
    let exit = daemon
        .wait_exit(SHUTDOWN_BUDGET)
        .expect("shutdown 后必须 ≤8.2s 退出");
    assert_eq!(exit, 0);
    assert!(
        started.elapsed() <= SHUTDOWN_BUDGET,
        "关闭序列超预算：{:?}",
        started.elapsed()
    );
}
