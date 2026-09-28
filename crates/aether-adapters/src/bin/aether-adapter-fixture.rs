//! M1-10 监督器验证夹具（T5a/T5b/M2-08/M4-01 可复用故障注入助手）。
//!
//! 用法（全部模式下 `--launch-token <token>` 均被接受，供 D5 PID 台账三条件核对）：
//!
//! ```text
//! aether-adapter-fixture --mode sleep  --seconds 60 [--launch-token T]
//! aether-adapter-fixture --mode tree   --seconds 60 --pid-file <path> [--launch-token T]
//! aether-adapter-fixture --mode stderr-crash --lines 60 --exit-code 7
//! aether-adapter-fixture --mode deaf   --seconds 60   # hello + initialize 后不响应心跳
//! aether-adapter-fixture --mode deaf   --seconds 300 --survive-eof  # 孤儿场景（核心强杀后仍存活）
//! aether-adapter-fixture --mode silent --seconds 60   # 存活但不发 hello（握手超时）
//! # M3-06 Mode R 恢复夹具：最小 D6 会话面（create 带 native_id → resumed=true）
//! aether-adapter-fixture --mode session --session-log <path> [--seconds 60]
//! # M1-10 资源告警夹具（M4-01 复用）：真实分配内存，按跨重启计数交替超限/回落
//! aether-adapter-fixture --mode deaf --seconds 30 --mb 160 \
//!     --mb-cycle-file <path> [--launch-token T]
//! ```
//!
//! 说明：bin 目标不打入产品路径，仅用于监督器集成测试与故障注入演练；
//! 命令层不感知该 CLI。

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::time::Duration;

const EXIT_USAGE: u8 = 2;
const EXIT_IO: u8 = 3;
/// 内存夹具线程的保活步长（进程存活期间持有已分配内存）。
const MEMORY_HOLD_TICK: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Sleep,
    Tree,
    StderrCrash,
    Deaf,
    Silent,
    /// M3-06：最小 D6 会话面（`session.create/send/interrupt/dispose`），
    /// `session.create` 携带 `native_id` → `resumed=true`（Mode R 恢复验证宿主）。
    Session,
}

