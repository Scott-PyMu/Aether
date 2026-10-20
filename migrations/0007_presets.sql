-- 0007_presets.sql —— M7-09（ADR-026 §2.2 / 附录 A-1；附录 C 终态同批合入）。
--
-- 背景：原型设置页「Agent 预设」（`deepseek_html_V0.0.1.html:1310-1359`）；
-- 三条内置系统提示词 + 编辑。0001–0006 已发布（禁止修改，AGENTS §8）。
--
-- 契约（M1-03 迁移框架，与 0002–0006 一致）：
--   * 不含 PRAGMA（连接打开时执行）；不含 BEGIN/COMMIT/ROLLBACK（由框架包裹单事务）；
--     不写 schema_migrations（由框架负责）；
--   * presets：名称/系统提示词/内置标记/时间戳（ADR-026 §2.2-1）；
--   * 播种 3 条内置（默认助手 / 代码专家 / 代码审查；`is_builtin=1`，
--     文案以原型 `:684-690` 为准，M7-09 落库定稿并记入证据）；
--   * sessions.preset_id：预设生效来源（`ON DELETE SET NULL`；删除预设后既有会话
--     提示词保持原样，回显「来源已删除」）——ADR-026 §2.2-4。

CREATE TABLE presets (
  id            TEXT PRIMARY KEY,
  name          TEXT NOT NULL,
  system_prompt TEXT NOT NULL,
  is_builtin    INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

-- 内置预设播种（固定 ULID；ts = 迁移执行时刻）。
INSERT INTO presets (id, name, system_prompt, is_builtin, created_at, updated_at) VALUES
  ('01J00000000000000000000P01', '默认助手',
   '你是 Aether 工作台中的通用 AI 助手，帮助用户完成软件工程任务。回答简洁、准确，优先使用工具验证事实。',
   1, CAST(strftime('%s','now') AS INTEGER) * 1000, CAST(strftime('%s','now') AS INTEGER) * 1000),
  ('01J00000000000000000000P02', '代码专家',
   '你是资深软件工程师，专注于代码实现、重构与调试。优先给出可运行的最小改动，并在改动后自检。',
   1, CAST(strftime('%s','now') AS INTEGER) * 1000, CAST(strftime('%s','now') AS INTEGER) * 1000),
  ('01J00000000000000000000P03', '代码审查',
   '你是严格的代码审查者。默认采用对抗式审查：主动寻找边界条件、竞态、错误处理遗漏与安全风险，并给出可操作的修改建议。',
   1, CAST(strftime('%s','now') AS INTEGER) * 1000, CAST(strftime('%s','now') AS INTEGER) * 1000);

ALTER TABLE sessions ADD COLUMN preset_id TEXT REFERENCES presets(id) ON DELETE SET NULL;
