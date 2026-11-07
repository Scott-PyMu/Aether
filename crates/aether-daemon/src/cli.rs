//! `aetherd --cli`：headless CLI（M10-03；ADR-033 v1.1 D-033-4）。
//!
//! 设计口径：
//! - **headless**：不依赖 Electron/Node，经 HTTP/WS 调用同一 `aether-app::dispatch`；
//! - 默认从 `<data_dir>/.aetherd-api.json` 读取端口/token（同用户）；也支持 `--url`；
//! - token 优先级：`--token` > `AETHERD_API_TOKEN` > `--token-file`（0600）> 发现文件；
//! - **不读取/打印 token**（`api rotate-token` 只输出轮换结果，token 留在发现文件）；
//! - 退出码：0 成功；1 内部错误；2 用法/参数；3 未找到；4 鉴权失败；
//!   5 服务不可用/未就绪；6 超时；7 取消。

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use aether_app::startup::pointer;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::api::DISCOVERY_FILE_NAME;

/// CLI 调用上下文（连接信息 + 输出偏好）。
#[derive(Debug, Clone)]
struct Endpoint {
    base: String,
    token: String,
}

/// CLI 失败（携带退出码与结构化错误；避免把 token/请求体写入 stderr）。
#[derive(Debug, Clone)]
struct CliFailure {
    exit_code: u8,
    message: String,
    error: Option<Value>,
}

impl CliFailure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            exit_code: 2,
            message: message.into(),
            error: None,
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            exit_code: 1,
            message: message.into(),
            error: None,
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            exit_code: 5,
            message: message.into(),
            error: None,
        }
    }

    fn auth(message: impl Into<String>) -> Self {
        Self {
            exit_code: 4,
            message: message.into(),
            error: None,
        }
    }

    fn timeout(message: impl Into<String>) -> Self {
        Self {
            exit_code: 6,
            message: message.into(),
            error: None,
        }
    }

    fn rpc(error: Value) -> Self {
        let exit_code = rpc_exit_code(&error);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("RPC 调用失败")
            .to_owned();
        Self {
            exit_code,
            message,
            error: Some(error),
        }
    }
}

fn rpc_exit_code(error: &Value) -> u8 {
    let code = error
        .get("data")
        .and_then(|data| {
            data.get("business_code")
                .and_then(Value::as_str)
                .or_else(|| data.get("aether_code").and_then(Value::as_str))
        })
        .unwrap_or_default();
    if code == "request_timeout" {
        return 6;
    }
    if code == "core_not_ready" {
        return 5;
    }
    if code == "not_implemented" || code.ends_with("_not_found") {
        return 3;
    }
    if error.get("code").and_then(Value::as_i64) == Some(-32601) {
        return 3;
    }
    1
}

/// CLI 入口（由 `aetherd --cli` 调用；返回进程退出码）。
pub fn run(args: &[String]) -> u8 {
    match execute(args) {
        Ok(code) => code,
        Err(error) => {
            if let Some(value) = &error.error {
                match serde_json::to_string(value) {
                    Ok(text) => eprintln!("{text}"),
                    Err(_) => eprintln!("{}", error.message),
                }
            } else {
                eprintln!("{}", error.message);
            }
            error.exit_code
        }
    }
}

fn execute(args: &[String]) -> Result<u8, CliFailure> {
    let (global, rest) = split_global(args)?;
    if rest.is_empty() || rest[0] == "--help" || rest[0] == "-h" {
        print_help();
        return Ok(if rest.is_empty() { 2 } else { 0 });
    }
    if rest[0] == "--version" || rest[0] == "-V" {
        println!(
            "aetherd {} (protocol {}; core {})",
            aether_app::version(),
            aether_rpc::PROTOCOL_VERSION,
            aether_app::core_version()
        );
        return Ok(0);
    }
    let endpoint = resolve_endpoint(&global)?;
    match rest[0].as_str() {
        "health" => {
            if wants_help(&rest[1..]) {
                print_group_help("health");
                return Ok(0);
            }
            let (_, _) = flags(&rest[1..], &[], &["json", "pretty"])?;
            let result = call_rpc(&endpoint, "health", Value::Null)?;
            print_json(&result, global.pretty)?;
            Ok(0)
        }
        "session" => session_command(&endpoint, &rest[1..], global.pretty),
        "workflow" => workflow_command(&endpoint, &rest[1..], global.pretty),
        "subagent" => subagent_command(&endpoint, &rest[1..], global.pretty),
        "import" => import_command(&endpoint, &rest[1..], global.pretty),
        "export" => export_command(&endpoint, &rest[1..], global.pretty),
        "events" => events_command(&global, &endpoint, &rest[1..]),
        "api" => api_command(&endpoint, &rest[1..], global.pretty),
        other => Err(CliFailure::usage(format!("未知子命令：{other}"))),
    }
}

