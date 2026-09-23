# ADR-009：`messages_page` 响应契约与补读错误码登记

| 项 | 内容 |
|---|---|
| 状态 | **已批准**（2026-09-23 评审裁定：M3-02 复核 5 项执行口径，见 §7；合入随本 ADR 同步执行） |
| 决策日期 | 2026-09-23 |
| 决策载体 | 已合入：《设计文档》v1.8 → **v1.9**（D7 响应契约行 + 错误码登记）；《实施计划与验收标准》v1.15 → **v1.16**（M3-02 属主承接项措辞修订）；《ADR-006》附录 B 增行（错误码登记表） |
| 关联 | 设计文档 D4（补读上限 10k、先日志后广播）、D7（命令面/校验框架/分页 ≤500）、附录 B/E；实施计划 M3-01 DoD2、M3-02 属主承接项、Gate 3、§8；ADR-003（事件信封/seq）、ADR-005（`client_msg_id` 幂等）、ADR-006（错误码登记规则：附录 B「新增取值须走 ADR」）、ADR-007（`core_not_ready` 增量登记先例） |
| 取代 | 无（增量登记：D7 新增 1 条响应契约、错误码 +1；不改既有命令参数/校验语义） |
| 被取代 | 无 |
| 回退条件 | 见 §6 |
| 未对齐项 | 0 处（实现与契约同批交付：代码修复见 `docs/M3-02-证据.md` §A） |

## 1. 背景

M3-02（会话工作台）交付「`messages_page` 真实后端 + `aetherStore` 注入生产补读源」属主承接项（实施计划 v1.15 登记）。2026-09-23 复核确认 5 项问题：

1. **P0-1 尾部语义实现与登记文本不符**：证据登记「最近一页 `messages` 取 `messages` 表尾部 limit 条」，实现调用 `ReadPool::messages_page(session_id, None, limit)`——该 SQL 为升序 `LIMIT`，实际返回**最早** limit 条（`ops.rs`）；前端生产路径 E2E 替身按尾部实现，测试假绿。
2. **P1-2 错误码未登记**：新增 IPC 错误码 `readback_gap_too_large` 未按 ADR-006 附录 B「新增取值须走 ADR」登记。
3. **P1-3 响应契约未回流**：`messages_page` 响应新增 `messages` 字段、仅最近一页返回——契约未回流设计/ADR；实施计划 v1.15 承接项措辞「按 `last_seq` 分页读 messages 表」与实现（读 events 表补读 + 叠加 messages 基线）及 D4（补读从 events 表读）冲突。
4. **P2-4 载荷边界未登记**：最近一页同时返回 ≤500 事件 + ≤500 消息，无聚合上限。
5. **P2-5 悬挂信号缺失**：`last_seq > max_seq` 时返回空 `events` + `complete=false`，调用方无终止信号。

**边界**：`ReadPool::messages_page`（`after_seq = None` 从会话起点读）为 M2-01 已交付语义，**不得修改**；尾部读取以新增 `ReadPool::messages_latest` 承载。本次不新增命令、不改 DTO、不改事件类型、不改 schema。

## 2. 决策

| # | 决策 | 落地位置 |
|---|---|---|
| 1 | **`messages_page` 响应契约登记**：字段表见附录 A。`messages` 仅最近一页（`last_seq` 缺省）返回，取 `messages` 表**尾部** limit 条（升序；`messages_latest`）；空值为 `Some([])`（字段存在为 `[]`），补读页为 `None`（字段省略）；`complete` 补读分支含 `last_seq >= max_seq` 边界（视为已到最新 → `events=[]`、`complete=true`）；`messages.seq` 与 `events.seq` 为两条独立序列，**仅 `events` 承载补读水位** | 设计文档 D7（响应契约行）；实施计划 M3-02 承接项措辞；代码 `crates/aether-tauri/src/session_backend.rs`、`crates/aether-store/src/write_queue.rs`（`ReadPool::messages_latest`） |
| 2 | **登记 IPC 错误码 `readback_gap_too_large`**：语义 = 补读缺口过大（D4 >10k）拒绝自动补发；触发 = `messages_page` 携带 `last_seq` 且 `max_seq - last_seq > READBACK_MAX_GAP`（10_000）；与核心管线 `PipelineError::ReadbackGapTooLarge` **同码透传**；前端 = `historyTooLarge`「历史消息过多」提示 → 确认后清缓存重载最近 N 条（不重启核心/应用）。登记表见附录 B；ADR-006 附录 B 增行 | ADR-006 附录 B；设计文档 D7；代码 `crates/aether-tauri/src/ipc/error.rs`（枚举已存在，不重新生成 bindings） |
| 3 | **载荷边界登记（P2-4）**：`messages_page` 单条 ≤1MiB、分页 ≤500（events 与 messages 各自，D7 既有上限）；最近一页两数组叠加**不设聚合上限**（本次不做裁剪）。失效条件与后续评估见 §5 | 设计文档 D7（响应契约行注记）；本 ADR 附录 A |

