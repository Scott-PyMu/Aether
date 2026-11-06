# ADR-032：M10 子 Agent 派生、隔离、结果回注与权限继承

| 项 | 内容 |
|---|---|
| 编号 | ADR-032（M10-00-02） |
| 版本 | **v1.2（2026-11-06，用户裁定：子 Agent 交付模式/隔离优先）**；v1.1 = M10 方案决策记录 v0.5 合入；v1.0 = M10-00 初始登记 |
| 状态 | **已接受 v1.2（2026-11-06，用户批准 `message_append`/`origin_fork`/`system_replace` 三模式；人工复核建议见 `docs/M10-00-ADR批次评审.md`）** |
| 决策日期 | 2026-11-06（v1.2）；2026-11-05（v1.0/v1.1） |
| 决策载体 | 《设计文档》**v1.33**（D6/D7/D9/D12、附录 B/C）；`docs/P2-架构迁移-实施计划与验收标准.md` **v0.11**；`docs/M10-00-登记表.json`；`docs/M10-子Agent交付模式决策记录.md` v0.1；`docs/M10-02-证据.md` |
| 关联 | 设计 §5 P2、PF-01/PF-02、UI-03、D9/D12；ADR-014（权限模式）、ADR-017（暂停/恢复）、ADR-022（路由）、ADR-029（daemon）、ADR-031（编排/节点会话）；Hermes Studio `delegate_task`/origin fork 行为调研（仅参考，不拷贝） |
| 取代 / 被取代 | 在 v1.1 基础上扩展 `subagent_context` 交付模式；不改变 M9 已冻结的 63 命令语义 |
| 回退条件 | 见 §7 |
| 实现状态 | **未开始（登记先行；M10-02 执行）**；未登记前不得写业务代码 |

## 1. 背景与触发

M10-02 要求父 Agent 派生子 Agent 执行独立审查/探索，父子会话隔离、结果结构化回注、并行探索与对抗式审查模板。设计 D12 已预留 `sessions.parent_session_id`，附录 B 已预留 `subagent.spawned/completed`；但没有冻结派生入口、隔离边界、结果 schema、权限继承、深度/fan-out 上限与结果处置语义。本 ADR 冻结这些契约，作为 M10-02 的启动门。

## 2. 决策

### D-032-1 派生入口（两条，均受同一核心校验）

1. **Agent 自主派生（主路径）**：新增适配器→核心 JSON-RPC 请求方法 **`subagent.spawn`**（30s）、**`subagent.cancel`**（5s）、**`subagent.status`**（10s）、**`subagent.result.get`**（10s，v1.1）。适配器 SDK 提供 `subagent_spawn` 工具；工具调用时适配器向核心发请求。线协议 major 仍为 1.0，帧上限 2MiB，方法表新增但不改变既有方法。
2. **UI/自动化派生**：IPC 命令 `subagent_spawn`（见 D-032-6），供工作台/脚本显式创建子会话；与适配器路径共用同一核心校验与审计。

- 适配器→核心请求与 core→adapter 请求 id 按方向独立；核心待决表 + 超时 + 断连清理；每适配器最多 8 个在途 `subagent.spawn`（与全局并发上限一致）。
- 核心静态能力表为三官方运行时声明 `subagent=true`（与 `multi_provider` 同口径：静态清单为唯一判定源，hello 回显仅诊断）；未声明能力的运行时收到 `subagent.spawn` → `invalid_value` + `business_code=subagent_context_rejected`，不得静默降级。

### D-032-2 派生参数与硬上限

