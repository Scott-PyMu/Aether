# ADR-017：会话暂停/恢复语义（D6/D5 P1 方法扩展；P1-A02）

| 项 | 内容 |
|---|---|
| 编号 | ADR-017（P1-A02） |
| 版本 | **v0.3（批次评审闭环稿）**：v0.1 创建（批次稿）；v0.2 合并「核心层受控中断 + Mode R 重放」评审要点（修正 v0.1 `resume → idle` 与 M7-03 DoD1 `running → paused → running` 的矛盾；补恢复重放、重启收口义务、交互矩阵与 DoD 对齐；见 §8）；v0.3 批次评审闭环（§5-3/§5-4 备选裁定保留主稿、C10 计数订正；不改决策） |
| 状态 | **已批准（2026-10-09；人工授权 AI 审批——独立评审通过即批准，见 `docs/M5-01-ADR批次评审意见.md` §7）**；已合入《设计文档》v1.16 |
| 决策日期 | 2026-10-09 |
| 决策载体 | 《设计文档》v1.15 → v1.16（D5/D6/D7 增量与 §5 P1、附录 A CP-03 行）；ADR-006 附录 B 错误码增行（随批次 ADR-018 汇总）；《UI-UX 设计规格》v0.6 → v0.7（状态图/文案/锚点）；`docs/P1-实施计划与验收标准.md` v0.7（§9 行标注、§10 第 6 项闭合） |
| 关联 | 设计 D5/D6/D7/D8、§4#22、§5 P1、附录 A/B/E；需求 CP-03；ADR-003（Mode R/N 恢复，§6.4）、ADR-005（native_id 恢复键）、ADR-006（错误码登记）、ADR-008（三运行时）、ADR-010（能力项先例）、ADR-014（命令面基线 51）、ADR-015（命令面 52 / 全集 23 基线、`active_sessions` 含 paused 复核项 §5-6）；P1 计划 §1.1/§9/§10-6、M5-01/M7-03；实现现状：`crates/aether-core/src/session_state.rs`、`crates/aether-control/src/lifecycle.rs`、`crates/aether-tauri/src/session_backend.rs`、`packages/adapter-sdk/src/protocol.ts`、三适配器能力清单 |
| 取代 / 被取代 | 无（新增；合并 2026-10-09 同名核心层评审稿要点，重复稿已删除） |
| 回退条件 | 见 §6 |
| 实现状态 | 未开始（登记先行；由 M7-03 执行） |

## 1. 背景与事实

- 设计 §4#22：pause/resume 在 P0 不做（「适配器能力不确定，MVP 只保证 interrupt」），P1 补齐（CP-03 部分）；附录 A CP-03 行「P1：pause/resume；P2：恢复 UI」。
- 现状：`sessions.status` DDL 已含 `paused` 枚举（0001 迁移）；`crates/aether-core/src/session_state.rs:50-74` 的 19 边转移表白名单**已含** `running → paused`、`paused → running`、`paused → failed`、`paused → cancelled`（注明为 P1 预留），**无** `idle → paused`、`paused → idle`、`waiting_permission → paused`；D6 方法集无暂停相关方法；IPC 命令面无暂停入口；`session_send` 在 `paused` 态行为未定义（当前会照常受理，`crates/aether-control/src/lifecycle.rs:914-949`）；`retry_run` 未设 paused 门禁（`lifecycle.rs:650-704`）。
- 既有能力（P0 已实现）：`SessionManager::interrupt` 受控中断（取消树 + run `cancelled` + 任务看门狗兜底；`lifecycle.rs:1069-1133`）；`retry_run` 终态重放（复用原输入消息，Mode R/N 按 `sessions.config.native_id`；`lifecycle.rs:355-362,650-783`）；重启收口 `reconcile_interrupted_runs`（非终态非空闲会话含 `paused` → `idle`；`lifecycle.rs:844-865`；`docs/M3-06-证据.md` §4）。
- 三运行时事实：Claude Code（每 run spawn CLI，`--session-id`/`--resume`，中断 = 进程树回收；`packages/adapter-claude-code/src/claude-cli.ts:71-95`）、Codex（`codex exec [resume]`；`packages/adapter-codex/src/codex-cli.ts:45-75`）、DSH（ACP `session/prompt`，中断 = `session/cancel` 5s 兜底；`packages/adapter-dsh/src/dsh-adapter.ts:8-9`）；三者均有 Mode R 原生续聊证据（M2-02/M2-11），**均无原生挂起/继续接口**。D6 方法表冻结为 9 方法（`packages/adapter-sdk/src/protocol.ts:64-75`）；能力项先例 = `thinking_depth`（存在即支持，设计 D6 `:420`）。
- 需定界的语义问题（P1 计划 §10 待确认 6）：**语义层级（适配器原生 vs 核心层）与三运行时支持矩阵**；以及 DoD 明示约束：不产生「假暂停」、恢复后上下文连续（Mode R 口径）、暂停期事件 seq 连续、与 interrupt/dispose/看门狗交互正确、重启后暂停态语义（M7-03 DoD1–6）。
- **为什么必须走 ADR**：本批次触及冻结面——D6 方法集、19 边会话状态机白名单（`session_state.rs` 自述「转移表为冻结契约，增删需评审」）、D7 命令面与错误码、M3-06 重启收口口径、UI-UX 锚点契约；AGENTS §3 禁止静默修改冻结决策与状态语义。

