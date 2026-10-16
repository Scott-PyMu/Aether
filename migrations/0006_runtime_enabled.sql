-- 0006_runtime_enabled.sql —— M7-01（ADR-015 决策 5；附录 C 终态同批合入）：
-- 官方运行时用户启停偏好（唯一事实源）。
--
-- 契约（M1-03 迁移框架，与 0002–0005 一致）：
--   * 不含 PRAGMA（连接打开时执行）；不含 BEGIN/COMMIT/ROLLBACK（由框架包裹单事务）；
--     不写 schema_migrations（由框架负责）；
--   * 已发布迁移 0001–0005 不可修改（AGENTS.md §8）；本文件为唯一增量；
--   * runtimes.enabled：INTEGER NOT NULL DEFAULT 1（1=启用 / 0=用户禁用）；
--     仅经 runtime_enable / runtime_disable 命令与审计维护；EnsureRuntime upsert
--     的 DO UPDATE 不包含该列（ADR-015 决策 5）；
--   * 启动矩阵（ADR-015 决策 5）：enabled=1（或无记录）→ 正常预热；
--     enabled=0 → cold → disabled(user_disabled)，不 spawn。

ALTER TABLE runtimes ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
