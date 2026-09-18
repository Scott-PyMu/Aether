//! D5 终止序列（跨平台，ADR-004）：逐步硬超时 + 每步平台机制断言。
//!
//! 冻结顺序（D5）：
//! 1. `shutdown` RPC（5s）；
//! 2. Unix `kill -TERM -<pgid>`（5s）/ Windows `taskkill /PID x /T`（5s，优雅）；
//! 3. Unix `kill -KILL -<pgid>` / Windows `TerminateJobObject`（首选强杀整树）；
//! 4. 兜底 `taskkill /PID x /T /F`。
//!
//! 每步：先发起动作，再以硬超时轮询整树是否退出；退出即停止后续步骤。
//! 严禁裸 kill 单个 PID（AGENTS §6：子进程会变孤儿）——本模块只经
//! [`TerminationTarget`] 的整树机制执行。

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// 步骤 1：`shutdown` RPC 硬超时（D5/D6 方法表 5s）。
pub const SHUTDOWN_RPC_TIMEOUT: Duration = Duration::from_secs(5);
/// 步骤 2：优雅终止硬超时（Unix SIGTERM / Windows `taskkill /T`）。
pub const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(5);
/// 步骤 3：强制整树回收硬超时（Unix SIGKILL / Windows `TerminateJobObject`）。
pub const FORCE_TIMEOUT: Duration = Duration::from_secs(3);
/// 步骤 4：兜底硬超时（Windows `taskkill /T /F`）；步骤 1+2+3+4 ≤ 15s（D5 重启预算）。
pub const FALLBACK_TIMEOUT: Duration = Duration::from_secs(2);
/// 退出轮询间隔。
pub const ALIVE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// 终止序列步骤（顺序即执行顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationStep {
    ShutdownRpc,
    Graceful,
    Force,
    Fallback,
}

impl TerminationStep {
    pub const ALL: [Self; 4] = [
        Self::ShutdownRpc,
        Self::Graceful,
        Self::Force,
        Self::Fallback,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ShutdownRpc => "shutdown_rpc",
            Self::Graceful => "graceful",
            Self::Force => "force",
            Self::Fallback => "fallback",
        }
    }
}

/// 单步结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepResult {
    /// 动作后整树已退出。
    Exited,
    /// 硬超时内仍存活 → 进入下一步。
    StillAlive,
    /// 动作下发失败（记录后继续下一步）。
    Failed { detail: String },
    /// 跳过（如无连接可发 RPC）。
    Skipped { detail: String },
}

/// 单步记录（平台断言证据：机制名 + 硬超时 + 实际耗时）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    pub step: TerminationStep,
    /// 实际机制：`shutdown_rpc` / `sigterm_pgid` / `taskkill_tree` /
    /// `sigkill_pgid` / `terminate_job_object` / `taskkill_tree_force` / ...
    pub mechanism: &'static str,
    pub result: StepResult,
    pub timeout_ms: u64,
    pub elapsed_ms: u64,
    pub exited_after: bool,
}

/// 终止序列报告。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminationReport {
    pub steps: Vec<StepOutcome>,
    /// 序列结束时整树是否已退出。
    pub exited: bool,
    pub total_ms: u64,
}

impl TerminationReport {
    /// 实际执行过的步骤序列（平台断言用）。
    pub fn executed_steps(&self) -> Vec<TerminationStep> {
        self.steps.iter().map(|step| step.step).collect()
    }

    /// 实际使用的机制序列（平台断言用）。
    pub fn mechanisms(&self) -> Vec<&'static str> {
        self.steps.iter().map(|step| step.mechanism).collect()
    }

    pub fn step(&self, step: TerminationStep) -> Option<&StepOutcome> {
        self.steps.iter().find(|outcome| outcome.step == step)
    }
}

/// 逐步硬超时预算（默认 D5；测试可压缩）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminationBudget {
    pub shutdown_rpc: Duration,
    pub graceful: Duration,
    pub force: Duration,
    pub fallback: Duration,
}