> **错误码计数（决策 2 影响）**：ADR-006 登记 3 个、ADR-007 增量 2 登记 1 个（`core_not_ready`）；本 ADR +1（`readback_gap_too_large`）→ D7 错误码登记全集 16 个（`IpcErrorCode` 枚举取值口径，见 `crates/aether-tauri/src/ipc/error.rs`）。

## 3. 影响

### 3.1 《设计文档》v1.8 → v1.9（合入文本见附录 C）

- 头部：文档版本行与状态行升 v1.9、纳入 ADR-009；修订记录追加 v1.9 行。
- D7：新增 `messages_page` 响应契约行（补读语义/最近一页/空值口径/`complete` 边界/载荷边界注记）；错误码登记补 `readback_gap_too_large`。
- 执行摘要「当前状态」行同步 v1.9 / 实施计划 v1.16。

### 3.2 《实施计划与验收标准》v1.15 → v1.16（合入文本见附录 D）

- 头部/§8：计划版本、状态、基线引用升 v1.16 / 设计文档 v1.9（含 ADR-001–ADR-009）；修订记录追加 v1.16。
- M3-02 属主承接项措辞修订：「按 `last_seq` 分页读 messages 表」→「`last_seq` 补读读 **events 表**（D4）；缺省返回最近一页事件 + `messages` 表最近一页消息基线」。
- 不改 M3-02 DoD1–4 与 Gate 3 条件结构。

### 3.3 实现对齐状态（同批交付）

| 项 | 状态 | 证据 |
|---|---|---|
| 尾部语义修复 | **已实现**：`ReadPool::messages_latest`（降序取 + 反转，升序契约）；`messages_page` 最近一页改调该方法；`ReadPool::messages_page` 语义未改 | `crates/aether-store/src/write_queue.rs`；`crates/aether-tauri/src/session_backend.rs`；`docs/M3-02-证据.md` §A |
| 空值口径 | **已实现**：`MessagesPageResponse.messages: Option<Vec<Message>>`（`skip_serializing_if = "Option::is_none"`）；最近一页 `Some([])` / 补读页省略 | 同上；集成用例 `messages_page_latest_returns_tail_ascending_and_option_shape` |
| `complete` 边界 | **已实现**：`last_seq >= max_seq` → `complete=true`、`events=[]` | 集成用例 `messages_page_pages_backfill_latest_and_guards_gap`（越过最新断点断言） |
| 错误码登记 | **已实现**（枚举/同码透传在 M3-02 代码中已存在，本次仅文档登记；bindings 无枚举变更、不重新生成） | `crates/aether-tauri/src/ipc/error.rs`；`session_backend.rs::readback_gap_code_is_stable`；ADR-006 附录 B 增行 |
| 静态检查 | **已实现**：`verify-m3-02` 追加 `messages_latest`（store）与 `MessagesPageResponse` 六字段 + `Option` 形状断言 | `scripts/test/m3-02/verify-m3-02.mjs` |

### 3.4 不影响的

- 不修改 `ReadPool::messages_page`（M2-01 已交付语义：`after_seq=None` 从会话起点读）；不改 `messages_page` 请求 DTO/参数校验；不新增命令。
- 不改事件类型（附录 B 清单不变）、不改数据库 schema/迁移、不改线协议帧上限。
- 不改 Gate 3 条件结构（属主承接项核验口径不变，仅措辞与证据同步）。

## 4. 版本（合入）

