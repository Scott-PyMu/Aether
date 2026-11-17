//! 单写队列、group commit 与读连接池（设计 D3；实施计划 M1-04）。
//!
//! - **单写任务**：`mpsc(4096)` 汇聚全部写入；1 个写连接串行化；
//!   每 16ms 或积压 ≥256 条提交一次事务（group commit），批次内多个提交共享一个事务；
//! - **读连接池**：4 个只读连接（WAL 下与写任务并发，读不被写阻塞）；
//! - **背压分级（D8 的存储侧接口层）**：队列深度 >1024 → L1 告警通知（边沿触发）；
//!   >4096 → [`WriteQueue::admission`] 返回 `storage_backpressure`（供 M2-04 联调）；
//! - **诊断**：[`QueueMetrics`] 暴露队列深度、批次统计与提交延迟（M3-05 诊断包消费）。
//!
//! 边界（AGENTS.md §7）：本模块只负责 D3 的写入串行化与 L1/L2 信号；
//! 事件 sequencer、先日志后广播、delta 合并与 `persist_degraded` 状态机属 M1-05。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_core::{EventEnvelope, Message, SessionId};
use rusqlite::{params, Connection};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc, oneshot, watch, Semaphore};
use tokio::task::JoinHandle;

use crate::checkpoint::{
    checkpoint_truncate_with_backoff, CheckpointConfig, CheckpointReport,
    WAL_FORCE_CHECKPOINT_BYTES,
};
use crate::error::StoreError;
use crate::layout::{shard_for, StorageLayout};
use crate::ops::{apply_command, StoreCommand, StoreOutcome};
use crate::store::Store;

/// 写队列容量（D3：`mpsc::channel(4096)`）。
pub const QUEUE_CAPACITY: usize = 4_096;
/// 批量提交条数阈值（D3：积压 ≥256 条提交一次事务）。
pub const MAX_BATCH_ENTRIES: usize = 256;
/// 提交间隔（D3：16ms group commit 窗口）。
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(16);
/// L1 水位（D8：写队列 >1024 → 告警）。
pub const L1_THRESHOLD: usize = 1_024;
/// L2 水位（D8：写队列 >4096 → 拒绝新 run；`storage_backpressure`）。
pub const L2_THRESHOLD: usize = 4_096;
/// 读连接数（D3：主库 4 个读连接）。
pub const READ_CONNECTION_COUNT: usize = 4;
/// 分片读连接数（ADR-039 D-039-1：每片 1–2；主库 4 + 8×2 = 20）。
pub const SHARD_READ_CONNECTION_COUNT: usize = 2;
/// 关闭序列写队列 drain 上限（D2：3s；放弃 delta、保留控制事件）。
pub const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
/// 关闭序列读连接关闭等待上限（D2 硬超时口径；常量级参数，证据记录）。
pub const SHUTDOWN_READ_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// 告警广播通道容量（边沿通知，慢消费者丢旧值不影响正确性）。
const ALERT_CHANNEL_CAPACITY: usize = 64;

/// 写队列运行参数（默认值即 D3 约定；测试/故障注入可参数化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteQueueConfig {
    /// 队列容量（默认 4096）。
    pub capacity: usize,
    /// 单批条数阈值（默认 256）。
    pub max_batch_entries: usize,
    /// 提交间隔（默认 16ms）。
    pub flush_interval: Duration,
    /// L1 告警水位（默认 1024）。
    pub l1_threshold: usize,
    /// L2 准入拒绝水位（默认 4096）。
    pub l2_threshold: usize,
    /// 读连接数（默认 4）。
    pub read_connections: usize,
    /// 提交前人为延迟（**测试/故障注入专用**：制造可控积压以验证 L1/L2 与准入接口；默认 0）。
    pub commit_delay: Duration,
}

impl Default for WriteQueueConfig {
    fn default() -> Self {
        Self {
            capacity: QUEUE_CAPACITY,
            max_batch_entries: MAX_BATCH_ENTRIES,
            flush_interval: FLUSH_INTERVAL,
            l1_threshold: L1_THRESHOLD,
            l2_threshold: L2_THRESHOLD,
            read_connections: READ_CONNECTION_COUNT,
            commit_delay: Duration::ZERO,
        }
    }
}

impl WriteQueueConfig {
    /// 校验参数（容量/批量/间隔/读连接数必须为正；L1 ≤ L2）。
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.capacity == 0 {
            return Err(StoreError::InvalidWriteQueueConfig {
                reason: "capacity 必须 >0".to_owned(),
            });
        }
        if self.max_batch_entries == 0 {
            return Err(StoreError::InvalidWriteQueueConfig {
                reason: "max_batch_entries 必须 >0".to_owned(),
            });
        }
        if self.flush_interval.is_zero() {
            return Err(StoreError::InvalidWriteQueueConfig {
                reason: "flush_interval 必须 >0".to_owned(),
            });
        }
        if self.read_connections == 0 {
            return Err(StoreError::InvalidWriteQueueConfig {
                reason: "read_connections 必须 >0".to_owned(),
            });
        }
        if self.l1_threshold > self.l2_threshold {
            return Err(StoreError::InvalidWriteQueueConfig {
                reason: "l1_threshold 不得大于 l2_threshold".to_owned(),
            });
        }
        Ok(())
    }
}

/// 背压等级（D8）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePressureLevel {
    /// 写队列 >1024：告警 + delta 合并批次放宽（M1-05 消费）。
    L1,
    /// 写队列 >4096：拒绝新工作准入（`storage_backpressure`）。
    L2,
}

/// 告警边沿（进入/退出某等级）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePressurePhase {
    /// 跨过水位向上（Enter）。
    Enter,
    /// 回落到水位之下（Exit）。
    Exit,
}

/// 写队列告警通知（边沿触发；M1-05 将 L1 映射为 `log`(warn) 事件广播，M2-04 消费 L2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteQueueAlert {
    pub level: QueuePressureLevel,
    pub phase: QueuePressurePhase,
    /// 触发时的队列深度（待提交条目数）。
    pub depth: usize,
    /// 该等级的水位阈值。
    pub threshold: usize,
    /// 触发时间（Unix epoch 毫秒）。
    pub at_ms: i64,
}

/// 批次触发方式（DoD2：16ms 或 ≥256 条）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchTrigger {
    /// 累积到 ≥256 条（写入侧触发）。
    Count,
    /// 16ms 提交窗口到期（定时触发）。
    Timer,
    /// 批次收集期间到达领域写命令（命令优先级：先提交事件批次，再执行命令，保持 FIFO）。
    Command,
    /// 批次收集期间到达 checkpoint 请求（M2-06；先提交事件批次，保持 FIFO）。
    Checkpoint,
    /// 关停 drain 触发的最后一批。
    Shutdown,
}

/// 提交回执（仅在事务 `COMMIT` 成功后返回；未落盘不会给出成功回执）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReceipt {
    /// 本作业携带的事件条数。
    pub entries: usize,
    /// 本批次合计条数（group commit 后的实际批量）。
    pub batch_entries: usize,
    /// 事务持续时间（毫秒；begin → commit 返回；D3 目标单事务 <500ms）。
    pub commit_ms: u64,
    /// 批次触发方式。
    pub trigger: BatchTrigger,
    /// 入队时间（Unix epoch 毫秒）。
    pub submitted_at_ms: i64,
    /// 提交完成时间（Unix epoch 毫秒）。
    pub committed_at_ms: i64,
}

/// 队列诊断快照（队列深度进诊断；M3-05 消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueMetrics {
    /// 待提交条目数（队列 + 在途批次）——诊断指标「队列深度」。
    pub depth: usize,
    /// 队列容量。
    pub capacity: usize,
    /// 在途批次条数（提交中）。
    pub pending_batch_entries: usize,
    /// 已提交批次数。
    pub committed_batches: u64,
    /// 已提交条目数。
    pub committed_entries: u64,
    /// 提交失败条目数（重复 seq、磁盘满等；M1-05 据此进入降级判定）。
    pub failed_entries: u64,
    /// 单批最大条数。
    pub max_batch_entries: usize,
    /// L1 告警次数（进入电平）。
    pub l1_alerts: u64,
    /// L2 告警次数（进入电平）。
    pub l2_alerts: u64,
    /// 最近一次事务耗时（毫秒）。
    pub last_commit_ms: u64,
    /// 最大事务耗时（毫秒）。
    pub max_commit_ms: u64,
    /// 当前背压等级（正常为 `None`）。
    pub pressure_level: Option<QueuePressureLevel>,
}

/// 写队列内部消息。
enum WriteMessage {
    /// 事件 journal 批量追加（M1-05 主路径；一个作业内多事件共享一个事务）。
    Events {
        events: Vec<EventEnvelope>,
        reply: oneshot::Sender<Result<CommitReceipt, StoreError>>,
        submitted_at_ms: i64,
    },
    /// 领域写命令（M2-01/M2-03：消息/run/会话/权限/审计；单命令单事务）。
    Command {
        /// 盒装以缩小 `WriteMessage` 尺寸（clippy::large_enum_variant）。
        command: Box<StoreCommand>,
        reply: oneshot::Sender<Result<StoreOutcome, StoreError>>,
        submitted_at_ms: i64,
    },
    /// 关闭序列阶段 1（M2-06）：FIFO drain 在途消息；随后写任务在 checkpoint 待命态
    /// 等待阶段 3 的 [`WriteMessage::Checkpoint`]（写连接保持打开）。
    Shutdown { reply: oneshot::Sender<()> },
    /// `wal_checkpoint(TRUNCATE)` 请求（运行期强制 checkpoint 与关闭序列阶段 3 共用）。
    Checkpoint {
        config: CheckpointConfig,
        reply: oneshot::Sender<CheckpointReport>,
    },
    /// 维护态屏障（ADR-039 D-039-5）：FIFO 处理完此前全部写入后回执。
    Drain { reply: oneshot::Sender<()> },
}

impl WriteMessage {
    fn entry_count(&self) -> usize {
        match self {
            Self::Events { events, .. } => events.len(),
            // 命令按 1 条计（准入水位与队列容量口径统一）。
            Self::Command { .. } => 1,
            Self::Shutdown { .. } | Self::Checkpoint { .. } | Self::Drain { .. } => 0,
        }
    }
}

/// 队列计数器（原子，进诊断）。
#[derive(Debug, Default)]
struct QueueCounters {
    submitted_entries: AtomicU64,
    resolved_entries: AtomicU64,
    committed_entries: AtomicU64,
    failed_entries: AtomicU64,
    pending_batch_entries: AtomicUsize,
    committed_batches: AtomicU64,
    max_batch_entries: AtomicUsize,
    l1_alerts: AtomicU64,
    l2_alerts: AtomicU64,
    last_commit_ms: AtomicU64,
    max_commit_ms: AtomicU64,
    /// 0 = 正常；1 = L1；2 = L2。
    pressure_level: AtomicUsize,
}

/// 维护态写入闸门（ADR-039 D-039-5）：暂停新写入并在 drain 后放行维护操作。
#[derive(Debug)]
struct WriteGate {
    paused: watch::Sender<bool>,
    /// 启动检查期：写请求快速拒绝（`storage_backpressure`），避免启动尾段阻塞。
    rejecting: AtomicBool,
    /// 保持至少一个 receiver，避免 `watch::Sender::send` 因无订阅者失败。
    _keepalive: watch::Receiver<bool>,
}