- 必填：`parent_session_id`（ULID）、`role` ∈ `explore|review|custom`、`purpose`（1–2048 字符，清洗控制字符）。
- 可选：`mode` ∈ `explore|review|custom`（缺省 = `role`）、`template` ∈ `parallel_explore|adversarial_review|custom`、`context_refs[]`、`runtime_id?`、`provider_id?`、`model?`、`permission_mode?`、`timeout_ms?`。
- **上限（硬）**：派生深度 ≤ **2**（根会话 depth=0，子=1，孙=2；孙不可再派生）；单父会话活动子 Agent ≤ **4**；全局并发子 Agent ≤ **8**；`context_refs` ≤ **32** 条，合计文本 ≤ **32 KiB**；单子 Agent `timeout_ms` 缺省 30min、上限 2h；单父会话累计派生次数 ≤ **64**（防循环）。
- 超限一律结构化拒绝：`out_of_range`/`invalid_value` + `business_code=subagent_limit_exceeded`（`error.data.limit` 指明 `depth|fanout|concurrency|spawn_count|context_refs`）；不创建会话、不写事件。
- 子 Agent 会话：`sessions.parent_session_id = parent_session_id`；`sessions.config.aether.subagent = { role, mode, template, depth, verdict, resolution, result_ref, spawned_by }`（JSON，不新增列）；标题自动生成 `子 Agent · <role> · <short purpose>`（≤256 字符）。

### D-032-3 隔离边界（默认不继承）

- **上下文**：子会话只接收显式 `context_refs`、`purpose`、模板提示词与父会话的**工作区/权限档位**；**不继承父消息全文、父工具调用状态、父草稿、父事件流**。父若需传递摘要，必须显式以 `context_refs.kind=text` 提供（≤32 KiB），并标注来源。
- **工作区**：子会话工作区 = 父会话工作区；不允许在派生时更换；`workspace_set` 对子会话拒绝（`subagent_context_rejected`）。
- **权限**：缺省继承父 `permission_mode`；允许请求**更严格或相同**档位，严格度顺序 `read_only < workspace_write < danger_full_access`；**禁止升权**（父 `workspace_write` 请求 `danger_full_access` → `subagent_context_rejected`）。子会话工具调用仍 100% 经 D9 网关；`actor=agent`，审计 `detail` 含 `parent_session_id`/`child_session_id`；无直通路径。
- **provider/model**：缺省继承父；显式指定必须存在/启用/兼容，且父会话的 provider 失效时不得为子会话选新 provider（只读）；不满足 → `provider_not_found`/`invalid_value`/`no_candidate_runtime`。
- **私有状态**：子会话不可读父会话的 `permissions` 历史、`audit_log`、未注入的消息；核心 API 不提供「读父会话内部状态」方法。
- **取消树**：父取消/中断 → 子级联取消；子失败不自动取消父（按模板策略标记父运行 `failed`/`continue`）。

### D-032-4 结果结构与回注

子 Agent 完成时，核心生成**结构化结果**（≤64 KiB；超出截断并 `truncated=true`）：

```json
{
  "verdict": "approve|reject|needs_changes|info",
  "summary": "<=8192 chars",
  "findings": [
    { "severity": "blocker|major|minor|info", "title": "<=256", "detail": "<=4096",
      "evidence_refs": [{ "path": "<=1024", "line_start?": 1, "line_end?": 1, "artifact_id?": "ULID" }] }
  ],
  "confidence": 0.0,
  "usage": { "tokens_in": 0, "tokens_out": 0 }
}
```

