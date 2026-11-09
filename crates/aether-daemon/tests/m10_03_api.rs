//! M10-03 进程级 HTTP/WS/CLI 验收（ADR-033 v1.1 D-033-1～D-033-6）。
//!
//! 覆盖：
//! - loopback 监听 + 发现文件 + bearer token（无 token 401）；
//! - `/v1/meta` 只列 daemon 命令（59；不含 shell 命令）；
//! - `/v1/rpc` JSON-RPC 成功/参数错误/未知方法；`health.api=listening`；
//! - CORS 预检 403、body >2MiB 413；
//! - token 轮换（旧 token 立即失效；发现文件同步）；
//! - WS 首帧 auth + subscribe + `aether.event` 9 字段；
//! - `aetherd --cli health` 与 `--http-only` headless 路径。
//!
//! 进程级用例串行执行（每个用例独立临时数据目录 + 随机端口）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::path::Path;
use std::time::{Duration, Instant};

use harness::{Daemon, TestDirs, FRAME_TIMEOUT};
use serde_json::{json, Value};

const SESSION_ID: &str = "01J00000000000000000000M03";

fn seed_database(data_dir: &Path) {
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

#[derive(Debug, Clone)]
struct Discovery {
    port: u16,
    token: String,
}

fn wait_discovery(dirs: &TestDirs) -> Discovery {
    let path = dirs.data_dir().join(".aetherd-api.json");
    let deadline = Instant::now() + FRAME_TIMEOUT;
    loop {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                if let (Some(port), Some(token)) = (
                    value.get("port").and_then(Value::as_u64),
                    value.get("token").and_then(Value::as_str),
                ) {
                    return Discovery {
                        port: u16::try_from(port).expect("port"),
                        token: token.to_owned(),
                    };
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "等待发现文件超时：{}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn post_json(agent: &ureq::Agent, url: &str, token: Option<&str>, body: &Value) -> (u16, Value) {
    let mut request = agent.post(url).header("Content-Type", "application/json");
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request.send(body.to_string()).expect("HTTP 请求");
    let status = response.status().as_u16();
    let text = response.into_body().read_to_string().expect("读取响应体");
    let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}));
    (status, value)
}

fn get_json(agent: &ureq::Agent, url: &str, token: Option<&str>) -> (u16, Value) {
    let mut request = agent.get(url);
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request.call().expect("HTTP GET");
    let status = response.status().as_u16();
    let text = response.into_body().read_to_string().expect("读取响应体");
    let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}));
    (status, value)
}