fn session_command(endpoint: &Endpoint, args: &[String], pretty: bool) -> Result<u8, CliFailure> {
    if wants_help(args) {
        print_group_help("session");
        return Ok(0);
    }
    let Some(sub) = args.first() else {
        return Err(CliFailure::usage(
            "session 缺少子命令（list/create/send/messages/interrupt）",
        ));
    };
    match sub.as_str() {
        "list" => {
            let (_, values) = flags(
                &args[1..],
                &["runtime", "status", "limit"],
                &["json", "pretty"],
            )?;
            let mut params = Map::new();
            insert_string(&mut params, "runtime_id", values.get("runtime"));
            insert_string(&mut params, "status", values.get("status"));
            insert_u64(&mut params, "limit", values.get("limit"))?;
            let result = call_rpc(endpoint, "session_list", Value::Object(params))?;
            print_json(&result, pretty)?;
        }
        "create" => {
            let (_, values) = flags(
                &args[1..],
                &[
                    "title",
                    "runtime",
                    "provider",
                    "model",
                    "workspace",
                    "thinking-depth",
                    "permission-mode",
                    "preset",
                ],
                &["json", "pretty"],
            )?;
            let title = values
                .get("title")
                .ok_or_else(|| CliFailure::usage("session create 缺少 --title"))?;
            let mut params = Map::new();
            params.insert("title".to_owned(), Value::String(title.clone()));
            insert_string(&mut params, "runtime_id", values.get("runtime"));
            insert_string(&mut params, "provider_id", values.get("provider"));
            insert_string(&mut params, "model", values.get("model"));
            insert_string(&mut params, "workspace_id", values.get("workspace"));
            insert_i64(&mut params, "thinking_depth", values.get("thinking-depth"))?;
            insert_string(
                &mut params,
                "permission_mode",
                values.get("permission-mode"),
            );
            insert_string(&mut params, "preset_id", values.get("preset"));
            let result = call_rpc(endpoint, "session_create", Value::Object(params))?;
            print_json(&result, pretty)?;
        }
        "send" => {
            let (positional, values) = flags(
                &args[1..],
                &[
                    "text",
                    "client-msg-id",
                    "thinking-depth",
                    "provider",
                    "model",
                ],
                &["json", "pretty"],
            )?;
            let session_id = require_positional(&positional, 0, "session send <session_id>")?;
            let text = values
                .get("text")
                .ok_or_else(|| CliFailure::usage("session send 缺少 --text"))?;
            let client_msg_id = match values.get("client-msg-id") {
                Some(value) => value.clone(),
                None => generate_ulid()?,
            };
            let mut params = Map::new();
            params.insert("session_id".to_owned(), Value::String(session_id));
            params.insert("text".to_owned(), Value::String(text.clone()));
            params.insert("client_msg_id".to_owned(), Value::String(client_msg_id));
            insert_i64(&mut params, "thinking_depth", values.get("thinking-depth"))?;
            insert_string(&mut params, "provider_id", values.get("provider"));
            insert_string(&mut params, "model", values.get("model"));
            let result = call_rpc(endpoint, "session_send", Value::Object(params))?;
            print_json(&result, pretty)?;
        }
        "messages" => {
            let (positional, values) = flags(
                &args[1..],
                &["last-seq", "limit", "max-bytes", "cursor"],
                &["json", "pretty"],
            )?;
            let session_id = require_positional(&positional, 0, "session messages <session_id>")?;
            let mut params = Map::new();
            params.insert("session_id".to_owned(), Value::String(session_id));
            insert_u64(&mut params, "last_seq", values.get("last-seq"))?;
            insert_u64(&mut params, "limit", values.get("limit"))?;
            insert_u64(&mut params, "max_bytes", values.get("max-bytes"))?;
            insert_string(&mut params, "next_cursor", values.get("cursor"));
            let result = call_rpc(endpoint, "messages_page", Value::Object(params))?;
            print_json(&result, pretty)?;
        }
        "interrupt" => {
            let (positional, _values) = flags(&args[1..], &[], &["json", "pretty"])?;
            let session_id = require_positional(&positional, 0, "session interrupt <session_id>")?;
            let result = call_rpc(
                endpoint,
                "session_interrupt",
                json!({"session_id": session_id}),
            )?;
            print_json(&result, pretty)?;
        }
        other => return Err(CliFailure::usage(format!("未知 session 子命令：{other}"))),
    }
    Ok(0)
}

