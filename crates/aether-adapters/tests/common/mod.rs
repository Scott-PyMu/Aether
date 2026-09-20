//! M1-09 集成测试共享工具：启动真实 Mock 适配器进程并驱动线协议。
//!
//! 通过 `AETHER_MOCK_ADAPTER` 指定 Bun 编译产物的路径；未设置时测试跳过
//! （由 `scripts/test/m1-09/verify-m1-09.mjs` 负责构建并设置；
//!  设置 `AETHER_REQUIRE_MOCK_ADAPTER=1` 时缺路径直接失败）。
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use aether_adapters::{
    AdapterConnection, AdapterNotification, AdapterProcess, ConnectionState, DisconnectReason,
    Method, RequestError,
};
use aether_core::{EventEnvelope, EventType};
use serde_json::{json, Value};

/// Mock 适配器路径（未设置且不要求时必须跳过）。
pub fn mock_binary() -> Option<PathBuf> {
    match std::env::var_os("AETHER_MOCK_ADAPTER") {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            if std::env::var("AETHER_REQUIRE_MOCK_ADAPTER").as_deref() == Ok("1") {
                panic!("AETHER_REQUIRE_MOCK_ADAPTER=1 但 AETHER_MOCK_ADAPTER 未设置");
            }
            eprintln!(
                "SKIP：AETHER_MOCK_ADAPTER 未设置（运行 pnpm verify:m1-09 构建 Mock 后执行）"
            );
            None
        }
    }
}

/// 真实 Mock 进程 + 线协议连接 + 已收集事件缓冲。
pub struct MockHarness {
    process: AdapterProcess,
    pub connection: AdapterConnection,
    pub events: Vec<EventEnvelope>,
    /// B3 边界证据（非功能验证）：M1 预置 ④⑤ 期间收到的 `permission.request` 通知。
    /// 预置路径应为空；M2-10 真实回环接入后此断言随之演进。
    pub permission_requests: Vec<Value>,
}

impl MockHarness {
    /// 启动 Mock 进程并建立连接（不握手）。
    pub async fn launch(args: &[&str]) -> Option<Self> {
        let path = mock_binary()?;
        let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let mut process = AdapterProcess::spawn(&path, owned)
            .await
            .expect("启动 Mock 适配器进程");
        let connection = process.connect().expect("连接 Mock stdio");
        Some(Self {
            process,
            connection,
            events: Vec::new(),
            permission_requests: Vec::new(),
        })
    }

    /// 启动并完成 10s 握手（major 校验通过）。
    pub async fn launch_ready(args: &[&str]) -> Option<Self> {
        let harness = Self::launch(args).await?;
        let hello = harness
            .connection
            .handshake()
            .await
            .expect("hello 必须在 10s 内到达且 major 兼容");
        assert_eq!(hello.protocol, "1.0");
        assert_eq!(hello.runtime.name, "mock");
        assert!(hello.runtime.has_capability("session.send"));
        Some(harness)
    }

    pub async fn open_session(&mut self) -> String {
        let initialized = self
            .connection
            .request(Method::Initialize, json!({"config": {}}))
            .await
            .expect("initialize");
        assert_eq!(initialized["acknowledged"], true);
        let created = self
            .connection
            .request(Method::SessionCreate, json!({"title": "m1-09"}))
            .await
            .expect("session.create");
        created["session_id"]
            .as_str()
            .expect("返回 session_id")
            .to_owned()
    }

    pub async fn send(&mut self, session_id: &str, text: &str, client_msg_id: &str) -> String {
        let ack = self
            .connection
            .request(
                Method::SessionSend,
                json!({
                    "session_id": session_id,
                    "client_msg_id": client_msg_id,
                    "text": text,
                }),
            )
            .await
            .expect("session.send ack（必须快返回，不等模型）");
        ack["run_id"].as_str().expect("ack 携带 run_id").to_owned()
    }

