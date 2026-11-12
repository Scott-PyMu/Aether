//! 并发闸门与优先级队列（M7-02/ADR-022；M10-07/ADR-037）。
//!
//! 形态（ADR-022 §2.1#6 / ADR-037 D-037-1）：
//! - `max_concurrent = clamp(floor(free_ram_gb / 1.5), 2, 10)`；
//! - 闸门计数 = 全局 `running` run 数；超限 → 新 run 进入 **优先级全局队列**
//!   （`runs.status='queued'`，不超卖）；
//! - 队列条目分数 `score = base_priority + runtime_weight + wait_boost`
//!   （升序，小者先）；同分按 `queued_at ASC → run_id ASC` 稳定排序；
//! - 等待 ≥ [`STARVATION_GUARD_MS`] 额外 `-100`（防饥饿，每 run 审计一次）；
//! - 全局队列上限 [`MAX_QUEUED_GLOBAL`]（超过由来源返回 `storage_backpressure`
//!   或 `session_busy`，不静默丢弃）。
//!
//! 本模块提供：
//! - 纯函数 [`max_concurrent_for_free_ram`]（参数注入断言的落点；8GB 模拟：
//!   `clamp(floor(7 / 1.5) = 4, 2, 10) = 4`）；
//! - 空闲内存采样源 [`FreeRamSource`]（生产 [`SysinfoFreeRam`]，测试替身
//!   [`FixedFreeRam`]；采样失败 → `None` = 保持既有闸门值）；
//! - 优先级队列载体 [`GateQueue`]（会话级等待槽与全局队列的两层合成语义见
//!   ADR-022 附录 A-6 / ADR-037 D-037-1）；
//! - [`ConcurrencyGateSnapshot`] 与 [`ConcurrencyGateCell`]：`health.concurrency_gate`
//!   的数据源（由 `SessionManager` 单写更新，健康查询同步读取）。
//!
//! 口径：队列位置从 1 计，仅指**全局队列**内位置（不含未进全局队列的会话等待槽
//! run）；`queued` = 全局队列长度。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use sysinfo::System;

use aether_core::{RunId, SessionId};

/// 闸门下限（A5：8GB 为下限；`clamp(..., 2, 10)`）。
pub const CONCURRENCY_GATE_MIN: usize = 2;
/// 闸门上限（A5：16GB 基准机支持 10 并发）。
pub const CONCURRENCY_GATE_MAX: usize = 10;
/// 每并发槽位所需空闲内存（GB；A5 公式分母）。
pub const CONCURRENCY_GATE_RAM_PER_SLOT_GB: f64 = 1.5;
/// 空闲内存采样周期（启动与巡检；常量级调参，AGENTS §3）。
pub const GATE_RAM_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

// ===== M10-07/ADR-037：优先级/公平性/重试常量 =====

/// 调用方优先级下界（ADR-037 D-037-1：−100…100）。
pub const QUEUE_PRIORITY_MIN: i32 = -100;
/// 调用方优先级上界。
pub const QUEUE_PRIORITY_MAX: i32 = 100;
/// 全局队列上限（ADR-037 D-037-1：64；超过按来源返回背压/会话忙）。
pub const MAX_QUEUED_GLOBAL: usize = 64;
/// 等待加权步长（ADR-037：`floor(wait_ms / 10s)`）。
pub const WAIT_BOOST_STEP_MS: i64 = 10_000;
/// 等待加权上限（ADR-037：60）。
pub const WAIT_BOOST_MAX: i32 = 60;
/// 饥饿保护阈值（ADR-037：5min；额外 −100 并审计一次）。
pub const STARVATION_GUARD_MS: i64 = 300_000;
/// 饥饿保护惩罚（ADR-037：−100）。
pub const STARVATION_GUARD_PENALTY: i32 = 100;
/// run 超时缺省值（ADR-037 D-037-2：30min）。
pub const DEFAULT_RUN_TIMEOUT_MS: i64 = 30 * 60 * 1000;
/// run 超时下界（ADR-037：1s）。
pub const MIN_RUN_TIMEOUT_MS: i64 = 1_000;
/// run 超时上界（ADR-037：24h）。
pub const MAX_RUN_TIMEOUT_MS: i64 = 24 * 60 * 60 * 1000;
/// 自动重试最大次数（ADR-037：1–5；缺省 1 = 关闭）。
pub const MAX_ATTEMPTS_LIMIT: u32 = 5;
/// 自动重试退避序列（ADR-037：1s/2s/4s/8s/16s/30s/60s）。
pub const RETRY_BACKOFF_MS: [i64; 7] = [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 60_000];
/// 退避抖动幅度（ADR-037：±20%）。
pub const RETRY_BACKOFF_JITTER_PERCENT: i64 = 20;
/// 超时后等待适配器收口的宽限（ADR-037 D-037-2：5s）。
pub const TIMEOUT_SETTLE_GRACE_MS: i64 = 5_000;

