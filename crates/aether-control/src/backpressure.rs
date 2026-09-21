//! 背压分级与 journal 补读（M2-04；设计 D8、评审 #4/#9；ADR-003/ADR-004）。
//!
//! 控制层职责（`aether-control` 不依赖适配器 crate；隔离经 [`IsolationSink`] 端口，
//! 生产实现由组合层（aether-tauri）桥接到 M1-10 监督器）：
//!
//! - **控制事件投递**（D8）：按适配器的有界内存投递队列；超限（超过 32MB 或超过
//!   会话数×5000 条）时该适配器整体切换 **journal 补读模式**（清空内存积压、从磁盘
//!   重读）——不反压 reader、不影响其他适配器；L3 熔断（拒绝新会话/新 run）并在
//!   [`DELIVERY_RESTART_DELAY_MS`]（30s）后自动解除（重启适配器）并从 journal 恢复投递
//!   （零丢失：补读锚点为「已确认 seq + 1」，消费方经 [`EventPipeline::readback`] 补齐）；
//! - **`Lagged(k)`**：广播 lag 不丢语义——清空该适配器内存积压并切换补读；最终一致；
//! - **存储侧背压例外**（D8/ADR-003/ADR-004，**仅写队列临时高水位**）：写队列 >4096（L2）
//!   时对控制事件读取施加背压——暂停窗口 250ms/次、单次暂停超时 2s；熔断条件 = 60s
//!   滑动窗口累计暂停 >10s 或连续 3 次暂停超时 → 隔离适配器（`degraded +
//!   status_reason=storage_backpressure`）；解除条件 = 写队列回落 ≤1024（L1）且持续 30s
//!   （**仅队列维度**）→ 自动解除并重启适配器；
//! - **`persist_degraded` 严格区分**（ADR-004 决策 1）：持久化降级**不适用**本模块的暂停/
//!   隔离/自动解除路径，冻结为 [`PressurePhase::PersistDegraded`]（D4：修复外部条件 +
//!   重启核心 + 启动自检；P0 无热恢复）；
//! - **L1（写队列 >1024）**：告警由 M1-04 边沿广播交付；delta 合并窗口放宽至 64ms 由
//!   [`crate::pipeline`] 消费 `JournalMetrics::pressure_level` 实现。
//!
//! 线程模型：控制器内部状态经 `Mutex` 保护（临界区不做 I/O）；投递泵与维护 ticker 为
//! 独立 tokio 任务，经 [`BackpressureController::shutdown`] 停止。

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aether_core::{EventEnvelope, RuntimeId, SessionId};
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::clock::SharedClock;
use crate::pipeline::EventPipeline;
use crate::storage_state::StorageState;

/// 控制投递内存上限（D8：32MB ≈ 6.4 万条 × 512B 中位事件）。
pub const DELIVERY_MAX_BYTES: usize = 32 * 1024 * 1024;
/// 控制投递每会话条数上限（D8：每会话 5000 条兜底；与会话数相乘取小生效）。
pub const DELIVERY_PER_SESSION_ITEMS: usize = 5_000;
/// L3 熔断后的自动解除/重启延迟（D8：30s 后重启并从 journal 恢复投递）。
pub const DELIVERY_RESTART_DELAY_MS: i64 = 30_000;
/// L3「持续未回落」判定窗口（D8：60s）。
pub const DELIVERY_NO_FALL_MS: i64 = 60_000;
/// 单次 `poll` 最大批量（帧级批处理；不影响零丢失——未确认事件可经 journal 重读）。
pub const DELIVERY_POLL_BATCH: usize = 256;
/// L1 水位（D8：写队列 >1024 → 告警）。
pub const STORAGE_L1_THRESHOLD: usize = 1_024;
/// L2 水位（D8：写队列 >4096 → 拒绝新 run + 控制读取背压）。
pub const STORAGE_L2_THRESHOLD: usize = 4_096;
/// 控制读取暂停窗口（D8：250ms/次）。
pub const PAUSE_WINDOW_MS: i64 = 250;
/// 单次暂停硬超时（D8：2s；超时即结束本次暂停并计数）。
pub const PAUSE_TIMEOUT_MS: i64 = 2_000;
/// 60s 滑动窗口累计暂停熔断阈值（D8：>10s）。
pub const PAUSE_BUDGET_MS: i64 = 10_000;
/// 暂停累计的滑动窗口长度（D8：60s）。
pub const PAUSE_BUDGET_WINDOW_MS: i64 = 60_000;
/// 连续暂停超时熔断阈值（D8：连续 3 次）。
pub const PAUSE_MAX_CONSECUTIVE_TIMEOUTS: u32 = 3;
/// 隔离解除的队列回落持续时长（D8/ADR-004：≤1024 持续 30s，仅队列维度）。
pub const RELEASE_SUSTAIN_MS: i64 = 30_000;
/// 维护 tick 默认周期（诊断/解除判定的巡检节奏；常量级调参）。
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(100);

/// 背压控制器运行参数（默认值即 D8/ADR-003/ADR-004 约定；测试/故障注入可参数化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackpressureConfig {
    /// 控制投递内存上限（默认 32MB）。
    pub delivery_max_bytes: usize,
    /// 控制投递每会话条数上限（默认 5000）。
    pub delivery_per_session_items: usize,
    /// L3 熔断自动解除延迟（默认 30s）。
    pub delivery_restart_delay_ms: i64,
    /// L3「持续未回落」判定（默认 60s）。
    pub delivery_no_fall_ms: i64,
    /// L1 水位（默认 1024）。
    pub storage_l1_threshold: usize,
    /// L2 水位（默认 4096）。
    pub storage_l2_threshold: usize,
    /// 暂停窗口（默认 250ms）。
    pub pause_window_ms: i64,
    /// 单次暂停超时（默认 2s）。
    pub pause_timeout_ms: i64,
    /// 累计暂停熔断阈值（默认 10s）。
    pub pause_budget_ms: i64,
    /// 累计暂停滑动窗口（默认 60s）。
    pub pause_budget_window_ms: i64,
    /// 连续暂停超时次数（默认 3）。
    pub pause_max_consecutive_timeouts: u32,
    /// 解除隔离的回落持续时长（默认 30s）。
    pub release_sustain_ms: i64,
    /// 投递泵人为减速（**测试/故障注入专用**：制造广播 lag 与投递积压；默认 0）。
    pub ingest_delay: Duration,
    /// 维护 tick 周期（默认 100ms）。
    pub tick_interval: Duration,
}

impl Default for BackpressureConfig {
    fn default() -> Self {
        Self {
            delivery_max_bytes: DELIVERY_MAX_BYTES,
            delivery_per_session_items: DELIVERY_PER_SESSION_ITEMS,
            delivery_restart_delay_ms: DELIVERY_RESTART_DELAY_MS,
            delivery_no_fall_ms: DELIVERY_NO_FALL_MS,
            storage_l1_threshold: STORAGE_L1_THRESHOLD,
            storage_l2_threshold: STORAGE_L2_THRESHOLD,
            pause_window_ms: PAUSE_WINDOW_MS,
            pause_timeout_ms: PAUSE_TIMEOUT_MS,
            pause_budget_ms: PAUSE_BUDGET_MS,
            pause_budget_window_ms: PAUSE_BUDGET_WINDOW_MS,
            pause_max_consecutive_timeouts: PAUSE_MAX_CONSECUTIVE_TIMEOUTS,
            release_sustain_ms: RELEASE_SUSTAIN_MS,
            ingest_delay: Duration::ZERO,
            tick_interval: DEFAULT_TICK_INTERVAL,
        }
    }
}