| 文档 | 修订前 | 修订后 |
|---|---|---|
| 《设计文档》 | v1.8 | **v1.9** |
| 《实施计划与验收标准》 | v1.15 | **v1.16** |
| 《需求文档》 | v0.6 | 不变（无需求项变更） |
| 《ADR-006》 | v0.3 | 附录 B 增行（错误码登记；不改决策） |

## 5. 后续（未决项）

1. **载荷聚合上限（决策 3 边界）**：最近一页 ≤500 事件 + ≤500 消息的叠加未设聚合上限；D7 单条/分页上限已约束内存量级。若后续基准（M4-04）显示最近一页重载压力超预算，再评估裁剪或聚合上限（须另立 ADR，不得静默放宽/收紧 D7 分页上限）。
2. **前端悬挂信号（P2-5）**：已由 `complete=true` 边界关闭；前端 `EventBackfillSource` 消费 `complete` 的既有语义无需变更。
3. **`messages.seq` 独立序列的消费面**：工作台消息基线按 `seq` 升序展示；若未来需要跨表统一时间线（事件 ↔ 消息），须另立决策（当前不承诺）。

## 6. 回退条件

1. **尾部语义影响其他调用方** → 停下报告并核查调用面（当前 `messages_latest` 仅 `messages_page` 最近一页调用）；不得回退为修改 `ReadPool::messages_page` 起点语义。
2. **`Option` 空值口径被前端消费者否定**（如需恒存在数组）→ 以 ADR 修订响应契约并同步 bindings/前端类型，不得只改一端。
3. **错误码语义与核心管线码漂移** → 以 `PipelineError::ReadbackGapTooLarge::code()` 为唯一事实源（单测 `readback_gap_code_is_stable` 锁定），先改核心再回流。

## 7. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-09-23 | 评审（M3-02 复核裁定） | 通过 | 裁定 5 项执行口径：① `messages` 最近一页 = 尾部 limit 条（升序，修代码与测试符合登记文本）；② 新增错误码补 ADR 登记（不重新生成 bindings）；③ 响应契约由 ADR 登记 + 计划文本修订（evidence 只作证据、不作契约宿主）；④ P2-4 仅登记边界（D7 已限单条 ≤1MiB、分页 ≤500，本次不做裁剪/聚合上限）；⑤ `last_seq >= max_seq` 视为已到最新（`complete=true`、`events=[]`）。设计文档 v1.9 / 计划 v1.16 / ADR-006 附录 B 增行的合入授权经复核确认（本任务指令）；证据重录见 `docs/M3-02-证据.md` |

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-09-23 | 创建：登记 `messages_page` 响应契约（尾部语义/空值口径/`complete` 边界/载荷边界）与错误码 `readback_gap_too_large`；随批合入设计文档 v1.9、计划 v1.16、ADR-006 附录 B 增行；实现修复与证据同批交付 | （文档维护，评审裁定） |

---

## 附录 A：`messages_page` 响应契约（已登记）

### A.1 字段表

| 字段 | 类型 | 语义 |
|---|---|---|
| `session_id` | string | 会话 id（回显） |
| `last_seq` | number（可选） | 请求断点；缺省/省略 = 最近一页分支 |
| `max_seq` | number \| null | 会话当前最大事件 seq（无事件为 `null`） |
| `events` | array | 补读分支：`seq > last_seq` 升序 ≤limit；最近一页：**尾部** limit 条（升序）。缺口 >10k → `readback_gap_too_large`（不返回） |
| `messages` | array（可选） | **仅最近一页返回**：`messages` 表**尾部** limit 条（升序，`ReadPool::messages_latest`）；无消息为 `[]`；补读页字段省略 |
| `complete` | bool | 补读分支：`末条 seq == max_seq` **或** `last_seq >= max_seq`；`max_seq` 缺失与最近一页恒 `true` |

### A.2 请求/响应矩阵

| 请求 | 行为 |
|---|---|
| `last_seq: Some(n)`，缺口 ≤10k | 读 **events 表** `seq > n`（升序，≤limit）；`complete = 末条 seq == max_seq \|\| n >= max_seq`；`messages` 省略 |
| `last_seq: Some(n)`，缺口 >10k | `readback_gap_too_large`（同码透传；不返回事件） |
| `last_seq` 缺省 | 最近一页：`events` 尾部 limit 条（升序）+ `messages` 尾部 limit 条（升序）；`complete=true` |
| 未接线读连接池 | `core_not_ready` |

