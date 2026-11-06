# ADR-037：M10 调度器完整优先级、重试、路由策略与队列可观测性

| 项 | 内容 |
|---|---|
| 编号 | ADR-037（M10-00-07） |
| 版本 | **v1.0（2026-11-05）** |
| 状态 | **已接受（2026-11-05，M10-00 ADR 批次；用户授权 AI 审批；人工复核建议见 `docs/M10-00-ADR批次评审.md`）** |
| 决策日期 | 2026-11-05 |
| 决策载体 | 《设计文档》**v1.31**（D5/D8/D9、§5 P2）；`docs/P2-架构迁移-实施计划与验收标准.md` **v0.9**；`docs/M10-00-登记表.json` |
| 关联 | 设计 §5 P2、CP-02；ADR-015（热插拔）、ADR-017（暂停/恢复）、ADR-022（路由/并发闸门）、ADR-023（审计）、ADR-031（工作流节点）、ADR-032（子 Agent） |
| 取代 / 被取代 | 无（新增）；**扩展** ADR-022 的固定优先级/并发闸门，不改变既有 `session_send`/`run_retry` 语义 |
| 回退条件 | 见 §7 |
| 实现状态 | **未开始（登记先行；M10-07 执行）**；未登记前不得写业务代码 |

## 1. 背景与触发

P1 已实现固定优先级路由（`claude-code=10`、`codex=20`、`deepseek-harness=30`、`mock=100`）与并发闸门 `clamp(floor(free_ram_gb/1.5),2,10)`；M10-07 要求补齐优先级、超时、重试与完整调度策略，并保证 10 并发零错误路由、不超卖、不饥饿。ADR-022 明确「改变排序语义（加权/负载均衡）须另立 ADR」；本 ADR 即该增量，冻结 M10 调度语义。

## 2. 决策

### D-037-1 队列模型与优先级

- 保留现有两层：**全局运行队列**（`runs.status='queued'`）+ **会话级等待槽**（D8：每会话同时 1 个 run，运行中再发送入 1 条等待队列，超过 `session_busy`）。两层不重复计数（ADR-022 A-6）。
- 队列条目优先级分数（整数，越小越先）：
  ```
  score = base_priority
        + runtime_weight
        + wait_boost
  ```
  - `base_priority`：用户/调用方传入 `priority` ∈ **−100…100**（缺省 0；`session_send.priority`、`workflow_run.priority`、`subagent_spawn` 内部节点继承）。
  - `runtime_weight`：沿用 ADR-022 固定常量（`claude-code=10`、`codex=20`、`deepseek-harness=30`、`mock=100`；未登记=100）；**不按负载改权重**（负载只用于同分候选选择，见 D-037-3）。
  - `wait_boost`：`min(60, floor(wait_ms / 10000))`；等待越久分数越低（提升），**上限 60**，防止长期饥饿；达到 `starvation_guard_ms=300000`（5min）时额外 `-100` 并写审计 `scheduler.starvation_guard`（每 run 一次）。
  - 同分排序：`queued_at ASC` → `run_id ASC`（稳定、可复现）。
- **公平性**：每个会话最多 `max_queued_per_session=2`（含运行中 1 + 等待 1；超过 `session_busy`）；全局队列上限 `max_queued_global=64`，超过 → `storage_backpressure` 或 `session_busy`（按来源）；不做超卖。
- 优先级不由 UI 任意提升到越过安全边界：`permission deny`/`cancelled` 等终态不受优先级影响；用户不能通过优先级绕过并发闸门。
- 队列位置可查：`session_send` 响应沿用 `queue:{position,limit}`；新增 `queue_list` 返回全量/按会话的 `QueueEntry`（`run_id,session_id,position,priority,score,queued_at,wait_ms,wait_reason`）。

### D-037-2 超时与重试

- `session_send` 增可选 `timeout_ms`（**1s–24h**，缺省 30min；越界 `out_of_range`）；工作流节点用 `node.timeout_ms`（ADR-031）；`run_retry` 沿用既有「仅终态可重试」语义。
- 自动重试（M10-07 新增，默认关闭）：`max_attempts` ∈ 1–5（缺省 1）；仅当来源显式设置 `max_attempts>1` 时启用；退避 `1s/2s/4s/8s/16s/30s/60s` + ±20% jitter；同一 run 的 attempt 递增，旧 attempt 终态保留。
- 可重试分类（与 ADR-031 附录 B 同一份）：`timeout`、`adapter_unavailable`、`provider_unreachable`、`storage_backpressure`（闸门回落且未超 attempts）；不可重试：`permission` deny、`provider_auth_failed`、`invalid_value`、`cancelled`、`session_disposed`；未知 `internal` 默认不自动重试，UI 提供 `run_retry`。
- **不重复执行/不丢终态**：自动重试前检查 run 是否已终态；终态不可变；重试写新 attempt 记录；`run_retry` 与自动重试互斥（同一 run 仅一个在途 attempt）。
- 超时：到点先发 `session.interrupt`，适配器 5s 内未收口 → 标 `timeout`；不无限等待；写审计。

