//! M3-06 DoD1（附录 D T4）：kill -9 ×20 数据完整性（run 中强杀）。
//!
//! 「已确认」定义（附录 D T4）：
//! ① 用户消息 = `session.send` 返回 runId 且消息/run 行已提交；
//! ② 助手消息 = 客户端收到 `message.completed` 且 journal 提交成功。
//!
//! 结构（复用 M2-08 宿主子进程模式）：
//! - **宿主子进程**（`t4_host_child`）：在共享数据目录上启动真实核心（存储 + 管线 +
//!   生命周期 + 流式执行器），每次迭代先完成一个「已确认」run（quick）并写确认日志，
//!   再发起一个长时间流式 run（slow）并在首条 delta 落盘后写 in-flight 标记，随后挂起
//!   等待被强杀；
//! - **驱动测试**：循环 20 次「宿主就绪 → 强杀（Windows `TerminateProcess` / Unix
//!   `SIGKILL`）→ 重启（下一次迭代的启动路径执行 M3-06 重启状态重建）」；
//! - **校验子进程**（`AETHER_M3_06_T4_VERIFY=1`）：重开核心并逐条核对确认日志：
//!   已确认用户/助手消息零丢失；未确认 run 不显示为完成（状态非 `succeeded`、
//!   无 `message.completed` 事件）且被重启收口为 `failed(run_interrupted)`。
//!
//! 运行：`cargo test -p aether-tauri --test m3_06_t4 -- --nocapture`（约 30–60s；
//! `scripts/test/m3-06/verify-m3-06.mjs` 执行）。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_control::{
    EventPipeline, ExecutorFuture, ExecutorOutcome, LifecycleConfig, RunExecutor, RunRequest,
    SessionManager, SystemClock,
};
use aether_core::{EventEnvelope, EventPayload, EventType, RunId, RunStatus, SessionId};
use aether_store::ReadPool;
use aether_tauri::core_health::{boot_core_full, CoreBoot, StaticRuntimeSummaries};
use serde_json::{json, Value};
use tempfile::TempDir;

/// 宿主子进程模式环境变量（值为状态目录）。
const HOST_ENV: &str = "AETHER_M3_06_T4_HOST";
/// 校验子进程模式环境变量（值 `1`）。
const VERIFY_ENV: &str = "AETHER_M3_06_T4_VERIFY";
/// 宿主测试名（`--exact` 过滤）。
const HOST_TEST: &str = "t4_host_child";
const RECONCILE_LINE: &str = "AETHER_M3_06_T4_RECONCILE";
const INFLIGHT_LINE: &str = "AETHER_M3_06_T4_INFLIGHT";
const VERIFY_LINE: &str = "AETHER_M3_06_T4_VERIFY";
/// 迭代次数（附录 D T4：循环 20 次）。
const ITERATIONS: usize = 20;

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("构建 tokio 运行时")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// ULID 形状 id（时间 + 计数器；跨宿主重启唯一，避免 `events.id` 主键冲突）。
fn fixture_ulid(counter: u64) -> String {
    format!("01J{:013}{:010}", now_ms(), counter % 10_000_000_000)
}

/// T4 流式执行器：把 delta 事件经真实管线提交（先日志后广播）。
struct T4StreamingExecutor {
    pipeline: EventPipeline,
    counter: AtomicU64,
}

impl T4StreamingExecutor {
    fn new(pipeline: EventPipeline) -> Self {
        Self {
            pipeline,
            counter: AtomicU64::new(0),
        }
    }
}

impl RunExecutor for T4StreamingExecutor {
    fn execute(&self, request: RunRequest) -> ExecutorFuture<'_> {
        Box::pin(async move {
            // `quick`：短流式（可被客户端确认）；其余（`slow`）：长流式（强杀时未确认）。
            let (deltas, interval) = if request.text == "quick" {
                (3usize, Duration::from_millis(5))
            } else {
                (usize::MAX, Duration::from_millis(50))
            };
            let message_id = fixture_ulid(self.counter.fetch_add(1, Ordering::SeqCst));
            let mut text = String::new();
            for _ in 0..deltas {
                if request.cancel.is_cancelled() {
                    return ExecutorOutcome::Cancelled {
                        reason: Some("cancelled".to_owned()),
                    };
                }
                text.push_str("frag");
                let event_id = fixture_ulid(self.counter.fetch_add(1, Ordering::SeqCst));
                let envelope = json!({
                    "v": 1,
                    "id": event_id,
                    "session_id": request.session_id.as_str(),
                    "run_id": request.run_id.as_str(),
                    "runtime_id": request.runtime_id.as_str(),
                    "seq": 0,
                    "ts": now_ms(),
                    "type": "message.delta",
                    "payload": {"message_id": message_id, "text": "frag"},
                });
                if let Err(error) = self.pipeline.submit(envelope).await {
                    tracing::warn!(error = %error, "T4 夹具 delta 提交失败");
                }
                tokio::time::sleep(interval).await;
            }
            ExecutorOutcome::Completed {
                assistant_text: Some(text),
                usage: None,
            }
        })
    }
}

