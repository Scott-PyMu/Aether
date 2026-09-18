//! 事件管线（M1-05；设计 D4）：normalizer → 校验 → 会话 sequencer → 先日志后广播
//! → delta 合并 → 补读；持久化降级状态机。
//!
//! 固定顺序（D4 实现要点，AGENTS §2.3）：
//!
//! ```text
//! 适配器事件 → Normalizer → serde 严格校验 → 会话 sequencer（seq 单调唯一）
//!            → journal 入队（D3 写队列）→ **写成功后** broadcast
//! ```
//!
//! - **先日志后广播**：仅当 `append` 返回成功（事务已提交）才 `broadcast::send`；
//!   不存在「仅内存广播」路径（ADR-003 决策 6）；
//! - **delta 合并**：`message.delta` 在 16ms/8KB 窗口内合并为一条持久化；同消息
//!   `message.completed` 到达时先冲刷 delta（终稿不受合并影响）；
//! - **补读**：UI 带 `last_seq` 断点续传，缺口从 `events` 表读，>10k 拒绝自动补发
//!   （错误码 `readback_gap_too_large`）；
//! - **降级**（D4 状态机）：写事务重试 3 次均失败 → `persist_degraded` + 只读；
//!   拒绝新写入/新 run；未落盘事件不广播；在途 run 转 `cancelled`；
//!   降级通知优先经正常管线落盘后广播 `error(recoverable=false)`，落盘失败则仅经
//!   [`EventPipeline::health`] 返回 `storage_state=persist_degraded`；
//! - **背压边界**（ADR-004）：写队列临时高水位（D8 L2）返回 `storage_backpressure`，
//!   **不**进入降级状态；
//! - **DB 兜底**：`UNIQUE(session_id, seq)` 命中视为管线 bug——计入诊断并按持久化
//!   失败路径处理（D4）；`evt.id` 主键冲突则按幂等命中丢弃。

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_core::{
    ErrorInfo, EventEnvelope, EventId, EventPayload, EventType, MessageId, RunId, RuntimeId,
    SessionId, EVENT_ENVELOPE_VERSION,
};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::delta::{DeltaBuffer, DELTA_FLUSH_BYTES, DELTA_FLUSH_INTERVAL};
use crate::error::{JournalError, PipelineError};
use crate::journal::{JournalMetrics, JournalWriter, PressureLevel};
use crate::normalizer::{Normalizer, PendingEvent};
use crate::sequencer::SessionSequencer;
use crate::source::EventSource;
use crate::storage_state::{
    DegradeTrigger, StartupSelfCheckReport, StorageState, StorageStateMachine,
};
use crate::time::now_ms;
use crate::ulid;

/// 入站命令队列容量（管线入口，独立于写队列）。
pub const SUBMIT_QUEUE_CAPACITY: usize = 4_096;
/// 事件广播通道容量（D8：`tokio::broadcast(4096)`）。
pub const BROADCAST_CAPACITY: usize = 4_096;
/// run 中断通知通道容量（降级时通知生命周期层 interrupt，非事件通道）。
pub const RUN_INTERRUPT_CAPACITY: usize = 256;
/// `evt.id` 去重缓存容量（有界；更早的重复由 `events.id` 主键兜底）。
pub const DEDUP_CAPACITY: usize = 100_000;
/// 写事务最大尝试次数（ADR-007 决策 2：**总尝试次数含首次**；重试 = 2 次）。
///
/// D4：连续 3 次写事务尝试失败（含首次）→ `persist_degraded`。
pub const MAX_WRITE_ATTEMPTS: usize = 3;
/// 持久化重试间隔（常量级调参；测试注入 0）。
pub const PERSIST_RETRY_DELAY: Duration = Duration::from_millis(25);
/// 补读缺口上限（D4：>10k 拒绝自动补发）。
pub const READBACK_MAX_GAP: u64 = 10_000;
/// 补读分页大小（D7 分页 ≤500 条）。
pub const READBACK_PAGE_SIZE: usize = 500;

/// 管线运行参数（默认值即 D4 约定；测试/故障注入可参数化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineConfig {
    /// 写事务最大尝试次数（默认 3，含首次；重试 2 次；ADR-007 决策 2）。
    pub max_write_attempts: usize,
    /// 持久化重试间隔（默认 25ms；测试可置 0）。
    pub persist_retry_delay: Duration,
    /// delta 合并窗口（默认 16ms）。
    pub delta_flush_interval: Duration,
    /// delta 合并字节阈值（默认 8KB）。
    pub delta_flush_bytes: usize,
    /// `evt.id` 去重缓存容量（默认 100k）。
    pub dedup_capacity: usize,
    /// 入站队列容量（默认 4096）。
    pub submit_queue_capacity: usize,
    /// 广播通道容量（默认 4096）。
    pub broadcast_capacity: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            max_write_attempts: MAX_WRITE_ATTEMPTS,
            persist_retry_delay: PERSIST_RETRY_DELAY,
            delta_flush_interval: DELTA_FLUSH_INTERVAL,
            delta_flush_bytes: DELTA_FLUSH_BYTES,
            dedup_capacity: DEDUP_CAPACITY,
            submit_queue_capacity: SUBMIT_QUEUE_CAPACITY,
            broadcast_capacity: BROADCAST_CAPACITY,
        }
    }
}