    pub async fn interrupt(&mut self, session_id: &str) -> Value {
        self.connection
            .request(Method::SessionInterrupt, json!({"session_id": session_id}))
            .await
            .expect("session.interrupt（5s 超时内）")
    }

    /// 继续收集事件直到指定事件出现（含）。
    pub async fn wait_for_event(
        &mut self,
        run_id: &str,
        event_type: EventType,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let envelope = self.next_event(deadline).await?;
            if envelope.run_id.as_ref().map(|id| id.as_str()) == Some(run_id)
                && envelope.event_type() == event_type
            {
                return Ok(());
            }
        }
    }

    /// 收集 run 事件直到终态（completed/failed/cancelled）。
    pub async fn drive_run(&mut self, run_id: &str, timeout: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let envelope = self.next_event(deadline).await?;
            if envelope.run_id.as_ref().map(|id| id.as_str()) != Some(run_id) {
                continue;
            }
            match envelope.event_type() {
                EventType::RunCompleted | EventType::RunFailed | EventType::RunCancelled => {
                    return Ok(())
                }
                _ => {}
            }
        }
    }

    async fn next_event(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> Result<EventEnvelope, String> {
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "等待事件超时（已收集 {} 条；连接状态={:?}）",
                    self.events.len(),
                    self.connection.state()
                ));
            }
            let item =
                match tokio::time::timeout(remaining, self.connection.next_notification()).await {
                    Ok(Some(item)) => item,
                    Ok(None) => return Err("通知通道关闭".to_owned()),
                    Err(_) => return Err("等待事件超时".to_owned()),
                };
            match item {
                AdapterNotification::Event(envelope) => {
                    let envelope = *envelope;
                    self.events.push(envelope.clone());
                    return Ok(envelope);
                }
                AdapterNotification::PermissionRequest(params) => {
                    // 边界 B3：M1 预置 ④⑤ 不应产生该通知；记录后继续（不代答网关）。
                    self.permission_requests.push(params);
                }
                AdapterNotification::Log(_) | AdapterNotification::Other { .. } => {}
            }
        }
    }

    pub fn run_events(&self, run_id: &str) -> Vec<&EventEnvelope> {
        self.events
            .iter()
            .filter(|event| event.run_id.as_ref().map(|id| id.as_str()) == Some(run_id))
            .collect()
    }

    pub fn run_types(&self, run_id: &str) -> Vec<String> {
        self.run_events(run_id)
            .into_iter()
            .map(|event| event.event_type().as_str().to_owned())
            .collect()
    }

    /// 工具/权限事件的精确序列（DoD6 断言口径）。
    pub fn tool_sequence(&self, run_id: &str) -> Vec<String> {
        self.run_types(run_id)
            .into_iter()
            .filter(|kind| kind.starts_with("tool.") || kind.starts_with("permission."))
            .collect()
    }

    pub fn payload_of<'a>(
        &'a self,
        run_id: &str,
        event_type: EventType,
    ) -> Option<&'a EventEnvelope> {
        self.run_events(run_id)
            .into_iter()
            .find(|event| event.event_type() == event_type)
    }

    /// 发送 shutdown（5s）并等待进程退出（5s）。
    pub async fn shutdown(&mut self) -> Option<ExitStatus> {
        let _ = self.connection.request(Method::Shutdown, json!({})).await;
        self.process.wait_timeout(Duration::from_secs(5)).await.ok()
    }

    pub async fn kill(&mut self) {
        let _ = self.process.kill().await;
    }

    pub fn exit_status(&mut self) -> Option<ExitStatus> {
        self.process.try_status()
    }

    /// 等待进程退出（崩溃注入后可能有毫秒级竞态）。
    pub async fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        self.process.wait_timeout(timeout).await.ok()
    }

    pub fn stderr_tail(&self) -> Vec<String> {
        self.process.stderr_tail()
    }

    pub fn connection_state(&self) -> ConnectionState {
        self.connection.state()
    }

    /// 等待连接断开并返回原因。
    pub async fn wait_for_disconnect(&mut self, timeout: Duration) -> Option<DisconnectReason> {
        self.connection.wait_for_disconnect(timeout).await
    }
}

