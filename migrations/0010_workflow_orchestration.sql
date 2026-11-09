-- 0010（M10-01/ADR-031）：编排引擎落库终态 DDL。
-- 约束：0001–0009 不修改；本迁移与设计文档附录 C、verify:m1-03 同批一致。
-- 说明：SQLite 不支持 ALTER TABLE 增加表级 UNIQUE；attempt 幂等由唯一索引承载
--       （与 0002 的 events/messages 唯一约束同一实现口径）。

ALTER TABLE workflows ADD COLUMN deleted_at INTEGER;

ALTER TABLE workflow_runs ADD COLUMN session_id TEXT REFERENCES sessions(id) ON DELETE CASCADE;
ALTER TABLE workflow_runs ADD COLUMN definition_version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE workflow_runs ADD COLUMN definition_hash TEXT;
ALTER TABLE workflow_runs ADD COLUMN error TEXT;
ALTER TABLE workflow_runs ADD COLUMN cancel_requested_at INTEGER;

CREATE UNIQUE INDEX idx_node_runs_attempt
  ON node_runs(workflow_run_id, node_id, attempt);
CREATE INDEX idx_node_runs_run_status
  ON node_runs(workflow_run_id, status);
CREATE INDEX idx_tasks_workflow_node
  ON tasks(workflow_run_id, node_id);
CREATE INDEX idx_tasks_session_status
  ON tasks(session_id, status);
