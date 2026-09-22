# ADR-008：P0 适配器范围扩展为三运行时（Claude Code / Codex / DeepSeek Harness）并激活 M2-11

| 项 | 内容 |
|---|---|
| 编号 | ADR-008 |
| 状态 | **已评审通过并归档（2026-09-22，架构评审 + 产品负责人 6 项裁决同步落盘，见 §7）**；随设计文档 v1.8 / 实施计划 v1.14 / 需求文档 v0.6 同批升版回流 |
| 触发 | 用户指令：「执行是需要同时适配 claude、codex、DSH，均需要验证测试」 |
| 关联决策 | A1、RA-02、D5、D6、D9、ADR-002、ADR-003、ADR-005 |
| 影响文档 | 《设计文档》§2.2 适配器路线图、§5 P0、D5 版本门闩表述、附录 E（native_id）（引用不动，范围条目增补）；《实施计划与验收标准》§1 汇总/§1.2/§1.4 Gate 2、§3 M2-11 条件与 DoD、§7 映射；《需求文档》§1.2/§3.2 RA-02/§3.8/§8 阶段表 |
| 净室合规 | 不修改/不拷贝上游项目源码（AGENTS §2.10、A11）；仅使用 DSH 包内公开契约与运行时行为观察 |
| 范围纪律 | 本 ADR 为 P0 范围扩展（设计文档 §4「不做清单」#8「第二/后续 Runtime 顺延 P1」由产品指令覆盖）：不砍既有任务，M2-11 由条件任务转为实际交付；扩展依据为产品负责人指令（2026-09-21）+ 评审裁决（2026-09-22），非自下而上蔓延 |

---

## 1. 背景

1. Gate 1 结论（`docs/spike/M1-11-接入笔记.md`）：A1 按「两候选先通过者进入 MVP」选定 Claude Code（Mode R），Codex 与 DSH 两种 transport 均只到部分通过；实施计划 v1.13 据此把 M2-11（DSH 增强）设为条件任务并在本仓库 M2 编排中不排期。
2. 产品负责人于 2026-09-21 明确变更范围：**P0 期间需同时适配 Claude Code、Codex、DeepSeek Harness 三个运行时，且每一运行时必须有可执行验证测试**。
3. M1-11 spike 已分别验证三个运行时的可编程接入面与恢复能力（Claude/Codex/DSH-ACP 均 Mode R；DSH headless Mode N 不满足会话模型映射，不采用）。
4. ADR-002 已冻结 DSH 增强方案（`--patch` 注入自研 Cordis 插件 + 带外 delta 通道 + 前缀去重 + 权威终态取 ACP `session/prompt` settle + pin 版本门闩），M2-11 DoD1–8 可直接作为 DSH 侧的验收口径。

## 2. 决策

