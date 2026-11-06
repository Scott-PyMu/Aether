# ADR-031：M10 编排 DSL、状态机、节点类型与失败/重试语义

| 项 | 内容 |
|---|---|
| 编号 | ADR-031（M10-00-01） |
| 版本 | **v1.0（2026-11-05）** |
| 状态 | **已接受（2026-11-05，M10-00 ADR 批次；用户授权 AI 审批；人工复核建议见 `docs/M10-00-ADR批次评审.md`）** |
| 决策日期 | 2026-11-05 |
| 决策载体 | 《设计文档》**v1.31**（D6/D7/D8/D12、附录 B/C）；`docs/P2-架构迁移-实施计划与验收标准.md` **v0.9**；`docs/M10-00-登记表.json` |
| 关联 | 设计 §5 P2、CP-06/PF-02、D12/附录 C；ADR-003/004（事件/存储）、ADR-014（权限模式）、ADR-018（命令面先例）、ADR-022（并发闸门）、ADR-029（daemon/命令面） |
| 取代 / 被取代 | 无（新增）；不改变 M9 已冻结的 63 命令语义 |
| 回退条件 | 见 §7 |
| 实现状态 | **未开始（登记先行；M10-01 执行）**；未登记前不得写业务代码 |

## 1. 背景与触发

M10-01 要求实现串行、并行、条件、汇总的工作流引擎，**无画布**（UI-04 画布仍属 P4），并覆盖定义校验、执行、持久化、重试/超时/取消与运行视图。设计 D12/附录 C 已预留 `workflows`、`workflow_runs`、`node_runs`、`tasks` 四张表；M10-00 必须先冻结 DSL、状态机、节点类型、失败语义、命令/事件/迁移登记，否则各任务会各自发明不一致的契约。

本 ADR 是 M10-01 的启动门；未通过本 ADR 前，M10-01 标注「未解锁」。

## 2. 决策

### D-031-1 定义格式：版本化 JSON，白名单模式，无任意代码

- 工作流定义是**版本化 JSON 对象**，`schema_version` 固定为 `1`；不允许脚本、表达式语言运行时代码、模板内 `eval`、任意 shell/HTTP 节点。
- 顶层字段：`schema_version`、`name`、`description?`、`nodes[]`、`edges[]`、`settings?`、`metadata?`。
- 定义序列化后 ≤ **256 KiB**；`nodes` ≤ **64**；DAG 深度 ≤ **32**；单节点 fan-out ≤ **16**；单次运行最大节点执行次数（含重试/展开）≤ **256**；`name` 1–128 字符；`description` ≤1024 字符；节点 `prompt` ≤32 KiB。
- 非法定义**结构化拒绝且不落库**：`invalid_value` + `business_code=workflow_invalid_definition`，`error.data.details[]` 至少含 `path`（JSON Pointer）、`reason`、`code`；一次返回全部可发现错误（上限 50 条，超出截断并标记 `truncated=true`）。
- 定义保存前必须通过完整校验：DAG 无环、节点 id 唯一且匹配 `^[a-zA-Z][a-zA-Z0-9_-]{0,63}$`、边端点存在、每个非入口节点至少一条入边、`join` 节点有 ≥1 条入边、`parallel` 节点有 ≥1 条出边、引用可达、上限未超。
- **定义不可变快照**：`workflow_run` 启动时把定义、`definition_version`、`definition_hash`（sha256 canonical JSON）写入运行记录（`workflow_runs.definition_version/definition_hash`，0010+ 规划位），运行期不受后续 `workflow_update` 影响。

### D-031-2 节点类型（5 类，固定枚举）

| 类型 | 作用 | 关键字段 | 约束 |
|---|---|---|---|
| `agent` | 启动一个子会话执行提示词 | `prompt`、`context_refs?`、`runtime_id?`、`provider_id?`、`model?`、`permission_mode?`、`timeout_ms?` | 子会话 `parent_session_id=anchor session`；权限只降不升（ADR-032） |
| `task` | 确定性内置操作 | `handler` ∈ `noop` / `set_context` / `merge_results` / `emit_summary`；`input?` | 不执行 shell/网络/任意代码；输入输出 ≤64 KiB |
| `condition` | 布尔判定 | `expression`（见下） | 仅纯函数表达式；只读 `context`/上游结果 |
| `parallel` | 运行时 fan-out | `fanout`（整数或引用数组）、`max_parallel?` | `fanout` ≤16；展开后的节点仍受总执行次数上限 |
| `join` | 汇合 | `mode` ∈ `all`/`any`/`quorum`、`quorum?`、`on_failure?` | `quorum` 仅 `mode=quorum`；`1…入边数` |