impl WriteGate {
    fn new() -> Self {
        let (paused, keepalive) = watch::channel(false);
        Self {
            paused,
            rejecting: AtomicBool::new(false),
            _keepalive: keepalive,
        }
    }

    async fn wait_open(&self) {
        let mut receiver = self.paused.subscribe();
        if *receiver.borrow() {
            let _ = receiver.wait_for(|paused| !*paused).await;
        }
    }

    fn pause(&self) {
        self.rejecting.store(false, Ordering::Release);
        let _ = self.paused.send(true);
    }

    fn pause_rejecting(&self) {
        self.rejecting.store(true, Ordering::Release);
        let _ = self.paused.send(true);
    }

    fn resume(&self) {
        self.rejecting.store(false, Ordering::Release);
        let _ = self.paused.send(false);
    }

    fn is_rejecting(&self) -> bool {
        self.rejecting.load(Ordering::Acquire)
    }
}

/// 维护态写入闸门守卫（Drop 恢复）。
pub struct MaintenanceWriteGuard {
    gate: Arc<WriteGate>,
}

impl Drop for MaintenanceWriteGuard {
    fn drop(&mut self) {
        self.gate.resume();
    }
}

/// 写队列句柄（克隆共享同一队列；写任务由 [`StoreRuntime`] 持有）。
#[derive(Clone)]
pub struct WriteQueue {
    sender: mpsc::Sender<WriteMessage>,
    counters: Arc<QueueCounters>,
    alerts: broadcast::Sender<WriteQueueAlert>,
    config: Arc<WriteQueueConfig>,
    gate: Arc<WriteGate>,
}

impl WriteQueue {
    /// 事件 journal 入队并等待落盘（D4：返回成功 = 已提交，调用方才可广播）。
    ///
    /// - 队列满时本方法 await 容量（写队列是 D8 允许的唯一反压例外，由上层暂停控制事件读取）；
    /// - 事务失败（含 `events UNIQUE(session_id, seq)` 兜底命中）返回
    ///   [`StoreError::WriteTransactionFailed`]，未落盘事件不得广播（M1-05）。
    pub async fn append_events(
        &self,
        events: Vec<EventEnvelope>,
    ) -> Result<CommitReceipt, StoreError> {
        if events.is_empty() {
            return Err(StoreError::EmptyWriteBatch);
        }
        if self.gate.is_rejecting() {
            return Err(StoreError::StorageBackpressure {
                depth: 0,
                threshold: 0,
            });
        }
        self.gate.wait_open().await;
        let entries = u64::try_from(events.len()).unwrap_or(u64::MAX);
        let (reply, reply_rx) = oneshot::channel();
        self.counters
            .submitted_entries
            .fetch_add(entries, Ordering::AcqRel);
        let message = WriteMessage::Events {
            events,
            reply,
            submitted_at_ms: now_ms(),
        };
        if self.sender.send(message).await.is_err() {
            self.counters
                .resolved_entries
                .fetch_add(entries, Ordering::AcqRel);
            return Err(StoreError::WriteQueueClosed);
        }
        review_pressure(&self.counters, &self.config, &self.alerts);
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => {
                self.counters
                    .resolved_entries
                    .fetch_add(entries, Ordering::AcqRel);
                Err(StoreError::WriteQueueClosed)
            }
        }
    }

    /// 当前队列深度（待提交条目数，含在途批次）。
    pub fn depth(&self) -> usize {
        depth_of(&self.counters)
    }

    /// 领域写命令入队并等待执行（M2-01/M2-03；单写者串行，返回即已提交）。
    ///
    /// - 队列满时 await 容量（与事件 journal 共用唯一的 D8 反压例外）；
    /// - 命令在写任务内独立事务执行：成功 = `COMMIT` 已返回；
    /// - 与事件批次的顺序保持 FIFO（命令到达前的批次先提交）。
    pub async fn execute(&self, command: StoreCommand) -> Result<StoreOutcome, StoreError> {
        // 启动检查期允许非事件命令（会话创建/审计等）完成，避免启动尾段阻塞；
        // 事件写入仍由 append_events 快速拒绝。维护态（rejecting=false,paused=true）继续阻塞。
        if !self.gate.is_rejecting() {
            self.gate.wait_open().await;
        }
        let (reply, reply_rx) = oneshot::channel();
        self.counters
            .submitted_entries
            .fetch_add(1, Ordering::AcqRel);
        let message = WriteMessage::Command {
            command: Box::new(command),
            reply,
            submitted_at_ms: now_ms(),
        };
        if self.sender.send(message).await.is_err() {
            self.counters
                .resolved_entries
                .fetch_add(1, Ordering::AcqRel);
            return Err(StoreError::WriteQueueClosed);
        }
        review_pressure(&self.counters, &self.config, &self.alerts);
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => {
                self.counters
                    .resolved_entries
                    .fetch_add(1, Ordering::AcqRel);
                Err(StoreError::WriteQueueClosed)
            }
        }
    }

    /// 诊断快照。
    pub fn metrics(&self) -> QueueMetrics {
        let pressure_level = match self.counters.pressure_level.load(Ordering::Acquire) {
            2 => Some(QueuePressureLevel::L2),
            1 => Some(QueuePressureLevel::L1),
            _ => None,
        };
        QueueMetrics {
            depth: depth_of(&self.counters),
            capacity: self.config.capacity,
            pending_batch_entries: self.counters.pending_batch_entries.load(Ordering::Acquire),
            committed_batches: self.counters.committed_batches.load(Ordering::Relaxed),
            committed_entries: self.counters.committed_entries.load(Ordering::Relaxed),
            failed_entries: self.counters.failed_entries.load(Ordering::Relaxed),
            max_batch_entries: self.counters.max_batch_entries.load(Ordering::Relaxed),
            l1_alerts: self.counters.l1_alerts.load(Ordering::Relaxed),
            l2_alerts: self.counters.l2_alerts.load(Ordering::Relaxed),
            last_commit_ms: self.counters.last_commit_ms.load(Ordering::Relaxed),
            max_commit_ms: self.counters.max_commit_ms.load(Ordering::Relaxed),
            pressure_level,
        }
    }

    /// 订阅 L1/L2 告警边沿（广播；无订阅者时静默丢弃）。
    pub fn subscribe_alerts(&self) -> broadcast::Receiver<WriteQueueAlert> {
        self.alerts.subscribe()
    }

    /// 当前背压等级（正常为 `None`）。
    pub fn pressure_level(&self) -> Option<QueuePressureLevel> {
        self.metrics().pressure_level
    }

    /// 新工作准入检查（D8 L2 接口，供 M2-04 联调）。
    ///
    /// 队列深度 >L2 阈值（默认 4096）时返回
    /// [`StoreError::StorageBackpressure`]（错误码 `storage_backpressure`）。
    pub fn admission(&self) -> Result<(), StoreError> {
        let depth = self.depth();
        if depth > self.config.l2_threshold {
            return Err(StoreError::StorageBackpressure {
                depth,
                threshold: self.config.l2_threshold,
            });
        }
        Ok(())
    }

    /// M10-10 启动检查：暂停新写入，期间写请求快速返回
    /// `storage_backpressure`（不阻塞启动尾段；维护态仍用 `pause_and_drain` 阻塞）。
    pub fn pause_writes(&self) {
        self.gate.pause_rejecting();
    }

    /// M10-10 启动检查通过：恢复写入。
    pub fn resume_writes(&self) {
        self.gate.resume();
    }

    /// 进入维护态：暂停新写入，FIFO drain 此前全部消息，返回 Drop 恢复守卫。
    ///
    /// 用于备份/恢复（ADR-039 D-039-5）：调用方持有守卫期间执行备份/恢复，
    /// 结束时 Drop 恢复写入。drain 只保证此前消息已处理；调用方仍应在维护态
    /// 命令层拒绝其他写命令（`MaintenanceCell`）。
    pub async fn pause_and_drain(&self) -> Result<MaintenanceWriteGuard, StoreError> {
        self.gate.pause();
        let (reply, reply_rx) = oneshot::channel();
        self.sender
            .send(WriteMessage::Drain { reply })
            .await
            .map_err(|_| {
                self.gate.resume();
                StoreError::WriteQueueClosed
            })?;
        match reply_rx.await {
            Ok(()) => Ok(MaintenanceWriteGuard {
                gate: Arc::clone(&self.gate),
            }),
            Err(_) => {
                self.gate.resume();
                Err(StoreError::WriteQueueClosed)
            }
        }
    }

    /// 在写连接上执行 `wal_checkpoint(TRUNCATE)`（运行期强制 checkpoint 入口；M2-06）。
    ///
    /// 读锁未释放时按 [`CheckpointConfig`] 退避重试并返回诊断（`succeeded=false` 为
    /// 结果数据而非调用错误，由调用方记录诊断）；写任务已退出时返回
    /// [`StoreError::WriteQueueClosed`]。
    pub async fn checkpoint_truncate(
        &self,
        config: CheckpointConfig,
    ) -> Result<CheckpointReport, StoreError> {
        config.validate()?;
        let (reply, reply_rx) = oneshot::channel();
        self.sender
            .send(WriteMessage::Checkpoint { config, reply })
            .await
            .map_err(|_| StoreError::WriteQueueClosed)?;
        reply_rx.await.map_err(|_| StoreError::WriteQueueClosed)
    }
}

/// 读连接关闭报告（M2-06 关闭序列步骤 2 的诊断；进 shadow 日志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadPoolCloseReport {
    /// 配置的读连接数。
    pub connections: usize,
    /// 实际拆除的读连接数。
    pub closed: usize,
    /// 是否未在超时内取得全部许可（存在长期占用的读操作）。
    pub timed_out: bool,
}

/// 读连接池（D3：4 个只读连接；WAL 下读写并发）。
///
/// 关闭语义（M2-06）：[`ReadPool::close`] 拆除全部当前读连接（checkpoint 前置条件，
/// 释放 WAL 读锁）；此后读取按需惰性重开只读连接（降级/退出后的诊断与备份读取仍可用，
/// D4 降级期读语义）。
#[derive(Clone)]
pub struct ReadPool {
    path: Arc<PathBuf>,
    connections: Arc<Vec<Arc<Mutex<Option<Connection>>>>>,
    permits: Arc<Semaphore>,
    cursor: Arc<AtomicUsize>,
    /// ADR-039：分片读连接池（`None` = 单库模式/安全模式）。
    shards: Option<Arc<ShardReadPools>>,
}

/// 分片读连接池（每片独立池；总数主库 4 + 8×2 = 20）。
#[derive(Clone)]
struct ShardReadPools {
    pools: Arc<Vec<ReadPool>>,
    paths: Arc<Vec<PathBuf>>,
}

impl ReadPool {
    fn open(path: &Path, size: usize) -> Result<Self, StoreError> {
        let mut connections = Vec::with_capacity(size);
        for _ in 0..size {
            connections.push(Arc::new(Mutex::new(Some(Store::open_read_only(path)?))));
        }
        Ok(Self {
            path: Arc::new(path.to_path_buf()),
            connections: Arc::new(connections),
            permits: Arc::new(Semaphore::new(size)),
            cursor: Arc::new(AtomicUsize::new(0)),
            shards: None,
        })
    }