impl PipelineConfig {
    /// 校验参数（容量/次数/阈值必须为正）。
    pub fn validate(&self) -> Result<(), PipelineError> {
        let invalid = |reason: &str| {
            Err(PipelineError::InvalidConfig {
                reason: reason.to_owned(),
            })
        };
        if self.max_write_attempts == 0 {
            return invalid("max_write_attempts 必须 >0");
        }
        if self.delta_flush_interval.is_zero() {
            return invalid("delta_flush_interval 必须 >0");
        }
        if self.delta_flush_bytes == 0 {
            return invalid("delta_flush_bytes 必须 >0");
        }
        if self.dedup_capacity == 0 {
            return invalid("dedup_capacity 必须 >0");
        }
        if self.submit_queue_capacity == 0 {
            return invalid("submit_queue_capacity 必须 >0");
        }
        if self.broadcast_capacity == 0 {
            return invalid("broadcast_capacity 必须 >0");
        }
        Ok(())
    }
}

/// 单次提交结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// 已落盘（此时已按序广播）。
    Persisted { seq: u64, event_type: EventType },
    /// `message.delta` 进入合并窗口（窗口到期/累计 8KB/同消息终稿时冲刷；
    /// 8KB 阈值路径在返回前完成落盘，其余路径异步冲刷）。
    Buffered,
    /// `evt.id` 幂等命中（丢弃并计数，不视为故障）。
    Duplicate { event_id: EventId },
    /// 严格校验失败（死信计数，不阻断会话）。
    DeadLettered { code: String, reason: String },
}

impl SubmitOutcome {
    /// 是否落盘（断言/诊断用）。
    pub fn is_persisted(&self) -> bool {
        matches!(self, Self::Persisted { .. })
    }

    /// 落盘 seq（未落盘为 `None`；delta 缓冲阶段无 seq）。
    pub fn seq(&self) -> Option<u64> {
        match self {
            Self::Persisted { seq, .. } => Some(*seq),
            _ => None,
        }
    }
}

/// 补读结果（D4：UI 带 `last_seq` 断点续传）。
#[derive(Debug, Clone, PartialEq)]
pub struct ReadbackFrame {
    pub session_id: SessionId,
    /// 请求的断点（不含）。
    pub last_seq: u64,
    /// 读取时的会话最大 seq。
    pub max_seq: Option<u64>,
    /// 补读事件（升序，`> last_seq`）。
    pub events: Vec<EventEnvelope>,
    /// 是否已补到读取时的 max（false 表示读取期间又有新事件，调用方可再次补读）。
    pub complete: bool,
}

/// 降级时对在途 run 的中断通知（D4 降级期语义 3；经控制通道，非事件通道）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInterrupt {
    pub session_id: SessionId,
    pub run_id: RunId,
    /// 固定为 `persist_degraded`（P0 仅此来源）。
    pub reason: String,
    pub at_ms: i64,
}

/// run 中断原因（D4 降级期语义 3）。
pub const RUN_INTERRUPT_REASON_DEGRADED: &str = "persist_degraded";

/// 管线健康快照（D2：UI 每 5s 轮询 `health`；降级通知经此返回）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineHealth {
    /// `storage_state`：`normal` / `persist_degraded`。
    pub storage_state: StorageState,
    /// 降级触发源（写失败/空间护栏/完整性失败）。
    pub degrade_trigger: Option<DegradeTrigger>,
    /// 降级进入时间（Unix epoch 毫秒）。
    pub degraded_since_ms: Option<i64>,
    /// 已落盘事件数。
    pub persisted_events: u64,
    /// 已广播事件数（仅落盘后的广播）。
    pub broadcast_events: u64,
    /// 因持久化失败/降级丢弃的事件数（未落盘，未广播）。
    pub dropped_events: u64,
    /// 死信（严格校验失败）事件数。
    pub dead_letter_events: u64,
    /// `evt.id` 幂等命中数。
    pub duplicate_events: u64,
    /// `UNIQUE(session_id, seq)` 兜底命中次数（管线 bug 诊断）。
    pub duplicate_seq_bugs: u64,
    /// 持久化重试次数（失败尝试累计）。
    pub persist_retries: u64,
    /// 写队列临时高水位导致的准入拒绝次数（**不**进入降级）。
    pub backpressure_rejections: u64,
    /// 降级时被取消的在途 run 数。
    pub cancelled_runs: u64,
    /// 当前在途 run 数。
    pub in_flight_runs: usize,
    /// 进入合并窗口的输入 delta 数。
    pub delta_input_events: u64,
    /// 合并后落盘的 delta 事件数。
    pub delta_persisted_events: u64,
    /// sequencer 重启次数（D4：崩溃恢复路径）。
    pub sequencer_restarts: u64,
    /// 重启/降级丢弃的未落盘 delta 输入数。
    pub delta_buffers_discarded: u64,
    /// 入站队列待处理命令数。
    pub ingress_pending: usize,
    /// journal（写队列）深度。
    pub journal_queue_depth: usize,
    /// journal 背压等级（临时高水位诊断）。
    pub journal_pressure_level: Option<PressureLevel>,
}

impl PipelineHealth {
    /// `health.storage_state` 取值（DoD⑥：`persist_degraded`）。
    pub const fn storage_state_code(&self) -> &'static str {
        self.storage_state.as_str()
    }

    /// 是否降级。
    pub const fn is_degraded(&self) -> bool {
        self.storage_state.is_degraded()
    }
}

#[derive(Debug, Default)]
struct PipelineCounters {
    ingress_pending: AtomicUsize,
    persisted_events: AtomicU64,
    broadcast_events: AtomicU64,
    dropped_events: AtomicU64,
    dead_letter_events: AtomicU64,
    duplicate_events: AtomicU64,
    duplicate_seq_bugs: AtomicU64,
    persist_retries: AtomicU64,
    backpressure_rejections: AtomicU64,
    cancelled_runs: AtomicU64,
    in_flight_runs: AtomicUsize,
    delta_input_events: AtomicU64,
    delta_persisted_events: AtomicU64,
    sequencer_restarts: AtomicU64,
    delta_buffers_discarded: AtomicU64,
}