1. **P0 适配器范围扩展为三运行时并行**：`claude-code`（已有，M2-02）、`codex`、`deepseek-harness` 均为官方内置运行时（`OFFICIAL_RUNTIME_IDS` 已含三者，见 `crates/aether-adapters/src/supervisor/admission.rs`）。
2. **M2-11 条件视为成立并执行**（条件由产品指令变更，不再以「A1=DSH」为唯一触发）：DSH 侧按 M2-11 DoD1–8 实施；Codex 侧按 **M2-02 等价验收口径**（一致性含异常路径、工具事件、Mode R、崩溃恢复、完成率代理）。
3. **每一运行时必须交付适配器包 + 验证测试**：TS 单测（vitest）+ Rust 集成测试（真实适配器进程 + 确定性夹具，经 `AdapterSessionClient`）+ 任务验证脚本（`scripts/test/m2-11/verify-m2-11.mjs`）+ 三平台 CI 矩阵接线。
4. **不新增线协议方法、不新增事件类型**：三适配器全部使用 D6 方法表与附录 B 事件；Codex 正文整段到达以 `message.delta`（整段 chunk）呈现，DSH token 级增量走带外通道 + 前缀去重。
5. **Codex `native_id` 采用适配器持久化别名**：Codex CLI 的 `thread_id` 由服务端在首个 run 生成、不可预生成；适配器在 `session.create` 返回稳定别名并在状态目录持久化 `别名 → thread_id` 映射，`session.create(native_id=别名)` 映射为 `codex exec resume <thread_id>`，对外保持 ADR-005 的 Mode R 语义（详见 §3.3）。
6. **DSH 版本门闩的失效信号**：复用协议应用码 `1003 VERSION_MISMATCH`——适配器 `initialize` 在 DSH 版本/插件契约探针失败时返回 1003 + 升级提示；监督器把 `initialize` 错误码 1003 映射为 `disabled + status_reason=version_mismatch`（增量，见 §3.4）。
7. **Gate 2 纳入三运行时一致性门禁**（评审裁决 1，2026-09-22）：M2-11（含 Codex 路径）从「不纳入 Gate 2」改为纳入；Gate 2 通过条件新增「三运行时（Claude Code / Codex / DSH）一致性验收全部通过」。ADR-002 §4「M2-11 不纳入 Gate 2」的冻结结论由本 ADR 显式覆盖。
8. **运行时集合冻结为三**（评审裁决 5，2026-09-22）：P0/P1 官方运行时集合 = `claude-code`、`codex`、`deepseek-harness`，**不新增**（Pi Agent、Hermes 维持顺延/备选；`OFFICIAL_RUNTIME_IDS` 中的 `pi` id 仅为准入白名单占位，不构成交付承诺）。新增运行时须另立 ADR。
9. **Codex 真实完成率硬指标**（评审裁决 3，2026-09-22）：真实运行时完成率验证必须达到 **≥20 次真实 run**（此前证据为 3 + 5 次），≥95% 完成率、每个 run 均有终态；M2-11 验收不视为完成直至该项补验归档。
10. **DSH 真实权限回环补验时点**（评审裁决 4，2026-09-22）：真实工具 ask 场景的权限回环补验在 **Gate 2 前**完成并归档证据；此前 M2-11 验收结论引用须附带该未验证项声明。

## 3. 实现要点

### 3.1 包与产物

| 运行时 | 适配器包 | 运行时 id | 夹具（确定性，无网络） |
|---|---|---|---|
| Claude Code | `packages/adapter-claude-code`（已有） | `claude-code` | `scripts/test/m2-02/fake-claude/cli.mjs` |
| Codex | `packages/adapter-codex`（新增） | `codex` | `scripts/test/m2-11/fake-codex/cli.mjs` |
| DeepSeek Harness | `packages/adapter-dsh`（新增） | `deepseek-harness` | `scripts/test/m2-11/fake-dsh/acp-server.mjs` |

三适配器均由 Bun `--compile` 产出单文件，经 `AETHER_{MOCK,CLAUDE,CODEX,DSH}_ADAPTER` 注入 Rust 集成测试；CI 三平台矩阵真实执行。

### 3.2 Codex 接入面（依据 M1-11 接入笔记与 `codex exec --help` 实测）

- `session.create` → 适配器生成/接受别名；首个 send 启动 `codex exec --json --skip-git-repo-check -s <sandbox> -m <model> -c model_reasoning_effort=<r> -C <workspace> -`，prompt 走 stdin；
- 续聊/恢复 → `codex exec resume --json ... <thread_id> -`（spike 已知坑 7：resume 不接受 `--sandbox`，改 `-c sandbox_mode=<mode>`）；
- 事件映射：`thread.started.thread_id` → 绑定原生会话；`item.completed(agent_message.text)` → `message.delta`（整段）；`item.started|completed(command_execution|file_change|mcp_tool_call)` → `tool.call_started/completed|failed`；`turn.completed(usage)` → `message.completed` + `run.completed`；`turn.failed` / 无终态退出 → `run.failed`；
- 异常：`error` 只记诊断（见 spike 已知坑 5：不得见 error 即判失败，终态仲裁以 `turn.completed|failed` 与进程退出为准，并设挂起超时）；
- 隔离：必须使用隔离 `CODEX_HOME`（spike 已知坑 9），由适配器 CLI `--codex-home` 注入。

### 3.3 Codex Mode R 别名语义