impl Mode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "sleep" => Some(Self::Sleep),
            "tree" => Some(Self::Tree),
            "stderr-crash" => Some(Self::StderrCrash),
            "deaf" => Some(Self::Deaf),
            "silent" => Some(Self::Silent),
            "session" => Some(Self::Session),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct Args {
    mode: Mode,
    seconds: u64,
    lines: u64,
    exit_code: i32,
    pid_file: Option<std::path::PathBuf>,
    launch_token: Option<String>,
    /// 孤儿夹具（M2-08）：stdin EOF（核心被强杀）后仍保持存活至 `--seconds`。
    survive_eof: bool,
    /// 资源夹具：真实分配的内存（MiB；0 = 不分配）。
    mb: u64,
    /// 资源夹具：跨重启计数文件（可选）。启动时读取计数 N、回写 N+1，
    /// 仅当 N 为偶数时按 `--mb` 分配——用于「超限 / 回落」跨进程交替
    /// （释放内存后宿主可能保留物理页，进程级回落不受影响）。
    mb_cycle_file: Option<std::path::PathBuf>,
    /// M3-06 会话夹具：`session.create`/`session.send` 调用记录（JSON Lines 追加）。
    session_log: Option<std::path::PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        mode: Mode::Sleep,
        seconds: 60,
        lines: 60,
        exit_code: 7,
        pid_file: None,
        launch_token: None,
        survive_eof: false,
        mb: 0,
        mb_cycle_file: None,
        session_log: None,
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < raw.len() {
        let flag = raw.get(index).map(String::as_str).unwrap_or_default();
        let mut value = || -> Result<String, String> {
            index += 1;
            raw.get(index)
                .cloned()
                .ok_or_else(|| format!("参数 {flag} 缺少值"))
        };
        match flag {
            "--mode" => args.mode = Mode::parse(&value()?).ok_or("未知 --mode")?,
            "--seconds" => {
                args.seconds = value()?
                    .parse::<u64>()
                    .map_err(|error| format!("--seconds 非法：{error}"))?
            }
            "--lines" => {
                args.lines = value()?
                    .parse::<u64>()
                    .map_err(|error| format!("--lines 非法：{error}"))?
            }
            "--exit-code" => {
                args.exit_code = value()?
                    .parse::<i32>()
                    .map_err(|error| format!("--exit-code 非法：{error}"))?
            }
            "--pid-file" => args.pid_file = Some(value()?.into()),
            "--mb" => {
                args.mb = value()?
                    .parse::<u64>()
                    .map_err(|error| format!("--mb 非法：{error}"))?
            }
            "--mb-cycle-file" => args.mb_cycle_file = Some(value()?.into()),
            "--session-log" => args.session_log = Some(value()?.into()),
            "--survive-eof" => args.survive_eof = true,
            "--launch-token" => args.launch_token = Some(value()?),
            other if other.starts_with("--launch-token=") => {
                // D5 spawn 注入形式：`--launch-token=<ULID>`（单参数）。
                args.launch_token = Some(other.trim_start_matches("--launch-token=").to_owned());
            }
            other => return Err(format!("未知参数：{other}")),
        }
        index += 1;
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(detail) => {
            eprintln!("[fixture] 参数错误：{detail}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(detail) => {
            eprintln!("[fixture] 运行失败：{detail}");
            ExitCode::from(EXIT_IO)
        }
    }
}

fn run(args: &Args) -> Result<(), String> {
    match args.mode {
        Mode::Sleep | Mode::Silent => {
            start_memory_profile(args);
            sleep_seconds(args.seconds);
            Ok(())
        }
        Mode::Tree => spawn_tree(args),
        Mode::StderrCrash => {
            for index in 0..args.lines {
                eprintln!("#{index} fixture stderr line");
            }
            std::process::exit(args.exit_code);
        }
        Mode::Deaf => serve_deaf(args),
        Mode::Session => serve_session(args),
    }
}

fn sleep_seconds(seconds: u64) {
    std::thread::sleep(Duration::from_secs(seconds));
}

/// M3-06 会话夹具状态（单会话最小投影）。
#[derive(Default)]
struct SessionFixtureState {
    /// `client_msg_id` → 适配器 run id（ADR-005 适配器侧幂等）。
    client_msg_ids: std::collections::HashMap<String, String>,
    /// 在途 run（`long` 触发后保持不返回终态，等待 `session.interrupt`）。
    active_run: Option<String>,
    disposed: bool,
}

/// 会话夹具调用记录（JSON Lines 追加；M3-06 测试断言 `resumed`/`session_id`）。
fn log_session_call(args: &Args, payload: serde_json::Value) {
    let Some(path) = &args.session_log else {
        return;
    };
    let line = match serde_json::to_string(&payload) {
        Ok(line) => line,
        Err(_) => return,
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

/// 写一行 JSON-RPC 帧（响应/通知共用）。
fn write_frame(frame: &serde_json::Value) -> Result<(), String> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let text = serde_json::to_string(frame).map_err(|error| format!("序列化：{error}"))?;
    writeln!(handle, "{text}").map_err(|error| format!("写帧：{error}"))?;
    handle.flush().map_err(|error| format!("flush：{error}"))
}

/// 夹具内 ULID 形状 id（26 字符、首字符 ≤7；仅需唯一与形状合法）。
fn fixture_ulid(counter: u64) -> String {
    format!("01J{counter:023}")
}

/// 会话事件信封（`event` 通知；信封 9 字段，seq 由核心 sequencer 重排）。
fn session_event(
    session_id: &str,
    run_id: &str,
    event_counter: &mut u64,
    event_type: &str,
    payload: serde_json::Value,
) -> serde_json::Value {
    *event_counter += 1;
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "event",
        "params": {
            "v": 1,
            "id": fixture_ulid(100_000 + *event_counter),
            "session_id": session_id,
            "run_id": run_id,
            "runtime_id": "fixture",
            "seq": *event_counter,
            "ts": 1_700_000_000_000i64,
            "type": event_type,
            "payload": payload,
        },
    })
}