fn append_line(path: &Path, value: &Value) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{value}");
        let _ = file.flush();
    }
}

fn read_lines(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

fn session_file(state_dir: &Path) -> PathBuf {
    state_dir.join("session.txt")
}

/// 取得/创建共享会话（跨宿主重启复用同一会话，模拟真实 UI 会话）。
fn ensure_session(
    runtime: &tokio::runtime::Runtime,
    manager: &SessionManager,
    state_dir: &Path,
) -> SessionId {
    if let Ok(text) = std::fs::read_to_string(session_file(state_dir)) {
        if let Ok(session_id) = SessionId::new(text.trim().to_owned()) {
            return session_id;
        }
    }
    let runtime_row = aether_core::Runtime {
        id: aether_core::RuntimeId::new("mock").unwrap(),
        name: "Mock".to_owned(),
        kind: "mock".to_owned(),
        version: "0.1.0".to_owned(),
        protocol: "1.0".to_owned(),
        capabilities: Vec::new(),
        endpoint: None,
        config: json!({}),
        status: aether_core::RuntimeStatus::Ready,
        status_reason: None,
        last_seen_at: None,
        created_at: 1,
        updated_at: 1,
    };
    let session = runtime
        .block_on(manager.create_session(runtime_row, "T4", None, None))
        .expect("创建会话");
    let _ = std::fs::write(session_file(state_dir), session.id.as_str());
    session.id
}

/// 等待管线广播中出现指定 run 的指定事件类型（返回首个命中的事件）。
async fn wait_event(
    events: &mut tokio::sync::broadcast::Receiver<EventEnvelope>,
    run_id: &RunId,
    event_type: EventType,
    timeout: Duration,
) -> Option<EventEnvelope> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(envelope)) => {
                if envelope.run_id.as_ref() == Some(run_id) && envelope.event_type() == event_type {
                    return Some(envelope);
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => return None,
        }
    }
}

/// 单次迭代：一个「已确认」run（quick）+ 一个 in-flight run（slow）→ 挂起等待被强杀。
fn run_iteration(
    runtime: &tokio::runtime::Runtime,
    boot: &CoreBoot,
    manager: &SessionManager,
    state_dir: &Path,
) {
    let session_id = ensure_session(runtime, manager, state_dir);
    let confirmed_path = state_dir.join("confirmed.jsonl");
    let mut events = boot.pipeline.subscribe();
    let iteration = iteration_number(state_dir);

    // ① 已确认 run：send 返回 runId（行已提交）→ 收到 message.completed（journal 已提交）。
    let quick_client_id = format!("01J8ZQ5R0N7W9Y8X6V4T2S0{iteration:03}");
    let quick = runtime
        .block_on(manager.send(&session_id, "quick", &quick_client_id))
        .expect("quick send");
    append_line(
        &confirmed_path,
        &json!({
            "kind": "user",
            "client_msg_id": quick_client_id,
            "message_id": quick.message_id.as_str(),
            "run_id": quick.run_id.as_str(),
        }),
    );
    let completed = runtime
        .block_on(wait_event(
            &mut events,
            &quick.run_id,
            EventType::MessageCompleted,
            Duration::from_secs(30),
        ))
        .expect("quick run 必须在 30s 内收到 message.completed（已确认）");
    let payload = match completed.payload {
        EventPayload::MessageCompleted(payload) => payload,
        other => panic!("message.completed payload 形状异常：{other:?}"),
    };
    append_line(
        &confirmed_path,
        &json!({
            "kind": "assistant",
            "message_id": payload.message.id.as_str(),
            "run_id": quick.run_id.as_str(),
            "content": payload.message.content,
        }),
    );

    // ② in-flight run：首条 delta 落盘后写标记，随后挂起等待强杀（未确认）。
    let client_msg_id = format!("01J8ZQ5R0N7W9Y8X6V4T2S1{iteration:03}");
    let slow = runtime
        .block_on(manager.send(&session_id, "slow", &client_msg_id))
        .expect("slow send");
    append_line(
        &confirmed_path,
        &json!({
            "kind": "user",
            "client_msg_id": client_msg_id,
            "message_id": slow.message_id.as_str(),
            "run_id": slow.run_id.as_str(),
        }),
    );
    assert!(
        runtime
            .block_on(wait_event(
                &mut events,
                &slow.run_id,
                EventType::MessageDelta,
                Duration::from_secs(30),
            ))
            .is_some(),
        "slow run 必须出现首条 delta（强杀点：run 进行中）"
    );
    let inflight = json!({
        "run_id": slow.run_id.as_str(),
        "message_id": slow.message_id.as_str(),
        "client_msg_id": client_msg_id,
    });
    append_line(&state_dir.join("inflight.jsonl"), &inflight);
    println!("{INFLIGHT_LINE} {inflight}");
    let _ = std::io::stdout().flush();
}