fn bump(counter: &AtomicU64, amount: u64) {
    counter.fetch_add(amount, Ordering::Relaxed);
}

fn bump_usize(counter: &AtomicUsize, amount: usize) {
    counter.fetch_add(amount, Ordering::Relaxed);
}

fn dec_usize(counter: &AtomicUsize, amount: usize) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(amount))
    });
}

/// 有界 `evt.id` 去重集（D4：`evt.id` 全局去重；更早的重复由 DB 主键兜底）。
#[derive(Debug)]
struct DedupSet {
    capacity: usize,
    ids: HashSet<EventId>,
    order: VecDeque<EventId>,
}

impl DedupSet {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            ids: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// 插入并返回是否为首次出现（`false` = 重复）。
    fn insert(&mut self, id: &EventId) -> bool {
        if self.ids.contains(id) {
            return false;
        }
        if self.ids.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.ids.remove(&evicted);
            }
        }
        self.ids.insert(id.clone());
        self.order.push_back(id.clone());
        true
    }
}

/// 会话管线状态（actor 内单线程持有）。
#[derive(Debug, Default)]
struct SessionState {
    /// 单一 sequencer（`None` = 待从库中 `max(seq)` 初始化/重启后重建）。
    sequencer: Option<SessionSequencer>,
    /// 待合并 delta（按消息保序）。
    deltas: Vec<DeltaBuffer>,
    /// 最近一次事件上报的 runtime（降级 `error` 事件需要用）。
    last_runtime_id: Option<RuntimeId>,
}

/// delta 冲刷模式。
#[derive(Debug, Clone, Copy)]
enum FlushMode {
    /// 全部（非 delta 事件到达时保序冲刷）。
    All,
    /// 仅窗口到期（定时器路径）。
    Due(Instant),
}

/// 落盘结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOutcome {
    Persisted,
    /// `events.id` 主键冲突（幂等命中）。
    DuplicateId,
}