- 首轮 `session.create`：`native_id = <ULID 别名>`，`resumed=false`；首个 run 收到 `thread.started` 后把 `别名 → thread_id` 原子写入 `<codex-home>/aether-bridge/sessions.json`；
- 恢复：`session.create(native_id=别名)` → 查表得 `thread_id` → `resumed=true`，后续 run 走 `exec resume`；
- 别名方案使核心无需新增「native_id 回写」通道（D6 方法表冻结，附录 B 无承载字段）；若未来引入 `session.updated` 承载字段，可平滑换成原生 id（不影响本 ADR 其余决策）。

### 3.4 DSH 版本门闩与监督器映射

- 适配器 `initialize` 时执行探针：① DSH 包版本必须等于 pin（默认 `0.1.5-rc.2`，`--dsh-version-pin` 可覆盖）；② 插件契约帧 `{type:"hello",contract:"aether-dsh-stream@1"}` 出现在带外通道；任一失败 → JSON-RPC `1003` + 升级提示；
- 监督器 `start()` 的 `initialize` 失败分支增加错误码判别：`RequestError::Rpc { code: 1003 }` → `DisabledReason::VersionMismatch`（其余仍为 `start_failed`）；不改变 hello 通道的既有协议 major 校验。

### 3.4b DSH provider 路由的官方分层配置（v0.2 评审补充）

- **问题**：`llm-pi-ai` 的 provider 路由仅由 `settings.yaml` 声明时，注册发生在 async settings 注入之后；而 `dsh-acp-app` 不等 settings 就绪即开始服务，冷启动窗口内 `session/new` 会返回 `-32603 no adapter registered for provider`（2026-09-22 实测 1/5，上游 0.1.6-alpha.2 亦未改此路径）。
- **官方口径**：`dsh-settings` 的 `installSection(owner, ns, schema, entry, hooks)` 语义为 **插件行 `config` = composition base 层，`settings.yaml` = user 覆盖层**（`register(base)` + `setSource(scope.get())`）。因此 provider 定义放进 overlay 的 `llm-pi-ai` 行 `config.providers`，即与该插件 `ensureRegistrationFacts()` 在 apply 时**同步注册**（与官方 `deepseek-official` 同路径），竞态从构造上消失；settings 仍是用户覆盖层（web UI 改模型照常生效）。
- **实现**：适配器新增 `--dsh-provider-config <inline-json|file>`（核心从同一份 provider 物化结果传入；容忍 UTF-8 BOM），生成的 overlay 追加 `- id: llm-pi-ai / config: <JSON>`；`session/new|resume` 另保留对 `no adapter registered` 的 ≤5s 有界重试作为防御纵深。
- **边界**：overlay 按 DSH 补丁语义**整体替换目标行 config**（当前 `llm-pi-ai` 基础行无 config，无字段丢失；若上游未来给该行加 config，需同步合并）。

### 3.5 DSH 带外 delta 通道（ADR-002 §3.6 落地）

- 注入：适配器生成 overlay patch（覆写 `acp` 行 provider/model + `insert` 插件行）并把插件包写入 `<DSH_HOME>/profiles/<profile>/node_modules/aether-dsh-stream/`（`package.json` 无 BOM，Node 侧字节生成）；共享 `profiles/node_modules` 放置一律拒绝（spike 已知坑 19）；
- 帧格式：插件订阅 `agent/assistant-stream`，向 `AETHER_DSH_DELTA_FILE`（适配器注入 env）追加 JSONL：
  `{v:1, type:"hello"|"start"|"chunk"|"end", attemptId, sessionId?, messageId?, chunkType:"text-delta"|..., text?, at}`；
- 去重：ACP committed `agent_message_chunk` 到达时，与已流式前缀比较——committed 以流式文本为前缀 → 仅发后缀 delta；否则丢弃流式、以 committed 全量重建（DoD5 兜底）；
- 终态：`session/prompt` settle → `message.completed`（取 committed 全文）+ `run.completed`；`stopReason=cancelled` → `run.cancelled`；
- 通道清理：`session.dispose` 关闭会话后删除/关闭通道并断言残留通道数 0（探针）。

### 3.6 DSH 权限回环（D9 边界 + M2-11 DoD4）