    /// 主库 + 分片读连接池（ADR-039；生成模式）。
    fn open_with_shards(
        path: &Path,
        size: usize,
        shard_paths: &[PathBuf],
        shard_size: usize,
    ) -> Result<Self, StoreError> {
        let mut pool = Self::open(path, size)?;
        let mut pools = Vec::with_capacity(shard_paths.len());
        for shard_path in shard_paths {
            pools.push(Self::open(shard_path, shard_size.max(1))?);
        }
        pool.shards = Some(Arc::new(ShardReadPools {
            pools: Arc::new(pools),
            paths: Arc::new(shard_paths.to_vec()),
        }));
        Ok(pool)
    }

    /// 分片数（单库模式 = 0）。
    pub fn shard_count(&self) -> usize {
        self.shards
            .as_ref()
            .map(|shards| shards.pools.len())
            .unwrap_or(0)
    }

    /// 按会话稳定哈希取分片读池。
    fn shard_pool(&self, session_id: &str) -> Option<ReadPool> {
        let shards = self.shards.as_ref()?;
        let index = usize::from(shard_for(session_id));
        shards.pools.get(index).cloned()
    }

    /// 在指定分片读连接上执行查询（归档/保留/诊断逐片读取；单库模式返回错误）。
    pub async fn with_shard_connection<T, F>(
        &self,
        shard_id: u8,
        operation: F,
    ) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let shards = self.shards.as_ref().ok_or_else(|| StoreError::Internal {
            reason: "单库模式没有事件分片".to_owned(),
        })?;
        let pool = shards
            .pools
            .get(usize::from(shard_id))
            .cloned()
            .ok_or_else(|| StoreError::Internal {
                reason: format!("分片 id 越界：{shard_id}"),
            })?;
        pool.with_connection(operation).await
    }

    /// 分片路径（单库模式为空）。
    pub fn shard_paths(&self) -> Vec<PathBuf> {
        self.shards
            .as_ref()
            .map(|shards| shards.paths.as_ref().clone())
            .unwrap_or_default()
    }

    /// 只读读连接池（M9-13 R10-A：`quick_check` 失败安全模式下的维护例外）。
    ///
    /// 安全模式没有写者，但 `backup_restore` 仍需读取备份台账/设置；该入口只建立
    /// 只读连接（`SQLITE_OPEN_READ_ONLY`），不启动写任务、不迁移、不写库。
    pub fn open_read_only(path: impl AsRef<Path>, size: usize) -> Result<Self, StoreError> {
        Self::open(path.as_ref(), size.max(1))
    }

    /// 读连接数（配置值）。
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// 当前已建立的读连接数（诊断/断言；`close` 后为 0，惰性重开后 >0）。
    pub fn open_connection_count(&self) -> usize {
        self.connections
            .iter()
            .filter(|slot| slot.lock().map(|guard| guard.is_some()).unwrap_or(false))
            .count()
    }

    /// 借用一个读连接执行查询（信号量限流 4 并发；阻塞查询在 blocking 线程执行）。
    pub async fn with_connection<T, F>(&self, operation: F) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        if self.connections.is_empty() {
            return Err(StoreError::Internal {
                reason: "读连接池为空".to_owned(),
            });
        }
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| StoreError::WriteQueueClosed)?;
        let index = self.cursor.fetch_add(1, Ordering::Relaxed) % self.connections.len();
        let slot = Arc::clone(&self.connections[index]);
        let path = Arc::clone(&self.path);
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = slot.lock().map_err(|_| StoreError::Internal {
                reason: "读连接互斥锁中毒".to_owned(),
            })?;
            if guard.is_none() {
                *guard = Some(Store::open_read_only(&path)?);
            }
            match guard.as_ref() {
                Some(connection) => operation(connection),
                None => Err(StoreError::Internal {
                    reason: "读连接不可用".to_owned(),
                }),
            }
        })
        .await
        .map_err(|_| StoreError::Internal {
            reason: "读任务异常退出".to_owned(),
        })?;
        drop(permit);
        result
    }

    /// 关闭全部读连接（M2-06 关闭序列步骤 2）：等待在途读操作完成后拆除连接。
    ///
    /// - `timeout` 内未取得全部读连接许可（存在长期占用）→ 记录 `timed_out`，
    ///   尽力拆除当前空闲连接（退出优先，D2）；
    /// - 返回报告供 shadow 日志记录。
    pub async fn close(&self, timeout: Duration) -> ReadPoolCloseReport {
        let mut report = self.close_self(timeout).await;
        if let Some(shards) = &self.shards {
            for pool in shards.pools.iter() {
                let shard_report = pool.close_self(timeout).await;
                report.connections += shard_report.connections;
                report.closed += shard_report.closed;
                report.timed_out |= shard_report.timed_out;
            }
        }
        report
    }

    async fn close_self(&self, timeout: Duration) -> ReadPoolCloseReport {
        let total = self.connections.len();
        let mut report = ReadPoolCloseReport {
            connections: total,
            closed: 0,
            timed_out: false,
        };
        if total > 0 {
            if let Ok(count) = u32::try_from(total) {
                match tokio::time::timeout(timeout, self.permits.clone().acquire_many_owned(count))
                    .await
                {
                    Ok(Ok(permits)) => {
                        report.closed = self.take_idle_connections();
                        drop(permits);
                    }
                    _ => {
                        report.timed_out = true;
                        report.closed = self.take_idle_connections();
                    }
                }
            } else {
                report.timed_out = true;
                report.closed = self.take_idle_connections();
            }
        }
        report
    }

    /// 拆除当前空闲的读连接（在途读持有的连接跳过）；返回拆除数。
    fn take_idle_connections(&self) -> usize {
        let mut closed = 0;
        for slot in self.connections.iter() {
            if let Ok(mut guard) = slot.try_lock() {
                if guard.take().is_some() {
                    closed += 1;
                }
            }
        }
        closed
    }

    /// 会话事件计数（诊断/基准；ADR-039 按会话路由到分片）。
    pub async fn event_count(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        let session = session_id.as_str().to_owned();
        let operation = move |conn: &Connection| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                [session.as_str()],
                |row| row.get(0),
            )?;
            u64::try_from(count).map_err(|_| StoreError::Internal {
                reason: format!("events 计数为负: {count}"),
            })
        };
        match self.shard_pool(session_id.as_str()) {
            Some(pool) => pool.with_connection(operation).await,
            None => self.with_connection(operation).await,
        }
    }

    /// 会话最大 seq（M1-05 sequencer 崩溃恢复：seq = max+1；ADR-039 按会话路由）。
    pub async fn max_seq(&self, session_id: &SessionId) -> Result<Option<u64>, StoreError> {
        let session = session_id.as_str().to_owned();
        let operation = move |conn: &Connection| {
            // 聚合查询恒返回一行；无匹配行时为 NULL。
            let max: Option<i64> = conn.query_row(
                "SELECT MAX(seq) FROM events WHERE session_id = ?1",
                [session.as_str()],
                |row| row.get::<_, Option<i64>>(0),
            )?;
            match max {
                Some(value) => match u64::try_from(value) {
                    Ok(value) => Ok(Some(value)),
                    Err(_) => Err(StoreError::Internal {
                        reason: format!("events.seq 为负: {value}"),
                    }),
                },
                None => Ok(None),
            }
        };
        match self.shard_pool(session_id.as_str()) {
            Some(pool) => pool.with_connection(operation).await,
            None => self.with_connection(operation).await,
        }
    }

    /// 补读页（升序，至多 `limit` 条；M1-05 断点续传入口；ADR-039 按会话路由）。
    ///
    /// - `after_seq = Some(last_seq)`：补读 `seq > last_seq`（UI 断点续传）；
    /// - `after_seq = None`：从会话起点读取（首屏/缓存清空后重载）。
    pub async fn events_page(
        &self,
        session_id: &SessionId,
        after_seq: Option<u64>,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let session = session_id.as_str().to_owned();
        let operation = move |conn: &Connection| read_events_page(conn, &session, after_seq, limit);
        match self.shard_pool(session_id.as_str()) {
            Some(pool) => pool.with_connection(operation).await,
            None => self.with_connection(operation).await,
        }
    }

    /// 最近一页事件（按 seq 降序取 `limit` 条后升序返回；M3-02 缓存清空后重载；ADR-039 按会话路由）。
    ///
    /// 语义与 [`ReadPool::events_page`] 的升序输出一致；`limit` 为 0 时返回空。
    pub async fn events_latest(
        &self,
        session_id: &SessionId,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let session = session_id.as_str().to_owned();
        let operation = move |conn: &Connection| read_events_latest(conn, &session, limit);
        match self.shard_pool(session_id.as_str()) {
            Some(pool) => pool.with_connection(operation).await,
            None => self.with_connection(operation).await,
        }
    }

    /// 最近一页消息（按 seq 降序取 `limit` 条后升序返回；M3-02 工作台消息基线）。
    ///
    /// 与 [`ReadPool::messages_page`]（`after_seq = None` 从会话起点读）语义区分：
    /// 本方法取**尾部**一页（缓存清空后重载工作台基线）；输出升序，与
    /// [`ReadPool::events_latest`] 同口径。`messages.seq` 与 `events.seq` 为两条
    /// 独立序列；`limit` 为 0 时返回空。
    pub async fn messages_latest(
        &self,
        session_id: &SessionId,
        limit: usize,
    ) -> Result<Vec<Message>, StoreError> {
        let session = session_id.as_str().to_owned();
        self.with_connection(move |conn| read_messages_latest(conn, &session, limit))
            .await
    }
}

/// 最近一页消息实现（降序取 + 反转，保持升序契约；列与 `ops::messages_page` 一致）。
fn read_messages_latest(
    connection: &Connection,
    session_id: &str,
    limit: usize,
) -> Result<Vec<Message>, StoreError> {
    let limit = i64::try_from(limit).map_err(|_| StoreError::Internal {
        reason: format!("分页上限超出 SQLite INTEGER 范围: {limit}"),
    })?;
    let mut statement = connection.prepare(
        "SELECT id, session_id, run_id, client_msg_id, role, content, content_parts, tool_calls, \
         parent_message_id, seq, created_at FROM messages \
         WHERE session_id = ?1 ORDER BY seq DESC LIMIT ?2",
    )?;
    let rows = statement.query_map(params![session_id, limit], crate::ops::read_message_row)?;
    let mut messages = Vec::new();
    for row in rows {
        messages.push(row?);
    }
    messages.reverse();
    Ok(messages)
}

/// 最近一页事件实现（降序取 + 反转，保持升序契约）。
fn read_events_latest(
    connection: &Connection,
    session_id: &str,
    limit: usize,
) -> Result<Vec<EventEnvelope>, StoreError> {
    let limit = i64::try_from(limit).map_err(|_| StoreError::Internal {
        reason: format!("分页上限超出 SQLite INTEGER 范围: {limit}"),
    })?;
    let mut statement = connection.prepare(
        "SELECT id, session_id, run_id, runtime_id, seq, type, payload, ts, v \
         FROM events WHERE session_id = ?1 ORDER BY seq DESC LIMIT ?2",
    )?;
    let rows = statement.query_map(params![session_id, limit], |row| {
        Ok(StoredEventRow {
            id: row.get(0)?,
            session_id: row.get(1)?,
            run_id: row.get(2)?,
            runtime_id: row.get(3)?,
            seq: row.get(4)?,
            event_type: row.get(5)?,
            payload: row.get(6)?,
            ts: row.get(7)?,
            v: row.get(8)?,
        })
    })?;
    let mut events = Vec::new();
    for row in rows {
        events.push(row?.into_envelope()?);
    }
    events.reverse();
    Ok(events)
}