### A.3 载荷边界（决策 3）

- 单条 ≤1MiB（D7；线协议帧上限 2MiB，`artifact_ref` <1MiB，D6）；
- 分页 ≤500 条（D7；`events` 与 `messages` 各自）；
- 最近一页两数组叠加**不设聚合上限**（本次不做裁剪）；评估触发条件见 §5-1。

## 附录 B：错误码登记（ADR-006 附录 B 增行）

| code | 语义 | 触发点 | 前端行为 |
|---|---|---|---|
| `readback_gap_too_large` | 补读缺口过大（D4：>10k）拒绝自动补发 | `messages_page`（`last_seq` 缺口 > `READBACK_MAX_GAP`）；与核心管线 `PipelineError::ReadbackGapTooLarge` 同码透传 | 提示「历史消息过多」→ 用户确认后清缓存重载最近 N 条（默认 500，可配置；不重启核心/应用） |

说明：错误码为稳定契约（`snake_case` 序列化），新增取值须走 ADR（ADR-006 附录 B 规则）；本行为 ADR-009（2026-09-23）增行，详 `docs/adr/ADR-009-messages-page-response-and-readback-error-code.md`。

## 附录 C：《设计文档》v1.9 合入文本（已应用）

**C.1 头部（第 7、10 行）**

```diff
-| 文档版本 | **v1.8（冻结）**；冻结后任何修改须走 ADR 并升版（v1.x）；本版含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005、ADR-006、ADR-007、ADR-008 |
+| 文档版本 | **v1.9（冻结）**；冻结后任何修改须走 ADR 并升版（v1.x）；本版含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005、ADR-006、ADR-007、ADR-008、ADR-009 |
-| 状态 | v1.8 已冻结（实现基线，2026-09-22 评审批准，ADR-008 升版；v1.6 = ADR-006、v1.7 = ADR-007、v1.8 = ADR-008；评审记录见各自 ADR §7） |
+| 状态 | v1.9 已冻结（实现基线，2026-09-23 评审裁定，ADR-009 升版；v1.6 = ADR-006、v1.7 = ADR-007、v1.8 = ADR-008、v1.9 = ADR-009；评审记录见各自 ADR §7） |
```

**C.2 修订记录（第 29 行 v1.8 行之后追加）**

```diff
+| v1.9 | ADR-009：`messages_page` 响应契约登记（`last_seq` 补读读 events 表（D4）；缺省返回最近一页事件 + `messages` 表最近一页消息基线；`messages` 仅最近一页返回、空为 `[]`、补读页省略；`complete` 含 `last_seq >= max_seq` 边界；载荷边界登记）；错误码 `readback_gap_too_large` 登记（详 `docs/adr/ADR-009-messages-page-response-and-readback-error-code.md`） |
```

**C.3 D7 命令面（第 446 行 `health` 行之后追加）**

```diff
+  - `messages_page`（ADR-009；响应契约登记）：`{ session_id, last_seq?, limit? }`；按 `last_seq` 读 **events 表**补读（`seq > last_seq` 升序，缺口 >10k → `readback_gap_too_large` 拒绝自动补发，D4）；`last_seq` 缺省返回最近一页——`events` 与 `messages` 均取**尾部** `limit` 条（升序）；`messages` 仅该分支返回（空为 `[]`，补读页字段省略），承载工作台消息基线；`complete` 补读分支含 `last_seq >= max_seq` 边界（视为已到最新）；`messages.seq` 与 `events.seq` 为两条独立序列，仅 `events` 承载补读水位；载荷边界：单条 ≤1MiB、分页 ≤500（events/messages 各自），不设聚合上限；错误码 `readback_gap_too_large`（ADR-009；与核心管线同码透传）；
```

**C.4 执行摘要「当前状态」（第 39 行）**

```diff
-- **当前状态**：**v1.8 已冻结**（2026-09-22 评审批准，ADR-008 升版，含 ADR-001–008）；实施计划见《实施计划与验收标准》v1.14（同批冻结；P0 官方运行时集合冻结为三，Gate 2 纳入三运行时一致性门禁）；……
+- **当前状态**：**v1.9 已冻结**（2026-09-23 评审裁定，ADR-009 升版，含 ADR-001–009）；实施计划见《实施计划与验收标准》v1.16（P0 官方运行时集合冻结为三，Gate 2 纳入三运行时一致性门禁）；……
```