fn workflow_command(endpoint: &Endpoint, args: &[String], pretty: bool) -> Result<u8, CliFailure> {
    if wants_help(args) {
        print_group_help("workflow");
        return Ok(0);
    }
    let Some(sub) = args.first() else {
        return Err(CliFailure::usage(
            "workflow 缺少子命令（run/status/cancel/retry-node）",
        ));
    };
    match sub.as_str() {
        "run" => {
            let (positional, values) = flags(
                &args[1..],
                &["session", "input", "priority"],
                &["json", "pretty"],
            )?;
            let workflow_id = require_positional(&positional, 0, "workflow run <workflow_id>")?;
            let session_id = values
                .get("session")
                .ok_or_else(|| CliFailure::usage("workflow run 缺少 --session"))?;
            let mut params = Map::new();
            params.insert("workflow_id".to_owned(), Value::String(workflow_id));
            params.insert("session_id".to_owned(), Value::String(session_id.clone()));
            if let Some(input) = values.get("input") {
                let parsed: Value = serde_json::from_str(input).map_err(|error| {
                    CliFailure::usage(format!("--input 不是合法 JSON：{error}"))
                })?;
                params.insert("input".to_owned(), parsed);
            }
            insert_i64(&mut params, "priority", values.get("priority"))?;
            let result = call_rpc(endpoint, "workflow_run", Value::Object(params))?;
            print_json(&result, pretty)?;
        }
        "status" => {
            let (positional, _values) = flags(&args[1..], &[], &["json", "pretty"])?;
            let run_id = require_positional(&positional, 0, "workflow status <workflow_run_id>")?;
            let result = call_rpc(
                endpoint,
                "workflow_run_get",
                json!({"workflow_run_id": run_id, "include_nodes": true}),
            )?;
            print_json(&result, pretty)?;
        }
        "cancel" => {
            let (positional, values) = flags(&args[1..], &["reason"], &["json", "pretty"])?;
            let run_id = require_positional(&positional, 0, "workflow cancel <workflow_run_id>")?;
            let mut params = json!({"workflow_run_id": run_id});
            if let Some(reason) = values.get("reason") {
                params["reason"] = Value::String(reason.clone());
            }
            let result = call_rpc(endpoint, "workflow_run_cancel", params)?;
            print_json(&result, pretty)?;
        }
        "retry-node" => {
            let (positional, _) = flags(&args[1..], &[], &["json", "pretty"])?;
            let run_id = require_positional(
                &positional,
                0,
                "workflow retry-node <workflow_run_id> <node_id>",
            )?;
            let node_id = require_positional(
                &positional,
                1,
                "workflow retry-node <workflow_run_id> <node_id>",
            )?;
            let result = call_rpc(
                endpoint,
                "workflow_node_retry",
                json!({"workflow_run_id": run_id, "node_id": node_id}),
            )?;
            print_json(&result, pretty)?;
        }
        other => return Err(CliFailure::usage(format!("未知 workflow 子命令：{other}"))),
    }
    Ok(0)
}

fn subagent_command(endpoint: &Endpoint, args: &[String], pretty: bool) -> Result<u8, CliFailure> {
    if wants_help(args) {
        print_group_help("subagent");
        return Ok(0);
    }
    let Some(sub) = args.first() else {
        return Err(CliFailure::usage(
            "subagent 缺少子命令（tree/result/resolve）",
        ));
    };
    match sub.as_str() {
        "tree" => {
            let (positional, values) =
                flags(&args[1..], &[], &["include-completed", "json", "pretty"])?;
            let parent = require_positional(&positional, 0, "subagent tree <parent_session_id>")?;
            let include_completed = values.contains_key("include-completed");
            let result = call_rpc(
                endpoint,
                "subagent_list",
                json!({"parent_session_id": parent, "include_completed": include_completed}),
            )?;
            print_json(&result, pretty)?;
        }
        "result" => {
            let (_, values) = flags(
                &args[1..],
                &["parent", "child", "offset", "limit-bytes"],
                &["json", "pretty"],
            )?;
            let parent = values
                .get("parent")
                .ok_or_else(|| CliFailure::usage("subagent result 缺少 --parent"))?;
            let child = values
                .get("child")
                .ok_or_else(|| CliFailure::usage("subagent result 缺少 --child"))?;
            let mut params = json!({"parent_session_id": parent, "child_session_id": child});
            insert_value_u64(&mut params, "offset", values.get("offset"))?;
            insert_value_u64(&mut params, "limit_bytes", values.get("limit-bytes"))?;
            let result = call_rpc(endpoint, "subagent_result_get", params)?;
            print_json(&result, pretty)?;
        }
        "resolve" => {
            let (positional, values) =
                flags(&args[1..], &["decision", "note"], &["json", "pretty"])?;
            let child = require_positional(&positional, 0, "subagent resolve <child_session_id>")?;
            let decision = values
                .get("decision")
                .ok_or_else(|| CliFailure::usage("subagent resolve 缺少 --decision"))?;
            let mut params = json!({"child_session_id": child, "decision": decision});
            if let Some(note) = values.get("note") {
                params["note"] = Value::String(note.clone());
            }
            let result = call_rpc(endpoint, "subagent_resolve", params)?;
            print_json(&result, pretty)?;
        }
        other => return Err(CliFailure::usage(format!("未知 subagent 子命令：{other}"))),
    }
    Ok(0)
}