- **表达式 DSL（`condition`）**：`{ "op": "eq|ne|gt|gte|lt|lte|contains|exists|and|or|not", "args": [...] }`；叶子为 `{"ref": "context.foo"}` 或 JSON 标量；不支持函数调用、正则、文件/网络访问；求值深度 ≤16，节点数 ≤256。
- **边**：`{ "from": "<node_id>", "to": "<node_id>", "when?": <expression>, "label?" }`；`when` 为假时目标节点标 `skipped`（若所有入边均跳过）。
- **模板**：`is_template=true` 的工作流可被 `workflow_run` 直接运行；模板不参与「同 name 唯一」限制。

### D-031-3 状态机（固定枚举；DB 无 CHECK，应用层校验）

**工作流运行 `workflow_runs.status`**

```
queued → running → succeeded
                 ├→ failed
                 ├→ cancelled
                 └→ interrupted（daemon 重启/维护）
```

**节点运行 `node_runs.status`**

```
pending → queued → running → succeeded
                          ├→ failed
                          ├→ timeout
                          ├→ cancelled
                          └→ skipped（条件不满足）
```

- 终态：`succeeded|failed|cancelled|interrupted`（运行）；`succeeded|failed|timeout|cancelled|skipped`（节点）。
- 终态不可变；重试必须新建 `attempt` 记录（`node_runs` 唯一键 `(workflow_run_id,node_id,attempt)`，0010+ 规划位），不得改写旧 attempt 的终态。
- `interrupted` 仅由 daemon 重启/维护触发；**不自动恢复执行**（避免重复副作用），UI 显示可重试；运行视图 `health` 可查。

### D-031-4 执行语义

- **串行/并行**：按 DAG 依赖调度；`parallel` 节点在运行时展开为子节点（内部 id `parent#i`），受 `max_parallel` 与全局并发闸门双重约束；`join` 按 `mode` 汇合。
- **条件**：边级 `when` 与 `condition` 节点均适用；条件为假 → 下游 `skipped`；不产生「半执行」。
- **汇总**：`join` 输出 `{ results: [{node_id, status, output}], joined: <按 mode 的确定性结果> }`；`merge_results` 的冲突策略固定为「后写覆盖 + 冲突列表」，冲突列表进入 `output.conflicts`，不静默丢弃。
- **失败策略**：`settings.fail_policy` ∈ `fail_fast`（默认）/`continue`；`fail_fast` 下首个不可重试失败 → 取消兄弟分支并置运行 `failed`；`continue` 下失败节点记 `failed`，可运行分支继续，最终运行 `failed` 仍保留汇总。
- **重试**：节点级 `retry { max_attempts 1…5, backoff_ms 100…60000, multiplier 1…4 }`；默认 `max_attempts=1`（不自动重试）；仅可重试错误自动重试；退避含 ±20% jitter；重试不重置已成功兄弟节点。
- **超时**：节点 `timeout_ms` 1s–24h（默认 30min）；运行级 `settings.timeout_ms` ≤7 天；超时先发中断，节点标 `timeout`，按 `fail_policy` 处理。
- **取消**：`workflow_run_cancel` 级联取消所有 `pending/queued/running` 节点；ack ≤1s，运行在 ≤5s 内收口；5s 未收口 → 节点标 `cancelled` + `cleanup_pending=true`，运行标 `cancelled`，写审计并返回 `workflow_cancel_timeout`（业务码）；**不重启 daemon**。
- **幂等**：同一 `(workflow_run_id,node_id,attempt)` 不重复执行；`workflow_node_retry` 递增 attempt；daemon 重启后 `running` 节点一律 reconcile 为 `interrupted`，不自动重放。
- **上下文隔离**：每个节点只接收 `{workflow_input, upstream_results（显式边）, allowlisted_context, node_config}`；**不注入 anchor session 的消息全文/工具状态**；单节点输出 ≤64 KiB，超出截断并 `truncated=true`（不静默）。
- **先写后广播**：`workflow_runs`/`node_runs` 状态先落库，再发 `workflow.*` 事件（D4）；事件失败不阻塞状态推进，但记诊断。