enum Command {
    Submit {
        raw: Value,
        reply: oneshot::Sender<Result<SubmitOutcome, PipelineError>>,
    },
    RestartSession {
        session_id: SessionId,
        reply: oneshot::Sender<Result<(), PipelineError>>,
    },
    SignalDegraded {
        trigger: DegradeTrigger,
        reply: oneshot::Sender<bool>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// 事件管线句柄（克隆共享；actor 任务由 [`EventPipeline::start`] 启动）。
#[derive(Clone)]
pub struct EventPipeline {
    config: Arc<PipelineConfig>,
    commands: mpsc::Sender<Command>,
    storage: Arc<StorageStateMachine>,
    counters: Arc<PipelineCounters>,
    events: broadcast::Sender<EventEnvelope>,
    run_interrupts: broadcast::Sender<RunInterrupt>,
    journal: Arc<dyn JournalWriter>,
    source: Arc<dyn EventSource>,
    actor: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl EventPipeline {
    /// 启动管线（actor spawn 到核心统一运行时，D2）。
    ///
    /// `startup` 为启动自检报告（D4 恢复路径）：自检失败时管线以只读降级状态启动，
    /// 拒绝新写入/新 run，直到「修复外部条件 + 重启核心 + 自检通过」。
    pub fn start(
        config: PipelineConfig,
        journal: Arc<dyn JournalWriter>,
        source: Arc<dyn EventSource>,
        startup: &StartupSelfCheckReport,
        handle: &tokio::runtime::Handle,
    ) -> Result<Self, PipelineError> {
        config.validate()?;
        let config = Arc::new(config);
        let storage = Arc::new(StorageStateMachine::from_startup_check(startup));
        let counters = Arc::new(PipelineCounters::default());
        let (commands, receiver) = mpsc::channel(config.submit_queue_capacity);
        let (events, _receiver) = broadcast::channel(config.broadcast_capacity);
        let (run_interrupts, _receiver) = broadcast::channel(RUN_INTERRUPT_CAPACITY);
        let actor = Actor {
            config: Arc::clone(&config),
            journal: Arc::clone(&journal),
            source: Arc::clone(&source),
            storage: Arc::clone(&storage),
            counters: Arc::clone(&counters),
            events: events.clone(),
            run_interrupts: run_interrupts.clone(),
            sessions: HashMap::new(),
            dedup: DedupSet::new(config.dedup_capacity),
            in_flight_runs: BTreeMap::new(),
        };
        let join = handle.spawn(run_actor(actor, receiver));
        Ok(Self {
            config,
            commands,
            storage,
            counters,
            events,
            run_interrupts,
            journal,
            source,
            actor: Arc::new(Mutex::new(Some(join))),
        })
    }

    /// 提交一条适配器事件（严格归一化 → sequencer → journal → 广播）。
    ///
    /// 返回落在提交完成（含落盘与广播）之后；`message.delta` 返回
    /// [`SubmitOutcome::Buffered`]（合并在窗口内异步冲刷）。
    pub async fn submit(&self, raw: Value) -> Result<SubmitOutcome, PipelineError> {
        let (reply, receiver) = oneshot::channel();
        bump_usize(&self.counters.ingress_pending, 1);
        if self
            .commands
            .send(Command::Submit { raw, reply })
            .await
            .is_err()
        {
            dec_usize(&self.counters.ingress_pending, 1);
            return Err(PipelineError::PipelineClosed);
        }
        match receiver.await {
            Ok(result) => result,
            Err(_) => Err(PipelineError::PipelineClosed),
        }
    }

    /// 新写入 / 新 run 准入（D4 降级期语义 1 + D8 L2）。
    ///
    /// - 持久化降级 → [`PipelineError::PersistDegraded`]（拒绝新写入/新 run）；
    /// - 写队列临时高水位 → [`PipelineError::StorageBackpressure`]（不改变存储状态）。
    pub fn admission(&self) -> Result<(), PipelineError> {
        self.storage.accept_write()?;
        match self.journal.admission() {
            Ok(()) => Ok(()),
            Err(JournalError::Backpressure { depth, threshold }) => {
                Err(PipelineError::StorageBackpressure { depth, threshold })
            }
            Err(JournalError::Closed) => Err(PipelineError::Journal(JournalError::Closed)),
            Err(error) => Err(PipelineError::Journal(error)),
        }
    }

    /// 运行期降级信号（空间护栏 / 完整性失败；写失败由管线内部触发）。
    ///
    /// 返回 `true` 表示本次调用完成了状态转移。写队列临时高水位**不得**走本入口
    /// （ADR-004 决策 1）。
    pub async fn signal_degraded(&self, trigger: DegradeTrigger) -> Result<bool, PipelineError> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::SignalDegraded { trigger, reply })
            .await
            .map_err(|_| PipelineError::PipelineClosed)?;
        receiver.await.map_err(|_| PipelineError::PipelineClosed)
    }

    /// 补读（D4：`last_seq` 断点续传）。
    ///
    /// 缺口 = `max(seq) - last_seq`；> [`READBACK_MAX_GAP`]（10k）返回
    /// [`PipelineError::ReadbackGapTooLarge`]（错误码 `readback_gap_too_large`）。
    /// 读路径不写库，降级期保持可用。
    pub async fn readback(
        &self,
        session_id: &SessionId,
        last_seq: u64,
    ) -> Result<ReadbackFrame, PipelineError> {
        let max_seq = self.source.max_seq(session_id).await?;
        let Some(max_seq) = max_seq else {
            return Ok(ReadbackFrame {
                session_id: session_id.clone(),
                last_seq,
                max_seq: None,
                events: Vec::new(),
                complete: true,
            });
        };
        if max_seq <= last_seq {
            return Ok(ReadbackFrame {
                session_id: session_id.clone(),
                last_seq,
                max_seq: Some(max_seq),
                events: Vec::new(),
                complete: true,
            });
        }
        let gap = max_seq - last_seq;
        if gap > READBACK_MAX_GAP {
            return Err(PipelineError::ReadbackGapTooLarge {
                gap,
                limit: READBACK_MAX_GAP,
            });
        }

        let mut events: Vec<EventEnvelope> = Vec::new();
        let mut cursor = last_seq;
        while (events.len() as u64) < gap {
            let page = self
                .source
                .events_after(session_id, Some(cursor), READBACK_PAGE_SIZE)
                .await?;
            if page.is_empty() {
                break;
            }
            if let Some(last) = page.last() {
                cursor = last.seq;
            }
            events.extend(page);
        }
        let last_read = events.last().map(|event| event.seq);
        Ok(ReadbackFrame {
            session_id: session_id.clone(),
            last_seq,
            max_seq: Some(max_seq),
            complete: last_read == Some(max_seq),
            events,
        })
    }

    /// sequencer 崩溃恢复（D4 失败场景表；由 M2-07 panic 隔离在任务重启后调用）。
    ///
    /// 丢弃该会话未落盘 delta（未广播，允许缺口），清空 sequencer；下一次提交从
    /// 库中 `max(seq)+1` 重新初始化，期间提交在入站队列排队。
    pub async fn restart_session(&self, session_id: SessionId) -> Result<(), PipelineError> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::RestartSession { session_id, reply })
            .await
            .map_err(|_| PipelineError::PipelineClosed)?;
        receiver.await.map_err(|_| PipelineError::PipelineClosed)?
    }

    /// 健康快照（UI 每 5s 轮询；降级通知经此返回 `storage_state=persist_degraded`）。
    pub fn health(&self) -> PipelineHealth {
        let metrics: JournalMetrics = self.journal.metrics();
        PipelineHealth {
            storage_state: self.storage.state(),
            degrade_trigger: self.storage.trigger(),
            degraded_since_ms: self.storage.degraded_since_ms(),
            persisted_events: self.counters.persisted_events.load(Ordering::Relaxed),
            broadcast_events: self.counters.broadcast_events.load(Ordering::Relaxed),
            dropped_events: self.counters.dropped_events.load(Ordering::Relaxed),
            dead_letter_events: self.counters.dead_letter_events.load(Ordering::Relaxed),
            duplicate_events: self.counters.duplicate_events.load(Ordering::Relaxed),
            duplicate_seq_bugs: self.counters.duplicate_seq_bugs.load(Ordering::Relaxed),
            persist_retries: self.counters.persist_retries.load(Ordering::Relaxed),
            backpressure_rejections: self
                .counters
                .backpressure_rejections
                .load(Ordering::Relaxed),
            cancelled_runs: self.counters.cancelled_runs.load(Ordering::Relaxed),
            in_flight_runs: self.counters.in_flight_runs.load(Ordering::Relaxed),
            delta_input_events: self.counters.delta_input_events.load(Ordering::Relaxed),
            delta_persisted_events: self.counters.delta_persisted_events.load(Ordering::Relaxed),
            sequencer_restarts: self.counters.sequencer_restarts.load(Ordering::Relaxed),
            delta_buffers_discarded: self
                .counters
                .delta_buffers_discarded
                .load(Ordering::Relaxed),
            ingress_pending: self.counters.ingress_pending.load(Ordering::Relaxed),
            journal_queue_depth: metrics.queue_depth,
            journal_pressure_level: metrics.pressure_level,
        }
    }

    /// 订阅落盘后的事件广播（D8：`broadcast(4096)`；慢消费者 `Lagged` 由 M2-04/M3-01 补读）。
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.events.subscribe()
    }

    /// 订阅降级 run 中断通知（控制通道；生命周期层消费并执行 `session.interrupt`）。
    pub fn subscribe_run_interrupts(&self) -> broadcast::Receiver<RunInterrupt> {
        self.run_interrupts.subscribe()
    }

    /// 管线配置（诊断/断言）。
    pub fn config(&self) -> &PipelineConfig {
        &self.config
    }

    /// 关停：actor drain 在途命令后退出（关闭序列的管线侧步骤）。
    pub async fn shutdown(&self) -> Result<(), PipelineError> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(Command::Shutdown { reply })
            .await
            .map_err(|_| PipelineError::PipelineClosed)?;
        let _ = receiver.await;
        let join = match self.actor.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(join) = join {
            let _ = join.await;
        }
        Ok(())
    }
}