/// M3-06 最小 D6 会话面（Mode R 恢复验证宿主；不修改 DSH/hermes 源码的独立夹具）。
///
/// - `session.create`：无 `native_id` → 新建 `fixture-sess-<n>` 且返回 `native_id`；
///   携带 `native_id` → 以该 id 恢复（`resumed=true`）；
/// - `session.send`：`client_msg_id` 幂等；`fail-once` → 仅首次 `run.failed`
///   （重放成功；M3-06 重放验证）；`fail*` → 恒 `run.failed`；`long` → 保持
///   在途直至 `session.interrupt`（`run.cancelled`）；其余 → 2 delta + 终稿 + 完成；
/// - 调用记录写入 `--session-log`（JSON Lines）。
fn serve_session(args: &Args) -> Result<(), String> {
    let hello = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "hello",
        "params": {
            "protocol": "1.0",
            "runtime": {"name": "fixture", "version": "0.1.0", "capabilities": ["session.create", "session.send"]},
        },
    });
    write_frame(&hello)?;

    let mut sessions: std::collections::HashMap<String, SessionFixtureState> =
        std::collections::HashMap::new();
    let mut session_counter: u64 = 0;
    let mut event_counter: u64 = 0;
    // `fail-once` 为**进程级**首次消费（Mode R 恢复会重建会话状态，进程级计数跨恢复保持）。
    let mut fail_once_used = false;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("读 stdin：{error}"))?;
        if read == 0 {
            return Ok(()); // 核心退出（stdin EOF）。
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let frame: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(frame) => frame,
            Err(_) => continue,
        };
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = frame
            .get("method")
            .and_then(|method| method.as_str())
            .unwrap_or_default()
            .to_owned();
        let params = frame
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let respond = |result: serde_json::Value| -> Result<(), String> {
            write_frame(&serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}))
        };
        let respond_error = |code: i64, message: &str| -> Result<(), String> {
            write_frame(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": code, "message": message},
            }))
        };
        match method.as_str() {
            "initialize" => respond(serde_json::json!({"acknowledged": true}))?,
            "session.create" => {
                session_counter += 1;
                let native_id = params
                    .get("native_id")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned);
                let resumed = native_id.is_some();
                let session_id =
                    native_id.unwrap_or_else(|| format!("fixture-sess-{session_counter}"));
                sessions.insert(session_id.clone(), SessionFixtureState::default());
                log_session_call(
                    args,
                    serde_json::json!({
                        "method": "session.create",
                        "session_id": session_id,
                        "native_id": session_id,
                        "resumed": resumed,
                    }),
                );
                respond(serde_json::json!({
                    "session_id": session_id,
                    "native_id": session_id,
                    "resumed": resumed,
                    "created_at": 1_700_000_000_000i64,
                }))?;
            }
            "session.send" => {
                let session_id = params
                    .get("session_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let client_msg_id = params
                    .get("client_msg_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let text = params
                    .get("text")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let Some(session) = sessions.get_mut(&session_id) else {
                    respond_error(1005, &format!("会话不存在: {session_id}"))?;
                    continue;
                };
                if session.disposed {
                    respond_error(1005, &format!("会话已关闭: {session_id}"))?;
                    continue;
                }
                if let Some(existing) = session.client_msg_ids.get(&client_msg_id) {
                    respond(serde_json::json!({
                        "accepted": true,
                        "run_id": existing,
                        "duplicate": true,
                    }))?;
                    continue;
                }
                let run_id = fixture_ulid(200_000 + session_counter * 1_000 + event_counter);
                session
                    .client_msg_ids
                    .insert(client_msg_id.clone(), run_id.clone());
                log_session_call(
                    args,
                    serde_json::json!({
                        "method": "session.send",
                        "session_id": session_id,
                        "client_msg_id": client_msg_id,
                        "text": text,
                    }),
                );
                write_frame(&session_event(
                    &session_id,
                    &run_id,
                    &mut event_counter,
                    "run.started",
                    serde_json::json!({"run_id": run_id}),
                ))?;
                let fail_once = text == "fail-once" && !fail_once_used;
                if fail_once {
                    fail_once_used = true;
                }
                if fail_once || (text.starts_with("fail") && text != "fail-once") {
                    write_frame(&session_event(
                        &session_id,
                        &run_id,
                        &mut event_counter,
                        "run.failed",
                        serde_json::json!({
                            "run_id": run_id,
                            "error": {"code": "fixture_fail", "message": "夹具注入失败", "recoverable": true},
                        }),
                    ))?;
                } else if text == "long" {
                    // 保持在途：终态由 `session.interrupt` 收口（崩溃/中断场景）。
                    session.active_run = Some(run_id.clone());
                } else {
                    let message_id = fixture_ulid(300_000 + event_counter);
                    let mut content = String::new();
                    for fragment in ["fixture-", "done"] {
                        content.push_str(fragment);
                        write_frame(&session_event(
                            &session_id,
                            &run_id,
                            &mut event_counter,
                            "message.delta",
                            serde_json::json!({"message_id": message_id, "text": fragment}),
                        ))?;
                    }
                    write_frame(&session_event(
                        &session_id,
                        &run_id,
                        &mut event_counter,
                        "message.completed",
                        serde_json::json!({
                            "message": {
                                "id": message_id,
                                "session_id": session_id,
                                "run_id": run_id,
                                "role": "assistant",
                                "content": content,
                                "created_at": 1_700_000_000_000i64,
                            },
                            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
                        }),
                    ))?;
                    write_frame(&session_event(
                        &session_id,
                        &run_id,
                        &mut event_counter,
                        "run.completed",
                        serde_json::json!({
                            "run_id": run_id,
                            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
                        }),
                    ))?;
                }
                respond(serde_json::json!({"accepted": true, "run_id": run_id}))?;
            }
            "session.interrupt" => {
                let session_id = params
                    .get("session_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let Some(session) = sessions.get_mut(&session_id) else {
                    respond_error(1005, &format!("会话不存在: {session_id}"))?;
                    continue;
                };
                let interrupted = session.active_run.take();
                if let Some(run_id) = &interrupted {
                    write_frame(&session_event(
                        &session_id,
                        run_id,
                        &mut event_counter,
                        "run.cancelled",
                        serde_json::json!({"run_id": run_id, "reason": "interrupted"}),
                    ))?;
                }
                respond(serde_json::json!({
                    "interrupted": interrupted.is_some(),
                    "run_id": interrupted,
                }))?;
            }
            "session.dispose" => {
                let session_id = params
                    .get("session_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned();
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.disposed = true;
                }
                respond(serde_json::json!({"disposed": true}))?;
            }
            "health.ping" => respond(serde_json::json!({"status": "ok"}))?,
            "shutdown" => {
                respond(serde_json::json!({"ok": true}))?;
                return Ok(());
            }
            other => respond_error(-32601, &format!("未知方法：{other}"))?,
        }
    }
}

