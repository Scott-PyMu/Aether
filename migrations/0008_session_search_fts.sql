-- 0008_session_search_fts.sql —— 会话检索 FTS5（ADR-025 / M7-07）。
--
-- 范围（ADR-025 §2.1 决策 1/5、附录 A-1）：
--   * 两张 external-content FTS5 表：`sessions_fts`（sessions.title）、
--     `messages_fts`（messages.content）；不复制正文，只建索引；
--   * INSERT/UPDATE/DELETE 同步触发器（external-content 的 'delete' 指令）；
--   * 迁移时对既有数据执行 `rebuild` 回填。
--
-- 契约（同 0001–0007）：不含 PRAGMA、不含事务控制（单事务由迁移框架包裹）、
-- 不写 schema_migrations；触发器内的 BEGIN … END 属 DDL 语法，不是事务控制。

CREATE VIRTUAL TABLE sessions_fts USING fts5(
  title,
  content='sessions',
  content_rowid='rowid'
);

CREATE VIRTUAL TABLE messages_fts USING fts5(
  content,
  content='messages',
  content_rowid='rowid'
);

-- 既有库回填（新库为空，命令为 no-op）。
INSERT INTO sessions_fts(sessions_fts) VALUES('rebuild');
INSERT INTO messages_fts(messages_fts) VALUES('rebuild');

-- sessions.title 同步：INSERT / DELETE / UPDATE OF title。
CREATE TRIGGER trg_sessions_fts_ai AFTER INSERT ON sessions BEGIN
  INSERT INTO sessions_fts(rowid, title) VALUES (new.rowid, new.title);
END;

CREATE TRIGGER trg_sessions_fts_ad AFTER DELETE ON sessions BEGIN
  INSERT INTO sessions_fts(sessions_fts, rowid, title)
    VALUES ('delete', old.rowid, old.title);
END;

CREATE TRIGGER trg_sessions_fts_au AFTER UPDATE OF title ON sessions BEGIN
  INSERT INTO sessions_fts(sessions_fts, rowid, title)
    VALUES ('delete', old.rowid, old.title);
  INSERT INTO sessions_fts(rowid, title) VALUES (new.rowid, new.title);
END;

-- messages.content 同步：INSERT / DELETE / UPDATE OF content。
CREATE TRIGGER trg_messages_fts_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TRIGGER trg_messages_fts_ad AFTER DELETE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, content)
    VALUES ('delete', old.rowid, old.content);
END;

CREATE TRIGGER trg_messages_fts_au AFTER UPDATE OF content ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, content)
    VALUES ('delete', old.rowid, old.content);
  INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
END;
