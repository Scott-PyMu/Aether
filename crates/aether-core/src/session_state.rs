//! 会话生命周期状态机（M2-01；设计 D2/D8、附录 C `sessions.status`）。
//!
//! 状态取值与 `sessions.status` 的 CHECK 枚举一一对应，不做映射；转移表为白名单，
//! 未列出的转移一律拒绝且不修改现状（与 M1-10 运行时状态机同口径）。
//!
//! P0 实际产生的转移（实现与测试覆盖口径）：
//! - `creating → idle`（会话创建完成）；`creating → failed`（创建失败）；
//! - `idle → running`（新 run 准入）；`running → idle`（run 达到终态，会话可继续送消息）；
//! - `running → waiting_permission → running`（D9 审批等待，归属 M2-03/M2-10）；
//! - `running/idle → cancelled`（interrupt/dispose）；`idle → completed`（优雅关闭）。
//!
//! `paused` 为 P1 暂停/恢复预留（设计 §4#22：P0 无 pause/resume），转移表一并给出，
//! 便于 M2-05 扩展取消树与 P1 启用时保持模型完整。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::SessionStatus;

/// 会话状态转移（`session.status_changed` payload 的模型侧来源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatusChange {
    pub from: SessionStatus,
    pub to: SessionStatus,
}

/// 非法会话状态转移（拒绝且不修改现状）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTransitionError {
    Illegal {
        from: SessionStatus,
        to: SessionStatus,
    },
}

impl fmt::Display for SessionTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Illegal { from, to } => {
                write!(f, "非法会话状态转移：{from} → {to}（M2-01 状态机）")
            }
        }
    }
}

impl std::error::Error for SessionTransitionError {}

/// 合法转移表（白名单；与 D8 run 串行 / D9 审批等待语义一致）。
pub const fn session_transition_allowed(from: SessionStatus, to: SessionStatus) -> bool {
    use SessionStatus::*;
    matches!(
        (from, to),
        (Creating, Idle)
            | (Creating, Failed)
            | (Creating, Cancelled)
            | (Idle, Running)
            | (Idle, Completed)
            | (Idle, Failed)
            | (Idle, Cancelled)
            | (Running, Idle)
            | (Running, Paused)
            | (Running, WaitingPermission)
            | (Running, Completed)
            | (Running, Failed)
            | (Running, Cancelled)
            | (Paused, Running)
            | (Paused, Failed)
            | (Paused, Cancelled)
            | (WaitingPermission, Running)
            | (WaitingPermission, Failed)
            | (WaitingPermission, Cancelled)
    )
}

/// 终态判定（终态无出边；`completed` / `failed` / `cancelled`）。
pub const fn session_status_is_terminal(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled
    )
}

/// 会话状态机（单一事实来源：管理器只经本类型转移状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFsm {
    status: SessionStatus,
}

impl SessionFsm {
    /// 以初始状态构造（新会话为 `creating`）。
    pub const fn new(status: SessionStatus) -> Self {
        Self { status }
    }

    pub const fn status(self) -> SessionStatus {
        self.status
    }

    pub const fn is_terminal(self) -> bool {
        session_status_is_terminal(self.status)
    }

    /// 是否允许转移到目标状态（不修改现状）。
    pub const fn can_transition(self, to: SessionStatus) -> bool {
        session_transition_allowed(self.status, to)
    }

    /// 执行一次转移；非法转移返回错误且不修改现状。
    pub fn transition(
        &mut self,
        to: SessionStatus,
    ) -> Result<SessionStatusChange, SessionTransitionError> {
        let from = self.status;
        if !session_transition_allowed(from, to) {
            return Err(SessionTransitionError::Illegal { from, to });
        }
        self.status = to;
        Ok(SessionStatusChange { from, to })
    }
}

