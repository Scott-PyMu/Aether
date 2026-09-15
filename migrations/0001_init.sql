-- 0001_init.sql —— 初始 schema（设计文档 附录 C / D3 / D12，迁移 0001 一次到位）。
--
-- 迁移文件契约（M1-03 迁移框架，见 crates/aether-store/src/migration.rs）：
--   * 纯 schema 初始化：不含 PRAGMA（连接打开时执行，D3）、不含 BEGIN/COMMIT
--     （单事务由迁移框架包裹）、不含 schema_migrations 版本写入（由框架负责）。
--   * 版本机制：schema_migrations(version, checksum, applied_at)；checksum 为本文件
--     字节的 sha256；禁止使用 PRAGMA user_version（D3 评审修订 #2）。
--   * 已发布后只允许新增迁移文件，禁止修改本文件（AGENTS.md §8）。
--   * events.type 为 TEXT + 应用层校验（D12），不加 CHECK 枚举。
--   * runtimes.status CHECK 枚举与 D5 监督状态机一一对应，不做映射。

CREATE TABLE schema_migrations (
  version    INTEGER PRIMARY KEY,
  checksum   TEXT NOT NULL,                     -- 迁移文件 sha256（唯一版本机制，D3；不用 PRAGMA user_version）
  applied_at INTEGER NOT NULL
);

CREATE TABLE runtimes (
  id           TEXT PRIMARY KEY,               -- 'codex' | 'claude-code' | 'deepseek-harness' | 'pi' | 'hermes' | 'mock'
  name         TEXT NOT NULL,
  kind         TEXT NOT NULL,
  version      TEXT NOT NULL,
  protocol     TEXT NOT NULL DEFAULT '1.0',
  capabilities TEXT NOT NULL DEFAULT '[]',     -- JSON 数组
  endpoint     TEXT,
  config       TEXT NOT NULL DEFAULT '{}',
  status       TEXT NOT NULL DEFAULT 'cold'
               CHECK (status IN ('cold','starting','ready','degraded','disabled')),  -- 与 D5 监督状态机一一对应
  status_reason TEXT,                          -- handshake_timeout | start_failed | crash_loop | untrusted | version_mismatch（ADR-002）...
  last_seen_at INTEGER,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);

CREATE TABLE workspaces (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  root_path    TEXT NOT NULL,
  memory_files TEXT NOT NULL DEFAULT '[]',
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);

CREATE TABLE sessions (
  id                TEXT PRIMARY KEY,
  runtime_id        TEXT NOT NULL REFERENCES runtimes(id),
  workspace_id      TEXT REFERENCES workspaces(id),
  parent_session_id TEXT REFERENCES sessions(id) ON DELETE SET NULL,  -- P2 子 Agent 启用
  title             TEXT NOT NULL,
  status            TEXT NOT NULL
                    CHECK (status IN ('creating','idle','running','paused',
                                      'waiting_permission','completed','failed','cancelled')),
  model             TEXT,                       -- UI-05 会话级模型覆盖
  system_prompt     TEXT,
  config            TEXT NOT NULL DEFAULT '{}', -- native_id 等适配器私有映射
  token_usage       TEXT NOT NULL DEFAULT '{}',
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL,
  closed_at         INTEGER
);
CREATE INDEX idx_sessions_parent  ON sessions(parent_session_id);
CREATE INDEX idx_sessions_runtime ON sessions(runtime_id, status);

CREATE TABLE messages (
  id                TEXT PRIMARY KEY,
  session_id        TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  run_id            TEXT,
  role              TEXT NOT NULL CHECK (role IN ('user','assistant','system','tool')),
  content           TEXT NOT NULL DEFAULT '',
  content_parts     TEXT,                       -- 多模态（P2）
  tool_calls        TEXT,                       -- JSON
  parent_message_id TEXT REFERENCES messages(id),
  seq               INTEGER NOT NULL,
  created_at        INTEGER NOT NULL
);
CREATE INDEX idx_messages_session_seq ON messages(session_id, seq);

CREATE TABLE runs (
  id               TEXT PRIMARY KEY,
  session_id       TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  status           TEXT NOT NULL
                   CHECK (status IN ('queued','running','succeeded','failed','cancelled','timeout')),
  input_message_id TEXT,
  error            TEXT,
  started_at       INTEGER NOT NULL,
  finished_at      INTEGER
);

CREATE TABLE events (
  id         TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  run_id     TEXT,
  runtime_id TEXT NOT NULL,                     -- 与 D4 信封一致
  seq        INTEGER NOT NULL,
  type       TEXT NOT NULL,                     -- 应用层校验（D12），无 CHECK 枚举
  payload    TEXT NOT NULL,                     -- JSON
  ts         INTEGER NOT NULL,
  v          INTEGER NOT NULL DEFAULT 1         -- 事件模型版本（与 D4 信封一致）
);
CREATE INDEX idx_events_session_seq ON events(session_id, seq);
CREATE INDEX idx_events_type_ts    ON events(type, ts);

