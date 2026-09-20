//! 审批队列（M2-03；设计 D9「审批流」：300s 超时 → deny）。
//!
//! 纯状态机（无 I/O、无时钟读取）：`requested_at` 由调用方传入，超时判定由调用方
//! 以注入时钟驱动 [`ApprovalQueue::expire`]；持久化与审计在服务层（aether-control）
//! 完成。同一会话同时最多 1 个待审批（D9）由服务层排队保证，本队列只做台账。

use std::collections::BTreeMap;

/// 审批超时（D9：300s → deny + 审计）。
pub const APPROVAL_TIMEOUT_MS: i64 = 300_000;

/// 待审批票据（`permissions` 行的模型侧投影；ID 为 ULID 文本）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalTicket {
    /// `permissions.id`（ULID）。
    pub id: String,
    /// 权限请求 ID（`permission.requested.request_id`，线协议回环键）。
    pub request_id: String,
    pub session_id: Option<String>,
    /// 线协议 `resource`（`fs.read` / `fs.write` / ...）。
    pub resource: String,
    pub action: String,
    /// 原始 target（未规范化；UI 与审计对照展示）。
    pub target: Option<String>,
    /// canonical 结果（路径类资源）。
    pub canonical_target: Option<String>,
    pub requested_at: i64,
}

impl ApprovalTicket {
    /// 超时截止时间（Unix epoch 毫秒）。
    pub const fn deadline_ms(&self) -> i64 {
        self.requested_at.saturating_add(APPROVAL_TIMEOUT_MS)
    }

    /// 是否已超时（`now_ms >= deadline`）。
    pub const fn is_expired(&self, now_ms: i64) -> bool {
        now_ms >= self.deadline_ms()
    }
}

/// 待审批台账（插入/决议/超时摘除；不持有等待者句柄）。
#[derive(Debug, Default)]
pub struct ApprovalQueue {
    tickets: BTreeMap<String, ApprovalTicket>,
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 插入票据；重复 id 返回 `false` 且不覆盖（同 id 是 ULID 冲突，属调用方 bug）。
    pub fn insert(&mut self, ticket: ApprovalTicket) -> bool {
        if self.tickets.contains_key(&ticket.id) {
            return false;
        }
        self.tickets.insert(ticket.id.clone(), ticket);
        true
    }

    pub fn get(&self, id: &str) -> Option<&ApprovalTicket> {
        self.tickets.get(id)
    }

    /// 取出并移除（决议/超时路径）。
    pub fn remove(&mut self, id: &str) -> Option<ApprovalTicket> {
        self.tickets.remove(id)
    }

    /// 当前待审批快照（按请求时间升序）。
    pub fn pending(&self) -> Vec<ApprovalTicket> {
        let mut tickets: Vec<ApprovalTicket> = self.tickets.values().cloned().collect();
        tickets.sort_by_key(|ticket| (ticket.requested_at, ticket.id.clone()));
        tickets
    }

    pub fn len(&self) -> usize {
        self.tickets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tickets.is_empty()
    }

    /// 摘除全部已超时票据并返回（调用方负责 deny 持久化 + 审计）。
    pub fn expire(&mut self, now_ms: i64) -> Vec<ApprovalTicket> {
        let expired: Vec<String> = self
            .tickets
            .iter()
            .filter(|(_, ticket)| ticket.is_expired(now_ms))
            .map(|(id, _)| id.clone())
            .collect();
        let mut removed = Vec::with_capacity(expired.len());
        for id in expired {
            if let Some(ticket) = self.tickets.remove(&id) {
                removed.push(ticket);
            }
        }
        removed
    }

    /// 指定会话的待审批票据（D9：同一会话同时最多 1 个，服务层据此排队）。
    pub fn pending_for_session(&self, session_id: &str) -> Vec<ApprovalTicket> {
        self.pending()
            .into_iter()
            .filter(|ticket| ticket.session_id.as_deref() == Some(session_id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(id: &str, requested_at: i64) -> ApprovalTicket {
        ApprovalTicket {
            id: id.to_owned(),
            request_id: format!("req-{id}"),
            session_id: Some("01J0000000000000000000000A".to_owned()),
            resource: "fs.write".to_owned(),
            action: "write".to_owned(),
            target: Some("C:/ws/a.txt".to_owned()),
            canonical_target: Some("C:/ws/a.txt".to_owned()),
            requested_at,
        }
    }

    #[test]
    fn deadline_is_300s_per_d9() {
        assert_eq!(APPROVAL_TIMEOUT_MS, 300_000);
        let ticket = ticket("p1", 1_000);
        assert_eq!(ticket.deadline_ms(), 301_000);
        assert!(!ticket.is_expired(300_999));
        assert!(ticket.is_expired(301_000));
    }

    #[test]
    fn expire_removes_only_timed_out_tickets() {
        let mut queue = ApprovalQueue::new();
        assert!(queue.insert(ticket("p1", 1_000)));
        assert!(queue.insert(ticket("p2", 250_000)));
        assert!(!queue.insert(ticket("p1", 2_000)), "重复 id 拒绝");
        assert_eq!(queue.len(), 2);

        let expired = queue.expire(301_000);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, "p1");
        assert_eq!(queue.len(), 1);
        assert!(queue.get("p2").is_some());
    }

    #[test]
    fn remove_is_exclusive() {
        let mut queue = ApprovalQueue::new();
        queue.insert(ticket("p1", 1));
        assert!(queue.remove("p1").is_some());
        assert!(queue.remove("p1").is_none(), "第二次决议无票据");
        assert!(queue.is_empty());
    }

    #[test]
    fn pending_for_session_filters_by_session() {
        let mut queue = ApprovalQueue::new();
        queue.insert(ticket("p1", 1));
        let mut other = ticket("p2", 2);
        other.session_id = Some("01J0000000000000000000000B".to_owned());
        queue.insert(other);
        assert_eq!(
            queue
                .pending_for_session("01J0000000000000000000000A")
                .len(),
            1
        );
        assert_eq!(
            queue
                .pending_for_session("01J0000000000000000000000B")
                .len(),
            1
        );
        assert!(queue.pending_for_session("missing").is_empty());
    }

    #[test]
    fn pending_is_sorted_by_request_time() {
        let mut queue = ApprovalQueue::new();
        queue.insert(ticket("p2", 20));
        queue.insert(ticket("p1", 10));
        let pending = queue.pending();
        assert_eq!(pending[0].id, "p1");
        assert_eq!(pending[1].id, "p2");
    }
}