/// 关闭序列步骤（D2：`写队列 drain → 关闭全部读连接 → wal_checkpoint(TRUNCATE)
/// → 关闭写连接 → 退出`；M2-06 收口存储侧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownStep {
    /// 写队列 drain（D2：3s 上限；放弃未落盘 delta、保留已入队控制事件）。
    DrainWriteQueue,
    /// 关闭全部读连接（checkpoint 前置条件：无其他连接持有 WAL 读锁）。
    CloseReadConnections,
    /// `wal_checkpoint(TRUNCATE)`。
    WalCheckpointTruncate,
    /// 关闭写连接。
    CloseWriteConnection,
    /// 退出（关闭序列完成）。
    Exit,
}

impl ShutdownStep {
    /// D2 冻结顺序（五步）。
    pub const D2_ORDER: [ShutdownStep; 5] = [
        ShutdownStep::DrainWriteQueue,
        ShutdownStep::CloseReadConnections,
        ShutdownStep::WalCheckpointTruncate,
        ShutdownStep::CloseWriteConnection,
        ShutdownStep::Exit,
    ];

    /// 诊断字段值（稳定 snake_case）。
    pub const fn code(self) -> &'static str {
        match self {
            Self::DrainWriteQueue => "drain_write_queue",
            Self::CloseReadConnections => "close_read_connections",
            Self::WalCheckpointTruncate => "wal_checkpoint_truncate",
            Self::CloseWriteConnection => "close_write_connection",
            Self::Exit => "exit",
        }
    }
}

/// shadow 日志条目（关闭序列顺序断言与诊断导出；M3-05 消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownStepRecord {
    /// 步骤标识。
    pub step: ShutdownStep,
    /// 稳定字符串码（`step.code()`）。
    pub code: &'static str,
    /// 步骤开始时间（Unix epoch 毫秒）。
    pub at_ms: i64,
    /// 步骤耗时（毫秒）。
    pub duration_ms: u64,
    /// 步骤诊断（不含密钥）。
    pub detail: String,
}

/// 关闭序列配置（默认值即 D2/D3 约定；故障注入可参数化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownConfig {
    /// 写队列 drain 上限（D2：3s）。
    pub drain_timeout: Duration,
    /// 关闭读连接的等待上限（在途读操作完成）。
    pub read_close_timeout: Duration,
    /// checkpoint 退避（D3：读锁失败 → 退避重试并记录诊断）。
    pub checkpoint: CheckpointConfig,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            drain_timeout: SHUTDOWN_DRAIN_TIMEOUT,
            read_close_timeout: SHUTDOWN_READ_CLOSE_TIMEOUT,
            checkpoint: CheckpointConfig::default(),
        }
    }
}

impl ShutdownConfig {
    /// 校验（超时为正值；checkpoint 配置合法）。
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.drain_timeout.is_zero() {
            return Err(StoreError::InvalidShutdownConfig {
                reason: "drain_timeout 必须 >0".to_owned(),
            });
        }
        if self.read_close_timeout.is_zero() {
            return Err(StoreError::InvalidShutdownConfig {
                reason: "read_close_timeout 必须 >0".to_owned(),
            });
        }
        self.checkpoint.validate()
    }
}

/// 关闭序列报告（含 shadow 日志；DoD1 顺序断言的唯一来源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// shadow 日志（五步顺序与每步诊断）。
    pub steps: Vec<ShutdownStepRecord>,
    /// 写队列是否在 drain 上限内完成。
    pub drained: bool,
    /// drain 是否超时（超时 = 终止写任务后以临时写连接完成 checkpoint）。
    pub drain_timed_out: bool,
    /// 读连接关闭报告。
    pub read_close: ReadPoolCloseReport,
    /// checkpoint 结果（drain 超时且临时连接失败时为 `None`）。
    pub checkpoint: Option<CheckpointReport>,
    /// 关闭序列整体耗时（毫秒）。
    pub duration_ms: u64,
}

impl ShutdownReport {
    /// 实际步骤顺序。
    pub fn order(&self) -> Vec<ShutdownStep> {
        self.steps.iter().map(|record| record.step).collect()
    }

    /// 步骤顺序是否与 D2 完全一致（五步）。
    pub fn matches_d2_order(&self) -> bool {
        self.order() == ShutdownStep::D2_ORDER.to_vec()
    }

    /// 取指定步骤记录。
    pub fn step(&self, step: ShutdownStep) -> Option<&ShutdownStepRecord> {
        self.steps.iter().find(|record| record.step == step)
    }

    /// shadow 日志单行摘要（诊断展示）。
    pub fn shadow_log(&self) -> String {
        self.steps
            .iter()
            .map(|record| {
                format!(
                    "{}[{}ms] {}",
                    record.code, record.duration_ms, record.detail
                )
            })
            .collect::<Vec<_>>()
            .join(" → ")
    }
}

/// 存储运行时：单写任务（[`WriteQueue`]）+ 读连接池（[`ReadPool`]）。
pub struct StoreRuntime {
    path: PathBuf,
    /// ADR-039：generation 布局（`None` = 单库模式，仅供既有单库测试/工具）。
    layout: Option<StorageLayout>,
    /// 是否在正常关闭后写 `clean_shutdown.json`（降级/故障路径关闭）。
    write_clean_sidecar: bool,
    /// M10-10：后台启动检查是否完成（写入 sidecar，供下次严格跳过）。
    background_check_completed: Arc<AtomicBool>,
    /// 上次完整 `integrity_check` 时间（0 = 从未）；写入 sidecar。
    last_full_check_at: Arc<AtomicI64>,
    /// 连续干净启动次数（20 次触发完整检查；成功后归零）。
    clean_start_count: Arc<AtomicU32>,
    queue: WriteQueue,
    reads: ReadPool,
    writer: Option<JoinHandle<()>>,
}

impl StoreRuntime {
    /// 打开库（PRAGMA → `quick_check` → 迁移，复用 M1-03 [`Store::open`]）并启动单写任务。
    ///
    /// - 安全模式（`quick_check` 失败）下拒绝启动写运行时（返回
    ///   [`StoreError::SafeModeWriteRefused`]）；只读/备份/导出入口继续由 [`Store`] 提供；
    /// - 写任务 spawn 到 `handle`（核心统一 tokio 多线程运行时，D2）。
    pub fn open(
        path: impl AsRef<Path>,
        config: WriteQueueConfig,
        handle: &Handle,
    ) -> Result<Self, StoreError> {
        config.validate()?;
        let store = Store::open(path)?;
        let (connection, path) = store.into_writer_connection()?;
        let reads = ReadPool::open(&path, config.read_connections)?;

        let counters = Arc::new(QueueCounters::default());
        let config = Arc::new(config);
        let (sender, receiver) = mpsc::channel(config.capacity);
        let (alerts, _receiver) = broadcast::channel(ALERT_CHANNEL_CAPACITY);
        let queue = WriteQueue {
            sender,
            counters: Arc::clone(&counters),
            alerts: alerts.clone(),
            config: Arc::clone(&config),
            gate: Arc::new(WriteGate::new()),
        };
        let writer = handle.spawn(writer_loop(
            receiver,
            connection,
            Vec::new(),
            Vec::new(),
            Arc::clone(&config),
            Arc::clone(&counters),
            alerts,
        ));
        Ok(Self {
            path,
            layout: None,
            write_clean_sidecar: false,
            background_check_completed: Arc::new(AtomicBool::new(false)),
            last_full_check_at: Arc::new(AtomicI64::new(0)),
            clean_start_count: Arc::new(AtomicU32::new(0)),
            queue,
            reads,
            writer: Some(writer),
        })
    }

    /// 打开 generation（主库 + 8 分片）：单写任务持有每片连接；读池按会话路由。
    ///
    /// - 任一库 `quick_check` 失败 → 拒绝启动写运行时（`SafeModeWriteRefused`）；
    /// - 启动断言主库 `events=0`、`sessions.shard_id`/`deletion_pending` 有效（ADR-039 D-039-6）；
    /// - 启动时对 `deletion_pending=1` 会话执行幂等 tombstone 收尾（D-039-8）。
    pub fn open_generation(
        layout: StorageLayout,
        config: WriteQueueConfig,
        handle: &Handle,
    ) -> Result<Self, StoreError> {
        Self::open_generation_with(layout, config, handle, false)
    }

    /// M10-10 快速启动：打开主库 + 8 分片写连接但**不执行** `quick_check`，
    /// 写门初始为关闭；调用方必须在后台检查完成后调用 [`WriteQueue::resume_writes`]。
    /// 结构不变量（主库 `events=0`、`sessions.shard_id` 等）仍同步校验。
    pub fn open_generation_deferred(
        layout: StorageLayout,
        config: WriteQueueConfig,
        handle: &Handle,
    ) -> Result<Self, StoreError> {
        Self::open_generation_with(layout, config, handle, true)
    }

    fn open_generation_with(
        layout: StorageLayout,
        config: WriteQueueConfig,
        handle: &Handle,
        defer_check: bool,
    ) -> Result<Self, StoreError> {
        config.validate()?;
        let main_store = if defer_check {
            Store::open_deferred_check(layout.main_db())?
        } else {
            Store::open(layout.main_db())?
        };
        let (main_connection, _) = main_store.into_writer_connection()?;
        let mut shard_connections = Vec::with_capacity(layout.shard_paths().len());
        for (shard_id, path) in layout.shard_paths().iter().enumerate() {
            let store = if defer_check {
                Store::open_deferred_check(path)?
            } else {
                Store::open(path)?
            };
            if let Some(reason) = store.safe_mode_reason() {
                return Err(StoreError::SafeModeWriteRefused {
                    reason: format!("分片 {shard_id} quick_check 失败：{reason}"),
                });
            }
            let (connection, _) = store.into_writer_connection()?;
            shard_connections.push(connection);
        }
        crate::layout::assert_invariants(&layout)?;
        let pending = crate::layout::pending_deletions(&layout)?;
        let reads = ReadPool::open_with_shards(
            layout.main_db(),
            config.read_connections,
            layout.shard_paths(),
            SHARD_READ_CONNECTION_COUNT,
        )?;

        let counters = Arc::new(QueueCounters::default());
        let config = Arc::new(config);
        let (sender, receiver) = mpsc::channel(config.capacity);
        let (alerts, _receiver) = broadcast::channel(ALERT_CHANNEL_CAPACITY);
        let queue = WriteQueue {
            sender,
            counters: Arc::clone(&counters),
            alerts: alerts.clone(),
            config: Arc::clone(&config),
            gate: Arc::new(WriteGate::new()),
        };
        if defer_check {
            queue.pause_writes();
        }
        let writer = handle.spawn(writer_loop(
            receiver,
            main_connection,
            shard_connections,
            pending,
            Arc::clone(&config),
            Arc::clone(&counters),
            alerts,
        ));
        let sidecar = crate::layout::read_clean_shutdown(&layout)
            .filter(|sidecar| sidecar.generation == layout.generation());
        // ADR-039 D-039-2 / P2-3：`clean_start_count` 仅在「干净启动且 sidecar 全匹配」
        // 时 +1；异常启动/sidecar 不匹配/版本升级/恢复/导入后重置为 0。
        let strict_start = crate::layout::plan_startup_checks(&layout, env!("CARGO_PKG_VERSION"))
            .strict_ok;
        let last_full_check_at = sidecar
            .as_ref()
            .and_then(|sidecar| sidecar.last_full_check_at)
            .unwrap_or(0);
        let clean_start_count = if strict_start {
            sidecar
                .as_ref()
                .map(|sidecar| sidecar.clean_start_count.saturating_add(1))
                .unwrap_or(0)
        } else {
            0
        };
        Ok(Self {
            path: layout.main_db().to_path_buf(),
            layout: Some(layout),
            write_clean_sidecar: true,
            background_check_completed: Arc::new(AtomicBool::new(false)),
            last_full_check_at: Arc::new(AtomicI64::new(last_full_check_at)),
            clean_start_count: Arc::new(AtomicU32::new(clean_start_count)),
            queue,
            reads,
            writer: Some(writer),
        })
    }