- `session/request_permission`（server→client）→ 适配器发 `permission.request` 通知（100%），**不直通**；
- 核心（当前测试侧；M2-10 落地后为权限网关）以 `permission.resolve` 应答；适配器按决议映射 ACP outcome：allow→`selected(optionId)`、deny/超时→`cancelled`；
- 适配器进程内不执行工具、不落权限审计（信任级边界与 M2-02 一致，见设计 §2.1 第 4 条）。

## 4. 影响

1. **正向**：P0 具备三运行时接入面；M2-11 从条件任务变为实际交付；Codex/DSH 的 Mode R 与异常路径被测试固化。
2. **范围**：本 ADR 不改变 D5/D6/D9 任何冻结语义；新增代码集中在 `packages/adapter-codex`、`packages/adapter-dsh`、测试与脚本；监督器仅新增 1003 → `version_mismatch` 映射分支（§3.4）。
3. **监督器**：新增一个错误码映射分支（§3.4），有单测覆盖；其余不变。
4. **文档（评审后同步完成）**：设计文档升 **v1.8**（§2.2 路线图、§5 P0 能力、D5 版本门闩表述、附录 E `native_id` 口径）；实施计划升 **v1.14**（M2-11 条件转正、Gate 2 纳入三运行时门禁、§1 计数 35→36、§7 映射）；需求文档升 **v0.6**（P0 阶段表 = 三运行时、RA-02、P1 阶段目标、§3.8 矩阵）。
5. **运维**：Codex 需稳定端点（spike 已知坑 4/5）与隔离 `CODEX_HOME`；DSH 需 `0.1.5-rc.2` 隔离安装与 provider 配置（密钥经环境/Keychain，不入仓库）。
6. **验收遗留（Gate 2 前必须关闭）——已全部闭环（2026-09-22 复核，见 §6）**：① Codex 真实 ≥20 次完成率补验（裁决 3）→ **20/20（100%）**；② DSH 真实权限回环补验（裁决 4）→ **allow/deny 双场景零直通**。唯一遗留：三平台 CI 矩阵结论（`638d2ff` run 已触发，待出）。

## 5. 风险与失效条件

| 风险 | 应对 | 失效条件 |
|---|---|---|
| Codex 端点不稳定导致完成率不达标 | 夹具验证协议面；真实完成率为 opt-in（`AETHER_REQUIRE_REAL_CODEX=1`）；成功标准按「协议面 + 夹具 + 可选真实 20 次」分层 | 真实端点连续两轮 <95% → 运行时可标 `degraded`，不得阻塞其他两运行时 |
| Codex 正文整段无 token delta | 以整段 `message.delta` 呈现，UI 不假设 token 粒度 | 若要求 token 级，转 P1（本 ADR 不承诺） |
| DSH 内部事件契约（`agent/assistant-stream`）变更 | pin 版本 + 探针 + 带外帧形状校验；失配即 `version_mismatch` | 官方废弃该事件且无替代 → 按 ADR-002 §5 降级为 committed 级流式 |
| 别名映射文件损坏/丢失 | 原子写 + 损坏即按新建会话处理并审计日志 | 连续恢复失败 → Mode N 降级并在 UI 明示（与 ADR-005 口径一致） |
| 插件注入被 DSH 加载器拒绝 | 插件包独立域 + 无 BOM 字节断言 + 启动探针 | 加载失败 → `disabled + start_failed`，不得静默降级为 headless |
| §3.4b overlay 整体替换目标行 config，上游演进可能造成字段丢失 | 升级 DSH 时核查 `llm-pi-ai` 行基础 config；合并逻辑随上游变更同步 | DSH 升版后该行出现非空 config 而适配器仍整体替换 → 字段丢失，触发合并适配 |

## 6. 证据与验收