/// 资源夹具（M1-10 增量 / M4-01 复用）：真实分配 `--mb` MiB 并逐页触写。
///
/// `--mb-cycle-file <path>` 提供跨重启交替：读取计数 N、回写 N+1，N 为偶数才分配。
/// 由于宿主释放大块内存后可能保留物理页（macOS 实测 RSS 不回落），集成测试
/// 用「崩溃 → 监督器重启」切换进程来实现真实回落：run#1 fat → run#2 lean → run#3 fat。
fn start_memory_profile(args: &Args) {
    if args.mb == 0 {
        return;
    }
    let allocate = match &args.mb_cycle_file {
        Some(path) => next_cycle_allocates(path),
        None => true,
    };
    if !allocate {
        return;
    }
    let mb = args.mb;
    std::thread::spawn(move || {
        let held = allocate_memory(mb);
        // 持有分配内存直到进程退出（RSS 采样可观测）。
        loop {
            std::hint::black_box(&held);
            std::thread::sleep(MEMORY_HOLD_TICK);
        }
    });
}

/// 读取并推进计数文件；返回是否本次应分配（偶数计数 → 分配）。
fn next_cycle_allocates(path: &std::path::Path) -> bool {
    let count = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if let Err(error) = std::fs::write(path, count.saturating_add(1).to_string()) {
        eprintln!("[fixture] mb-cycle-file 写入失败（按首次分配继续）：{error}");
    }
    count % 2 == 0
}

