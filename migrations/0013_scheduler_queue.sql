-- M10-07/ADR-037 D-037-5：调度器优先级/等待/重试元数据（0010+ 规划位 5 列 + 1 索引）。
--
-- 依赖 0001–0012；已发布迁移不可修改（AGENTS §2.4）。
-- 口径：
--   1) priority = 调用方优先级 −100…100（缺省 0）；
--   2) queued_at = 等待时长起点（NULL → 读取端回退 started_at）；
--   3) retry_count / max_attempts = 自动重试链（旧 attempt 行终态保留）；
--   4) wait_reason = ADR-037 附录 B 枚举文本（concurrency_gate 等）；
--   5) 索引支持 status + priority DESC + queued_at ASC 的队列重建查询。

ALTER TABLE runs ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runs ADD COLUMN queued_at INTEGER;
ALTER TABLE runs ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runs ADD COLUMN max_attempts INTEGER;
ALTER TABLE runs ADD COLUMN wait_reason TEXT;

CREATE INDEX idx_runs_queue ON runs(status, priority DESC, queued_at ASC);
