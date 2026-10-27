//! M9-03 进程级验收：`aetherd --stdio` 生命周期、params 形状、传输错误、致命启动检查
//! 与只读安全模式（DoD-3/5/6/7；ADR-029 §3.5/§4.1–§4.3/§4.8）。
//!
//! 每个用例启动真实 `aetherd` 二进制（`CARGO_BIN_EXE_aetherd`），使用独立临时数据目录；
//! 运行时注册目录置空（不加载/启动真实适配器）。

// 集成测试文件级豁免（与仓库既有集成测试同口径：断言失败即 panic）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::io::{Seek, SeekFrom, Write};
use std::time::Duration;

use harness::{fixture_lines, Daemon, TestDirs, FRAME_TIMEOUT};
use serde_json::{json, Value};

/// 断言 stdout 每一行都是 JSON 对象（stdout 只允许 JSONL）。
fn assert_stdout_is_jsonl(daemon: &Daemon) {
    let lines = daemon.stdout_lines();
    assert!(!lines.is_empty(), "stdout 至少应有响应帧");
    for line in &lines {
        let value: Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("stdout 出现非 JSON 行（{error}）：{line}"));
        assert_eq!(
            value["jsonrpc"],
            json!("2.0"),
            "每帧必须是 JSON-RPC 2.0：{line}"
        );
    }
}

/// 读取下一帧响应（跳过 `daemon.status` 通知，按序断言）。
fn next_response(daemon: &mut Daemon) -> Value {
    loop {
        let frame = daemon
            .try_next_frame(FRAME_TIMEOUT)
            .expect("等待响应帧超时");
        if frame.get("method").and_then(Value::as_str) == Some("daemon.status") {
            daemon.notifications.push(frame);
            continue;
        }
        return frame;
    }
}

#[test]
fn initialize_shutdown_and_stdout_is_jsonl_only() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn(&dirs);

    let response = daemon.initialize(1);
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], json!("1.0"));
    assert_eq!(result["daemonVersion"], json!(env!("CARGO_PKG_VERSION")));
    assert_eq!(result["coreVersion"], json!(aether_core::version()));
    assert_eq!(
        result["daemonCapabilities"],
        json!(["events", "permissions", "dialogs"])
    );
    let phase = result["phase"].as_str().unwrap_or_default();
    assert!(
        ["warming", "ready", "degraded"].contains(&phase),
        "initialize 阶段必须是 warming/ready/degraded（M9 不产生 migrating/blocked）：{phase}"
    );
    let data_dir = result["dataDir"].as_str().unwrap_or_default();
    assert_eq!(
        std::path::Path::new(data_dir),
        dirs.data_dir(),
        "initialize 必须返回解析后的数据目录"
    );

    let health = daemon.wait_ready();
    assert_eq!(health["result"]["storage_state"], json!("normal"));
    // `startup_get` 契约与迁移期壳一致：数据目录 + security_level（M3-05/D10 探针）。
    let startup = daemon.request(2, "startup_get", Value::Null);
    assert_eq!(startup["result"]["phase"], json!("ready"));
    assert_eq!(
        startup["result"]["data_dir"].as_str().unwrap_or_default(),
        data_dir
    );
    let level = startup["result"]["security_level"]["level"]
        .as_str()
        .unwrap_or_default();
    assert!(
        ["os", "degraded"].contains(&level),
        "security_level 必须是 os/degraded：{startup}"
    );
    assert!(
        daemon.stderr_text().contains("aetherd 启动"),
        "stderr 必须承载运行期日志"
    );
    assert_stdout_is_jsonl(&daemon);

    let code = daemon.shutdown();
    assert_eq!(code, 0, "shutdown 必须干净退出（退出码 0）");
}

