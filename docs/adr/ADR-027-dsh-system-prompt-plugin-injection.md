# ADR-027：DSH 系统提示词插件注入（D14/D7；ADR-002 扩展）

| 项 | 内容 |
|---|---|
| 编号 | ADR-027 |
| 版本 | **v1.1（评审修订稿）**：v1.0 实施稿；v1.1 按 `docs/ADR-027-评审意见.md`（P1-1/P2-1/P2-2/P2-3）修订，新增 `system_prompt_effective` 三态语义、风险/缓解小节、多 profile 检查项与评审留痕 |
| 状态 | **已通过（2026-10-20）**：v1.1 评审整改（P1-1/P1-2/P2-1/P2-2/P2-3、N-3）全部闭环；最终验证 `cargo test --workspace` 877 passed / 0 failed / 11 ignored、`cargo clippy -D warnings` exit 0、`verify:m7-09` 18/18；实现与证据见 `docs/M7-09-证据.md` §4.6/§4.7/§7.6、`docs/ADR-027-评审意见.md` §8.5 |
| 决策日期 | 2026-10-20 |
| 决策载体 | 《设计文档》v1.16 → **v1.17**（D5/D6/D14 增量、§5 P1 落点）；`docs/P1-实施计划与验收标准.md` M7-09 执行状态补充（任务进度，不升版）；本 ADR v1.1 评审修订（`docs/ADR-027-评审意见.md`，2026-10-20） |
| 关联 | 设计 D5/D6/D7/D14/D9、附录 B；ADR-002（DSH `--patch` 注入）、ADR-008（三运行时集合冻结）、ADR-024（供应商消费映射）、ADR-026（预设合成）、M7-09 / M2-11 / M7-05 |
| 取代 / 被取代 | 无（扩展 ADR-002；不改其 delta 通道决策） |
| 回退条件 | 见 §6 |
| 实现状态 | v1.0 已实现：`packages/adapter-dsh/src/dsh-plugin.ts`（插件包）、`dsh-adapter.ts`（侧信道与生命周期）、`scripts/test/m7-09/dsh-prompt-e2e.mjs`（真实 DSH E2E）；**v1.1 `system_prompt_effective` 三态字段已于 2026-10-20 代码承接**（`dsh-adapter.ts` + `dsh-adapter.test.ts`，见 §10-1），插件/侧信道/清理/E2E 主链路不变 |

## 1. 背景

- M7-09/ADR-026 §2.2-5：核心把「预设块 → 工作区记忆块」合成的 `system_prompt` 随 `session.create` 下发适配器；Claude Code（CLI `--append-system-prompt-file`）与 Codex（`-c developer_instructions=…`）已完成消费闭环。
- DSH 官方 ACP 面**没有**系统提示词字段：`session/new` 只接受 `{cwd, mcpServers}`，`session/set_config_option` 仅 `model` / `reasoning_effort`（0.2.1-rc.2 复测；见 M7-09 证据 §4.5.1/§4.6）。因此仅靠 ACP 无法让 DSH 感知 `session.create.system_prompt`。
- DSH 公开插件契约提供 `ctx.systemPrompt.section(...)`；`agent/created` 在首次提示词组装前派发，且通过 `agent.ctx` 注册的贡献按 agent 作用域隔离（`dsh-system-prompt` 类型定义与 README；0.2.1-rc.2 实测）。
- ADR-002 已确立「`--patch` overlay + 自研 Cordis 插件」的注入机制，本 ADR 只扩展其用途（系统提示词注入），不改 delta 带外通道/权威终态/去重规则。

## 2. 决策

1. **通道**：新增自研插件包 `aether-dsh-prompt`（与 `aether-dsh-stream` 同机制，放入 `profiles/<profile>/node_modules`，`package.json` 无 BOM）；overlay 的 `- insert:` 同时挂载两个插件。
2. **作用域与时机**：插件在 `agent/created` 回调内通过 `agent.ctx.systemPrompt.section(...)` 注册 **agent 作用域** section：
   - `name = "aether:session-prompt"`；
   - `order = 100`（harness 身份 `-1000`、persona 前缀 `0` 之后；plan `500`、工具段 `1000+` 之前）；
   - `text` 为每次 assemble 求值的函数，读取侧信道文件；缺失/空文件返回空串（`renderPrompt` 丢弃空段）。
   - `interpolate: false`：用户提示词按原文注入。DSH 默认对 `{{variable}}` 做严格插值，未知变量会使整次组装失败；预设/记忆正文可能包含模板语法，必须禁止插值。
   - cordis 服务注入门控：`export const inject = ["systemPrompt"]`（未声明时访问服务会抛错）。