/// 迭代序号（inflight 标记计数 + 1；供 client_msg_id 唯一化）。
fn iteration_number(state_dir: &Path) -> usize {
    read_lines(&state_dir.join("inflight.jsonl")).len() + 1
}

/// 校验（重启后）：已确认零丢失 + 未确认不误显示完成 + 重启收口。
fn verify_journal(runtime: &tokio::runtime::Runtime, boot: &CoreBoot, state_dir: &Path) -> Value {
    let session_id = SessionId::new(
        std::fs::read_to_string(session_file(state_dir))
            .expect("会话 id 文件")
            .trim()
            .to_owned(),
    )
    .expect("会话 id");
    let reads: &ReadPool = &boot.reads;
    let entries = read_lines(&state_dir.join("confirmed.jsonl"));
    let mut user_confirmed = 0usize;
    let mut assistant_confirmed = 0usize;
    let mut loss: Vec<Value> = Vec::new();

    for entry in &entries {
        match entry["kind"].as_str() {
            Some("user") => {
                let client_msg_id = entry["client_msg_id"].as_str().unwrap_or_default();
                let expected_message = entry["message_id"].as_str().unwrap_or_default();
                let expected_run = entry["run_id"].as_str().unwrap_or_default();
                let stored = runtime
                    .block_on(reads.message_by_client_msg_id(&session_id, client_msg_id))
                    .ok()
                    .flatten();
                match stored {
                    Some(message) if message.id.as_str() == expected_message => {
                        let run = runtime
                            .block_on(reads.run(&RunId::new(expected_run.to_owned()).unwrap()))
                            .ok()
                            .flatten();
                        match run {
                            Some(run) if run.input_message_id.as_ref().map(|id| id.as_str()) == Some(expected_message) => {
                                user_confirmed += 1;
                            }
                            other => loss.push(json!({"kind":"user","run":expected_run,"detail":format!("run 行缺失或输入关联不符：{other:?}")})),
                        }
                    }
                    other => loss.push(json!({"kind":"user","client_msg_id":client_msg_id,"detail":format!("消息行缺失：{other:?}")})),
                }
            }
            Some("assistant") => {
                let message_id = entry["message_id"].as_str().unwrap_or_default();
                let content = entry["content"].as_str().unwrap_or_default();
                let stored = runtime
                    .block_on(
                        reads.message(&aether_core::MessageId::new(message_id.to_owned()).unwrap()),
                    )
                    .ok()
                    .flatten();
                match stored {
                    Some(message) if message.content == content => assistant_confirmed += 1,
                    other => loss.push(json!({"kind":"assistant","message_id":message_id,"detail":format!("助手终稿缺失/内容不符：{other:?}")})),
                }
            }
            _ => {}
        }
    }

    // 未确认 run：不得 succeeded、不得有 message.completed 事件；必须被重启收口为 failed。
    let inflight = read_lines(&state_dir.join("inflight.jsonl"));
    let frame = runtime
        .block_on(boot.pipeline.readback(&session_id, 0))
        .expect("事件补读");
    let mut false_completions: Vec<Value> = Vec::new();
    let mut unreconciled: Vec<Value> = Vec::new();
    for entry in &inflight {
        let run_id = RunId::new(entry["run_id"].as_str().unwrap_or_default().to_owned()).unwrap();
        let stored = runtime.block_on(reads.run(&run_id)).ok().flatten();
        if stored.as_ref().map(|run| run.status) == Some(RunStatus::Succeeded) {
            false_completions
                .push(json!({"run_id": run_id.as_str(), "detail": "未确认 run 被标记 succeeded"}));
        }
        if frame.events.iter().any(|event| {
            event.run_id.as_ref() == Some(&run_id)
                && event.event_type() == EventType::MessageCompleted
        }) {
            false_completions.push(json!({"run_id": run_id.as_str(), "detail": "未确认 run 存在 message.completed 事件"}));
        }
        match stored {
            Some(run)
                if run.status == RunStatus::Failed
                    && run.error.as_deref() == Some(aether_control::RUN_INTERRUPTED_CODE) => {}
            other => unreconciled
                .push(json!({"run_id": run_id.as_str(), "detail": format!("{other:?}")})),
        }
    }

    json!({
        "user_confirmed": user_confirmed,
        "assistant_confirmed": assistant_confirmed,
        "inflight": inflight.len(),
        "loss": loss,
        "false_completions": false_completions,
        "unreconciled": unreconciled,
    })
}

