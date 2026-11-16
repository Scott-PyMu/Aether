-- 0014_session_shard.sql（ADR-039 v1.0 D-039-1/D-039-6；主库与分片库均完整执行）
-- sessions.shard_id：会话事件分片映射（新会话写入非空 0..7；既有开发库启动校验）
-- sessions.deletion_pending：跨库删除 tombstone（仅 0/1；D-039-8）
-- 0001-0013 不修改；分片库完整执行同一迁移链（列存在但不参与分片业务）。
ALTER TABLE sessions ADD COLUMN shard_id INTEGER;
ALTER TABLE sessions ADD COLUMN deletion_pending INTEGER NOT NULL DEFAULT 0
  CHECK (deletion_pending IN (0,1));
CREATE INDEX IF NOT EXISTS idx_sessions_shard_id ON sessions(shard_id);
CREATE INDEX IF NOT EXISTS idx_sessions_deletion_pending ON sessions(deletion_pending);