## 2. 决策

### 2.1 核心决策（五段式）

- **决策**：P1 pause/resume 采用**核心层「受控中断 + 恢复重放」语义**（不依赖适配器原生挂起；三运行时均无原生暂停/继续接口）：
  - `session_pause`：会话进入 `paused`（**门禁态**）。若存在在途 run，先按**既有 interrupt 路径**受控收口为 `cancelled`（`reason=user_pause`，`recoverable=true`，可 `run_retry`）；等待队列（queued run）**保留不提升**；绑定该 run 的权限等待级联取消（D8 取消树）。随后对该会话下发线协议 **`session.pause`**（净 +1；适配器取消残余工作并释放运行时侧会话资源；`native_id` 恢复能力必须保留）；**不新增 `session.resume` 方法**。
  - `session_resume`：存在**暂停标记**（= 被中断 run，内存态）→ 以 `run_retry` 等价路径**重放**该输入并按 ADR-005 **Mode R** 延续原生上下文，会话 `paused → running`（先转移、后 spawn）；无标记（含 idle 暂停、重启后）→ 会话 `paused → idle`，由用户显式 `run_retry`/发送继续。恢复失败按既有 run 失败路径收口（`run.failed`，可重试），不新增失败面。
  - 支持门：适配器须在 `hello.runtime.capabilities` 声明能力项 **`pause_resume`**（= 可受控中断 + Mode R 原生恢复能力）；未声明或连接未就绪 → `session_pause` 返回 **`pause_unsupported`**，**不做任何状态转移**（不产生假暂停）；UI 按能力置灰。
