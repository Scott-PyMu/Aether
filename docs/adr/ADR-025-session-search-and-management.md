# ADR-025：会话检索与管理（搜索 / 改名 / 删除；P1-A11）

| 项 | 内容 |
|---|---|
| 编号 | ADR-025（P1-A11） |
| 版本 | **v0.2（批次评审通过稿）**：v0.1 创建（批次稿）；v0.2 按 `docs/M5-01-ADR批次评审意见.md` 修订闭环（P0/P1/P2 相关条目全部闭环；不改决策） |
| 状态 | **已批准（2026-10-09；人工授权 AI 审批——独立评审通过即批准，见 `docs/M5-01-ADR批次评审意见.md` §7）**；已合入《设计文档》v1.16 |
| 决策日期 | 2026-10-09 |
| 决策载体 | 《设计文档》v1.15 → v1.16（D7 语义、D3/D4 交互、附录 C 迁移规划位、附录 E 补行）；`docs/P1-实施计划与验收标准.md` v0.7（§9 行标注）；UI-UX §7.3（锚点已登记，补语义） |
| 关联 | 设计 D3/D4/D7、附录 B（`session.updated`/`session.closed` 既有类型）；需求 UI-UX §8#11/#12；ADR-005（幂等/恢复）、ADR-009（补读契约）、ADR-014 附录 C-1（命令已登记）、ADR-018（错误码汇总）、ADR-021（`events` 不级联、按保留策略清理，R7）；P1 计划 M7-07（**直接实施项，不构成 P1 门禁**） |
| 取代 / 被取代 | 无（新增） |
| 回退条件 | 见 §6 |
| 实现状态 | 未开始（登记先行；由 M7-07 执行；**非门禁**） |

## 1. 背景

- UI-UX §8#11/#12（P1）：会话搜索（Ctrl+K）、重命名、删除；ADR-014 决策 2 登记为 **P1 直接实施项**（M7-07，不构成门禁）；命令 `session_search`/`session_rename`/`session_delete` 已在 ADR-014 附录 C-1 登记。
- R7（ADR-014 复审）：删除会话级联 `messages`/`runs`/`artifacts`/`permissions`；**`events` 不级联**，按保留策略清理。
- ADR-014 §5-3 未决项：删除语义（软删/级联）与搜索实现（FTS vs 索引 + LIKE）及性能阈值由 P1-A11 定界。
- 现状：`session_list`（过滤 runtime/status）；无搜索/改名/删除命令实现；`sessions_fts` 不存在；`session.updated` 事件类型已定义但零发射（本 ADR 补齐发射点）。

## 2. 决策

### 2.1 核心决策（五段式）

- **决策**：
  1. **搜索实现 = SQLite FTS5**（bundled SQLite 已启用 FTS5；**不引入新依赖**）：两张 **external-content FTS5 表**（`sessions_fts` 覆盖 `sessions.title`；`messages_fts` 覆盖 `messages.content`）+ 同步触发器（INSERT/UPDATE/DELETE 全套）；不复制正文、只建索引（存储可控）。
  2. **搜索语义**：`session_search { query, limit?, offset? }`——查询在标题与消息正文中匹配，按 `bm25()` 排序会话（标题命中加权高于正文）；返回 `SessionSummary[]` + `snippet` + 命中来源（`title|message`）；`query` 非空、≤256 字符（超限 `too_large`；空/纯空白 `invalid_value`）；`limit` ≤100（缺省 20）；`offset` ≥0。**性能目标：≥1e5 会话 / ≥1e6 消息下 P95 ≤300ms**（任务内定值；基准与数据构造记录于 M7-07 证据）。
  3. **改名**：`session_rename { session_id, title }`——ULID 校验 + 标题长度 ≤256/控制字符校验（D7 既有口径）；`sessions.title` 更新走**单写队列**（单写者不变）；FTS 触发器自动同步；**补齐 `session.updated` 发射点**（payload：`sessionId`、`changed: ["title"]`、`title`；附录 B 既有类型允许新增可选字段）；写审计 `session.renamed`。
  4. **删除 = 硬删除**（`session_delete { session_id }`）：
     - 前置：会话存在（否则 `session_not_found`）；无在途 run（`running`/`queued`）且无 `waiting_permission`/pending 票据（否则 **`session_busy`**，要求先中断/处理）；**二次确认与「不可恢复」提示由 UI 承担**（锚点 `session-delete-confirm`）。
     - 执行：单事务删除 `sessions` 行（FK `ON DELETE CASCADE` 连带 `messages`/`runs`/`artifacts`/`permissions`；FTS 触发器同步清理）；**`events` 不级联**（无 FK；保留至 M5-04 保留策略清理）；审计 `session.deleted`（detail：级联行数摘要）；广播 `session.closed`（payload `reason="deleted"`；附录 B 既有类型）。
     - 删除后：`messages_page`/补读 → `session_not_found`；`permissions_pending` 不残留（级联）；在途若并发出现（竞态）→ 以单写者事务串行化 + 前置检查在事务内复核。
  5. **迁移 `0004+`**：两张 FTS5 虚拟表 + 触发器（终态 DDL 随 M7-07 落库；**`verify-m1-03.mjs` 的 SQL 解析器须同批扩展 `CREATE VIRTUAL TABLE`/`CREATE TRIGGER` 支持**，索引计数常量同步评估——触发器不产生显式索引，FTS 内部表不计入 `EXPECTED_EXPLICIT_INDEXES`，须由 M7-07 实测确认并在任务证据记录）。