    /// 关闭前禁用 `clean_shutdown.json`（降级/检查失败/故障路径；M10-10 严格 sidecar）。
    pub fn disable_clean_sidecar(&mut self) {
        self.write_clean_sidecar = false;
    }

    /// M10-10：启动检查通过后恢复写入（与 `open_generation_deferred` 配对）。
    pub fn resume_writes(&self) {
        self.queue.resume_writes();
    }

    /// M10-10：后台检查完成标志（写入 shutdown sidecar）。
    pub fn background_check_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.background_check_completed)
    }

    /// 周检状态（last_full_check_at / clean_start_count），供后台检查任务更新。
    pub fn full_check_state(&self) -> (Arc<AtomicI64>, Arc<AtomicU32>) {
        (
            Arc::clone(&self.last_full_check_at),
            Arc::clone(&self.clean_start_count),
        )
    }

    /// M10-10：标记后台启动检查完成（调用方在检查全部通过后调用）。
    pub fn mark_background_check_completed(&self) {
        self.background_check_completed
            .store(true, Ordering::Release);
    }

    /// 上次完整 `integrity_check` 时间（0 = 从未）。
    pub fn last_full_check_at(&self) -> Option<i64> {
        let value = self.last_full_check_at.load(Ordering::Acquire);
        (value > 0).then_some(value)
    }

    /// 连续干净启动次数。
    pub fn clean_start_count(&self) -> u32 {
        self.clean_start_count.load(Ordering::Acquire)
    }

    /// 周检（7 天）或 20 次启动触发完整 `integrity_check`。
    pub fn full_check_due(&self, now_ms: i64) -> bool {
        if self.clean_start_count() >= 20 {
            return true;
        }
        self.last_full_check_at()
            .is_some_and(|last| now_ms.saturating_sub(last) >= 7 * 24 * 60 * 60 * 1000)
    }

    /// 完整检查成功：记录时间并重置 20 次计数。
    pub fn mark_full_check_completed(&self, now_ms: i64) {
        self.last_full_check_at.store(now_ms, Ordering::Release);
        self.clean_start_count.store(0, Ordering::Release);
    }

    /// generation 布局（单库模式 `None`）。
    pub fn layout(&self) -> Option<&StorageLayout> {
        self.layout.as_ref()
    }

    /// 数据库路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 写队列句柄。
    pub fn queue(&self) -> &WriteQueue {
        &self.queue
    }

    /// 读连接池。
    pub fn reads(&self) -> &ReadPool {
        &self.reads
    }

    /// `-wal` 文件路径（SQLite 侧车文件命名）。
    pub fn wal_path(&self) -> PathBuf {
        sqlite_sidecar_path(&self.path, "-wal")
    }

    /// `-shm` 文件路径（SQLite 侧车文件命名）。
    pub fn shm_path(&self) -> PathBuf {
        sqlite_sidecar_path(&self.path, "-shm")
    }

    /// `-wal` 当前字节数（文件不存在 → 0）。
    pub fn wal_size_bytes(&self) -> u64 {
        std::fs::metadata(self.wal_path())
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    }

    /// 运行期 checkpoint 请求（经写任务在写连接上执行；不判断阈值）。
    pub async fn checkpoint_truncate(
        &self,
        config: CheckpointConfig,
    ) -> Result<CheckpointReport, StoreError> {
        self.queue.checkpoint_truncate(config).await
    }

    /// 运行期强制 checkpoint（D3：WAL > 阈值 → `wal_checkpoint(TRUNCATE)`）。
    ///
    /// WAL 未超阈值返回 `None`；超阈值返回报告（读锁失败时含退避重试诊断）。
    pub async fn checkpoint_if_wal_exceeds(
        &self,
        threshold: u64,
        config: CheckpointConfig,
    ) -> Result<Option<CheckpointReport>, StoreError> {
        config.validate()?;
        if self.wal_size_bytes() <= threshold {
            return Ok(None);
        }
        self.checkpoint_truncate(config).await.map(Some)
    }

    /// D3 默认运行期入口：WAL >256MB 强制 checkpoint（失败退避重试 + 诊断）。
    pub async fn maintenance_checkpoint(&self) -> Result<Option<CheckpointReport>, StoreError> {
        self.checkpoint_if_wal_exceeds(WAL_FORCE_CHECKPOINT_BYTES, CheckpointConfig::default())
            .await
    }

    /// 完整关闭序列（M2-06；D2 存储侧五步，默认配置）：
    /// drain → 关闭全部读连接 → `wal_checkpoint(TRUNCATE)` → 关闭写连接 → 退出。
    pub async fn shutdown(self) -> Result<ShutdownReport, StoreError> {
        self.shutdown_with(ShutdownConfig::default()).await
    }

    /// 完整关闭序列（参数化：故障注入/超时口径）。
    ///
    /// - 步骤 1（drain）超时：终止写任务并放弃在途写入（无成功回执），以临时写连接
    ///   完成 checkpoint（D2「保证能退出优先」）；
    /// - 步骤 3 checkpoint 失败（读锁/临时连接失败）：记录诊断后继续关闭（不阻断退出）；
    /// - 返回的 [`ShutdownReport`] 携带 shadow 日志与各步诊断。
    pub async fn shutdown_with(
        mut self,
        config: ShutdownConfig,
    ) -> Result<ShutdownReport, StoreError> {
        config.validate()?;
        let started = std::time::Instant::now();
        let mut steps: Vec<ShutdownStepRecord> = Vec::with_capacity(5);

        // 步骤 1：写队列 drain（D2：3s 上限；放弃 delta、保留控制事件）
        let step_started = std::time::Instant::now();
        let depth_before = self.queue.depth();
        let (reply, reply_rx) = oneshot::channel();
        let mut drained = false;
        let mut drain_timed_out = false;
        if self
            .queue
            .sender
            .send(WriteMessage::Shutdown { reply })
            .await
            .is_ok()
        {
            match tokio::time::timeout(config.drain_timeout, reply_rx).await {
                Ok(Ok(())) => drained = true,
                Ok(Err(_closed)) => {}
                Err(_elapsed) => drain_timed_out = true,
            }
        }
        let depth_after = self.queue.depth();
        steps.push(step_record(
            ShutdownStep::DrainWriteQueue,
            step_started,
            format!(
                "depth {depth_before}→{depth_after}; drained={drained}; timed_out={drain_timed_out}; timeout={}ms",
                config.drain_timeout.as_millis()
            ),
        ));

        // 步骤 2：关闭全部读连接（checkpoint 要求无其他连接持有 WAL 读锁）
        let step_started = std::time::Instant::now();
        let read_close = self.reads.close(config.read_close_timeout).await;
        steps.push(step_record(
            ShutdownStep::CloseReadConnections,
            step_started,
            format!(
                "closed={}/{}; timed_out={}",
                read_close.closed, read_close.connections, read_close.timed_out
            ),
        ));

        // 步骤 3：wal_checkpoint(TRUNCATE)（主库 + 全部分片）
        let step_started = std::time::Instant::now();
        let wal_before = self.wal_size_bytes();
        let (checkpoint, checkpoint_detail) = if drained {
            match self
                .queue
                .checkpoint_truncate(config.checkpoint.clone())
                .await
            {
                Ok(report) => {
                    let detail = report.diagnostic_summary();
                    (Some(report), detail)
                }
                Err(error) => self.checkpoint_via_temporary_connection(
                    &config.checkpoint,
                    format!("写任务已退出（{error}）"),
                ),
            }
        } else {
            // drain 未完成：终止写任务（放弃未落盘事件，无成功回执），临时写连接兜底。
            if let Some(writer) = self.writer.take() {
                writer.abort();
                let _ = writer.await;
            }
            self.checkpoint_via_temporary_connection(
                &config.checkpoint,
                "drain 未完成：写任务已终止".to_owned(),
            )
        };
        let wal_after = self.wal_size_bytes();
        let checkpoint_ok = checkpoint.as_ref().is_some_and(|report| report.succeeded);
        steps.push(step_record(
            ShutdownStep::WalCheckpointTruncate,
            step_started,
            format!("wal_bytes {wal_before}→{wal_after}；{checkpoint_detail}"),
        ));

        // 步骤 4：关闭写连接（等待写任务退出；连接随任务结束释放）
        let step_started = std::time::Instant::now();
        let mut writer_error: Option<String> = None;
        if let Some(writer) = self.writer.take() {
            if writer.await.is_err() {
                writer_error = Some("写任务已终止（drain 超时）".to_owned());
            }
        }
        steps.push(step_record(
            ShutdownStep::CloseWriteConnection,
            step_started,
            match &writer_error {
                Some(error) => format!("closed=true; {error}"),
                None => "closed=true".to_owned(),
            },
        ));

        // ADR-039 附录 B：关闭序列完成 + drain + checkpoint 成功才写 clean sidecar。
        if self.write_clean_sidecar && drained && checkpoint_ok {
            if let Err(error) = self.write_clean_shutdown_sidecar() {
                tracing::warn!(%error, "clean_shutdown.json 写入失败（下次启动全量检查）");
            }
        }

        // 步骤 5：退出
        steps.push(step_record(
            ShutdownStep::Exit,
            std::time::Instant::now(),
            "关闭序列完成".to_owned(),
        ));

        Ok(ShutdownReport {
            steps,
            drained,
            drain_timed_out,
            read_close,
            checkpoint,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }

    /// drain 未完成/写任务已退出时的 checkpoint 兜底：主库 + 全部分片临时写连接执行后立即关闭。
    fn checkpoint_via_temporary_connection(
        &self,
        config: &CheckpointConfig,
        context: String,
    ) -> (Option<CheckpointReport>, String) {
        let paths: Vec<PathBuf> = match &self.layout {
            Some(layout) => layout.all_db_paths(),
            None => vec![self.path.clone()],
        };
        let mut reports = Vec::with_capacity(paths.len());
        let mut failures = Vec::new();
        for path in &paths {
            match Store::open_writer_connection(path) {
                Ok(connection) => {
                    reports.push(checkpoint_truncate_with_backoff(&connection, config))
                }
                Err(error) => failures.push(format!("{}：{error}", path.display())),
            }
        }
        if reports.is_empty() {
            return (
                None,
                format!(
                    "{context}；临时写连接打开失败，checkpoint 未执行：{}",
                    failures.join("；")
                ),
            );
        }
        let aggregate = aggregate_checkpoint_reports(&reports);
        let detail = format!(
            "{context}；临时写连接（{} 库）：{}{}",
            reports.len(),
            aggregate.diagnostic_summary(),
            if failures.is_empty() {
                String::new()
            } else {
                format!("；打开失败：{}", failures.join("；"))
            }
        );
        (Some(aggregate), detail)
    }

    /// 写 `clean_shutdown.json`（M10-09：generation 匹配；M10-10 严格跳过判定）。
    fn write_clean_shutdown_sidecar(&self) -> Result<(), StoreError> {
        let Some(layout) = self.layout.as_ref() else {
            return Ok(());
        };
        let clean_start_count = self.clean_start_count();
        let last_full_check_at = self.last_full_check_at();
        let mut shards = Vec::with_capacity(layout.shard_paths().len());
        for (shard_id, path) in layout.shard_paths().iter().enumerate() {
            let last_seq = crate::layout::shard_last_seq(path)?;
            shards.push(crate::layout::ShardFingerprint {
                shard_id: u8::try_from(shard_id).unwrap_or(0),
                database: crate::layout::database_fingerprint(path, last_seq)?,
            });
        }
        let sidecar = crate::layout::CleanShutdown {
            generation: layout.generation().to_owned(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            closed_at: now_ms(),
            close_sequence_completed: true,
            write_queue_drained: true,
            background_check_completed: self
                .background_check_completed
                .load(Ordering::Acquire),
            last_full_check_at,
            clean_start_count,
            main: crate::layout::database_fingerprint(layout.main_db(), None)?,
            shards,
        };
        crate::layout::write_clean_shutdown(layout, &sidecar)
    }
}

/// SQLite 侧车文件路径（`<db>-wal` / `<db>-shm`）。
fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

/// 构造 shadow 日志条目。
fn step_record(
    step: ShutdownStep,
    started: std::time::Instant,
    detail: String,
) -> ShutdownStepRecord {
    ShutdownStepRecord {
        step,
        code: step.code(),
        at_ms: now_ms(),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        detail,
    }
}

/// 写任务主循环：批收集（16ms / ≥256 条）→ 单事务提交 → 回执。
async fn writer_loop(
    mut receiver: mpsc::Receiver<WriteMessage>,
    mut connection: Connection,
    mut shards: Vec<Connection>,
    pending_deletions: Vec<String>,
    config: Arc<WriteQueueConfig>,
    counters: Arc<QueueCounters>,
    alerts: broadcast::Sender<WriteQueueAlert>,
) {
    if !pending_deletions.is_empty() {
        match finalize_pending_deletions(&mut connection, &mut shards, &pending_deletions) {
            Ok(count) => tracing::info!(count, "启动恢复：完成 deletion_pending 会话删除"),
            Err(error) => {
                tracing::warn!(%error, "启动恢复：deletion_pending 会话删除失败（保留 tombstone 待重试）")
            }
        }
    }
    // 上一轮批次收集期间到达的命令/checkpoint 消息（保持 FIFO，避免重排）。
    let mut deferred: Option<WriteMessage> = None;
    loop {
        let mut batch: Vec<WriteMessage> = Vec::new();
        let mut pending_shutdown: Option<oneshot::Sender<()>> = None;
        let mut batch_entries = 0usize;

        let first = match deferred.take() {
            Some(message) => Some(message),
            None => receiver.recv().await,
        };
        let Some(first) = first else {
            break;
        };
        match first {
            WriteMessage::Shutdown { reply } => {
                // M2-06：drain 已确认 → 保持写连接打开，等待关闭序列阶段 3 的 checkpoint。
                let _ = reply.send(());
                let _ = await_shutdown_checkpoint(connection, shards, &mut receiver).await;
                break;
            }
            WriteMessage::Checkpoint {
                config: checkpoint_config,
                reply,
            } => {
                let report = checkpoint_all(&connection, &shards, &checkpoint_config);
                let _ = reply.send(report);
                continue;
            }
            WriteMessage::Drain { reply } => {
                let _ = reply.send(());
                continue;
            }
            WriteMessage::Command {
                command,
                reply,
                submitted_at_ms,
            } => {
                // 命令独立事务立即执行（不参与事件批次，保持到达顺序）。
                execute_command_message(
                    &mut connection,
                    &mut shards,
                    &counters,
                    *command,
                    reply,
                    submitted_at_ms,
                );
                review_pressure(&counters, &config, &alerts);
                continue;
            }
            events @ WriteMessage::Events { .. } => {
                batch_entries += events.entry_count();
                batch.push(events);
            }
        }

        let deadline = tokio::time::Instant::now() + config.flush_interval;
        let trigger = loop {
            if batch_entries >= config.max_batch_entries {
                break BatchTrigger::Count;
            }
            match tokio::time::timeout_at(deadline, receiver.recv()).await {
                Ok(Some(message)) => match message {
                    WriteMessage::Events { .. } => {
                        batch_entries += message.entry_count();
                        batch.push(message);
                    }
                    WriteMessage::Shutdown { reply } => {
                        pending_shutdown = Some(reply);
                        break BatchTrigger::Shutdown;
                    }
                    checkpoint @ WriteMessage::Checkpoint { .. } => {
                        // checkpoint 到达：先提交已收集的事件批次（FIFO），checkpoint 留待下轮。
                        deferred = Some(checkpoint);
                        break BatchTrigger::Checkpoint;
                    }
                    command @ WriteMessage::Command { .. } => {
                        // 命令到达：先提交已收集的事件批次（FIFO），命令留待下轮。
                        deferred = Some(command);
                        break BatchTrigger::Command;
                    }
                    drain @ WriteMessage::Drain { .. } => {
                        // 维护态屏障：先提交已收集的事件批次，再回执 drain。
                        deferred = Some(drain);
                        break BatchTrigger::Command;
                    }
                },
                Ok(None) => break BatchTrigger::Shutdown,
                Err(_elapsed) => break BatchTrigger::Timer,
            }
        };

        counters
            .pending_batch_entries
            .store(batch_entries, Ordering::Release);
        if !config.commit_delay.is_zero() {
            tokio::time::sleep(config.commit_delay).await;
        }
        let started = std::time::Instant::now();
        let outcome = commit_events_batch(&mut connection, &mut shards, &batch);
        let commit_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        counters.pending_batch_entries.store(0, Ordering::Release);
        counters.last_commit_ms.store(commit_ms, Ordering::Relaxed);
        counters
            .max_commit_ms
            .fetch_max(commit_ms, Ordering::Relaxed);
        let committed_at_ms = now_ms();

        let failures = outcome;
        if failures.iter().all(Option::is_none) {
            counters.committed_batches.fetch_add(1, Ordering::Relaxed);
            counters
                .max_batch_entries
                .fetch_max(batch_entries, Ordering::Relaxed);
            let entries = u64::try_from(batch_entries).unwrap_or(u64::MAX);
            counters
                .committed_entries
                .fetch_add(entries, Ordering::Relaxed);
            counters
                .resolved_entries
                .fetch_add(entries, Ordering::AcqRel);
            for message in batch {
                if let WriteMessage::Events {
                    events,
                    reply,
                    submitted_at_ms,
                } = message
                {
                    let _ = reply.send(Ok(CommitReceipt {
                        entries: events.len(),
                        batch_entries,
                        commit_ms,
                        trigger,
                        submitted_at_ms,
                        committed_at_ms,
                    }));
                }
            }
        } else {
            counters.failed_entries.fetch_add(
                u64::try_from(batch_entries).unwrap_or(u64::MAX),
                Ordering::AcqRel,
            );
            counters.resolved_entries.fetch_add(
                u64::try_from(batch_entries).unwrap_or(u64::MAX),
                Ordering::AcqRel,
            );
            for job in batch {
                if let WriteMessage::Events {
                    events,
                    reply,
                    submitted_at_ms,
                } = job
                {
                    let failed = if shards.is_empty() {
                        failures.first().cloned().flatten()
                    } else {
                        events.iter().find_map(|event| {
                            failures
                                .get(usize::from(shard_for(event.session_id.as_str())))
                                .cloned()
                                .flatten()
                        })
                    };
                    match failed {
                        Some((code, message)) => {
                            let _ = reply
                                .send(Err(StoreError::WriteTransactionFailed { code, message }));
                        }
                        None => {
                            let _ = reply.send(Ok(CommitReceipt {
                                entries: events.len(),
                                batch_entries,
                                commit_ms,
                                trigger,
                                submitted_at_ms,
                                committed_at_ms,
                            }));
                        }
                    }
                }
            }
        }

        review_pressure(&counters, &config, &alerts);

        if let Some(reply) = pending_shutdown.take() {
            let _ = reply.send(());
            let _ = await_shutdown_checkpoint(connection, shards, &mut receiver).await;
            break;
        }
    }
    counters.pending_batch_entries.store(0, Ordering::Release);
}

/// 关闭序列：drain 确认后在写连接上等待并执行 checkpoint（M2-06 / D2）。
///
/// 取走写连接所有权（`Connection` 为 `Send`，等待期间不跨线程共享；checkpoint 完成后
/// 连接随函数返回释放，即 D2 步骤 4「关闭写连接」）。返回是否执行了 checkpoint：
/// 发送端全部关闭（未收到 checkpoint）时返回 `false`，写连接随任务退出关闭。
/// 待命期间到达的写入一律以 [`StoreError::WriteQueueClosed`] 拒绝
/// （关停后不得再产生成功收据）。
async fn await_shutdown_checkpoint(
    connection: Connection,
    shards: Vec<Connection>,
    receiver: &mut mpsc::Receiver<WriteMessage>,
) -> bool {
    loop {
        match receiver.recv().await {
            Some(WriteMessage::Checkpoint {
                config: checkpoint_config,
                reply,
            }) => {
                let report = checkpoint_all(&connection, &shards, &checkpoint_config);
                let _ = reply.send(report);
                return true;
            }
            Some(WriteMessage::Events { reply, .. }) => {
                let _ = reply.send(Err(StoreError::WriteQueueClosed));
            }
            Some(WriteMessage::Command { reply, .. }) => {
                let _ = reply.send(Err(StoreError::WriteQueueClosed));
            }
            Some(WriteMessage::Shutdown { reply }) => {
                // 重复关停标记：幂等确认（drain 已完成）。
                let _ = reply.send(());
            }
            Some(WriteMessage::Drain { reply }) => {
                // 关停待命态：此前写入已 drain，直接回执。
                let _ = reply.send(());
            }
            None => return false,
        }
    }
}

/// 执行一条领域写命令并回执（单写任务内串行；成功 = `COMMIT` 已返回）。
fn execute_command_message(
    connection: &mut Connection,
    shards: &mut [Connection],
    counters: &QueueCounters,
    command: StoreCommand,
    reply: oneshot::Sender<Result<StoreOutcome, StoreError>>,
    _submitted_at_ms: i64,
) {
    let started = std::time::Instant::now();
    let outcome = apply_command_multi(connection, shards, &command);
    let commit_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    counters.last_commit_ms.store(commit_ms, Ordering::Relaxed);
    counters
        .max_commit_ms
        .fetch_max(commit_ms, Ordering::Relaxed);
    match outcome {
        Ok(outcome) => {
            counters.committed_entries.fetch_add(1, Ordering::Relaxed);
            counters.resolved_entries.fetch_add(1, Ordering::AcqRel);
            let _ = reply.send(Ok(outcome));
        }
        Err(error) => {
            counters.failed_entries.fetch_add(1, Ordering::AcqRel);
            counters.resolved_entries.fetch_add(1, Ordering::AcqRel);
            let _ = reply.send(Err(error));
        }
    }
}

/// 多库写命令：`DeleteSession` 走 tombstone 协议；`DeleteEventsByIds` 按分片；
/// 其余命令仍在主库单事务执行（ADR-039 D-039-8；不做跨片事务）。
fn apply_command_multi(
    main: &mut Connection,
    shards: &mut [Connection],
    command: &StoreCommand,
) -> Result<StoreOutcome, StoreError> {
    match command {
        StoreCommand::DeleteSession { id } => {
            if shards.is_empty() {
                apply_command(main, command)
            } else {
                delete_session_tombstone(main, shards, id.as_str())
            }
        }
        StoreCommand::DeleteEventsByIds { .. } => {
            // 单库模式（既有测试/工具）在主线执行；生成模式必须指定分片。
            if shards.is_empty() {
                apply_command(main, command)
            } else {
                Err(StoreError::Internal {
                    reason: "生成模式 DeleteEventsByIds 必须使用 DeleteEventsInShard".to_owned(),
                })
            }
        }
        StoreCommand::DeleteEventsInShard { shard_id, rowids } => {
            let connection =
                shards
                    .get_mut(usize::from(*shard_id))
                    .ok_or_else(|| StoreError::Internal {
                        reason: format!("分片 id 越界：{shard_id}"),
                    })?;
            apply_delete_events(connection, rowids)
        }
        _ => apply_command(main, command),
    }
}

fn apply_delete_events(
    connection: &mut Connection,
    rowids: &[i64],
) -> Result<StoreOutcome, StoreError> {
    if rowids.is_empty() {
        return Ok(StoreOutcome::Applied { affected: 0 });
    }
    let transaction = connection.transaction()?;
    let mut affected = 0usize;
    for chunk in rowids.chunks(crate::ops::DELETE_EVENTS_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("DELETE FROM events WHERE rowid IN ({placeholders})");
        affected = affected
            .saturating_add(transaction.execute(&sql, rusqlite::params_from_iter(chunk.iter()))?);
    }
    transaction.commit()?;
    Ok(StoreOutcome::Applied { affected })
}

/// 会话删除 tombstone（ADR-039 D-039-8 §2）：
/// 主库置 `deletion_pending=1` → 分片删除 events（单分片事务、幂等）→ 主库删行（级联）。
///
/// 任一步失败保留 tombstone，返回错误；重试/启动恢复继续；不得先删主库（孤儿事件）。
fn delete_session_tombstone(
    main: &mut Connection,
    shards: &mut [Connection],
    session_id: &str,
) -> Result<StoreOutcome, StoreError> {
    let (shard_id, already_pending, counts) = {
        let transaction = main.transaction()?;
        let row = transaction.query_row(
            "SELECT status, shard_id, deletion_pending FROM sessions WHERE id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        );
        let (status, shard_id, pending) = match row {
            Ok(values) => values,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Ok(StoreOutcome::SessionNotFound);
            }
            Err(error) => return Err(error.into()),
        };
        if pending == 0 {
            let active_runs: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM runs WHERE session_id = ?1 AND status IN ('queued','running')",
                [session_id],
                |row| row.get(0),
            )?;
            let pending_permissions: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM permissions WHERE session_id = ?1 AND status = 'pending'",
                [session_id],
                |row| row.get(0),
            )?;
            let busy_status = matches!(
                status.as_str(),
                "waiting_permission" | "creating" | "running"
            );
            if busy_status || active_runs > 0 || pending_permissions > 0 {
                return Ok(StoreOutcome::SessionBusy {
                    active_runs: u64::try_from(active_runs).unwrap_or(u64::MAX),
                    pending_permissions: u64::try_from(pending_permissions).unwrap_or(u64::MAX),
                });
            }
            transaction.execute(
                "UPDATE sessions SET deletion_pending = 1 WHERE id = ?1",
                [session_id],
            )?;
        }
        let counts = (
            transaction.query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, i64>(0),
            )?,
            transaction.query_row(
                "SELECT COUNT(*) FROM runs WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, i64>(0),
            )?,
            transaction.query_row(
                "SELECT COUNT(*) FROM artifacts WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, i64>(0),
            )?,
            transaction.query_row(
                "SELECT COUNT(*) FROM permissions WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, i64>(0),
            )?,
        );
        transaction.commit()?;
        let shard_id = match shard_id {
            Some(value) => u8::try_from(value).map_err(|_| StoreError::ShardInvariant {
                reason: format!("会话 {session_id} 的 shard_id 越界：{value}"),
            })?,
            None => {
                return Err(StoreError::ShardInvariant {
                    reason: format!("会话 {session_id} 的 shard_id 为空"),
                })
            }
        };
        (shard_id, pending == 1, counts)
    };

    let shard =
        shards
            .get_mut(usize::from(shard_id))
            .ok_or_else(|| StoreError::ShardInvariant {
                reason: format!("会话 {session_id} 指向不存在的分片 {shard_id}"),
            })?;
    {
        let transaction = shard.transaction()?;
        transaction.execute("DELETE FROM events WHERE session_id = ?1", [session_id])?;
        transaction.commit()?;
    }

    let transaction = main.transaction()?;
    let affected = transaction.execute(
        "DELETE FROM sessions WHERE id = ?1 AND deletion_pending = 1",
        [session_id],
    )?;
    transaction.commit()?;
    if affected == 0 && !already_pending {
        return Ok(StoreOutcome::SessionNotFound);
    }
    Ok(StoreOutcome::SessionDeleted {
        messages: u64::try_from(counts.0).unwrap_or(u64::MAX),
        runs: u64::try_from(counts.1).unwrap_or(u64::MAX),
        artifacts: u64::try_from(counts.2).unwrap_or(u64::MAX),
        permissions: u64::try_from(counts.3).unwrap_or(u64::MAX),
        events_retained: 0,
    })
}