fn import_command(endpoint: &Endpoint, args: &[String], pretty: bool) -> Result<u8, CliFailure> {
    if wants_help(args) {
        print_group_help("import");
        return Ok(0);
    }
    let (positional, values) = flags(args, &["mode"], &["archive", "dry-run", "json", "pretty"])?;
    let source = require_positional(&positional, 0, "import <file>")?;
    let method = if values.contains_key("archive") {
        "archive_import"
    } else {
        "session_import"
    };
    let mut params = json!({"source_path": source});
    insert_value_string(&mut params, "mode", values.get("mode"));
    if values.contains_key("dry-run") {
        params["dry_run"] = Value::Bool(true);
    }
    let result = call_rpc(endpoint, method, params)?;
    print_json(&result, pretty)?;
    Ok(0)
}

fn export_command(endpoint: &Endpoint, args: &[String], pretty: bool) -> Result<u8, CliFailure> {
    if wants_help(args) {
        print_group_help("export");
        return Ok(0);
    }
    let (positional, values) = flags(args, &["target-dir"], &["json", "pretty"])?;
    let session_id = require_positional(&positional, 0, "export <session_id>")?;
    let mut params = json!({"session_id": session_id});
    insert_value_string(&mut params, "target_dir", values.get("target-dir"));
    let result = call_rpc(endpoint, "session_export", params)?;
    print_json(&result, pretty)?;
    Ok(0)
}

fn api_command(endpoint: &Endpoint, args: &[String], _pretty: bool) -> Result<u8, CliFailure> {
    if wants_help(args) {
        print_group_help("api");
        return Ok(0);
    }
    let Some(sub) = args.first() else {
        return Err(CliFailure::usage("api 缺少子命令（rotate-token）"));
    };
    match sub.as_str() {
        "rotate-token" => {
            let (_, _) = flags(&args[1..], &[], &["json", "pretty"])?;
            let url = format!("{}/v1/admin/rotate-token", endpoint.base);
            let agent = http_agent(65);
            let response = agent
                .post(&url)
                .header("Authorization", format!("Bearer {}", endpoint.token))
                .header("Content-Type", "application/json")
                .send("{}")
                .map_err(|error| transport_error(&error))?;
            let status = response.status().as_u16();
            let text = response
                .into_body()
                .read_to_string()
                .map_err(|error| CliFailure::internal(format!("读取响应失败：{error}")))?;
            if status == 401 {
                return Err(CliFailure::auth("鉴权失败：token 无效"));
            }
            if !(200..300).contains(&status) {
                return Err(CliFailure::internal(format!(
                    "token 轮换失败（HTTP {status}）：{text}"
                )));
            }
            // 安全：不打印 token；新 token 已写入 0600 发现文件。
            println!("{}", json!({"rotated": true}));
            Ok(0)
        }
        other => Err(CliFailure::usage(format!("未知 api 子命令：{other}"))),
    }
}

fn events_command(
    global: &GlobalOptions,
    endpoint: &Endpoint,
    args: &[String],
) -> Result<u8, CliFailure> {
    if wants_help(args) {
        print_group_help("events");
        return Ok(0);
    }
    let (_, values) = flags(args, &["session", "last-seq"], &["json", "pretty"])?;
    let session = values
        .get("session")
        .ok_or_else(|| CliFailure::usage("events 缺少 --session"))?;
    let last_seq = values
        .get("last-seq")
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| CliFailure::usage("--last-seq 必须是 u64"))
        })
        .transpose()?;
    let _ = global;
    let ws_url = ws_url(&endpoint.base);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| CliFailure::internal(format!("tokio 运行时创建失败：{error}")))?;
    runtime.block_on(events_loop(
        ws_url,
        endpoint.token.clone(),
        session.clone(),
        last_seq,
    ))
}