/// 闸门上限：`clamp(floor(free_ram_gb / 1.5), 2, 10)`（A5）。
///
/// NaN / 负值按最小档处理（保守，不放大并发）；`+∞` 按上限处理。
pub fn max_concurrent_for_free_ram(free_ram_gb: f64) -> usize {
    let raw = (free_ram_gb / CONCURRENCY_GATE_RAM_PER_SLOT_GB).floor();
    if raw.is_nan() || raw <= 0.0 {
        return CONCURRENCY_GATE_MIN;
    }
    // 上限先于转换（防超 usize 范围的理论输入）。
    if !raw.is_finite() || raw >= CONCURRENCY_GATE_MAX as f64 {
        return CONCURRENCY_GATE_MAX;
    }
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let value = raw as usize;
    value.clamp(CONCURRENCY_GATE_MIN, CONCURRENCY_GATE_MAX)
}

/// 空闲物理内存采样源（生产 sysinfo / 测试替身）。
pub trait FreeRamSource: Send + Sync + 'static {
    /// 当前空闲内存（GB）；不可用 → `None`（本次跳过，保持既有闸门值）。
    fn free_ram_gb(&self) -> Option<f64>;
}

/// 基于 `sysinfo` 的系统空闲内存采样器（与 RSS 巡检同源版本）。
#[derive(Debug)]
pub struct SysinfoFreeRam {
    system: Mutex<System>,
}

impl SysinfoFreeRam {
    pub fn new() -> Self {
        Self {
            system: Mutex::new(System::new()),
        }
    }
}

impl Default for SysinfoFreeRam {
    fn default() -> Self {
        Self::new()
    }
}

impl FreeRamSource for SysinfoFreeRam {
    fn free_ram_gb(&self) -> Option<f64> {
        let mut system = match self.system.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        system.refresh_memory();
        let bytes = system.available_memory();
        if bytes == 0 {
            return None;
        }
        Some(bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// 固定值采样替身（测试/演练参数注入；DoD3「8GB 模拟」样本 = 7.0）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FixedFreeRam {
    free_ram_gb: f64,
}

impl FixedFreeRam {
    pub fn new(free_ram_gb: f64) -> Self {
        Self { free_ram_gb }
    }
}

impl FreeRamSource for FixedFreeRam {
    fn free_ram_gb(&self) -> Option<f64> {
        Some(self.free_ram_gb)
    }
}

/// 等待原因（ADR-037 附录 B；`queue_list` 与 UI 状态条共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// 并发闸门满（全局队列/会话等待槽）。
    ConcurrencyGate,
    /// 运行时禁用（保留；当前运行中 run 不受禁用影响）。
    RuntimeDisabled,
    /// 供应商失效。
    ProviderInvalid,
    /// 存储背压。
    StorageBackpressure,
    /// 用户/审批暂停。
    ManualHold,
    /// 适配器不可用。
    AdapterUnavailable,
}

impl WaitReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConcurrencyGate => "concurrency_gate",
            Self::RuntimeDisabled => "runtime_disabled",
            Self::ProviderInvalid => "provider_invalid",
            Self::StorageBackpressure => "storage_backpressure",
            Self::ManualHold => "manual_hold",
            Self::AdapterUnavailable => "adapter_unavailable",
        }
    }
}

