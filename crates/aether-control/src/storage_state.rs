//! 持久化降级状态机（D4；ADR-003/ADR-004 边界澄清）。
//!
//! ```text
//! normal ──连续 3 次写事务尝试失败（含首次）──→ persist_degraded（只读）
//!   ▲                                        │
//!   └──修复外部条件 + 核心重启 + 启动自检通过──┘   （P0 无运行期热恢复）
//! ```
//!
//! 关键边界（ADR-004 决策 1；重试口径 ADR-007 决策 2）：
//! - 触发源仅三类：写事务连续尝试失败（`MAX_WRITE_ATTEMPTS = 3`，含首次）、
//!   空间护栏（剩余 <500MB）、完整性失败（`quick_check` 不过）；
//!   **写队列临时高水位（≤L2）不进入本状态**（D8 背压，毫秒级回落，队列回落即恢复）；
//! - 恢复仅经「修复外部条件 + 重启核心 + 启动自检通过」：本模块**不提供运行期热恢复
//!   API**——新进程用 [`StorageStateMachine::from_startup_check`] 依据自检报告重建状态；
//! - 降级期语义：拒绝新写入/新 run；读查询、诊断导出、备份/导出保持可用。
//!
//! 状态机并发安全（进程内唯一实例，管线与命令层共享）。

use std::sync::Mutex;

use crate::error::PipelineError;
use crate::time::now_ms;

/// 空间护栏下限（D3：剩余磁盘 <500MB → 只读模式）。
pub const SPACE_GUARD_MIN_FREE_BYTES: u64 = 500 * 1024 * 1024;

/// 存储状态（`health` 返回 `storage_state`；只读模式并入 `persist_degraded`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageState {
    /// 读写正常。
    Normal,
    /// 持久化降级：写事务连续失败 / 空间护栏 / 完整性失败触发；语义等价只读。
    PersistDegraded,
}

impl StorageState {
    /// `health.storage_state` 取值（DoD：`storage_state=persist_degraded`）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::PersistDegraded => "persist_degraded",
        }
    }

    pub const fn is_degraded(self) -> bool {
        matches!(self, Self::PersistDegraded)
    }
}

/// 降级触发源（仅记录差异，状态语义相同；写入诊断包）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DegradeTrigger {
    /// 写事务连续尝试失败（`MAX_WRITE_ATTEMPTS = 3`，含首次）。
    WriteFailure { attempts: u32, last_error: String },
    /// 空间护栏：剩余磁盘低于 500MB。
    SpaceGuard { free_bytes: u64 },
    /// 完整性失败：启动 `quick_check` 不过。
    IntegrityFailure { detail: String },
}

impl DegradeTrigger {
    /// 触发源代码（诊断/evidence 断言用）。
    pub const fn code(&self) -> &'static str {
        match self {
            Self::WriteFailure { .. } => "write_failure",
            Self::SpaceGuard { .. } => "space_guard",
            Self::IntegrityFailure { .. } => "integrity_failure",
        }
    }

    /// 诊断用描述。
    pub fn describe(&self) -> String {
        match self {
            Self::WriteFailure {
                attempts,
                last_error,
            } => format!("写事务连续尝试失败 {attempts} 次（含首次）：{last_error}"),
            Self::SpaceGuard { free_bytes } => {
                format!("空间护栏触发：剩余 {free_bytes} 字节 < {SPACE_GUARD_MIN_FREE_BYTES}（D3）")
            }
            Self::IntegrityFailure { detail } => format!("完整性失败（quick_check）：{detail}"),
        }
    }
}

/// 启动自检报告（D4 恢复路径：修复外部条件 + 重启核心 + 启动自检通过）。
///
/// 由启动序列（M1-06/M2-07）在打开库后构造；运行期任何路径都不得伪造该报告恢复状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSelfCheckReport {
    /// 启动 `quick_check` 是否通过（D3）。
    pub quick_check_ok: bool,
    /// `quick_check` 失败详情（通过时为 `None`）。
    pub quick_check_detail: Option<String>,
    /// 数据目录所在卷剩余字节（空间护栏输入，D3）。
    pub free_bytes: u64,
    /// 自检时间（Unix epoch 毫秒）。
    pub checked_at_ms: i64,
}