struct Actor {
    config: Arc<PipelineConfig>,
    journal: Arc<dyn JournalWriter>,
    source: Arc<dyn EventSource>,
    storage: Arc<StorageStateMachine>,
    counters: Arc<PipelineCounters>,
    events: broadcast::Sender<EventEnvelope>,
    run_interrupts: broadcast::Sender<RunInterrupt>,
    sessions: HashMap<SessionId, SessionState>,
    dedup: DedupSet,
    in_flight_runs: BTreeMap<RunId, SessionId>,
}

async fn run_actor(mut actor: Actor, mut receiver: mpsc::Receiver<Command>) {
    loop {
        let command = match actor.earliest_delta_deadline() {
            Some(deadline) => match tokio::time::timeout_at(deadline, receiver.recv()).await {
                Ok(Some(command)) => Some(command),
                Ok(None) => break,
                Err(_elapsed) => {
                    actor.flush_due_deltas().await;
                    None
                }
            },
            None => match receiver.recv().await {
                Some(command) => Some(command),
                None => break,
            },
        };
        match command {
            Some(Command::Submit { raw, reply }) => actor.handle_submit(raw, reply).await,
            Some(Command::RestartSession { session_id, reply }) => {
                actor.handle_restart_session(&session_id);
                let _ = reply.send(Ok(()));
            }
            Some(Command::SignalDegraded { trigger, reply }) => {
                let origin = actor.default_origin();
                let transitioned = actor.enter_degraded(trigger, origin, 0).await;
                let _ = reply.send(transitioned);
            }
            Some(Command::Shutdown { reply }) => {
                // 关闭序列（D2）：放弃未落盘 delta（控制事件已按提交序落盘），不广播。
                actor.discard_pending_deltas();
                let _ = reply.send(());
                break;
            }
            None => {}
        }
    }
}

impl Actor {
    async fn handle_submit(
        &mut self,
        raw: Value,
        reply: oneshot::Sender<Result<SubmitOutcome, PipelineError>>,
    ) {
        dec_usize(&self.counters.ingress_pending, 1);
        if let Err(error) = self.storage.accept_write() {
            let _ = reply.send(Err(error));
            return;
        }
        let pending = match Normalizer::normalize(&raw) {
            Ok(pending) => pending,
            Err(error) => {
                bump(&self.counters.dead_letter_events, 1);
                let _ = reply.send(Ok(SubmitOutcome::DeadLettered {
                    code: error.code().to_owned(),
                    reason: error.to_string(),
                }));
                return;
            }
        };
        if !self.dedup.insert(&pending.id) {
            bump(&self.counters.duplicate_events, 1);
            let _ = reply.send(Ok(SubmitOutcome::Duplicate {
                event_id: pending.id,
            }));
            return;
        }
        let outcome = self.process(pending).await;
        let _ = reply.send(outcome);
    }

    async fn process(&mut self, pending: PendingEvent) -> Result<SubmitOutcome, PipelineError> {
        let message_id = match &pending.payload {
            EventPayload::MessageDelta(delta) => Some(delta.message_id.clone()),
            _ => None,
        };
        if let Some(message_id) = message_id {
            return self.buffer_delta(pending, message_id).await;
        }
        let tracked_run = self.track_run(&pending);
        if let Err(error) = self
            .flush_session_deltas(&pending.session_id, FlushMode::All)
            .await
        {
            if let Some(run_id) = tracked_run {
                self.untrack_run(&run_id);
            }
            return Err(error);
        }
        let outcome = self.persist_pending(pending, 1).await;
        if outcome.is_err() {
            if let Some(run_id) = tracked_run {
                self.untrack_run(&run_id);
            }
        }
        outcome
    }