#[test]
fn params_shapes_are_equivalent_and_payload_wrapper_is_rejected() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn(&dirs);
    daemon.initialize(1);
    daemon.wait_ready();

    let lines = fixture_lines("params.jsonl");
    assert_eq!(lines.len(), 5, "params fixture 5 行");

    // 无参命令：省略 / null / {} 三者等价（ADR-029 P1-5）。
    let mut results = Vec::new();
    for line in &lines[..3] {
        daemon.send_raw(line.as_bytes());
    }
    for id in [11, 12, 13] {
        let response = daemon.wait_for_id(id, FRAME_TIMEOUT);
        assert!(
            response.get("result").is_some(),
            "无参形状必须成功（id={id}）：{response}"
        );
        results.push(response["result"].clone());
    }
    assert_eq!(results[0], results[1], "省略 params 与 null 必须等价");
    assert_eq!(results[0], results[2], "省略 params 与 {{}} 必须等价");

    // 有参命令：不得再有 `{payload}` 包装（内层 DTO）。
    daemon.send_raw(lines[3].as_bytes());
    let wrapped = daemon.wait_for_id(14, FRAME_TIMEOUT);
    assert_eq!(wrapped["error"]["code"], json!(-32602));
    assert_eq!(
        wrapped["error"]["data"]["aether_code"],
        json!("unknown_field")
    );
    assert_eq!(
        wrapped["error"]["data"]["details"]["field"],
        json!("payload")
    );

    // 内层 DTO 直传：严格解析通过（后端可能拒绝，但不得是参数形状错误）。
    daemon.send_raw(lines[4].as_bytes());
    let inner = daemon.wait_for_id(15, FRAME_TIMEOUT);
    if let Some(error) = inner.get("error") {
        let code = error["data"]["aether_code"].as_str().unwrap_or_default();
        assert!(
            !["unknown_field", "invalid_json", "missing_field"].contains(&code),
            "内层 DTO 必须被接受（错误码 {code}）：{inner}"
        );
    }

    assert_eq!(daemon.shutdown(), 0);
}

#[test]
fn transport_errors_do_not_kill_the_daemon() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn(&dirs);
    daemon.initialize(1);
    daemon.wait_ready();

    let lines = fixture_lines("transport.jsonl");
    assert_eq!(lines.len(), 7, "transport fixture 7 行");
    for line in &lines {
        daemon.send_raw(line.as_bytes());
    }

    // 顺序断言（请求串行处理）：parse / not-object / bad-jsonrpc / missing-method /
    // 首次 id / 重复 id / 未知方法。
    let parse_error = next_response(&mut daemon);
    assert_eq!(parse_error["id"], Value::Null);
    assert_eq!(parse_error["error"]["code"], json!(-32700));
    assert_eq!(
        parse_error["error"]["data"]["aether_code"],
        json!("invalid_json")
    );

    let not_object = next_response(&mut daemon);
    assert_eq!(not_object["error"]["code"], json!(-32600));
    assert_eq!(
        not_object["error"]["data"]["aether_code"],
        json!("invalid_json")
    );

    let bad_jsonrpc = next_response(&mut daemon);
    assert_eq!(bad_jsonrpc["id"], json!(22));
    assert_eq!(bad_jsonrpc["error"]["code"], json!(-32600));
    assert_eq!(
        bad_jsonrpc["error"]["data"]["aether_code"],
        json!("invalid_value")
    );

    let missing_method = next_response(&mut daemon);
    assert_eq!(missing_method["id"], json!(23));
    assert_eq!(missing_method["error"]["code"], json!(-32600));

    let first_use = next_response(&mut daemon);
    assert!(
        first_use.get("result").is_some(),
        "首次 id 必须成功：{first_use}"
    );

    let duplicate = next_response(&mut daemon);
    assert_eq!(duplicate["id"], json!(23));
    assert_eq!(duplicate["error"]["code"], json!(-32600));
    assert_eq!(
        duplicate["error"]["data"]["reason"],
        json!("duplicate_request_id")
    );

    let unknown = next_response(&mut daemon);
    assert_eq!(unknown["id"], json!(24));
    assert_eq!(unknown["error"]["code"], json!(-32601));
    assert_eq!(
        unknown["error"]["data"]["aether_code"],
        json!("not_implemented")
    );

    // 连接与进程保持：继续可服务。
    let response = daemon.request(99, "startup_get", Value::Null);
    assert!(
        response.get("result").is_some(),
        "错误帧不得杀死 daemon：{response}"
    );
    assert_eq!(daemon.shutdown(), 0);
}