- **实现要点**：
  1. **D6 方法集净 +1**：`session.pause`（超时 10s；核心→适配器；语义 = 在核心侧受控中断收口**之后**，通知适配器取消残余工作、释放该会话运行时侧资源，保留 `native_id` 恢复能力）。调用为**收尾性 best-effort**：成功/失败均完成核心侧暂停收口（失败记诊断 + 审计 detail，不阻塞、不自动重试）。**不新增 `session.resume`**（恢复复用 Mode R 启动路径，避免第二个失败面）。
  2. **状态机（本 ADR 为显式修订；19 边 → 22 边）**：
     - 合法（新增边以「+」标注）：`running → paused`（既有）、`idle → paused`（+）、`waiting_permission → paused`（+）、`paused → running`（既有；有标记重放）、`paused → idle`（+；无标记解除）。
     - 非法（拒绝并返回结构化错误）：`paused → paused`；非 `paused` 态执行 resume；`creating`/终态执行 pause；`paused` 态执行 `session_send`/`run_retry`（`invalid_value`，message 指明当前状态与稳定业务码，见附录 A-6）；其余仍按原 19 边白名单拒绝。
     - 每次转移写 `sessions.status` 并广播既有 `session.status_changed`（from/to；**不新增事件类型**）；`idle → paused` 为纯门禁暂停（无在途 run、无标记）。
  3. **暂停流程**：① 能力门 + 状态检查（`idle`/`running`/`waiting_permission`）；② 受控中断（在途 run 摘除 + 取消树级联 + `run.cancelled(reason=user_pause)` + 看门狗兜底；`waiting` 保留）；③ 下发 `session.pause`（best-effort）；④ 状态转移（`running`/`idle` 一跳；`waiting_permission → paused` 直连边）+ `session.status_changed` + 审计 `session.pause`；⑤ 有在途 run 时登记暂停标记（**内存，不落库**）。
  4. **恢复流程**：① 状态 = `paused` + 能力门 + 准入检查（`pipeline.admission()` + 背压 + 运行时可用：disabled/未注册 → 拒绝并保持 paused）；② 有标记：以 `retry_run` 等价路径创建重放 run（复用原输入消息与 `client_msg_id`；旧 run 保留审计；新 run 先于保留的 queued run 设为 active）→ `paused → running`（先转移、后 spawn，避免 `dispatch_and_finalize` 的 `idle` 守卫竞态）+ 审计 `session.resume`，清除标记；③ 无标记：`paused → idle` + 审计 `session.resume`。
  5. **门禁与交互**（详见附录 A-6 交互矩阵）：`paused` 态 `session_send`/`run_retry` 一律拒绝（不排队、不绕过）；`session_interrupt` 于 paused = no-op；`session_dispose` 于 paused → 终态 `cancelled`（修正既有 `dispose_single` 在 paused 无在途时误算 `completed` 的路径，避免命中白名单外转移）；看门狗/断流超时对 paused 不适用（active 已摘除，迟到结果按既有「已摘除忽略」路径）；`persist_degraded`/写队列背压不改变暂停态（恢复受准入门约束）；`runtime_disable`（ADR-015）计入 `active_sessions`（含 paused）、不自动丢弃暂停标记（重启用后仍可恢复）。
  6. **重启语义**：`paused` 持久化于 `sessions.status`；核心重启后**保持 `paused`（不自动恢复、不自动清理）**——M7-03 调整 `reconcile_interrupted_runs`：`creating`/`running`/`waiting_permission` 维持既有 `idle` 收口，**`paused` 不再收口**；暂停标记不跨重启（内存），重启后 `session_resume` 走 `paused → idle`（被中断 run 已 `run_interrupted` 失败，可在 UI 显式 `run_retry`，重放按 Mode R/N）。
  7. **审计与诊断**：`session.pause` / `session.resume`（actor=user，result 含 from→to、在途 run 收口摘要、`session.pause` 下发结果）；诊断包含暂停次数/当前暂停会话数统计（M7-03 DoD6）。
  8. **UI/命令**：`session_pause { session_id }`、`session_resume { session_id }`（严格解析；ULID 校验；状态校验；回执见附录 A-1）；UI 明示**「暂停将中断当前回复；恢复将重放该输入（原生上下文保留，Mode R）」**（不产生假暂停的用户侧承诺）；锚点/文案见附录 A-8。
- **主要风险**：用户误以为「暂停 = 保留在途输出」（实际在途 run 被受控中断，恢复为重放）；Mode N-only 运行时被误启用（假暂停/假恢复）；`native_id` 失效导致重放退化为新会话而用户无感；pause 与看门狗/dispose 竞态导致状态机悬挂；暂停标记内存态与重启后重放能力退化（重启后仅解除暂停）；D6 `session.pause` best-effort 失败被误读为「未暂停」。
- **缓解措施**：能力项 `pause_resume` 硬门 + UI 置灰 + 命令层实时复核；pause 回执与 UI 明确「在途任务已中断，可重试；恢复将重放」；重放按 Mode R，失败以既有 run 失败/`failed` 呈现（不静默；Mode N 时 UI 沿用 ADR-005「原生上下文可能丢失」明示）；取消树复用保证单一收口路径；状态锁内顺序转移 + observer 迟到 no-op 竞态用例；`session.pause` 失败落审计/诊断且不影响暂停态（核心侧已无可运行 run）。
- **失效条件**：pause/resume 出现**任何数据丢失或事件 seq 断档** → 立即回退至「P0 语义（仅 interrupt，无 paused 状态入口）」，暂停功能停用并另立 ADR；出现「暂停后该会话仍产生 run 级事件/工具副作用且无终态收口」= 假暂停 → 停止合入；Mode R 在三运行时出现系统性恢复失败 → 该运行时撤销 `pause_resume` 声明（UI 置灰），并把「恢复 = Mode N 明示」议题另立 ADR。

### 2.2 三运行时支持矩阵（登记口径）