### D-031-5 事件与命令面登记

**事件（仅使用附录 B 预留 `workflow.*` 行；Envelope 9 字段不变）**

| type | payload 必填字段 |
|---|---|
| `workflow.started` | `workflow_run_id`、`workflow_id`、`session_id`、`priority`、`node_count` |
| `workflow.node_started` | `workflow_run_id`、`workflow_id`、`node_id`、`node_type`、`attempt`、`node_session_id?` |
| `workflow.node_completed` | `workflow_run_id`、`workflow_id`、`node_id`、`attempt`、`status`、`duration_ms`、`result_ref?` |
| `workflow.node_failed` | `workflow_run_id`、`workflow_id`、`node_id`、`attempt`、`status`、`error`、`retryable` |
| `workflow.node_cancelled` | `workflow_run_id`、`workflow_id`、`node_id`、`attempt`、`reason` |
| `workflow.node_skipped` | `workflow_run_id`、`workflow_id`、`node_id`、`reason` |
| `workflow.completed` | `workflow_run_id`、`workflow_id`、`status`、`node_count`、`duration_ms` |
| `workflow.failed` | `workflow_run_id`、`workflow_id`、`status`、`failed_node_id?`、`error`、`retryable` |
| `workflow.cancelled` | `workflow_run_id`、`workflow_id`、`reason`、`remaining_nodes` |

- 所有 `workflow.*` 事件的 `session_id` = 运行锚点会话（`workflow_runs.session_id`），保证 anchor 会话内 `seq` 单调；节点自身子会话的 `run.*`/`message.*` 事件仍使用子会话 `session_id`，**两套序列不混用**。
- 不新增事件类型行；`workflow.*` 精确子类型以本表为唯一登记。

**命令面（+11；63 → 74 实现前累计登记，M10 全部完成后 86）**

| 命令 | 参数（严格解析） | 返回 | 超时 | 主要业务码 |
|---|---|---|---|---|
| `workflow_create` | `{name, description?, definition, is_template?}` | `WorkflowSummary` | 30s | `workflow_invalid_definition` |
| `workflow_list` | `{limit?, offset?, include_templates?}` | `{items,total}` | 10s | `out_of_range` |
| `workflow_get` | `{workflow_id}` | `WorkflowDetail` | 10s | `workflow_not_found` |
| `workflow_update` | `{workflow_id, expected_version, name?, description?, definition?, is_template?}` | `WorkflowDetail` | 30s | `workflow_version_conflict` |
| `workflow_delete` | `{workflow_id, force?}` | `{deleted:true}` | 15s | `workflow_run_active` |
| `workflow_import` | `{source_path, name_override?}` | `WorkflowDetail` | 30s | `workflow_invalid_definition` |
| `workflow_run` | `{workflow_id, session_id, input?, priority?}` | `{workflow_run_id,status}` | 30s | `workflow_not_found`/`session_not_found` |
| `workflow_run_get` | `{workflow_run_id, include_nodes?}` | `WorkflowRunDetail` | 10s | `workflow_run_not_found` |
| `workflow_run_list` | `{workflow_id?, session_id?, status?, limit?, offset?}` | `{items,total}` | 10s | `invalid_enum` |
| `workflow_run_cancel` | `{workflow_run_id, reason?}` | `{status}` | 15s | `workflow_run_not_active`/`workflow_cancel_timeout` |
| `workflow_node_retry` | `{workflow_run_id, node_id, reset_downstream?}` | `{node_run_id,attempt}` | 30s | `workflow_node_not_retryable` |

- 全部命令纳入 M1-08 参数校验框架（`deny_unknown_fields` + 类型/枚举/长度/路径校验）；失败不落库、不产生事件、不透传下游。
- `workflow_delete` 为**软删除**（`workflows.deleted_at`，0010+ 规划位）；`workflow_list`/`workflow_get` 默认排除已删除；运行历史保留用于审计；`force=true` 先级联取消活动运行。
- `workflow_run` 必须提供 `session_id` 作为锚点；不支持「无会话工作流」，避免事件 `session_id` 语义漂移。
- `workflow_update` 使用 `expected_version` 乐观并发；冲突返回 `workflow_version_conflict`，不覆盖。