#[test]
fn oversized_frame_only_fails_the_request() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn(&dirs);
    daemon.initialize(1);
    daemon.wait_ready();
    let pid = daemon.id();

    // >2MiB 单行：完整读到上限后丢弃，不重启进程。
    let mut oversized = Vec::with_capacity(2 * 1024 * 1024 + 64);
    oversized.extend_from_slice(
        b"{\"jsonrpc\":\"2.0\",\"id\":77,\"method\":\"startup_get\",\"params\":{\"pad\":\"",
    );
    oversized.extend_from_slice(&vec![b'a'; 2 * 1024 * 1024]);
    oversized.extend_from_slice(b"\"}}");
    daemon.send_raw(&oversized);

    let frame = daemon
        .try_next_frame(FRAME_TIMEOUT)
        .expect("超限必须回错误帧");
    assert_eq!(frame["error"]["code"], json!(-32003));
    assert_eq!(frame["error"]["data"]["aether_code"], json!("too_large"));
    assert_eq!(
        frame["error"]["data"]["details"]["limit"],
        json!(2 * 1024 * 1024)
    );

    // 断言重连续读：同一进程、同一连接继续服务。
    let response = daemon.request(78, "startup_get", Value::Null);
    assert!(
        response.get("result").is_some(),
        "超限后必须继续可读：{response}"
    );
    assert_eq!(daemon.id(), pid, "不得重启 daemon");
    assert_eq!(daemon.shutdown(), 0);
}

#[test]
fn pre_initialize_business_request_is_core_not_ready_then_initialize_works() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn(&dirs);
    let response = daemon.request(2, "startup_get", Value::Null);
    assert_eq!(response["error"]["code"], json!(-32002));
    assert_eq!(
        response["error"]["data"]["aether_code"],
        json!("core_not_ready")
    );

    let response = daemon.request(
        3,
        "initialize",
        harness::initialize_request(3)["params"].clone(),
    );
    assert!(
        response.get("result").is_some(),
        "initialize 后必须可用：{response}"
    );
    assert_eq!(daemon.shutdown(), 0);
}

#[test]
fn second_daemon_on_same_data_dir_exits_20_with_first_frame_error() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let mut first = Daemon::spawn(&dirs);
    first.initialize(1);
    first.wait_ready();

    let mut second = Daemon::spawn(&dirs);
    let frame = second
        .try_next_frame(Duration::from_secs(30))
        .expect("第二个 daemon 必须写出唯一首帧 error");
    assert_eq!(frame["id"], Value::Null, "预初始化失败首帧 id 必须为 null");
    assert_eq!(frame["error"]["code"], json!(-32005));
    assert_eq!(
        frame["error"]["data"]["business_code"],
        json!("data_dir_locked")
    );
    assert_eq!(frame["error"]["data"]["phase"], json!("failed"));
    assert!(
        frame["error"]["data"]["log_path"]
            .as_str()
            .unwrap_or_default()
            .ends_with("aetherd.log"),
        "首帧必须携带日志路径：{frame}"
    );
    assert_eq!(second.wait_exit(Duration::from_secs(30)), Some(20));

    assert_eq!(first.shutdown(), 0);
}

#[cfg(windows)]
#[test]
fn a4_sync_dir_is_fatal_exit_21() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let onedrive = dirs.root.path().join("onedrive");
    let data_dir = onedrive.join("Aether");
    let mut env = dirs.base_env();
    env.retain(|(key, _)| key != "AETHER_DATA_DIR");
    env.push((
        "AETHER_DATA_DIR".to_owned(),
        data_dir.to_string_lossy().into_owned(),
    ));
    let mut daemon = Daemon::spawn_with_env(&env, &[("OneDrive", &onedrive.to_string_lossy())]);
    let frame = daemon
        .try_next_frame(Duration::from_secs(30))
        .expect("A4 必须写出首帧 error");
    assert_eq!(frame["error"]["code"], json!(-32005));
    assert_eq!(
        frame["error"]["data"]["aether_code"],
        json!("startup_blocked")
    );
    assert_eq!(
        frame["error"]["data"]["business_code"],
        json!("sync_dir_blocked")
    );
    assert_eq!(daemon.wait_exit(Duration::from_secs(30)), Some(21));
}

#[test]
fn db_open_failed_is_fatal_exit_22() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    std::fs::create_dir_all(dirs.data_dir()).expect("数据目录");
    std::fs::write(
        dirs.data_dir().join("aether.db"),
        b"this is not a sqlite database",
    )
    .expect("写坏库");
    let mut daemon = Daemon::spawn(&dirs);
    let frame = daemon
        .try_next_frame(Duration::from_secs(30))
        .expect("DB 打开失败必须写出首帧 error");
    assert_eq!(frame["error"]["data"]["aether_code"], json!("internal"));
    assert_eq!(
        frame["error"]["data"]["business_code"],
        json!("db_open_failed")
    );
    assert_eq!(daemon.wait_exit(Duration::from_secs(30)), Some(22));
}