impl BackpressureConfig {
    /// 校验参数（容量/阈值/时长必须为正；L1 ≤ L2；窗口 ≤ 超时）。
    pub fn validate(&self) -> Result<(), BackpressureError> {
        let invalid = |reason: &str| {
            Err(BackpressureError::InvalidConfig {
                reason: reason.to_owned(),
            })
        };
        if self.delivery_max_bytes == 0 {
            return invalid("delivery_max_bytes 必须 >0");
        }
        if self.delivery_per_session_items == 0 {
            return invalid("delivery_per_session_items 必须 >0");
        }
        if self.delivery_restart_delay_ms <= 0 {
            return invalid("delivery_restart_delay_ms 必须 >0");
        }
        if self.delivery_no_fall_ms <= 0 {
            return invalid("delivery_no_fall_ms 必须 >0");
        }
        if self.storage_l1_threshold > self.storage_l2_threshold {
            return invalid("storage_l1_threshold 不得大于 storage_l2_threshold");
        }
        if self.pause_window_ms <= 0 || self.pause_timeout_ms <= 0 {
            return invalid("pause_window_ms / pause_timeout_ms 必须 >0");
        }
        if self.pause_window_ms > self.pause_timeout_ms {
            return invalid("pause_window_ms 不得大于 pause_timeout_ms");
        }
        if self.pause_budget_ms <= 0 || self.pause_budget_window_ms <= 0 {
            return invalid("pause_budget_ms / pause_budget_window_ms 必须 >0");
        }
        if self.pause_max_consecutive_timeouts == 0 {
            return invalid("pause_max_consecutive_timeouts 必须 >0");
        }
        if self.release_sustain_ms <= 0 {
            return invalid("release_sustain_ms 必须 >0");
        }
        if self.tick_interval.is_zero() {
            return invalid("tick_interval 必须 >0");
        }
        Ok(())
    }
}

/// 隔离原因（D8 熔断两类来源；`status_reason` 词典取值见 D5/ADR-003）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationReason {
    /// 写队列临时高水位熔断（`degraded + status_reason=storage_backpressure`）。
    StorageBackpressure,
    /// 控制投递积压熔断（D8 L3；detail 区分 `delivery_backlog`）。
    DeliveryBacklog,
}

impl IsolationReason {
    /// `status_reason` 词典取值（D5；`DeliveryBacklog` 归入 `storage_backpressure` 语义域）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StorageBackpressure => "storage_backpressure",
            Self::DeliveryBacklog => "storage_backpressure",
        }
    }

    /// 诊断细节代码（区分两类熔断来源）。
    pub const fn detail_code(self) -> &'static str {
        match self {
            Self::StorageBackpressure => "storage_backpressure",
            Self::DeliveryBacklog => "delivery_backlog",
        }
    }
}

/// 适配器隔离端口（D8 熔断动作；生产实现由组合层桥接到 M1-10 监督器）。
///
/// 契约：
/// - [`IsolationSink::isolate`]：适配器进入 `degraded + status_reason`（生产实现应终止
///   进程以停止事件生产；`ready`/`starting` 之外的状态返回 `false`）；
/// - [`IsolationSink::release`]：解除隔离并重启（`degraded → starting → ready`）；返回
///   `true` 表示已解除/无需解除，`false` 表示当前无法解除（控制层将按 30s 重试）。
pub trait IsolationSink: Send + Sync + 'static {
    /// 隔离适配器；返回是否完成转移。
    fn isolate(&self, runtime_id: &RuntimeId, reason: IsolationReason, detail: &str) -> bool;
    /// 解除隔离并重启；返回是否已解除/无需解除。
    fn release(&self, runtime_id: &RuntimeId) -> bool;
}

/// 空隔离出口（不需要隔离时的默认实现；不改变控制层熔断判定）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopIsolationSink;

impl IsolationSink for NoopIsolationSink {
    fn isolate(&self, _runtime_id: &RuntimeId, _reason: IsolationReason, _detail: &str) -> bool {
        false
    }

    fn release(&self, _runtime_id: &RuntimeId) -> bool {
        true
    }
}

/// 背压错误（准入拒绝；`code()` 为稳定错误码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackpressureError {
    /// 适配器熔断/隔离中：拒绝新会话/新 run（错误码 `storage_backpressure`）。
    CircuitOpen {
        runtime_id: RuntimeId,
        reason: IsolationReason,
        since_ms: i64,
    },
    /// 持久化降级（D4）：与背压解除路径严格区分（修复 + 重启 + 自检）。
    PersistDegraded { reason: String },
    /// 配置非法。
    InvalidConfig { reason: String },
}

impl BackpressureError {
    /// 稳定错误码。
    pub const fn code(&self) -> &'static str {
        match self {
            Self::CircuitOpen { .. } => "storage_backpressure",
            Self::PersistDegraded { .. } => "persist_degraded",
            Self::InvalidConfig { .. } => "invalid_backpressure_config",
        }
    }
}

impl std::fmt::Display for BackpressureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CircuitOpen {
                runtime_id,
                reason,
                since_ms,
            } => write!(
                f,
                "适配器背压熔断（storage_backpressure）：{runtime_id} 于 {since_ms} 隔离\
                 （来源 {}，{}）",
                reason.detail_code(),
                if *reason == IsolationReason::StorageBackpressure {
                    "写队列临时高水位"
                } else {
                    "控制投递积压"
                }
            ),
            Self::PersistDegraded { reason } => write!(
                f,
                "存储降级（persist_degraded）：拒绝新写入/新 run；{reason}；\
                 修复外部条件后重启核心并以启动自检恢复（P0 无热恢复）"
            ),
            Self::InvalidConfig { reason } => write!(f, "背压配置非法: {reason}"),
        }
    }
}

impl std::error::Error for BackpressureError {}

/// 存储侧背压阶段（诊断/断言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PressurePhase {
    /// 正常（含 L1 告警电平与 L1–L2 滞后区未暂停）。
    #[default]
    Normal,
    /// 控制读取暂停中（写队列 >L2）。
    Paused,
    /// 已隔离（`degraded + status_reason=storage_backpressure`），等待队列回落解除。
    Isolated,
    /// 持久化降级：本模块不暂停/不隔离/不自动解除（D4 路径）。
    PersistDegraded,
}

impl PressurePhase {
    /// 诊断代码。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Paused => "paused",
            Self::Isolated => "isolated",
            Self::PersistDegraded => "persist_degraded",
        }
    }
}

/// 控制读取闸门（适配器宿主每次读取 stdout 前查询；D8）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlReadGate {
    /// 可读。
    Open,
    /// 暂停读取（写队列临时高水位）；`remaining_ms` 不超过单次暂停超时（默认 2s）。
    Paused { remaining_ms: i64 },
}

/// 投递轮询结果（D8：内存队列 → journal 补读）。
#[derive(Debug, Clone, PartialEq)]
pub enum DeliveryPoll {
    /// 无待投递内容。
    Empty,
    /// 内存队列批次（升序；消费后调用 [`BackpressureController::ack`] 推进游标）。
    Events(Vec<EventEnvelope>),
    /// 该适配器已切换 journal 补读模式：消费方须经
    /// [`EventPipeline::readback`] 从 `from_seq - 1` 起补齐，再调用
    /// [`BackpressureController::ack_readback`]。
    ReadbackRequired {
        session_id: SessionId,
        from_seq: u64,
    },
}