| 运行时 | `pause_resume` 声明 | 依据 |
|---|---|---|
| Claude Code | ✅（Mode R 已确认） | M1-11 spike 结论（ADR-005）；M2-02 回归 |
| Codex | ✅（适配器持久化别名 + Mode R 语义） | ADR-008 §3.3；M2-11 回归 |
| DeepSeek Harness | 按 M7-03 实测登记（声明须有真实恢复证据；无证据 = 不声明） | ADR-002/ADR-008；M2-11 回归；M7-03 DoD2/3 |
| Mock | ✅（测试构建按确定性实现声明；负向注入可关闭供置灰用例） | 一致性测试包需要 |

> 矩阵最终值以 M7-03 任务证据的实测结论为准；**未标 ✅ 的一律置灰 + `pause_unsupported`**（禁止静默按「部分等价」实现）。

### 2.3 M7-03 DoD 对齐（本 ADR 落点）

| M7-03 DoD | 本 ADR 决策 | 说明 |
|---|---|---|
| DoD1 状态机 `running → paused → running` 合法、非法转移拒绝 | 2.1-2（既有边保留；新增 3 边；非法清单） | **v0.1 `resume → idle` 已修正为有标记时 `paused → running`（重放）** |
| DoD2 暂停期不丢事件、seq 连续；恢复后按 Mode R 上下文连续 | 2.1-3/4（受控中断先日志后广播；重放走 Mode R；真实运行时抽检 + Mock 断言） | 事件复用 `run.cancelled`/`session.status_changed` |
| DoD3 不支持暂停的运行时：UI 置灰 + 结构化错误，不产生假暂停 | 2.1 支持门 + `pause_unsupported` | 能力判定 = `pause_resume` |
| DoD4 与 interrupt/dispose/看门狗/断流超时交互正确 | 2.1-5 + 附录 A-6 交互矩阵 | 取消树回归 |
| DoD5 重启后暂停态语义按 ADR | 2.1-6（paused 保留；标记不跨重启；resume→idle） | 需 M7-03 调整 reconcile（仅 paused 不再收口） |
| DoD6 暂停/恢复入审计；诊断含暂停统计 | 2.1-7 + 附录 A-3 | — |

## 3. 影响

### 3.1 《设计文档》v1.15 → v1.16（本 ADR 合入要点）

- D5 实现要点：补「pause/resume（P1，ADR-017）」段（核心层受控中断 + 恢复重放、门禁与标记、能力门、重启语义、交互矩阵引用）。
- D6 方法表：`session.pause | 10s | 核心侧受控中断收口后，通知适配器取消残余工作并释放运行时侧会话资源（保留 native_id 恢复能力）；best-effort`；能力项清单补 `pause_resume`。
- D7 命令面：+2（`session_pause`/`session_resume`，参数与校验矩阵见本 ADR 附录 A-1；命令面计数 52 → 63 由 ADR-018 汇总登记）；命令参数校验清单补两条（ULID + 状态前置）。
- 附录 A CP-03 行补注（P1：pause/resume = 核心层受控中断 + Mode R 重放，ADR-017）；§5 P1「新增能力」暂停/恢复行补注实语义（同）。
- 附录 B（事件类型）：不变；注明「新增发射点 = `session.status_changed`（暂停/恢复）」。

### 3.2 `docs/P1-实施计划与验收标准.md` v0.6 → v0.7

- §9 P1-A02 行标注正式编号 **ADR-017（2026-10-09）**；§10 第 6 项闭合（语义层级 = 核心层受控中断 + Mode R 重放；支持矩阵 = §2.2）；§1.1 映射表「暂停/恢复（CP-03）」行补注（ADR-017）。
- M7-03 描述补「语义按 ADR-017」；**DoD 文本不变**（§2.3 对齐表）。

### 3.3 《UI-UX 设计规格》v0.6 → v0.7

- §3.3 会话状态图增用户暂停/恢复边与 `paused` 呈现行（P0 不可达注改为「P1 按 ADR-017 可达」）；§5 文案表增 `paused` 与 `pause_unsupported` 行；§7.3 锚点增 `session-pause-toggle`；§2.4 增暂停语义常驻声明；§0.1 C2 行更新（P1 解除禁止按钮，能力未声明时置灰）。要点见附录 A-8。

### 3.4 ADR-006 附录 B（经 ADR-018 汇总登记）