impl fmt::Display for SessionStatusChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} → {}", self.from, self.to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_table_covers_exactly_allowed_pairs() {
        use SessionStatus::*;
        let legal = [
            (Creating, Idle),
            (Creating, Failed),
            (Creating, Cancelled),
            (Idle, Running),
            (Idle, Completed),
            (Idle, Failed),
            (Idle, Cancelled),
            (Running, Idle),
            (Running, Paused),
            (Running, WaitingPermission),
            (Running, Completed),
            (Running, Failed),
            (Running, Cancelled),
            (Paused, Running),
            (Paused, Failed),
            (Paused, Cancelled),
            (WaitingPermission, Running),
            (WaitingPermission, Failed),
            (WaitingPermission, Cancelled),
        ];
        for from in SessionStatus::ALL {
            for to in SessionStatus::ALL {
                assert_eq!(
                    session_transition_allowed(from, to),
                    legal.contains(&(from, to)),
                    "{from} → {to}"
                );
            }
        }
        assert_eq!(legal.len(), 19, "转移表为冻结契约，增删需评审");
    }

    #[test]
    fn terminal_states_have_no_outgoing_edges() {
        for terminal in [
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Cancelled,
        ] {
            assert!(session_status_is_terminal(terminal));
            for to in SessionStatus::ALL {
                assert!(
                    !session_transition_allowed(terminal, to),
                    "终态 {terminal} 不应有出边 → {to}"
                );
            }
        }
        for active in [
            SessionStatus::Creating,
            SessionStatus::Idle,
            SessionStatus::Running,
        ] {
            assert!(!session_status_is_terminal(active));
        }
    }

    #[test]
    fn full_lifecycle_transitions_and_illegal_are_rejected() {
        let mut fsm = SessionFsm::new(SessionStatus::Creating);
        assert_eq!(
            fsm.transition(SessionStatus::Idle).unwrap().to,
            SessionStatus::Idle
        );
        assert_eq!(
            fsm.transition(SessionStatus::Running).unwrap().from,
            SessionStatus::Idle
        );

        // running → waiting_permission → running（D9 审批等待）。
        fsm.transition(SessionStatus::WaitingPermission).unwrap();
        fsm.transition(SessionStatus::Running).unwrap();

        // running → idle（run 终态），随后再次 running（可继续送消息）。
        fsm.transition(SessionStatus::Idle).unwrap();
        fsm.transition(SessionStatus::Running).unwrap();
        fsm.transition(SessionStatus::Cancelled).unwrap();
        assert!(fsm.is_terminal());

        // 终态后任何转移都被拒绝且不修改现状。
        let error = fsm.transition(SessionStatus::Running).unwrap_err();
        assert_eq!(
            error,
            SessionTransitionError::Illegal {
                from: SessionStatus::Cancelled,
                to: SessionStatus::Running,
            }
        );
        assert_eq!(fsm.status(), SessionStatus::Cancelled);
    }

    #[test]
    fn illegal_edges_do_not_mutate_state() {
        let mut fsm = SessionFsm::new(SessionStatus::Idle);
        let error = fsm
            .transition(SessionStatus::WaitingPermission)
            .unwrap_err();
        assert_eq!(
            error,
            SessionTransitionError::Illegal {
                from: SessionStatus::Idle,
                to: SessionStatus::WaitingPermission,
            }
        );
        assert_eq!(fsm.status(), SessionStatus::Idle);

        // creating → running 不允许（必须先落 idle）。
        let mut creating = SessionFsm::new(SessionStatus::Creating);
        assert!(creating.transition(SessionStatus::Running).is_err());
        assert_eq!(creating.status(), SessionStatus::Creating);
    }

    #[test]
    fn can_transition_matches_transition_result() {
        let fsm = SessionFsm::new(SessionStatus::Running);
        assert!(fsm.can_transition(SessionStatus::Idle));
        assert!(fsm.can_transition(SessionStatus::WaitingPermission));
        assert!(!fsm.can_transition(SessionStatus::Creating));
        let idle = SessionFsm::new(SessionStatus::Idle);
        assert!(!idle.can_transition(SessionStatus::Paused));
        assert!(idle.can_transition(SessionStatus::Completed));
    }

    #[test]
    fn change_serializes_lower_snake_case() {
        let change = SessionStatusChange {
            from: SessionStatus::WaitingPermission,
            to: SessionStatus::Running,
        };
        let value = serde_json::to_value(change).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"from": "waiting_permission", "to": "running"})
        );
        assert_eq!(change.to_string(), "waiting_permission → running");
    }

    #[test]
    fn terminal_classification_covers_all_statuses() {
        use SessionStatus::*;
        for status in SessionStatus::ALL {
            let expected = matches!(status, Completed | Failed | Cancelled);
            assert_eq!(session_status_is_terminal(status), expected, "{status}");
        }
    }
}