async fn events_loop(
    url: String,
    token: String,
    session: String,
    mut last_seq: Option<u64>,
) -> Result<u8, CliFailure> {
    let mut retries = 0_u32;
    loop {
        let connect = tokio_tungstenite::connect_async(url.as_str()).await;
        let (mut socket, _) = match connect {
            Ok(pair) => pair,
            Err(error) => {
                retries = retries.saturating_add(1);
                if retries > 5 {
                    return Err(CliFailure::unavailable(format!("WS 连接失败：{error}")));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        let auth = json!({"type": "auth", "token": token});
        if socket
            .send(WsMessage::Text(auth.to_string().into()))
            .await
            .is_err()
        {
            return Err(CliFailure::unavailable("WS auth 发送失败"));
        }
        let mut subscribe = json!({"type": "subscribe", "session_id": session});
        if let Some(seq) = last_seq {
            subscribe["last_seq"] = Value::from(seq);
        }
        if socket
            .send(WsMessage::Text(subscribe.to_string().into()))
            .await
            .is_err()
        {
            return Err(CliFailure::unavailable("WS subscribe 发送失败"));
        }

        let mut reconnect = false;
        while let Some(message) = socket.next().await {
            match message {
                Ok(WsMessage::Text(text)) => {
                    let value: Value = match serde_json::from_str(&text) {
                        Ok(value) => value,
                        Err(_) => continue,
                    };
                    match value.get("type").and_then(Value::as_str) {
                        Some("event") => {
                            let envelope = value.get("envelope").cloned().unwrap_or(Value::Null);
                            if let Some(seq) = envelope.get("seq").and_then(Value::as_u64) {
                                last_seq = Some(seq);
                            }
                            println!("{}", envelope);
                        }
                        Some("gap") => {
                            if let Some(from_seq) = value.get("from_seq").and_then(Value::as_u64) {
                                last_seq = Some(from_seq);
                            }
                            reconnect = true;
                            break;
                        }
                        Some("error") => {
                            eprintln!("{value}");
                            return Ok(5);
                        }
                        _ => {}
                    }
                }
                Ok(WsMessage::Close(frame)) => {
                    let code = frame.as_ref().map(|frame| u16::from(frame.code));
                    match code {
                        Some(4401) => return Err(CliFailure::auth("WS 鉴权失败")),
                        Some(4400) => return Err(CliFailure::usage("WS 订阅参数非法")),
                        Some(1013) | Some(1012) => reconnect = true,
                        _ => {}
                    }
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!("WS 错误：{error}");
                    reconnect = true;
                    break;
                }
            }
        }
        if reconnect {
            retries = retries.saturating_add(1);
            if retries > 8 {
                return Err(CliFailure::unavailable("WS 重连次数超限"));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        return Ok(0);
    }
}

fn call_rpc(endpoint: &Endpoint, method: &str, params: Value) -> Result<Value, CliFailure> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let url = format!("{}/v1/rpc", endpoint.base);
    let agent = http_agent(65);
    let response = agent
        .post(&url)
        .header("Authorization", format!("Bearer {}", endpoint.token))
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(|error| transport_error(&error))?;
    let status = response.status().as_u16();
    let text = response
        .into_body()
        .read_to_string()
        .map_err(|error| CliFailure::internal(format!("读取响应失败：{error}")))?;
    if status == 401 {
        return Err(CliFailure::auth("鉴权失败：token 无效"));
    }
    if status == 429 {
        return Err(CliFailure::unavailable("请求被限流（HTTP 429）"));
    }
    if status == 503 {
        return Err(CliFailure::unavailable(format!(
            "核心未就绪（HTTP 503）：{text}"
        )));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| CliFailure::internal(format!("响应不是合法 JSON：{error}")))?;
    if let Some(error) = value.get("error") {
        return Err(CliFailure::rpc(error.clone()));
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

fn http_agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn transport_error(error: &ureq::Error) -> CliFailure {
    match error {
        ureq::Error::Timeout(_) => CliFailure::timeout(format!("请求超时：{error}")),
        ureq::Error::StatusCode(code) if *code == 401 => CliFailure::auth("鉴权失败"),
        ureq::Error::StatusCode(code) if *code >= 500 => {
            CliFailure::unavailable(format!("服务不可用（HTTP {code}）"))
        }
        _ => CliFailure::internal(format!("HTTP 调用失败：{error}")),
    }
}

fn print_json(value: &Value, pretty: bool) -> Result<(), CliFailure> {
    let text = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .map_err(|error| CliFailure::internal(format!("输出序列化失败：{error}")))?;
    println!("{text}");
    Ok(())
}

/// 全局选项（从任意位置抽取；子命令仍按位置解析）。
#[derive(Debug, Default)]
struct GlobalOptions {
    url: Option<String>,
    token: Option<String>,
    token_file: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    pretty: bool,
}

fn split_global(args: &[String]) -> Result<(GlobalOptions, Vec<String>), CliFailure> {
    let mut global = GlobalOptions::default();
    let mut rest = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--url" => {
                global.url = Some(next_value(args, &mut index, "--url")?);
            }
            "--token" => {
                global.token = Some(next_value(args, &mut index, "--token")?);
            }
            "--token-file" => {
                global.token_file =
                    Some(PathBuf::from(next_value(args, &mut index, "--token-file")?));
            }
            "--data-dir" => {
                global.data_dir = Some(PathBuf::from(next_value(args, &mut index, "--data-dir")?));
            }
            "--pretty" => {
                global.pretty = true;
                index += 1;
            }
            _ => {
                rest.push(args[index].clone());
                index += 1;
            }
        }
    }
    Ok((global, rest))
}

fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, CliFailure> {
    let Some(value) = args.get(*index + 1) else {
        return Err(CliFailure::usage(format!("{flag} 缺少值")));
    };
    if value.starts_with("--") {
        return Err(CliFailure::usage(format!("{flag} 缺少值")));
    }
    *index += 2;
    Ok(value.clone())
}

fn resolve_endpoint(global: &GlobalOptions) -> Result<Endpoint, CliFailure> {
    let discovery = discovery_info(global)?;
    let base = match &global.url {
        Some(url) => normalize_url(url),
        None => discovery
            .as_ref()
            .map(|(base, _)| base.clone())
            .ok_or_else(|| {
                CliFailure::unavailable(
                    "未找到 daemon HTTP API：请使用 --url 或确认已以 --http/--http-only 启动",
                )
            })?,
    };
    let token = if let Some(token) = &global.token {
        token.clone()
    } else if let Ok(token) = std::env::var("AETHERD_API_TOKEN") {
        if token.trim().is_empty() {
            return Err(CliFailure::auth("AETHERD_API_TOKEN 为空"));
        }
        token
    } else if let Some(path) = &global.token_file {
        read_token_file(path)?
    } else if let Some((_, token)) = discovery {
        token
    } else {
        return Err(CliFailure::auth(
            "缺少 token：请设置 AETHERD_API_TOKEN / --token-file 或使用发现文件",
        ));
    };
    Ok(Endpoint { base, token })
}

/// 发现文件 → `(base_url, token)`（读取失败/不存在 → `None`）。
fn discovery_info(global: &GlobalOptions) -> Result<Option<(String, String)>, CliFailure> {
    let data_dir = match resolve_data_dir(global) {
        Ok(dir) => dir,
        Err(_) => return Ok(None),
    };
    let path = data_dir.join(DISCOVERY_FILE_NAME);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| CliFailure::unavailable(format!("发现文件不可读：{error}")))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| CliFailure::unavailable(format!("发现文件损坏：{error}")))?;
    let port = value
        .get("port")
        .and_then(Value::as_u64)
        .ok_or_else(|| CliFailure::unavailable("发现文件缺少 port"))?;
    let bind = value
        .get("bind")
        .and_then(Value::as_str)
        .unwrap_or("127.0.0.1");
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| CliFailure::unavailable("发现文件缺少 token"))?;
    let host = if bind.contains(':') {
        format!("[{bind}]")
    } else {
        bind.to_owned()
    };
    Ok(Some((format!("http://{host}:{port}"), token.to_owned())))
}