- 错误码 +1：`pause_unsupported`（逐行文本见附录 A-2；全集 23 → 32 与登记行 11 → 20 的总账由 ADR-018 附录 A-2/A-3 汇总）。

### 3.5 不影响的

- 不新增事件类型（`session.status_changed` 为附录 B 既有类型）；不改线协议 major / 帧上限（2MiB / `artifact_ref` <1MiB）；不引入新依赖；不改迁移 0001–0003（`paused` 枚举与状态列已存在，**本 ADR 无迁移**；暂停标记不落库）；不改 D9 权限矩阵与回环；不改 P0 T1–T15 口径与 Gate 1–4；官方运行时集合仍冻结为三（ADR-008）。

## 4. 版本（拟合入）

| 文档 | 修订前 | 修订后 |
|---|---|---|
| 《设计文档》 | v1.15 | **v1.16**（随 M5-01 批次） |
| 《UI-UX 设计规格》 | v0.6 | **v0.7** |
| ADR-006 附录 B | 全集 23 / 登记行 11 | 全集 **32** / 登记行 **20**（批次汇总，见 ADR-018） |
| 《P1-实施计划与验收标准》 | v0.6 | **v0.7** |

## 5. 后续（未决项）

1. `paused` 会话的自动清理策略（长期滞留）——P1 不实施，转 P2 评估（不阻塞）。
2. 适配器原生「真暂停」（保留在途模型输出）——不在 P1；如需另立 ADR（本 ADR 能力项 `pause_resume` 语义预留可复用）。
3. **D6 `session.pause`（best-effort）保留性**：本稿按批次稿保留（D6 净 +1）；备选口径 = 纯核心层不扩展 D6（暂停收口不依赖适配器配合）。**批次评审裁定（2026-10-09）：保留主稿**（D6 `session.pause` 保留）。
4. **`waiting_permission → paused` 直连边**：本稿按新增边（单跳、事件简洁）；备选口径 = `waiting_permission → running → paused` 两跳（零新增边，但多一次状态事件）。**批次评审裁定（2026-10-09）：保留主稿**（直连边）。
5. `session_send` 响应中暂停态拒绝的稳定业务码命名（`session_paused`，附录 A-6）与 UI 文案随 M7-03 定稿微调。

## 6. 回退条件

1. **本 ADR 被否决** → M7-03 回退为「仅 interrupt + 状态展示」；`session_pause`/`session_resume` 与 D6 `session.pause` 撤回，命令面计数回落 52，错误码 `pause_unsupported` 撤回；`paused` 维持不可达（UI-UX C2 恢复）。
2. **pause/resume 数据事故**（事件断档/消息丢失）→ 按 §2.1 失效条件回退并另立 ADR。
3. **某运行时假暂停** → 撤销其 `pause_resume` 声明并置灰；不得以 UI 兜底。
4. **新错误码需求** → 先按 ADR-006 附录 B 登记（增行），不得自造。
5. **恢复重放被否决**（要求仅解除门禁）→ `paused → running` 不再由 resume 触发，DoD1 与状态机边需同步修订（另立 ADR 或随批次修订 M7-03 DoD），不得静默改变已登记转移语义。
6. **重启保留 paused 被否决** → 回退为 M3-06 既有收口（paused → idle）并同步修改 §2.1-6/DoD5 断言。
7. **`dispose` 于 paused 转 `cancelled` 修正被否决** → 需新增 `paused → completed` 边或改关闭口径，属状态机语义变更，须另立 ADR。