/// 启动恢复：重试全部 `deletion_pending=1` 会话（幂等）。
fn finalize_pending_deletions(
    main: &mut Connection,
    shards: &mut [Connection],
    session_ids: &[String],
) -> Result<usize, StoreError> {
    let mut completed = 0usize;
    for session_id in session_ids {
        match delete_session_tombstone(main, shards, session_id) {
            Ok(StoreOutcome::SessionDeleted { .. }) | Ok(StoreOutcome::SessionNotFound) => {
                completed += 1;
            }
            Ok(other) => {
                tracing::warn!(session_id = %session_id, outcome = ?other, "deletion_pending 收尾未完成");
            }
            Err(error) => {
                tracing::warn!(session_id = %session_id, error = %error, "deletion_pending 收尾失败");
            }
        }
    }
    Ok(completed)
}

/// 逐分片 checkpoint（无跨片事务；主库 + 全部分片合计诊断）。
fn checkpoint_all(
    main: &Connection,
    shards: &[Connection],
    config: &CheckpointConfig,
) -> CheckpointReport {
    let mut reports = Vec::with_capacity(1 + shards.len());
    reports.push(checkpoint_truncate_with_backoff(main, config));
    for shard in shards {
        reports.push(checkpoint_truncate_with_backoff(shard, config));
    }
    aggregate_checkpoint_reports(&reports)
}

