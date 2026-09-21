//! delta 合并（D4：每 16ms 或累计 8KB 合并为一条持久化）。
//!
//! 语义边界：
//! - 合并只作用于 **`message.delta` 的持久化形态**：多个输入 delta 在窗口内合并为一条
//!   持久化事件（text 为按到达顺序的拼接），窗口满/到期/同消息 `message.completed` 到达时冲刷；
//! - `message.completed` 终稿不参与合并（`content` 以 completed 为准）——冲刷顺序保证
//!   completed 的 seq 在所属 delta 之后；
//! - 未落盘的合并缓冲在降级/sequencer 重启时丢弃并计数（D4：不存在「仅内存广播」路径）。
//!
//! 纯逻辑、无 I/O，全部可单测。

use std::time::Duration;

use aether_core::{EventId, MessageId, RunId, RuntimeId, SessionId};
use tokio::time::Instant;

use crate::normalizer::PendingEvent;
use crate::time::now_ms;

/// delta 合并窗口（D4：16ms）。
pub const DELTA_FLUSH_INTERVAL: Duration = Duration::from_millis(16);
/// L1 写队列压力下的 delta 合并窗口（D8：写队列 >1024 → 告警 + 批次放宽至 64ms）。
pub const DELTA_FLUSH_INTERVAL_L1: Duration = Duration::from_millis(64);
/// delta 合并字节阈值（D4：累计 8KB）。
pub const DELTA_FLUSH_BYTES: usize = 8 * 1024;

/// 单条消息的待合并 delta 缓冲。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaBuffer {
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub runtime_id: RuntimeId,
    pub message_id: MessageId,
    /// 已累积文本（按到达顺序拼接）。
    pub text: String,
    /// 进入本缓冲的输入 delta 条数（诊断/丢弃计数用）。
    pub event_count: usize,
    /// 合并窗口截止时刻。
    pub deadline: Instant,
}

impl DeltaBuffer {
    /// 新建缓冲（`now + interval` 到期）。
    pub fn new(
        session_id: SessionId,
        run_id: Option<RunId>,
        runtime_id: RuntimeId,
        message_id: MessageId,
        now: Instant,
        interval: Duration,
    ) -> Self {
        Self {
            session_id,
            run_id,
            runtime_id,
            message_id,
            text: String::new(),
            event_count: 0,
            deadline: now + interval,
        }
    }

    /// 追加一个输入 delta。
    pub fn push(&mut self, text: &str) {
        self.text.push_str(text);
        self.event_count += 1;
    }

    /// 累计字节数（UTF-8 字节）。
    pub fn bytes(&self) -> usize {
        self.text.len()
    }

    /// 是否达到 8KB 阈值（应立即冲刷）。
    pub fn is_full(&self, threshold: usize) -> bool {
        self.bytes() >= threshold
    }

    /// 合并窗口是否到期。
    pub fn is_due(&self, now: Instant) -> bool {
        self.deadline <= now
    }

    /// 放宽合并窗口（不早于 `now + interval`；M2-07 RSS 限流强制放宽在途缓冲）。
    pub fn relax_deadline(&mut self, now: Instant, interval: Duration) {
        let relaxed = now + interval;
        if relaxed > self.deadline {
            self.deadline = relaxed;
        }
    }

    /// 固化为待持久化事件（`id` 由调用方生成；seq 由 sequencer 分配）。
    pub fn into_pending(self, id: EventId, ts: i64) -> PendingEvent {
        PendingEvent {
            id,
            session_id: self.session_id,
            run_id: self.run_id,
            runtime_id: self.runtime_id,
            ts,
            payload: aether_core::EventPayload::MessageDelta(aether_core::MessageDeltaPayload {
                message_id: self.message_id,
                text: self.text,
            }),
        }
    }

    /// 用当前墙钟构造待持久化事件（管线路径）。
    pub fn into_pending_now(self, id: EventId) -> PendingEvent {
        self.into_pending(id, now_ms())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(now: Instant) -> DeltaBuffer {
        DeltaBuffer::new(
            SessionId::new("01J0000000000000000000000S").expect("会话 id"),
            None,
            RuntimeId::new("mock").expect("runtime id"),
            MessageId::new("01J0000000000000000000000M").expect("消息 id"),
            now,
            DELTA_FLUSH_INTERVAL,
        )
    }

    #[test]
    fn accumulates_in_arrival_order() {
        let now = Instant::now();
        let mut buffer = buffer(now);
        buffer.push("Hel");
        buffer.push("lo ");
        buffer.push("world");
        assert_eq!(buffer.text, "Hello world");
        assert_eq!(buffer.event_count, 3);
        assert_eq!(buffer.bytes(), 11);
    }

    #[test]
    fn full_threshold_and_deadline() {
        let now = Instant::now();
        let mut buffer = buffer(now);
        buffer.push(&"x".repeat(DELTA_FLUSH_BYTES - 1));
        assert!(!buffer.is_full(DELTA_FLUSH_BYTES));
        buffer.push("x");
        assert!(buffer.is_full(DELTA_FLUSH_BYTES), "8KB 阈值必须生效");

        assert!(!buffer.is_due(now));
        assert!(buffer.is_due(now + DELTA_FLUSH_INTERVAL));
        assert!(buffer.is_due(now + DELTA_FLUSH_INTERVAL + Duration::from_millis(1)));
    }

    #[test]
    fn into_pending_keeps_text_and_metadata() {
        let now = Instant::now();
        let mut buffer = buffer(now);
        buffer.push("abc");
        let pending =
            buffer.into_pending(EventId::new("01J0000000000000000000000E").expect("事件"), 7);
        assert_eq!(pending.ts, 7);
        match pending.payload {
            aether_core::EventPayload::MessageDelta(delta) => {
                assert_eq!(delta.text, "abc");
                assert_eq!(delta.message_id.as_str(), "01J0000000000000000000000M");
            }
            other => panic!("类型不符: {other:?}"),
        }
    }
}
