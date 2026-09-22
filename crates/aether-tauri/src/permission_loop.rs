//! 权限回环的核心侧网关接线（M2-10；设计 D9/D6）。
//!
//! [`PermissionServiceGate`] 把 [`aether_adapters::permission_loop::PermissionGate`] 薄适配到
//! [`aether_control::PermissionService`]（组合根职责；服务语义与错误码不改写、不放宽）：
//!
//! - 请求映射：`PermissionLoopRequest` → [`aether_control::PermissionRequest`]
//!   （`resource` / `action` / `target` / `content_bytes` / `session_id` / `runtime_id` 原样）；
//! - 决议映射：[`aether_control::PermissionResolution`] → `PermissionLoopDecision`
//!   （`decision` / `scope` / `reason` / `timed_out` 原样；`Ask` 不会出现在决议中）；
//! - 网关错误（非法资源/存储错误/管线错误）以稳定错误码字符串回传回环探针，
//!   由 `permission_loop` 记为 `gate_failures`（零直通断言随之失败，不静默吞错）。
//!
//! 边界（D9 评审修订 #1 / AGENTS §2.7）：本回环**仅约束适配器经线协议上报的工具调用**；
//! 适配器进程内行为不经此门。

use std::sync::Arc;

use aether_adapters::permission_loop::{
    PermissionGate, PermissionGateFuture, PermissionLoopDecision, PermissionLoopRequest,
};
use aether_control::{PermissionRequest, PermissionService};
use aether_core::{RuntimeId, SessionId};

/// 生产网关：`PermissionService`（策略矩阵 / 审批持久化 / 300s 超时 / 审计）。
pub struct PermissionServiceGate {
    service: PermissionService,
}

impl PermissionServiceGate {
    /// 以核心权限服务构造网关。
    pub fn new(service: PermissionService) -> Arc<Self> {
        Arc::new(Self { service })
    }

    /// 底层权限服务（UI 决议 / pending 清单复用）。
    pub fn service(&self) -> &PermissionService {
        &self.service
    }
}

impl PermissionGate for PermissionServiceGate {
    fn decide(&self, request: PermissionLoopRequest) -> PermissionGateFuture<'_> {
        Box::pin(async move {
            let session_id = request
                .session_id
                .as_deref()
                .and_then(|value| SessionId::new(value).ok());
            let runtime_id = request
                .runtime_id
                .as_deref()
                .and_then(|value| RuntimeId::new(value).ok());
            let control_request = PermissionRequest {
                request_id: request.request_id.clone(),
                session_id,
                runtime_id,
                resource: request.resource.clone(),
                action: request.action.clone(),
                target: request.target.clone(),
                content_bytes: request.content_bytes,
            };
            match self.service.request(control_request).await {
                Ok(resolution) => Ok(PermissionLoopDecision {
                    decision: resolution.decision,
                    scope: resolution.scope,
                    reason: resolution.reason,
                    timed_out: resolution.timed_out,
                }),
                Err(error) => Err(format!("{}: {error}", error.code())),
            }
        })
    }
}
