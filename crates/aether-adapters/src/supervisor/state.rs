//! D5 监督状态机：`cold → starting → ready → degraded → disabled`（评审修订 #3）。
//!
//! 硬约束：
//! - 状态取值与 `runtimes.status` CHECK 枚举**一一对应，不做映射**（D5，`RuntimeStatus` 即
//!   `aether-core::domain::RuntimeStatus`）；
//! - 每次合法转移产出一条 [`StatusChange`]（即 `runtime.status_changed` 事件的载荷来源），
//!   由监督器交给 [`SupervisorObserver`] 广播；
//! - `status_reason` 只允许取自 [`DisabledReason`] 词典；`disabled` 必带原因（D5）；
//! - 非法转移一律拒绝（[`TransitionError`]），不修改现状。
//!
//! 恢复转移（ADR-004/评审 #7）：
//! - `degraded → ready`：原因解除后经监督器自动重启（`degraded → starting → ready`），
//!   或健康探测通过时直接恢复；
//! - `disabled → cold/starting`：仅经人工动作（`runtime_retry` / `runtime_enable` /
//!   修复后重启应用）触发；`untrusted` / `version_mismatch` 必须先修复。

use std::time::{SystemTime, UNIX_EPOCH};

use aether_core::{EventType, RuntimeId, RuntimeStatus, RuntimeStatusChangedPayload};

use crate::protocol::DisabledReason;

/// 监督器观察者：状态转移广播、审计、资源告警的统一出口。
///
/// M1-10 不引入事件管线依赖（`aether-adapters` 仅依赖 `aether-core`）；实现方
/// （M2-01 / aether-tauri 命令层）负责把 [`StatusChange`] 转成 `runtime.status_changed`
/// 信封并按 D4「先日志后广播」写入。
pub trait SupervisorObserver: Send + Sync + 'static {
    /// 每次合法状态转移调用一次（D5：广播 `runtime.status_changed`）。
    fn on_status_changed(&self, _change: &StatusChange) {}

    /// 审计记录（准入拒绝、台账处置、熔断等）。
    fn on_audit(&self, _record: &AuditRecord) {}

    /// 资源告警（RSS >1GB 或 CPU >200% 持续 60s；D5：只告警不自动杀）。
    fn on_resource_alert(&self, _alert: &ResourceEvent) {}
}

/// 空观察者（不需要广播时的默认实现）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopObserver;

impl SupervisorObserver for NoopObserver {}

/// 一次状态转移（`runtime.status_changed` 的来源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusChange {
    pub runtime_id: RuntimeId,
    pub from: RuntimeStatus,
    pub to: RuntimeStatus,
    /// 转移原因；仅 `disabled`/`degraded` 允许携带，`ready`/`starting`/`cold` 必为 `None`。
    pub reason: Option<DisabledReason>,
    /// 诊断细节（不进入事件载荷的正式字段）。
    pub detail: Option<String>,
    pub at_ms: i64,
}

impl StatusChange {
    /// 事件类型（附录 B：`runtime.status_changed`）。
    pub const fn event_type(&self) -> EventType {
        EventType::RuntimeStatusChanged
    }

    /// 附录 B 事件载荷（`runtimeId` / `from` / `to` / `reason`）。
    pub fn payload(&self) -> RuntimeStatusChangedPayload {
        RuntimeStatusChangedPayload {
            runtime_id: self.runtime_id.clone(),
            from: self.from,
            to: self.to,
            reason: self.reason.map(|reason| reason.as_str().to_owned()),
        }
    }
}

/// 审计类别（D5 失败表：准入拒绝、台账处置、熔断等）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditKind {
    /// 非官方 manifest 拒绝加载（评审 #1）。
    AdmissionRejected,
    /// 台账三条件不满足 → 仅记录，不 kill（评审 #5）。
    LedgerSkipped,
    /// 台账三条件全命中 → 整树清理（评审 #5）。
    LedgerReclaimed,
    /// PID 已不存在 → 清理陈旧台账记录。
    LedgerStaleRecord,
    /// 60s 内 ≥5 次崩溃 → 熔断 `crash_loop`（D5）。
    CircuitBroken,
    /// 终止序列完成（含逐步机制与耗时）。
    Terminated,
}

