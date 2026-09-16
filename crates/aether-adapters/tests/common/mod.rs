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

    /// 继续收集事件直到指定事件出现（含）；权限请求按 `permission` 自动应答。
    pub async fn wait_for_event(
        &mut self,
        run_id: &str,
        event_type: EventType,
        permission: Option<(&str, &str)>,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let envelope = self.next_event(permission, deadline).await?;
            if envelope.run_id.as_ref().map(|id| id.as_str()) == Some(run_id)
                && envelope.event_type() == event_type
            {
                return Ok(());
            }
        }
    }

    /// 收集 run 事件直到终态（completed/failed/cancelled）；权限请求按需应答。
    pub async fn drive_run(
        &mut self,
        run_id: &str,
        permission: Option<(&str, &str)>,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let envelope = self.next_event(permission, deadline).await?;
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
        permission: Option<(&str, &str)>,
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
                    let (decision, scope) = permission
                        .ok_or_else(|| "收到 permission.request 但测试未配置决策".to_owned())?;
                    let request_id = params
                        .get("request_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "permission.request 缺少 request_id".to_owned())?;
                    self.connection
                        .request(
                            Method::PermissionResolve,
                            json!({
                                "request_id": request_id,
                                "decision": decision,
                                "scope": scope,
                            }),
                        )
                        .await
                        .map_err(|error| format!("permission.resolve 失败: {error}"))?;
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