3. **侧信道**：适配器在 `session/new|resume` 返回后，把 `session.create.system_prompt` 整段写入 `<deltaDir>/prompts/<encodeURIComponent(sessionId)>.md`；插件按 `agent.id`（实测等于 ACP `sessionId`）读取。`session.dispose` 与 `shutdown` 删除文件；启动时清理上次进程残留的 `.md` 文件；恢复会话但未下发提示词时清理同 id 残留；写入/删除失败只记诊断、不阻断会话（与 D14「记忆注入尽力而为」一致），但**写失败必须以 `system_prompt_effective=false` 向下反映**，不得静默回显为「已生效」（v1.1）。
4. **消费契约（v1.1 三态化）**：DSH 适配器 `session.create` 解析并回显两个字段（均不含正文）：
   - `has_system_prompt: boolean`：**核心已下发**语义（与 v1.0 及既有测试向后兼容），不表示注入已生效；
   - `system_prompt_effective?: boolean | null`：**注入生效**语义——`null`/缺省 = 核心未下发；`true` = 侧信道写入成功；`false` = 写入失败或侧信道目录未初始化（注入未生效）。
   未携带 → 不写文件、不注册非空 section；`system_prompt_effective` 为 `null`/缺省。核心下发的合成结果**原样注入**，插件不拆分预设块/记忆块（保持 ADR-026 合成语义）。
5. **版本门闩**：DSH pin 从 `0.1.5-rc.2` 升级为 **`0.2.1-rc.2`**（本机实测；插件契约 `agent/assistant-stream`、`agent/created`、`agent.ctx.systemPrompt`、ACP 面均复测）。pin 仍为精确匹配；升级需同批复测本 ADR 的 E2E 与会话映射断言。
6. **安全与边界**：
   - 仅注入提示词文本，不经工具调用、不触 D9 权限门；插件运行在 DSH 进程内（信任级边界不变）；
   - 侧信道位于应用数据目录 `<DSH_HOME>/aether-bridge` 下，不写用户工作区；文件名为 URL 编码（防御 `native_id` 路径分隔符）；**该目录不进入 DS-05 P1 自动备份范围**（产品确认，2026-10-20）；
   - 不记录提示词正文到日志/审计/诊断包；`has_system_prompt` 仅布尔回显，`system_prompt_effective` 仅三态回显（`null`/`true`/`false`）。
7. **不新增（v1.1 口径）**：不改 ACP 协议、不改 D6 线协议版本、不新增事件类型、不新增 IPC 命令/错误码、不新增迁移；仅按 D6「跨 `v` 版本允许新增字段（只增不改语义）」在 `session.create` 响应中新增可选字段 `system_prompt_effective`。

## 3. 硬约束（缺一不可）

1. 插件必须在 `agent/created` 派发窗口内完成 section 注册（首次提示词组装之前）；若未来 DSH 把窗口后移，需在 `AgentSetup` 作用域注册并复测，不得以全局 section 替代（会串会话）。
2. 同一会话只能注册一个 `aether:session-prompt` section；重复注册由 DSH 抛错，适配器/插件不得静默吞掉后继续（当前捕获后写 stderr 诊断；E2E 断言注入生效）。
3. `agent.id` 必须与 ACP `sessionId` 同源映射；升级 DSH 后复跑 E2E，映射失效即回退到 `agent/session-start` 观测并另立 ADR。
4. 侧信道读取失败/为空 → 空 section（不阻断 run）；不得把缺失提示词替换为任何默认文本。
5. 版本 pin 精确匹配；不匹配 → `1003 VERSION_MISMATCH` + `disabled + status_reason=version_mismatch`（ADR-002 §3.1 口径）。
6. 合规（A11）：插件为净室自研，仅使用 DSH 公开插件契约/类型定义；不拷贝上游代码。
7. **生效位（v1.1）**：`has_system_prompt` 只表示「核心已下发」；侧信道写入成功/失败必须分别以 `system_prompt_effective=true/false` 向下反映。写失败/目录未初始化时不得恒回显为已下发成功（不得假装注入生效）。
8. **多 profile 清理作用域（v1.1 检查项）**：启动清理 `clearPromptDir` 当前按单实例 `<DSH_HOME>/aether-bridge/prompts` 设计；多 profile 并行实例评估落地时，清理作用域必须改为 per-profile/per-instance，避免互删在途会话侧信道文件（P2-2；见 §6-5 与 §10 P2-2 行）。