- `findings` ≤50；`evidence_refs` 每条 finding ≤16；路径只作为文本证据，**不自动读取/执行**；读取仍走 `fs.read` 权限门。
- **完整结果落库（v1.1 / Q6 A）**：完整结果存 **`subagent_results` 表**（0010+），`result` 为 canonical JSON（UTF-8），`total_bytes`/`truncated`/`verdict`/`summary`（≤512B）/`findings_count`/`created_at`；`result_ref = "subagent:<child_session_id>"`。**不放入 `sessions.config`/`SessionSummary`**，避免列表 DTO 膨胀与 2MiB 帧风险。
- **注入体解耦（v1.1 / Q5/Q6/Q9）**：父上下文只接收 compact envelope（≤8KiB/批），含 `verdict`/`summary`/`counts`/`top_findings≤5`/`conflicts`/`children≤16`/`result_refs`；完整 `findings` 不进入注入体。
- **N 条并行 → 1 条**：同 `batch_id` 的 N 个子结果汇总为 **一条** `subagent_injections` 行；`verdict` 优先级 `reject > needs_changes > approve > info`；findings 按 `(title,path,line)` 去重；冲突列表 `conflicts[]`；部分失败保留成功结果并标 `failed_children`。
- **落库 ≠ 进入 LLM 上下文**：完整结果与 injection 行先落库；`state=pending`；父会话到达**安全边界**（空闲/无在途 run 或新 run 启动前）才 `injected`；运行中不插队。
- **父 Agent 按需拉取**：新增适配器方法 `subagent.result.get`（10s）与 IPC `subagent_result_get`；参数必须含 `parent_session_id` + `child_session_id`，校验 `child.parent_session_id == parent_session_id`；UTF-8 字节分页 16KiB/页、最多 4 页（64KiB）；兄弟/堂兄弟/根直读拒绝且不泄露存在性。
- **组装层折叠**：父上下文最多保留最近 **8** 条 envelope 原文 + 1 条折叠摘要（≤2KiB），总量 ≤16KiB；更早行标 `folded_at` 但保留 payload；折叠不删除持久化结果，父 Agent 仍可按需拉取。
- **`subagent_context` 能力与交付模式（v1.2 / Q5）**：`subagent_context` 表示“版本化、可 ack 的结果交付能力”，**不强制 system prompt 替换**。`session.send` 增可选 `subagent_context: {version, block, mode?}`；适配器能力项 `subagent_context`；未声明 → 不得 `auto_inject`，转 hold 或返回 `subagent_context_rejected`。`session.send` ack 增 `applied_context_version` + `applied_context_mode`；core 仅在 `applied_context_version >= version` 且 `applied_context_mode` 与请求一致时置 `injected`；`context_version` 每父会话单调，注入/折叠/追加 bump，重启后重发当前快照。
  - **`message_append`（默认）**：父会话**不修改 system prompt**；compact envelope 作为 tool result（有在途 tool call）或带 `source=subagent` 的合成 user message 追加；必须带 `version`、`supersedes`、`result_refs`；旧注入行标 `superseded`；模型指令明确“最高 version 有效，旧块仅历史证据，不得当作用户指令”；追加成功 ack 后才置 `injected`。父前缀保持不变，缓存 best-effort。
  - **`origin_fork`（后台/长任务）**：在 delegation boundary 冻结父消息 **immutable origin fork**；callback = origin fork + 子结果；使用独立 ephemeral provider context key（如 `<parent_session_id>:background-callback:<child_session_id>`）；不读取 live history、不附加 intervening turns；callback 内 memory/skill review disabled；输出追加到 live timeline；callback 允许 cache miss，隔离优先。origin fork 为进程内状态；重启后无法恢复时观测性拒绝，不回退 live history。
  - **`system_replace`（可选严格模式）**：仅当运行时声明支持时使用；Claude 需 `--system-prompt-snapshot off`，DSH 用 section 热更新；用于“父 system 块只保留最新结果”的严格场景；大 system 块 cache 可 miss，**不得作为默认**，也不得作为 Q5 硬门禁。
  - **能力/降级**：Claude/Codex 默认 `message_append`（后台 `origin_fork`），DSH 默认 `system_replace` 或 `message_append`；运行时不支持请求 mode → `subagent_context_rejected` 或 hold，不得静默降级为另一种 mode。
- **Q5 测试门禁（v1.2）**：Q5 mock/设计（capability/ack/version/mode/fold/restart/失败 + delivery-mode spike）是 M10-02 前置；**真实三运行时 `message_append`/`origin_fork` 完整验证作为 M10-02 验收项执行**（不阻塞 M10-02 开工，也不阻塞 M10-03）。硬门禁：隔离矩阵、结果 exactly once、`message_append` 父前缀不被主动破坏、`origin_fork` 后台 callback 不含 intervening turns、权限/审计、ack/version/supersedes/fold/restart。缓存为**非硬门禁**：允许 `origin_fork`/`system_replace` cache miss。真实测试任一 mode 失败 → 该运行时降级该 mode 为 hold 或 `subagent_context=false`，不得静默累积；`system_replace` 失败不阻塞默认 `message_append`。
- **回注策略**：`template=parallel_explore` 默认 `auto_inject`；`template=adversarial_review` 默认 `hold_for_approval`；`custom` 默认 `hold_for_approval`。`auto_inject` 写 `pending` 注入行，按运行时默认 mode 在安全边界交付；`hold_for_approval` 等 `subagent_resolve`。
- **父处置**：`subagent_resolve` 支持 `accept|reject|rerun`；返回 `{decision,injected,injection_id?,context_version,mode,new_child_session_id?}`；`accept` 置 `accepted` 并按 mode 交付；`reject` 置 `rejected`；`rerun` 置 `superseded` 并创建新批次（受上限）。
- **去重/冲突**：同一 `(parent_session_id,batch_id)` 只一行（`UNIQUE`）；重复 resolve → `subagent_resolution_conflict`；终态不可重复注入；`message_append` 的 `supersedes` 指向旧 injection id/version，旧行标 `superseded`。
- **不污染主上下文**：`message_append` 只追加 compact envelope，不写完整 findings 的普通消息；`origin_fork` callback 只使用 fork + 结果，不读取 live history；`system_replace` 仅严格模式。UI 结果卡显示摘要，详情按需 `subagent_result_get`；`subagent.completed.injected` 为必填 boolean（完成时快照；后台在 callback 追加前可为 false），晚注入不新增事件，UI/外部消费者在 `subagent_list`/`resolve` 后 refetch。