fn aggregate_checkpoint_reports(reports: &[CheckpointReport]) -> CheckpointReport {
    let mut aggregate = CheckpointReport {
        attempts: 0,
        succeeded: true,
        busy: false,
        frames_log: 0,
        frames_checkpointed: 0,
        error: None,
        history: Vec::new(),
        duration_ms: 0,
    };
    for report in reports {
        aggregate.attempts = aggregate.attempts.max(report.attempts);
        aggregate.succeeded &= report.succeeded;
        aggregate.busy |= report.busy;
        if report.frames_log >= 0 {
            aggregate.frames_log = aggregate.frames_log.saturating_add(report.frames_log);
        } else {
            aggregate.frames_log = -1;
        }
        if report.frames_checkpointed >= 0 {
            aggregate.frames_checkpointed = aggregate
                .frames_checkpointed
                .saturating_add(report.frames_checkpointed);
        } else {
            aggregate.frames_checkpointed = -1;
        }
        if aggregate.error.is_none() {
            aggregate.error = report.error.clone();
        }
        aggregate.history.extend(report.history.iter().cloned());
        aggregate.duration_ms = aggregate.duration_ms.saturating_add(report.duration_ms);
    }
    aggregate
}

/// 单写任务按 `session_id` 路由到分片；每片一个事务（无跨片事务）。
///
/// 返回每个分片的失败信息（`None` = 该片提交成功）；单库模式（`shards` 为空）
/// 使用主库连接（既有测试/工具兼容）。
fn commit_events_batch(
    main: &mut Connection,
    shards: &mut [Connection],
    batch: &[WriteMessage],
) -> Vec<Option<(Option<i32>, String)>> {
    let single_db = shards.is_empty();
    let shard_count = if single_db { 1 } else { shards.len() };
    let mut failures: Vec<Option<(Option<i32>, String)>> = vec![None; shard_count];
    for (shard_id, failure) in failures.iter_mut().enumerate() {
        let events: Vec<&EventEnvelope> = batch
            .iter()
            .filter_map(|message| match message {
                WriteMessage::Events { events, .. } => Some(events),
                _ => None,
            })
            .flatten()
            .filter(|event| {
                if single_db {
                    return true;
                }
                usize::from(shard_for(event.session_id.as_str())) == shard_id
            })
            .collect();
        if events.is_empty() {
            continue;
        }
        let connection: &mut Connection = if single_db {
            main
        } else {
            match shards.get_mut(shard_id) {
                Some(connection) => connection,
                None => {
                    *failure = Some((None, format!("分片连接缺失：{shard_id}")));
                    continue;
                }
            }
        };
        if let Err(error) = commit_events_to_connection(connection, &events) {
            *failure = Some(error);
        }
    }
    failures
}

fn commit_events_to_connection(
    connection: &mut Connection,
    events: &[&EventEnvelope],
) -> Result<(), (Option<i32>, String)> {
    let transaction = connection.transaction().map_err(classify_sqlite)?;
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .map_err(classify_sqlite)?;
        for event in events {
            let payload = event
                .payload
                .to_value()
                .map_err(|error| (None, format!("事件 payload 无法序列化: {error}")))?;
            let seq = i64::try_from(event.seq)
                .map_err(|_| (None, format!("seq 超出 SQLite INTEGER 范围: {}", event.seq)))?;
            statement
                .execute(params![
                    event.id.as_str(),
                    event.session_id.as_str(),
                    event.run_id.as_ref().map(|id| id.as_str()),
                    event.runtime_id.as_str(),
                    seq,
                    event.event_type().as_str(),
                    payload.to_string(),
                    event.ts,
                    event.v,
                ])
                .map_err(classify_sqlite)?;
        }
    }
    transaction.commit().map_err(classify_sqlite)
}