/// 全局队列等待项（ADR-037 D-037-1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateWaiter {
    pub session_id: SessionId,
    pub run_id: RunId,
    /// 调用方优先级（−100…100）。
    pub base_priority: i32,
    /// 固定运行时权重（ADR-022：claude-code=10 / codex=20 / deepseek-harness=30 / 其他=100）。
    pub runtime_weight: i32,
    /// 入队时刻（等待时长起点；重排队沿用原值）。
    pub queued_at: i64,
    /// 等待原因。
    pub wait_reason: WaitReason,
    /// 饥饿保护是否已审计（每 run 一次）。
    pub starvation_guarded: bool,
}

impl GateWaiter {
    pub fn new(
        session_id: SessionId,
        run_id: RunId,
        base_priority: i32,
        runtime_weight: i32,
        queued_at: i64,
        wait_reason: WaitReason,
    ) -> Self {
        Self {
            session_id,
            run_id,
            base_priority,
            runtime_weight,
            queued_at,
            wait_reason,
            starvation_guarded: false,
        }
    }

    /// 已等待毫秒（负值按 0，防时钟回拨）。
    pub fn wait_ms(&self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(self.queued_at).max(0)
    }

    /// 等待加权：`min(60, floor(wait_ms / 10s))`。
    pub fn wait_boost(&self, now_ms: i64) -> i32 {
        let steps = self.wait_ms(now_ms) / WAIT_BOOST_STEP_MS;
        i32::try_from(steps)
            .unwrap_or(WAIT_BOOST_MAX)
            .min(WAIT_BOOST_MAX)
    }

    /// 是否触发饥饿保护（≥5min 且尚未审计）。
    pub fn needs_starvation_guard(&self, now_ms: i64) -> bool {
        !self.starvation_guarded && self.wait_ms(now_ms) >= STARVATION_GUARD_MS
    }

    /// 队列分数（升序，小者先）。
    pub fn score(&self, now_ms: i64) -> i32 {
        let guard = if self.wait_ms(now_ms) >= STARVATION_GUARD_MS {
            STARVATION_GUARD_PENALTY
        } else {
            0
        };
        self.base_priority
            .saturating_add(self.runtime_weight)
            .saturating_add(self.wait_boost(now_ms))
            .saturating_sub(guard)
    }
}

/// 队列快照条目（`queue_list` / 诊断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueSnapshotEntry {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub priority: i32,
    pub score: i32,
    pub queued_at: i64,
    pub wait_ms: i64,
    pub wait_reason: WaitReason,
    /// 1 起；按 score/queued_at/run_id 排序后的位置。
    pub position: usize,
}

/// 优先级全局队列（单写者；不超卖；稳定排序）。
#[derive(Debug, Default)]
pub struct GateQueue {
    entries: VecDeque<GateWaiter>,
}

impl GateQueue {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 队列是否已满（ADR-037：全局上限 64）。
    pub fn is_full(&self) -> bool {
        self.entries.len() >= MAX_QUEUED_GLOBAL
    }

    /// 等待项只读切片（诊断/快照）。
    pub fn entries(&self) -> &VecDeque<GateWaiter> {
        &self.entries
    }

    /// 队列内位置（1 起；按当前分数排序；不在队列 → `None`）。
    pub fn position(&self, run_id: &RunId, now_ms: i64) -> Option<usize> {
        let mut ordered: Vec<&GateWaiter> = self.entries.iter().collect();
        ordered.sort_by(|left, right| compare_waiters(left, right, now_ms));
        ordered
            .iter()
            .position(|waiter| &waiter.run_id == run_id)
            .map(|index| index + 1)
    }

    /// 入队（重复入队为不变量破坏；调用方保证 run 唯一）。
    ///
    /// 队列满 → 返回 `false`（不插入；调用方按来源返回 `storage_backpressure`）。
    pub fn enqueue(&mut self, waiter: GateWaiter) -> bool {
        if self.is_full() {
            return false;
        }
        self.entries.push_back(waiter);
        true
    }

    /// 从队列移除（中断/取消路径；「槽位让给下一项」= 后续位置前移）。
    pub fn remove(&mut self, run_id: &RunId) -> Option<GateWaiter> {
        let index = self
            .entries
            .iter()
            .position(|waiter| &waiter.run_id == run_id)?;
        self.entries.remove(index)
    }