- **实现要点**：
  1. 触发器模板（三表所需最小集）：
     ```sql
     CREATE TRIGGER trg_messages_fts_ai AFTER INSERT ON messages BEGIN
       INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content); END;
     CREATE TRIGGER trg_messages_fts_ad AFTER DELETE ON messages BEGIN
       INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.rowid, old.content); END;
     CREATE TRIGGER trg_messages_fts_au AFTER UPDATE ON messages BEGIN
       INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.rowid, old.content);
       INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content); END;
     -- sessions.title 同构（sessions_fts）
     ```
  2. `session_search` 实现：`SELECT ... FROM sessions_fts JOIN sessions ... UNION` 汇总标题/正文命中，`bm25` 排序；`snippet()` 生成摘要；只读连接执行（不阻塞写）。
  3. `session.updated` 发射链路按 M7-01 DoD0 同构（先日志后广播；审计保留不变）。
  4. UI：左栏搜索框（`session-search`）+ 结果（`session-search-result`）；重命名入口（`session-rename`）；删除入口 + 确认（`session-delete`/`session-delete-confirm`）。
  5. 回归：M3-12 会话分组与 `session_list` 过滤语义不变；`session_search` 不改 `session_list` 结构。
- **主要风险**：FTS 索引与主表失同步（触发器缺失/迁移半途）；删除级联遗漏（`permissions` 残留）；`session.updated` 发射遗漏；大结果集分页慢；搜索词注入（FTS 查询语法）；改名与 FTS 触发器并发；删除与在途 run 竞态。
- **缓解措施**：触发器随表创建同批迁移（原子）；级联由 FK 保证 + 集成断言；`session.updated` 发射纳入 DoD 与回归；FTS 查询参数化并提供语法转义（用户输入按短语查询处理，不直接拼接 FTS 语法）；分页上限；事务内前置复核。
- **失效条件**：FTS 与主表不一致（抽查失配）→ 重建索引并复审触发器；删除出现级联遗漏/在途数据损坏 → 立即停用删除入口并另立 ADR；性能不达标 → 调整索引/分页（修订本 ADR），不得改断言。

### 2.2 明确不做（P1 边界）

- 不做软删除/回收站（硬删 + 二次确认）；
- 不做跨工作区/跨运行时高级过滤（维持 `session_list` 既有过滤面）；
- 不做搜索历史/建议；不做标签/收藏；
- 不做 `events` 级联删除（R7；按保留策略）。

## 3. 影响

### 3.1 《设计文档》v1.15 → v1.16

- D7：`session_search`/`session_rename`/`session_delete` 语义补全（参数/前置/错误码）；`session.updated`/`session.closed` 发射点启用。
- D4：补注「FTS 同步触发器不改变事件管线；搜索为只读路径」。
- 附录 B：事件类型不变；`session.updated` payload 可选字段 `changed`/`title`、`session.closed` payload `reason` 登记（允许新增可选字段）。
- 附录 C：迁移规划位增 FTS5 虚拟表 + 触发器（M7-07）。
- 附录 E：补 `Session 搜索/删除语义` 行（下划线映射不涉协议）。