/// 从 `rusqlite::Error` 提取可跨作业复制的失败信息（扩展错误码 + 文案）。
fn classify_sqlite(error: rusqlite::Error) -> (Option<i32>, String) {
    match &error {
        rusqlite::Error::SqliteFailure(failure, message) => (
            Some(failure.extended_code),
            message.clone().unwrap_or_else(|| error.to_string()),
        ),
        _ => (None, error.to_string()),
    }
}

/// 队列深度（提交中 + 待提交）。
fn depth_of(counters: &QueueCounters) -> usize {
    let submitted = counters.submitted_entries.load(Ordering::Acquire);
    let resolved = counters.resolved_entries.load(Ordering::Acquire);
    usize::try_from(submitted.saturating_sub(resolved)).unwrap_or(usize::MAX)
}

/// 依据当前深度复核背压电平；跨级时发出边沿告警（D8）。
fn review_pressure(
    counters: &QueueCounters,
    config: &WriteQueueConfig,
    alerts: &broadcast::Sender<WriteQueueAlert>,
) {
    let depth = depth_of(counters);
    let previous = counters.pressure_level.load(Ordering::Acquire);
    let current = if depth > config.l2_threshold {
        2
    } else if depth > config.l1_threshold {
        1
    } else {
        0
    };
    if current == previous {
        return;
    }
    if counters
        .pressure_level
        .compare_exchange(previous, current, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    for level in (previous + 1)..=current {
        emit_alert(
            counters,
            config,
            alerts,
            level,
            QueuePressurePhase::Enter,
            depth,
        );
    }
    for level in ((current + 1)..=previous).rev() {
        emit_alert(
            counters,
            config,
            alerts,
            level,
            QueuePressurePhase::Exit,
            depth,
        );
    }
}

fn emit_alert(
    counters: &QueueCounters,
    config: &WriteQueueConfig,
    alerts: &broadcast::Sender<WriteQueueAlert>,
    level: usize,
    phase: QueuePressurePhase,
    depth: usize,
) {
    let (level, threshold, counter) = if level >= 2 {
        (
            QueuePressureLevel::L2,
            config.l2_threshold,
            &counters.l2_alerts,
        )
    } else {
        (
            QueuePressureLevel::L1,
            config.l1_threshold,
            &counters.l1_alerts,
        )
    };
    if phase == QueuePressurePhase::Enter {
        counter.fetch_add(1, Ordering::Relaxed);
    }
    let _ = alerts.send(WriteQueueAlert {
        level,
        phase,
        depth,
        threshold,
        at_ms: now_ms(),
    });
}

/// 补读页实现（升序；重建信封走 M1-02 严格校验）。
fn read_events_page(
    connection: &Connection,
    session_id: &str,
    after_seq: Option<u64>,
    limit: usize,
) -> Result<Vec<EventEnvelope>, StoreError> {
    let after = match after_seq {
        Some(value) => Some(i64::try_from(value).map_err(|_| StoreError::Internal {
            reason: format!("after_seq 超出 SQLite INTEGER 范围: {value}"),
        })?),
        None => None,
    };
    let limit = i64::try_from(limit).map_err(|_| StoreError::Internal {
        reason: format!("分页上限超出 SQLite INTEGER 范围: {limit}"),
    })?;
    let mut statement = connection.prepare(
        "SELECT id, session_id, run_id, runtime_id, seq, type, payload, ts, v \
         FROM events WHERE session_id = ?1 AND (?2 IS NULL OR seq > ?2) \
         ORDER BY seq ASC LIMIT ?3",
    )?;
    let rows = statement.query_map(params![session_id, after, limit], |row| {
        Ok(StoredEventRow {
            id: row.get(0)?,
            session_id: row.get(1)?,
            run_id: row.get(2)?,
            runtime_id: row.get(3)?,
            seq: row.get(4)?,
            event_type: row.get(5)?,
            payload: row.get(6)?,
            ts: row.get(7)?,
            v: row.get(8)?,
        })
    })?;
    let mut events = Vec::new();
    for row in rows {
        let row = row?;
        events.push(row.into_envelope()?);
    }
    Ok(events)
}

/// `events` 原始行（重建信封用）。
struct StoredEventRow {
    id: String,
    session_id: String,
    run_id: Option<String>,
    runtime_id: String,
    seq: i64,
    event_type: String,
    payload: String,
    ts: i64,
    v: i64,
}

impl StoredEventRow {
    fn into_envelope(self) -> Result<EventEnvelope, StoreError> {
        let payload: Value = serde_json::from_str(&self.payload).map_err(|error| {
            StoreError::InvalidStoredEvent {
                id: self.id.clone(),
                reason: format!("payload 非法 JSON: {error}"),
            }
        })?;
        let mut object = serde_json::Map::new();
        object.insert("v".to_owned(), Value::from(self.v));
        object.insert("id".to_owned(), Value::from(self.id.clone()));
        object.insert("session_id".to_owned(), Value::from(self.session_id));
        object.insert(
            "run_id".to_owned(),
            self.run_id.map_or(Value::Null, Value::from),
        );
        object.insert("runtime_id".to_owned(), Value::from(self.runtime_id));
        object.insert("seq".to_owned(), Value::from(self.seq));
        object.insert("ts".to_owned(), Value::from(self.ts));
        object.insert("type".to_owned(), Value::from(self.event_type));
        object.insert("payload".to_owned(), payload);
        EventEnvelope::from_json_str(&Value::Object(object).to_string()).map_err(|error| {
            StoreError::InvalidStoredEvent {
                id: self.id,
                reason: error.to_string(),
            }
        })
    }
}

fn now_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_design_constants() {
        let config = WriteQueueConfig::default();
        assert_eq!(config.capacity, 4_096, "D3：mpsc(4096)");
        assert_eq!(config.max_batch_entries, 256, "D3：≥256 条提交");
        assert_eq!(config.flush_interval, Duration::from_millis(16), "D3：16ms");
        assert_eq!(config.l1_threshold, 1_024, "D8：L1 >1024");
        assert_eq!(config.l2_threshold, 4_096, "D8：L2 >4096");
        assert_eq!(config.read_connections, 4, "D3：4 个读连接");
        assert_eq!(config.commit_delay, Duration::ZERO);
        config.validate().expect("默认配置必须合法");
    }

    #[test]
    fn invalid_config_is_rejected() {
        let base = WriteQueueConfig::default();
        let cases = [
            WriteQueueConfig {
                capacity: 0,
                ..base.clone()
            },
            WriteQueueConfig {
                max_batch_entries: 0,
                ..base.clone()
            },
            WriteQueueConfig {
                flush_interval: Duration::ZERO,
                ..base.clone()
            },
            WriteQueueConfig {
                read_connections: 0,
                ..base.clone()
            },
            WriteQueueConfig {
                l1_threshold: 8_192,
                l2_threshold: 4_096,
                ..base.clone()
            },
        ];
        for config in cases {
            let error = config.validate().expect_err("非法配置必须被拒绝");
            assert!(
                matches!(error, StoreError::InvalidWriteQueueConfig { .. }),
                "实际: {error:?}"
            );
            assert_eq!(error.code(), "invalid_write_queue_config");
        }
    }

    #[test]
    fn sqlite_error_classification_keeps_extended_code() {
        let conn = Connection::open_in_memory().expect("内存库");
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("建表");
        conn.execute("INSERT INTO t (id, value) VALUES (1, 'a')", [])
            .expect("首次插入");
        let error = conn
            .execute("INSERT INTO t (id, value) VALUES (1, 'b')", [])
            .expect_err("重复主键必须失败");
        let (code, message) = classify_sqlite(error);
        assert_eq!(
            code,
            Some(rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY),
            "扩展错误码必须保留（UNIQUE(seq) 兜底命中同类）"
        );
        assert!(!message.is_empty());
    }

    #[test]
    fn storage_backpressure_error_exposes_contract() {
        let error = StoreError::StorageBackpressure {
            depth: 4_352,
            threshold: 4_096,
        };
        assert_eq!(error.code(), "storage_backpressure");
    }

    #[test]
    fn shutdown_step_order_matches_d2() {
        assert_eq!(
            ShutdownStep::D2_ORDER.map(ShutdownStep::code),
            [
                "drain_write_queue",
                "close_read_connections",
                "wal_checkpoint_truncate",
                "close_write_connection",
                "exit",
            ],
            "D2 五步顺序与稳定码必须一一对应"
        );
        assert_eq!(ShutdownStep::D2_ORDER.len(), 5);
    }

    #[test]
    fn shutdown_config_validation_rejects_zero_timeouts() {
        let error = ShutdownConfig {
            drain_timeout: Duration::ZERO,
            ..ShutdownConfig::default()
        }
        .validate()
        .expect_err("drain_timeout=0 必须拒绝");
        assert_eq!(error.code(), "invalid_shutdown_config");

        let error = ShutdownConfig {
            read_close_timeout: Duration::ZERO,
            ..ShutdownConfig::default()
        }
        .validate()
        .expect_err("read_close_timeout=0 必须拒绝");
        assert_eq!(error.code(), "invalid_shutdown_config");

        ShutdownConfig::default()
            .validate()
            .expect("默认关闭序列配置必须合法");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn messages_latest_reads_tail_in_ascending_order() {
        let dir = tempfile::TempDir::new().expect("临时目录");
        let path = dir.path().join("aether.db");
        drop(Store::open(&path).expect("建库并迁移"));
        {
            let connection = Store::open_writer_connection(&path).expect("写连接");
            connection
                .execute_batch(
                    "INSERT INTO runtimes (id, name, kind, version, created_at, updated_at) \
                     VALUES ('mock', 'Mock', 'mock', '0.1.0', 1, 1);\
                     INSERT INTO sessions (id, runtime_id, title, status, created_at, updated_at) \
                     VALUES ('01J0000000000000000000000A', 'mock', 't', 'idle', 1, 1);",
                )
                .expect("种子运行时与会话");
            for index in 1..=3i64 {
                connection
                    .execute(
                        "INSERT INTO messages (id, session_id, role, content, seq, created_at) \
                         VALUES (?1, '01J0000000000000000000000A', 'user', ?2, ?3, 1)",
                        params![
                            format!("01J000000000000000000000M{index}"),
                            format!("m{index}"),
                            index
                        ],
                    )
                    .expect("插入消息");
            }
        }
        let reads = ReadPool::open(&path, 1).expect("读连接池");
        let session = SessionId::new("01J0000000000000000000000A").expect("会话 id");

        let tail = reads.messages_latest(&session, 2).await.expect("最近一页");
        let seqs: Vec<u64> = tail.iter().map(|message| message.seq).collect();
        assert_eq!(seqs, vec![2, 3], "尾部 limit 条（升序契约）");
        assert_eq!(tail[0].content, "m2");

        let zero = reads.messages_latest(&session, 0).await.expect("limit=0");
        assert!(zero.is_empty());

        let missing = SessionId::new("01J0000000000000000000009Z").expect("会话 id");
        assert!(reads
            .messages_latest(&missing, 2)
            .await
            .expect("无消息会话")
            .is_empty());
    }
}