impl AuditKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AdmissionRejected => "admission_rejected",
            Self::LedgerSkipped => "ledger_skipped",
            Self::LedgerReclaimed => "ledger_reclaimed",
            Self::LedgerStaleRecord => "ledger_stale_record",
            Self::CircuitBroken => "circuit_broken",
            Self::Terminated => "terminated",
        }
    }
}

/// 审计记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub at_ms: i64,
    pub runtime_id: RuntimeId,
    pub kind: AuditKind,
    pub detail: String,
}

impl AuditRecord {
    pub fn new(
        runtime_id: RuntimeId,
        kind: AuditKind,
        detail: impl Into<String>,
        at_ms: i64,
    ) -> Self {
        Self {
            at_ms,
            runtime_id,
            kind,
            detail: detail.into(),
        }
    }

    pub fn kind(&self) -> AuditKind {
        self.kind
    }

    pub fn runtime_id(&self) -> &RuntimeId {
        &self.runtime_id
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// 资源告警事件（`error` 事件语义由 M2-01 接入；本层只上报观察者）。
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceEvent {
    pub runtime_id: RuntimeId,
    pub pid: u32,
    pub rss_bytes: u64,
    pub cpu_percent: f32,
    /// 超限持续时长（毫秒）。
    pub sustained_ms: u64,
    pub limit: ResourceLimitKind,
}

/// 触发的资源上限类别（D5：RSS >1GB 或 CPU >200% 持续 60s → 告警，不自动杀）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceLimitKind {
    Rss,
    Cpu,
}

/// 非法状态转移（拒绝且不修改现状）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    #[error("非法状态转移：{from} → {to}（D5 状态机）")]
    Illegal {
        from: RuntimeStatus,
        to: RuntimeStatus,
    },
    #[error("转到 disabled 必须携带 status_reason（D5）")]
    MissingDisabledReason,
    #[error("转到 ready 必须清除 status_reason（D5）")]
    StaleReason,
}

/// 当前毫秒级 epoch（时钟异常时返回 0 并保持单调可用——仅诊断用途）。
pub fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// D5 合法转移表（`cold → starting → ready → degraded → disabled` + 恢复转移）。
///
/// - `disabled → cold`：`runtime_retry` / `runtime_enable` 的第一步（DoD7）；
/// - `disabled → starting`：兼容直达启动路径（仍受命令层状态校验约束）；
/// - `cold → disabled`：准入拒绝（非官方 manifest，评审 #1）不经过进程启动；
/// - `degraded → ready`：原因解除且健康探测通过（ADR-004）。
pub const fn transition_allowed(from: RuntimeStatus, to: RuntimeStatus) -> bool {
    use RuntimeStatus::{Cold, Degraded, Disabled, Ready, Starting};
    matches!(
        (from, to),
        (Cold, Starting)
            | (Cold, Disabled)
            | (Starting, Ready)
            | (Starting, Degraded)
            | (Starting, Disabled)
            | (Ready, Degraded)
            | (Ready, Disabled)
            | (Degraded, Starting)
            | (Degraded, Ready)
            | (Degraded, Disabled)
            | (Disabled, Cold)
            | (Disabled, Starting)
    )
}

/// 状态机核心（与 `runtimes.status` / `runtimes.status_reason` 一一对应）。
#[derive(Debug, Clone)]
pub struct StateCore {
    runtime_id: RuntimeId,
    status: RuntimeStatus,
    status_reason: Option<DisabledReason>,
}

impl StateCore {
    /// 初始状态恒为 `cold`（与 DDL 默认值一致）。
    pub fn new(runtime_id: RuntimeId) -> Self {
        Self {
            runtime_id,
            status: RuntimeStatus::Cold,
            status_reason: None,
        }
    }

    pub fn runtime_id(&self) -> &RuntimeId {
        &self.runtime_id
    }

    pub fn status(&self) -> RuntimeStatus {
        self.status
    }

    pub fn status_reason(&self) -> Option<DisabledReason> {
        self.status_reason
    }

    /// 是否允许转移到目标状态（不修改现状）。
    pub fn can_transition(&self, to: RuntimeStatus) -> bool {
        transition_allowed(self.status, to)
    }