    /// 弹出当前最高优先级的等待项（score 最小；同分 queued_at/run_id）。
    pub fn pop_best(&mut self, now_ms: i64) -> Option<GateWaiter> {
        let index = self
            .entries
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| compare_waiters(left, right, now_ms))
            .map(|(index, _)| index)?;
        self.entries.remove(index)
    }

    /// 排序后的快照（不改变队列）。
    pub fn snapshot(&self, now_ms: i64) -> Vec<QueueSnapshotEntry> {
        let mut ordered: Vec<&GateWaiter> = self.entries.iter().collect();
        ordered.sort_by(|left, right| compare_waiters(left, right, now_ms));
        ordered
            .into_iter()
            .enumerate()
            .map(|(index, waiter)| QueueSnapshotEntry {
                run_id: waiter.run_id.clone(),
                session_id: waiter.session_id.clone(),
                priority: waiter.base_priority,
                score: waiter.score(now_ms),
                queued_at: waiter.queued_at,
                wait_ms: waiter.wait_ms(now_ms),
                wait_reason: waiter.wait_reason,
                position: index + 1,
            })
            .collect()
    }

    /// 标记饥饿保护已审计（每 run 一次）。
    pub fn mark_starvation_guarded(&mut self, run_id: &RunId) -> bool {
        if let Some(waiter) = self
            .entries
            .iter_mut()
            .find(|waiter| &waiter.run_id == run_id)
        {
            if waiter.starvation_guarded {
                return false;
            }
            waiter.starvation_guarded = true;
            return true;
        }
        false
    }
}

/// 等待项全序：score 升序 → queued_at 升序 → run_id 升序。
fn compare_waiters(left: &GateWaiter, right: &GateWaiter, now_ms: i64) -> std::cmp::Ordering {
    left.score(now_ms)
        .cmp(&right.score(now_ms))
        .then(left.queued_at.cmp(&right.queued_at))
        .then_with(|| left.run_id.as_str().cmp(right.run_id.as_str()))
}

/// 优先级桶（`health.concurrency_gate.queued_by_priority`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PriorityBucket {
    pub priority: i32,
    pub count: usize,
}

/// 闸门快照（`health.concurrency_gate`；ADR-018 附录 A-5 / ADR-037 D-037-3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConcurrencyGateSnapshot {
    /// 当前闸门上限（`clamp(floor(free_ram_gb / 1.5), 2, 10)`；只对新 run 生效）。
    pub limit: usize,
    /// 全局 `running` run 数（在途且已派发）。
    pub active: usize,
    /// 全局队列长度（闸门排队 run 数）。
    pub queued: usize,
    /// 按调用方优先级分桶的排队数（可选；M10-07）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_by_priority: Option<Vec<PriorityBucket>>,
    /// 当前触发饥饿保护的排队 run 数（可选；M10-07）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starvation_guard: Option<usize>,
}

impl ConcurrencyGateSnapshot {
    /// 兼容 M7-02 三字段构造（可选字段为 `None`）。
    pub fn new(limit: usize, active: usize, queued: usize) -> Self {
        Self {
            limit,
            active,
            queued,
            queued_by_priority: None,
            starvation_guard: None,
        }
    }
}

/// 闸门快照共享单元（`SessionManager` 单写；`health` 同步读；未接线 → `None`）。
#[derive(Debug, Clone, Default)]
pub struct ConcurrencyGateCell {
    inner: Arc<Mutex<Option<ConcurrencyGateSnapshot>>>,
}

impl ConcurrencyGateCell {
    pub fn new() -> Self {
        Self::default()
    }

    /// 写入快照（管理器在状态锁内调用；单写者）。
    pub fn set(&self, snapshot: ConcurrencyGateSnapshot) {
        match self.inner.lock() {
            Ok(mut guard) => *guard = Some(snapshot),
            Err(poisoned) => *poisoned.into_inner() = Some(snapshot),
        }
    }

