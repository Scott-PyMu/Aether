//! 权限服务（M2-03；设计 D9）：策略评估 → 审批（pending 持久化 / 300s 超时 deny）
//! → 审计最小写入 → `permission.requested/resolved` 事件。
//!
//! M2-05：审批等待可经取消树级联取消（[`PermissionService::request_cancellable`]）——
//! `interrupt`/`dispose`/父会话取消命中时，待审批票据按 deny 收口（审计
//! `permission.cancelled`）；D9 边界口径不变（仅约束经线协议上报的工具调用）。
//!
//! 分层：
//! - 决策（矩阵/路径/审批票据）在 `aether-security::permission`（纯模型，无 I/O）；
//! - 本模块承接持久化（`permissions` / `audit_log` 经单写队列）、超时巡检（注入时钟）、
//!   事件广播（先日志后广播）与等待者唤醒。
//!
//! 边界（D9 评审修订 #1 / AGENTS §2.7）：仅约束适配器经线协议上报的工具调用；
//! 适配器进程内行为不经此门。
//!
//! T6（100 次并发 ask）语义：服务层不限制同会话并发 pending（每请求独立等待者与行），
//! 无丢失/重复/死锁；「同一会话最多 1 个待审批」是 UI 排队展示口径（M3 工作台）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_core::{
    PermissionDecision, PermissionScope, PermissionStatus, RuntimeId, SessionId, ENVELOPE_FIELDS,
    EVENT_ENVELOPE_VERSION,
};
use aether_security::{
    ApprovalQueue, ApprovalTicket, PathViolation, PermissionResource, PolicyDecision, PolicyEngine,
    PolicyRequest, APPROVAL_TIMEOUT_MS,
};
use aether_store::{
    AuditLogRecord, PermissionRecord, ReadPool, StoreCommand, StoreError, StoreOutcome, WriteQueue,
};
use serde_json::{json, Value};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::cancel::RunCancelToken;
use crate::clock::SharedClock;
use crate::error::PipelineError;
use crate::pipeline::{EventPipeline, SubmitOutcome};
use crate::ulid;

/// 服务配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionConfig {
    /// 审批超时（D9：300s；测试经时钟注入而非改此常量）。
    pub ask_timeout_ms: i64,
    /// 超时巡检周期（默认 1s）。
    pub sweep_tick: Duration,
    /// 等待者兜底超时（真实时间；`None` = 仅靠巡检/显式决议，测试用）。
    /// 默认 330s（>300s，给巡检留余量）。
    pub wait_timeout: Option<Duration>,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            ask_timeout_ms: APPROVAL_TIMEOUT_MS,
            sweep_tick: Duration::from_secs(1),
            wait_timeout: Some(Duration::from_secs(330)),
        }
    }
}

/// 权限请求（M2-10 从 `permission.request` 通知映射）。
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    /// 适配器回环键（`permission.request.requestId`）。
    pub request_id: String,
    pub session_id: Option<SessionId>,
    pub runtime_id: Option<RuntimeId>,
    /// 线协议 `resource`（`fs.read` / `fs.write` / `exec` / `net`）。
    pub resource: String,
    pub action: String,
    /// 原始 target（未规范化）。
    pub target: Option<String>,
    /// 写入内容大小（记忆白名单 1MB 上限）。
    pub content_bytes: Option<u64>,
}

/// 决议结果（回传适配器；`decision` 仅 allow/deny）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionResolution {
    pub request_id: String,
    pub decision: PermissionDecision,
    pub scope: Option<PermissionScope>,
    pub reason: String,
    /// 是否由 300s 超时路径判 deny。
    pub timed_out: bool,
}

impl PermissionResolution {
    pub fn is_allowed(&self) -> bool {
        self.decision == PermissionDecision::Allow
    }
}