/// 宿主子进程：迭代或校验（直接运行时不做事，保证常规 `cargo test` 全绿）。
#[test]
fn t4_host_child() {
    let Ok(state_dir) = std::env::var(HOST_ENV) else {
        return;
    };
    let state_dir = PathBuf::from(state_dir);
    let verify = std::env::var(VERIFY_ENV).as_deref() == Ok("1");
    let runtime = new_runtime();
    let handle = runtime.handle().clone();
    let boot = boot_core_full(
        &state_dir,
        &handle,
        Arc::new(StaticRuntimeSummaries::unwired()),
    )
    .expect("宿主核心启动（含存储/管线）");
    let executor = Arc::new(T4StreamingExecutor::new(boot.pipeline.clone()));
    let manager = SessionManager::new(
        LifecycleConfig::default(),
        Arc::new(SystemClock),
        boot.write.clone(),
        boot.reads.clone(),
        boot.pipeline.clone(),
        executor,
    );
    // M3-06 生产启动路径：重启状态重建（未收口 run → failed + 会话回 idle）。
    let report = runtime
        .block_on(manager.reconcile_interrupted_runs())
        .expect("重启状态重建");
    println!(
        "{RECONCILE_LINE} {}",
        json!({
            "runs_failed": report.runs_failed.iter().map(|run| run.as_str()).collect::<Vec<_>>(),
            "sessions_reset": report.sessions_reset.iter().map(|session| session.as_str()).collect::<Vec<_>>(),
        })
    );
    let _ = std::io::stdout().flush();

    if verify {
        let result = verify_journal(&runtime, &boot, &state_dir);
        println!("{VERIFY_LINE} {result}");
        let _ = std::io::stdout().flush();
        return;
    }

    run_iteration(&runtime, &boot, &manager, &state_dir);
    // 挂起等待被强杀（模拟核心运行中被 kill -9）。
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// 驱动：kill -9 ×20 → 校验子进程核对「已确认零丢失 + 未确认不误显示完成」。
#[test]
fn kill_nine_20_times_preserves_confirmed_messages() {
    if std::env::var(HOST_ENV).is_ok() {
        return; // 宿主/校验子进程模式：不递归。
    }
    let dir = TempDir::new().expect("临时目录");
    let state_dir = dir.path().to_path_buf();
    let exe = std::env::current_exe().expect("当前测试二进制");

    let mut boot_reports: Vec<Value> = Vec::new();
    for iteration in 0..ITERATIONS {
        let mut child = spawn_host(&exe, &state_dir, false);
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let stdout = child.stdout.take().expect("宿主 stdout");
        let reader_lines = Arc::clone(&lines);
        let reader = std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                println!("[t4-host {iteration}] {line}");
                match reader_lines.lock() {
                    Ok(mut guard) => guard.push(line),
                    Err(poisoned) => poisoned.into_inner().push(line),
                }
            }
        });

        // 等待 in-flight 标记（run 进行中）→ 强杀。
        let marker = state_dir.join("inflight.jsonl");
        let expected_markers = iteration + 1;
        let deadline = Instant::now() + Duration::from_secs(60);
        while read_lines(&marker).len() < expected_markers {
            assert!(
                Instant::now() < deadline,
                "第 {iteration} 次迭代未在 60s 内进入 in-flight（标记文件 {}）",
                marker.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();

        let captured = match lines.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        // 注意：libtest 的 `test <name> ... ` 状态行与子进程 stdout 可能同处一行，
        // 证据行按「标记子串」定位而非行首。
        let reconcile = captured
            .iter()
            .find_map(|line| {
                line.find(RECONCILE_LINE)
                    .map(|index| line[index + RECONCILE_LINE.len()..].trim().to_owned())
            })
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        let inflight = read_lines(&marker);
        let current = inflight.last().cloned().unwrap_or(Value::Null);
        println!("[t4] iteration={iteration} reconcile={reconcile} inflight={current}");
        boot_reports
            .push(json!({"iteration": iteration, "reconcile": reconcile, "inflight": current}));
    }

    // 校验子进程：重开核心 → 逐条核对确认日志 + 未确认收口。
    let verify_child = spawn_host(&exe, &state_dir, true);
    let output = verify_child.wait_with_output().expect("等待校验子进程");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        println!("[t4-verify] {line}");
    }
    assert!(output.status.success(), "校验子进程必须成功退出");
    let verify = stdout
        .lines()
        .find_map(|line| {
            line.find(VERIFY_LINE)
                .map(|index| line[index + VERIFY_LINE.len()..].trim().to_owned())
        })
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .expect("校验行必须存在");
    println!("[t4] verify={verify}");
    // 校验子进程的重启收口（覆盖最后一次迭代的 in-flight run）。
    let verify_reconcile = stdout
        .lines()
        .find_map(|line| {
            line.find(RECONCILE_LINE)
                .map(|index| line[index + RECONCILE_LINE.len()..].trim().to_owned())
        })
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);
    boot_reports.push(json!({"iteration": ITERATIONS, "reconcile": verify_reconcile}));

    // ===== 断言 =====
    // 每次迭代两个已确认用户消息（quick + slow 的 send ack 均已提交）。
    assert_eq!(
        verify["user_confirmed"],
        json!(ITERATIONS * 2),
        "已确认用户消息必须零丢失：{verify}"
    );
    assert_eq!(
        verify["assistant_confirmed"],
        json!(ITERATIONS),
        "已确认助手消息必须零丢失：{verify}"
    );
    assert_eq!(
        verify["inflight"],
        json!(ITERATIONS),
        "每次迭代必须有一个未确认 in-flight run：{verify}"
    );
    assert_eq!(verify["loss"], json!([]), "已确认消息不得丢失：{verify}");
    assert_eq!(
        verify["false_completions"],
        json!([]),
        "未确认 run 不得显示为完成：{verify}"
    );
    assert_eq!(
        verify["unreconciled"],
        json!([]),
        "未确认 run 必须被重启收口为 failed(run_interrupted)：{verify}"
    );

    // 每次重启（第 i+1 次启动）必须收口上一次的 in-flight run。
    let inflight_entries = read_lines(&state_dir.join("inflight.jsonl"));
    for (index, entry) in inflight_entries.iter().enumerate() {
        let run_id = entry["run_id"].as_str().unwrap_or_default();
        let next_report = &boot_reports
            .get(index + 1)
            .map(|report| report["reconcile"].clone())
            .unwrap_or(Value::Null);
        let reconciled: Vec<&str> = next_report["runs_failed"]
            .as_array()
            .map(|runs| runs.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        assert!(
            reconciled.contains(&run_id),
            "第 {index} 次迭代的 in-flight run {run_id} 必须在第 {} 次启动被收口：{next_report}",
            index + 1
        );
    }
}

/// 启动宿主/校验子进程（`--exact` 过滤宿主测试；stdout 管道供证据行解析）。
fn spawn_host(exe: &Path, state_dir: &Path, verify: bool) -> Child {
    let mut command = Command::new(exe);
    command
        .args(["--exact", HOST_TEST, "--nocapture", "--test-threads=1"])
        .env(HOST_ENV, state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if verify {
        command.env(VERIFY_ENV, "1");
    }
    command.spawn().expect("启动 T4 宿主子进程")
}
