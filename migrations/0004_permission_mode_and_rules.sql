-- 0004_permission_mode_and_rules.sql —— M6-01（ADR-014 决策 3 + ADR-019）：
-- 会话级权限模式三档（D9 修订）与用户自定义权限规则表。
--
-- 契约（M1-03 迁移框架，与 0002/0003 一致）：
--   * 不含 PRAGMA（连接打开时执行）；不含 BEGIN/COMMIT/ROLLBACK（由框架包裹单事务）；
--     不写 schema_migrations（由框架负责）；
--   * 已发布迁移 0001–0003 不可修改（AGENTS.md §8）；本文件为唯一增量；
--   * sessions.permission_mode：read_only | workspace_write（缺省）| danger_full_access
--     （ADR-014 附录 B；值与会话级判定入参一一对应）；
--   * permission_rules：列定义与 CHECK 见 ADR-019 附录 A-1（终态 DDL 同批合入设计附录 C）。

ALTER TABLE sessions ADD COLUMN permission_mode TEXT NOT NULL DEFAULT 'workspace_write'
  CHECK (permission_mode IN ('read_only','workspace_write','danger_full_access'));

CREATE TABLE permission_rules (
  id          TEXT PRIMARY KEY,               -- ULID
  name        TEXT NOT NULL,                  -- ≤64 字符（应用层校验）
  resource    TEXT NOT NULL CHECK (resource IN ('fs.read','fs.write','exec','net')),
  action      TEXT NOT NULL CHECK (action IN ('read','write','exec','connect')),
  target_kind TEXT NOT NULL CHECK (target_kind IN ('path','command','host','any')),
  target      TEXT,                           -- any 时为 NULL；path 为 canonicalize 形态
  decision    TEXT NOT NULL CHECK (decision IN ('allow','deny')),
  priority    INTEGER NOT NULL DEFAULT 0 CHECK (priority BETWEEN 0 AND 100),
  enabled     INTEGER NOT NULL DEFAULT 1,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);

CREATE INDEX idx_permission_rules_enabled ON permission_rules(enabled);