### D-032-5 事件、审计与命令面

**事件（仅使用附录 B 预留 `subagent.spawned/completed` 行；Envelope 9 字段不变）**

| type | payload 必填字段 |
|---|---|
| `subagent.spawned` | `child_session_id`、`parent_session_id`、`role`、`mode`、`depth`、`context_refs_count` |
| `subagent.completed` | `child_session_id`、`parent_session_id`、`status`（`succeeded|failed|cancelled|rejected`）、`verdict?`、`result_ref?`、`batch_id`、`injected`（**必填 boolean**，完成时快照） |

- 事件 `session_id` = 父会话 id（保证父会话内 `seq` 单调）；子会话自身的 `session.created/status_changed/run.*` 事件使用子会话 id，两套序列不混用。
- **不新增 `subagent.injected` 事件（Q11 A）**：晚注入不推送事件；外部消费者收到 `completed` 后调用 `subagent_list`/`subagent_resolve` 读取权威 `injection_state`。

**新增命令（+5；M10-02 累计 +5）**

| 命令 | 参数（严格解析） | 返回 | 超时 | 主要业务码 |
|---|---|---|---|---|
| `subagent_spawn` | `{parent_session_id, role, purpose, mode?, template?, context_refs?, runtime_id?, provider_id?, model?, permission_mode?, timeout_ms?}` | `{child_session_id,status,depth,mode,batch_id}` | 30s | `subagent_limit_exceeded`/`subagent_context_rejected` |
| `subagent_list` | `{parent_session_id, include_completed?}` | `{items: SubagentNode[]}` | 10s | `session_not_found` |
| `subagent_cancel` | `{child_session_id, reason?}` | `{status}` | 15s | `subagent_not_found`/`subagent_resolution_conflict` |
| `subagent_resolve` | `{child_session_id, decision, note?}` | `{decision,injected,injection_id?,context_version,new_child_session_id?}` | 30s | `subagent_result_not_ready`/`subagent_resolution_conflict` |
| `subagent_result_get` | `{parent_session_id, child_session_id, offset?, limit_bytes?}` | `{child_session_id, result_chunk, offset, next_offset?, total_bytes, truncated}` | 10s | `subagent_not_found`/`subagent_result_not_ready` |

- 全部纳入 M1-08 参数校验框架；失败不落库、不产生事件。
- `session_list` 增可选 `include_children`（默认 `false`）；默认排除有 `parent_session_id` 的会话，避免子会话污染主列表；M9 数据无子会话，向后兼容。
- `subagent_result_get` **父会话授权**：必须 `parent_session_id` + `child_session_id`，校验 `child.parent_session_id == parent_session_id`；分页按 UTF-8 字节，`limit_bytes` ≤16384，最多 64KiB；不泄露兄弟/根结果存在性。
- `SubagentNode`：`child_session_id`、`parent_session_id`、`title`、`role`、`mode`、`depth`、`status`、`verdict?`、`result_summary?`（≤512B）、`summary_truncated`、`result_ref?`、`injection_state`（`pending|injected|folded|rejected|superseded`）、`delivery_mode`（`message_append|origin_fork|system_replace`）、`has_more_details`、`created_at`。