    /// 记录/解除 run 台账（D4 降级期语义 3：在途 run 转 cancelled）。
    ///
    /// 返回本次新登记的 run（持久化失败时回滚台账）。
    fn track_run(&mut self, pending: &PendingEvent) -> Option<RunId> {
        let session = self.sessions.entry(pending.session_id.clone()).or_default();
        session.last_runtime_id = Some(pending.runtime_id.clone());
        match &pending.payload {
            EventPayload::RunStarted(payload) => {
                self.in_flight_runs
                    .insert(payload.run_id.clone(), pending.session_id.clone());
                bump_usize(&self.counters.in_flight_runs, 1);
                Some(payload.run_id.clone())
            }
            EventPayload::RunCompleted(payload) => {
                self.untrack_run(&payload.run_id);
                None
            }
            EventPayload::RunFailed(payload) => {
                self.untrack_run(&payload.run_id);
                None
            }
            EventPayload::RunCancelled(payload) => {
                self.untrack_run(&payload.run_id);
                None
            }
            _ => None,
        }
    }

    fn untrack_run(&mut self, run_id: &RunId) {
        if self.in_flight_runs.remove(run_id).is_some() {
            dec_usize(&self.counters.in_flight_runs, 1);
        }
    }

    async fn buffer_delta(
        &mut self,
        pending: PendingEvent,
        message_id: MessageId,
    ) -> Result<SubmitOutcome, PipelineError> {
        let (session_id, run_id, runtime_id, text) = match pending {
            PendingEvent {
                session_id,
                run_id,
                runtime_id,
                payload: EventPayload::MessageDelta(delta),
                ..
            } => (session_id, run_id, runtime_id, delta.text),
            other => {
                return Err(PipelineError::Internal {
                    reason: format!(
                        "delta 路径收到非 delta 事件: {:?}",
                        other.payload.event_type()
                    ),
                })
            }
        };
        bump(&self.counters.delta_input_events, 1);
        let threshold = self.config.delta_flush_bytes;
        let now = Instant::now();
        let state = self.sessions.entry(session_id.clone()).or_default();
        state.last_runtime_id = Some(runtime_id.clone());

        let mut flush_index = None;
        match state
            .deltas
            .iter_mut()
            .position(|buffer| buffer.message_id == message_id)
        {
            Some(index) => {
                let buffer = &mut state.deltas[index];
                buffer.push(&text);
                if buffer.is_full(threshold) {
                    flush_index = Some(index);
                }
            }
            None => {
                let mut buffer = DeltaBuffer::new(
                    session_id.clone(),
                    run_id,
                    runtime_id,
                    message_id,
                    now,
                    self.config.delta_flush_interval,
                );
                buffer.push(&text);
                let full = buffer.is_full(threshold);
                state.deltas.push(buffer);
                if full {
                    flush_index = Some(state.deltas.len() - 1);
                }
            }
        }
        if let Some(index) = flush_index {
            self.flush_delta_at(&session_id, index).await?;
        }
        Ok(SubmitOutcome::Buffered)
    }

    fn earliest_delta_deadline(&self) -> Option<Instant> {
        self.sessions
            .values()
            .flat_map(|state| state.deltas.iter())
            .map(|buffer| buffer.deadline)
            .min()
    }