    /// 读取快照（`None` = 尚未接线/未写过）。
    pub fn snapshot(&self) -> Option<ConcurrencyGateSnapshot> {
        match self.inner.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(value: &str) -> RunId {
        RunId::new(value).unwrap()
    }

    fn session(value: &str) -> SessionId {
        SessionId::new(value).unwrap()
    }

    fn waiter(value: &str, priority: i32, queued_at: i64) -> GateWaiter {
        GateWaiter::new(
            session("s1"),
            run(value),
            priority,
            10,
            queued_at,
            WaitReason::ConcurrencyGate,
        )
    }

    #[test]
    fn gate_limit_follows_a5_formula() {
        // ADR-022 §2.1 实现要点 4：8GB 模拟（free=7）→ clamp(floor(7/1.5)=4, 2, 10) = 4。
        assert_eq!(max_concurrent_for_free_ram(7.0), 4);
        // 16GB 基准机 → floor(16/1.5)=10 → 10（上限）。
        assert_eq!(max_concurrent_for_free_ram(16.0), 10);
        assert_eq!(max_concurrent_for_free_ram(15.0), 10);
        // floor 边界：10.4/1.5=6.93 → 6；10.5/1.5=7.0 → 7。
        assert_eq!(max_concurrent_for_free_ram(10.4), 6);
        assert_eq!(max_concurrent_for_free_ram(10.5), 7);
        // 下限钳制：3/1.5=2 → 2；1.4/1.5=0.93 → 2；0/负值 → 2。
        assert_eq!(max_concurrent_for_free_ram(3.0), 2);
        assert_eq!(max_concurrent_for_free_ram(1.4), 2);
        assert_eq!(max_concurrent_for_free_ram(0.0), 2);
        assert_eq!(max_concurrent_for_free_ram(-1.0), 2);
        // 非有限值 → 保守下限。
        assert_eq!(max_concurrent_for_free_ram(f64::NAN), 2);
        assert_eq!(max_concurrent_for_free_ram(f64::INFINITY), 10);
    }

    #[test]
    fn fixed_source_injects_parameter() {
        let source = FixedFreeRam::new(7.0);
        assert_eq!(source.free_ram_gb(), Some(7.0));
        assert_eq!(
            max_concurrent_for_free_ram(source.free_ram_gb().unwrap_or(0.0)),
            4
        );
    }

    #[test]
    fn priority_score_and_tie_break_follow_adr_037() {
        // 附录 A：0+10+0=10；0+20+6=26；−50+30+0=−20；100+10+300s boost 60 → 70。
        let base = waiter("r1", 0, 0);
        assert_eq!(base.score(0), 10);
        let mid = GateWaiter::new(
            session("s1"),
            run("r2"),
            0,
            20,
            0,
            WaitReason::ConcurrencyGate,
        );
        assert_eq!(mid.score(60_000), 26);
        let negative = GateWaiter::new(
            session("s1"),
            run("r3"),
            -50,
            30,
            0,
            WaitReason::ConcurrencyGate,
        );
        assert_eq!(negative.score(0), -20);
        let boosted = GateWaiter::new(
            session("s1"),
            run("r4"),
            100,
            10,
            0,
            WaitReason::ConcurrencyGate,
        );
        // wait_boost = min(60, floor(300000/10000)) = 30；guard −100 → 100 + 10 + 30 − 100 = 40。
        assert_eq!(boosted.score(300_000), 40);
        assert_eq!(
            boosted.score(300_000),
            100 + 10 + 30 - STARVATION_GUARD_PENALTY,
            "guard 生效时分数额外 −100"
        );

        // 同分：queued_at 更早优先，其次 run_id 字典序。
        let mut queue = GateQueue::default();
        queue.enqueue(waiter("r-b", 0, 100));
        queue.enqueue(waiter("r-a", 0, 100));
        queue.enqueue(waiter("r-c", 0, 50));
        let snapshot = queue.snapshot(100);
        assert_eq!(snapshot[0].run_id, run("r-c"), "queued_at 早者先");
        assert_eq!(snapshot[1].run_id, run("r-a"), "同刻按 run_id 字典序");
        assert_eq!(snapshot[2].run_id, run("r-b"));
        assert_eq!(snapshot[0].position, 1);
    }

    #[test]
    fn higher_priority_pops_first_and_long_wait_boosts() {
        let mut queue = GateQueue::default();
        queue.enqueue(waiter("low", 0, 0));
        queue.enqueue(waiter("high", -80, 0));
        assert_eq!(queue.pop_best(0).expect("队首").run_id, run("high"));
        assert_eq!(queue.pop_best(0).expect("队首").run_id, run("low"));

        // 等待加权：低优先级等待 700s 后分数反超（boost 60）。
        let mut queue = GateQueue::default();
        queue.enqueue(waiter("low", 0, 0));
        queue.enqueue(waiter("high", -80, 700_000));
        // low: 0+10+60=70；high: −80+10+0=−70（high 仍先，验证 boost 不越界）。
        assert_eq!(queue.pop_best(700_000).expect("队首").run_id, run("high"));
        let mut queue = GateQueue::default();
        queue.enqueue(waiter("low", 0, 0));
        queue.enqueue(waiter("high", 0, 700_000));
        assert_eq!(
            queue.pop_best(700_000).expect("队首").run_id,
            run("low"),
            "同优先级等待更久者先（wait_boost）"
        );
    }

    #[test]
    fn queue_positions_are_one_based_and_shift_after_remove() {
        let mut queue = GateQueue::default();
        assert!(queue.is_empty());
        queue.enqueue(waiter("r1", 0, 0));
        queue.enqueue(waiter("r2", 0, 1));
        queue.enqueue(waiter("r3", 0, 2));
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.position(&run("r1"), 2), Some(1));
        assert_eq!(queue.position(&run("r3"), 2), Some(3));
        assert_eq!(queue.position(&run("r9"), 2), None);

        let removed = queue.remove(&run("r2")).expect("移除命中");
        assert_eq!(removed.session_id, session("s1"));
        assert_eq!(queue.position(&run("r3"), 2), Some(2));
        assert_eq!(queue.remove(&run("r2")), None);

        let head = queue.pop_best(2).expect("队首");
        assert_eq!(head.run_id, run("r1"));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.position(&run("r3"), 2), Some(1));
    }

    #[test]
    fn queue_enforces_global_limit_and_marks_starvation_once() {
        let mut queue = GateQueue::default();
        for index in 0..MAX_QUEUED_GLOBAL {
            assert!(queue.enqueue(waiter(&format!("r{index}"), 0, 0)));
        }
        assert!(queue.is_full());
        assert!(!queue.enqueue(waiter("overflow", 0, 0)));
        assert_eq!(queue.len(), MAX_QUEUED_GLOBAL);

        assert!(queue.mark_starvation_guarded(&run("r0")));
        assert!(
            !queue.mark_starvation_guarded(&run("r0")),
            "每 run 只审计一次"
        );
        assert!(!queue.mark_starvation_guarded(&run("missing")));
    }

    #[test]
    fn gate_cell_roundtrip_and_default_is_empty() {
        let cell = ConcurrencyGateCell::new();
        assert_eq!(cell.snapshot(), None);
        let snapshot = ConcurrencyGateSnapshot::new(4, 4, 2);
        cell.set(snapshot.clone());
        assert_eq!(cell.snapshot(), Some(snapshot));
        // 克隆共享同一单元（管理器写 / health 读）。
        let clone = cell.clone();
        assert_eq!(clone.snapshot(), cell.snapshot());
        let serialized =
            serde_json::to_value(ConcurrencyGateSnapshot::new(4, 4, 2)).expect("序列化");
        assert_eq!(
            serialized,
            serde_json::json!({ "limit": 4, "active": 4, "queued": 2 })
        );
        let extended = ConcurrencyGateSnapshot {
            limit: 4,
            active: 3,
            queued: 2,
            queued_by_priority: Some(vec![PriorityBucket {
                priority: 10,
                count: 1,
            }]),
            starvation_guard: Some(1),
        };
        let value = serde_json::to_value(extended).expect("序列化");
        assert_eq!(value["queued_by_priority"][0]["priority"], 10);
        assert_eq!(value["starvation_guard"], 1);
    }
}
