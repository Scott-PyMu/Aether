//! 权限网关（M2-03；设计 D9）——策略矩阵、路径校验、审批队列。
//!
//! 边界口径（AGENTS §2.7 / D9 评审修订 #1）：仅约束适配器经线协议上报的工具调用
//! （`permission.request`）；适配器进程内行为不经此门，属信任级已知边界。
//!
//! 本模块为**纯决策层**（无 I/O、无存储）：持久化、审计、超时巡检与事件广播由
//! `aether-control` 的权限服务承接（依赖方向：security 仅依赖 core，D2/AGENTS §8）。

pub mod approval;
pub mod path;
pub mod policy;

pub use approval::{ApprovalQueue, ApprovalTicket, APPROVAL_TIMEOUT_MS};
pub use path::{
    case_insensitive_platform, expand_t7_sample, PathGuard, PathGuardError, PathViolation,
    T7_TEXTUAL_SAMPLES,
};
pub use policy::{
    PermissionResource, PolicyDecision, PolicyEngine, PolicyRequest, PolicyVerdict,
    MEMORY_FILE_MAX_BYTES, MEMORY_FILE_NAMES,
};
