//! journal 抽象（D4：事件经 D3 写队列落盘，**写成功后才允许广播**）。
//!
//! - [`JournalWriter`]：管线唯一的落盘入口。生产实现 [`StoreJournal`] 包装
//!   M1-04 的 `WriteQueue`（单写任务 + group commit）；测试/故障注入用替身实现
//!   （写失败、重复 id/seq、临时高水位）验证调用序与降级语义；
//! - [`JournalError`] 分类承载 D4 的分支语义：`DuplicateEventId`（幂等丢弃）、
//!   `DuplicateSeq`（管线 bug 诊断 + 持久化失败路径）、`Backpressure`（D8 临时高水位，
//!   **不得**触发 `persist_degraded`）。

use std::future::Future;
use std::pin::Pin;

use aether_core::EventEnvelope;
use aether_store::{QueuePressureLevel, StoreError, WriteQueue};

use crate::error::JournalError;

/// 落盘回执（journal 抽象层的稳定形状，只暴露管线需要的字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalReceipt {
    /// 本次作业携带的事件条数。
    pub entries: usize,
    /// group commit 批次合计条数。
    pub batch_entries: usize,
    /// 事务耗时（毫秒）。
    pub commit_ms: u64,
}

/// journal 诊断快照（写队列深度进 `health`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JournalMetrics {
    /// 待提交条目数（队列 + 在途批次）。
    pub queue_depth: usize,
    /// 当前背压等级（正常为 `None`；L1/L2 均为临时高水位，不改变存储状态）。
    pub pressure_level: Option<PressureLevel>,
}

/// 存储侧背压等级（D8；仅作诊断展示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    L1,
    L2,
}

/// 落盘异步结果（object-safe，不引入 `async-trait` 依赖）。
pub type JournalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<JournalReceipt, JournalError>> + Send + 'a>>;

/// 事件 journal（D3 写队列在管线视角的抽象）。
pub trait JournalWriter: Send + Sync + 'static {
    /// 提交一批事件并等待落盘（返回成功 = 事务已提交，调用方此时才可广播）。
    fn append(&self, events: Vec<EventEnvelope>) -> JournalFuture<'_>;

    /// 新工作准入（D8 L2）：队列深度 >4096 时返回
    /// [`JournalError::Backpressure`]；默认不设限（测试替身）。
    fn admission(&self) -> Result<(), JournalError> {
        Ok(())
    }

    /// 诊断快照（默认空）。
    fn metrics(&self) -> JournalMetrics {
        JournalMetrics::default()
    }
}

/// 生产实现：M1-04 单写队列。
#[derive(Clone)]
pub struct StoreJournal {
    queue: WriteQueue,
}

impl StoreJournal {
    pub fn new(queue: WriteQueue) -> Self {
        Self { queue }
    }

    /// 底层写队列（关闭序列 / 深度断言用）。
    pub fn queue(&self) -> &WriteQueue {
        &self.queue
    }
}

impl JournalWriter for StoreJournal {
    fn append(&self, events: Vec<EventEnvelope>) -> JournalFuture<'_> {
        Box::pin(async move {
            match self.queue.append_events(events).await {
                Ok(receipt) => Ok(JournalReceipt {
                    entries: receipt.entries,
                    batch_entries: receipt.batch_entries,
                    commit_ms: receipt.commit_ms,
                }),
                Err(error) => Err(classify_store_error(error)),
            }
        })
    }

    fn admission(&self) -> Result<(), JournalError> {
        self.queue.admission().map_err(|error| match error {
            StoreError::StorageBackpressure { depth, threshold } => {
                JournalError::Backpressure { depth, threshold }
            }
            other => JournalError::Other {
                message: other.to_string(),
            },
        })
    }

    fn metrics(&self) -> JournalMetrics {
        let metrics = self.queue.metrics();
        JournalMetrics {
            queue_depth: metrics.depth,
            pressure_level: match metrics.pressure_level {
                Some(QueuePressureLevel::L2) => Some(PressureLevel::L2),
                Some(QueuePressureLevel::L1) => Some(PressureLevel::L1),
                None => None,
            },
        }
    }
}

/// `StoreError` → 管线 journal 错误分类（D4 分支语义；不丢失 SQLite 扩展码）。
fn classify_store_error(error: StoreError) -> JournalError {
    if error.is_duplicate_event_id() {
        return JournalError::DuplicateEventId;
    }
    if error.is_duplicate_seq() {
        return JournalError::DuplicateSeq {
            message: error.to_string(),
        };
    }
    match error {
        StoreError::WriteTransactionFailed { code, message } => {
            JournalError::TransactionFailed { code, message }
        }
        StoreError::StorageBackpressure { depth, threshold } => {
            JournalError::Backpressure { depth, threshold }
        }
        StoreError::WriteQueueClosed => JournalError::Closed,
        other => JournalError::Other {
            message: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_error_classification_preserves_contract() {
        let duplicate_id = classify_store_error(StoreError::WriteTransactionFailed {
            code: Some(rusqlite_constraint_primary_key()),
            message: "duplicate id".to_owned(),
        });
        assert_eq!(duplicate_id, JournalError::DuplicateEventId);

        let duplicate_seq = classify_store_error(StoreError::WriteTransactionFailed {
            code: Some(rusqlite_constraint_unique()),
            message: "duplicate seq".to_owned(),
        });
        assert_eq!(duplicate_seq.code(), "duplicate_seq");

        let disk_full = classify_store_error(StoreError::WriteTransactionFailed {
            code: Some(13),
            message: "database or disk is full".to_owned(),
        });
        assert_eq!(disk_full.code(), "journal_transaction_failed");

        let backpressure = classify_store_error(StoreError::StorageBackpressure {
            depth: 4_097,
            threshold: 4_096,
        });
        assert_eq!(backpressure.code(), "storage_backpressure");

        let closed = classify_store_error(StoreError::WriteQueueClosed);
        assert_eq!(closed, JournalError::Closed);
    }

    // SQLite 扩展错误码（公开 ABI 常量；与 aether-store 的 rusqlite 常量一致）。
    fn rusqlite_constraint_primary_key() -> i32 {
        1_555
    }

    fn rusqlite_constraint_unique() -> i32 {
        2_067
    }
}
