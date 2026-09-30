-- 0003_p0_ui_extensions.sql —— ADR-010（P0 UI 能力扩展）增量迁移。
--
-- 背景：0001/0002 已发布（禁止修改，AGENTS.md §8）。本迁移只新增列与表：
--   1) sessions.thinking_depth / runs.thinking_depth：会话级思考深度（0–4）与 run 生效值；
--   2) artifacts：会话文件/目录引用（canonicalize 后路径；D9 权限门不受影响）；
--   3) providers / provider_models：模型与供应商配置（密钥只存 keychain 引用，D10）；
--      播种 4 条内置供应商（is_builtin=1，enabled=0，无密钥/模型；不可删除）。
--
-- 契约（同 0002）：不含 PRAGMA、不含 BEGIN/COMMIT/ROLLBACK（单事务由迁移框架包裹）、
--   不含 schema_migrations 版本写入。

ALTER TABLE sessions ADD COLUMN thinking_depth INTEGER NOT NULL DEFAULT 2;
ALTER TABLE runs ADD COLUMN thinking_depth INTEGER;

CREATE TABLE artifacts (
  id          TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  path        TEXT NOT NULL,          -- canonicalize 后的绝对路径（artifact_add 校验后写入）
  kind        TEXT NOT NULL DEFAULT 'file',  -- file | directory（应用层校验；目录不递归）
  size_bytes  INTEGER,                -- 文件字节数；目录为 NULL
  created_at  INTEGER NOT NULL,
  UNIQUE (session_id, path)
);

CREATE TABLE providers (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  type         TEXT NOT NULL,        -- anthropic | openai | deepseek | google | custom（应用层校验，D12 口径）
  base_url     TEXT,
  api_key_ref  TEXT,                 -- keychain://aether/<service>/<key>，只存引用（D10）
  enabled      INTEGER NOT NULL DEFAULT 1,
  is_builtin   INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);

CREATE TABLE provider_models (
  id           TEXT PRIMARY KEY,
  provider_id  TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  model_id     TEXT NOT NULL,
  display_name TEXT NOT NULL,
  enabled      INTEGER NOT NULL DEFAULT 1,
  created_at   INTEGER NOT NULL,
  UNIQUE (provider_id, model_id)
);

-- 内置供应商播种（固定 ULID；ts = 迁移执行时刻）
INSERT INTO providers (id, name, type, base_url, api_key_ref, enabled, is_builtin, created_at, updated_at) VALUES
  ('01J00000000000000000000B01', 'Anthropic', 'anthropic', 'https://api.anthropic.com',                NULL, 0, 1, CAST(strftime('%s','now') AS INTEGER) * 1000, CAST(strftime('%s','now') AS INTEGER) * 1000),
  ('01J00000000000000000000B02', 'OpenAI',    'openai',    'https://api.openai.com',                    NULL, 0, 1, CAST(strftime('%s','now') AS INTEGER) * 1000, CAST(strftime('%s','now') AS INTEGER) * 1000),
  ('01J00000000000000000000B03', 'DeepSeek',  'deepseek',  'https://api.deepseek.com',                  NULL, 0, 1, CAST(strftime('%s','now') AS INTEGER) * 1000, CAST(strftime('%s','now') AS INTEGER) * 1000),
  ('01J00000000000000000000B04', 'Google Gemini', 'google','https://generativelanguage.googleapis.com', NULL, 0, 1, CAST(strftime('%s','now') AS INTEGER) * 1000, CAST(strftime('%s','now') AS INTEGER) * 1000);
