-- 0002_unique_keys.sql —— 唯一约束增量迁移（ADR-004 决策 6 / ADR-005 决策 2）。
--
-- 背景：0001_init.sql 已发布（禁止修改，AGENTS.md §8），其中 events 仅有普通索引
--       idx_events_session_seq、messages 无 client_msg_id 列与唯一约束。既有库经本
--       0002+ 增量迁移补齐；新库同样按 0001 → 0002 顺序执行，得到一致终态。
--
-- 覆盖：
--   1) events UNIQUE(session_id, seq)：D4/ADR-003 —— 代码层单一 sequencer 的 DB 兜底；
--      自带 session+seq 索引，删除冗余的 idx_events_session_seq；
--   2) messages.client_msg_id + UNIQUE(session_id, client_msg_id)：ADR-005 —— 幂等键
--      持久化去重（NULL 不参与去重：系统/助手消息 client_msg_id 为 NULL）。
--
-- 契约（同 0001，见 crates/aether-store/src/migration.rs）：
--   * 不含 PRAGMA、不含 BEGIN/COMMIT/ROLLBACK（单事务由迁移框架包裹）、
--     不含 schema_migrations 版本写入；
--   * 迁移执行前须先运行 scripts/test/m1-03/audit-duplicate-keys.mjs 输出历史重复键
--     审计报告（ADR-003 §6 / ADR-005 §5）：存在重复时 CREATE UNIQUE INDEX 会失败，
--     迁移整体回滚（单版本单事务），不产生半迁移状态。

ALTER TABLE messages ADD COLUMN client_msg_id TEXT;

DROP INDEX idx_events_session_seq;
CREATE UNIQUE INDEX idx_events_session_seq_uq ON events(session_id, seq);
CREATE UNIQUE INDEX idx_messages_client_msg ON messages(session_id, client_msg_id);