### D-037-3 路由策略（能力/健康/负载）

- 候选过滤（顺序）：
  1. `enabled=1` 且 `status != disabled`；
  2. 能力匹配：请求的 `runtime_id`（手动指定）或自动路由所需的协议/provider/model/`thinking_depth`/`pause_resume`/`multimodal`（按命令）匹配；
  3. 健康：排除 `degraded`/`crash_loop`/`start_failed`/`version_mismatch`/`untrusted`；`cold` 仅在无 `ready` 候选时作为候选（保持 ADR-022）；
  4. provider 协议兼容（ADR-028 v0.9）；provider 停用/删除 → 无候选；
  5. 负载：`active_runs / max_concurrent`（每运行时）作为同分候选的次级排序；不改变固定 runtime_weight 的主排序。
- 选择：最低 `runtime_weight` → 最低负载比例 → `id` 字典序；手动指定优先（ADR-022），但仍受闸门/健康约束。
- 无候选 → `invalid_value` / `no_candidate_runtime`（业务码沿用 ADR-022）；不自动降级到 Mock/未启用运行时。
- 审计：自动路由写 `route.decided`（actor=system；detail 含候选、过滤原因、选中、负载快照）；手动指定不写路由决策（沿用 ADR-022）。
- `health.concurrency_gate` 增 `queued_by_priority`/`starvation_guard`/`api.state` 可选字段；不改变既有字段语义。

### D-037-4 故障、重启与收口

- daemon 重启：`queued` run 保留并按原 `queued_at/priority` 重新入队（不丢）；`running` run 置 `failed`（`recoverable=true`）或 `timeout`？统一置 `failed` + `error.code=interrupted`，UI 可 `run_retry`；**不自动重放**（避免重复副作用）。
- 适配器断连：在途 run 标 `failed`（`adapter_unavailable`，可重试）；排队的 run 不受影响；适配器恢复后队列继续推进。
- 存储背压：L1/L2/L3 沿用 ADR-003/ADR-022；L2 拒绝新 run；L3 熔断适配器；队列不清空、不重复计数。
- 取消/终态释放：`session_interrupt`/`session_pause`/`run_retry`/`session_dispose` 必须释放队列槽与并发计数；取消排队 run → 出队 + 审计；无孤儿。
- 10 并发：10 会话/运行零错误路由；队列推进无死锁；`queue_list`/`health` 指标可查；长队列（≥64）无饥饿（等待 >5min 触发 guard）。

### D-037-5 命令面与迁移

| 命令 | 参数（严格解析） | 返回 | 超时 | 主要业务码 |
|---|---|---|---|---|
| `queue_list` | `{session_id?, limit?, offset?}` | `{items: QueueEntry[], total, gate:{limit,active,queued}}` | 10s | `session_not_found`/`out_of_range` |

- 现有命令增量：
  - `session_send`：可选 `priority`（−100…100）、`timeout_ms`（1s–24h）、`max_attempts`（1–5，缺省 1）；响应 `queue` 保持。
  - `health`：`concurrency_gate` 增 `queued_by_priority`/`starvation_guard`；`api.state`（ADR-033）。
  - `run_retry`：语义不变；新增可重试分类校验（不可重试 → `invalid_value` + `error.data.retryable=false`）。
- 迁移规划位（0010+，不修改 0001–0009）：
  - `runs.priority INTEGER NOT NULL DEFAULT 0`
  - `runs.queued_at INTEGER`
  - `runs.retry_count INTEGER NOT NULL DEFAULT 0`
  - `runs.max_attempts INTEGER`
  - `runs.wait_reason TEXT`
  - 索引 `runs(status, priority DESC, queued_at ASC)`
- 队列内存实现 + DB 持久化双写：入队写 `runs`，推进以 DB 为准；重启重建。

### D-037-6 UI/E2E

- 队列位置/等待原因：会话状态条/健康面板显示 `queue.position`、`wait_reason`（`concurrency_gate|runtime_disabled|provider_invalid|storage_backpressure|manual_hold`）；`queue_list` 刷新。
- 重试入口：`run_retry`（终态可重试）、工作流 `workflow_node_retry`；显示 `retryable`/`attempt/max_attempts`。
- 与 M7-02 现有 UI 一致：不新增页面；复用状态条/健康面板/错误 toast；不改变既有锚点。
- E2E：10 并发、长队列、取消、重启、适配器断连、存储背压；断言无超卖、无孤儿、无死锁。

