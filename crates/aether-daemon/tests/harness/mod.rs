//! `aetherd` 进程级测试夹具（M9-03）：spawn + JSONL 收发 + 退出码 + stderr 捕获。
//!
//! 只用于集成测试：以 `CARGO_BIN_EXE_aetherd` 启动真实二进制（stdio 传输），
//! 每个实例使用独立临时数据目录 + 空运行时注册目录（不加载/启动真实适配器）。

#![allow(dead_code)]
// 集成测试夹具：断言失败即 panic（与仓库既有集成测试同口径）。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// 默认单帧等待上限（预热含 DB 打开/迁移；CI 慢机留足余量）。
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(60);

/// `initialize` 请求行（golden fixture 同源口径）。
pub fn initialize_request(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "1.0",
            "appVersion": "0.1.0",
            "client": "electron",
            "clientCapabilities": ["events", "permissions", "dialogs", "browser-view"],
            "requiredCapabilities": ["events", "permissions"]
        }
    })
}

/// 进程级用例串行闸门。
///
/// 12 个用例各自 spawn 一个真实 `aetherd`（含 A3 keyring 自检与 DB 迁移）；并行执行会在
/// CI 慢机上互相抢 CPU，使「握手 <10s」这类时延断言失真。串行后总时长仍在数秒量级。
pub fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    match SERIAL.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 测试数据目录（数据目录 + 指针文件 + 空注册目录）。
pub struct TestDirs {
    pub root: tempfile::TempDir,
}

impl TestDirs {
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("临时目录");
        std::fs::create_dir_all(root.path().join("registry")).expect("注册目录");
        Self { root }
    }

    pub fn data_dir(&self) -> PathBuf {
        self.root.path().join("data")
    }

    pub fn pointer(&self) -> PathBuf {
        self.root.path().join("data-location.json")
    }

    pub fn registry(&self) -> PathBuf {
        self.root.path().join("registry")
    }

    /// 仅注入数据目录相关 env（A4/锁/DB 失败用例；不含注册目录覆盖）。
    pub fn base_env(&self) -> Vec<(String, String)> {
        vec![
            (
                "AETHER_DATA_DIR".to_owned(),
                self.data_dir().to_string_lossy().into_owned(),
            ),
            (
                "AETHER_DATA_LOCATION_FILE".to_owned(),
                self.pointer().to_string_lossy().into_owned(),
            ),
        ]
    }

    /// 常规 env（含空注册目录：不加载/启动真实适配器，预热快速且无副作用）。
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env = self.base_env();
        env.push((
            "AETHER_RUNTIME_REGISTRY_DIR".to_owned(),
            self.registry().to_string_lossy().into_owned(),
        ));
        env
    }
}

/// 运行中的 `aetherd` 实例。
pub struct Daemon {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Receiver<Value>,
    stdout_lines: Arc<Mutex<Vec<String>>>,
    stderr_text: Arc<Mutex<String>>,
    /// 非响应帧（`daemon.status` 通知等），供后续查询。
    pub notifications: Vec<Value>,
}