fn resolve_data_dir(global: &GlobalOptions) -> Result<PathBuf, CliFailure> {
    if let Some(dir) = &global.data_dir {
        return Ok(dir.clone());
    }
    if let Some(result) = pointer::env_data_dir() {
        return result.map_err(CliFailure::usage);
    }
    if let Some(path) = pointer::pointer_file_path() {
        if let Ok(Some(dir)) = pointer::read_pointer(&path) {
            return Ok(dir);
        }
    }
    pointer::default_data_dir().ok_or_else(|| CliFailure::unavailable("无法解析数据目录"))
}

fn read_token_file(path: &std::path::Path) -> Result<String, CliFailure> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(path)
            .map_err(|error| CliFailure::auth(format!("token 文件不可读：{error}")))?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(CliFailure::auth(format!(
                "token 文件权限过宽（{mode:o}）；要求 0600"
            )));
        }
    }
    let mut file = std::fs::File::open(path)
        .map_err(|error| CliFailure::auth(format!("token 文件不可读：{error}")))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| CliFailure::auth(format!("token 文件不可读：{error}")))?;
    let token = text.trim().to_owned();
    if token.is_empty() {
        return Err(CliFailure::auth("token 文件为空"));
    }
    Ok(token)
}

fn normalize_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_owned()
    } else {
        format!("http://{url}")
    }
}

fn ws_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_owned()
    };
    format!("{base}/v1/events")
}