CREATE TABLE permissions (
  id           TEXT PRIMARY KEY,
  session_id   TEXT REFERENCES sessions(id) ON DELETE CASCADE,
  request_id   TEXT,
  resource     TEXT NOT NULL,                   -- fs.read | fs.write | exec | net | ...
  action       TEXT NOT NULL,
  target       TEXT,
  decision     TEXT NOT NULL CHECK (decision IN ('allow','deny','ask')),
  scope        TEXT CHECK (scope IN ('once','session','always')),
  status       TEXT NOT NULL DEFAULT 'pending'
               CHECK (status IN ('pending','resolved','timeout')),
  requested_at INTEGER NOT NULL,
  resolved_at  INTEGER,
  resolver     TEXT                             -- user | rule:<id>（P1）
);

CREATE TABLE audit_log (
  id         TEXT PRIMARY KEY,
  session_id TEXT,
  runtime_id TEXT,
  actor      TEXT NOT NULL,                     -- user | agent | system
  action     TEXT NOT NULL,
  resource   TEXT,
  detail     TEXT,
  result     TEXT,
  ts         INTEGER NOT NULL
);
CREATE INDEX idx_audit_ts ON audit_log(ts);

CREATE TABLE settings (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,                     -- JSON
  updated_at INTEGER NOT NULL
);

-- ===== P2/P4 预留（MVP 建表不写入） =====
-- P2/P4 启用：任务调度（MVP 建表不写入）
CREATE TABLE tasks (
  id              TEXT PRIMARY KEY,
  session_id      TEXT REFERENCES sessions(id) ON DELETE CASCADE,
  workflow_run_id TEXT,
  node_id         TEXT,
  type            TEXT NOT NULL,
  status          TEXT NOT NULL,
  priority        INTEGER NOT NULL DEFAULT 0,
  dependencies    TEXT NOT NULL DEFAULT '[]',
  input           TEXT,
  result          TEXT,
  retry_count     INTEGER NOT NULL DEFAULT 0,
  timeout_ms      INTEGER,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);

-- P2/P4 启用：编排定义（MVP 建表不写入）
CREATE TABLE workflows (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  description TEXT,
  version     INTEGER NOT NULL DEFAULT 1,
  definition  TEXT NOT NULL,
  is_template INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);

-- P2/P4 启用：编排运行（MVP 建表不写入）
CREATE TABLE workflow_runs (
  id          TEXT PRIMARY KEY,
  workflow_id TEXT NOT NULL REFERENCES workflows(id),
  status      TEXT NOT NULL,
  context     TEXT NOT NULL DEFAULT '{}',
  started_at  INTEGER NOT NULL,
  finished_at INTEGER
);

-- P2/P4 启用：编排节点运行（MVP 建表不写入）
CREATE TABLE node_runs (
  id              TEXT PRIMARY KEY,
  workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id) ON DELETE CASCADE,
  node_id         TEXT NOT NULL,
  session_id      TEXT,
  status          TEXT NOT NULL,
  input           TEXT,
  output          TEXT,
  error           TEXT,
  attempt         INTEGER NOT NULL DEFAULT 1,
  started_at      INTEGER,
  finished_at     INTEGER
);

-- P2/P4 启用：记忆（D14；P4 向量检索，MVP 建表不写入）
CREATE TABLE memories (
  id           TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  scope        TEXT NOT NULL CHECK (scope IN ('project','user','session')),
  key          TEXT NOT NULL,
  content      TEXT NOT NULL,
  source       TEXT,
  updated_at   INTEGER NOT NULL,
  UNIQUE (workspace_id, scope, key)
);

-- P2/P4 启用：适配器插件台账（MVP 建表不写入）
CREATE TABLE adapter_plugins (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  version      TEXT NOT NULL,
  path         TEXT NOT NULL,
  manifest     TEXT NOT NULL,
  enabled      INTEGER NOT NULL DEFAULT 1,
  trusted      INTEGER NOT NULL DEFAULT 0,
  installed_at INTEGER NOT NULL
);

-- P2/P4 启用：备份台账（MVP 建表不写入）
CREATE TABLE backups (
  id         TEXT PRIMARY KEY,
  path       TEXT NOT NULL,
  size_bytes INTEGER NOT NULL,
  encrypted  INTEGER NOT NULL DEFAULT 0,
  kind       TEXT NOT NULL DEFAULT 'manual',
  created_at INTEGER NOT NULL
);