#[test]
fn applied_migration_checksum_mismatch_is_fatal_exit_22() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    std::fs::create_dir_all(dirs.data_dir()).expect("数据目录");
    let db = dirs.data_dir().join("aether.db");
    {
        let _store = aether_store::Store::open(&db).expect("建库并迁移");
    }
    {
        let conn = rusqlite::Connection::open(&db).expect("直连");
        conn.execute(
            "UPDATE schema_migrations SET checksum = 'tampered' WHERE version = 1",
            [],
        )
        .expect("篡改 checksum");
    }
    let mut daemon = Daemon::spawn(&dirs);
    let frame = daemon
        .try_next_frame(Duration::from_secs(30))
        .expect("checksum 不符必须写出首帧 error");
    assert_eq!(
        frame["error"]["data"]["business_code"],
        json!("schema_checksum_mismatch")
    );
    assert_eq!(daemon.wait_exit(Duration::from_secs(30)), Some(22));
}

#[test]
fn initialize_handshake_meets_hard_timeout_and_records_target() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn(&dirs);
    let started = std::time::Instant::now();
    let response = daemon.initialize(1);
    let elapsed = started.elapsed();
    assert!(
        response.get("result").is_some(),
        "initialize 必须成功：{response}"
    );
    // ADR-029 §3.5：目标 ≤2s、硬超时 ≥10s（超时重试在主进程；M9-07/G7 记录超时率）。
    eprintln!(
        "initialize 握手耗时 {}ms（目标 ≤2000ms）",
        elapsed.as_millis()
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "initialize 超过硬超时（{:?}）",
        elapsed
    );
    assert_eq!(daemon.shutdown(), 0);
}

#[test]
fn daemon_status_notifications_follow_phase_transitions() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn(&dirs);
    daemon.initialize(1);
    daemon.wait_ready();
    assert!(
        daemon
            .wait_for_status("ready", Duration::from_secs(30))
            .is_some(),
        "预热完成必须推送 daemon.status ready（ADR-029 §4.4）"
    );
    daemon.send(&json!({"jsonrpc": "2.0", "id": 9001, "method": "shutdown"}));
    let accepted = daemon.wait_for_id(9001, FRAME_TIMEOUT);
    assert_eq!(accepted["result"]["accepted"], json!(true));
    assert!(
        daemon
            .wait_for_status("shutting_down", Duration::from_secs(30))
            .is_some(),
        "shutdown 必须推送 daemon.status shutting_down"
    );
    assert_eq!(daemon.wait_exit(FRAME_TIMEOUT), Some(0));
}

#[test]
fn quick_check_failure_enters_read_only_safe_mode_without_exiting() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    std::fs::create_dir_all(dirs.data_dir()).expect("数据目录");
    let db = dirs.data_dir().join("aether.db");
    {
        let _store = aether_store::Store::open(&db).expect("建库并迁移");
        let conn = rusqlite::Connection::open(&db).expect("直连");
        conn.execute_batch(
            "CREATE TABLE corrupt_probe (x BLOB);\n\
             WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 120)\n\
             INSERT INTO corrupt_probe (x) SELECT randomblob(2000) FROM seq;\n\
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .expect("构造损坏样本");
        let page_size: i64 = conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))
            .expect("page_size");
        let page_count: i64 = conn
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .expect("page_count");
        assert!(page_count > 4, "样本库必须多页");
        let offset = u64::try_from((page_count - 1) * page_size).expect("offset");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&db)
            .expect("打开库文件");
        file.seek(SeekFrom::Start(offset)).expect("seek");
        file.write_all(&[0xFF; 256]).expect("破坏末页");
        file.flush().expect("flush");
    }
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }

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

    // 轮询 health 直到后端注入完成（预热阶段），断言 persist_degraded 且进程存活。
    let deadline = std::time::Instant::now() + FRAME_TIMEOUT;
    let mut id = 100;
    let health = loop {
        let health = daemon.request(id, "health", Value::Null);
        id += 1;
        if health.get("result").is_some() {
            break health;
        }
        assert_eq!(
            health["error"]["data"]["aether_code"],
            json!("core_not_ready"),
            "预热期只允许 core_not_ready：{health}"
        );
        assert!(std::time::Instant::now() < deadline, "等待安全模式超时");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        health["result"]["storage_state"],
        json!("persist_degraded"),
        "quick_check 失败 → 只读安全模式（persist_degraded/S-08）：{health}"
    );
    assert_eq!(daemon.shutdown(), 0, "安全模式仍可干净退出");
}