fn require_positional(
    positional: &[String],
    index: usize,
    usage: &str,
) -> Result<String, CliFailure> {
    positional
        .get(index)
        .cloned()
        .ok_or_else(|| CliFailure::usage(format!("用法：{usage}")))
}

fn insert_string(map: &mut Map<String, Value>, key: &str, value: Option<&String>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), Value::String(value.clone()));
    }
}

fn insert_i64(
    map: &mut Map<String, Value>,
    key: &str,
    value: Option<&String>,
) -> Result<(), CliFailure> {
    let Some(value) = value else {
        return Ok(());
    };
    let parsed = value
        .parse::<i64>()
        .map_err(|_| CliFailure::usage(format!("--{key} 必须是整数")))?;
    map.insert(key.to_owned(), Value::from(parsed));
    Ok(())
}

fn insert_u64(
    map: &mut Map<String, Value>,
    key: &str,
    value: Option<&String>,
) -> Result<(), CliFailure> {
    let Some(value) = value else {
        return Ok(());
    };
    let parsed = value
        .parse::<u64>()
        .map_err(|_| CliFailure::usage(format!("--{key} 必须是非负整数")))?;
    map.insert(key.to_owned(), Value::from(parsed));
    Ok(())
}

fn insert_value_string(value: &mut Value, key: &str, item: Option<&String>) {
    if let (Some(item), Value::Object(map)) = (item, value) {
        map.insert(key.to_owned(), Value::String(item.clone()));
    }
}

fn insert_value_u64(value: &mut Value, key: &str, item: Option<&String>) -> Result<(), CliFailure> {
    let Some(item) = item else {
        return Ok(());
    };
    let parsed = item
        .parse::<u64>()
        .map_err(|_| CliFailure::usage(format!("--{key} 必须是非负整数")))?;
    if let Value::Object(map) = value {
        map.insert(key.to_owned(), Value::from(parsed));
    }
    Ok(())
}

/// 解析 `--key value` / `--key=value` / 布尔开关；未知参数 → 用法错误。
fn flags(
    args: &[String],
    values: &[&str],
    bools: &[&str],
) -> Result<(Vec<String>, HashMap<String, String>), CliFailure> {
    let mut positional = Vec::new();
    let mut map = HashMap::new();
    let mut index = 0;
    while index < args.len() {
        let token = &args[index];
        let Some(rest) = token.strip_prefix("--") else {
            positional.push(token.clone());
            index += 1;
            continue;
        };
        let (key, inline) = match rest.split_once('=') {
            Some((key, value)) => (key, Some(value.to_owned())),
            None => (rest, None),
        };
        if bools.contains(&key) {
            if map.contains_key(key) {
                return Err(CliFailure::usage(format!("--{key} 重复")));
            }
            map.insert(key.to_owned(), inline.unwrap_or_else(|| "true".to_owned()));
            index += 1;
            continue;
        }
        if !values.contains(&key) {
            return Err(CliFailure::usage(format!("未知参数 --{key}")));
        }
        let value = match inline {
            Some(value) => value,
            None => {
                let Some(next) = args.get(index + 1) else {
                    return Err(CliFailure::usage(format!("--{key} 缺少值")));
                };
                if next.starts_with("--") {
                    return Err(CliFailure::usage(format!("--{key} 缺少值")));
                }
                index += 1;
                next.clone()
            }
        };
        if map.insert(key.to_owned(), value).is_some() {
            return Err(CliFailure::usage(format!("--{key} 重复")));
        }
        index += 1;
    }
    Ok((positional, map))
}

/// 生成 26 字符 ULID（`client_msg_id` 缺省值；Crockford Base32）。
fn generate_ulid() -> Result<String, CliFailure> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let mut random = [0_u8; 10];
    getrandom::fill(&mut random)
        .map_err(|error| CliFailure::internal(format!("ULID 随机数生成失败：{error}")))?;
    let mut bytes = [0_u8; 16];
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6..].copy_from_slice(&random);
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut value = 0_u128;
    for byte in bytes {
        value = (value << 8) | u128::from(byte);
    }
    let mut output = String::with_capacity(26);
    for _ in 0..26 {
        output.push(ALPHABET[(value & 31) as usize] as char);
        value >>= 5;
    }
    Ok(output.chars().rev().collect())
}

fn wants_help(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--help" || arg == "-h")
}