**新增业务码（+6）**：`subagent_not_found`、`subagent_limit_exceeded`、`subagent_context_rejected`、`subagent_result_not_ready`、`subagent_resolution_conflict`、**`session_has_children`**（M10-02，`invalid_value`；父会话有子会话时拒绝删除）；**警告码** `subagent_result_truncated`。

**父会话删除（Q7）**：`session_delete` 前置查询 `sessions.parent_session_id`；存在任一子会话（含已完成/失败）→ 拒绝，不级联、不改 `parent_session_id`，不产生 `session.closed`；`invalid_value` + `business_code=session_has_children`；审计 `session.deleted` `result=denied` + `reason=has_children`；UI 删除按钮置灰并提示「请先删除子会话」。

**审计动作（ADR-023 台账增行）**：`subagent.spawn`、`subagent.cancel`、`subagent.result.injected`、`subagent.result.folded`、`subagent.result.pulled`、`subagent.result.accepted`、`subagent.result.rejected`、`subagent.result.rerun`；`actor` 按调用方 `agent|user`；`detail` 含 parent/child、role、depth、verdict，不记结果内容。

### D-032-6 迁移规划位（0010+，Q6 A）

| 表 | 变更 | 用途 |
|---|---|---|
| `sessions` | 索引 `(parent_session_id, status)` | 子会话树查询 |
| `subagent_results` | 新表 | 完整结果（canonical JSON，≤64KiB） |
| `subagent_injections` | 新表 + 索引 `(parent_session_id, state, created_at)` | compact envelope / 注入状态 / 批次 |

```sql
CREATE TABLE subagent_results (
  child_session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  result           TEXT NOT NULL,
  total_bytes      INTEGER NOT NULL,
  truncated        INTEGER NOT NULL DEFAULT 0,
  verdict          TEXT,
  summary          TEXT,                   -- <=512 字节（UTF-8），不是字符
  summary_truncated INTEGER NOT NULL DEFAULT 0,
  findings_count   INTEGER NOT NULL DEFAULT 0,
  created_at       INTEGER NOT NULL
);

CREATE TABLE subagent_injections (
  id                 TEXT PRIMARY KEY,
  parent_session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  batch_id           TEXT NOT NULL,
  child_session_ids  TEXT NOT NULL,
  payload            TEXT NOT NULL,
  result_refs        TEXT NOT NULL DEFAULT '[]',
  state              TEXT NOT NULL,
  mode               TEXT NOT NULL DEFAULT 'message_append', -- message_append|origin_fork|system_replace
  supersedes_id      TEXT,                                  -- 同父会话上一 injection id（可空）
  requires_approval  INTEGER NOT NULL DEFAULT 0,
  context_version    INTEGER NOT NULL DEFAULT 0,
  created_at         INTEGER NOT NULL,
  injected_at        INTEGER,
  folded_at          INTEGER,
  UNIQUE (parent_session_id, batch_id)
);
CREATE INDEX idx_subagent_injections_ctx
  ON subagent_injections(parent_session_id, state, created_at);
```

- **交付模式（v1.2）**：`mode` 记录本行实际交付模式；`supersedes_id` 用于 `message_append` 的旧行替代链；`origin_fork` 的 fork 为进程内状态，不落库（重启不可恢复时观测性拒绝，不回退 live history）；`system_replace` 仅严格模式。
- **计数不变**：仍为 `subagent_injections` 单表 + 索引，0010+ 规划位仍 **22**；新增列不新增表/索引。

- **不加 `(parent_session_id, created_at)` 索引（v0.7 确认）**：`subagent_results` 只有 `child_session_id` 主键，不新增 `parent_session_id` 列/索引；按 child PK 查询，父列表走 `sessions(parent_session_id,status)` 或 `subagent_injections(parent_session_id,state,created_at)`。后续实测慢查询再以新迁移加列+索引，不预加。
- **`summary` 512 字节（UTF-8）**：`subagent_results.summary` 与 `SubagentNode.result_summary` 同一口径；写入按 UTF-8 边界截断，超限置 `summary_truncated=1`；全量 summary 仍在 `result` JSON，详情走 `subagent_result_get`。
- `sessions.parent_session_id` 已存在（0001），不新增列；子 Agent 元数据仍可在 `sessions.config` 放非结果状态，但**完整结果不进 config**。
- 迁移不修改 0001–0009；由 M10-02 落库时与附录 C/`verify:m1-03` 同批。迁移规划位由 20 → **22**（两表）。

