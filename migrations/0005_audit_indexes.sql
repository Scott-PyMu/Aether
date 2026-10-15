-- 0005_audit_indexes.sql —— M6-03（ADR-023 附录 A-2；v0.3 索引设计修订）：审计查询复合索引。
--
-- 背景（M5-01 DoD5① 规划位；ADR-023 §2.1-1 / §6-3）：
--   * `audit_log` 保留既有 `idx_audit_ts`，新增三个复合索引以支撑 `audit_query`
--     的「会话/actor/action + 时间」过滤与 `ts DESC, id DESC` 稳定排序
--     （1e6 行下已过滤分页查询 P95 ≤300ms）；
--   * 索引末位含 `id`：`ORDER BY ts DESC, id DESC` 在列等值前缀下可由索引逆向扫描
--     全覆盖（v0.2 的 `(col, ts)` 形态不覆盖 `id` 尾键，低选择度过滤触发全量排序，
--     实测 P95 3.2s；修订记录见 ADR-023 §6-3 / v0.3 变更记录与 M6-03 证据）；
--   * 迁移文件卫生遵循 M1-03 口径：无 PRAGMA、无事务控制、不写 schema_migrations；
--   * 0001–0004 不改（AGENTS §8：已发布迁移文件不可修改）。
--
-- 终态 DDL 同批合入《设计文档》附录 C；`EXPECTED_EXPLICIT_INDEXES` 6 → 9
-- （verify:m1-03 强制计数，含附录 C 之外索引的拒绝口径）。

CREATE INDEX idx_audit_session_ts ON audit_log(session_id, ts, id);
CREATE INDEX idx_audit_actor_ts   ON audit_log(actor, ts, id);
CREATE INDEX idx_audit_action_ts  ON audit_log(action, ts, id);