/// 权限服务错误（稳定错误码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionError {
    /// `resource` 不在 D9 资源清单内。
    InvalidResource {
        resource: String,
    },
    /// 决议目标不在待审批队列（已决议/已超时/不存在）。
    NotPending {
        request_id: String,
    },
    /// 会话不存在（`permissions.session_id` FK）。
    SessionNotFound {
        session_id: SessionId,
    },
    Storage {
        code: String,
        message: String,
    },
    Pipeline {
        code: String,
        message: String,
    },
    Internal {
        reason: String,
    },
}

impl PermissionError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidResource { .. } => "invalid_permission_resource",
            Self::NotPending { .. } => "permission_not_pending",
            Self::SessionNotFound { .. } => "session_not_found",
            Self::Storage { .. } => "storage_error",
            Self::Pipeline { .. } => "pipeline_error",
            Self::Internal { .. } => "internal",
        }
    }
}

impl std::fmt::Display for PermissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidResource { resource } => {
                write!(f, "权限资源不在 D9 清单内：{resource}")
            }
            Self::NotPending { request_id } => {
                write!(
                    f,
                    "审批请求不在待决议队列（已决议/已超时/不存在）：{request_id}"
                )
            }
            Self::SessionNotFound { session_id } => write!(f, "会话不存在：{session_id}"),
            Self::Storage { code, message } => write!(f, "存储错误（{code}）：{message}"),
            Self::Pipeline { code, message } => write!(f, "事件管线错误（{code}）：{message}"),
            Self::Internal { reason } => write!(f, "权限服务内部错误：{reason}"),
        }
    }
}

impl std::error::Error for PermissionError {}

impl From<StoreError> for PermissionError {
    fn from(error: StoreError) -> Self {
        Self::Storage {
            code: error.code().to_owned(),
            message: error.to_string(),
        }
    }
}

impl From<PipelineError> for PermissionError {
    fn from(error: PipelineError) -> Self {
        Self::Pipeline {
            code: error.code().to_owned(),
            message: error.to_string(),
        }
    }
}

/// 会话授权键（D9：`session` 作用域仅限具体 target）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GrantKey {
    session_id: String,
    resource: String,
    action: String,
    canonical_target: String,
}

#[derive(Default)]
struct PermissionInner {
    /// 待审批台账（启动时从 `permissions` 表恢复）。
    queue: ApprovalQueue,
    /// 等待者（每请求独立 oneshot；核心内等待）。
    waiters: HashMap<String, oneshot::Sender<PermissionResolution>>,
    /// 会话级授权（`session` 决议；进程内有效，重启失效——会话为活跃概念）。
    grants: HashSet<GrantKey>,
}

struct ServiceInner {
    config: PermissionConfig,
    clock: SharedClock,
    policy: PolicyEngine,
    write: WriteQueue,
    reads: ReadPool,
    pipeline: EventPipeline,
    inner: Mutex<PermissionInner>,
    background: Mutex<Vec<JoinHandle<()>>>,
}

/// 权限服务（克隆共享同一实例）。
#[derive(Clone)]
pub struct PermissionService {
    inner: Arc<ServiceInner>,
}