impl TerminationBudget {
    /// D5 默认：5s / 5s / 3s / 2s（合计 ≤15s，重启预算）。
    pub const fn d5() -> Self {
        Self {
            shutdown_rpc: SHUTDOWN_RPC_TIMEOUT,
            graceful: GRACEFUL_TIMEOUT,
            force: FORCE_TIMEOUT,
            fallback: FALLBACK_TIMEOUT,
        }
    }

    pub const fn for_step(&self, step: TerminationStep) -> Duration {
        match step {
            TerminationStep::ShutdownRpc => self.shutdown_rpc,
            TerminationStep::Graceful => self.graceful,
            TerminationStep::Force => self.force,
            TerminationStep::Fallback => self.fallback,
        }
    }

    /// 序列总预算（D5 重启预算 ≤15s）。
    pub const fn total(&self) -> Duration {
        Duration::from_millis(
            self.shutdown_rpc.as_millis() as u64
                + self.graceful.as_millis() as u64
                + self.force.as_millis() as u64
                + self.fallback.as_millis() as u64,
        )
    }
}

impl Default for TerminationBudget {
    fn default() -> Self {
        Self::d5()
    }
}

pub type ActionFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// 终止目标抽象（真实实现：`AdapterProcess`；单测：脚本化假目标）。
///
/// 动作方法均为异步且接收本步硬超时：Windows `taskkill` 兜底必须在超时内被中止，
/// 不得阻塞异步执行器（否则「逐步硬超时」失效）。
pub trait TerminationTarget: Send {
    fn pid(&self) -> u32;

    /// 进程（含整树）是否仍存活。
    fn is_alive(&mut self) -> bool;

    /// 步骤 1：发送 `shutdown` RPC（无连接时可返回 `Err`，由序列记为 Skipped/Failed）。
    fn shutdown_rpc(&self) -> ActionFuture<'_>;

    /// 步骤 2：优雅终止（Unix `kill -TERM -<pgid>` / Windows `taskkill /PID x /T`）。
    fn graceful(&mut self, timeout: Duration) -> ActionFuture<'_>;

    /// 步骤 3：强制整树回收（Unix `kill -KILL -<pgid>` / Windows `TerminateJobObject`）。
    fn force(&mut self, timeout: Duration) -> ActionFuture<'_>;

    /// 步骤 4：兜底（Windows `taskkill /PID x /T /F`；Unix 为 SIGKILL 组重试）。
    fn fallback(&mut self, timeout: Duration) -> ActionFuture<'_>;

    /// 平台机制名（进入报告，作为平台断言证据）。
    fn mechanism(&self, step: TerminationStep) -> &'static str;
}

/// 执行 D5 终止序列。
pub async fn run_termination(
    target: &mut dyn TerminationTarget,
    budget: TerminationBudget,
) -> TerminationReport {
    let started = Instant::now();
    let mut report = TerminationReport::default();
    for step in TerminationStep::ALL {
        if !target.is_alive() {
            report.exited = true;
            break;
        }
        let timeout = budget.for_step(step);
        let step_started = Instant::now();
        let result = match step {
            TerminationStep::ShutdownRpc => {
                let issued = match tokio::time::timeout(timeout, target.shutdown_rpc()).await {
                    Ok(Ok(())) => StepResult::StillAlive,
                    Ok(Err(detail)) => StepResult::Failed { detail },
                    Err(_) => StepResult::StillAlive,
                };
                if target.is_alive() {
                    issued
                } else {
                    StepResult::Exited
                }
            }
            TerminationStep::Graceful | TerminationStep::Force | TerminationStep::Fallback => {
                let action = match step {
                    TerminationStep::Graceful => target.graceful(timeout),
                    TerminationStep::Force => target.force(timeout),
                    _ => target.fallback(timeout),
                };
                match tokio::time::timeout(timeout, action).await {
                    Ok(Ok(())) => {
                        let remaining = timeout.saturating_sub(step_started.elapsed());
                        if await_exit(target, remaining).await {
                            StepResult::Exited
                        } else {
                            StepResult::StillAlive
                        }
                    }
                    Ok(Err(detail)) => StepResult::Failed { detail },
                    Err(_) => StepResult::StillAlive,
                }
            }
        };
        let elapsed = step_started.elapsed();
        let exited_after = !target.is_alive();
        report.steps.push(StepOutcome {
            step,
            mechanism: target.mechanism(step),
            result,
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            exited_after,
        });
        if exited_after {
            report.exited = true;
            break;
        }
    }
    report.exited = !target.is_alive();
    report.total_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    report
}