#[test]
fn http_api_auth_meta_rpc_limits_and_rotation() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    seed_database(&dirs.data_dir());
    let mut daemon = Daemon::spawn_with_args(&dirs.env(), &[], &["--stdio", "--http"]);
    daemon.initialize(1);
    let health = daemon.wait_ready();
    assert_eq!(health["result"]["storage_state"], json!("normal"));
    assert!(
        daemon.wait_for_status("ready", FRAME_TIMEOUT).is_some(),
        "必须收到 ready 阶段通知"
    );

    let discovery = wait_discovery(&dirs);
    let base = format!("http://127.0.0.1:{}", discovery.port);
    let client = agent();

    // 1) 无 token → 401；错 token → 401。
    let (status, _) = get_json(&client, &format!("{base}/v1/meta"), None);
    assert_eq!(status, 401, "无 token 必须 401");
    let (status, _) = get_json(&client, &format!("{base}/v1/meta"), Some("wrong"));
    assert_eq!(status, 401, "错 token 必须 401");

    // 2) /v1/meta 只列 59 daemon 命令；不含 shell 命令。
    let (status, meta) = get_json(&client, &format!("{base}/v1/meta"), Some(&discovery.token));
    assert_eq!(status, 200, "meta：{meta}");
    let commands = meta["commands"].as_array().expect("commands");
    assert_eq!(commands.len(), 75, "meta 必须只列 75 daemon 命令");
    assert!(meta["commands"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item.get("timeout_ms").and_then(Value::as_u64).is_some()));
    assert!(
        !commands
            .iter()
            .any(|item| item["name"] == json!("app_exit")),
        "shell 命令不得出现在 /v1/meta"
    );
    assert_eq!(meta["protocol"], json!("1.0"));

    // 3) health：HTTP 结果与 stdio 同源，并注入 api 状态。
    let (status, health) = post_json(
        &client,
        &format!("{base}/v1/rpc"),
        Some(&discovery.token),
        &json!({"jsonrpc": "2.0", "id": 1, "method": "health", "params": null}),
    );
    assert_eq!(status, 200, "health：{health}");
    assert_eq!(health["result"]["storage_state"], json!("normal"));
    assert_eq!(health["result"]["api"]["state"], json!("listening"));
    assert_eq!(
        health["result"]["api"]["port"],
        json!(discovery.port),
        "health.api.port 必须与发现文件一致"
    );

    // 4) 未知方法 / 参数错误：HTTP 200 + 结构化 error（与 stdio 一致）。
    let (status, unknown) = post_json(
        &client,
        &format!("{base}/v1/rpc"),
        Some(&discovery.token),
        &json!({"jsonrpc": "2.0", "id": 2, "method": "not_a_command", "params": null}),
    );
    assert_eq!(status, 200, "业务错误保持 HTTP 200：{unknown}");
    assert_eq!(unknown["error"]["code"], json!(-32601));
    assert_eq!(
        unknown["error"]["data"]["aether_code"],
        json!("not_implemented")
    );

    let (status, invalid) = post_json(
        &client,
        &format!("{base}/v1/rpc"),
        Some(&discovery.token),
        &json!({"jsonrpc": "2.0", "id": 3, "method": "session_create", "params": {}}),
    );
    assert_eq!(status, 200, "参数错误保持 HTTP 200：{invalid}");
    assert_eq!(invalid["error"]["code"], json!(-32602));
    assert_eq!(
        invalid["error"]["data"]["aether_code"],
        json!("missing_field")
    );

    // 5) CORS 预检默认拒绝；body > 2MiB → 413。
    let response = client
        .options(&format!("{base}/v1/rpc"))
        .header("Origin", "http://evil.example")
        .call()
        .expect("OPTIONS");
    assert_eq!(response.status().as_u16(), 403, "CORS 预检必须默认拒绝");

    let oversized = "x".repeat(2 * 1024 * 1024 + 1);
    let (status, _) = post_json(
        &client,
        &format!("{base}/v1/rpc"),
        Some(&discovery.token),
        &json!({"jsonrpc": "2.0", "id": 4, "method": "health", "params": oversized}),
    );
    assert_eq!(status, 413, "body 超 2MiB 必须 413");

    // 6) token 轮换：旧 token 立即失效，发现文件同步新 token。
    let (status, rotated) = post_json(
        &client,
        &format!("{base}/v1/admin/rotate-token"),
        Some(&discovery.token),
        &json!({}),
    );
    assert_eq!(status, 200, "rotate：{rotated}");
    assert!(rotated.get("token").and_then(Value::as_str).is_some());
    let rotated_discovery = wait_discovery(&dirs);
    assert_ne!(
        rotated_discovery.token, discovery.token,
        "发现文件必须同步新 token"
    );
    let (status, _) = get_json(
        &client,
        &format!("{base}/v1/health"),
        Some(&discovery.token),
    );
    assert_eq!(status, 401, "旧 token 必须立即失效");
    let (status, _) = get_json(
        &client,
        &format!("{base}/v1/health"),
        Some(&rotated_discovery.token),
    );
    assert_eq!(status, 200, "新 token 必须可用");

    assert_eq!(daemon.shutdown(), 0, "daemon 必须干净退出");
    assert!(
        !dirs.data_dir().join(".aetherd-api.json").exists(),
        "关闭后必须删除发现文件"
    );
}