- DSH：`docs/M2-11-证据.md`（DoD1–8 逐条原始输出）；
- Codex：同证据文档「Codex 一致性验收」章节（M2-02 等价口径）；
- 验证入口：`pnpm verify:m2-11`（构建三适配器 + TS 单测 + Rust 集成 + 静态合规 + opt-in 真实运行）；
- 真实运行 opt-in 触发方式：Claude 为 `AETHER_REQUIRE_REAL_CLAUDE=1` 或凭证（`AETHER_CLAUDE_{BASE_URL,TOKEN}`）；Codex 为 `AETHER_REQUIRE_REAL_CODEX=1` 强制 / 凭证 `AETHER_REAL_CODEX_HOME` 触发；DSH 为 `AETHER_REQUIRE_REAL_DSH=1` 强制 / 凭证 `AETHER_REAL_DSH_{BIN,HOME}` 触发；缺凭证显式 SKIP（禁止静默跳过）；
- **Gate 2 前必须补验归档**（评审裁决 3/4）——**已闭环（2026-09-22，证据经评审 Agent 读原始 summary.json 复核）**：
  1. ✅ Codex 真实完成率 ≥20 次：`--runs 20` → **20/20（100%，≥95%）、逐 run `run.completed`、零挂起**（`docs/M2-11-证据.md` §5.1；summary `scripts/test/.tmp/m2-11/real-codex-2026-09-22T08-20-09-685Z/summary.json`：`regression{runs:20, completed:20, hang:0, completionRate:1}`，gitignored）；
  2. ✅ DSH 真实工具 ask 权限回环：DeepSeek 官方 API + `DSH_PERMISSION_MODE=read-only` 触发真实 `session/request_permission`，**allow/deny 双场景零直通**（ask=1、`tool.call_started` 映射 1/1、收口晚于决议 1.2s）（`docs/M2-11-证据.md` §5.2b；summary `real-dsh-2026-09-22T08-41-58-034Z`（allow）/ `real-dsh-2026-09-22T08-38-28-653Z`（deny），gitignored）；
  3. ✅ 三平台 CI 矩阵（2026-09-22 结论已出，经 GitHub API 独立核验）：`adapters-platform-tests` 在 **3 次 run（`638d2ff` / `c96f6aa` / `b4961a4`）中均 3/3 success**，矩阵内真实构建三适配器并执行 M2-11 集成 + M2-02/M1-09 回归——决策 3「三平台 CI 矩阵真实执行」达成；
  4. ✅ 非本任务遗留（M2-11 范围外）**已由责任方加固闭环（2026-09-22）**：同一批 run 中 **M1-08 `Tauri security baseline` 3 次均失败**（根因：测试夹具按「全局第 N 次调用」选择 panic 目标，跨会话 `JoinSet` 调度顺序不确定 → panic 落到其他会话；叠加收割循环以「任意任务被收割」为界的次生竞态）与 **M1-06 `Data dir guard` 多次失败**（根因：E2E 逐 chunk `split` 导致长 PHASE JSON 被管道分片切段、谓词永不命中）——均非产品缺陷，修复与复跑证据见 `docs/M2-07-证据.md` §6、`docs/M1-06-目录选择器与迁移主路径验证.md` §6（M1-08 CSP / M2-07 健康 E2E 同缺陷一并修复）；Gate 2 以加固后的下一次 CI run 结果为准。

## 7. 评审记录

