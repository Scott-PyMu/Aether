//! 事件读取源（D4：补读 `last_seq` 断点续传；sequencer 崩溃恢复读 `max(seq)`）。
//!
//! 生产实现 [`StoreEventSource`] 包装 M1-04 的 4 读连接池（WAL 下与写任务并发）；
//! 测试/故障注入用替身实现（可控缺口与读取失败）。
//!
//! 读取不经过写队列、不受写降级影响：D4 降级期「读查询、诊断导出、备份/导出保持可用」。

use std::future::Future;
use std::pin::Pin;

use aether_core::{EventEnvelope, SessionId};
use aether_store::{ReadPool, StoreError};

use crate::error::SourceError;

/// 读取异步结果（object-safe）。
pub type SourceFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SourceError>> + Send + 'a>>;

/// 事件读取源。
pub trait EventSource: Send + Sync + 'static {
    /// 会话当前最大 `seq`（无事件 → `None`）。
    fn max_seq(&self, session_id: &SessionId) -> SourceFuture<'_, Option<u64>>;

    /// 升序读取 `seq > after_seq` 的事件，至多 `limit` 条。
    fn events_after(
        &self,
        session_id: &SessionId,
        after_seq: Option<u64>,
        limit: usize,
    ) -> SourceFuture<'_, Vec<EventEnvelope>>;
}

/// 生产实现：M1-04 读连接池。
#[derive(Clone)]
pub struct StoreEventSource {
    reads: ReadPool,
}

impl StoreEventSource {
    pub fn new(reads: ReadPool) -> Self {
        Self { reads }
    }

    /// 底层读连接池（诊断用）。
    pub fn reads(&self) -> &ReadPool {
        &self.reads
    }
}

impl EventSource for StoreEventSource {
    fn max_seq(&self, session_id: &SessionId) -> SourceFuture<'_, Option<u64>> {
        let session_id = session_id.clone();
        Box::pin(async move {
            self.reads
                .max_seq(&session_id)
                .await
                .map_err(map_store_error)
        })
    }

    fn events_after(
        &self,
        session_id: &SessionId,
        after_seq: Option<u64>,
        limit: usize,
    ) -> SourceFuture<'_, Vec<EventEnvelope>> {
        let session_id = session_id.clone();
        Box::pin(async move {
            self.reads
                .events_page(&session_id, after_seq, limit)
                .await
                .map_err(map_store_error)
        })
    }
}

fn map_store_error(error: StoreError) -> SourceError {
    SourceError::Unavailable {
        message: error.to_string(),
    }
}