### D-031-6 迁移规划位（0010+，不预合入终态 DDL）

| 表 | 变更 | 用途 |
|---|---|---|
| `workflows` | `deleted_at INTEGER` | 软删除 |
| `workflow_runs` | `session_id TEXT REFERENCES sessions(id) ON DELETE CASCADE` | 事件锚点/外键 |
| `workflow_runs` | `definition_version INTEGER NOT NULL DEFAULT 1` | 定义快照版本 |
| `workflow_runs` | `definition_hash TEXT` | 定义快照哈希 |
| `workflow_runs` | `error TEXT` | 终态错误摘要 |
| `workflow_runs` | `cancel_requested_at INTEGER` | 取消确认 |
| `node_runs` | `UNIQUE(workflow_run_id, node_id, attempt)` | attempt 幂等 |
| `node_runs` | 索引 `(workflow_run_id, status)` | 运行视图 |
| `tasks` | 索引 `(workflow_run_id, node_id)`、`(session_id, status)` | 编排任务查询 |

- 上述变更**不修改 0001–0009**；由 M10-01 落库时与附录 C、`verify:m1-03`、`cargo test -p aether-store --test m1_03_schema` 同批合入；编号由落库任务按当时顺序确定（不预分配）。

### D-031-7 并发、持久化与恢复

- 10 并发工作流：节点 agent 运行进入既有并发闸门（ADR-022），工作流级 `max_parallel` 不得绕过闸门；队列/健康指标可查（`health.concurrency_gate` 增量）。
- 每节点状态推进使用存储写队列；批量提交目标 <500ms；`workflow_runs`/`node_runs` 写入失败 → 运行标 `failed` + `storage_backpressure` 业务码，不产生半成品成功。
- daemon 重启：启动 reconcile 把所有 `running` 运行/节点置 `interrupted`，记录 `reconciled_at`；UI 提供 `workflow_node_retry`/重新运行入口；**无重复、无悬挂**。
- 取消树：anchor session 取消 → 级联工作流取消；节点子会话取消 → 节点失败/取消；不扩散到无关分支。

## 3. 影响

- 《设计文档》v1.31：D7 命令面 **63（已实现）→ 86（登记；本 ADR +11，累计 74）**；D8 补 run 优先级/重试/超时；D12 启用预留表；附录 B 精确登记 `workflow.*` 子类型；附录 C 增 0010+ 规划位。
- P2 计划 v0.9：M10-01 解锁；G9 以本 ADR 的 DAG/并行/条件/汇总、10 并发、故障注入为验收口径。
- UI-UX v0.18：新增 S-14 工作流管理/运行视图（无画布）与锚点组 `workflow-list`/`workflow-editor`/`workflow-run-view`（+3 组）。
- 不新增运行时/依赖/设置键；不改变 EventEnvelope 9 字段；不改变 `IpcErrorCode` 32 枚举（业务码走 `business_code`）。

## 4. 主要风险与缓解

| 风险 | 缓解 | 失效条件 |
|---|---|---|
| 编排爆炸半径 | 节点/深度/fan-out/执行次数上限；默认 fail-fast；节点/运行超时；取消级联 | 任一上限被绕过或 10 并发失控 → 停止 M10-01 实现 |
| 定义 DSL 变成任意代码入口 | 固定 5 类节点；表达式纯函数白名单；无 shell/HTTP/eval | 出现任意代码执行路径 → 该能力作废重登记 |
| 运行态与事件不一致 | 先写后广播；reconcile；attempt 唯一键 | 出现重复执行/悬挂 → G9 不通过 |
| 长事务/写放大 | 批量写、目标 <500ms；运行态小行更新 | 写队列持续 >4096 或单事务 >500ms → 记录并优化 |

## 5. 备选方案（未采纳）

| 方案 | 原因 |
|---|---|
| 引入通用表达式语言/脚本节点 | 安全面与审计不可控；P3 沙箱前禁止 |
| 画布优先（UI-04 前移） | 违反 §4#1 与 P4 边界；M10 只做 JSON 编辑/运行视图 |
| 运行期自动重放 `running` 节点 | 可能重复副作用；改为 `interrupted` + 人工重试 |
| 无锚点会话的「系统工作流」 | EventEnvelope `session_id` 语义漂移；要求 `session_id` |
| 用 `tasks` 表替代 `workflow_runs`/`node_runs` | 破坏 D12 预留语义与运行视图查询 |