/// 分配并逐页写入（触发物理页提交，保证 RSS 真实上升）。
fn allocate_memory(mb: u64) -> Vec<u8> {
    let bytes = usize::try_from(mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX);
    let mut buffer = vec![0_u8; bytes];
    let mut index = 0;
    while index < buffer.len() {
        buffer[index] = 0xAB;
        index = index.saturating_add(4096);
    }
    buffer
}

/// 父进程 + 子进程（同一可执行文件，child 与父同进程组/Job，验证整树回收）。
fn spawn_tree(args: &Args) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| format!("current_exe：{error}"))?;
    let mut child_args = vec![
        "--mode".to_owned(),
        "sleep".to_owned(),
        "--seconds".to_owned(),
        args.seconds.to_string(),
    ];
    if let Some(token) = &args.launch_token {
        child_args.push("--launch-token".to_owned());
        child_args.push(format!("{token}-child"));
    }
    let mut child = std::process::Command::new(exe)
        .args(&child_args)
        .spawn()
        .map_err(|error| format!("spawn 子进程：{error}"))?;
    if let Some(path) = &args.pid_file {
        let payload = format!("parent={}\nchild={}\n", std::process::id(), child.id());
        std::fs::write(path, payload).map_err(|error| format!("写 pid-file：{error}"))?;
    }
    sleep_seconds(args.seconds);
    let _ = child.wait();
    Ok(())
}

/// 最小线协议行为：hello + `initialize` 应答，其余请求一律不响应（T5b 心跳失败注入）。
///
/// `--mb` 等资源参数存在时同时启动内存夹具线程（M1-10 资源告警集成 / M4-01 复用）。
fn serve_deaf(args: &Args) -> Result<(), String> {
    start_memory_profile(args);
    let hello = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "hello",
        "params": {
            "protocol": "1.0",
            "runtime": {"name": "fixture", "version": "0.1.0", "capabilities": []},
        },
    });
    let stdout = std::io::stdout();
    {
        let mut handle = stdout.lock();
        writeln!(handle, "{hello}").map_err(|error| format!("写 hello：{error}"))?;
        handle.flush().map_err(|error| format!("flush：{error}"))?;
    }

    let started = std::time::Instant::now();
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    loop {
        if started.elapsed() >= Duration::from_secs(args.seconds) {
            return Ok(());
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // M2-08 孤儿场景：核心被强杀 → stdin 写端关闭。真实卡死适配器不会因
                // EOF 自行退出；`--survive-eof` 显式保持存活至 `--seconds`（默认仍退出）。
                if !args.survive_eof {
                    return Ok(());
                }
                let deadline = Duration::from_secs(args.seconds);
                while started.elapsed() < deadline {
                    std::thread::sleep(Duration::from_millis(200));
                }
                return Ok(());
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let frame: serde_json::Value = match serde_json::from_str(trimmed) {
                    Ok(frame) => frame,
                    Err(_) => continue,
                };
                // `initialize` 正常应答（预热路径）；health.ping/shutdown 等一律不响应
                //（deaf 模式：模拟卡死无响应，D5 失败表「卡死无响应」）。
                if frame.get("method").and_then(|method| method.as_str()) == Some("initialize") {
                    let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"acknowledged": true},
                    });
                    let mut handle = stdout.lock();
                    writeln!(handle, "{response}").map_err(|error| format!("写响应：{error}"))?;
                    handle.flush().map_err(|error| format!("flush：{error}"))?;
                }
            }
            Err(error) => return Err(format!("读 stdin：{error}")),
        }
    }
}
