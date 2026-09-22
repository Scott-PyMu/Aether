//! 权限回环（M2-10；设计 D9/D6）：适配器 `permission.request` 通知 → 核心权限门决议
//! → `permission.resolve` 请求下发。
//!
//! 边界（D9 评审修订 #1 / AGENTS §2.7）：本回环**仅约束适配器经线协议上报的工具调用**；
//! 适配器进程内行为（含其自身执行的 shell 命令）不经此门。
//!
//! 零直通（M2-10 DoD1）：每个经回环上报的工具调用必须产生且仅产生一次网关决议，
//! 决议必须成功下发；[`PermissionLoopProbe`] 以计数断言该性质——
//! `requests_received == decisions == resolutions_sent` 且无解析/网关/下发失败。
//!
//! 与 M1-09 预置 ④⑤ 的关系（边界 B1）：预置路径由 Mock 自包含产出
//! `permission.requested → permission.resolved`，**不**发 `permission.request` 通知、
//! 不经核心网关；本模块是 M2 阶段的真实回环（通知 → 网关 → 决议下发）。
//!
//! 本模块不写存储、不发事件；只做「线协议通知 → 网关 → 线协议请求」的搬运。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_core::{PermissionDecision, PermissionScope};
use serde_json::{json, Value};
use tokio::task::JoinHandle;

use crate::connection::AdapterConnection;
use crate::protocol::Method;

/// `permission.resolve` 下发超时（D6 方法表 5s）。
pub const PERMISSION_RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// 失败诊断记录上限（保序，防无限增长）。
pub const PERMISSION_LOOP_ERROR_LIMIT: usize = 64;

/// 回环请求（`permission.request` 通知 params 的映射）。
///
/// 通知形状（M2-10 冻结；D6 仅固定方法名，成员为增量定义，未知成员忽略）：
/// `{request_id, session_id?, run_id?, resource, action, target?, content_bytes?}`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionLoopRequest {
    /// 适配器回环键（`permission.request.request_id`；`permission.resolve` 原样回传）。
    pub request_id: String,
    /// 适配器侧会话 id（核心侧会话行关联）。
    pub session_id: Option<String>,
    /// 适配器侧 run id（诊断用）。
    pub run_id: Option<String>,
    /// 线协议 `resource`（`fs.read` / `fs.write` / `exec` / `net`）。
    pub resource: String,
    pub action: String,
    /// 原始 target（未规范化）。
    pub target: Option<String>,
    /// 写入内容大小（记忆白名单 1MB 上限）。
    pub content_bytes: Option<u64>,
    /// 适配器 `hello` 上报的 runtime 名（由回环 pump 注入，不采信通知字段）。
    pub runtime_id: Option<String>,
}

impl PermissionLoopRequest {
    /// 从通知 `params` 解析（`runtime_id` 由连接上下文注入）。
    pub fn from_params(params: &Value, runtime_id: Option<String>) -> Result<Self, String> {
        let request_id = required_str(params, "request_id")?;
        let resource = required_str(params, "resource")?;
        let action = required_str(params, "action")?;
        let target = optional_str(params, "target");
        let session_id = optional_str(params, "session_id");
        let run_id = optional_str(params, "run_id");
        let content_bytes = params.get("content_bytes").and_then(Value::as_u64);
        Ok(Self {
            request_id,
            session_id,
            run_id,
            resource,
            action,
            target,
            content_bytes,
            runtime_id,
        })
    }

    /// `permission.resolve` 请求 params（决议 + 原因原样下发；D9：拒绝回传 `reason`）。
    pub fn resolve_params(request_id: &str, decision: &PermissionLoopDecision) -> Value {
        json!({
            "request_id": request_id,
            "decision": decision.decision.as_str(),
            "scope": decision.scope.map(|scope| scope.as_str()),
            "reason": decision.reason,
        })
    }
}

fn required_str(params: &Value, field: &str) -> Result<String, String> {
    params
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("permission.request 缺少 {field}"))
}

fn optional_str(params: &Value, field: &str) -> Option<String> {
    params
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// 网关决议（回传适配器；`decision` 仅 allow/deny，`Ask` 不进入回环）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionLoopDecision {
    pub decision: PermissionDecision,
    pub scope: Option<PermissionScope>,
    pub reason: String,
    /// 是否由 300s 超时路径判 deny（证据字段）。
    pub timed_out: bool,
}

