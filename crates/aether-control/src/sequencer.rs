//! 会话内 seq 分配（D4：单一 sequencer，seq 单调唯一；DB `UNIQUE(session_id, seq)` 兜底）。
//!
//! - 分配只发生在 journal 落盘前一刻（delta 合并窗口内的输入事件不占 seq）；
//! - **崩溃恢复**（D4 失败场景表）：sequencer 崩溃重启后从库中 `max(seq)+1` 恢复；
//!   期间提交的事件在管线队列排队，恢复后按到达顺序继续分配。
//!
//! 纯逻辑、无 I/O，全部可单测。

/// 单调递增的会话 sequencer。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSequencer {
    next: u64,
}

impl SessionSequencer {
    /// 空会话：首个 seq 为 1。
    pub const fn new() -> Self {
        Self { next: 1 }
    }

    /// 崩溃恢复：从库中 `max(seq)` 恢复（无事件 → 1）。
    pub const fn resume_after(max_seq: Option<u64>) -> Self {
        match max_seq {
            Some(max) => Self {
                next: max.saturating_add(1),
            },
            None => Self { next: 1 },
        }
    }

    /// 分配下一个 seq（调用方必须保证返回值在 journal 落盘成功前不被复用）。
    pub const fn next_seq(&mut self) -> u64 {
        let seq = self.next;
        self.next = self.next.saturating_add(1);
        seq
    }

    /// 下一个待分配 seq（诊断/断言用）。
    pub const fn peek(&self) -> u64 {
        self.next
    }
}

impl Default for SessionSequencer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_one_and_is_monotonic() {
        let mut sequencer = SessionSequencer::new();
        assert_eq!(sequencer.peek(), 1);
        let seqs: Vec<u64> = (0..5).map(|_| sequencer.next_seq()).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn restart_resumes_at_max_plus_one() {
        assert_eq!(SessionSequencer::resume_after(None).next_seq(), 1);
        assert_eq!(SessionSequencer::resume_after(Some(0)).next_seq(), 1);
        let mut sequencer = SessionSequencer::resume_after(Some(41));
        assert_eq!(sequencer.next_seq(), 42);
        assert_eq!(sequencer.next_seq(), 43);
    }

    #[test]
    fn saturation_does_not_wrap() {
        let mut sequencer = SessionSequencer::resume_after(Some(u64::MAX));
        assert_eq!(sequencer.next_seq(), u64::MAX);
        assert_eq!(sequencer.next_seq(), u64::MAX, "饱和而非回绕");
    }
}