## 4. 实现要点（落地位置）

| 位置 | 内容 |
|---|---|
| `packages/adapter-dsh/src/dsh-plugin.ts` | `PROMPT_PLUGIN_PACKAGE_NAME` / `PROMPT_SECTION_NAME` / `PROMPT_SECTION_ORDER` / `PROMPT_DIR_ENV`；`PROMPT_PLUGIN_INDEX_JS`（含 `interpolate: false`）；`materializePromptPlugin`；`promptFilePath`（URL 编码）；`clearPromptDir`；`buildPatchText` 同时 insert 两个插件 |
| `packages/adapter-dsh/src/dsh-adapter.ts` | 初始化创建 `prompts/`、清理残留并传 `AETHER_DSH_PROMPT_DIR`；`session.create.system_prompt` 写侧信道；未下发时清理同 id 残留；dispose/shutdown 清理；`has_system_prompt` 回显；**v1.1 承接**：写成功置 `system_prompt_effective=true`，写失败/目录未初始化置 `false`，未下发为 `null`/缺省（见 §10-1） |
| `packages/adapter-dsh/src/dsh-plugin.test.ts` | 插件落盘/无 BOM/overlay 双 insert；运行时 fake ctx/agent（按会话读取、缺失空串）；`interpolate: false`；路径编码；启动清理残留 |
| `packages/adapter-dsh/src/dsh-adapter.test.ts` | 真实夹具进程：写入/未提供/dispose 删除；**v1.1 承接**：写成功→`true`、写失败/目录缺失→`false`、未下发→`null`/缺省断言 |
| `scripts/test/m7-09/dsh-prompt-e2e.mjs` | 真实 DSH 0.2.1-rc.2 E2E：适配器 → 插件 → agent 回合 → 本地 OpenAI 兼容上游；断言 system 消息含预设块与记忆块、顺序正确、未携带不注入、dispose 清理 |
| `docs/M7-09-证据.md` §4.6/§4.7 | 升级复测、注入实现与真实 E2E 证据 |

## 5. 影响

### 5.1 《设计文档》v1.16 → v1.17

- D5「DSH 适配器增强机制」：pin 更新为 `0.2.1-rc.2`；补系统提示词插件注入（section 作用域/时机/侧信道/清理）。
- D6「DSH 适配器增强机制」：补「系统提示词注入不改变线协议」；`session.create.system_prompt` 对 DSH 的消费路径。
- D14：补 DSH 消费说明（与 Claude/Codex 并列；合成语义不变）。
- §5 P1 段：ADR 落点追加 ADR-027。
- 修订记录追加 `v1.17`。
- **v1.1 增量（不升设计版本）**：`session.create` 响应新增可选字段 `system_prompt_effective`（`null`/`true`/`false`），属 D6「跨 `v` 版本允许新增字段（只增不改语义）」；不改方法表、事件类型、命令面、错误码、迁移。
- **N-3 对称性登记（评审 §8.2，2026-10-20）**：D14 三运行时消费行补「生效位按运行时分别承接」——本 ADR 仅登记 DSH 的 `system_prompt_effective`；Claude Code / Codex 如需等效字段，另立增量/ADR，不复用 DSH 字段语义。

### 5.2 实施计划

- `docs/P1-实施计划与验收标准.md` M7-09 执行状态：DSH 消费链路由「方案研究」改为「已实现（ADR-027）」。任务性质不变（直接实施项，不构成 P1 门禁）。

### 5.3 不影响的

- 命令面 63 / 错误码 32 / 设置键 8 / 迁移 `0004+` 规划位不变；
- D6 方法表、能力项、事件附录 B 不变；
- Claude Code / Codex 消费路径与 M3-08 记忆注入语义不变；
- D9 权限矩阵与 D5 进程监督（Job Object/整树回收）不变。

### 5.4 主要风险与缓解措施（v1.1，P2-1）