impl StartupSelfCheckReport {
    /// 通过的自检（`quick_check` ok + 空间充足）。
    pub fn passing(free_bytes: u64) -> Self {
        Self {
            quick_check_ok: true,
            quick_check_detail: None,
            free_bytes: free_bytes.max(SPACE_GUARD_MIN_FREE_BYTES),
            checked_at_ms: now_ms(),
        }
    }

    /// 完整性失败报告（`quick_check` 不过）。
    pub fn failing_integrity(detail: impl Into<String>) -> Self {
        Self {
            quick_check_ok: false,
            quick_check_detail: Some(detail.into()),
            free_bytes: SPACE_GUARD_MIN_FREE_BYTES,
            checked_at_ms: now_ms(),
        }
    }

    /// 空间不足报告。
    pub fn failing_space(free_bytes: u64) -> Self {
        Self {
            quick_check_ok: true,
            quick_check_detail: None,
            free_bytes,
            checked_at_ms: now_ms(),
        }
    }

    /// 依报告判定初始状态与触发源（启动自检顺序：完整性先于空间）。
    pub fn evaluate(&self) -> Result<(), DegradeTrigger> {
        if !self.quick_check_ok {
            return Err(DegradeTrigger::IntegrityFailure {
                detail: self
                    .quick_check_detail
                    .clone()
                    .unwrap_or_else(|| "quick_check 未通过".to_owned()),
            });
        }
        if self.free_bytes < SPACE_GUARD_MIN_FREE_BYTES {
            return Err(DegradeTrigger::SpaceGuard {
                free_bytes: self.free_bytes,
            });
        }
        Ok(())
    }
}

struct StateInner {
    state: StorageState,
    trigger: Option<DegradeTrigger>,
    since_ms: Option<i64>,
}

/// 持久化降级状态机（进程内共享；见模块文档）。
pub struct StorageStateMachine {
    inner: Mutex<StateInner>,
}

impl StorageStateMachine {
    /// 依据启动自检报告构造（通过 → `normal`；失败 → `persist_degraded` + 触发源）。
    pub fn from_startup_check(report: &StartupSelfCheckReport) -> Self {
        match report.evaluate() {
            Ok(()) => Self {
                inner: Mutex::new(StateInner {
                    state: StorageState::Normal,
                    trigger: None,
                    since_ms: None,
                }),
            },
            Err(trigger) => Self {
                inner: Mutex::new(StateInner {
                    state: StorageState::PersistDegraded,
                    trigger: Some(trigger),
                    since_ms: Some(now_ms()),
                }),
            },
        }
    }

    /// 运行期进入降级（**唯一**运行期状态变更入口；首次触发源胜出，不可热恢复）。
    ///
    /// 返回 `true` 表示本次调用完成了状态转移。
    pub fn enter_degraded(&self, trigger: DegradeTrigger) -> bool {
        self.with_inner(|inner| {
            if inner.state.is_degraded() {
                return false;
            }
            inner.state = StorageState::PersistDegraded;
            inner.trigger = Some(trigger);
            inner.since_ms = Some(now_ms());
            true
        })
    }

    /// 当前状态。
    pub fn state(&self) -> StorageState {
        self.with_inner(|inner| inner.state)
    }

    /// 降级触发源（正常时为 `None`）。
    pub fn trigger(&self) -> Option<DegradeTrigger> {
        self.with_inner(|inner| inner.trigger.clone())
    }

    /// 降级进入时间（Unix epoch 毫秒；正常时为 `None`）。
    pub fn degraded_since_ms(&self) -> Option<i64> {
        self.with_inner(|inner| inner.since_ms)
    }

    /// 写入/新 run 准入：降级期返回 `persist_degraded` 结构化错误（D4 降级期语义 1）。
    pub fn accept_write(&self) -> Result<(), PipelineError> {
        let inner_state = self.state();
        if inner_state.is_degraded() {
            let reason = self
                .trigger()
                .map(|trigger| trigger.describe())
                .unwrap_or_else(|| "存储只读（触发源未记录）".to_owned());
            return Err(PipelineError::PersistDegraded { reason });
        }
        Ok(())
    }

    fn with_inner<T>(&self, operation: impl FnOnce(&mut StateInner) -> T) -> T {
        match self.inner.lock() {
            Ok(mut guard) => operation(&mut guard),
            // 互斥锁中毒（持锁线程 panic）：状态本身仍然有效，继续使用不 panic 逃逸。
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                operation(&mut guard)
            }
        }
    }
}