## 7. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-10-09 | 文档维护（AI） | v0.1 创建 | 按 P1 计划 M7-03 DoD 与 §10-6 定界；登记先行，代码未改 |
| 2026-10-09 | 会稿（核心层评审稿合并） | v0.2 合并修订 | 修正 v0.1 与 DoD1 的矛盾（resume 重放）；补重启收口义务/交互矩阵/UI/DoD 对齐；重复稿已删除 |
| 2026-10-09 | 独立评审（M5-01 批次，`docs/M5-01-ADR批次评审意见.md`） | 通过（附批次裁定） | v0.2 与 M7-03 DoD1 的一致性获核验；§5-3（D6 `session.pause` 保留）与 §5-4（`waiting_permission → paused` 直连边）裁定保留主稿；本 ADR 无 P0/P1 遗留 |
| 2026-10-09 | 文档维护（AI） | v0.3：批次评审闭环 | §5-3/§5-4 裁定落文；A-8 C10 命令总计数订正为 63（与 ADR-018 总账一致）；不改决策 |

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-10-09 | 创建：冻结式 pause/resume 语义、`session.pause` 线协议方法（净 +1）、能力项 `pause_resume`、状态机与重启语义、IPC 2 命令、错误码 `pause_unsupported`、三运行时支持矩阵 | 文档维护 |
| v0.2 | 2026-10-09 | **合并「核心层受控中断 + Mode R 重放」评审稿**：① 修正恢复语义——有暂停标记时 `paused → running` + `run_retry` 等价重放（Mode R），无标记时 `paused → idle`（对齐 M7-03 DoD1/DoD2；原 `resume → idle` 单口径撤回）；② 明确 `session.pause` 为受控中断收口后的 best-effort 收尾调用（失败不阻塞暂停、不重试）；③ 状态机修订显式化为「19 → 22 边」并新增交互矩阵、DoD 对齐表、UI 契约；④ 重启语义补 M7-03 调整 reconcile 义务（仅 paused 不再收口；标记不跨重启）；⑤ 补 §5 未决项（D6 方法与 `waiting_permission → paused` 两处备选口径） | 文档维护（合并稿） |
| v0.3 | 2026-10-09 | 批次评审闭环（不改决策）：§5-3/§5-4 备选口径裁定保留主稿；A-8 命令总计数订正 63 | 文档维护 |

---

## 附录 A：登记（命令 / 错误码 / 审计 / D6 / 能力项 / 矩阵）

**A-1 新增命令（+2）**

| 命令 | 参数（严格解析） | 校验与语义 | 响应 |
|---|---|---|---|
| `session_pause` | `{ session_id: string }` | ULID（否则 `invalid_format`）；会话存在（否则 `session_not_found`，随 ADR-025 登记）；状态 ∈ `idle/running/waiting_permission`（否则 `invalid_value`）；适配器声明 `pause_resume`（否则 `pause_unsupported`，不转移）；在途 run 收口 `cancelled`（`reason=user_pause`，recoverable，可 `run_retry`）；等待队列保留 | `{ session_id, status: "paused" }` |
| `session_resume` | `{ session_id: string }` | ULID；会话存在；状态 = `paused`（否则 `invalid_value`）；有暂停标记且准入通过 → 创建重放 run 并 `paused → running`；无标记 → `paused → idle` | `{ session_id, status: "running" \| "idle", run_id?: string }`（`run_id` = 重放 run，仅 `running` 分支） |

- 两条命令纳入 M1-08 式畸形参数矩阵（未知字段/缺参/非字符串/非法 `session_id`）；失败不落库、不产生事件。

**A-2 ADR-006 附录 B 错误码增行（逐行文本）**

| code | 语义 | 触发点 | 前端行为 |
|---|---|---|---|
| `pause_unsupported` | 运行时未声明 `pause_resume` 能力或连接未就绪，拒绝暂停（不产生假暂停） | `session_pause`（能力检查失败） | 暂停按钮置灰 + 提示「当前运行时不支持暂停」；不改变会话状态 |

**A-3 审计动作**

| 动作 | actor | 说明 |
|---|---|---|
| `session.pause` | `user` | 请求与结果（from→to、在途 run 收口摘要、`waiting` 保留数、`session.pause` 下发结果） |
| `session.resume` | `user` | 请求与结果（from→to、重放 run id（如有）） |

**A-4 D6 方法登记**

| 方法 | 方向 | 超时 | 说明 |
|---|---|---|---|
| `session.pause` | 核心→适配器 | 10s | 在核心侧受控中断收口**之后**调用：通知适配器取消残余工作、释放该会话运行时侧资源；`native_id` 恢复能力（Mode R）必须保留；best-effort——失败/超时记诊断与审计，不阻塞核心侧暂停收口、不自动重试；未声明 `pause_resume` 的适配器不应收到本方法 |

**A-5 能力项登记**

| 能力项 | 声明位置 | 判定 |
|---|---|---|
| `pause_resume` | `hello.runtime.capabilities` / `initialize` | pause/resume 时同步判定（Ready/Degraded 可判；Connecting/Disconnected 视为未声明）；未声明 → `pause_unsupported` + UI 置灰 |

**A-6 状态机修订与交互矩阵**