| 风险 | 级别 | 缓解措施 | 失效/回退 |
|---|---|---|---|
| 上游插件契约变更（`ctx.systemPrompt.section` / `agent/created` 移除或语义变化） | P1 | 精确版本 pin + 启动插件契约帧 + 真实 DSH E2E；升级同批复测 | §6-1/§6-4：停用插件 insert，显式标注 DSH 消费不可用 |
| 启动清理与恢复竞态（清空目录后、核心重写前发生组装） | P2 | 插件 `text` 惰性读取（每次 assemble 才读文件）；assemble 最早发生在首个 `session/prompt`，晚于 `session.create` 返回与文件写入；同 id 恢复显式清理 + 启动清空兜底 | §6-3：出现提示词丢失/串会话立即回退清理逻辑 |
| 侧信道写失败静默（磁盘异常/目录未初始化） | P1 | v1.1 `system_prompt_effective=false` 向下反映；`has_system_prompt` 仅表示已下发；写失败仍不阻断会话但不再「假装生效」 | §6-6：生效位失真时停用注入并修复 |
| 提示词副本明文留存（崩溃/异常退出） | P2 | 目录位于 `<DSH_HOME>/aether-bridge/prompts`，不写工作区、不进备份/日志/诊断包；会话 dispose/shutdown 删除；启动清理残留 | §6-7：发现备份/导出包含该目录 → 立即停止发布并修复 |
| 多 profile 并行实例互删侧信道文件 | P2（远期） | 当前单实例假设；多 profile 评估落地时清理作用域改 per-profile/per-instance | §3-8/§6-5：未改造前不得启用多 profile 并行实例 |

## 6. 回退条件

1. DSH 官方移除/不兼容 `ctx.systemPrompt.section` 或 `agent/created` → 停用 `aether-dsh-prompt`，DSH 的 `system_prompt` 消费降级为「不可见」并在 UI/诊断中显式标注（**不得假装注入成功**）；另立替代 ADR（如 ACP 扩展提案）。
2. `agent.id != sessionId` 无法恢复映射 → 停用按会话注入；不得退化为进程级全局提示词（会串会话）。
3. 注入导致 DSH 启动/组装失败或提示词串会话 → 立即回退插件 insert，保留 `session.create.system_prompt` 下发契约与 Claude/Codex 路径。
4. 升级 DSH 后 pin 复测失败 → 按 D5 `version_mismatch` 禁用运行时，不得放宽 pin。
5. **多 profile（v1.1 检查项）**：P1 计划 §8「D5 DSH 多 profile 评估」启动时，必须复核启动清理作用域；未改为 per-profile/per-instance 前，不得启用多 profile 并行实例（P2-2）。
6. **生效位失真（v1.1）**：若 `system_prompt_effective` 与实际注入结果不一致（写成功但未生效 / 写失败却回显 true）→ 视为实现缺陷，停用 DSH 提示词注入并修复；不得以恒 `true` 掩盖。
7. **备份/导出越界（v1.1）**：若 P1 自动备份、手动备份、会话导出或诊断包包含 `<DSH_HOME>/aether-bridge/prompts` → 立即停止发布并修复（产品确认该目录不进入备份范围）。

## 7. 验证计划与证据

| 层级 | 验证 |
|---|---|
| 插件单测 | `dsh-plugin.test.ts`：落盘/无 BOM/overlay 双 insert；fake ctx 下 `agent/created` 注册 agent 作用域 section、按 `agent.id` 读取、缺失返回空串；`interpolate: false`；路径编码；启动清理残留 `.md` |
| 适配器单测 | `dsh-adapter.test.ts`：`session.create.system_prompt` → 侧信道文件（按会话 id）；未提供不写；dispose 删除；**v1.1**：写成功→`system_prompt_effective=true`、写失败/目录未初始化→`false`、未下发→`null`/缺省，且 `has_system_prompt` 语义不变 |
| 真实 DSH E2E | `dsh-prompt-e2e.mjs`（opt-in）：`dsh 0.2.1-rc.2` + 真实适配器进程 + 本地记录型上游；断言 system 消息包含预设块/记忆块、预设块先于记忆块、位于 harness 身份之后；`{{aether_literal}}` 原样保留（`interpolate: false`）；未携带不注入；dispose 清理 |
| 回归 | M2-11 DSH 适配器测试、M7-05 三运行时一致性、M7-06 DSH 消费映射、`verify:m7-09`；**P1-2 收口**：在具备 Rust 工具链的环境/CI 复跑 `cargo test --workspace`、`cargo clippy`、`verify:m1-03`/`verify:m3-08` 涉 cargo 子项及 opt-in E2E 后，更新 `docs/M7-09-证据.md` §7.6 限制记录 |

实际输出见 `docs/M7-09-证据.md` §4.6/§4.7 与 §7。**2026-10-20 收口实测**：`cargo test --workspace` 877 passed / 0 failed / 11 ignored（exit 0）、`cargo clippy --workspace --all-targets -- -D warnings` exit 0、`verify:m7-09` 18/18、`adapter-dsh` 49/49、DSH 真实 E2E 5/5。