## 3. 影响

- 《设计文档》v1.31：D8 补优先级/超时/重试/队列可观测；D5 路由补负载/健康；D7 命令面 +1（累计 86）+ 既有命令增量；附录 C 增 0010+ 规划位。
- P2 计划 v0.9：M10-07 解锁；G9 以 10 并发零错误/无超卖/无饥饿为验收口径（与 M10-01/02 合并）。
- UI-UX v0.18：不新增锚点组；复用 `session-status`/健康面板；队列原因文案见 §5.4。
- 不新增事件类型（run 状态沿用 `run.*`；队列位置走响应/`health`）；不扩展 `IpcErrorCode` 32。

## 4. 主要风险与缓解

| 风险 | 缓解 | 失效条件 |
|---|---|---|
| 饥饿/优先级反转 | wait_boost 上限 + 5min guard + 公平性测试 | 长队列出现无限等待 → G9 不通过 |
| 超卖 | 双层队列不重复计数；闸门前置；10 并发断言 | active > limit → 停止实现 |
| 重试重复执行 | attempt 记录；终态检查；旧 attempt 保留 | 重复副作用 → 冻结 M10-07 |
| 重启丢队列 | `runs` 持久化 + 启动重建 | 队列丢失/重复 → 修复后重跑 |
| 路由不确定 | 固定权重 + 明确 tie-break + 审计 | 同输入不同候选 → G9 不通过 |
| 背压死锁 | 复用 ADR-003/022；L3 熔断；队列不阻塞 reader | 死锁 → 停止实现 |

## 5. 备选方案（未采纳）

| 方案 | 原因 |
|---|---|
| 完全抢占式调度 | 中断在途 run 成本高/副作用；M10 用排队 + 优先级 |
| 按负载动态改 runtime_weight | 路由不确定/难复现；负载仅作同分 tie-break |
| 无等待加权 FIFO | 长任务饥饿；引入 wait_boost |
| 自动重放 running run | 重复副作用；改为 failed + 人工 retry |
| 队列放内存不落库 | 重启丢失；必须持久化 |

## 6. 契约登记汇总

- 命令：1（`queue_list`；累计登记 85 → 86）。
- 现有命令增量：`session_send.priority/timeout_ms/max_attempts`；`health.concurrency_gate`；`run_retry` 分类。
- 事件：0 新增。
- 设置键：0；迁移：0010+ 5 列 + 1 索引。
- 业务码：0 新增（复用 `session_busy`/`storage_backpressure`/`no_candidate_runtime`/`invalid_value`）。
- 审计：`route.decided`（既有）、`scheduler.starvation_guard`（新增，ADR-023 台账增行）、`run.retry`（既有/实现登记）。
- UI-UX 锚点：0 新增组。

## 7. 回退条件

1. 本 ADR 被否决 → M10-07 保持「未解锁」；保留 P1 固定优先级/FIFO；不写优先级列。
2. 10 并发/公平性/重启任一失败 → 冻结 M10-07；修复后重跑 G9；不得以「平均通过」替代。
3. 需要抢占式调度/动态权重 → 另立 ADR；M10 不扩范围。
4. 0010+ 迁移与附录 C/`verify:m1-03` 不一致 → 修正后合入；不得手改库。

## 8. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-11-05 | 文档维护（AI） | v1.0 创建 | 按 M10-00 DoD-1/2/7/8；登记先行，代码未改 |
| 2026-11-05 | 独立评审（AI，M10-00 批次） | 通过 | 检查优先级/公平性/重试分类/路由/10 并发/迁移；闭环见 `docs/M10-00-ADR批次评审.md` |

## 9. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v1.0 | 2026-11-05 | 创建：score/公平性/超时重试/路由/故障收口/queue_list/0010+ | 文档维护 |

---

## 附录 A：优先级示例

| base | runtime_weight | wait_ms | wait_boost | score |
|---|---|---|---|---|
| 0 | 10 | 0 | 0 | 10 |
| 0 | 20 | 60000 | 6 | 26 |
| −50 | 30 | 0 | 0 | −20 |
| 100 | 10 | 300000 | 60（+guard −100） | 70（guard 生效时 −30） |

> guard 仅在等待 ≥5min 时触发一次，写审计；不永久改变用户优先级。

## 附录 B：等待原因枚举

`concurrency_gate`（并发闸门满）、`runtime_disabled`（运行时禁用）、`provider_invalid`（供应商失效）、`storage_backpressure`（存储背压）、`manual_hold`（用户/审批暂停）、`adapter_unavailable`（适配器不可用）。