## 6. 契约登记汇总

- 命令：11（见 §2 D-031-5）；累计登记 63 → 74（M10-01 完成后），M10 全部 86。
- 事件：9 个 `workflow.*` 精确子类型（不新增附录 B 行）。
- 设置键：0。
- 迁移：0010+ 规划位 9 项（见 §2 D-031-6）。
- UI-UX 锚点：3 组（`workflow-list`/`workflow-editor`/`workflow-run-view`，共 9 个 testid）。
- 审计动作：`workflow.started`、`workflow.completed`、`workflow.failed`、`workflow.cancelled`、`workflow.node.retry`（由 M10-01 落库时在 ADR-023 台账增行；本 ADR 登记）。

## 7. 回退条件

1. 本 ADR 被否决 → M10-01 标注「未解锁」；删除 11 条命令/9 个事件子类型/0010+ 规划位登记；不写代码。
2. 实现中发现 DSL 需要任意代码/网络节点 → 停止实现，另立 ADR；不得静默扩展节点类型。
3. 10 并发或故障注入出现重复执行/悬挂 → 冻结 M10-01，修复后重跑 G9；不得以「演示通过」替代。
4. 0010+ 迁移与附录 C/`verify:m1-03` 不一致 → 以设计文档为准修正；修正前不得合入迁移。

## 8. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-11-05 | 文档维护（AI） | v1.0 创建 | 按 M10-00 DoD-1/2/3/4/6/8；登记先行，代码未改 |
| 2026-11-05 | 独立评审（AI，M10-00 批次） | 通过 | 检查 DSL 安全性、状态机终态、事件锚点、迁移规划位、命令计数；闭环见 `docs/M10-00-ADR批次评审.md` |

## 9. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v1.0 | 2026-11-05 | 创建：DSL/节点/状态机/失败重试/事件/11 命令/0010+ 规划位 | 文档维护 |

---

## 附录 A：定义示例（最小合法）

```json
{
  "schema_version": 1,
  "name": "review-then-summarize",
  "description": "并行审查两个文件后汇总",
  "nodes": [
    { "id": "start", "type": "task", "handler": "noop" },
    { "id": "fan", "type": "parallel", "fanout": 2, "max_parallel": 2 },
    { "id": "review_a", "type": "agent", "prompt": "审查 A", "context_refs": [{ "kind": "file", "path": "src/a.ts" }] },
    { "id": "review_b", "type": "agent", "prompt": "审查 B", "context_refs": [{ "kind": "file", "path": "src/b.ts" }] },
    { "id": "join", "type": "join", "mode": "all" },
    { "id": "summary", "type": "task", "handler": "emit_summary" }
  ],
  "edges": [
    { "from": "start", "to": "fan" },
    { "from": "fan", "to": "review_a" },
    { "from": "fan", "to": "review_b" },
    { "from": "review_a", "to": "join" },
    { "from": "review_b", "to": "join" },
    { "from": "join", "to": "summary" }
  ],
  "settings": { "fail_policy": "fail_fast", "timeout_ms": 1800000 }
}
```

## 附录 B：重试分类（固定）

| 分类 | 错误来源 | 自动重试 |
|---|---|---|
| 可重试 | `timeout`、`adapter_unavailable`、`provider_unreachable`、`storage_backpressure`（闸门回落且未超 attempts） | 是（仅 `max_attempts>1`） |
| 不可重试 | `permission` deny、`provider_auth_failed`、`invalid_value`/定义错误、`cancelled`、`session_disposed` | 否 |
| 不确定 | 适配器崩溃/未知 `internal` | 默认否；UI 提供人工 `workflow_node_retry` |

## 附录 C：运行视图字段（UI 最小集）

`WorkflowRunDetail`：`workflow_run_id`、`workflow_id`、`session_id`、`status`、`priority`、`started_at`、`finished_at`、`error?`、`nodes[]`（`node_id`、`type`、`status`、`attempt`、`started_at`、`finished_at`、`error?`、`result_summary?`）。不返回完整节点上下文/消息全文；需要时经 `messages_page` 读取节点子会话。