/// 以硬超时轮询整树退出。
async fn await_exit(target: &mut dyn TerminationTarget, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !target.is_alive() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(ALIVE_POLL_INTERVAL.min(remaining)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// 脚本化假目标：在指定步骤后退出；或永不退出（验证硬超时）。
    #[derive(Debug)]
    struct FakeTarget {
        pid: u32,
        alive: Arc<AtomicBool>,
        calls: Arc<Mutex<Vec<TerminationStep>>>,
        exit_after: Option<TerminationStep>,
        rpc_fails: bool,
    }

    impl FakeTarget {
        fn new(exit_after: Option<TerminationStep>) -> Self {
            Self {
                pid: 4242,
                alive: Arc::new(AtomicBool::new(true)),
                calls: Arc::new(Mutex::new(Vec::new())),
                exit_after,
                rpc_fails: false,
            }
        }

        fn record(&self, step: TerminationStep) {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(step);
            }
            if self.exit_after == Some(step) {
                self.alive.store(false, Ordering::SeqCst);
            }
        }
    }

    impl TerminationTarget for FakeTarget {
        fn pid(&self) -> u32 {
            self.pid
        }

        fn is_alive(&mut self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }

        fn shutdown_rpc(&self) -> ActionFuture<'_> {
            Box::pin(async move {
                if self.rpc_fails {
                    return Err("连接不可用".to_owned());
                }
                self.record(TerminationStep::ShutdownRpc);
                Ok(())
            })
        }

        fn graceful(&mut self, _timeout: Duration) -> ActionFuture<'_> {
            self.record(TerminationStep::Graceful);
            Box::pin(async { Ok(()) })
        }

        fn force(&mut self, _timeout: Duration) -> ActionFuture<'_> {
            self.record(TerminationStep::Force);
            Box::pin(async { Ok(()) })
        }

        fn fallback(&mut self, _timeout: Duration) -> ActionFuture<'_> {
            self.record(TerminationStep::Fallback);
            Box::pin(async { Ok(()) })
        }

        fn mechanism(&self, step: TerminationStep) -> &'static str {
            match step {
                TerminationStep::ShutdownRpc => "shutdown_rpc",
                TerminationStep::Graceful => "fake_graceful",
                TerminationStep::Force => "fake_force",
                TerminationStep::Fallback => "fake_fallback",
            }
        }
    }

    fn fast_budget() -> TerminationBudget {
        TerminationBudget {
            shutdown_rpc: Duration::from_millis(50),
            graceful: Duration::from_millis(50),
            force: Duration::from_millis(50),
            fallback: Duration::from_millis(50),
        }
    }

    #[tokio::test]
    async fn already_dead_target_runs_no_step() {
        let mut target = FakeTarget::new(None);
        target.alive.store(false, Ordering::SeqCst);
        let report = run_termination(&mut target, fast_budget()).await;
        assert!(report.exited);
        assert!(report.steps.is_empty());
    }

    #[tokio::test]
    async fn graceful_exit_stops_sequence_after_rpc_step() {
        let mut target = FakeTarget::new(Some(TerminationStep::Graceful));
        let report = run_termination(&mut target, fast_budget()).await;
        assert!(report.exited);
        assert_eq!(
            report.executed_steps(),
            vec![TerminationStep::ShutdownRpc, TerminationStep::Graceful]
        );
        assert_eq!(report.mechanisms(), vec!["shutdown_rpc", "fake_graceful"]);
        assert_eq!(
            report.step(TerminationStep::Graceful).map(|s| &s.result),
            Some(&StepResult::Exited)
        );
    }

    #[tokio::test]
    async fn full_sequence_steps_run_in_d5_order_when_target_never_exits() {
        let mut target = FakeTarget::new(None);
        let budget = fast_budget();
        let report = run_termination(&mut target, budget).await;
        assert!(!report.exited);
        assert_eq!(report.executed_steps(), TerminationStep::ALL.to_vec());
        for step in &report.steps {
            assert_eq!(step.result, StepResult::StillAlive, "{:?}", step.step);
            assert_eq!(
                step.timeout_ms,
                budget.for_step(step.step).as_millis() as u64
            );
            assert!(!step.exited_after);
        }
        // 逐步硬超时：每步实际耗时接近该步预算（含 50ms 轮询）。
        assert!(
            report.total_ms < 4 * 100 + 200,
            "总耗时 {}",
            report.total_ms
        );
    }

    #[tokio::test]
    async fn graceful_failure_falls_through_to_force() {
        struct FailingGraceful(FakeTarget);

        impl TerminationTarget for FailingGraceful {
            fn pid(&self) -> u32 {
                self.0.pid()
            }
            fn is_alive(&mut self) -> bool {
                self.0.is_alive()
            }
            fn shutdown_rpc(&self) -> ActionFuture<'_> {
                self.0.shutdown_rpc()
            }
            fn graceful(&mut self, _timeout: Duration) -> ActionFuture<'_> {
                Box::pin(async { Err("taskkill 不可用".to_owned()) })
            }
            fn force(&mut self, _timeout: Duration) -> ActionFuture<'_> {
                self.0.record(TerminationStep::Force);
                self.0.alive.store(false, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
            fn fallback(&mut self, timeout: Duration) -> ActionFuture<'_> {
                self.0.fallback(timeout)
            }
            fn mechanism(&self, step: TerminationStep) -> &'static str {
                match step {
                    TerminationStep::Graceful => "failing_graceful",
                    _ => self.0.mechanism(step),
                }
            }
        }

        let mut target = FailingGraceful(FakeTarget::new(None));
        let report = run_termination(&mut target, fast_budget()).await;
        assert!(report.exited);
        assert_eq!(
            report.executed_steps(),
            vec![
                TerminationStep::ShutdownRpc,
                TerminationStep::Graceful,
                TerminationStep::Force
            ]
        );
        assert!(matches!(
            report.step(TerminationStep::Graceful).map(|s| &s.result),
            Some(StepResult::Failed { .. })
        ));
        assert_eq!(
            report.step(TerminationStep::Force).map(|s| &s.result),
            Some(&StepResult::Exited)
        );
    }

    #[tokio::test]
    async fn rpc_failure_is_recorded_and_sequence_continues() {
        let mut target = FakeTarget::new(Some(TerminationStep::Force));
        target.rpc_fails = true;
        let report = run_termination(&mut target, fast_budget()).await;
        assert!(report.exited);
        assert_eq!(
            report.executed_steps(),
            vec![
                TerminationStep::ShutdownRpc,
                TerminationStep::Graceful,
                TerminationStep::Force
            ]
        );
        assert!(matches!(
            report.step(TerminationStep::ShutdownRpc).map(|s| &s.result),
            Some(StepResult::Failed { .. })
        ));
    }

    #[test]
    fn budget_matches_d5_and_total_within_15s() {
        let budget = TerminationBudget::d5();
        assert_eq!(budget.shutdown_rpc, Duration::from_secs(5));
        assert_eq!(budget.graceful, Duration::from_secs(5));
        assert_eq!(budget.force, Duration::from_secs(3));
        assert_eq!(budget.fallback, Duration::from_secs(2));
        assert!(budget.total() <= Duration::from_secs(15));
        assert_eq!(budget.total(), Duration::from_secs(15));
        assert_eq!(TerminationBudget::default(), budget);
    }
}
