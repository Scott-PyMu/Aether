//! 并发闸门（M7-02/ADR-022；A5）。
//!
//! 形态（ADR-022 §2.1#6）：`max_concurrent = clamp(floor(free_ram_gb / 1.5), 2, 10)`；
//! 闸门计数 = 全局 `running` run 数；超限 → 新 run 入 **FIFO 全局队列**
//! （`runs.status='queued'`，不超卖）；run 终态触发队列推进（单写者顺序）。
//!
//! 本模块提供：
//! - 纯函数 [`max_concurrent_for_free_ram`]（参数注入断言的落点；8GB 模拟：
//!   `clamp(floor(7 / 1.5) = 4, 2, 10) = 4`）；
//! - 空闲内存采样源 [`FreeRamSource`]（生产 [`SysinfoFreeRam`]，测试替身
//!   [`FixedFreeRam`]；采样失败 → `None` = 保持既有闸门值）；
//! - FIFO 队列载体 [`GateQueue`]（会话级等待槽与全局 FIFO 的两层合成语义见
//!   ADR-022 附录 A-6）；
//! - [`ConcurrencyGateSnapshot`] 与 [`ConcurrencyGateCell`]：`health.concurrency_gate`
//!   的数据源（由 `SessionManager` 单写更新，健康查询同步读取）。
//!
//! 口径：FIFO 位置从 1 计，仅指**全局 FIFO** 内位置（不含未进全局队列的会话等待槽
//! run）；`queued` = 全局 FIFO 长度。

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

/// 闸门快照（`health.concurrency_gate`；ADR-018 附录 A-5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ConcurrencyGateSnapshot {
    /// 当前闸门上限（`clamp(floor(free_ram_gb / 1.5), 2, 10)`；只对新 run 生效）。
    pub limit: usize,
    /// 全局 `running` run 数（在途且已派发）。
    pub active: usize,
    /// 全局 FIFO 长度（闸门排队 run 数）。
    pub queued: usize,
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
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

/// FIFO 等待项（闸门排队 run；占「当前 run 位」但未派发）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateWaiter {
    pub session_id: SessionId,
    pub run_id: RunId,
}

/// 全局 FIFO 队列（单写者；位置从 1 计；不重排）。
#[derive(Debug, Default)]
pub struct GateQueue {
    fifo: VecDeque<GateWaiter>,
}

impl GateQueue {
    pub fn len(&self) -> usize {
        self.fifo.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fifo.is_empty()
    }

    /// FIFO 内位置（1 起；不在队列 → `None`）。
    pub fn position(&self, run_id: &RunId) -> Option<usize> {
        self.fifo
            .iter()
            .position(|waiter| &waiter.run_id == run_id)
            .map(|index| index + 1)
    }

    /// 入队尾（重复入队为不变量破坏；调用方保证 run 唯一）。
    pub fn enqueue(&mut self, session_id: SessionId, run_id: RunId) {
        self.fifo.push_back(GateWaiter { session_id, run_id });
    }

    /// 从队列移除（中断/取消路径；「槽位让给 FIFO 下一项」= 后续位置前移）。
    pub fn remove(&mut self, run_id: &RunId) -> Option<GateWaiter> {
        let index = self
            .fifo
            .iter()
            .position(|waiter| &waiter.run_id == run_id)?;
        self.fifo.remove(index)
    }

    /// 弹出队首（推进；调用方每次仅启动该 run）。
    pub fn pop_front(&mut self) -> Option<GateWaiter> {
        self.fifo.pop_front()
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
    fn gate_queue_positions_are_one_based_and_shift_after_remove() {
        let mut queue = GateQueue::default();
        assert!(queue.is_empty());
        queue.enqueue(session("s1"), run("r1"));
        queue.enqueue(session("s2"), run("r2"));
        queue.enqueue(session("s3"), run("r3"));
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.position(&run("r1")), Some(1));
        assert_eq!(queue.position(&run("r3")), Some(3));
        assert_eq!(queue.position(&run("r9")), None);

        // 「槽位让给 FIFO 下一项」：中段移除后后续位置前移。
        let removed = queue.remove(&run("r2")).expect("移除命中");
        assert_eq!(removed.session_id, session("s2"));
        assert_eq!(queue.position(&run("r3")), Some(2));
        assert_eq!(queue.remove(&run("r2")), None);

        let head = queue.pop_front().expect("队首");
        assert_eq!(head.run_id, run("r1"));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.position(&run("r3")), Some(1));
    }

    #[test]
    fn gate_cell_roundtrip_and_default_is_empty() {
        let cell = ConcurrencyGateCell::new();
        assert_eq!(cell.snapshot(), None);
        let snapshot = ConcurrencyGateSnapshot {
            limit: 4,
            active: 4,
            queued: 2,
        };
        cell.set(snapshot);
        assert_eq!(cell.snapshot(), Some(snapshot));
        // 克隆共享同一单元（管理器写 / health 读）。
        let clone = cell.clone();
        assert_eq!(clone.snapshot(), Some(snapshot));
        let serialized = serde_json::to_value(snapshot).expect("序列化");
        assert_eq!(
            serialized,
            serde_json::json!({ "limit": 4, "active": 4, "queued": 2 })
        );
    }
}