impl Daemon {
    /// 以给定 env 启动（`extra_env` 覆盖同名变量）。
    pub fn spawn_with_env(env: &[(String, String)], extra_env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aetherd"));
        command.arg("--stdio");
        for (key, value) in env {
            command.env(key, value);
        }
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags_hide();
        let mut child = command.spawn().expect("启动 aetherd");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");

        let (sender, receiver) = mpsc::channel();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let lines_for_reader = Arc::clone(&lines);
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(text) => {
                        if let Ok(mut guard) = lines_for_reader.lock() {
                            guard.push(text.clone());
                        }
                        if let Ok(value) = serde_json::from_str::<Value>(&text) {
                            if sender.send(value).is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let stderr_text = Arc::new(Mutex::new(String::new()));
        let stderr_for_reader = Arc::clone(&stderr_text);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buffer = String::new();
            loop {
                buffer.clear();
                match reader.read_line(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(mut guard) = stderr_for_reader.lock() {
                            guard.push_str(&buffer);
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            child,
            stdin,
            frames: receiver,
            stdout_lines: lines,
            stderr_text,
            notifications: Vec::new(),
        }
    }

    pub fn spawn(dirs: &TestDirs) -> Self {
        Self::spawn_with_env(&dirs.env(), &[])
    }

    /// 写入一帧（原始字节 + 换行）。
    pub fn send_raw(&mut self, raw: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin 仍打开");
        stdin.write_all(raw).expect("写 stdin");
        stdin.write_all(b"\n").expect("写换行");
        stdin.flush().expect("flush stdin");
    }

    /// 写入一帧（JSON）。
    pub fn send(&mut self, value: &Value) {
        let text = serde_json::to_vec(value).expect("序列化请求");
        self.send_raw(&text);
    }

    /// 发送请求并等待同 id 响应（跳过并记录通知）。
    pub fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if !params.is_null() {
            request["params"] = params;
        }
        self.send(&request);
        self.wait_for_id(id, FRAME_TIMEOUT)
    }

    /// 等待指定 id 的响应帧。
    pub fn wait_for_id(&mut self, id: i64, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("等待 id={id} 响应超时（{timeout:?}）");
            }
            match self.frames.recv_timeout(remaining) {
                Ok(value) => {
                    if value.get("id").and_then(Value::as_i64) == Some(id) {
                        return value;
                    }
                    self.notifications.push(value);
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("等待 id={id} 响应超时（{timeout:?}）");
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("等待 id={id} 响应时 stdout 关闭（进程退出）");
                }
            }
        }
    }

    /// 等待下一帧（无 id 匹配要求；超时返回 `None`）。
    pub fn try_next_frame(&mut self, timeout: Duration) -> Option<Value> {
        self.frames.recv_timeout(timeout).ok()
    }

    /// 等待指定阶段的 `daemon.status` 通知（先查已跳过帧）。
    pub fn wait_for_status(&mut self, phase: &str, timeout: Duration) -> Option<Value> {
        if let Some(found) = self
            .notifications
            .iter()
            .find(|frame| {
                frame.get("method").and_then(Value::as_str) == Some("daemon.status")
                    && frame["params"]["phase"].as_str() == Some(phase)
            })
            .cloned()
        {
            return Some(found);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let frame = self.try_next_frame(remaining)?;
            if frame.get("method").and_then(Value::as_str) == Some("daemon.status")
                && frame["params"]["phase"].as_str() == Some(phase)
            {
                return Some(frame);
            }
            self.notifications.push(frame);
        }
    }

    /// 完成 `initialize` 握手（返回响应）。
    pub fn initialize(&mut self, id: i64) -> Value {
        let params = initialize_request(id)["params"].clone();
        self.request(id, "initialize", params)
    }

    /// 轮询 `health` 直到核心就绪（返回最后一次 health 响应）。
    pub fn wait_ready(&mut self) -> Value {
        let deadline = Instant::now() + FRAME_TIMEOUT;
        let mut id = 5000;
        loop {
            let health = self.request(id, "health", Value::Null);
            id += 1;
            if health.get("result").is_some() {
                return health;
            }
            let code = health["error"]["data"]["aether_code"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            assert_eq!(code, "core_not_ready", "预热期间的唯一预期错误：{health}");
            if Instant::now() > deadline {
                panic!("等待核心就绪超时：{health}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 以 `shutdown` 结束并返回退出码。
    pub fn shutdown(mut self) -> i32 {
        let response = self.request(9001, "shutdown", Value::Null);
        assert_eq!(
            response["result"]["accepted"],
            json!(true),
            "shutdown 必须立即回执：{response}"
        );
        self.wait_exit(FRAME_TIMEOUT).expect("进程必须退出")
    }

    /// 等待进程退出（返回退出码；超时 `None`）。
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status.code().unwrap_or(-1)),
                Ok(None) => {
                    if Instant::now() > deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => panic!("wait 失败：{error}"),
            }
        }
    }

    /// stdout 原始行（断言「只允许 JSONL」）。
    pub fn stdout_lines(&self) -> Vec<String> {
        self.stdout_lines
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// stderr 全文（日志通道断言）。
    pub fn stderr_text(&self) -> String {
        self.stderr_text
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // 关闭 stdin 触发优雅关闭；超时则强杀（避免测试失败后残留进程）。
        self.stdin.take();
        if self.wait_exit(Duration::from_secs(15)).is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Windows：隐藏控制台窗口（与服务端 `windowsHide: true` 同口径）。
trait HideConsole {
    fn creation_flags_hide(&mut self) -> &mut Self;
}

impl HideConsole for Command {
    fn creation_flags_hide(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

/// 读取 fixture 行（`scripts/test/m9-03/fixtures/<name>`）。
pub fn fixture_lines(name: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts")
        .join("test")
        .join("m9-03")
        .join("fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取 fixture {} 失败：{error}", path.display()));
    text.split('\n')
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

/// 读取 golden 期望（`expectations.json`）。
pub fn expectations() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts")
        .join("test")
        .join("m9-03")
        .join("fixtures")
        .join("expectations.json");
    let text = std::fs::read_to_string(&path).expect("读取 expectations.json");
    serde_json::from_str(&text).expect("解析 expectations.json")
}