### D-032-7 UI-03 子会话树与父会话授权（Q7）

- 树根 = 当前会话；子节点按 `depth` 缩进，显示 role/status/verdict/injection_state；`data-depth`、`data-status`、`data-verdict` 可断言。
- **仅父会话进入**：子会话详情不是独立根路由；路由必须带 `parent_session_id` + `child_session_id`（如 `/sessions/<parent>/subagents/<child>`）；`session_list` 默认排除子会话；`subagent_list` 是唯一树入口；`session_search` 命中子会话返回 `parent_session_id`，点击跳父树并选中子节点；通知/深链/书签必须带 parent，仅 child 的深链拒绝或重定向到父树。
- 父会话提供结果卡（accept/reject/rerun/查看详情）；`pending` 显示「待处理」、`injected` 显示「已回注」、`folded` 显示「已折叠」、`rejected`/`superseded` 终态；详情按需 `subagent_result_get`（16KiB/页），不把全文塞进父上下文。现有节点可增 `data-delivery-mode`（`message_append|origin_fork|system_replace`）诊断属性；**不新增 testid/锚点**。
- 实时更新来自 `subagent.spawned/completed` + 子会话 `session.*` 事件；`completed` 后 refetch `subagent_list` 获取权威 `injection_state`；重连后以 `subagent_list` 重建；不依赖内存状态。
- 父会话删除：有子会话时按钮置灰 + 提示「请先删除子会话」；后端拒绝 `session_has_children`；不级联、不改父指针、不产生 `session.closed`。
- 不实现多 Agent 群聊/@mention（§4#2）；树 + 结果卡是本阶段唯一协作 UI。
- **UI-UX 锚点（Q10）**：`subagent-tree` / `subagent-node` / `subagent-result` / `subagent-result-action` 保持；**不新增 `subagent-result-detail` testid**（testid 18 不变，29 组不变）；详情复用现有结果卡锚点。

## 3. 影响

- 《设计文档》v1.33：D6 增 `subagent.spawn/cancel/status/result.get`、`subagent_context` 能力/交付 mode 与 ack；D7 命令面 +5（累计 87）；D9 增「子 Agent 权限只降不升」与父会话授权/删除拒绝；D12 启用 `sessions.parent_session_id` 语义；附录 B 精确登记 `subagent.*` payload；附录 C 增 `subagent_results`/`subagent_injections`（含 `mode`/`supersedes_id`）。
- P2 计划 v0.11：M10-02 解锁；G9 以隔离矩阵/结果交付/权限审计/10 并发/父授权为验收口径；缓存 best-effort，不作为硬门禁。
- UI-UX v0.19：S-02 增子会话树/结果卡；锚点 `subagent-tree`/`subagent-result`（2 组，testid 18 不变）；补 `pending/injected/folded` 状态与 `session_has_children` 文案；不新增锚点/testid。
- 不新增运行时/依赖/设置键；不扩展 `IpcErrorCode` 32 枚举；业务码 34 → 35（+`session_has_children`）；命令/事件/迁移规划位计数不变（87/11/22）。

## 4. 主要风险与缓解

| 风险 | 缓解 | 失效条件 |
|---|---|---|
| 上下文污染 | 显式 context_refs；不继承消息全文/工具状态；结果摘要化 | 子会话读到父私有状态 → G9 不通过 |
| 权限升权 | 严格度比较；只降不升；D9 网关 100% 覆盖 | 任一子会话工具调用绕过网关 → 停止实现 |
| 派生爆炸 | 深度 2/父 fan-out 4/全局 8/累计 64；超时；取消树 | 出现无限派生或超卖 → 冻结 M10-02 |
| 结果注入污染/重复 | 注入体/完整结果解耦；批次汇总；context_version + mode ack；组装层折叠；supersedes | 重复注入或无法拒绝 → G9 不通过 |
| `message_append` 旧块残留 | version/supersedes + 最高版本指令；fold/compact | 旧块被当用户指令或重复生效 → G9 不通过 |
| 后台 fork 重启丢失 | origin fork 进程内；重启不可恢复时观测性拒绝 | 回退 live history 或引入 intervening turns → G9 不通过 |
| 父/子越权读取 | parent_session_id + child 校验；兄弟/根拒绝；search/深链带 parent | 任一越权读成功 → G9 不通过 |
| 父会话删除丢结果 | 有子会话拒绝删除（`session_has_children`），不级联 | 父删导致子/结果丢失 → 停止实现 |
| 适配器协议扩展风险 | 新方法只增不改；超时/待决表/断连清理；major 1.0 | 旧适配器被破坏 → 回退方法并另立 ADR |