impl Default for StorageStateMachine {
    fn default() -> Self {
        Self::from_startup_check(&StartupSelfCheckReport::passing(SPACE_GUARD_MIN_FREE_BYTES))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_check_passing_starts_normal() {
        let machine = StorageStateMachine::from_startup_check(&StartupSelfCheckReport::passing(
            10 * 1024 * 1024 * 1024,
        ));
        assert_eq!(machine.state(), StorageState::Normal);
        assert_eq!(machine.state().as_str(), "normal");
        assert_eq!(machine.trigger(), None);
        assert_eq!(machine.degraded_since_ms(), None);
        assert!(machine.accept_write().is_ok());
    }

    #[test]
    fn startup_check_failures_start_degraded_with_source() {
        let integrity = StorageStateMachine::from_startup_check(
            &StartupSelfCheckReport::failing_integrity("page 3 损坏"),
        );
        assert_eq!(integrity.state(), StorageState::PersistDegraded);
        assert_eq!(
            integrity.trigger().map(|trigger| trigger.code()),
            Some("integrity_failure")
        );
        assert!(integrity.degraded_since_ms().is_some());
        let error = integrity.accept_write().expect_err("只读必须拒绝写入");
        assert_eq!(error.code(), "persist_degraded");

        let space = StorageStateMachine::from_startup_check(
            &StartupSelfCheckReport::failing_space(SPACE_GUARD_MIN_FREE_BYTES - 1),
        );
        assert_eq!(space.state(), StorageState::PersistDegraded);
        assert_eq!(
            space.trigger().map(|trigger| trigger.code()),
            Some("space_guard")
        );
    }

    #[test]
    fn runtime_entry_keeps_first_trigger_and_rejects_writes() {
        let machine = StorageStateMachine::default();
        assert!(machine.enter_degraded(DegradeTrigger::WriteFailure {
            attempts: 3,
            last_error: "database or disk is full".to_owned(),
        }));
        assert!(!machine.enter_degraded(DegradeTrigger::SpaceGuard { free_bytes: 1 }));
        assert_eq!(
            machine.trigger().map(|trigger| trigger.code()),
            Some("write_failure"),
            "首次触发源胜出"
        );
        let error = machine.accept_write().expect_err("降级期拒绝新写入/新 run");
        assert_eq!(error.code(), "persist_degraded");
        assert!(error.to_string().contains("重启核心"));
    }

    #[test]
    fn no_hot_recovery_only_restart_with_passing_check() {
        let machine = StorageStateMachine::default();
        machine.enter_degraded(DegradeTrigger::WriteFailure {
            attempts: 3,
            last_error: "io error".to_owned(),
        });
        // P0 无运行期热恢复：本机无任何 API 能把状态改回 normal。
        assert_eq!(machine.state(), StorageState::PersistDegraded);
        assert!(machine.accept_write().is_err());

        // 退出断言（模拟重启）：新进程 + 启动自检通过 → normal；旧实例保持降级。
        let restarted = StorageStateMachine::from_startup_check(&StartupSelfCheckReport::passing(
            2 * 1024 * 1024 * 1024,
        ));
        assert_eq!(restarted.state(), StorageState::Normal);
        assert_eq!(machine.state(), StorageState::PersistDegraded);
    }

    #[test]
    fn startup_failure_survives_until_check_passes() {
        let failing = StartupSelfCheckReport::failing_space(1024);
        let machine = StorageStateMachine::from_startup_check(&failing);
        assert!(machine.accept_write().is_err());
        let still_failing = StartupSelfCheckReport::failing_space(2048);
        assert!(StorageStateMachine::from_startup_check(&still_failing)
            .accept_write()
            .is_err());
        let passing = StartupSelfCheckReport::passing(SPACE_GUARD_MIN_FREE_BYTES);
        assert!(StorageStateMachine::from_startup_check(&passing)
            .accept_write()
            .is_ok());
    }

    #[test]
    fn pressure_is_not_a_degrade_trigger() {
        // D8/ADR-004：写队列临时高水位（≤L2）不改变存储状态——状态机只接受三类触发源。
        let machine = StorageStateMachine::default();
        assert_eq!(machine.state(), StorageState::Normal);
        // 触发源枚举之外无入口：enter_degraded 的两个调用点（管线）只传三类触发源。
        assert!(machine.accept_write().is_ok());
        assert_eq!(machine.trigger(), None);
    }
}