/// 背压诊断快照（诊断包/证据断言）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BackpressureMetrics {
    /// 全适配器内存投递字节数。
    pub delivery_bytes: usize,
    /// 全适配器内存投递事件数。
    pub delivery_events: usize,
    /// 控制投递超限（>32MB / >会话数×5000）次数。
    pub delivery_overflows: u64,
    /// 补读模式持续 60s 未回落触发次数（L3 第二条件）。
    pub delivery_no_falls: u64,
    /// 广播 `Lagged(k)` 丢弃的 k 累计。
    pub delivery_lagged_events: u64,
    /// 当前熔断打开的适配器数。
    pub delivery_circuits_open: usize,
    /// L3 熔断自动解除（重启）次数。
    pub delivery_restarts: u64,
    /// 控制读取暂停开始次数。
    pub pause_starts: u64,
    /// 单次暂停超时次数。
    pub pause_timeouts: u64,
    /// 存储侧背压隔离次数。
    pub storage_isolations: u64,
    /// 隔离自动解除次数。
    pub storage_releases: u64,
    /// 当前持久化降级（D4 事实源透传）。
    pub persist_degraded: bool,
}

/// 投递状态（按适配器）。
#[derive(Debug, Default)]
struct RuntimeDelivery {
    /// 有界内存投递队列（FIFO）。
    queue: VecDeque<EventEnvelope>,
    /// 队列字节数（按 JSON 序列化估算）。
    bytes: usize,
    /// 已知会话（`会话数 × 5000` 上限的会话数来源）。
    sessions: BTreeSet<SessionId>,
    /// 会话 → 已确认（投递/补读）的最大 seq。
    acked: BTreeMap<SessionId, u64>,
    /// 会话 → 已摄入的最大 seq（超限/`Lagged` 时判定未确认缺口）。
    max_seen: BTreeMap<SessionId, u64>,
    /// 会话 → 需补读的起始 seq（非空 = 补读模式）。
    readback_from: BTreeMap<SessionId, u64>,
    /// 补读模式开始时间（L3「持续 60s 未回落」）。
    readback_since_ms: Option<i64>,
    /// 熔断状态（None = 未熔断）。
    circuit: Option<CircuitState>,
}

/// L3 熔断状态。
#[derive(Debug, Clone)]
struct CircuitState {
    reason: IsolationReason,
    since_ms: i64,
    /// 自动解除时刻（D8：30s 后重启）。
    release_at_ms: i64,
    detail: String,
}

/// 存储侧背压状态（按适配器）。
#[derive(Debug, Default)]
struct StoragePressure {
    phase: PressurePhase,
    /// 连续暂停起点（None = 当前未暂停）。
    pause_started_ms: Option<i64>,
    /// 当前 250ms 记账窗口起点。
    window_started_ms: Option<i64>,
    /// 滑动窗口内已记账的暂停区间 `(start, end)`。
    pause_intervals: VecDeque<(i64, i64)>,
    /// 连续暂停超时次数（队列回落 ≤L1 时清零）。
    consecutive_timeouts: u32,
    /// 队列 ≤L1 的持续起点（解除条件）。
    low_since_ms: Option<i64>,
    /// 隔离起点（诊断）。
    isolated_since_ms: Option<i64>,
}

/// 按适配器的运行时状态。
#[derive(Debug, Default)]
struct RuntimeState {
    delivery: RuntimeDelivery,
    pressure: StoragePressure,
}

/// 累计计数器。
#[derive(Debug, Default, Clone, Copy)]
struct CounterSet {
    delivery_overflows: u64,
    delivery_no_falls: u64,
    delivery_lagged_events: u64,
    delivery_restarts: u64,
    pause_starts: u64,
    pause_timeouts: u64,
    storage_isolations: u64,
    storage_releases: u64,
}

/// 控制器状态（单一互斥保护；临界区不做 I/O）。
#[derive(Debug)]
struct ControllerState {
    runtimes: HashMap<RuntimeId, RuntimeState>,
    last_depth: usize,
    last_storage_state: StorageState,
    counters: CounterSet,
}

impl Default for ControllerState {
    fn default() -> Self {
        Self {
            runtimes: HashMap::new(),
            last_depth: 0,
            last_storage_state: StorageState::Normal,
            counters: CounterSet::default(),
        }
    }
}

/// 存储观测（tick / 故障注入入口）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Observation {
    now_ms: i64,
    queue_depth: usize,
    storage_state: StorageState,
}

/// 待执行的隔离端口动作（锁外执行）。
#[derive(Debug, Clone)]
enum SinkAction {
    Isolate {
        runtime_id: RuntimeId,
        reason: IsolationReason,
        detail: String,
    },
    Release {
        runtime_id: RuntimeId,
        reason: IsolationReason,
        detail: String,
    },
}

/// 背压控制器（D8；克隆共享同一实例）。
///
/// 生产启动：`BackpressureController::start(config, clock, sink, &pipeline, &handle)` 启动
/// 投递泵；`spawn_ticker` 启动维护巡检；`shutdown` 停止后台任务。
pub struct BackpressureController {
    config: Arc<BackpressureConfig>,
    clock: SharedClock,
    sink: Arc<dyn IsolationSink>,
    pipeline: Option<EventPipeline>,
    state: Mutex<ControllerState>,
    pump: Mutex<Option<JoinHandle<()>>>,
    ticker: Mutex<Option<JoinHandle<()>>>,
}

impl BackpressureController {
    /// 构造控制器（不订阅管线；测试/单元故障注入用 [`BackpressureController::observe`]）。
    pub fn new(
        config: BackpressureConfig,
        clock: SharedClock,
        sink: Arc<dyn IsolationSink>,
    ) -> Result<Arc<Self>, BackpressureError> {
        config.validate()?;
        Ok(Arc::new(Self {
            config: Arc::new(config),
            clock,
            sink,
            pipeline: None,
            state: Mutex::new(ControllerState::default()),
            pump: Mutex::new(None),
            ticker: Mutex::new(None),
        }))
    }