/// 轮询等待条件成立。
pub async fn wait_until<F: Fn() -> bool>(condition: F, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    condition()
}

/// 断言错误码（用于 `Result<Value, RequestError>`）。
pub fn rpc_code(error: &RequestError) -> i64 {
    error.code()
}

// ===== M1-09 DoD6 权威夹具（与 TS / Node 共用同一份 JSON）=====

/// DoD6 权威场景（`scripts/test/m1-09/fixtures/tool-call-scenarios.json`）。
#[derive(Debug, Clone)]
pub struct FixtureScenario {
    pub id: String,
    pub label: String,
    pub trigger: String,
    pub events: Vec<String>,
    pub terminal: String,
    pub decision: Option<String>,
    pub error_code: Option<String>,
    pub interruption: Option<String>,
}

/// 读取全部权威场景（夹具为权威定义，供 M2-02/M2-10 复用）。
pub fn fixture_scenarios() -> Vec<FixtureScenario> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/test/m1-09/fixtures/tool-call-scenarios.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取权威夹具失败 {}: {error}", path.display()));
    let value: Value = serde_json::from_str(&text).expect("权威夹具 JSON 解析");
    value["scenarios"]
        .as_array()
        .expect("scenarios 数组")
        .iter()
        .map(|scenario| FixtureScenario {
            id: str_field(scenario, "id"),
            label: str_field(scenario, "label"),
            trigger: str_field(scenario, "trigger"),
            events: scenario["events"]
                .as_array()
                .expect("events 数组")
                .iter()
                .map(|event| event.as_str().unwrap_or_default().to_owned())
                .collect(),
            terminal: str_field(scenario, "terminal"),
            decision: scenario
                .get("decision")
                .and_then(Value::as_str)
                .map(str::to_owned),
            error_code: scenario
                .get("errorCode")
                .and_then(Value::as_str)
                .map(str::to_owned),
            interruption: scenario
                .get("interruption")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
        .collect()
}

/// 按 id 取权威场景（缺失即失败，防止测试与夹具漂移）。
pub fn fixture_scenario(id: &str) -> FixtureScenario {
    fixture_scenarios()
        .into_iter()
        .find(|scenario| scenario.id == id)
        .unwrap_or_else(|| panic!("权威夹具缺少场景 {id}"))
}

fn str_field(value: &Value, field: &str) -> String {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("夹具字段 {field} 缺失"))
        .to_owned()
}

// ===== M1-10 监督器测试支持 =====

use aether_adapters::supervisor::{
    AuditKind, AuditRecord, ResourceEvent, StatusChange, SupervisorObserver,
};
use aether_core::RuntimeStatus;
use std::process::Command;
use std::sync::Mutex;

/// M1-10 故障注入夹具路径（Cargo 为同包集成测试注入 `CARGO_BIN_EXE_*`）。
pub fn fixture_binary() -> &'static str {
    env!("CARGO_BIN_EXE_aether-adapter-fixture")
}

/// 启动一个常驻夹具进程（std 直启；台账/终止测试用），返回子进程句柄。
///
/// Unix：新建独立进程组（`process_group(0)`，pgid == pid），与生产侧
/// `ProcessSession`/`setsid` 的适配器进程模型一致——台账整树回收
/// （`kill -KILL -<pgid>`）依赖该前提。
pub fn spawn_fixture(args: &[&str]) -> std::process::Child {
    let mut command = Command::new(fixture_binary());
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().expect("启动 aether-adapter-fixture")
}