    /// 执行一次转移；非法转移返回错误且不修改现状。
    pub fn transition(
        &mut self,
        to: RuntimeStatus,
        reason: Option<DisabledReason>,
        detail: Option<String>,
    ) -> Result<StatusChange, TransitionError> {
        let from = self.status;
        if !transition_allowed(from, to) {
            return Err(TransitionError::Illegal { from, to });
        }
        if to == RuntimeStatus::Disabled && reason.is_none() {
            return Err(TransitionError::MissingDisabledReason);
        }
        if to == RuntimeStatus::Ready && reason.is_some() {
            return Err(TransitionError::StaleReason);
        }
        let next_reason = match to {
            RuntimeStatus::Disabled | RuntimeStatus::Degraded => reason,
            RuntimeStatus::Cold | RuntimeStatus::Starting | RuntimeStatus::Ready => None,
        };
        self.status = to;
        self.status_reason = next_reason;
        Ok(StatusChange {
            runtime_id: self.runtime_id.clone(),
            from,
            to,
            reason: next_reason,
            detail,
            at_ms: now_ms(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core() -> StateCore {
        StateCore::new(RuntimeId::new("mock").unwrap())
    }

    #[test]
    fn runtime_status_enum_matches_db_enum_in_order() {
        // 断言来源：migrations/0001_init.sql runtimes.status CHECK 枚举。
        let db_enum = ["cold", "starting", "ready", "degraded", "disabled"];
        assert_eq!(RuntimeStatus::ALL.len(), db_enum.len());
        for (status, expected) in RuntimeStatus::ALL.into_iter().zip(db_enum) {
            assert_eq!(status.as_str(), expected);
        }
    }

    #[test]
    fn status_reason_dictionary_values_are_from_d5() {
        // D5「如」清单 + ADR-003/ADR-004 补充（无 CHECK 约束，取值只增不改语义）。
        let expected = [
            "handshake_timeout",
            "version_mismatch",
            "start_failed",
            "protocol_error",
            "crash_loop",
            "untrusted",
            "heartbeat_failed",
            "crashed",
            "storage_backpressure",
        ];
        let actual: Vec<&str> = DisabledReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn legal_transition_table_covers_d5_and_recovery_paths() {
        use RuntimeStatus::*;
        let legal = [
            (Cold, Starting),
            (Cold, Disabled),
            (Starting, Ready),
            (Starting, Degraded),
            (Starting, Disabled),
            (Ready, Degraded),
            (Ready, Disabled),
            (Degraded, Starting),
            (Degraded, Ready),
            (Degraded, Disabled),
            (Disabled, Cold),
            (Disabled, Starting),
        ];
        for from in RuntimeStatus::ALL {
            for to in RuntimeStatus::ALL {
                assert_eq!(
                    transition_allowed(from, to),
                    legal.contains(&(from, to)),
                    "{from} → {to}"
                );
            }
        }
    }

    #[test]
    fn happy_path_produces_one_change_per_transition() {
        let mut fsm = core();
        assert_eq!(fsm.status(), RuntimeStatus::Cold);
        let cold_to_starting = fsm
            .transition(RuntimeStatus::Starting, None, Some("warmup".to_owned()))
            .unwrap();
        assert_eq!(cold_to_starting.from, RuntimeStatus::Cold);
        assert_eq!(cold_to_starting.to, RuntimeStatus::Starting);
        assert_eq!(
            cold_to_starting.event_type(),
            EventType::RuntimeStatusChanged
        );
        assert_eq!(
            cold_to_starting.event_type().as_str(),
            "runtime.status_changed"
        );

        let starting_to_ready = fsm.transition(RuntimeStatus::Ready, None, None).unwrap();
        assert_eq!(starting_to_ready.from, RuntimeStatus::Starting);
        assert_eq!(starting_to_ready.to, RuntimeStatus::Ready);
        assert_eq!(starting_to_ready.reason, None);
        assert!(fsm.status_reason().is_none());

        let ready_to_degraded = fsm
            .transition(
                RuntimeStatus::Degraded,
                Some(DisabledReason::HeartbeatFailed),
                Some("3 次心跳失败".to_owned()),
            )
            .unwrap();
        assert_eq!(
            ready_to_degraded.reason,
            Some(DisabledReason::HeartbeatFailed)
        );
        assert_eq!(fsm.status_reason(), Some(DisabledReason::HeartbeatFailed));

        let degraded_to_disabled = fsm
            .transition(
                RuntimeStatus::Disabled,
                Some(DisabledReason::CrashLoop),
                None,
            )
            .unwrap();
        assert_eq!(degraded_to_disabled.reason, Some(DisabledReason::CrashLoop));
    }

    #[test]
    fn payload_matches_appendix_b_shape() {
        let mut fsm = core();
        fsm.transition(RuntimeStatus::Starting, None, None).unwrap();
        fsm.transition(RuntimeStatus::Ready, None, None).unwrap();
        let change = fsm
            .transition(
                RuntimeStatus::Degraded,
                Some(DisabledReason::StorageBackpressure),
                None,
            )
            .unwrap();
        let payload = change.payload();
        assert_eq!(payload.from, RuntimeStatus::Ready);
        assert_eq!(payload.to, RuntimeStatus::Degraded);
        assert_eq!(payload.reason.as_deref(), Some("storage_backpressure"));
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "runtime_id": "mock",
                "from": "ready",
                "to": "degraded",
                "reason": "storage_backpressure",
            })
        );
    }

    #[test]
    fn illegal_transition_is_rejected_without_mutation() {
        let mut fsm = core();
        // cold → ready 不在 D5 转移表内。
        let error = fsm
            .transition(RuntimeStatus::Ready, None, None)
            .unwrap_err();
        assert_eq!(
            error,
            TransitionError::Illegal {
                from: RuntimeStatus::Cold,
                to: RuntimeStatus::Ready,
            }
        );
        assert_eq!(fsm.status(), RuntimeStatus::Cold);

        // ready → cold 不允许。
        fsm.transition(RuntimeStatus::Starting, None, None).unwrap();
        fsm.transition(RuntimeStatus::Ready, None, None).unwrap();
        let error = fsm.transition(RuntimeStatus::Cold, None, None).unwrap_err();
        assert!(matches!(error, TransitionError::Illegal { .. }));
        assert_eq!(fsm.status(), RuntimeStatus::Ready);
    }

    #[test]
    fn disabled_requires_reason_and_ready_clears_reason() {
        let mut fsm = core();
        fsm.transition(RuntimeStatus::Starting, None, None).unwrap();
        let error = fsm
            .transition(RuntimeStatus::Disabled, None, None)
            .unwrap_err();
        assert_eq!(error, TransitionError::MissingDisabledReason);
        assert_eq!(fsm.status(), RuntimeStatus::Starting);

        fsm.transition(
            RuntimeStatus::Disabled,
            Some(DisabledReason::Untrusted),
            None,
        )
        .unwrap();
        assert_eq!(fsm.status_reason(), Some(DisabledReason::Untrusted));

        // disabled → cold 清除原因（人工动作第一步）。
        let change = fsm.transition(RuntimeStatus::Cold, None, None).unwrap();
        assert_eq!(change.to, RuntimeStatus::Cold);
        assert!(fsm.status_reason().is_none());

        // cold → starting → ready。
        fsm.transition(RuntimeStatus::Starting, None, None).unwrap();
        fsm.transition(RuntimeStatus::Ready, None, None).unwrap();
        // ready 不允许携带原因。
        let mut fsm2 = core();
        fsm2.transition(RuntimeStatus::Starting, None, None)
            .unwrap();
        let error = fsm2
            .transition(RuntimeStatus::Ready, Some(DisabledReason::Crashed), None)
            .unwrap_err();
        assert_eq!(error, TransitionError::StaleReason);
    }

    #[test]
    fn recovery_paths_are_disabled_to_cold_to_starting() {
        let mut fsm = core();
        fsm.transition(RuntimeStatus::Starting, None, None).unwrap();
        fsm.transition(
            RuntimeStatus::Disabled,
            Some(DisabledReason::StartFailed),
            None,
        )
        .unwrap();
        let first = fsm.transition(RuntimeStatus::Cold, None, None).unwrap();
        let second = fsm.transition(RuntimeStatus::Starting, None, None).unwrap();
        assert_eq!(first.from, RuntimeStatus::Disabled);
        assert_eq!(first.to, RuntimeStatus::Cold);
        assert_eq!(second.from, RuntimeStatus::Cold);
        assert_eq!(second.to, RuntimeStatus::Starting);
    }

    #[test]
    fn audit_record_accessors() {
        let record = AuditRecord::new(
            RuntimeId::new("nonofficial").unwrap(),
            AuditKind::AdmissionRejected,
            "manifest 未命中官方白名单",
            42,
        );
        assert_eq!(record.kind(), AuditKind::AdmissionRejected);
        assert_eq!(record.kind().as_str(), "admission_rejected");
        assert_eq!(record.runtime_id().as_str(), "nonofficial");
        assert_eq!(record.detail(), "manifest 未命中官方白名单");
    }
}