状态机（本 ADR 显式修订；白名单 19 → 22 边）：

| 边 | 类型 | 触发 |
|---|---|---|
| `running → paused` | 既有 | `session_pause`（在途 run 已受控中断） |
| `paused → running` | 既有 | `session_resume`（有暂停标记 → 重放，先转移后 spawn） |
| `idle → paused` | **新增** | `session_pause`（纯门禁暂停，无在途 run、无标记） |
| `waiting_permission → paused` | **新增** | `session_pause`（权限等待级联取消 + 在途 run 中断后直连转移） |
| `paused → idle` | **新增** | `session_resume`（无标记，含重启后） |

非法（拒绝，`invalid_value`，message 指明当前状态）：`paused → paused`；非 `paused` 态 resume；`creating`/终态 pause；`paused` 态 `session_send`/`run_retry`（稳定业务码 `session_paused`，见 §5-5）。

交互矩阵（paused 态）：

| 交互对象 | 行为 | 依据/落点 |
|---|---|---|
| `session_send` | 拒绝（不排队；稳定业务码 `session_paused`） | `lifecycle.rs:914-949` 增门 |
| `run_retry` | 拒绝 `session_paused`（防绕过暂停门禁） | `lifecycle.rs:650-704` 增门 |
| `session_interrupt` | no-op（不动标记、不动等待队列） | 决策 2.1-5 |
| `session_dispose` | → `cancelled`（修正 paused 无在途时误算 `completed` 的路径） | `dispose_single` |
| run 断流看门狗（120s） | 不适用（active 已摘除；迟到结果按「已摘除忽略」） | D8；`lifecycle.rs:2179-2214` |
| 任务看门狗（10s dump/强清） | 对被中断任务照常生效 | M2-05 |
| `persist_degraded` / 写队列背压 | 不改变暂停态；恢复受准入门拒绝（保持 paused） | D4/D8 |
| `runtime_disable`（ADR-015） | 计入 `active_sessions`（含 paused；闭合 ADR-015 §5-6 复核项）；不自动丢弃暂停标记；恢复时 runtime 不可用 → 拒绝并保持 paused | ADR-015 §2.2/§5-6 |
| 核心重启 | `paused` 保留；标记不跨重启；resume → `paused → idle`；被中断 run 可手动 `run_retry` | 决策 2.1-6 |

**A-7 M7-03 承接义务（计划回流要点）**

- 实现门禁三处：`send`/`retry_run` 的 paused 拒绝、`dispose` 于 paused 转 `cancelled`、`reconcile` 仅对 paused 不再收口（保持 `creating/running/waiting_permission → idle` 既有路径）。
- 恢复重放复用 `retry_run` 等价路径（复用原输入消息；旧 run 保留；新 run 先于 queued run 成为 active）。
- 真实运行时抽检：≥1 运行时「暂停 → 恢复重放 → recall 命中原生上下文（Mode R）」端到端证据；Mock 确定性断言同口径（M7-03 DoD2）。
- 负向夹具：`pause_resume` 未声明的 Mock 变体（沿用 `--no-thinking-depth` 式注入先例，命名入任务证据）断言置灰 + `pause_unsupported` + 零状态转移。
- P1 计划回流：§9 P1-A02 行标注 ADR-017；§10-6 闭合；§1.1 CP-03 行补注；M7-03 描述补「语义按 ADR-017」。

**A-8 UI 锚点与文案（UI-UX v0.7 要点）**

| 项 | 内容 |
|---|---|
| 锚点 | `session-pause-toggle`（`data-session-id` / `data-status`：`running`/`paused`/`idle`；`data-disabled`：能力未声明或连接未就绪时置灰）；会话列表暂停标记沿用 `session-item-status` |
| 文案（§5） | `paused`：「已暂停：当前回复已中断；恢复后将重放该输入（原生上下文保留）」；操作 = 恢复 / 关闭会话。`pause_unsupported`：「当前运行时不支持暂停/恢复（需原生会话恢复能力）」；操作 = 无（按钮置灰） |
| 常驻声明（§2.4） | 「暂停 = 中断当前回复并冻结会话；恢复 = 重放被中断的输入（原生会话上下文保留，Mode R）；暂停不承诺保留未完成的生成内容」 |
| 计数同步 | §0.1 C10 命令总计数 52 → **63**（批次总账见 ADR-018；本 ADR 贡献 +2） |