impl PermissionLoopDecision {
    /// 允许（`scope` 为 `once`/`session`）。
    pub fn allow(scope: Option<PermissionScope>) -> Self {
        Self {
            decision: PermissionDecision::Allow,
            scope,
            reason: "允许".to_owned(),
            timed_out: false,
        }
    }

    /// 拒绝（`Ask` 一律按 deny 收口，无直通）。
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            decision: PermissionDecision::Deny,
            scope: None,
            reason: reason.into(),
            timed_out: false,
        }
    }
}

/// 网关决议 future（`'static` 无关；由回环在后台任务内 await）。
pub type PermissionGateFuture<'a> =
    Pin<Box<dyn Future<Output = Result<PermissionLoopDecision, String>> + Send + 'a>>;

/// 核心权限门（`aether-control::PermissionService` 由组合根薄适配实现，见
/// `aether-tauri::permission_loop::PermissionServiceGate`）。
pub trait PermissionGate: Send + Sync + 'static {
    /// 请求一次工具调用权限（阻塞至决议/超时；实现方不得直通）。
    fn decide(&self, request: PermissionLoopRequest) -> PermissionGateFuture<'_>;
}

/// 回环探针快照（Gate 2 显式证据：计数断言，非日志字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionLoopSnapshot {
    /// 收到的合法 `permission.request` 通知数。
    pub requests_received: u64,
    /// 形状非法被拒绝的通知数。
    pub requests_invalid: u64,
    /// 网关决议数。
    pub decisions: u64,
    /// 成功下发的 `permission.resolve` 请求数。
    pub resolutions_sent: u64,
    /// 下发失败数（超时/断连）。
    pub resolution_failures: u64,
    /// 网关自身失败数。
    pub gate_failures: u64,
}

impl PermissionLoopSnapshot {
    /// 零直通判定：无解析/网关/下发失败，且「收到 = 决议 = 下发」。
    pub fn zero_passthrough(&self) -> bool {
        self.requests_invalid == 0
            && self.gate_failures == 0
            && self.resolution_failures == 0
            && self.requests_received == self.decisions
            && self.decisions == self.resolutions_sent
    }

    /// 人类可读摘要（证据输出）。
    pub fn summary(&self) -> String {
        format!(
            "requests={} decisions={} resolves={} invalid={} gate_failures={} resolve_failures={} zero_passthrough={}",
            self.requests_received,
            self.decisions,
            self.resolutions_sent,
            self.requests_invalid,
            self.gate_failures,
            self.resolution_failures,
            self.zero_passthrough()
        )
    }
}

/// 回环探针（原子计数；证据导出用）。
#[derive(Debug, Default)]
pub struct PermissionLoopProbe {
    requests_received: AtomicU64,
    requests_invalid: AtomicU64,
    decisions: AtomicU64,
    resolutions_sent: AtomicU64,
    resolution_failures: AtomicU64,
    gate_failures: AtomicU64,
    errors: Mutex<Vec<String>>,
}

impl PermissionLoopProbe {
    pub fn new() -> Self {
        Self::default()
    }

    /// 计数快照。
    pub fn snapshot(&self) -> PermissionLoopSnapshot {
        PermissionLoopSnapshot {
            requests_received: self.requests_received.load(Ordering::SeqCst),
            requests_invalid: self.requests_invalid.load(Ordering::SeqCst),
            decisions: self.decisions.load(Ordering::SeqCst),
            resolutions_sent: self.resolutions_sent.load(Ordering::SeqCst),
            resolution_failures: self.resolution_failures.load(Ordering::SeqCst),
            gate_failures: self.gate_failures.load(Ordering::SeqCst),
        }
    }

    /// 失败诊断（保序、上限 [`PERMISSION_LOOP_ERROR_LIMIT`]）。
    pub fn errors(&self) -> Vec<String> {
        match self.errors.lock() {
            Ok(errors) => errors.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn record_error(&self, detail: String) {
        if let Ok(mut errors) = self.errors.lock() {
            if errors.len() >= PERMISSION_LOOP_ERROR_LIMIT {
                errors.remove(0);
            }
            errors.push(detail);
        }
    }
}

/// 权限回环（`Arc` 共享；由会话客户端通知 pump 调用）。
pub struct PermissionLoop {
    gate: Arc<dyn PermissionGate>,
    probe: Arc<PermissionLoopProbe>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for PermissionLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionLoop")
            .field("probe", &self.probe.snapshot())
            .field("pending_tasks", &self.pending_tasks())
            .finish_non_exhaustive()
    }
}

impl PermissionLoop {
    /// 以网关构造回环（内部新建探针）。
    pub fn new(gate: Arc<dyn PermissionGate>) -> Arc<Self> {
        Self::with_probe(gate, Arc::new(PermissionLoopProbe::new()))
    }