### 3.2 `docs/P1-实施计划与验收标准.md` v0.6 → v0.7

- §9 P1-A11 行标注正式编号 **ADR-025（2026-10-09）**；直接实施项性质不变（不构成 P1 门禁）。

### 3.3 不影响的

- 命令面计数不变（命令已登记于 ADR-014）；不新增事件类型；不改线协议；不引入依赖；不改迁移 0001–0003；不改 M3-12 分组与 `session_list` 过滤。

## 4. 版本（拟合入）

| 文档 | 修订前 | 修订后 |
|---|---|---|
| 《设计文档》 | v1.15 | **v1.16** |
| 《P1-实施计划与验收标准》 | v0.6 | **v0.7** |

## 5. 后续（未决项）

1. 搜索范围扩展（工具调用内容/元数据）——P2+。
2. 软删除/回收站（如需）——另立 ADR。
3. 批量管理操作（多选删除/移动）——P2+。

## 6. 回退条件

1. **本 ADR 被否决** → M7-07 缩减为「改名 + 删除（无 FTS）」或整体延后（直接实施项性质，不阻塞 P1 门禁，ADR-014 §6-7）。
2. **FTS 失配/迁移失败** → 移除 FTS 表与触发器（回落 `LIKE` 降级路径由任务内测量决定）并修订本 ADR；不得留下半同步索引。
3. **删除数据事故** → 停用删除入口 + 复盘 + 另立 ADR。
4. **新错误码需求** → 先按 ADR-006 附录 B 登记，不得自造。

## 7. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-10-09 | 文档维护（AI） | v0.1 创建 | 按 M7-07 DoD 与 ADR-014 §5-3/R7；登记先行，代码未改 |
| 2026-10-09 | 独立评审（M5-01 批次，`docs/M5-01-ADR批次评审意见.md`） | 通过（含修订闭环） | 批次复核 17/17 闭环；本 ADR 相关条目见该意见 §6。未另行修改决策 |

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-10-09 | 创建：FTS5 方案与触发器、搜索语义与阈值、改名与 `session.updated` 发射、硬删语义与级联边界、迁移与校验脚本扩展义务 | 文档维护 |
| v0.2 | 2026-10-09 | 批次评审闭环（不改决策）：按 `docs/M5-01-ADR批次评审意见.md` 修订（含该意见 §6 闭环核验所列条目） | 文档维护 |

---

## 附录 A：迁移 / 错误码 / 发射点登记

**A-1 迁移 `0004+` 规划位（FTS；随 M7-07 落库）**

```sql
-- 规划位（非终态；编号取落库时当前最大编号 +1）
-- CREATE VIRTUAL TABLE sessions_fts USING fts5(title, content='sessions', content_rowid='rowid');
-- CREATE VIRTUAL TABLE messages_fts USING fts5(content, content='messages', content_rowid='rowid');
-- CREATE TRIGGER trg_sessions_fts_ai/ad/au ...;   -- 同 ADR-025 §2.1 模板
-- CREATE TRIGGER trg_messages_fts_ai/ad/au ...;
-- 校验脚本义务：verify-m1-03.mjs 解析器扩展 VIRTUAL TABLE/TRIGGER；EXPECTED_EXPLICIT_INDEXES 实测确认（FTS 内部表不计）
```

**A-2 ADR-006 附录 B 错误码增行（逐行文本）**

| code | 语义 | 触发点 | 前端行为 |
|---|---|---|---|
| `session_not_found` | 会话不存在 | `session_search` 引用层/`session_rename`/`session_delete`/`messages_page`（按 id 查无） | 刷新会话列表 + 提示；若来自补读页则回退最近页 |

**A-3 事件发射点（既有类型；不新增类型）**

| 事件 | 发射点 | payload 增量 |
|---|---|---|
| `session.updated` | 改名（`session_rename`） | `changed: ["title"]`、`title`（可选字段） |
| `session.closed` | 删除（`session_delete`） | `reason: "deleted"`（可选字段） |

**A-4 审计动作**：`session.renamed` / `session.deleted`（actor=user；detail 含级联行数摘要，见 ADR-023 附录 A-1）。