fn print_group_help(group: &str) {
    let text = match group {
        "health" => "\
用法：aetherd --cli health [--pretty]
说明：GET /v1/health（health 命令 + health.api）；JSON 输出。
示例：aetherd --cli health
退出码：0 成功；4 鉴权失败；5 未就绪；6 超时。",
        "session" => "\
用法：
  aetherd --cli session list [--runtime <id>] [--status <status>] [--limit N]
  aetherd --cli session create --title <text> [--runtime <id>] [--provider <ULID>] [--model <model>] [--workspace <ULID>] [--thinking-depth 0-4] [--permission-mode <mode>] [--preset <ULID>]
  aetherd --cli session send <session_id> --text <text> [--client-msg-id <ULID>] [--thinking-depth 0-4] [--provider <ULID>] [--model <model>]
  aetherd --cli session messages <session_id> [--last-seq N] [--limit N] [--max-bytes N] [--cursor <token>]
  aetherd --cli session interrupt <session_id>
示例：aetherd --cli session list --limit 20 --pretty
退出码：0 成功；1 内部；2 用法；3 未找到；4 鉴权；5 未就绪；6 超时。",
        "workflow" => "\
用法：
  aetherd --cli workflow run <workflow_id> --session <session_id> [--input <json>] [--priority N]
  aetherd --cli workflow status <workflow_run_id>
  aetherd --cli workflow cancel <workflow_run_id> [--reason <text>]
  aetherd --cli workflow retry-node <workflow_run_id> <node_id>
说明：M10-01 实现前，daemon 对 workflow_* 返回 not_implemented（退出码 3）。",
        "subagent" => "\
用法：
  aetherd --cli subagent tree <parent_session_id> [--include-completed]
  aetherd --cli subagent result --parent <parent_session_id> --child <child_session_id> [--offset N] [--limit-bytes N]
  aetherd --cli subagent resolve <child_session_id> --decision <accept|reject|rerun> [--note <text>]
说明：M10-02 实现前，daemon 对 subagent_* 返回 not_implemented（退出码 3）。",
        "import" => "\
用法：aetherd --cli import <file> [--archive] [--mode <mode>] [--dry-run]
说明：M10-04 实现前，daemon 对 session_import/archive_import 返回 not_implemented（退出码 3）。",
        "export" => "\
用法：aetherd --cli export <session_id> [--target-dir <dir>]
说明：调用 session_export；JSON 输出。",
        "events" => "\
用法：aetherd --cli events --session <session_id> [--last-seq N]
说明：原生 WebSocket（/v1/events），输出 JSONL 事件信封；遇到 gap/1013/1012 自动用 last_seq 重连。
示例：aetherd --cli events --session 01J... --last-seq 120",
        "api" => "\
用法：aetherd --cli api rotate-token
说明：轮换 token（需当前 token）；新 token 写入 0600 发现文件，CLI 不打印 token。",
        _ => "未知帮助主题",
    };
    println!("{text}");
}

fn print_help() {
    println!(
        "\
aetherd --cli — Aether headless CLI（同一业务核心；ADR-033）

用法：
  aetherd --cli [全局选项] <命令> [参数]

全局选项：
  --url <http://127.0.0.1:PORT>   显式 API 地址（缺省读发现文件）
  --token <token>                 token（缺省 AETHERD_API_TOKEN / --token-file / 发现文件）
  --token-file <path>             token 文件（Unix 要求 0600）
  --data-dir <path>               数据目录（发现文件所在目录）
  --pretty                        美化 JSON 输出

命令：
  health
  session list|create|send|messages|interrupt
  workflow run|status|cancel|retry-node
  subagent tree|result|resolve
  import <file> [--archive] [--dry-run]
  export <session_id> [--target-dir <dir>]
  events --session <id> [--last-seq N]
  api rotate-token

退出码：
  0 成功   1 内部错误   2 用法/参数   3 未找到   4 鉴权失败
  5 服务不可用/未就绪   6 超时   7 取消
"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulid_is_26_crockford_chars() {
        let ulid = generate_ulid().expect("ulid");
        assert_eq!(ulid.len(), 26);
        assert!(ulid
            .chars()
            .all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c)));
        assert_ne!(ulid, generate_ulid().expect("ulid"));
    }

    #[test]
    fn flags_parse_values_and_booleans() {
        let args = vec![
            "--title".to_owned(),
            "x".to_owned(),
            "--dry-run".to_owned(),
            "positional".to_owned(),
        ];
        let (positional, values) = flags(&args, &["title"], &["dry-run"]).expect("flags");
        assert_eq!(positional, vec!["positional"]);
        assert_eq!(values.get("title"), Some(&"x".to_owned()));
        assert_eq!(values.get("dry-run"), Some(&"true".to_owned()));
    }

    #[test]
    fn ws_url_switches_scheme() {
        assert_eq!(
            ws_url("http://127.0.0.1:43123"),
            "ws://127.0.0.1:43123/v1/events"
        );
        assert_eq!(
            ws_url("https://127.0.0.1:43123"),
            "wss://127.0.0.1:43123/v1/events"
        );
    }
}