#[test]
fn websocket_event_stream_uses_same_envelope_and_seq() {
    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    seed_database(&dirs.data_dir());
    let mut daemon = Daemon::spawn_with_args(&dirs.env(), &[], &["--stdio", "--http"]);
    daemon.initialize(1);
    let _ = daemon.wait_ready();
    assert!(daemon.wait_for_status("ready", FRAME_TIMEOUT).is_some());
    let discovery = wait_discovery(&dirs);
    let client = agent();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio");
    runtime.block_on(async {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (mut socket, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{}/v1/events",
            discovery.port
        ))
        .await
        .expect("WS 连接");
        socket
            .send(Message::Text(
                json!({"type": "auth", "token": discovery.token})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("auth");
        socket
            .send(Message::Text(
                json!({"type": "subscribe", "session_id": SESSION_ID})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("subscribe");
        // 给服务端处理 auth/subscribe 的时间（同进程测试，非时延断言）。
        tokio::time::sleep(Duration::from_millis(200)).await;

        let (status, renamed) = post_json(
            &client,
            &format!("http://127.0.0.1:{}/v1/rpc", discovery.port),
            Some(&discovery.token),
            &json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "session_rename",
                "params": {"session_id": SESSION_ID, "title": "M10-03 WS"}
            }),
        );
        assert_eq!(status, 200, "rename：{renamed}");
        assert!(
            renamed.get("result").is_some(),
            "rename 必须成功：{renamed}"
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let message = tokio::time::timeout_at(deadline, socket.next())
                .await
                .expect("等待 WS 事件超时")
                .expect("WS 流结束")
                .expect("WS 消息");
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).expect("WS JSON");
            if value["type"] != json!("event") {
                continue;
            }
            let envelope = value["envelope"].as_object().expect("envelope");
            assert_eq!(envelope.len(), 9, "EventEnvelope 必须 9 字段：{envelope:?}");
            for field in aether_core::ENVELOPE_FIELDS {
                assert!(envelope.contains_key(field), "缺少字段 {field}");
            }
            assert_eq!(envelope["session_id"], json!(SESSION_ID));
            assert_eq!(envelope["type"], json!("session.updated"));
            assert_eq!(envelope["payload"]["title"], json!("M10-03 WS"));
            break;
        }

        let _ = socket.close(None).await;
    });

    assert_eq!(daemon.shutdown(), 0);
}

