//! M9-03 golden 帧回放（DoD-1/DoD-2/DoD-7）：`scripts/test/m9-03/fixtures/` 的 JSONL
//! 请求帧在 `aether-rpc::Connection` 上重放，逐条断言 `error.code` / `data.aether_code`
//! / `details`（不含动态版本/路径/时间）。
//!
//! 业务方法用 [`FakeServer`] 承接（本层不验证 63 命令语义；命令语义在 `aether-daemon`
//! 的进程级测试与 M9-04 矩阵中验收）。

// 集成测试文件级豁免（与仓库既有集成测试同口径：断言失败即 panic）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use aether_rpc::{Connection, DaemonPhase, Frame, FrameReader, Handled, RpcFailure, Server};
use serde_json::{json, Value};

struct FakeServer {
    phase: DaemonPhase,
}

impl FakeServer {
    fn new() -> Self {
        Self {
            phase: DaemonPhase::Warming,
        }
    }
}

impl Server for FakeServer {
    fn phase(&self) -> DaemonPhase {
        self.phase
    }

    fn data_dir(&self) -> String {
        r"C:\Aether".to_owned()
    }

    fn daemon_version(&self) -> String {
        "0.1.0".to_owned()
    }

    fn core_version(&self) -> String {
        "0.1.0".to_owned()
    }

