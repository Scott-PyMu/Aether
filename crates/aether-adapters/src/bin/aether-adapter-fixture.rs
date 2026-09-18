//! M1-10 监督器验证夹具（T5a/T5b/M2-08/M4-01 可复用故障注入助手）。
//!
//! 用法（全部模式下 `--launch-token <token>` 均被接受，供 D5 PID 台账三条件核对）：
//!
//! ```text
//! aether-adapter-fixture --mode sleep  --seconds 60 [--launch-token T]
//! aether-adapter-fixture --mode tree   --seconds 60 --pid-file <path> [--launch-token T]
//! aether-adapter-fixture --mode stderr-crash --lines 60 --exit-code 7
//! aether-adapter-fixture --mode deaf   --seconds 60   # hello + initialize 后不响应心跳
//! aether-adapter-fixture --mode silent --seconds 60   # 存活但不发 hello（握手超时）
//! # M1-10 资源告警夹具（M4-01 复用）：真实分配内存并可按阶段回落/再分配
//! aether-adapter-fixture --mode deaf --seconds 30 --mb 128 \
//!     --release-after-secs 3 --realloc-after-secs 6 [--launch-token T]
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
}

impl Mode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "sleep" => Some(Self::Sleep),
            "tree" => Some(Self::Tree),
            "stderr-crash" => Some(Self::StderrCrash),
            "deaf" => Some(Self::Deaf),
            "silent" => Some(Self::Silent),
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
    /// 资源夹具：真实分配的内存（MiB；0 = 不分配）。
    mb: u64,
    /// 资源夹具：分配后多少秒释放（0 = 不释放）。
    release_after_secs: u64,
    /// 资源夹具：释放后多少秒再次分配（0 = 不再分配）。
    realloc_after_secs: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        mode: Mode::Sleep,
        seconds: 60,
        lines: 60,
        exit_code: 7,
        pid_file: None,
        launch_token: None,
        mb: 0,
        release_after_secs: 0,
        realloc_after_secs: 0,
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
            "--release-after-secs" => {
                args.release_after_secs = value()?
                    .parse::<u64>()
                    .map_err(|error| format!("--release-after-secs 非法：{error}"))?
            }
            "--realloc-after-secs" => {
                args.realloc_after_secs = value()?
                    .parse::<u64>()
                    .map_err(|error| format!("--realloc-after-secs 非法：{error}"))?
            }
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
    }
}

fn sleep_seconds(seconds: u64) {
    std::thread::sleep(Duration::from_secs(seconds));
}

/// 资源夹具（M1-10 增量 / M4-01 复用）：真实分配 `--mb` MiB 并逐页触写；
/// 可选在 `--release-after-secs` 后释放（内存回落），再在 `--realloc-after-secs`
/// 后重新分配（用于验证告警复位后的二次触发）。
fn start_memory_profile(args: &Args) {
    if args.mb == 0 {
        return;
    }
    let mb = args.mb;
    let release_after_secs = args.release_after_secs;
    let realloc_after_secs = args.realloc_after_secs;
    std::thread::spawn(move || {
        let mut held = allocate_memory(mb);
        if release_after_secs > 0 {
            std::thread::sleep(Duration::from_secs(release_after_secs));
            held = Vec::new();
        }
        if realloc_after_secs > 0 {
            std::thread::sleep(Duration::from_secs(realloc_after_secs));
            held = allocate_memory(mb);
        }
        // 持有分配内存直到进程退出（RSS 采样可观测）。
        loop {
            std::hint::black_box(&held);
            std::thread::sleep(MEMORY_HOLD_TICK);
        }
    });
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
            Ok(0) => return Ok(()),
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