    /// 以既有探针构造（证据导出方持有同一探针）。
    pub fn with_probe(gate: Arc<dyn PermissionGate>, probe: Arc<PermissionLoopProbe>) -> Arc<Self> {
        Arc::new(Self {
            gate,
            probe,
            tasks: Mutex::new(Vec::new()),
        })
    }

    /// 探针句柄。
    pub fn probe(&self) -> Arc<PermissionLoopProbe> {
        Arc::clone(&self.probe)
    }

    /// 处理一条 `permission.request` 通知：解析 → 网关决议 → `permission.resolve` 下发。
    ///
    /// 由通知 pump 调用；决议在后台任务执行（不阻塞 pump / 其他通知）。
    pub fn dispatch(
        self: &Arc<Self>,
        connection: Arc<AdapterConnection>,
        params: Value,
        runtime_id: Option<String>,
    ) {
        let request = match PermissionLoopRequest::from_params(&params, runtime_id) {
            Ok(request) => request,
            Err(detail) => {
                self.probe.requests_invalid.fetch_add(1, Ordering::SeqCst);
                self.probe
                    .record_error(format!("permission.request 解析失败: {detail}"));
                return;
            }
        };
        self.probe.requests_received.fetch_add(1, Ordering::SeqCst);
        let this = Arc::clone(self);
        let task = tokio::spawn(async move {
            let decision = match this.gate.decide(request.clone()).await {
                Ok(decision) => decision,
                Err(detail) => {
                    this.probe.gate_failures.fetch_add(1, Ordering::SeqCst);
                    this.probe
                        .record_error(format!("网关决议失败（{}）: {detail}", request.request_id));
                    return;
                }
            };
            this.probe.decisions.fetch_add(1, Ordering::SeqCst);
            let params = PermissionLoopRequest::resolve_params(&request.request_id, &decision);
            match connection
                .request_with_timeout(
                    Method::PermissionResolve,
                    params,
                    PERMISSION_RESOLVE_TIMEOUT,
                )
                .await
            {
                Ok(_) => {
                    this.probe.resolutions_sent.fetch_add(1, Ordering::SeqCst);
                }
                Err(error) => {
                    this.probe
                        .resolution_failures
                        .fetch_add(1, Ordering::SeqCst);
                    this.probe.record_error(format!(
                        "permission.resolve 下发失败（{}）: {error}",
                        request.request_id
                    ));
                }
            }
        });
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.retain(|handle| !handle.is_finished());
            tasks.push(task);
        }
    }

    /// 中止在途回环任务（核心关闭/重启序列；返回中止数）。
    pub async fn shutdown(&self) -> usize {
        let handles: Vec<JoinHandle<()>> = match self.tasks.lock() {
            Ok(mut tasks) => tasks.drain(..).collect(),
            Err(poisoned) => poisoned.into_inner().drain(..).collect(),
        };
        let count = handles.len();
        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
        count
    }

    /// 在途回环任务数（诊断）。
    pub fn pending_tasks(&self) -> usize {
        match self.tasks.lock() {
            Ok(tasks) => tasks.iter().filter(|handle| !handle.is_finished()).count(),
            Err(poisoned) => poisoned
                .into_inner()
                .iter()
                .filter(|handle| !handle.is_finished())
                .count(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// 记录型网关：固定决议 + 调用计数。
    struct RecordingGate {
        calls: AtomicUsize,
        requests: Mutex<Vec<PermissionLoopRequest>>,
        decision: PermissionLoopDecision,
    }

    impl RecordingGate {
        fn new(decision: PermissionLoopDecision) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
                decision,
            })
        }
    }