## 8. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-10-20 | 用户指令 | 批准实施 | 「继续 M7-09 未完成的任务，针对 dsh 进行插件注入的方式去注入系统提示词」；实现与 E2E 同批闭环 |
| 2026-10-20 | 独立评审（`docs/ADR-027-评审意见.md`） | **有条件认可**（P1×2、P2×3；无 P0） | 一致性核验全部通过；整改项：P1-1 生效位三态化、P1-2 全量回归复跑收口、P2-1 风险/缓解小节、P2-2 多 profile 清理作用域检查项、P2-3 评审留痕；本 ADR v1.1 承接 ADR 侧整改（见 §10） |
| 2026-10-20 | 独立评审第二轮（`docs/ADR-027-评审意见.md` §8） | 文档侧闭环，维持**有条件认可** | §8.3-1 代码承接：本 ADR v1.1 已落地 `system_prompt_effective` 三态（`dsh-adapter.ts` + 测试）；§8.3-2 全量回归已收口（877 passed / 0 failed / 11 ignored、clippy exit=0、`verify:m7-09` 18/18）；§8.3-3 N-3 已在 §5.1 登记。待评审方最终核销改判 |
| 2026-10-20 | 最终复核 | **通过** | §8.3-1/§8.3-2/§8.3-3 全部闭环；`docs/ADR-027-评审意见.md` §8.5 承接方回执；状态由「有条件认可」改判「通过」 |

## 9. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v1.0 | 2026-10-20 | 创建：DSH 系统提示词插件注入通道、作用域/时机/侧信道/清理、版本 pin 升级、验证计划与回退条件 | 文档维护 |
| v1.1 | 2026-10-20 | 按 `docs/ADR-027-评审意见.md` 修订：§2.4 新增 `system_prompt_effective` 三态语义；§2.3/§3-7 生效位硬约束；§2.6 备份不覆盖与多 profile 检查项（§3-8）；§4 实现承接点；§5.4 风险/缓解；§6-5/6-7 回退；§7 验证断言；§8 评审留痕；§10 整改承接表。代码承接同批完成：`dsh-adapter.ts` 三态赋值 + `dsh-adapter.test.ts` 断言；回归收口：`cargo test --workspace` 877 passed / 0 failed / 11 ignored、clippy exit=0、`verify:m7-09` 18/18 | 文档维护 |

## 10. 评审整改承接（v1.1）

| 评审项 | 整改动作 | 本文档状态 | 代码/证据承接 |
|---|---|---|---|
| P1-1 生效位语义 | §2.4 定义 `has_system_prompt`（已下发）与 `system_prompt_effective`（null/true/false）；§3-7 硬约束；§4/§7 补承接点与断言 | **已闭环（ADR + 代码，2026-10-20）** | `dsh-adapter.ts` 写失败/目录缺失置 `false`、成功置 `true`、未下发 `null`；`dsh-adapter.test.ts` 补三态断言；核心侧仅验通过、不强制消费（D6 只增字段） |
| P1-2 全量回归收口 | §7 回归行补收口条件 | **已闭环（2026-10-20）** | `cargo test --workspace`（877 passed / 0 failed / 11 ignored）、`cargo clippy --workspace --all-targets -- -D warnings`（exit=0）、`verify:m7-09` 18/18（含 `verify:m1-03`、`verify:m3-08`、opt-in DSH E2E）；证据 §7.6 |
| P2-1 风险/缓解小节 | §5.4 新增风险/缓解表 | **ADR 侧已闭环** | — |
| P2-2 多 profile 清理作用域 | §3-8 检查项、§6-5 回退条件 | **ADR 侧已闭环** | 多 profile 评估（P1 计划 §8）时复核；未改造前不得启用并行实例 |
| P2-3 评审留痕 | §8 追加独立评审行 | **已闭环** | `docs/ADR-027-评审意见.md` |
| N-3 三运行时生效位对称性 | §5.1 补 D14「生效位按运行时分别承接」登记；本 ADR 仅 DSH 字段 | **已登记** | Claude/Codex 等效字段如需扩展，另立增量/ADR |

> 复核口径：P1-1（ADR 定义 + 代码三态）、P1-2（全量回归）、P2-1/P2-2/P2-3 与 N-3 登记均已闭环；请评审方按 `docs/ADR-027-评审意见.md` §8.3 做最终核销（预期改判「通过」）。