/// 判断 PID 是否存活（跨平台：Windows `tasklist`，Unix `kill -0`）。
///
/// Unix 僵尸进程（已退出未 reap）对 `kill -0` 仍返回成功，必须显式判死：
/// Linux 读 `/proc/<pid>/stat` 状态位，macOS 读 `ps -o state=`。
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")),
            Err(_) => false,
        }
    }
    #[cfg(unix)]
    {
        #[cfg(target_os = "linux")]
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            if let Some(after_comm) = stat.rsplit(')').next() {
                if after_comm.trim_start().starts_with('Z') {
                    return false;
                }
            }
        }
        #[cfg(target_os = "macos")]
        if let Ok(output) = Command::new("ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()
        {
            let state = String::from_utf8_lossy(&output.stdout);
            let state = state.trim();
            if state.is_empty() || state.starts_with('Z') {
                return false;
            }
        }
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

/// 强杀夹具（std 子进程；测试清理用，含整树）。
pub fn kill_fixture(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 记录型观察者（M1-10 集成断言：状态转移/审计/资源告警）。
#[derive(Debug, Default)]
pub struct ObserverLog {
    pub status_changes: Vec<StatusChange>,
    pub audits: Vec<AuditRecord>,
    pub alerts: Vec<ResourceEvent>,
}

#[derive(Debug, Default)]
pub struct RecordingObserver {
    log: Mutex<ObserverLog>,
}

impl RecordingObserver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> ObserverLog {
        match self.log.lock() {
            Ok(log) => ObserverLog {
                status_changes: log.status_changes.clone(),
                audits: log.audits.clone(),
                alerts: log.alerts.clone(),
            },
            Err(poisoned) => {
                let log = poisoned.into_inner();
                ObserverLog {
                    status_changes: log.status_changes.clone(),
                    audits: log.audits.clone(),
                    alerts: log.alerts.clone(),
                }
            }
        }
    }

    /// 转移序列（from → to）。
    pub fn transitions(&self) -> Vec<(RuntimeStatus, RuntimeStatus)> {
        self.snapshot()
            .status_changes
            .into_iter()
            .map(|change| (change.from, change.to))
            .collect()
    }

    /// 转移原因序列（None 保留）。
    pub fn reasons(&self) -> Vec<Option<String>> {
        self.snapshot()
            .status_changes
            .into_iter()
            .map(|change| change.reason.map(|reason| reason.as_str().to_owned()))
            .collect()
    }

    pub fn has_audit(&self, kind: AuditKind) -> bool {
        self.snapshot()
            .audits
            .iter()
            .any(|record| record.kind() == kind)
    }
}

impl SupervisorObserver for RecordingObserver {
    fn on_status_changed(&self, change: &StatusChange) {
        if let Ok(mut log) = self.log.lock() {
            log.status_changes.push(change.clone());
        }
    }

    fn on_audit(&self, record: &AuditRecord) {
        if let Ok(mut log) = self.log.lock() {
            log.audits.push(record.clone());
        }
    }

    fn on_resource_alert(&self, alert: &ResourceEvent) {
        if let Ok(mut log) = self.log.lock() {
            log.alerts.push(alert.clone());
        }
    }
}

/// M1-10 临时目录（每个测试用例唯一，避免并发互踩）。
///
/// 目录名 = tag + 进程 id + 单调计数 + UNIX 纳秒 nonce。仅用 `pid-counter` 不够：
/// OS 会回收进程 id，上一轮运行遗留的目录会被同名复用，其中的旧产物（如 pid-file）
/// 可能被本轮当成新结果读取（Gate 1 `m1_10_termination` PID 断言 flaky 根因）。
/// 命中已存在目录时先清空，保证调用方拿到干净目录。
pub fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let dir = std::env::temp_dir()
        .join("aether-m1-10-tests")
        .join(format!(
            "{tag}-{}-{}-{nonce}",
            std::process::id(),
            unique_counter()
        ));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn unique_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// 轮询等待异步条件。
pub async fn wait_for_async<F, Fut>(mut condition: F, timeout: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