## 5. 备选方案（未采纳）

| 方案 | 原因 |
|---|---|
| 子会话继承父消息全文 | 上下文成本与污染不可控，违反 PF-02 |
| 子 Agent 直接复用父会话 | 无法隔离、无法并行、无法独立取消 |
| 仅 UI 派生、不支持 Agent 自主派生 | 无法实现对抗式审查/并行探索主路径 |
| 新增 `subagent_results` 表 | **已采纳（v1.1/Q6 A）**：完整结果不能放 `sessions.config`，否则 `SessionSummary`/2MiB 帧膨胀 |
| 子 Agent 默认 `danger_full_access` | 违反只降不升与最小权限 |
| 注入体直接放完整 findings | 上下文膨胀/污染；v1.1 改为 compact envelope + 按需拉取 |

## 6. 契约登记汇总

- 命令：5（累计登记 **82 → 87**）。
- 适配器→核心方法：4（`subagent.spawn`/`subagent.cancel`/`subagent.status`/`subagent.result.get`），不计入 IPC 命令面。
- 事件：2（`subagent.spawned/completed`，附录 B 预留行；`injected` 必填；不新增 `subagent.injected`）。
- 设置键：0。
- 迁移：0010+ 规划位 3 项（`sessions(parent_session_id,status)` 索引 + `subagent_results` 表 + `subagent_injections` 表/索引）；总规划位 20 → 22；v1.2 仅给 `subagent_injections` 增 `mode`/`supersedes_id` 列，计数不变。
- UI-UX 锚点：2 组（`subagent-tree`/`subagent-result`，共 4 个 testid；不新增 detail testid）。
- 业务码：6（含 `session_has_children`）；警告码：1。
- 交付模式：`message_append`（默认）/`origin_fork`（后台）/`system_replace`（可选严格）；缓存 best-effort，不作为硬门禁。
- 父会话删除：有子会话拒绝；`session_delete` 语义增量。

## 7. 回退条件

1. 本 ADR 被否决 → M10-02 保持「未解锁」；移除 5 命令/2 事件/4 适配器方法/0010+ 表登记；不写代码。
2. 适配器协议扩展导致旧适配器不兼容 → 回退新方法（仅登记不实现），另立 ADR；不得改 major。
3. 隔离矩阵/权限升权/父授权测试失败 → 冻结 M10-02，修复后重跑 G9；不得以「演示通过」替代。
4. 结果交付出现重复/污染 → 停用对应 mode，改为 `hold_for_approval` 或 `subagent_context=false`；另立 ADR 修订。
5. Q5 三运行时 spike：`message_append`/`origin_fork` 失败 → 该运行时降级该 mode 为 hold 或 `subagent_context=false`；`system_replace` 失败不阻塞默认交付；不得静默累积。
6. 后台 origin fork 无法保证不读取 live history → 禁止 `origin_fork`，退回 foreground/`message_append`；不得以「缓存命中」为由复用 live context。

## 8. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-11-05 | 文档维护（AI） | v1.0 创建 | 按 M10-00 DoD-1/2/3/5/6/8；登记先行，代码未改 |
| 2026-11-05 | 独立评审（AI，M10-00 批次） | 通过 | 检查隔离/权限只降不升/结果 schema/协议增量/计数；闭环见 `docs/M10-00-ADR批次评审.md` |
| 2026-11-05 | 用户决策记录 v0.5 | **v1.1 通过** | Q2 87/+5、Q5 context ack、Q6 A 结果表、Q7 父授权/父删除、Q10 不新增 testid、Q11 不新增事件；见 `docs/M10-方案决策记录与修订建议.md` |
| 2026-11-06 | 用户裁定 | **v1.2 通过** | 子 Agent 隔离优先、缓存 best-effort；`message_append`/`origin_fork`/`system_replace` 三模式；Codex 不再降级；见 `docs/M10-子Agent交付模式决策记录.md` |