    /// 构造并启动投递泵（订阅 `pipeline` 落盘后广播；`Lagged` → 补读模式）。
    pub fn start(
        config: BackpressureConfig,
        clock: SharedClock,
        sink: Arc<dyn IsolationSink>,
        pipeline: &EventPipeline,
        handle: &Handle,
    ) -> Result<Arc<Self>, BackpressureError> {
        config.validate()?;
        let controller = Arc::new(Self {
            config: Arc::new(config),
            clock,
            sink,
            pipeline: Some(pipeline.clone()),
            state: Mutex::new(ControllerState::default()),
            pump: Mutex::new(None),
            ticker: Mutex::new(None),
        });
        let mut subscription = pipeline.subscribe();
        let pump_controller = Arc::clone(&controller);
        let ingest_delay = controller.config.ingest_delay;
        let pump = handle.spawn(async move {
            loop {
                match subscription.recv().await {
                    Ok(envelope) => {
                        if !ingest_delay.is_zero() {
                            tokio::time::sleep(ingest_delay).await;
                        }
                        pump_controller.ingest(envelope);
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        pump_controller.handle_lagged(skipped);
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
        *lock_slot(&controller.pump) = Some(pump);
        Ok(controller)
    }

    /// 启动维护巡检（周期 [`BackpressureConfig::tick_interval`]；读管线健康推进状态机）。
    ///
    /// 返回本控制器持有的任务数（诊断）。
    pub fn spawn_ticker(self: &Arc<Self>, handle: &Handle) -> usize {
        let ticker_controller = Arc::clone(self);
        let interval = self.config.tick_interval;
        let ticker = handle.spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                ticker_controller.tick();
            }
        });
        *lock_slot(&self.ticker) = Some(ticker);
        usize::from(self.pipeline.is_some())
    }

    /// 停止后台任务（投递泵 + 维护 ticker）。
    pub async fn shutdown(&self) {
        let pump = lock_slot(&self.pump).take();
        if let Some(pump) = pump {
            pump.abort();
            let _ = pump.await;
        }
        let ticker = lock_slot(&self.ticker).take();
        if let Some(ticker) = ticker {
            ticker.abort();
            let _ = ticker.await;
        }
    }

    /// 注册会话（`会话数 × 5000` 上限的会话数来源；`session.create` 时调用）。
    pub fn register_session(&self, runtime_id: &RuntimeId, session_id: &SessionId) {
        let mut guard = self.lock_state();
        let runtime = guard.runtimes.entry(runtime_id.clone()).or_default();
        runtime.delivery.sessions.insert(session_id.clone());
    }

    /// 新会话/新 run 准入（D8 L2/L3 + D4 降级透传）。
    pub fn admission(&self, runtime_id: &RuntimeId) -> Result<(), BackpressureError> {
        let guard = self.lock_state();
        if guard.last_storage_state.is_degraded() {
            return Err(BackpressureError::PersistDegraded {
                reason: "存储只读（persist_degraded）：拒绝新会话/新 run（D4）".to_owned(),
            });
        }
        let Some(runtime) = guard.runtimes.get(runtime_id) else {
            return Ok(());
        };
        if let Some(circuit) = &runtime.delivery.circuit {
            return Err(BackpressureError::CircuitOpen {
                runtime_id: runtime_id.clone(),
                reason: circuit.reason,
                since_ms: circuit.since_ms,
            });
        }
        if runtime.pressure.phase == PressurePhase::Isolated {
            return Err(BackpressureError::CircuitOpen {
                runtime_id: runtime_id.clone(),
                reason: IsolationReason::StorageBackpressure,
                since_ms: runtime.pressure.isolated_since_ms.unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// 控制读取闸门（适配器宿主每次读取 stdout 前查询；暂停中返回剩余窗口）。
    pub fn control_read_gate(&self, runtime_id: &RuntimeId) -> ControlReadGate {
        let guard = self.lock_state();
        let Some(runtime) = guard.runtimes.get(runtime_id) else {
            return ControlReadGate::Open;
        };
        if runtime.pressure.phase != PressurePhase::Paused {
            return ControlReadGate::Open;
        }
        let now = self.clock.now_ms();
        let remaining = runtime
            .pressure
            .pause_started_ms
            .map(|started| (self.config.pause_timeout_ms - (now - started)).max(0))
            .unwrap_or(0);
        ControlReadGate::Paused {
            remaining_ms: remaining,
        }
    }

    /// 轮询该适配器待投递内容（补读模式优先）。
    pub fn poll(&self, runtime_id: &RuntimeId) -> DeliveryPoll {
        let mut guard = self.lock_state();
        let Some(runtime) = guard.runtimes.get_mut(runtime_id) else {
            return DeliveryPoll::Empty;
        };
        if let Some((session_id, from_seq)) = runtime.delivery.readback_from.iter().next() {
            return DeliveryPoll::ReadbackRequired {
                session_id: session_id.clone(),
                from_seq: *from_seq,
            };
        }
        if runtime.delivery.queue.is_empty() {
            return DeliveryPoll::Empty;
        }
        let take = runtime.delivery.queue.len().min(DELIVERY_POLL_BATCH);
        let mut events = Vec::with_capacity(take);
        for _ in 0..take {
            if let Some(event) = runtime.delivery.queue.pop_front() {
                runtime.delivery.bytes = runtime
                    .delivery
                    .bytes
                    .saturating_sub(envelope_bytes(&event));
                events.push(event);
            }
        }
        DeliveryPoll::Events(events)
    }

    /// 确认内存投递（推进会话游标；用于补读锚点与诊断）。
    pub fn ack(&self, runtime_id: &RuntimeId, session_id: &SessionId, up_to_seq: u64) {
        let mut guard = self.lock_state();
        let Some(runtime) = guard.runtimes.get_mut(runtime_id) else {
            return;
        };
        let acked = runtime
            .delivery
            .acked
            .entry(session_id.clone())
            .or_insert(0);
        *acked = (*acked).max(up_to_seq);
        drop_acked(&mut runtime.delivery, session_id, up_to_seq);
    }

    /// 确认补读完成（消费方已从 journal 读到 `up_to_seq`）：退出该会话补读模式。
    pub fn ack_readback(&self, runtime_id: &RuntimeId, session_id: &SessionId, up_to_seq: u64) {
        let mut guard = self.lock_state();
        let Some(runtime) = guard.runtimes.get_mut(runtime_id) else {
            return;
        };
        runtime.delivery.readback_from.remove(session_id);
        let acked = runtime
            .delivery
            .acked
            .entry(session_id.clone())
            .or_insert(0);
        *acked = (*acked).max(up_to_seq);
        drop_acked(&mut runtime.delivery, session_id, up_to_seq);
        if runtime.delivery.readback_from.is_empty() {
            runtime.delivery.readback_since_ms = None;
        }
    }

    /// 维护 tick：读取管线健康（写队列深度 + 存储状态）推进背压状态机。
    pub fn tick(&self) {
        let Some(pipeline) = &self.pipeline else {
            return;
        };
        let health = pipeline.health();
        self.observe(
            self.clock.now_ms(),
            health.journal_queue_depth,
            health.storage_state,
        );
    }

    /// 故障注入/测试入口：以显式观测推进状态机（生产经 [`BackpressureController::tick`]）。
    pub fn observe(&self, now_ms: i64, queue_depth: usize, storage_state: StorageState) {
        let observation = Observation {
            now_ms,
            queue_depth,
            storage_state,
        };
        let mut actions = Vec::new();
        {
            let mut guard = self.lock_state();
            let state = &mut *guard;
            state.last_depth = queue_depth;
            state.last_storage_state = storage_state;
            let ControllerState {
                runtimes, counters, ..
            } = state;
            for (runtime_id, runtime) in runtimes.iter_mut() {
                if let Some(action) =
                    storage_observe(runtime, &self.config, runtime_id, observation, counters)
                {
                    actions.push(action);
                }
                if let Some(action) =
                    delivery_no_fall_check(runtime, &self.config, runtime_id, observation, counters)
                {
                    actions.push(action);
                }
                if let Some(action) = delivery_release_check(runtime, runtime_id, observation) {
                    actions.push(action);
                }
            }
        }
        self.apply_actions(actions);
    }

    /// 诊断快照。
    pub fn metrics(&self) -> BackpressureMetrics {
        let guard = self.lock_state();
        let mut delivery_bytes = 0usize;
        let mut delivery_events = 0usize;
        let mut circuits_open = 0usize;
        for runtime in guard.runtimes.values() {
            delivery_bytes += runtime.delivery.bytes;
            delivery_events += runtime.delivery.queue.len();
            if runtime.delivery.circuit.is_some() {
                circuits_open += 1;
            }
        }
        BackpressureMetrics {
            delivery_bytes,
            delivery_events,
            delivery_overflows: guard.counters.delivery_overflows,
            delivery_no_falls: guard.counters.delivery_no_falls,
            delivery_lagged_events: guard.counters.delivery_lagged_events,
            delivery_circuits_open: circuits_open,
            delivery_restarts: guard.counters.delivery_restarts,
            pause_starts: guard.counters.pause_starts,
            pause_timeouts: guard.counters.pause_timeouts,
            storage_isolations: guard.counters.storage_isolations,
            storage_releases: guard.counters.storage_releases,
            persist_degraded: guard.last_storage_state.is_degraded(),
        }
    }

    /// 指定适配器的存储背压阶段（诊断/断言）。
    pub fn phase(&self, runtime_id: &RuntimeId) -> PressurePhase {
        let guard = self.lock_state();
        guard
            .runtimes
            .get(runtime_id)
            .map(|runtime| runtime.pressure.phase)
            .unwrap_or(PressurePhase::Normal)
    }

    // ===== 内部 =====

    fn ingest(&self, envelope: EventEnvelope) {
        let now = self.clock.now_ms();
        let runtime_id = envelope.runtime_id.clone();
        let session_id = envelope.session_id.clone();
        let seq = envelope.seq;
        let bytes = envelope_bytes(&envelope);
        let mut actions = Vec::new();
        {
            let mut guard = self.lock_state();
            let state = &mut *guard;
            let ControllerState {
                runtimes, counters, ..
            } = state;
            let runtime = runtimes.entry(runtime_id.clone()).or_default();
            runtime.delivery.sessions.insert(session_id.clone());
            let seen = runtime
                .delivery
                .max_seen
                .entry(session_id.clone())
                .or_insert(0);
            *seen = (*seen).max(seq);
            runtime.delivery.queue.push_back(envelope);
            runtime.delivery.bytes = runtime.delivery.bytes.saturating_add(bytes);
            let session_count = runtime.delivery.sessions.len().max(1);
            let item_limit = session_count.saturating_mul(self.config.delivery_per_session_items);
            let over = runtime.delivery.bytes > self.config.delivery_max_bytes
                || runtime.delivery.queue.len() > item_limit;
            if !over {
                return;
            }
            counters.delivery_overflows += 1;
            let entered = enter_readback(runtime, now);
            if entered && runtime.delivery.circuit.is_none() {
                let detail = format!(
                    "控制投递积压超限（{} 字节 / {} 条，会话 {session_count}，条数上限 {item_limit}）：\
                     切换 journal 补读模式并熔断，{}ms 后自动重启",
                    runtime.delivery.bytes,
                    runtime.delivery.queue.len(),
                    self.config.delivery_restart_delay_ms
                );
                runtime.delivery.circuit = Some(CircuitState {
                    reason: IsolationReason::DeliveryBacklog,
                    since_ms: now,
                    release_at_ms: now + self.config.delivery_restart_delay_ms,
                    detail: detail.clone(),
                });
                actions.push(SinkAction::Isolate {
                    runtime_id: runtime_id.clone(),
                    reason: IsolationReason::DeliveryBacklog,
                    detail,
                });
            }
        }
        self.apply_actions(actions);
    }

    fn handle_lagged(&self, skipped: u64) {
        let now = self.clock.now_ms();
        let mut guard = self.lock_state();
        let state = &mut *guard;
        state.counters.delivery_lagged_events = state
            .counters
            .delivery_lagged_events
            .saturating_add(skipped);
        for runtime in state.runtimes.values_mut() {
            // 广播 lag：内存积压可能已不连续 → 清空并切换补读（最终一致）。
            let _ = enter_readback(runtime, now);
        }
    }

    fn apply_actions(&self, actions: Vec<SinkAction>) {
        for action in actions {
            match action {
                SinkAction::Isolate {
                    runtime_id,
                    reason,
                    detail,
                } => {
                    let _ = self.sink.isolate(&runtime_id, reason, &detail);
                }
                SinkAction::Release {
                    runtime_id,
                    reason,
                    detail,
                } => {
                    let released = self.sink.release(&runtime_id);
                    let now = self.clock.now_ms();
                    let mut guard = self.lock_state();
                    let state = &mut *guard;
                    let ControllerState {
                        runtimes, counters, ..
                    } = state;
                    if let Some(runtime) = runtimes.get_mut(&runtime_id) {
                        if released {
                            counters.delivery_restarts += 1;
                        } else if runtime.delivery.circuit.is_none() {
                            // 端口未能解除：按 30s 重试（保持熔断语义）。
                            runtime.delivery.circuit = Some(CircuitState {
                                reason,
                                since_ms: now,
                                release_at_ms: now + self.config.delivery_restart_delay_ms,
                                detail,
                            });
                        }
                    }
                }
            }
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, ControllerState> {
        match self.state.lock() {
            Ok(guard) => guard,
            // 互斥锁中毒：状态本身仍有效，继续使用不 panic 逃逸。
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// 中毒容忍的槽位加锁（任务句柄/诊断槽）。
fn lock_slot<T>(slot: &Mutex<Option<T>>) -> MutexGuard<'_, Option<T>> {
    match slot.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 事件近似字节数（按 JSON 序列化；失败按 0 计，不影响正确性）。
fn envelope_bytes(event: &EventEnvelope) -> usize {
    serde_json::to_vec(event).map_or(0, |bytes| bytes.len())
}

/// 切换补读模式：清空内存积压，未确认会话的补读锚点 = 已确认 seq + 1。
///
/// 返回是否本次新进入（已有未确认缺口且此前不在补读模式）。
fn enter_readback(runtime: &mut RuntimeState, now: i64) -> bool {
    let delivery = &mut runtime.delivery;
    let was_empty = delivery.readback_from.is_empty();
    for (session_id, max_seen) in &delivery.max_seen {
        let acked = delivery.acked.get(session_id).copied().unwrap_or(0);
        if *max_seen > acked {
            delivery
                .readback_from
                .entry(session_id.clone())
                .or_insert(acked + 1);
        }
    }
    delivery.queue.clear();
    delivery.bytes = 0;
    let entered = was_empty && !delivery.readback_from.is_empty();
    if entered {
        delivery.readback_since_ms = Some(now);
    }
    entered
}

/// 丢弃队列中该会话 ≤ `up_to_seq` 的事件（避免补读后重复投递）。
fn drop_acked(delivery: &mut RuntimeDelivery, session_id: &SessionId, up_to_seq: u64) {
    let mut removed = 0usize;
    delivery.queue.retain(|event| {
        let drop = event.session_id == *session_id && event.seq <= up_to_seq;
        if drop {
            removed += 1;
        }
        !drop
    });
    if removed > 0 {
        // 字节数按当前队列重算（事件大小不保留，重算成本可控）。
        delivery.bytes = delivery
            .queue
            .iter()
            .map(envelope_bytes)
            .fold(0usize, usize::saturating_add);
    }
}

/// 存储侧背压状态推进（D8/ADR-003/ADR-004）。
fn storage_observe(
    runtime: &mut RuntimeState,
    config: &BackpressureConfig,
    runtime_id: &RuntimeId,
    observation: Observation,
    counters: &mut CounterSet,
) -> Option<SinkAction> {
    let pressure = &mut runtime.pressure;
    if observation.storage_state.is_degraded() {
        // ADR-004 决策 1：persist_degraded 不属于本例外（不暂停/不隔离/不自动解除）。
        pressure.phase = PressurePhase::PersistDegraded;
        pressure.pause_started_ms = None;
        pressure.window_started_ms = None;
        pressure.low_since_ms = None;
        pressure.consecutive_timeouts = 0;
        pressure.pause_intervals.clear();
        return None;
    }
    if pressure.phase == PressurePhase::PersistDegraded {
        // 运行期无热恢复：冻结至新进程（启动自检重建状态）。
        return None;
    }

    let now = observation.now_ms;
    let depth = observation.queue_depth;

    if pressure.phase == PressurePhase::Isolated {
        // 解除条件 = 写队列回落 ≤L1 且持续 30s（仅队列维度）。
        if depth <= config.storage_l1_threshold {
            let low_since = *pressure.low_since_ms.get_or_insert(now);
            if now - low_since >= config.release_sustain_ms {
                pressure.phase = PressurePhase::Normal;
                pressure.isolated_since_ms = None;
                pressure.low_since_ms = None;
                pressure.consecutive_timeouts = 0;
                pressure.pause_intervals.clear();
                counters.storage_releases += 1;
                return Some(SinkAction::Release {
                    runtime_id: runtime_id.clone(),
                    reason: IsolationReason::StorageBackpressure,
                    detail: format!(
                        "写队列回落 ≤{} 持续 {}ms：自动解除隔离并重启适配器（D8/ADR-004）",
                        config.storage_l1_threshold, config.release_sustain_ms
                    ),
                });
            }
        } else {
            pressure.low_since_ms = None;
        }
        return None;
    }

    if depth > config.storage_l2_threshold {
        pressure.low_since_ms = None;
        match pressure.pause_started_ms {
            None => {
                // 开始一次新的暂停（连续超时计数保留到队列回落 ≤L1）。
                pressure.pause_started_ms = Some(now);
                pressure.window_started_ms = Some(now);
                pressure.phase = PressurePhase::Paused;
                counters.pause_starts += 1;
                return None;
            }
            Some(started) => {
                // 记账 250ms 窗口（仅用于累计预算统计）。
                while let Some(window_start) = pressure.window_started_ms {
                    if now - window_start < config.pause_window_ms {
                        break;
                    }
                    pressure.pause_intervals.push_back((
                        window_start,
                        window_start.saturating_add(config.pause_window_ms),
                    ));
                    pressure.window_started_ms = Some(window_start + config.pause_window_ms);
                }
                prune_intervals(pressure, now, config.pause_budget_window_ms);
                let accumulated = accumulated_pause_ms(pressure);
                if now - started >= config.pause_timeout_ms {
                    // 单次暂停超时：结束本次暂停并计数（下一次 tick 重新开始新暂停）。
                    counters.pause_timeouts += 1;
                    pressure.consecutive_timeouts += 1;
                    pressure.pause_started_ms = None;
                    pressure.window_started_ms = None;
                    if pressure.consecutive_timeouts >= config.pause_max_consecutive_timeouts
                        || accumulated > config.pause_budget_ms
                    {
                        return isolate_storage(pressure, config, runtime_id, counters, now);
                    }
                    pressure.phase = PressurePhase::Normal;
                } else if accumulated > config.pause_budget_ms {
                    return isolate_storage(pressure, config, runtime_id, counters, now);
                }
            }
        }
    } else if depth <= config.storage_l1_threshold {
        // 回落：结束暂停并重置连续超时（解除路径仅队列维度）。
        pressure.pause_started_ms = None;
        pressure.window_started_ms = None;
        pressure.consecutive_timeouts = 0;
        if pressure.phase == PressurePhase::Paused {
            pressure.phase = PressurePhase::Normal;
        }
    }
    None
}

/// 隔离（存储侧背压熔断；`degraded + status_reason=storage_backpressure`）。
fn isolate_storage(
    pressure: &mut StoragePressure,
    config: &BackpressureConfig,
    runtime_id: &RuntimeId,
    counters: &mut CounterSet,
    now: i64,
) -> Option<SinkAction> {
    if pressure.phase == PressurePhase::Isolated {
        return None;
    }
    let accumulated = accumulated_pause_ms(pressure);
    let detail = format!(
        "写队列临时高水位：{}ms 滑动窗口累计暂停 {}ms / 连续超时 {} 次 → 隔离\
         （degraded + status_reason=storage_backpressure；{}ms 后按队列回落解除）",
        config.pause_budget_window_ms,
        accumulated,
        pressure.consecutive_timeouts,
        config.release_sustain_ms
    );
    pressure.phase = PressurePhase::Isolated;
    pressure.isolated_since_ms = Some(now);
    pressure.pause_started_ms = None;
    pressure.window_started_ms = None;
    pressure.low_since_ms = None;
    counters.storage_isolations += 1;
    Some(SinkAction::Isolate {
        runtime_id: runtime_id.clone(),
        reason: IsolationReason::StorageBackpressure,
        detail,
    })
}

/// 清理滑动窗口之外的暂停区间。
fn prune_intervals(pressure: &mut StoragePressure, now: i64, window_ms: i64) {
    let horizon = now - window_ms;
    while let Some((_, end)) = pressure.pause_intervals.front() {
        if *end <= horizon {
            pressure.pause_intervals.pop_front();
        } else {
            break;
        }
    }
}

/// 滑动窗口内累计暂停毫秒数。
fn accumulated_pause_ms(pressure: &StoragePressure) -> i64 {
    pressure
        .pause_intervals
        .iter()
        .map(|(start, end)| end - start)
        .fold(0i64, i64::saturating_add)
}

/// L3「持续 60s 未回落」判定：补读模式未恢复 → 熔断。
fn delivery_no_fall_check(
    runtime: &mut RuntimeState,
    config: &BackpressureConfig,
    runtime_id: &RuntimeId,
    observation: Observation,
    counters: &mut CounterSet,
) -> Option<SinkAction> {
    let delivery = &mut runtime.delivery;
    if delivery.circuit.is_some() {
        return None;
    }
    let since = delivery.readback_since_ms?;
    if observation.now_ms - since < config.delivery_no_fall_ms {
        return None;
    }
    let detail = format!(
        "控制投递补读模式持续 {}ms 未回落（>{}ms）：熔断适配器，{}ms 后自动重启并从 journal 恢复",
        observation.now_ms - since,
        config.delivery_no_fall_ms,
        config.delivery_restart_delay_ms
    );
    delivery.circuit = Some(CircuitState {
        reason: IsolationReason::DeliveryBacklog,
        since_ms: observation.now_ms,
        release_at_ms: observation.now_ms + config.delivery_restart_delay_ms,
        detail: detail.clone(),
    });
    counters.delivery_no_falls += 1;
    counters.delivery_overflows += 1;
    Some(SinkAction::Isolate {
        runtime_id: runtime_id.clone(),
        reason: IsolationReason::DeliveryBacklog,
        detail,
    })
}

/// L3 熔断到期 → 自动解除（重启适配器；投递经 journal 补读恢复）。
fn delivery_release_check(
    runtime: &mut RuntimeState,
    runtime_id: &RuntimeId,
    observation: Observation,
) -> Option<SinkAction> {
    let circuit = runtime.delivery.circuit.as_ref()?;
    if observation.now_ms < circuit.release_at_ms {
        return None;
    }
    let reason = circuit.reason;
    let detail = circuit.detail.clone();
    runtime.delivery.circuit = None;
    Some(SinkAction::Release {
        runtime_id: runtime_id.clone(),
        reason,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::clock::{Clock, ManualClock};
    use aether_core::{EventId, EventPayload, LogLevel, LogPayload, EVENT_ENVELOPE_VERSION};

    fn runtime_id() -> RuntimeId {
        RuntimeId::new("mock").expect("runtime id")
    }

    fn session_id() -> SessionId {
        SessionId::new("01J0000000000000000000000A").expect("session id")
    }

    fn log_envelope(id: &str, seq: u64, message: &str) -> EventEnvelope {
        EventEnvelope {
            v: EVENT_ENVELOPE_VERSION,
            id: EventId::new(id).expect("event id"),
            session_id: session_id(),
            run_id: None,
            runtime_id: runtime_id(),
            seq,
            ts: 1,
            payload: EventPayload::Log(LogPayload {
                level: LogLevel::Info,
                message: message.to_owned(),
            }),
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        isolations: Mutex<Vec<(String, String, String)>>,
        releases: Mutex<Vec<String>>,
        release_ok: AtomicU64,
    }

    impl RecordingSink {
        fn isolations(&self) -> Vec<(String, String, String)> {
            self.isolations.lock().expect("锁").clone()
        }

        fn releases(&self) -> Vec<String> {
            self.releases.lock().expect("锁").clone()
        }
    }

    impl IsolationSink for RecordingSink {
        fn isolate(&self, runtime_id: &RuntimeId, reason: IsolationReason, detail: &str) -> bool {
            self.isolations.lock().expect("锁").push((
                runtime_id.as_str().to_owned(),
                reason.detail_code().to_owned(),
                detail.to_owned(),
            ));
            true
        }

        fn release(&self, runtime_id: &RuntimeId) -> bool {
            self.releases
                .lock()
                .expect("锁")
                .push(runtime_id.as_str().to_owned());
            self.release_ok.load(Ordering::Relaxed) == 0
        }
    }

    fn controller_with(
        config: BackpressureConfig,
    ) -> (
        Arc<BackpressureController>,
        Arc<RecordingSink>,
        Arc<ManualClock>,
    ) {
        let clock = Arc::new(ManualClock::new(1_000_000));
        let sink = Arc::new(RecordingSink::default());
        let controller = BackpressureController::new(
            config,
            clock.clone(),
            Arc::clone(&sink) as Arc<dyn IsolationSink>,
        )
        .expect("控制器构造");
        (controller, sink, clock)
    }

    #[test]
    fn default_config_matches_d8_constants() {
        let config = BackpressureConfig::default();
        assert_eq!(config.delivery_max_bytes, 32 * 1024 * 1024, "D8：32MB");
        assert_eq!(config.delivery_per_session_items, 5_000, "D8：会话数×5000");
        assert_eq!(config.delivery_restart_delay_ms, 30_000, "D8：30s 后重启");
        assert_eq!(config.delivery_no_fall_ms, 60_000, "D8：60s 未回落");
        assert_eq!(config.storage_l1_threshold, 1_024, "D8：L1 >1024");
        assert_eq!(config.storage_l2_threshold, 4_096, "D8：L2 >4096");
        assert_eq!(config.pause_window_ms, 250, "D8：250ms/次");
        assert_eq!(config.pause_timeout_ms, 2_000, "D8：单次暂停超时 2s");
        assert_eq!(config.pause_budget_ms, 10_000, "D8：60s 累计 >10s");
        assert_eq!(config.pause_budget_window_ms, 60_000, "D8：60s 滑动窗口");
        assert_eq!(config.pause_max_consecutive_timeouts, 3, "D8：连续 3 次");
        assert_eq!(
            config.release_sustain_ms, 30_000,
            "D8/ADR-004：≤1024 持续 30s"
        );
        config.validate().expect("默认配置必须合法");
    }

    #[test]
    fn invalid_config_is_rejected() {
        let base = BackpressureConfig::default();
        let cases = [
            BackpressureConfig {
                delivery_max_bytes: 0,
                ..base.clone()
            },
            BackpressureConfig {
                delivery_per_session_items: 0,
                ..base.clone()
            },
            BackpressureConfig {
                delivery_restart_delay_ms: 0,
                ..base.clone()
            },
            BackpressureConfig {
                storage_l1_threshold: 8_192,
                ..base.clone()
            },
            BackpressureConfig {
                pause_window_ms: 0,
                ..base.clone()
            },
            BackpressureConfig {
                pause_window_ms: 4_000,
                pause_timeout_ms: 2_000,
                ..base.clone()
            },
            BackpressureConfig {
                pause_max_consecutive_timeouts: 0,
                ..base.clone()
            },
            BackpressureConfig {
                release_sustain_ms: 0,
                ..base.clone()
            },
            BackpressureConfig {
                tick_interval: Duration::ZERO,
                ..base.clone()
            },
        ];
        for config in cases {
            let error = config.validate().expect_err("非法配置必须拒绝");
            assert_eq!(error.code(), "invalid_backpressure_config");
        }
    }

    #[test]
    fn delivery_overflow_switches_to_readback_and_opens_circuit() {
        let config = BackpressureConfig {
            delivery_per_session_items: 4,
            delivery_max_bytes: 8 * 1024,
            ..BackpressureConfig::default()
        };
        let (controller, sink, _clock) = controller_with(config);
        controller.register_session(&runtime_id(), &session_id());

        for seq in 1..=5 {
            controller.ingest(log_envelope(
                &format!("01J0000000000000000000{seq:03}"),
                seq,
                "x",
            ));
        }
        // 会话数 1 × 4 条 → 第 5 条触发超限。
        let metrics = controller.metrics();
        assert_eq!(metrics.delivery_overflows, 1);
        assert_eq!(metrics.delivery_circuits_open, 1);
        assert_eq!(metrics.delivery_events, 0, "补读模式必须清空内存积压");
        let isolations = sink.isolations();
        assert_eq!(isolations.len(), 1);
        assert_eq!(isolations[0].0, "mock");
        assert_eq!(isolations[0].1, "delivery_backlog");
        assert!(isolations[0].2.contains("journal 补读模式"));

        // 准入熔断（L3：拒绝新会话/新 run）。
        let error = controller
            .admission(&runtime_id())
            .expect_err("熔断必须拒绝");
        assert_eq!(error.code(), "storage_backpressure");

        // 补读锚点 = 已确认 seq + 1（零丢失）。
        match controller.poll(&runtime_id()) {
            DeliveryPoll::ReadbackRequired {
                session_id: polled,
                from_seq,
            } => {
                assert_eq!(polled, session_id());
                assert_eq!(from_seq, 1);
            }
            other => panic!("必须切换补读模式: {other:?}"),
        }
        controller.ack_readback(&runtime_id(), &session_id(), 5);
        assert_eq!(controller.poll(&runtime_id()), DeliveryPoll::Empty);
        assert!(
            controller.admission(&runtime_id()).is_err(),
            "熔断窗口内仍拒绝"
        );
    }

    #[test]
    fn delivery_circuit_releases_after_restart_delay() {
        let config = BackpressureConfig {
            delivery_per_session_items: 2,
            delivery_restart_delay_ms: 30_000,
            ..BackpressureConfig::default()
        };
        let (controller, sink, clock) = controller_with(config);
        controller.register_session(&runtime_id(), &session_id());
        for seq in 1..=3 {
            controller.ingest(log_envelope(
                &format!("01J0000000000000000000{seq:03}"),
                seq,
                "x",
            ));
        }
        assert!(controller.admission(&runtime_id()).is_err());
        controller.observe(clock.now_ms() + 29_999, 0, StorageState::Normal);
        assert!(sink.releases().is_empty(), "30s 前不得解除");
        assert!(controller.admission(&runtime_id()).is_err());
        controller.observe(clock.now_ms() + 30_000, 0, StorageState::Normal);
        assert_eq!(sink.releases().len(), 1);
        assert!(
            controller.admission(&runtime_id()).is_ok(),
            "解除后恢复准入"
        );
        assert_eq!(controller.metrics().delivery_restarts, 1);
        // 补读锚点保留：消费方仍须从 journal 补齐（零丢失）。
        assert!(matches!(
            controller.poll(&runtime_id()),
            DeliveryPoll::ReadbackRequired { from_seq: 1, .. }
        ));
    }

    #[test]
    fn lagged_switches_to_readback_without_circuit() {
        let (controller, sink, _clock) = controller_with(BackpressureConfig::default());
        controller.register_session(&runtime_id(), &session_id());
        controller.ingest(log_envelope("01J000000000000000000001", 1, "a"));
        controller.handle_lagged(7);
        assert_eq!(controller.metrics().delivery_lagged_events, 7);
        assert_eq!(
            controller.metrics().delivery_circuits_open,
            0,
            "Lagged 不熔断"
        );
        assert!(sink.isolations().is_empty());
        assert!(matches!(
            controller.poll(&runtime_id()),
            DeliveryPoll::ReadbackRequired { from_seq: 1, .. }
        ));
        controller.ack_readback(&runtime_id(), &session_id(), 1);
        assert_eq!(controller.poll(&runtime_id()), DeliveryPoll::Empty);
    }

    #[test]
    fn storage_pause_times_out_at_two_seconds_and_isolates_after_three() {
        let (controller, sink, clock) = controller_with(BackpressureConfig::default());
        controller.register_session(&runtime_id(), &session_id());

        // 第一次暂停：闸门暂停且剩余 ≤2s。
        controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
        assert_eq!(controller.phase(&runtime_id()), PressurePhase::Paused);
        match controller.control_read_gate(&runtime_id()) {
            ControlReadGate::Paused { remaining_ms } => {
                assert!(remaining_ms <= 2_000, "单次暂停 ≤2s: {remaining_ms}");
            }
            other => panic!("压力下必须暂停: {other:?}"),
        }

        // 3 次连续 2s 超时 → 隔离。
        for round in 1..=3u32 {
            clock.advance(2_000);
            controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
            let metrics = controller.metrics();
            assert_eq!(
                metrics.pause_timeouts,
                u64::from(round),
                "第 {round} 次超时"
            );
            if round < 3 {
                assert_eq!(controller.phase(&runtime_id()), PressurePhase::Normal);
                controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
                assert_eq!(controller.phase(&runtime_id()), PressurePhase::Paused);
            }
        }
        assert_eq!(controller.phase(&runtime_id()), PressurePhase::Isolated);
        let isolations = sink.isolations();
        assert_eq!(isolations.len(), 1);
        assert_eq!(isolations[0].1, "storage_backpressure");
        assert!(isolations[0]
            .2
            .contains("status_reason=storage_backpressure"));
        let error = controller
            .admission(&runtime_id())
            .expect_err("隔离期拒绝新 run");
        assert_eq!(error.code(), "storage_backpressure");

        // 回落 ≤L1 持续 30s → 自动解除。
        controller.observe(clock.now_ms(), 1_024, StorageState::Normal);
        clock.advance(29_999);
        controller.observe(clock.now_ms(), 1_024, StorageState::Normal);
        assert!(sink.releases().is_empty(), "30s 前不得解除");
        clock.advance(1);
        controller.observe(clock.now_ms(), 1_024, StorageState::Normal);
        assert_eq!(sink.releases().len(), 1);
        assert_eq!(controller.phase(&runtime_id()), PressurePhase::Normal);
        assert!(controller.admission(&runtime_id()).is_ok());
        assert_eq!(controller.metrics().storage_releases, 1);
    }

    #[test]
    fn pause_budget_isolates_without_timeouts() {
        // 预算路径：单次暂停窗口内累计 > 阈值即隔离（连续超时阈值放宽到 100）。
        let config = BackpressureConfig {
            pause_budget_ms: 500,
            pause_max_consecutive_timeouts: 100,
            ..BackpressureConfig::default()
        };
        let (controller, sink, clock) = controller_with(config);
        controller.register_session(&runtime_id(), &session_id());
        controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
        // 750ms：3 个 250ms 窗口（750ms > 500ms 预算）。
        clock.advance(750);
        controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
        assert_eq!(controller.phase(&runtime_id()), PressurePhase::Isolated);
        assert_eq!(sink.isolations().len(), 1);
        assert!(controller.metrics().pause_timeouts <= 1);
    }

    #[test]
    fn persist_degraded_freezes_pause_isolation_and_release() {
        let (controller, sink, clock) = controller_with(BackpressureConfig::default());
        controller.register_session(&runtime_id(), &session_id());

        // 先隔离，再进入 persist_degraded：自动解除路径不得生效。
        controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
        for _ in 0..3 {
            clock.advance(2_000);
            controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
            if controller.phase(&runtime_id()) == PressurePhase::Isolated {
                break;
            }
            controller.observe(clock.now_ms(), 5_000, StorageState::Normal);
        }
        assert_eq!(controller.phase(&runtime_id()), PressurePhase::Isolated);

        controller.observe(clock.now_ms(), 0, StorageState::PersistDegraded);
        assert_eq!(
            controller.phase(&runtime_id()),
            PressurePhase::PersistDegraded
        );
        for _ in 0..4 {
            clock.advance(30_000);
            controller.observe(clock.now_ms(), 0, StorageState::PersistDegraded);
        }
        assert!(
            sink.releases().is_empty(),
            "persist_degraded 不得走队列自动解除"
        );
        let error = controller.admission(&runtime_id()).expect_err("降级期拒绝");
        assert_eq!(error.code(), "persist_degraded");
        assert!(controller.metrics().persist_degraded);

        // 降级期高水位也不得重新暂停/隔离。
        controller.observe(clock.now_ms(), 9_999, StorageState::PersistDegraded);
        assert_eq!(
            controller.phase(&runtime_id()),
            PressurePhase::PersistDegraded
        );
        assert_eq!(sink.isolations().len(), 1, "降级期不得新增隔离");
    }

    #[test]
    fn poll_and_ack_deliver_in_order_with_bounded_bytes() {
        let (controller, _sink, _clock) = controller_with(BackpressureConfig::default());
        controller.register_session(&runtime_id(), &session_id());
        for seq in 1..=3 {
            controller.ingest(log_envelope(
                &format!("01J0000000000000000000{seq:03}"),
                seq,
                "hello",
            ));
        }
        assert!(controller.metrics().delivery_bytes > 0);
        let events = match controller.poll(&runtime_id()) {
            DeliveryPoll::Events(events) => events,
            other => panic!("必须返回事件: {other:?}"),
        };
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        controller.ack(&runtime_id(), &session_id(), 3);
        assert_eq!(controller.poll(&runtime_id()), DeliveryPoll::Empty);
        assert_eq!(controller.metrics().delivery_bytes, 0);
    }
}