## 附录 D：《实施计划与验收标准》v1.16 合入文本（已应用）

**D.1 头部（第 1、3、7–9 行）与 §8 变更控制（第 625 行）**

```diff
-# Aether 实施计划与验收标准 v1.15
+# Aether 实施计划与验收标准 v1.16
-（基线：《Aether 设计文档》 v1.8（冻结），含 ADR-001–ADR-008）
+（基线：《Aether 设计文档》 v1.9（冻结），含 ADR-001–ADR-009）
-| 计划版本 | v1.15 |
-| 状态 | v1.15 已修订（2026-09-23 M3-01 评审范围修订：……） |
-| 基线 | 设计文档 v1.8（冻结，含 ADR-001–ADR-008）；需求文档 v0.6（P0 三运行时口径同步） |
+| 计划版本 | v1.16 |
+| 状态 | v1.16 已修订（2026-09-23 ADR-009 合入：M3-02 承接项措辞修订 + `messages_page` 响应契约/错误码登记回流；任务进度可更新，结构与门禁变更走 ADR） |
+| 基线 | 设计文档 v1.9（冻结，含 ADR-001–ADR-009）；需求文档 v0.6（P0 三运行时口径同步） |
-- **变更控制**：设计文档已冻结 v1.8（含 ADR-001–ADR-008）；……
+- **变更控制**：设计文档已冻结 v1.9（含 ADR-001–ADR-009）；……
```

**D.2 修订记录（第 34 行 v1.15 行之后追加）**

```diff
+| v1.16 | 同步设计文档 v1.9（ADR-009）：M3-02 属主承接项措辞修订——`last_seq` 补读读 **events 表**（D4）；缺省返回最近一页事件 + `messages` 表最近一页消息基线（响应契约/错误码登记详 ADR-009）；§8 基线与设计文档 v1.9 同步 |
```

**D.3 M3-02 属主承接项（第 412 行）**

```diff
-- **属主承接项（M3-01 生产补读缺口，v1.15 登记）**：实现 `messages_page` 真实后端（按 `last_seq` 分页读 messages 表，M1-08 DTO/命令层已存在）+ `aetherStore` 注入 `EventBackfillSource`（backfill 映射 `messages_page`，`readback_gap_too_large` 同码透传）+ 「缺口 >10k → 确认 → 清缓存重载最近 N 条」生产路径 E2E；证据归档于本任务证据文档，作为 Gate 3 通过条件。
+- **属主承接项（M3-01 生产补读缺口，v1.15 登记；v1.16 按 ADR-009 修订措辞）**：实现 `messages_page` 真实后端（`last_seq` 补读读 **events 表**（D4）；缺省返回最近一页事件 + `messages` 表最近一页消息基线；响应契约见 ADR-009，M1-08 DTO/命令层已存在）+ `aetherStore` 注入 `EventBackfillSource`（backfill 映射 `messages_page`，`readback_gap_too_large` 同码透传）+ 「缺口 >10k → 确认 → 清缓存重载最近 N 条」生产路径 E2E；证据归档于本任务证据文档，作为 Gate 3 通过条件。
```

## 附录 E：实现与证据索引

| 内容 | 路径 |
|---|---|
| 响应契约实现（尾部/空值/`complete` 边界） | `crates/aether-tauri/src/session_backend.rs` |
| 最近一页消息读取（降序取 + 反转） | `crates/aether-store/src/write_queue.rs`（`ReadPool::messages_latest`） |
| 错误码枚举与同码单测 | `crates/aether-tauri/src/ipc/error.rs`；`session_backend.rs::readback_gap_code_is_stable` |
| 集成测试（尾部/空值/边界/缺口守卫） | `crates/aether-tauri/tests/m3_02_session_backend.rs` |
| 静态契约检查 | `scripts/test/m3-02/verify-m3-02.mjs` |
| 任务证据（逐条真实输出） | `docs/M3-02-证据.md` §A/§边界 2 |
| 前端生产路径 E2E（同码透传 + 尾部基线） | `apps/desktop/src/workbenchBackfillE2E.test.tsx`；`backfillSource.ts` |