    impl PermissionGate for RecordingGate {
        fn decide(&self, request: PermissionLoopRequest) -> PermissionGateFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(request);
            let decision = self.decision.clone();
            Box::pin(async move { Ok(decision) })
        }
    }

    fn params() -> Value {
        json!({
            "request_id": "01J00000000000000000000PR",
            "session_id": "mock-sess-1",
            "run_id": "01J00000000000000000000RU",
            "resource": "fs.write",
            "action": "write",
            "target": "notes.md",
        })
    }

    #[test]
    fn request_parses_full_and_minimal_shapes() {
        let request =
            PermissionLoopRequest::from_params(&params(), Some("mock".to_owned())).unwrap();
        assert_eq!(request.request_id, "01J00000000000000000000PR");
        assert_eq!(request.session_id.as_deref(), Some("mock-sess-1"));
        assert_eq!(request.run_id.as_deref(), Some("01J00000000000000000000RU"));
        assert_eq!(request.resource, "fs.write");
        assert_eq!(request.action, "write");
        assert_eq!(request.target.as_deref(), Some("notes.md"));
        assert_eq!(request.runtime_id.as_deref(), Some("mock"));

        let minimal = json!({
            "request_id": "r-1",
            "resource": "fs.read",
            "action": "read",
            "target": null,
            "content_bytes": 1024,
        });
        let request = PermissionLoopRequest::from_params(&minimal, None).unwrap();
        assert_eq!(request.target, None);
        assert_eq!(request.content_bytes, Some(1024));
        assert_eq!(request.session_id, None);
    }

    #[test]
    fn invalid_shapes_are_rejected_with_field_reason() {
        for (label, value) in [
            (
                "缺 request_id",
                json!({"resource": "fs.read", "action": "read"}),
            ),
            (
                "空 request_id",
                json!({"request_id": "", "resource": "fs.read", "action": "read"}),
            ),
            (
                "缺 resource",
                json!({"request_id": "r-1", "action": "read"}),
            ),
            (
                "缺 action",
                json!({"request_id": "r-1", "resource": "fs.read"}),
            ),
        ] {
            let error = PermissionLoopRequest::from_params(&value, None).unwrap_err();
            assert!(!error.is_empty(), "{label}");
        }
    }

    #[test]
    fn resolve_params_carry_decision_scope_and_reason() {
        let allow = PermissionLoopDecision::allow(Some(PermissionScope::Once));
        let params = PermissionLoopRequest::resolve_params("r-1", &allow);
        assert_eq!(params["decision"], "allow");
        assert_eq!(params["scope"], "once");
        assert!(params["reason"].as_str().is_some());

        let deny = PermissionLoopDecision::deny("策略拒绝");
        let params = PermissionLoopRequest::resolve_params("r-2", &deny);
        assert_eq!(params["decision"], "deny");
        assert_eq!(params["scope"], Value::Null);
        assert_eq!(params["reason"], "策略拒绝");
    }

    #[test]
    fn snapshot_zero_passthrough_requires_exact_equality() {
        let probe = PermissionLoopProbe::new();
        assert!(probe.snapshot().zero_passthrough(), "空快照视为零直通");

        probe.requests_received.fetch_add(2, Ordering::SeqCst);
        probe.decisions.fetch_add(1, Ordering::SeqCst);
        assert!(
            !probe.snapshot().zero_passthrough(),
            "收到 2 决议 1 → 存在直通"
        );

        probe.decisions.fetch_add(1, Ordering::SeqCst);
        assert!(!probe.snapshot().zero_passthrough(), "决议未下发 → 不完整");
        probe.resolutions_sent.fetch_add(2, Ordering::SeqCst);
        assert!(probe.snapshot().zero_passthrough());

        probe.resolution_failures.fetch_add(1, Ordering::SeqCst);
        assert!(
            !probe.snapshot().zero_passthrough(),
            "下发失败不得视为零直通"
        );
    }

    #[test]
    fn probe_error_log_is_bounded_and_ordered() {
        let probe = PermissionLoopProbe::new();
        for index in 0..(PERMISSION_LOOP_ERROR_LIMIT + 5) {
            probe.record_error(format!("e{index}"));
        }
        let errors = probe.errors();
        assert_eq!(errors.len(), PERMISSION_LOOP_ERROR_LIMIT);
        assert_eq!(errors[0], "e5", "最旧记录被裁剪");
        assert_eq!(
            errors[PERMISSION_LOOP_ERROR_LIMIT - 1],
            format!("e{}", PERMISSION_LOOP_ERROR_LIMIT + 4)
        );
    }

    #[test]
    fn snapshot_summary_contains_zero_passthrough_flag() {
        let probe = PermissionLoopProbe::new();
        let summary = probe.snapshot().summary();
        assert!(summary.contains("requests=0"));
        assert!(summary.contains("zero_passthrough=true"));
    }

    #[test]
    fn shutdown_without_tasks_is_noop() {
        let gate = RecordingGate::new(PermissionLoopDecision::allow(None));
        let loop_ = PermissionLoop::new(gate);
        assert_eq!(loop_.pending_tasks(), 0);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(runtime.block_on(loop_.shutdown()), 0);
    }
}