| 版本 | 日期 | 说明 | 评审人 |
|---|---|---|---|
| v0.1 | 2026-09-21 | 草案：依据产品指令提出三运行时范围扩展与 M2-11 激活；实现随任务先行，待评审归档 | （待填） |
| v0.2 | 2026-09-22 | 评审补充：新增 §3.4b（DSH provider 路由走官方 composition base 分层 + 有界重试兜底）；真实运行时证据更新 | （待填） |
| v0.3 | 2026-09-22 | **架构评审（AI 评审 Agent，按《评审要求》）**：结论「有条件通过」，提出 P0×2 / P1×5 / P2×4 共 11 项问题；产品负责人同日裁决 6 项并落盘本版——① Gate 2 纳入三运行时一致性门禁（决策 7）；② 需求文档升版与设计 v1.8 / 计划 v1.14 同批评审（决策 + §4.4）；③ Codex 真实完成率必须达 20 次（决策 9）；④ DSH 真实权限回环 Gate 2 前补验（决策 10）；⑤ 运行时集合冻结为三、不新增（决策 8）；⑥ 按裁决更新全部文档（§4.4）。P1/P2 评审问题随本版修订闭环：影响文档补列（表头）、附录 E 口径（§4.4）、opt-in 变量口径（§6）、§3.4b 风险入表（§5）、范围纪律留痕（表头）。**评审通过，归档** | 产品负责人 + 架构评审 Agent |
| v0.4 | 2026-09-22 | **裁决 9/10 补验闭环复核**（AI 评审 Agent）：① Codex 真实 20/20（summary `real-codex-2026-09-22T08-20-09-685Z`，completionRate=1、perRun 20/20）；② DSH 真实 ask 回环 allow/deny 双场景零直通（summary `real-dsh-2026-09-22T08-41-58-034Z` / `08-38-28-653Z`）——均读取 gitignored 原始 summary.json 核实，非仅转述文档；M2-11 证据头同步闭环标记；三平台 CI 矩阵结论经 GitHub API 独立核验（3 次 run adapters 3/3 全绿，决策 3 达成）；⚠️ 登记非本任务遗留：M1-08（m2_07_panic flake ×3）/ M1-06（WebView E2E flake ×2）需责任方在 Gate 2 汇总评审前 rerun 留痕或加固 | 架构评审 Agent |
| v0.5 | 2026-09-22 | 责任方遗留项加固闭环（M2-07 / M1-06）：§4.6-4 由 ⚠️ 遗留转为 ✅ 已加固——`m2_07_panic` 改为按输入文本选择 panic 目标 + 持续收割至终态（普通 250/250、llvm-cov 插桩 8/8）；M1-06/M1-08/M2-07 E2E 引入 `lineFramer` 跨 chunk 行框定并加固探针回填等待；证据见 `docs/M2-07-证据.md` §6、`docs/M1-06-目录选择器与迁移主路径验证.md` §6。**加固后 CI 复跑（`24a9160`）进一步暴露同 job 内 M2-06 读锁退避用例的固定 `sleep(120ms)` 时序 flake（`m2_06_shutdown.rs:174`），已改为两阶段确定性驱动（`docs/M2-06-证据.md` §6）；再复跑（`439424c`）暴露 M2-09 步骤的 CI 接线缺口——`security-baseline` job 缺 Bun（`windows-2022` 镜像不含），已补 `oven-sh/setup-bun@v2` 1.4.2 并强化失败诊断；随后 Coverage gate 暴露 `m2_09_memory` 解析等待预算不足（插桩下 14.3–15.3s 贴线 15s 上限），预算放宽至 60s 并为 Coverage job 补失败诊断（`docs/M2-09-证据.md` §6）。**最终复跑 `873e74d`（run 35734860794）12/12 job success，M1-08/M1-06 两项遗留闭环** | M2-07 / M1-06 / M2-06 / M2-09 责任方 |

## 8. 变更记录

| 版本 | 日期 | 变更 |
|---|---|---|
| v0.1 | 2026-09-21 | 初稿 |
| v0.2 | 2026-09-22 | 评审补充：新增 §3.4b（DSH provider 路由走官方 composition base 分层 + 有界重试兜底）；真实运行时证据更新 |
| v0.3 | 2026-09-22 | 评审归档版：新增决策 7（Gate 2 纳入三运行时门禁，覆盖 ADR-002 §4 冻结结论）、决策 8（运行时集合冻结为三）、决策 9（Codex 真实 ≥20 次硬指标）、决策 10（DSH 真实权限回环 Gate 2 前补验）；影响文档补列《需求文档》与附录 E；范围纪律留痕；§5 补 §3.4b 上游演进风险；§6 补 opt-in 变量口径与 Gate 2 前补验清单；§4.4 文档升版口径更新（v0.6 / v1.8 / v1.14） |
| v0.4 | 2026-09-22 | 裁决 9/10 补验闭环：§4.6 与 §6 标记两项补验已完成并附证据路径（Codex 20/20；DSH ask 回环 allow/deny 零直通）；§7 补复核记录；三平台 CI 矩阵结论闭环（3 次 run adapters 3/3 全绿）；登记 M1-08/M1-06 flaky 为 Gate 2 前责任方遗留 |
| v0.5 | 2026-09-22 | 责任方加固闭环：§4.6-4 遗留项（M1-08 `m2_07_panic` flake / M1-06 WebView E2E flake）完成根因定位与修复并附复跑证据；§7 补记录 |
