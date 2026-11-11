-- M10-05/ADR-035 v1.1 多模态附件：外置内容寻址元数据 + 模型能力列。
--
-- 依赖 0001–0011，已发布迁移不可修改（AGENTS §2.4）。
-- 口径（ADR-035 D-035-2）：
--   1) attachments 只存元数据与相对路径，不存二进制/大字段；
--   2) UNIQUE(session_id, sha256)：同会话同内容幂等，跨会话共享磁盘文件；
--   3) storage_path 相对 data_dir，绝对路径不入库（路径逃逸防线）；
--   4) provider_models.capabilities 为 JSON 数组（如 ["vision"]），缺省空数组。

CREATE TABLE attachments (
  id           TEXT PRIMARY KEY,
  session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  message_id   TEXT REFERENCES messages(id) ON DELETE SET NULL,
  sha256       TEXT NOT NULL,
  mime         TEXT NOT NULL,
  size_bytes   INTEGER NOT NULL,
  width        INTEGER,
  height       INTEGER,
  storage_path TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  UNIQUE (session_id, sha256)
);

CREATE INDEX idx_attachments_session ON attachments(session_id, created_at);

ALTER TABLE provider_models ADD COLUMN capabilities TEXT NOT NULL DEFAULT '[]';