## 9. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v1.0 | 2026-11-05 | 创建：派生入口/上限/隔离/结果/权限/事件/4 命令/协议方法/0010+ 索引 | 文档维护 |
| v1.1 | 2026-11-05 | 用户决策记录 v0.5：+`subagent_result_get`（5 命令）；`subagent_results`+`subagent_injections`（22 迁移）；compact envelope/N→1/折叠/按需拉取；`subagent_context` 能力/ack；父授权/父删除 `session_has_children`；`injected` 必填、不新增事件 | 文档维护 |
| v1.1 修订 | 2026-11-05 | 决策记录 v0.7：`subagent_results` 不加索引；`summary` 512 **字节** UTF-8 + `summary_truncated`；Q5 真实测试为 M10-02 前置门禁、不阻塞 M10-03 | 文档维护 |
| v1.2 | 2026-11-06 | 用户裁定：子 Agent 隔离优先、缓存 best-effort；`subagent_context` 增 `mode=message_append|origin_fork|system_replace` 与 ack `applied_context_mode`；后台 immutable origin fork + 新 provider context；`subagent_injections` 增 `mode`/`supersedes_id`（计数不变）；Q5 硬门禁改为隔离/exactly-once/不污染，缓存非硬门禁；真实三运行时 `message_append`/`origin_fork` 完整验证在 M10-02 验收执行（不阻塞 M10-02 开工/M10-03）；Codex 不再 `false`/hold | 文档维护 |

---

## 附录 A：隔离矩阵（实现断言基准）

| 维度 | 父→子 | 断言 |
|---|---|---|
| 消息全文 | 不继承 | 子会话 `messages_page` 不含父消息 |
| 工具状态 | 不继承 | 子会话无父工具调用记录 |
| 工作区 | 相同，不可改 | `workspace_set` 对子会话拒绝 |
| 权限档位 | 相同或更严格 | 升权 → `subagent_context_rejected` |
| provider/model | 继承或显式有效值 | 父 provider 失效时子不可选新 provider |
| 父私有状态 | 不可读 | 无 API 路径；审计/权限历史不暴露 |
| 取消 | 父→子级联 | 父取消后子 ≤5s 收口 |
| 结果 | 摘要 envelope，全文存 `subagent_results` | 父上下文仅 compact envelope；`message_append`/`origin_fork`；详情按需 `subagent_result_get` |
| 结果交付 | 默认 append-only；后台 origin fork | `message_append` 不改 system prompt；`origin_fork` 不含 intervening turns；`system_replace` 仅严格模式 |
| 父会话删除 | 有子会话拒绝 | `session_has_children`；不级联、不改父指针 |

## 附录 B：结果处置状态机

```
spawning → running → completed(result_ready)
                         ├→ injected (auto_inject: message_append/origin_fork/system_replace)
                         ├→ accepted (resolve accept)
                         ├→ rejected (resolve reject)
                         └→ superseded (resolve rerun → new child; message_append 旧行)
running → cancelled
running → failed (no result; parent policy fail/continue)
```

- `message_append`：追加 compact envelope（tool result/合成 user message）→ ack version/mode → `injected`；旧行 `superseded`。
- `origin_fork`：后台 callback 使用 origin fork + 结果 → 追加 live timeline → `injected`；callback 前 `injected=false`（完成时快照）。
- `system_replace`：仅运行时声明支持；替换 system 块 → ack version/mode → `injected`；缓存 best-effort。

## 附录 C：模板

| 模板 | role | 默认回注 | 用途 |
|---|---|---|---|
| `parallel_explore` | `explore` | `auto_inject` | N 个子 Agent 并行探索不同方案，汇总去重 |
| `adversarial_review` | `review` | `hold_for_approval` | 独立审查/对抗式审查，父确认后回注 |
| `custom` | `custom` | `hold_for_approval` | UI 自定义 |
