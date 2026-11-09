-- M10-02/ADR-032：子 Agent 结果与注入（0010+ 规划位 3 项）。
-- 1) 子会话树查询索引；2) 完整结果表；3) 注入体/状态表 + 索引。
-- 已发布迁移 0001–0010 不修改（AGENTS §2.4）。

CREATE INDEX idx_sessions_parent_status
  ON sessions(parent_session_id, status);

CREATE TABLE subagent_results (
  child_session_id  TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  result            TEXT NOT NULL,              -- canonical JSON（UTF-8，≤64KiB）
  total_bytes       INTEGER NOT NULL,
  truncated         INTEGER NOT NULL DEFAULT 0,
  verdict           TEXT,
  summary           TEXT,                       -- ≤512 字节（UTF-8），不是字符
  summary_truncated INTEGER NOT NULL DEFAULT 0,
  findings_count    INTEGER NOT NULL DEFAULT 0,
  created_at        INTEGER NOT NULL
);

CREATE TABLE subagent_injections (
  id                 TEXT PRIMARY KEY,
  parent_session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  batch_id           TEXT NOT NULL,
  child_session_ids  TEXT NOT NULL,             -- JSON array
  payload            TEXT NOT NULL,             -- compact envelope canonical JSON
  result_refs        TEXT NOT NULL DEFAULT '[]',
  state              TEXT NOT NULL,             -- pending|injected|folded|rejected|superseded
  mode               TEXT NOT NULL DEFAULT 'message_append', -- message_append|origin_fork|system_replace
  supersedes_id      TEXT,                      -- 同父会话上一 injection id（可空）
  requires_approval  INTEGER NOT NULL DEFAULT 0,
  context_version    INTEGER NOT NULL DEFAULT 0,
  created_at         INTEGER NOT NULL,
  injected_at        INTEGER,
  folded_at          INTEGER,
  UNIQUE (parent_session_id, batch_id)
);

CREATE INDEX idx_subagent_injections_ctx
  ON subagent_injections(parent_session_id, state, created_at);