impl PermissionService {
    pub fn new(
        config: PermissionConfig,
        clock: SharedClock,
        policy: PolicyEngine,
        write: WriteQueue,
        reads: ReadPool,
        pipeline: EventPipeline,
    ) -> Self {
        Self {
            inner: Arc::new(ServiceInner {
                config,
                clock,
                policy,
                write,
                reads,
                pipeline,
                inner: Mutex::new(PermissionInner::default()),
                background: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn config(&self) -> &PermissionConfig {
        &self.inner.config
    }

    pub fn workspace_root(&self) -> &std::path::Path {
        self.inner.policy.workspace_root()
    }

    /// 启动时恢复待审批（DoD3：核心重启后 pending 恢复）。
    ///
    /// 返回恢复的待审批数量；等待者天然不存在（原请求方已随核心退出），仅恢复台账
    /// 供巡检超时与 `permissions_pending` 查询。
    pub async fn restore_pending(&self) -> Result<usize, PermissionError> {
        let pending = self.inner.reads.permissions_pending(None).await?;
        let mut inner = lock_inner(&self.inner);
        let mut restored = 0usize;
        for record in pending {
            let ticket = ticket_from_record(&record);
            if inner.queue.insert(ticket) {
                restored += 1;
            }
        }
        Ok(restored)
    }

    /// 待审批清单（`permissions_pending` 命令语义；可按会话过滤）。
    pub fn pending_list(&self, session_id: Option<&SessionId>) -> Vec<ApprovalTicket> {
        let inner = lock_inner(&self.inner);
        match session_id {
            Some(session_id) => inner.queue.pending_for_session(session_id.as_str()),
            None => inner.queue.pending(),
        }
    }

    /// 请求一次工具调用权限（阻塞至决议/超时）。
    pub async fn request(
        &self,
        request: PermissionRequest,
    ) -> Result<PermissionResolution, PermissionError> {
        self.request_cancellable(request, None).await
    }

    /// 请求一次工具调用权限；等待可被取消树级联取消（M2-05：权限等待可取消）。
    ///
    /// `cancel` 命中（`interrupt`/`dispose`/父会话取消）时，待审批票据按 deny 收口
    /// （落库 `resolved` + `permission.resolved(deny)` 事件 + 审计 `permission.cancelled`），
    /// 返回 `timed_out=false` 的 deny 决议；决议与取消并发时以票据摘除为仲裁，不丢决议。
    pub async fn request_cancellable(
        &self,
        request: PermissionRequest,
        cancel: Option<&RunCancelToken>,
    ) -> Result<PermissionResolution, PermissionError> {
        let resource = PermissionResource::from_code(&request.resource).ok_or_else(|| {
            PermissionError::InvalidResource {
                resource: request.resource.clone(),
            }
        })?;
        let policy_request = PolicyRequest {
            resource,
            action: &request.action,
            target: request.target.as_deref(),
            content_bytes: request.content_bytes,
        };
        let verdict = self.inner.policy.evaluate(&policy_request);
        let requested_target = request.target.clone();
        let canonical_target = verdict
            .canonical_target
            .as_ref()
            .map(|path| path.display().to_string());

        match verdict.decision {
            PolicyDecision::Allow => {
                self.audit(
                    &request,
                    "system",
                    "permission.allowed_by_policy",
                    "allow",
                    Some(verdict.reason.clone()),
                )
                .await?;
                return Ok(PermissionResolution {
                    request_id: request.request_id,
                    decision: PermissionDecision::Allow,
                    scope: None,
                    reason: verdict.reason,
                    timed_out: false,
                });
            }
            PolicyDecision::Deny => {
                self.audit(
                    &request,
                    "system",
                    "permission.denied_by_policy",
                    "deny",
                    Some(verdict.reason.clone()),
                )
                .await?;
                return Ok(PermissionResolution {
                    request_id: request.request_id,
                    decision: PermissionDecision::Deny,
                    scope: None,
                    reason: verdict.reason,
                    timed_out: false,
                });
            }
            PolicyDecision::Ask => {}
        }

        // 会话级授权命中（D9：session 作用域仅限具体 target）。
        if let (Some(session_id), Some(canonical)) =
            (request.session_id.as_ref(), &canonical_target)
        {
            let key = GrantKey {
                session_id: session_id.as_str().to_owned(),
                resource: request.resource.clone(),
                action: request.action.clone(),
                canonical_target: canonical.clone(),
            };
            if lock_inner(&self.inner).grants.contains(&key) {
                let reason = format!("会话级授权命中（session 作用域，target={canonical}）");
                self.audit(
                    &request,
                    "system",
                    "permission.session_grant_hit",
                    "allow",
                    Some(reason.clone()),
                )
                .await?;
                return Ok(PermissionResolution {
                    request_id: request.request_id,
                    decision: PermissionDecision::Allow,
                    scope: Some(PermissionScope::Session),
                    reason,
                    timed_out: false,
                });
            }
        }

        // ask：入队 + 持久化 + permission.requested，然后等待。
        let now = self.inner.clock.now_ms();
        let id = ulid::generate();
        let ticket = ApprovalTicket {
            id: id.clone(),
            request_id: request.request_id.clone(),
            session_id: request.session_id.as_ref().map(|id| id.as_str().to_owned()),
            resource: request.resource.clone(),
            action: request.action.clone(),
            target: requested_target.clone(),
            canonical_target: canonical_target.clone(),
            requested_at: now,
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut inner = lock_inner(&self.inner);
            if !inner.queue.insert(ticket.clone()) {
                return Err(PermissionError::Internal {
                    reason: format!("审批票据 id 冲突：{id}"),
                });
            }
            inner.waiters.insert(id.clone(), tx);
        }
        self.persist_permission(
            &id,
            &request,
            PermissionDecision::Ask,
            PermissionStatus::Pending,
            None,
            None,
        )
        .await?;
        if let (Some(session_id), Some(runtime_id)) =
            (request.session_id.as_ref(), request.runtime_id.as_ref())
        {
            self.emit(
                session_id,
                runtime_id,
                "permission.requested",
                json!({
                    "request_id": request.request_id,
                    "resource": request.resource,
                    "action": request.action,
                    "target": requested_target,
                }),
            )
            .await?;
        }
        self.audit(
            &request,
            "agent",
            "permission.requested",
            "pending",
            Some(verdict.reason.clone()),
        )
        .await?;

        let resolution = self.wait_for_resolution(&id, rx, cancel).await?;
        Ok(resolution)
    }

    /// 等待审批决议（`permission.request` 等待段；M2-05 权限等待可取消）。
    ///
    /// 三路等待：决议（oneshot）/ 取消树级联（[`RunCancelToken`]）/ 兜底超时；
    /// 取消与决议并发时以「票据摘除」为仲裁——取消方摘到票据则按 deny 收口，
    /// 否则等待并返回实际决议（不丢决议、不死锁）。
    async fn wait_for_resolution(
        &self,
        id: &str,
        mut rx: oneshot::Receiver<PermissionResolution>,
        cancel: Option<&RunCancelToken>,
    ) -> Result<PermissionResolution, PermissionError> {
        if let Some(token) = cancel {
            if token.is_cancelled() {
                if let Some(resolution) = self.try_cancel_ticket(id).await? {
                    return Ok(resolution);
                }
            }
        }
        let wait_timeout = self.inner.config.wait_timeout;
        let cancelled = async {
            match cancel {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        let timeout = async {
            match wait_timeout {
                Some(limit) => tokio::time::sleep(limit).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(cancelled, timeout);
        tokio::select! {
            result = &mut rx => match result {
                Ok(resolution) => Ok(resolution),
                Err(_) => Err(PermissionError::Internal {
                    reason: "等待者通道被丢弃".to_owned(),
                }),
            },
            () = &mut cancelled => match self.try_cancel_ticket(id).await? {
                Some(resolution) => Ok(resolution),
                // 取消与决议并发：票据已被决议路径摘除 → 等待实际决议（不丢）。
                None => match rx.await {
                    Ok(resolution) => Ok(resolution),
                    Err(_) => Err(PermissionError::Internal {
                        reason: "等待者通道被丢弃".to_owned(),
                    }),
                },
            },
            () = &mut timeout => self.timeout_ticket(id).await,
        }
    }

    /// 用户决议（UI `permission.resolve` 语义）：`once` / `session` 授权，`deny` 拒绝。
    pub async fn resolve(
        &self,
        request_id: &str,
        decision: PermissionDecision,
        scope: Option<PermissionScope>,
    ) -> Result<ApprovalTicket, PermissionError> {
        let (ticket, waiter) = {
            let mut inner = lock_inner(&self.inner);
            let ticket = inner
                .queue
                .pending()
                .into_iter()
                .find(|ticket| ticket.request_id == request_id)
                .ok_or_else(|| PermissionError::NotPending {
                    request_id: request_id.to_owned(),
                })?;
            inner.queue.remove(&ticket.id);
            let waiter = inner.waiters.remove(&ticket.id);
            (ticket, waiter)
        };

        let resolved_at = self.inner.clock.now_ms();
        let (out_decision, out_scope) = match decision {
            PermissionDecision::Ask => (PermissionDecision::Deny, None),
            PermissionDecision::Allow => {
                let scope = scope.filter(|scope| *scope != PermissionScope::Always);
                (PermissionDecision::Allow, scope)
            }
            PermissionDecision::Deny => (PermissionDecision::Deny, None),
        };
        let status = PermissionStatus::Resolved;
        let affected = self
            .inner
            .write
            .execute(StoreCommand::ResolvePermission {
                id: ticket.id.clone(),
                decision: out_decision,
                scope: out_scope,
                status,
                resolved_at,
                resolver: Some("user".to_owned()),
            })
            .await?;
        if let StoreOutcome::Applied { affected } = affected {
            if affected == 0 {
                return Err(PermissionError::NotPending {
                    request_id: request_id.to_owned(),
                });
            }
        }

        if let (Some(session_id), Some(runtime_id)) = (
            ticket.session_id.as_ref(),
            self.runtime_id_of(&ticket).await,
        ) {
            if let Ok(session_id) = SessionId::new(session_id.clone()) {
                self.emit(
                    &session_id,
                    &runtime_id,
                    "permission.resolved",
                    json!({
                        "request_id": ticket.request_id,
                        "decision": out_decision.as_str(),
                        "scope": out_scope.map(|scope| scope.as_str()),
                    }),
                )
                .await?;
            }
        }
        self.insert_audit(
            ticket.session_id.as_deref(),
            "user",
            "permission.resolved",
            &format!("{}:{}", ticket.resource, ticket.action),
            "resolved",
            Some(
                json!({
                    "request_id": ticket.request_id,
                    "decision": out_decision.as_str(),
                    "scope": out_scope.map(|scope| scope.as_str()),
                })
                .to_string(),
            ),
        )
        .await?;

        if out_decision == PermissionDecision::Allow && out_scope == Some(PermissionScope::Session)
        {
            if let (Some(session_id), Some(canonical)) =
                (ticket.session_id.as_ref(), ticket.canonical_target.as_ref())
            {
                lock_inner(&self.inner).grants.insert(GrantKey {
                    session_id: session_id.clone(),
                    resource: ticket.resource.clone(),
                    action: ticket.action.clone(),
                    canonical_target: canonical.clone(),
                });
            }
        }

        let resolution = PermissionResolution {
            request_id: ticket.request_id.clone(),
            decision: out_decision,
            scope: out_scope,
            reason: "用户决议".to_owned(),
            timed_out: false,
        };
        // 唤醒等待者（若仍在本核心内等待）。
        if let Some(waiter) = waiter {
            let _ = waiter.send(resolution);
        }
        Ok(ticket)
    }

    /// 超时巡检单次执行：摘除超时票据 → `status=timeout` + `decision=deny` + 审计 +
    /// `permission.resolved`（decision=deny）→ 唤醒等待者（deny, timed_out）。
    pub async fn sweep_timeouts_once(&self) -> Vec<String> {
        let now = self.inner.clock.now_ms();
        let expired: Vec<ApprovalTicket> = {
            let mut inner = lock_inner(&self.inner);
            inner.queue.expire(now)
        };
        let mut timed_out = Vec::new();
        for ticket in expired {
            let waiter = {
                let mut inner = lock_inner(&self.inner);
                inner.waiters.remove(&ticket.id)
            };
            if self.finalize_timeout(&ticket, waiter).await.is_ok() {
                timed_out.push(ticket.request_id);
            }
        }
        timed_out
    }

    /// 启动后台巡检（超时 deny；默认 1s 周期）。
    pub fn spawn_background(&self, handle: &Handle) -> usize {
        let inner = Arc::clone(&self.inner);
        let task = handle.spawn(async move {
            loop {
                let tick = inner.config.sweep_tick;
                tokio::time::sleep(tick).await;
                let service = PermissionService {
                    inner: Arc::clone(&inner),
                };
                let _ = service.sweep_timeouts_once().await;
            }
        });
        let mut background = lock_background(&self.inner);
        background.push(task);
        background.len()
    }

    pub async fn shutdown_background(&self) {
        let handles: Vec<JoinHandle<()>> = {
            let mut background = lock_background(&self.inner);
            background.drain(..).collect()
        };
        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
    }

    // ===== 内部 =====

    async fn timeout_ticket(&self, id: &str) -> Result<PermissionResolution, PermissionError> {
        let (ticket, waiter) = {
            let mut inner = lock_inner(&self.inner);
            let ticket = inner
                .queue
                .remove(id)
                .ok_or_else(|| PermissionError::NotPending {
                    request_id: id.to_owned(),
                })?;
            let waiter = inner.waiters.remove(&ticket.id);
            (ticket, waiter)
        };
        self.finalize_timeout(&ticket, waiter).await
    }

    /// 取消树级联（M2-05）：摘除待审批票据并按 deny 收口；票据已被决议/超时摘除时
    /// 返回 `None`（调用方回落到实际决议）。
    async fn try_cancel_ticket(
        &self,
        id: &str,
    ) -> Result<Option<PermissionResolution>, PermissionError> {
        let (ticket, waiter) = {
            let mut inner = lock_inner(&self.inner);
            let Some(ticket) = inner.queue.remove(id) else {
                return Ok(None);
            };
            (ticket, inner.waiters.remove(id))
        };
        self.finalize_cancelled(&ticket, waiter).await.map(Some)
    }

    /// 取消收口：落库 deny（`resolved`）+ `permission.resolved(deny)` 事件 +
    /// 审计 `permission.cancelled` + 唤醒等待者（deny，`timed_out=false`）。
    async fn finalize_cancelled(
        &self,
        ticket: &ApprovalTicket,
        waiter: Option<oneshot::Sender<PermissionResolution>>,
    ) -> Result<PermissionResolution, PermissionError> {
        let resolved_at = self.inner.clock.now_ms();
        let affected = self
            .inner
            .write
            .execute(StoreCommand::ResolvePermission {
                id: ticket.id.clone(),
                decision: PermissionDecision::Deny,
                scope: None,
                status: PermissionStatus::Resolved,
                resolved_at,
                resolver: Some("system".to_owned()),
            })
            .await?;
        if let StoreOutcome::Applied { affected } = affected {
            if affected == 0 {
                return Err(PermissionError::NotPending {
                    request_id: ticket.request_id.clone(),
                });
            }
        }
        if let (Some(session_id), Some(runtime_id)) =
            (ticket.session_id.as_ref(), self.runtime_id_of(ticket).await)
        {
            if let Ok(session_id) = SessionId::new(session_id.clone()) {
                self.emit(
                    &session_id,
                    &runtime_id,
                    "permission.resolved",
                    json!({
                        "request_id": ticket.request_id,
                        "decision": "deny",
                        "scope": Value::Null,
                    }),
                )
                .await?;
            }
        }
        self.insert_audit(
            ticket.session_id.as_deref(),
            "system",
            "permission.cancelled",
            &format!("{}:{}", ticket.resource, ticket.action),
            "cancelled",
            Some(
                json!({
                    "request_id": ticket.request_id,
                    "reason": "取消树级联（interrupt/dispose）",
                })
                .to_string(),
            ),
        )
        .await?;

        let resolution = PermissionResolution {
            request_id: ticket.request_id.clone(),
            decision: PermissionDecision::Deny,
            scope: None,
            reason: "会话取消（取消树级联）→ deny（已审计）".to_owned(),
            timed_out: false,
        };
        if let Some(waiter) = waiter {
            let _ = waiter.send(resolution.clone());
        }
        Ok(resolution)
    }

    /// 超时落库 + 审计 + 事件 + 唤醒等待者（票据须已从队列摘除）。
    async fn finalize_timeout(
        &self,
        ticket: &ApprovalTicket,
        waiter: Option<oneshot::Sender<PermissionResolution>>,
    ) -> Result<PermissionResolution, PermissionError> {
        let resolved_at = self.inner.clock.now_ms();
        let affected = self
            .inner
            .write
            .execute(StoreCommand::TimeoutPermission {
                id: ticket.id.clone(),
                resolved_at,
            })
            .await?;
        if let StoreOutcome::Applied { affected } = affected {
            if affected == 0 {
                return Err(PermissionError::NotPending {
                    request_id: ticket.request_id.clone(),
                });
            }
        }
        if let (Some(session_id), Some(runtime_id)) =
            (ticket.session_id.as_ref(), self.runtime_id_of(ticket).await)
        {
            if let Ok(session_id) = SessionId::new(session_id.clone()) {
                self.emit(
                    &session_id,
                    &runtime_id,
                    "permission.resolved",
                    json!({
                        "request_id": ticket.request_id,
                        "decision": "deny",
                        "scope": Value::Null,
                    }),
                )
                .await?;
            }
        }
        self.insert_audit(
            ticket.session_id.as_deref(),
            "system",
            "permission.timeout",
            &format!("{}:{}", ticket.resource, ticket.action),
            "timeout",
            Some(
                json!({
                    "request_id": ticket.request_id,
                    "timeout_ms": self.inner.config.ask_timeout_ms,
                })
                .to_string(),
            ),
        )
        .await?;

        let resolution = PermissionResolution {
            request_id: ticket.request_id.clone(),
            decision: PermissionDecision::Deny,
            scope: None,
            reason: format!(
                "审批超时 {}ms → deny（D9；已审计）",
                self.inner.config.ask_timeout_ms
            ),
            timed_out: true,
        };
        if let Some(waiter) = waiter {
            let _ = waiter.send(resolution.clone());
        }
        Ok(resolution)
    }

    /// 会话 runtime（事件信封 `runtime_id`）：读 `sessions.runtime_id`。
    async fn runtime_id_of(&self, ticket: &ApprovalTicket) -> Option<RuntimeId> {
        let session_id = ticket.session_id.as_ref()?;
        let session_id = SessionId::new(session_id.clone()).ok()?;
        self.inner
            .reads
            .session(&session_id)
            .await
            .ok()
            .flatten()
            .map(|session| session.runtime_id)
    }

    async fn persist_permission(
        &self,
        id: &str,
        request: &PermissionRequest,
        decision: PermissionDecision,
        status: PermissionStatus,
        scope: Option<PermissionScope>,
        resolved_at: Option<i64>,
    ) -> Result<(), PermissionError> {
        let record = PermissionRecord {
            id: id.to_owned(),
            session_id: request.session_id.as_ref().map(|id| id.as_str().to_owned()),
            request_id: Some(request.request_id.clone()),
            resource: request.resource.clone(),
            action: request.action.clone(),
            target: request.target.clone(),
            decision,
            scope,
            status,
            requested_at: self.inner.clock.now_ms(),
            resolved_at,
            resolver: None,
        };
        self.inner
            .write
            .execute(StoreCommand::InsertPermission { record })
            .await?;
        Ok(())
    }

    async fn audit(
        &self,
        request: &PermissionRequest,
        actor: &str,
        action: &str,
        result: &str,
        detail: Option<String>,
    ) -> Result<(), PermissionError> {
        self.insert_audit(
            request.session_id.as_ref().map(|id| id.as_str()),
            actor,
            action,
            &format!("{}:{}", request.resource, request.action),
            result,
            Some(
                json!({
                    "request_id": request.request_id,
                    "target": request.target,
                    "detail": detail,
                    "runtime_id": request.runtime_id.as_ref().map(|id| id.as_str()),
                })
                .to_string(),
            ),
        )
        .await
    }

    async fn insert_audit(
        &self,
        session_id: Option<&str>,
        actor: &str,
        action: &str,
        resource: &str,
        result: &str,
        detail: Option<String>,
    ) -> Result<(), PermissionError> {
        let record = AuditLogRecord {
            id: ulid::generate(),
            session_id: session_id.map(str::to_owned),
            runtime_id: None,
            actor: actor.to_owned(),
            action: action.to_owned(),
            resource: Some(resource.to_owned()),
            detail,
            result: Some(result.to_owned()),
            ts: self.inner.clock.now_ms(),
        };
        self.inner
            .write
            .execute(StoreCommand::InsertAudit { record })
            .await?;
        Ok(())
    }

    async fn emit(
        &self,
        session_id: &SessionId,
        runtime_id: &RuntimeId,
        event_type: &str,
        payload: Value,
    ) -> Result<(), PermissionError> {
        let mut object = serde_json::Map::with_capacity(ENVELOPE_FIELDS.len());
        object.insert("v".to_owned(), Value::from(EVENT_ENVELOPE_VERSION));
        object.insert("id".to_owned(), Value::from(ulid::generate()));
        object.insert("session_id".to_owned(), Value::from(session_id.as_str()));
        object.insert("run_id".to_owned(), Value::Null);
        object.insert("runtime_id".to_owned(), Value::from(runtime_id.as_str()));
        object.insert("seq".to_owned(), Value::from(0_u64));
        object.insert("ts".to_owned(), Value::from(self.inner.clock.now_ms()));
        object.insert("type".to_owned(), Value::from(event_type));
        object.insert("payload".to_owned(), payload);
        match self
            .inner
            .pipeline
            .submit(Value::Object(object))
            .await
            .map_err(PermissionError::from)?
        {
            SubmitOutcome::Persisted { .. } | SubmitOutcome::Buffered => Ok(()),
            SubmitOutcome::Duplicate { .. } => Ok(()),
            SubmitOutcome::DeadLettered { code, reason } => Err(PermissionError::Internal {
                reason: format!("权限事件被死信（{code}）：{reason}"),
            }),
        }
    }
}

fn ticket_from_record(record: &PermissionRecord) -> ApprovalTicket {
    ApprovalTicket {
        id: record.id.clone(),
        request_id: record
            .request_id
            .clone()
            .unwrap_or_else(|| record.id.clone()),
        session_id: record.session_id.clone(),
        resource: record.resource.clone(),
        action: record.action.clone(),
        target: record.target.clone(),
        canonical_target: None,
        requested_at: record.requested_at,
    }
}

fn lock_inner(inner: &Arc<ServiceInner>) -> std::sync::MutexGuard<'_, PermissionInner> {
    match inner.inner.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_background(inner: &Arc<ServiceInner>) -> std::sync::MutexGuard<'_, Vec<JoinHandle<()>>> {
    match inner.background.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 路径违规 → 审计错误码（M2-10 证据字段）。
pub const fn path_violation_code(violation: &PathViolation) -> &'static str {
    violation.code()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_d9() {
        let config = PermissionConfig::default();
        assert_eq!(config.ask_timeout_ms, 300_000, "D9：300s");
        assert_eq!(config.ask_timeout_ms, APPROVAL_TIMEOUT_MS);
        assert_eq!(config.sweep_tick, Duration::from_secs(1));
        assert!(config.wait_timeout.is_some());
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(
            PermissionError::InvalidResource {
                resource: "x".to_owned()
            }
            .code(),
            "invalid_permission_resource"
        );
        assert_eq!(
            PermissionError::NotPending {
                request_id: "r".to_owned()
            }
            .code(),
            "permission_not_pending"
        );
    }
}