    fn shutdown_id(&mut self) -> String {
        "shutdown-fixture".to_owned()
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcFailure> {
        match method {
            "unknown_method" => Err(RpcFailure::method_not_found(method)),
            // 模拟命令层的严格解析（`deny_unknown_fields`）：`params` 是内层 DTO，
            // **不得**存在 `{payload}` 包装（ADR-029 P1-5）。
            "session_create" => {
                if params.get("payload").is_some() {
                    return Err(RpcFailure::invalid_params(
                        "unknown_field",
                        "出现未声明字段；严格模式下未知字段一律拒绝",
                    )
                    .with_detail("field", "payload"));
                }
                Err(RpcFailure::application(
                    "no_candidate_runtime",
                    "没有可用的运行时（fixture 替身）",
                ))
            }
            "startup_get" => Ok(json!({"phase": "ready", "data_dir_source": "env_override"})),
            other => Ok(json!({"ok": true, "method": other})),
        }
    }
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts")
        .join("test")
        .join("m9-03")
        .join("fixtures")
}

fn expectations() -> Value {
    let text = std::fs::read_to_string(fixture_root().join("expectations.json"))
        .expect("读取 expectations.json");
    serde_json::from_str(&text).expect("解析 expectations.json")
}

fn fixture_lines(name: &str) -> Vec<String> {
    let text = std::fs::read_to_string(fixture_root().join(name))
        .unwrap_or_else(|error| panic!("读取 {name}：{error}"));
    text.split('\n')
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn line_of(file: &str, index: usize) -> Vec<u8> {
    let lines = fixture_lines(file);
    lines
        .get(index)
        .unwrap_or_else(|| panic!("{file} 缺少第 {index} 行"))
        .clone()
        .into_bytes()
}

fn handled_value(handled: Handled) -> Value {
    match handled {
        Handled::Reply(value) => value,
        Handled::ReplyAndClose(value) => value,
    }
}

fn is_close(handled: &Handled) -> bool {
    matches!(handled, Handled::ReplyAndClose(_))
}

fn assert_case(case: &Value, value: &Value, close: bool) {
    let id = case["id"].as_str().unwrap_or("<unnamed>");
    let expect = &case["expect"];
    match expect["kind"].as_str().unwrap_or_default() {
        "result" => {
            assert!(
                value.get("error").is_none(),
                "[{id}] 期望成功，实际 {value}"
            );
            if let Some(keys) = expect["result_keys"].as_array() {
                let result = value["result"].as_object().unwrap_or_else(|| {
                    panic!("[{id}] 结果必须是对象：{value}");
                });
                let expected: Vec<&str> = keys.iter().filter_map(Value::as_str).collect();
                let mut actual: Vec<&str> = result.keys().map(String::as_str).collect();
                actual.sort_unstable();
                let mut expected_sorted = expected.clone();
                expected_sorted.sort_unstable();
                assert_eq!(
                    actual, expected_sorted,
                    "[{id}] initialize 结果字段集合冻结"
                );
            }
            if let Some(version) = expect["protocol_version"].as_str() {
                assert_eq!(value["result"]["protocolVersion"], json!(version), "[{id}]");
            }
            if let Some(phases) = expect["phase_in"].as_array() {
                let phase = value["result"]["phase"]
                    .as_str()
                    .or_else(|| value["result"].get("phase").and_then(Value::as_str))
                    .unwrap_or_default();
                let allowed: Vec<&str> = phases.iter().filter_map(Value::as_str).collect();
                assert!(
                    allowed.contains(&phase),
                    "[{id}] phase={phase} 不在 {allowed:?}"
                );
            }
            if let Some(capabilities) = expect["daemon_capabilities"].as_array() {
                assert_eq!(
                    value["result"]["daemonCapabilities"],
                    Value::Array(capabilities.clone()),
                    "[{id}]"
                );
            }
            if expect["accepted"].as_bool() == Some(true) {
                assert_eq!(value["result"]["accepted"], json!(true), "[{id}]");
            }
            if expect["close"].as_bool() == Some(true) {
                assert!(close, "[{id}] shutdown 必须进入关闭序列");
            }
        }
        "error" => {
            let error = value.get("error").unwrap_or_else(|| {
                panic!("[{id}] 期望错误，实际 {value}");
            });
            if let Some(code) = expect["transport"].as_i64() {
                assert_eq!(error["code"], json!(code), "[{id}] 传输码");
            }
            if let Some(aether) = expect["aether_code"].as_str() {
                assert_eq!(error["data"]["aether_code"], json!(aether), "[{id}]");
            }
            if let Some(reason) = expect["reason"].as_str() {
                assert_eq!(error["data"]["reason"], json!(reason), "[{id}]");
            }
            if let Some(field) = expect["details_field"].as_str() {
                assert_eq!(error["data"]["details"]["field"], json!(field), "[{id}]");
            }
        }
        "anything-but" => {
            if let Some(error) = value.get("error") {
                let forbidden: Vec<&str> = expect["forbidden_aether_codes"]
                    .as_array()
                    .map(|items| items.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let code = error["data"]["aether_code"].as_str().unwrap_or_default();
                assert!(
                    !forbidden.contains(&code),
                    "[{id}] 内层 DTO 不得被形状拒绝（{code}）：{value}"
                );
            }
        }
        other => panic!("[{id}] 未知期望类型：{other}"),
    }
}

#[test]
fn golden_fixtures_replay_on_connection_state_machine() {
    let expectations = expectations();
    assert_eq!(
        expectations["schema"], "aether.m9-03.golden.v1",
        "fixture schema 版本"
    );
    let cases = expectations["cases"].as_array().expect("cases 数组");
    assert!(cases.len() >= 15, "用例数量");

    let mut connection = Connection::new();
    let mut server = FakeServer::new();
    for case in cases {
        if case["reset"].as_bool() == Some(true) {
            connection = Connection::new();
            server = FakeServer::new();
        }
        let file = case["file"].as_str().expect("file");
        let line = case["line"].as_u64().expect("line") as usize;
        let raw = line_of(file, line);
        let handled = connection.handle(&raw, &mut server);
        let close = is_close(&handled);
        let value = handled_value(handled);
        assert_case(case, &value, close);
        if case["expect"]["close"].as_bool() == Some(true) {
            assert!(
                connection.shutting_down(),
                "shutdown 后必须进入 shutting_down"
            );
        }
    }
}

#[test]
fn pre_initialize_business_request_is_core_not_ready() {
    let expectations = expectations();
    let cases = expectations["pre_initialize_cases"]
        .as_array()
        .expect("pre_initialize_cases");
    for case in cases {
        let mut connection = Connection::new();
        let mut server = FakeServer::new();
        let raw = line_of(
            case["file"].as_str().expect("file"),
            case["line"].as_u64().expect("line") as usize,
        );
        let value = handled_value(connection.handle(&raw, &mut server));
        assert_case(case, &value, false);
        assert!(!connection.initialized(), "预初始化请求不得改变连接状态");
    }
}

#[test]
fn oversized_frame_matches_golden_expectation_and_keeps_connection() {
    let expectations = expectations();
    let oversized = &expectations["oversized"];
    assert_eq!(
        oversized["limit_bytes"].as_u64(),
        Some(aether_rpc::MAX_FRAME_BYTES as u64)
    );
    let mut connection = Connection::new();
    let mut server = FakeServer::new();
    let value = handled_value(connection.oversized(
        aether_rpc::MAX_FRAME_BYTES,
        aether_rpc::MAX_FRAME_BYTES as u64 + 1,
    ));
    assert_case(
        &json!({
            "id": oversized["id"],
            "expect": {
                "kind": "error",
                "transport": oversized["expect"]["transport"],
                "aether_code": oversized["expect"]["aether_code"],
            }
        }),
        &value,
        false,
    );
    assert_eq!(
        value["error"]["data"]["details"]["limit"],
        oversized["expect"]["details_limit"]
    );
    // 连接仍可完成 initialize（不重启/不断连）。
    let raw = line_of("initialize.jsonl", 0);
    let value = handled_value(connection.handle(&raw, &mut server));
    assert!(value.get("result").is_some(), "超限后连接必须可继续");
}

#[test]
fn frame_reader_decodes_every_fixture_line_as_a_frame() {
    for name in [
        "initialize.jsonl",
        "params.jsonl",
        "transport.jsonl",
        "shutdown.jsonl",
    ] {
        let text = fixture_lines(name).join("\n") + "\n";
        let mut reader = FrameReader::new(text.as_bytes());
        let mut count = 0;
        while let Some(frame) = reader.read_frame().expect("读取帧") {
            match frame {
                Frame::Line(bytes) => {
                    assert!(!bytes.is_empty(), "{name} 第 {count} 帧为空");
                    count += 1;
                }
                Frame::Oversized { .. } => panic!("{name} 不应出现超限帧"),
            }
        }
        assert_eq!(count, fixture_lines(name).len(), "{name} 帧数");
    }
}