    async fn flush_due_deltas(&mut self) {
        let now = Instant::now();
        let sessions: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, state)| state.deltas.iter().any(|buffer| buffer.is_due(now)))
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in sessions {
            if self
                .flush_session_deltas(&session_id, FlushMode::Due(now))
                .await
                .is_err()
            {
                return;
            }
        }
    }

    /// 关闭序列：放弃全部未落盘 delta（D2：drain 限时放弃 delta，保留控制事件）。
    fn discard_pending_deltas(&mut self) {
        let mut discarded = 0u64;
        for state in self.sessions.values_mut() {
            for buffer in state.deltas.drain(..) {
                discarded += buffer.event_count as u64;
            }
        }
        bump(&self.counters.delta_buffers_discarded, discarded);
    }

    async fn flush_session_deltas(
        &mut self,
        session_id: &SessionId,
        mode: FlushMode,
    ) -> Result<(), PipelineError> {
        loop {
            let index = match self.sessions.get(session_id) {
                Some(state) => state.deltas.iter().position(|buffer| match mode {
                    FlushMode::All => true,
                    FlushMode::Due(now) => buffer.is_due(now),
                }),
                None => None,
            };
            let Some(index) = index else { return Ok(()) };
            self.flush_delta_at(session_id, index).await?;
        }
    }

    async fn flush_delta_at(
        &mut self,
        session_id: &SessionId,
        index: usize,
    ) -> Result<(), PipelineError> {
        let buffer = match self.sessions.get_mut(session_id) {
            Some(state) if index < state.deltas.len() => state.deltas.remove(index),
            _ => return Ok(()),
        };
        let input_events = buffer.event_count;
        let id = match EventId::new(ulid::generate()) {
            Ok(id) => id,
            Err(_) => {
                bump(&self.counters.dropped_events, input_events as u64);
                return Err(PipelineError::Internal {
                    reason: "ULID 生成失败".to_owned(),
                });
            }
        };
        let pending = buffer.into_pending_now(id);
        match self.persist_pending(pending, input_events).await? {
            SubmitOutcome::Persisted { .. } => {
                bump(&self.counters.delta_persisted_events, 1);
                Ok(())
            }
            SubmitOutcome::Duplicate { .. } => Ok(()),
            other => Err(PipelineError::Internal {
                reason: format!("delta 冲刷返回非持久化结果: {other:?}"),
            }),
        }
    }

    async fn next_seq(&mut self, session_id: &SessionId) -> Result<u64, PipelineError> {
        let needs_init = self
            .sessions
            .get(session_id)
            .map_or(true, |state| state.sequencer.is_none());
        if needs_init {
            // D4：sequencer 崩溃恢复 / 首次使用——seq = 库中 max(seq)+1。
            let max_seq = self.source.max_seq(session_id).await?;
            let state = self.sessions.entry(session_id.clone()).or_default();
            state.sequencer = Some(SessionSequencer::resume_after(max_seq));
        }
        let state = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| PipelineError::Internal {
                reason: "会话状态缺失".to_owned(),
            })?;
        match state.sequencer.as_mut() {
            Some(sequencer) => Ok(sequencer.next_seq()),
            None => Err(PipelineError::Internal {
                reason: "sequencer 未初始化".to_owned(),
            }),
        }
    }

    async fn persist_pending(
        &mut self,
        pending: PendingEvent,
        dropped_units: usize,
    ) -> Result<SubmitOutcome, PipelineError> {
        let event_type = pending.payload.event_type();
        let session_id = pending.session_id.clone();
        let seq = match self.next_seq(&session_id).await {
            Ok(seq) => seq,
            Err(error) => {
                bump(&self.counters.dropped_events, dropped_units as u64);
                return Err(error);
            }
        };
        let envelope = pending.with_seq(seq);
        match self
            .write_with_retry(vec![envelope.clone()], dropped_units)
            .await?
        {
            WriteOutcome::Persisted => {
                bump(&self.counters.persisted_events, 1);
                bump(&self.counters.broadcast_events, 1);
                // 先日志后广播（D4/AGENTS §2.3）：仅在上方 append 成功后执行。
                let _ = self.events.send(envelope);
                Ok(SubmitOutcome::Persisted { seq, event_type })
            }
            WriteOutcome::DuplicateId => {
                bump(&self.counters.duplicate_events, 1);
                Ok(SubmitOutcome::Duplicate {
                    event_id: envelope.id,
                })
            }
        }
    }

    /// 落盘重试（ADR-007 决策 2：`MAX_WRITE_ATTEMPTS = 3` **含首次**，重试 2 次；
    /// 连续 3 次写事务尝试失败 → `persist_degraded`；每次失败输出 `attempt=n/3`）。
    async fn write_with_retry(
        &mut self,
        events: Vec<EventEnvelope>,
        dropped_units: usize,
    ) -> Result<WriteOutcome, PipelineError> {
        let attempts_limit = self.config.max_write_attempts;
        let mut attempts: usize = 0;
        let mut backpressure_waits: usize = 0;
        loop {
            match self.journal.append(events.clone()).await {
                Ok(_receipt) => return Ok(WriteOutcome::Persisted),
                Err(JournalError::DuplicateEventId) => return Ok(WriteOutcome::DuplicateId),
                Err(JournalError::Backpressure { depth, threshold }) => {
                    // ADR-004：临时高水位不进入降级；尝试等待回落，仍高则拒绝本次准入。
                    backpressure_waits += 1;
                    if backpressure_waits >= attempts_limit {
                        bump(&self.counters.backpressure_rejections, 1);
                        bump(&self.counters.dropped_events, dropped_units as u64);
                        return Err(PipelineError::StorageBackpressure { depth, threshold });
                    }
                    self.sleep_retry().await;
                }
                Err(error) => {
                    attempts += 1;
                    bump(&self.counters.persist_retries, 1);
                    let is_seq_bug = matches!(error, JournalError::DuplicateSeq { .. });
                    if is_seq_bug {
                        // D4：重复 seq 视为管线 bug，计入诊断并按持久化失败路径处理。
                        bump(&self.counters.duplicate_seq_bugs, 1);
                    }
                    // ADR-007 决策 2：每次失败尝试输出 `attempt=n/3`（含最终失败的一次）。
                    // 日志经 `tracing::warn!` 发出；验证由测试侧订阅器捕获完成
                    // （ADR-007 增量修订 1 决策 2：`health` 不承载日志内容）。
                    let attempt_line =
                        format!("attempt={attempts}/{attempts_limit}: {}", error.describe());
                    tracing::warn!(
                        attempt = attempts,
                        max_attempts = attempts_limit,
                        error = %error.describe(),
                        "持久化写事务失败（{attempt_line}），{}",
                        if attempts >= attempts_limit {
                            "进入 persist_degraded（只读）"
                        } else {
                            "将重试"
                        }
                    );
                    if attempts >= attempts_limit {
                        let origin = events
                            .first()
                            .map(|event| (event.session_id.clone(), event.runtime_id.clone()));
                        let last_error = error.describe();
                        self.enter_degraded(
                            DegradeTrigger::WriteFailure {
                                attempts: attempts as u32,
                                last_error: last_error.clone(),
                            },
                            origin,
                            dropped_units,
                        )
                        .await;
                        return Err(PipelineError::PersistDegraded {
                            reason: format!("{last_error}；连续 {attempts} 次尝试均失败（含首次）"),
                        });
                    }
                    self.sleep_retry().await;
                }
            }
        }
    }

    async fn sleep_retry(&self) {
        if !self.config.persist_retry_delay.is_zero() {
            tokio::time::sleep(self.config.persist_retry_delay).await;
        }
    }

    fn default_origin(&self) -> Option<(SessionId, RuntimeId)> {
        self.sessions.iter().find_map(|(session_id, state)| {
            state
                .last_runtime_id
                .clone()
                .map(|runtime_id| (session_id.clone(), runtime_id))
        })
    }

    /// 进入持久化降级（D4 状态机；首次触发源胜出）。
    ///
    /// 序列（D4 降级期语义）：
    /// 1. 抛弃全部未落盘 delta（不广播，计数）；
    /// 2. 在途 run 转 `cancelled` + 控制通道发出 interrupt 通知；
    /// 3. 尝试按正常管线落盘 `error(recoverable=false)`——成功才广播；失败仅经 `health`。
    async fn enter_degraded(
        &mut self,
        trigger: DegradeTrigger,
        origin: Option<(SessionId, RuntimeId)>,
        dropped_units: usize,
    ) -> bool {
        if !self.storage.enter_degraded(trigger.clone()) {
            return false;
        }
        bump(&self.counters.dropped_events, dropped_units as u64);

        let mut discarded = 0u64;
        for state in self.sessions.values_mut() {
            for buffer in state.deltas.drain(..) {
                discarded += buffer.event_count as u64;
            }
        }
        bump(&self.counters.delta_buffers_discarded, discarded);
        bump(&self.counters.dropped_events, discarded);

        let runs: Vec<(RunId, SessionId)> = self
            .in_flight_runs
            .iter()
            .map(|(run_id, session_id)| (run_id.clone(), session_id.clone()))
            .collect();
        self.in_flight_runs.clear();
        dec_usize(&self.counters.in_flight_runs, runs.len());
        for (run_id, session_id) in runs {
            bump(&self.counters.cancelled_runs, 1);
            let _ = self.run_interrupts.send(RunInterrupt {
                session_id,
                run_id,
                reason: RUN_INTERRUPT_REASON_DEGRADED.to_owned(),
                at_ms: now_ms(),
            });
        }

        if let Some((session_id, runtime_id)) = origin {
            if let Some(envelope) = self
                .degraded_error_event(&session_id, &runtime_id, &trigger)
                .await
            {
                match self.journal.append(vec![envelope.clone()]).await {
                    Ok(_) => {
                        bump(&self.counters.persisted_events, 1);
                        bump(&self.counters.broadcast_events, 1);
                        let _ = self.events.send(envelope);
                    }
                    Err(_) => {
                        // 降级即写失败场景：不广播，由 health 呈现（D4 降级期语义 4）。
                        bump(&self.counters.dropped_events, 1);
                    }
                }
            }
        }
        true
    }

    async fn degraded_error_event(
        &mut self,
        session_id: &SessionId,
        runtime_id: &RuntimeId,
        trigger: &DegradeTrigger,
    ) -> Option<EventEnvelope> {
        let seq = self.next_seq(session_id).await.ok()?;
        let id = EventId::new(ulid::generate()).ok()?;
        Some(EventEnvelope {
            v: EVENT_ENVELOPE_VERSION,
            id,
            session_id: session_id.clone(),
            run_id: None,
            runtime_id: runtime_id.clone(),
            seq,
            ts: now_ms(),
            payload: EventPayload::Error(ErrorInfo {
                code: "persist_degraded".to_owned(),
                message: format!(
                    "存储降级（persist_degraded）：{}；已进入只读，在途 run 已中断；\
                     修复外部条件后重启核心并以启动自检恢复（P0 无热恢复）",
                    trigger.describe()
                ),
                recoverable: false,
            }),
        })
    }

    fn handle_restart_session(&mut self, session_id: &SessionId) {
        let mut discarded = 0u64;
        if let Some(state) = self.sessions.get_mut(session_id) {
            for buffer in state.deltas.drain(..) {
                discarded += buffer.event_count as u64;
            }
            state.sequencer = None;
        }
        bump(&self.counters.delta_buffers_discarded, discarded);
        bump(&self.counters.sequencer_restarts, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_design_constants() {
        let config = PipelineConfig::default();
        assert_eq!(config.max_write_attempts, 3, "ADR-007：总尝试 3 次含首次");
        assert_eq!(config.max_write_attempts - 1, 2, "ADR-007：重试次数 = 2");
        assert_eq!(
            config.delta_flush_interval,
            Duration::from_millis(16),
            "D4：16ms"
        );
        assert_eq!(config.delta_flush_bytes, 8 * 1024, "D4：8KB");
        assert_eq!(config.submit_queue_capacity, 4_096);
        assert_eq!(config.broadcast_capacity, 4_096, "D8：broadcast(4096)");
        assert_eq!(READBACK_MAX_GAP, 10_000, "D4：补读上限 10k");
        assert_eq!(READBACK_PAGE_SIZE, 500, "D7：分页 ≤500");
        config.validate().expect("默认配置必须合法");
    }

    #[test]
    fn invalid_config_is_rejected() {
        let cases = [
            PipelineConfig {
                max_write_attempts: 0,
                ..PipelineConfig::default()
            },
            PipelineConfig {
                delta_flush_interval: Duration::ZERO,
                ..PipelineConfig::default()
            },
            PipelineConfig {
                delta_flush_bytes: 0,
                ..PipelineConfig::default()
            },
            PipelineConfig {
                dedup_capacity: 0,
                ..PipelineConfig::default()
            },
            PipelineConfig {
                submit_queue_capacity: 0,
                ..PipelineConfig::default()
            },
            PipelineConfig {
                broadcast_capacity: 0,
                ..PipelineConfig::default()
            },
        ];
        for config in cases {
            let error = config.validate().expect_err("非法配置必须拒绝");
            assert_eq!(error.code(), "invalid_pipeline_config");
        }
    }

    #[test]
    fn dedup_set_is_bounded() {
        let mut set = DedupSet::new(2);
        let first = EventId::new("e1").expect("id");
        let second = EventId::new("e2").expect("id");
        let third = EventId::new("e3").expect("id");
        assert!(set.insert(&first));
        assert!(!set.insert(&first), "重复必须识别");
        assert!(set.insert(&second));
        assert!(set.insert(&third), "超容量时淘汰最旧");
        assert!(set.insert(&first), "被淘汰后允许重新出现（DB 主键兜底）");
    }
}