#[test]
fn multi_client_consistency_concurrency_and_port_conflict() {
    use std::time::Instant as StdInstant;

    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    seed_database(&dirs.data_dir());
    let mut daemon = Daemon::spawn_with_args(&dirs.env(), &[], &["--stdio", "--http"]);
    daemon.initialize(1);
    let _ = daemon.wait_ready();
    assert!(daemon.wait_for_status("ready", FRAME_TIMEOUT).is_some());
    let discovery = wait_discovery(&dirs);
    let base = format!("http://127.0.0.1:{}", discovery.port);
    let client = agent();

    // stdio vs HTTP vs CLI：同一业务结果 0 差异。
    let stdio_response = daemon.request(100, "session_list", json!({}));
    assert!(
        stdio_response.get("result").is_some(),
        "stdio session_list 失败：{stdio_response}"
    );
    let stdio_list = stdio_response["result"].clone();
    let (status, http_list) = post_json(
        &client,
        &format!("{base}/v1/rpc"),
        Some(&discovery.token),
        &json!({"jsonrpc": "2.0", "id": 100, "method": "session_list", "params": {}}),
    );
    assert_eq!(status, 200, "HTTP session_list：{http_list}");
    assert_eq!(
        http_list["result"], stdio_list,
        "stdio/HTTP session_list 必须一致"
    );

    let cli = std::process::Command::new(env!("CARGO_BIN_EXE_aetherd"))
        .args([
            "--cli",
            "session",
            "list",
            "--url",
            &base,
            "--token",
            &discovery.token,
        ])
        .output()
        .expect("CLI session list");
    assert!(
        cli.status.success(),
        "CLI 失败：{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_value: Value = serde_json::from_slice(&cli.stdout).expect("CLI JSON");
    assert_eq!(
        cli_value,
        stdio_list,
        "stdio/CLI session_list 必须一致；stdout={} stderr={}",
        String::from_utf8_lossy(&cli.stdout),
        String::from_utf8_lossy(&cli.stderr)
    );

    // 10 并发客户端：互不串流、全部成功。
    let mut handles = Vec::new();
    for index in 0..10 {
        let base = base.clone();
        let token = discovery.token.clone();
        handles.push(std::thread::spawn(move || {
            let client = agent();
            let (status, value) = post_json(
                &client,
                &format!("{base}/v1/rpc"),
                Some(&token),
                &json!({"jsonrpc": "2.0", "id": index, "method": "health", "params": null}),
            );
            (status, value)
        }));
    }
    for handle in handles {
        let (status, value) = handle.join().expect("并发线程");
        assert_eq!(status, 200, "并发 health：{value}");
        assert_eq!(value["result"]["storage_state"], json!("normal"));
        assert_eq!(value["result"]["api"]["state"], json!("listening"));
    }

    // stdio 事件与 WS 事件：同一 EventEnvelope 0 差异。
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio");
    runtime.block_on(async {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let (mut socket, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{}/v1/events",
            discovery.port
        ))
        .await
        .expect("WS 连接");
        socket
            .send(Message::Text(
                json!({"type": "auth", "token": discovery.token})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("auth");
        socket
            .send(Message::Text(
                json!({"type": "subscribe", "session_id": SESSION_ID})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let (status, renamed) = post_json(
            &client,
            &format!("{base}/v1/rpc"),
            Some(&discovery.token),
            &json!({
                "jsonrpc": "2.0",
                "id": 101,
                "method": "session_rename",
                "params": {"session_id": SESSION_ID, "title": "M10-03 4-way"}
            }),
        );
        assert_eq!(status, 200, "rename：{renamed}");

        let stdio_event = next_stdio_event(&mut daemon);
        let deadline = StdInstant::now() + Duration::from_secs(20);
        let ws_event = loop {
            assert!(StdInstant::now() < deadline, "等待 WS 事件超时");
            let message = tokio::time::timeout(Duration::from_secs(20), socket.next())
                .await
                .expect("WS 超时")
                .expect("WS 结束")
                .expect("WS 消息");
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).expect("WS JSON");
            if value["type"] == json!("event") {
                break value["envelope"].clone();
            }
        };
        assert_eq!(
            ws_event, stdio_event,
            "stdio/WS 的 EventEnvelope 必须 0 差异"
        );
        let _ = socket.close(None).await;
    });

    assert_eq!(daemon.shutdown(), 0);

    // 固定端口占用：--http-only 必须退出码 25（明确报错，不静默换端口）。
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("占用端口");
    let port = occupied.local_addr().expect("地址").port();
    let port_text = port.to_string();
    let conflict_dirs = TestDirs::new();
    let mut conflict = Daemon::spawn_with_args(
        &conflict_dirs.env(),
        &[("AETHERD_HTTP_PORT", &port_text)],
        &["--http-only"],
    );
    let exit = conflict.wait_exit(FRAME_TIMEOUT);
    assert_eq!(exit, Some(25), "端口冲突必须退出码 25（实际 {exit:?}）");
    drop(occupied);
}

fn next_stdio_event(daemon: &mut Daemon) -> Value {
    if let Some(index) = daemon
        .notifications
        .iter()
        .position(|frame| frame.get("method").and_then(Value::as_str) == Some("aether.event"))
    {
        return daemon.notifications.remove(index)["params"].clone();
    }
    let deadline = Instant::now() + FRAME_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "等待 aether.event 超时");
        let frame = daemon.try_next_frame(remaining).expect("aether.event 帧");
        if frame.get("method").and_then(Value::as_str) == Some("aether.event") {
            return frame["params"].clone();
        }
        daemon.notifications.push(frame);
    }
}

#[test]
fn http_listener_failure_is_fatal_only_when_required() {
    let _serial = harness::serial_guard();
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("占用端口");
    let port = occupied.local_addr().expect("地址").port();
    let port_text = port.to_string();

    // AETHERD_HTTP_REQUIRED=1：listener 失败 → 退出码 25。
    let required_dirs = TestDirs::new();
    let mut required = Daemon::spawn_with_args(
        &required_dirs.env(),
        &[
            ("AETHERD_HTTP_PORT", &port_text),
            ("AETHERD_HTTP_REQUIRED", "1"),
        ],
        &["--stdio", "--http"],
    );
    assert_eq!(required.wait_exit(FRAME_TIMEOUT), Some(25));

    // 非必需：stdio 继续，health.api.state=error。
    let dirs = TestDirs::new();
    let mut daemon = Daemon::spawn_with_args(
        &dirs.env(),
        &[("AETHERD_HTTP_PORT", &port_text)],
        &["--stdio", "--http"],
    );
    daemon.initialize(1);
    let health = daemon.wait_ready();
    assert_eq!(health["result"]["api"]["state"], json!("error"));
    assert!(health["result"]["api"]["last_error"].as_str().is_some());
    assert_eq!(daemon.shutdown(), 0);
    drop(occupied);
}

#[test]
fn cli_events_streams_jsonl_from_native_ws() {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;

    let _serial = harness::serial_guard();
    let dirs = TestDirs::new();
    seed_database(&dirs.data_dir());
    let mut daemon = Daemon::spawn_with_args(&dirs.env(), &[], &["--stdio", "--http"]);
    daemon.initialize(1);
    let _ = daemon.wait_ready();
    assert!(daemon.wait_for_status("ready", FRAME_TIMEOUT).is_some());
    let discovery = wait_discovery(&dirs);
    let client = agent();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_aetherd"))
        .args([
            "--cli",
            "events",
            "--session",
            SESSION_ID,
            "--url",
            &format!("http://127.0.0.1:{}", discovery.port),
            "--token",
            &discovery.token,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("启动 CLI events");
    let stdout = child.stdout.take().expect("CLI stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    std::thread::sleep(Duration::from_millis(500));

    let (status, renamed) = post_json(
        &client,
        &format!("http://127.0.0.1:{}/v1/rpc", discovery.port),
        Some(&discovery.token),
        &json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "session_rename",
            "params": {"session_id": SESSION_ID, "title": "M10-03 CLI events"}
        }),
    );
    assert_eq!(status, 200, "rename：{renamed}");

    let line = receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("CLI events 必须输出 JSONL");
    let value: Value = serde_json::from_str(&line).expect("JSONL");
    assert_eq!(value["session_id"], json!(SESSION_ID));
    assert_eq!(value["type"], json!("session.updated"));
    assert_eq!(value.as_object().expect("envelope").len(), 9);
    assert_eq!(value["payload"]["title"], json!("M10-03 CLI events"));

    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(daemon.shutdown(), 0);
}

#[test]
fn cli_health_and_http_only_headless() {
    let _serial = harness::serial_guard();

    // 1) --http-only：无 stdio initialize，自动预热；CLI 走同一 HTTP 核心。
    let dirs = TestDirs::new();
    let daemon = Daemon::spawn_with_args(&dirs.env(), &[], &["--http-only"]);
    let discovery = wait_discovery(&dirs);
    let client = agent();
    let deadline = Instant::now() + FRAME_TIMEOUT;
    let health = loop {
        let (status, value) = get_json(
            &client,
            &format!("http://127.0.0.1:{}/v1/health", discovery.port),
            Some(&discovery.token),
        );
        if status == 200 {
            break value;
        }
        assert!(Instant::now() < deadline, "http-only 未就绪：{value}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(health["api"]["state"], json!("listening"));
    assert_eq!(health["storage_state"], json!("normal"));
    assert!(
        daemon.stdout_lines().is_empty(),
        "--http-only 不应输出 stdio JSONL：{:?}",
        daemon.stdout_lines()
    );

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aetherd"))
        .args([
            "--cli",
            "health",
            "--url",
            &format!("http://127.0.0.1:{}", discovery.port),
            "--token",
            &discovery.token,
        ])
        .output()
        .expect("运行 CLI");
    assert!(
        output.status.success(),
        "CLI 必须成功：stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(text.trim()).expect("CLI JSON 输出");
    assert_eq!(value["api"]["state"], json!("listening"));

    // CLI 错误路径：未知命令 → 退出码 3；无 token → 4。
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aetherd"))
        .args([
            "--cli",
            "session",
            "list",
            "--url",
            &format!("http://127.0.0.1:{}", discovery.port),
            "--token",
            "wrong",
        ])
        .output()
        .expect("运行 CLI");
    assert_eq!(output.status.code(), Some(4), "错 token 必须退出码 4");

    daemon.kill();
}
