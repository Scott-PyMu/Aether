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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_core::{EventEnvelope, SessionId};
use rusqlite::{params, Connection};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc, oneshot, Semaphore};
use tokio::task::JoinHandle;

use crate::error::StoreError;
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
/// 读连接数（D3：4 个读连接）。
pub const READ_CONNECTION_COUNT: usize = 4;
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
    /// 关停 drain 标记（FIFO：此前入队的消息先提交完成）。
    Shutdown { reply: oneshot::Sender<()> },
}

impl WriteMessage {
    fn entry_count(&self) -> usize {
        match self {
            Self::Events { events, .. } => events.len(),
            // 命令按 1 条计（准入水位与队列容量口径统一）。
            Self::Command { .. } => 1,
            Self::Shutdown { .. } => 0,
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

/// 写队列句柄（克隆共享同一队列；写任务由 [`StoreRuntime`] 持有）。
#[derive(Clone)]
pub struct WriteQueue {
    sender: mpsc::Sender<WriteMessage>,
    counters: Arc<QueueCounters>,
    alerts: broadcast::Sender<WriteQueueAlert>,
    config: Arc<WriteQueueConfig>,
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
}

/// 读连接池（D3：4 个只读连接；WAL 下读写并发）。
#[derive(Clone)]
pub struct ReadPool {
    connections: Vec<Arc<Mutex<Connection>>>,
    permits: Arc<Semaphore>,
    cursor: Arc<AtomicUsize>,
}

impl ReadPool {
    fn open(path: &Path, size: usize) -> Result<Self, StoreError> {
        let mut connections = Vec::with_capacity(size);
        for _ in 0..size {
            connections.push(Arc::new(Mutex::new(Store::open_read_only(path)?)));
        }
        Ok(Self {
            connections,
            permits: Arc::new(Semaphore::new(size)),
            cursor: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// 读连接数。
    pub fn connection_count(&self) -> usize {
        self.connections.len()
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
        let connection = Arc::clone(&self.connections[index]);
        let result = tokio::task::spawn_blocking(move || {
            let guard = connection.lock().map_err(|_| StoreError::Internal {
                reason: "读连接互斥锁中毒".to_owned(),
            })?;
            operation(&guard)
        })
        .await
        .map_err(|_| StoreError::Internal {
            reason: "读任务异常退出".to_owned(),
        })?;
        drop(permit);
        result
    }

    /// 会话事件计数（诊断/基准）。
    pub async fn event_count(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        let session = session_id.as_str().to_owned();
        self.with_connection(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                [session.as_str()],
                |row| row.get(0),
            )?;
            u64::try_from(count).map_err(|_| StoreError::Internal {
                reason: format!("events 计数为负: {count}"),
            })
        })
        .await
    }

    /// 会话最大 seq（M1-05 sequencer 崩溃恢复：seq = max+1）。
    pub async fn max_seq(&self, session_id: &SessionId) -> Result<Option<u64>, StoreError> {
        let session = session_id.as_str().to_owned();
        self.with_connection(move |conn| {
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
        })
        .await
    }

    /// 补读页（升序，至多 `limit` 条；M1-05 断点续传入口）。
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
        self.with_connection(move |conn| read_events_page(conn, &session, after_seq, limit))
            .await
    }
}

/// 存储运行时：单写任务（[`WriteQueue`]）+ 读连接池（[`ReadPool`]）。
pub struct StoreRuntime {
    path: PathBuf,
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
        };
        let writer = handle.spawn(writer_loop(
            receiver,
            connection,
            Arc::clone(&config),
            Arc::clone(&counters),
            alerts,
        ));
        Ok(Self {
            path,
            queue,
            reads,
            writer: Some(writer),
        })
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

    /// 关停：drain 在途队列（FIFO 关停标记）→ 等待写任务退出（M2-06 关闭序列的存储侧步骤）。
    pub async fn shutdown(mut self) -> Result<(), StoreError> {
        let (reply, reply_rx) = oneshot::channel();
        if self
            .queue
            .sender
            .send(WriteMessage::Shutdown { reply })
            .await
            .is_ok()
        {
            let _ = reply_rx.await;
        }
        if let Some(writer) = self.writer.take() {
            writer.await.map_err(|_| StoreError::Internal {
                reason: "写任务异常退出".to_owned(),
            })?;
        }
        Ok(())
    }
}

/// 写任务主循环：批收集（16ms / ≥256 条）→ 单事务提交 → 回执。
async fn writer_loop(
    mut receiver: mpsc::Receiver<WriteMessage>,
    mut connection: Connection,
    config: Arc<WriteQueueConfig>,
    counters: Arc<QueueCounters>,
    alerts: broadcast::Sender<WriteQueueAlert>,
) {
    // 上一轮批次收集期间到达的命令/关停消息（保持 FIFO，避免重排）。
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
                let _ = reply.send(());
                break;
            }
            WriteMessage::Command {
                command,
                reply,
                submitted_at_ms,
            } => {
                // 命令独立事务立即执行（不参与事件批次，保持到达顺序）。
                execute_command_message(
                    &mut connection,
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
                    command @ WriteMessage::Command { .. } => {
                        // 命令到达：先提交已收集的事件批次（FIFO），命令留待下轮。
                        deferred = Some(command);
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
        let outcome = commit_events_batch(&mut connection, &batch);
        let commit_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        counters.pending_batch_entries.store(0, Ordering::Release);
        counters.last_commit_ms.store(commit_ms, Ordering::Relaxed);
        counters
            .max_commit_ms
            .fetch_max(commit_ms, Ordering::Relaxed);
        let committed_at_ms = now_ms();

        match outcome {
            Ok(()) => {
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
            }
            Err((code, message)) => {
                counters.failed_entries.fetch_add(
                    u64::try_from(batch_entries).unwrap_or(u64::MAX),
                    Ordering::AcqRel,
                );
                counters.resolved_entries.fetch_add(
                    u64::try_from(batch_entries).unwrap_or(u64::MAX),
                    Ordering::AcqRel,
                );
                for job in batch {
                    if let WriteMessage::Events { reply, .. } = job {
                        let _ = reply.send(Err(StoreError::WriteTransactionFailed {
                            code,
                            message: message.clone(),
                        }));
                    }
                }
            }
        }

        review_pressure(&counters, &config, &alerts);

        if let Some(reply) = pending_shutdown.take() {
            let _ = reply.send(());
            break;
        }
    }
    counters.pending_batch_entries.store(0, Ordering::Release);
}

/// 执行一条领域写命令并回执（单写任务内串行；成功 = `COMMIT` 已返回）。
fn execute_command_message(
    connection: &mut Connection,
    counters: &QueueCounters,
    command: StoreCommand,
    reply: oneshot::Sender<Result<StoreOutcome, StoreError>>,
    _submitted_at_ms: i64,
) {
    let started = std::time::Instant::now();
    let outcome = apply_command(connection, &command);
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

/// 单事务写入一批事件（D4 信封 9 字段 ↔ `events` 列一一对应）。
fn commit_events_batch(
    connection: &mut Connection,
    batch: &[WriteMessage],
) -> Result<(), (Option<i32>, String)> {
    let transaction = connection.transaction().map_err(classify_sqlite)?;
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO events (id, session_id, run_id, runtime_id, seq, type, payload, ts, v) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .map_err(classify_sqlite)?;
        for message in batch {
            let WriteMessage::Events { events, .. } = message else {
                continue;
            };
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
}
