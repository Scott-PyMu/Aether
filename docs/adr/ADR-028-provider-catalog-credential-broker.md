# ADR-028：多供应商目录、协议端点与按需凭据代理（D6/D7/D10；M7-10）

| 项 | 内容 |
|---|---|
| 编号 | ADR-028 |
| 版本 | **v0.9（v0.8 复审编辑性 P2 清理稿 + 补选上下文文案）**：按 `docs/ADR-028-复审意见-v0.8.md` 的 3 个编辑性 P2 修订，并补选 `session_busy` 的 UI 上下文文案；v0.8 为 v0.7 复审 P2 清理稿；v0.7 为上一轮复审整改稿；v0.6 为 v0.5 复审整改稿；v0.5 为 v0.4 复审整改稿；v0.4 为用户 10 项裁定整改稿；v0.2 为首版实现稿（仅 Claude Code 接入） |
| 状态 | **已通过（2026-10-23，v0.9；文档侧）**；补选 `session_busy` 上下文文案已补；代码/E2E/证据与共享 token 正式签署完成前不得进入实现验收 |
| 决策日期 | 2026-10-23 |
| 决策载体 | 《设计文档》v1.23 → **v1.24（已同步）**；《需求文档》v0.17 → **v0.18（已同步）**；`docs/P1-实施计划与验收标准.md` v0.12 → **v0.13（已同步）**；`docs/UI-UX-设计规格.md` v0.13 → **v0.14（已同步）**；`AGENTS.md` 基线已同步；迁移 `0009_provider_endpoints.sql`（不变，不新增迁移）；复审依据 `docs/ADR-028-复审意见-v0.8.md`、`docs/ADR-028-复审意见-v0.9.md`（2026-10-23） |
| 关联 | D6/D7/D10、ADR-006（错误码）、ADR-008（三运行时）、ADR-010（供应商/密钥）、ADR-014（命令面）、ADR-015（运行时注册）、ADR-018（设置键）、ADR-022（自动路由）、ADR-023（完整审计；本 ADR 不落库）、ADR-024（供应商消费映射）、UI-09、M7-06/M7-09/M7-10 |
| 取代 / 被取代 | 扩展 ADR-024 的供应商选择与注入规则；不改 ADR-024 的密钥只经 env、不落盘原则 |
| 回退条件 | 见 §6 |
| 实现状态 | v0.2 首版实现已落地（端点层、`session_create.provider_id`、Credential Broker、Claude Code `multi_provider` + per-run env；当时仅 Claude Code 接入）；**v0.9 代码/测试/证据未闭合**：21 行真相表/补选并发、provider 空 model_state（含 model 停用样本）、UI 优先级、状态复制约束、准备成功判定、旧会话补选适配器语义、目录查询、静态能力 UI 通路、只读态中断、OpenAI `/v1` 归一化、三运行时 per-session、全局记忆、三适配器 CLI token 剥离、应用级 E2E 与 `docs/M7-10-证据.md` 均待整改；承接见 §10 |

## 1. 背景

- 用户要求：**同一个 Agent（如 Claude Code）可以同时搭配多个供应商**（Anthropic / DeepSeek / 其他 Anthropic 兼容网关），而不是切换供应商必须重启运行时。
- 现有实现（ADR-024）：`ProviderEnvHook` 按运行时类型选一个 provider，在适配器 **spawn 时**把该 provider 的密钥注入适配器进程 env。一个适配器进程只有一套供应商环境，无法同时服务多个 provider。
- 现有 provider 只有 `type + base_url`；主流供应商可能同时提供 OpenAI 格式与 Anthropic 格式端点（例如 DeepSeek：`https://api.deepseek.com` 与 `https://api.deepseek.com/anthropic`）。Claude Code 必须使用 Anthropic 端点，Codex 使用 OpenAI 端点。
- 若把全部 provider 密钥一次性注入每个适配器，会扩大泄露面（适配器崩溃/内存 dump/子进程 env 继承均可暴露全部 key）。需要把“目录元数据”和“密钥”分离。
- v0.3 复审（`docs/ADR-028-评审意见.md`，2026-10-22）结论为**附条件通过**，用户裁定 10 项；核心变化：
  - 多供应商是 **P1 正式需求**，**三个官方运行时（Claude Code / Codex / DSH）均须支持**，不再保留“仅 Claude 接入、其余后续”的口径；
  - `multi_provider` 改为**静态能力声明**，不依赖 `hello`/runtime ready；
  - 新增**非机密目录查询接口**（`provider.catalog.get`），命令面仍为 63；
  - 供应商选择改为**全局设置记忆 + 预填视为已选**，不再自动选默认；
  - 供应商停用/删除后**已有会话只读**（历史可看，send/retry/resume 拒绝）；
  - 端点完全由用户输入，**不做 HTTPS/主机白名单/可信保证**，风险书面接受；
  - 密钥扫描排除测试代码并对合成/假 key 开白名单；
  - 新增 5 个 UI 锚点并登记 UI-UX v0.9。
- v0.4 复审（`docs/ADR-028-复审意见.md`，2026-10-23）结论为**附条件通过（维持）**；文档侧 10 项用户裁定已落文，但仍有 **P0-1**（旧会话补选无路径）+ **P1×8**（目录查询协议语义、静态能力 UI 通路、D6 判定矛盾、§5 退出标准、附录 A UI-09、`model` 省略、只读态中断、OpenAI `/v1` 设计条款）。本版 v0.5 闭合文档侧；代码/E2E/证据仍待承接。
- v0.5 复审（`docs/ADR-028-复审意见-v0.5.md`，2026-10-23）结论为**附条件通过（维持）**；上一轮 1×P0 + 8×P1 文档侧已闭合，但新增 **P1-1**（旧会话补选适配器语义：Codex/DSH 配置切换、原生上下文保留、失败回滚未定义）+ **P1-2**（v0.2 “provider_id 有、model 空”会话无补选路径）+ P2×6。本版 v0.6 闭合文档侧；代码/E2E/证据仍待承接。
- v0.6 复审（`docs/ADR-028-复审意见-v0.6.md`，2026-10-23）结论为**附条件通过（维持）**；上一轮 2×P1 已闭合、P2 大部分闭合，但仍有 **P1-1**（真相表不完整/ADR 与设计不一致）+ P2×5（model_state 不属于 provider、Codex/DSH 状态复制约束、`verify:m7-10`、量级、补选后 run 失败状态）。本版 v0.7 闭合文档侧；代码/E2E/证据仍待承接。
- v0.7 复审（`docs/ADR-028-复审意见-v0.7.md`，2026-10-23）结论为**附条件通过（维持）**；文档侧 **无 P0/P1**，上一轮 P1-1/P2×5 已闭合；剩余 P2×6（版本标签、provider 空 model_state、UI 优先级、补选并发、`verify:m7-10`/M8-03/L、共享 token 签署）。本版 v0.8 清理文档侧 P2；代码/E2E/证据仍待承接。
- v0.8 复审（`docs/ADR-028-复审意见-v0.8.md`，2026-10-23）结论为**附条件通过（维持）**；文档侧无阻塞性 P0/P1/P2，上一轮 P2×6 已清理；剩余 3 个编辑性 P2（provider 空 `model_state` 补“且启用”、UI-UX 锚点表日期、§10 历史行注记）与实现/验收条件。本版 v0.9 清理编辑性 P2；代码/E2E/证据仍待承接。
- v0.9 复审（`docs/ADR-028-复审意见-v0.9.md`，2026-10-23）结论为**附条件通过（维持）**；文档侧闭合，仅 1 条可选 UX P2（补选 `session_busy` 使用上下文文案）。本版 v0.9 补该文案并由用户确认文档侧通过；代码/E2E/证据仍待承接。
- 本 ADR v0.9 完成文档侧编辑性修订，并经用户确认补选 `session_busy` 上下文文案后标记通过；代码、测试与证据按 §7/§10 承接，未闭合前不得声称实现验收通过。

## 2. 决策

### 2.1 核心决策

1. **ProviderCatalog（非机密单例，三官方运行时共用）**
   - 核心维护唯一的供应商目录：provider id/name/type/enabled、协议端点（`providers.endpoints`）、模型清单；**认证方式为派生字段（`protocol → auth_env`），不在目录响应中返回**；**不含密钥值，也不向适配器返回 `api_key_ref`/token**。
   - 三个官方运行时共用同一份目录；目录查询走 D6 只读方法 `provider.catalog.get`（见 §2.1-6），不再依赖 spawn 时快照。
   - 目录变更（新增 provider/端点/模型）后，下一次查询即生效；密钥永远按需获取。

2. **Credential Broker（按需密钥单例，三官方运行时）**
   - 核心在本地启动凭据 broker（Windows named pipe / Unix domain socket；仅本机、仅当前用户）；Windows 使用当前用户 DACL、Unix 使用 `0700` 目录 + socket 权限，拒绝其他用户连接。
   - 适配器 spawn 时获得 broker 地址 + **应用级 token**（`AETHER_CREDENTIAL_BROKER` = `{endpoint, token}`，仅含地址/token，不含供应商密钥）。**应用级 token 每次应用启动随机生成、同一启动内连接复用**；token 不落盘、不进日志/诊断包/导出/测试夹具。
   - 适配器在**每次 run 启动时**按 `provider_id + protocol` 请求凭据；**broker 与适配器均不缓存密钥**。broker 校验 token、`runtime_id`（必填）、官方 runtime label 与 **provider → protocol 兼容性**后，从 OS 凭据库或 A3 加密文件读取**该一家** provider 的 key，返回 `{base_url, api_key, auth_env}`；不兼容 → 拒绝，不返回 key。
   - **runtime → protocol 白名单/兼容表**：`claude-code` → `anthropic`；`codex` → `openai`；`deepseek-harness` → `dsh`；表为补偿措施，不改变“应用级共享 token”的信任模型。
   - `session_create` 在创建会话前做一次“可解析性预检”：核心经安全层读取后立即丢弃，不把 key 写入会话状态；预检覆盖 broker 可用、`api_key_ref` 存在、secret store 已挂载、key 可读、provider → protocol 兼容。
   - 适配器只把该 key 放入本次 CLI/工具子进程 env，启动后即丢弃；不缓存全部 key。**所有官方适配器的 CLI/工具子进程统一剥离 `AETHER_CREDENTIAL_BROKER`**（共享实现 `stripBrokerEnv`，见 §4）。
   - broker 仅写**运行日志（tracing）**：`provider_id/runtime_id/protocol/结果`，不记录密钥；**不写数据库、不新增事件类型、进程重启后不可查、不作为合规审计**。若后续要求持久审计，另立 ADR。
   - broker 启动失败或未接线：**multi_provider 会话创建直接失败**（`core_not_ready`），不得回退单供应商 env，也不得静默改用其他 provider。
   - 仅官方适配器可获得 broker token；第三方适配器（P3 沙箱后）另行评审。
   - `runtime_id` 由客户端自报，仅作标签与范围收窄，**不构成强认证**；补偿与残余风险见 §2.3。

3. **协议端点（`providers.endpoints`）**
   - 迁移 `0009_provider_endpoints.sql` 新增 `providers.endpoints TEXT NOT NULL DEFAULT '{}'`，JSON：`{"anthropic":"…","openai":"…","google":"…","dsh":"…"}`；本版不新增迁移，`0009` 不变。
   - `type` 仅表示厂商标识；运行时按协议选择端点。解析顺序如下（显式端点优先，其次 `base_url`，最后官方默认）：

     | 运行时 | 协议 | 显式端点 | 次选 | 官方默认 |
     |---|---|---|---|---|
     | Claude Code | `anthropic` | `endpoints.anthropic` | `base_url`（DeepSeek 自动补 `/anthropic`） | `type=anthropic` → `https://api.anthropic.com`；`type=deepseek` → `https://api.deepseek.com/anthropic` |
     | Codex | `openai` | `endpoints.openai` | `base_url` | `type=openai` → `https://api.openai.com/v1`；`type=deepseek` → `https://api.deepseek.com` |
     | DSH | `dsh` | `endpoints.dsh` | `base_url` | `type=deepseek` → `https://api.deepseek.com` |

   - `base_url` 先去除尾部 `/`；DeepSeek 的 Anthropic 补全仅在路径未以 `/anthropic` 结尾时追加，避免重复。
   - **内置 OpenAI 修正**：内置 OpenAI provider 的 `base_url` 统一为 `https://api.openai.com/v1`；历史值若为 `https://api.openai.com`（缺 `/v1`）且无显式 `endpoints.openai`，解析时补 `/v1`，避免把主机根当作完整端点。
   - 其他类型（如 Google）没有统一官方默认：**必须显式配置端点，否则 `invalid_value`**。
   - **明确修复**：DeepSeek 显式 provider + `{}` endpoints + Claude Code 必须解析到 `https://api.deepseek.com/anthropic`，不得返回“无端点”。
   - 端点由用户输入，**保留基本校验**：仅 `http(s)`、长度上限、控制字符拒绝；**不承诺 HTTPS、不做主机白名单、不保证端点可信**。key 可能被发送到用户指定地址，网络与信任风险由用户承担（§2.3）；UI 必须提示。
   - 内置/主流供应商预设端点由 UI 预填（DeepSeek/Anthropic/OpenAI/Google 等）；用户只需填 Key，可覆盖端点。
   - 一个 provider 暂只保存一把 Key（ADR-028 v0.1；DeepSeek 已验证同一 key 同时服务 OpenAI/Anthropic 端点）。若未来网关要求每协议独立 Key，再扩展 `endpoints` 项结构。

4. **会话级 provider 选择（三官方运行时统一）**
   - **所有官方运行时**：`session_create.provider_id` **语义必填**；DTO 保持可选以兼容旧调用，但核心在创建时必须拒绝缺失 → `invalid_value("请选择供应商")`；**不得按 `(updated_at, id)`、端点或任何其他规则自动选默认**。
   - **`multi_provider` 为静态能力**：核心内置官方运行时静态清单声明 `claude-code` / `codex` / `deepseek-harness` 均支持；**不以 `hello`/runtime ready 为判定门禁**；不改 `RuntimeManifest` 结构（不触发 ADR-015 增量）。`hello.runtime.capabilities` 可继续回显，但仅作诊断，不参与 `session_create` 必选判定。
   - 核心校验：provider 存在/启用、**`model` 必填且属于该 provider 并启用**（三官方运行时；缺失 → `invalid_value("请选择模型")`）、运行时协议端点可解析（含官方默认）、provider → protocol 兼容、静态 `multi_provider` 声明；任一失败 → 创建前拒绝，无部分会话。
   - `provider_id` + `model` 随 `session.create` 下发适配器；`provider_id` 持久化到 `sessions.config.aether.provider_id`，`model` 持久化到 `sessions.model`（不存密钥）。
   - **全局记忆**：使用 settings 键 `ui.provider_selection`（string JSON，≤64KiB，缺省 `"{}"`，全局跨工作区），按运行时映射 `{ "claude-code": "<provider_id>", "codex": "<provider_id>", "deepseek-harness": "<provider_id>" }`。首次成功创建后写入；下次新建会话**预填视为已选择**，用户可直接创建或修改，不再要求每次手动点选。
   - **供应商失效**：记忆值**不自动清空**；provider 被停用/删除时，UI 标记失效并禁止创建/发送，由用户自行重选；**不得自动回退到其他 provider**。
   - **旧会话补选（v0.9 真相表）**：历史消息可查看；`session_send` 的 `provider_id`/`model` 仅在补选场景生效；**provider 切换只允许 `sessions.provider_id` 为空时**；**`sessions.model` 为空或失效时允许一次性 `session_send.model` 补选**（即使已有 provider_id）；补选成功后持久化 `sessions.config.aether.provider_id` / `sessions.model` 并继续本次 send；`run_retry`/`session_resume` 在补选前拒绝；**已选且 model 有效时再传 `provider_id` 或 `model` → `invalid_value`（禁止运行期切换）**；provider 停用/删除后只读，不补选 provider；**适配器准备成功后才持久化，失败不持久化、保持只读**。

     | # | `sessions.provider_id` | `sessions.model` | `send.provider_id` | `send.model` | 结果 |
     |---|---|---|---|---|---|
     | 1 | 空 | 空 | 有 | 有 | 允许补选 provider+model；校验后持久化 |
     | 2 | 空 | 空 | 有 | 无 | `invalid_value("请选择模型")` |
     | 3 | 空 | 空 | 无 | 有 | `invalid_value("请选择供应商")` |
     | 4 | 空 | 空 | 无 | 无 | `invalid_value("请选择供应商")`（旧会话正常 send 也拒绝） |
     | 5 | 空 | 有效 | 有 | 无 | 允许补选 provider；沿用现有 model 并校验归属 |
     | 6 | 空 | 有效 | 有 | 有 | 允许补选 provider+model（model 覆盖，仅一次） |
     | 7 | 空 | 有效 | 无 | 有 | `invalid_value("请选择供应商")` |
     | 8 | 空 | 有效 | 无 | 无 | `invalid_value("请选择供应商")` |
     | 9 | 空 | 失效 | 有 | 有 | 允许补选 provider+model |
     | 10 | 空 | 失效 | 有 | 无 | `invalid_value("请选择模型")` |
     | 11 | 空 | 失效 | 无 | 任意 | `invalid_value("请选择供应商")` |
     | 12 | 有 | 有效 | 无 | 无 | 正常 send |
     | 13 | 有 | 有效 | 有 | 任意 | `invalid_value`（禁止切换 provider） |
     | 14 | 有 | 有效 | 无 | 有 | `invalid_value`（禁止运行期改 model） |
     | 15 | 有 | 空 | 无 | 有 | 允许一次性补选 model（校验归属/启用）；持久化 |
     | 16 | 有 | 空 | 无 | 无 | `invalid_value("请选择模型")` |
     | 17 | 有 | 空 | 有 | 任意 | `invalid_value`（provider 已选，禁止切换） |
     | 18 | 有 | 失效（停用/删除/不属于 provider） | 无 | 有 | 允许一次性补选 model（校验归属/启用）；持久化 |
     | 19 | 有 | 失效 | 无 | 无 | `invalid_value("请选择模型")` |
     | 20 | 有 | 失效 | 有 | 任意 | `invalid_value`（provider 已选，禁止切换） |
     | 21 | 有（provider 停用/删除） | 任意 | 任意 | 任意 | `invalid_value`（只读；禁用所有补选，含 model） |

     > **默认拒绝**：未列出的组合一律 `invalid_value`（不得放行/静默忽略）；补选成功后再次传 `provider_id`/`model` 按已选状态处理。**补选并发（v0.9）**：补选仅在会话空闲/无在途 run 时允许；有在途 run → `session_busy`（不排队）；补选与 send 同批串行。**UI 上下文文案**：补选场景的 `session_busy` 使用「当前任务仍在运行，暂不能补选供应商/模型，请等当前任务结束后重试。」，不复用等待队列满文案。**优先级**：第 21 行（provider 失效）先于并发检查返回 `invalid_value`；`session_busy` 仅用于 provider 有效、需补选且存在在途 run 的场景。

   - **补选适配器语义（v0.9）**：仅在适配器准备成功后由核心持久化；失败返回 `core_not_ready`（配置物化/原生状态采用失败）或 `invalid_value`（参数/归属失败），**不持久化、不产生部分会话**；不得静默重建原生会话（Mode N）；dispose 清理 per-session 目录，legacy 目录不动。
   - **停用/删除后的只读**：既有会话仍可查看历史；`session_send`、`run_retry`、`session_resume` 拒绝（`invalid_value`；broker 未接线时 `core_not_ready`）；不自动切换 provider；**不中止当前已在途 run，`session_interrupt` 仍可用**；UI 禁用发送并提示。**UI 优先级（v0.9）**：`provider_state=invalid` → 只读，禁用所有补选（含 model）；仅 provider 有效且 model 空/失效/不属于时才显示 model 补选入口。
   - **自动路由**：ADR-022 的自动路由保留为显式选项；选择自动路由时，provider 仍需显式指定；**provider 列表按“至少一个候选运行时支持该协议”过滤**；无候选 → `no_candidate_runtime`；路由结果与 provider 协议不兼容 → `invalid_value`；UI 同步过滤。
   - 模型选择器按 provider 分组；同一模型 ID 出现在多个 provider 时，必须以 `provider_id` 消歧。
   - **回显状态（v0.9）**：`SessionSummary` 增 `model_state`（`active`=启用、`invalid`=停用/删除/**不属于该 provider**、`missing`=无 model）；**`provider_id` 空时**：model 在任一启用 provider 中**存在且启用** → `active`，否则 `invalid`；选中 provider 后再按归属复核；**provider 有效且 model 失效/缺失时 UI 显示一次性补选入口**；provider 失效时只读、禁用所有补选；仅 provider 有效时允许一次性 `session_send.model` 补选。

5. **三官方运行时同步支持与配置隔离**
   - **Claude Code**：保持 per-run provider env（`ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY`，必要时 `ANTHROPIC_AUTH_TOKEN`）；两个并发会话可使用不同 provider。
   - **Codex**：必须改为 **per-session `CODEX_HOME`**（应用数据目录下按 session id 隔离）并物化该会话的 `config.toml`；`config.toml` 只写 `base_url` 与 env 键名，**不写密钥值**；spawn env 注入该 run 的 key；并发会话不得共享同一 `CODEX_HOME`/`config.toml`；dispose 清理。
   - **DSH**：优先验证 ACP `session/new` 是否支持 per-session provider；不支持则采用 **per-session profile 目录**（应用数据目录下按 session id 隔离），不得让两个并发会话共享同一 profile；dispose 清理。
   - **旧会话补选（v0.9）**：三运行时统一“先准备、后持久化；失败不持久化、保持只读；不静默重建原生会话”：
     - **Claude Code**：per-run env 切换（`ANTHROPIC_BASE_URL`/`ANTHROPIC_API_KEY`），原生会话 id 不变；env 无法构建 → 本次 send 失败（不持久化）。
     - **Codex**：首次补选 send 时创建 per-session `CODEX_HOME`；把旧会话当前 `CODEX_HOME` 中的 thread/session 状态**复制/采用**到新 home（legacy home 不改），再物化新 `config.toml`（仅 env 键名）；若旧会话已是 per-session home，则直接更新该 home；复制/物化失败 → 本次 send 失败（`core_not_ready`，不持久化、不重建 thread）。
     - **DSH**：优先 ACP per-session provider；不支持则创建 per-session profile，并把旧 profile 的会话状态**复制/采用**到新 profile；失败 → `core_not_ready`，不持久化、不重建 ACP session；禁止并发共享 profile。
     - **model 空/失效补选**：provider 不变，仅准备该 provider 的 model 配置（Codex 更新 per-session config、DSH 更新 profile、Claude per-run model）；失败同样不持久化。
     - dispose 清理 per-session home/profile；legacy home/profile 不动。
     - **状态复制约束（v0.9）**：仅在会话**无在途 run / 空闲**时执行；有在途 run → 补选 `session_busy`（不排队）；适配器必须持有 per-session/legacy 配置目录的独占锁，复制/采用过程不得并发写；SQLite/文件锁失败或复制不完整 → 清理新目录、本次 send 失败（`core_not_ready`）；**“复制/采用”在真实 Codex/DSH 冒烟前只作设计假设，不写成已验证事实**；E2E 可用真实 CLI 冒烟或明确 SKIP 理由。
     - **准备成功判定（v0.9）**：= 配置物化完成 + 原生状态采用完成 + 适配器 ack（`session.send` ack 前完成）；run 启动后的失败不回滚已持久化的 provider/model；若 run 失败由 provider/model 失效导致，按只读/model 补选处理；若准备阶段失败，不持久化、不产生部分会话。
   - **可行性门**：若 Codex/DSH 的 per-session 隔离在 P1 内不可行，必须回到用户重新裁定范围/工期，**不得静默退回“仅 Claude”**或“Codex/DSH 后续”。
   - 三个运行时的选择语义、记忆、失效只读、目录查询、fail-fast 必须一致；差异仅限配置物化方式。

6. **目录查询与刷新**
   - 新增 D6 只读方法 **`provider.catalog.get`**（方向：adapter → core 请求/响应；超时 5s；帧上限沿用 D6 2MiB；不新增 IPC 命令）。现有线协议已支持 adapter → core 通知帧；本方法复用同一 JSON-Lines 双向读写与 JSON-RPC id 匹配，不新增帧类型、不改 protocol major。
   - **双向请求语义（v0.5）**：仅登记 `provider.catalog.get` 一个 adapter→core 请求；adapter 侧请求 id 使用独立命名空间（与 core→adapter 请求按方向 + id 匹配，互不冲突）；core 入站分发器校验方法白名单与严格参数；每适配器同一时刻最多 1 个在途目录查询，并发请求复用同一在途结果（coalesce，不排队）；超时/断连清除待决表并丢弃迟到响应；未知方法 `-32601`，参数错误 `-32602`，内部错误/未就绪 `-32603`。
   - 请求参数：`{}`（严格解析，无成员）；响应示例：`{"providers":[{"id":"<provider_id>","name":"<display_name>","type":"deepseek","enabled":true,"endpoints":{},"models":[{"id":"<model_id>","display_name":"<model_name>","enabled":true}]}]}`；**不得含 `api_key`、`api_key_ref`、token**；认证方式为派生字段（`protocol → auth_env`），不进入响应。
   - **错误映射（v0.5）**：JSON-RPC 标准码 + `error.data.aether_code`（既有 IPC 语义，不新增错误码）：
     - `-32601`（未知方法/旧核心）→ `aether_code=core_not_ready`；
     - `-32602`（参数非法）→ `aether_code=invalid_value`；
     - `-32603`（内部/未就绪/响应超限）→ `aether_code=core_not_ready`（若为参数语义则 `invalid_value`）；
     - 传输超时/断连 → 适配器按 `core_not_ready` 处理。适配器只把该结果用于本次操作失败，不写库、不缓存。
   - **D6 错误 data 约定（v0.9）**：`error.data.aether_code` 取值仅 `core_not_ready` / `invalid_value`，用于适配器映射；不新增 Aether 错误码、不增 ADR-006 登记行；其他 adapter→core 方法若后续登记，复用同一 data 形状。
   - 适配器在会话创建/每次 run 前按需查询，不缓存密钥；目录查询失败 → 本次操作失败（`core_not_ready`），不得静默使用旧快照或 spawn 时快照。
   - **响应超过 2MiB**：核心返回 `-32603` + `error.data.aether_code=core_not_ready` 错误帧（稳定 message），**不截断、不断连**；适配器按失败处理；若目录规模逼近上限，分页另立 ADR。M7-10 证据记录目录规模与查询延迟（本地基准，不设外部门槛）。
   - `hello`/`initialize` 不再作为 `multi_provider` 门禁；可继续回显能力，但 `session_create` 以静态清单为准。

### 2.2 明确不做

- 不把全部供应商密钥注入所有适配器进程；
- 不让适配器直接读取 OS 凭据库/A3 加密文件；
- 不把密钥写入任何配置文件、日志、诊断包、导出或测试夹具；
- 不新增 IPC 命令、事件类型、错误码、迁移；命令面保持 63；
- 新增一个 D6 只读目录查询方法 `provider.catalog.get`（非 IPC 命令；线协议 major/帧上限不变；不新增事件/错误码）；
- 不做 per-adapter token、per-session token（本次为“暂定接受”应用级共享 token；触发条件见 §2.3，届时另立 ADR）；
- 不做 broker 审计落库、不新增审计事件；仅 `tracing` 运行日志，进程重启后不可查，不作为合规审计；
- 不自动选择 provider、不自动切换 provider、不自动清空失效记忆；
- legacy 单供应商自动选择仅保留 P3/非官方/历史回退；P1 三官方运行时永不进入 legacy 自动选择，相关测试不纳入 P1 官方验收；
- 不新增第 64 个 IPC 命令；旧会话补选只经 `session_send.provider_id`（+ 可选 `model`）承载；
- 不在旧会话补选时静默重建原生会话（Mode N）；无法复制/采用原生状态 → 本次 send 失败并保持只读；
- 不在 provider 已选且 model 有效时允许再次改 provider/model；model 空/失效时仅允许一次性补选 model；
- 不做端点 HTTPS 强制、主机白名单、端点可信保证；仅保留基本 URL 校验；
- 不把真实供应商 key 放进 CI/测试夹具；测试仅用合成/假 key 并显式白名单；
- 不在 P1 支持第三方适配器（仍 P3）。

### 2.3 信任边界与补偿

- **共享 token（暂定接受）**
  - 适配器进程本身可信；broker token 为**应用级共享**，不是 per-adapter / per-session 凭据。
  - 已接受残余风险：同用户恶意进程，或被篡改的官方适配器，可读取 token 并向 broker 请求任意 provider 的凭据。**2026-10-22 用户暂定接受**；正式发布前仍需产品/安全负责人签署，或按触发条件转 per-adapter token ADR。
  - `runtime_id` 由客户端自报，仅作标签与范围收窄，不构成强认证。
- **端点（用户输入风险）**
  - 端点完全由用户输入；不做 HTTPS/主机白名单/端点可信保证。key 可能被发送至用户指定地址，网络传输与端点信任风险由用户承担；UI 必须提示。
  - 保留 `http(s)`、长度、控制字符等基本校验，防止明显非法输入；不得宣称“密钥不泄露”为无条件保证。
- **补偿措施（不得因端点风险放弃）**
  - token 不落盘、不进日志/诊断包/导出/测试夹具；
  - broker 仅本机、仅当前用户；Windows DACL / Unix socket 权限；
  - 仅官方 runtime label 可请求；runtime → protocol 与 provider → protocol 双重校验；
  - **所有官方适配器的 CLI/工具子进程统一剥离 `AETHER_CREDENTIAL_BROKER`**（不是只 Claude）；
  - 每次会话/每次 run 重新取密钥；broker 与适配器均不缓存；
  - 创建前 fail-fast（broker/端点/模型/key/兼容性任一不可用即拒绝，无部分会话）；
  - 日志/诊断/导出/证据产物密钥扫描 0 命中；测试代码/合成 key 白名单除外。
- **后续改造触发条件**
  - 第三方适配器接入（P3）；安全评审要求 per-adapter token；或出现越权事件；届时**另立 ADR**，改为 per-adapter token + runtime/protocol 范围，不得在本 ADR 内静默扩展。

### 2.4 密钥扫描口径

- **扫描对象**：非测试源码（排除 `tests/`、`*.test.ts`、`*.test.tsx` 等测试代码）、运行日志、诊断包、导出文件、证据产物。
- **禁止**：真实供应商 key、JWT、PEM 私钥出现在上述对象。
- **合成/假测试密钥**：仅测试代码/夹具可使用；使用可识别前缀/固定值，纳入显式白名单；白名单文件本身不得含真实 key。
- 证据文件记录扫描命令、排除项、白名单、0 命中输出。

## 3. 硬约束

1. 三个官方运行时的 `multi_provider` 以核心静态清单为准，**不依赖 `hello`/runtime ready**；`runtimes_list.capabilities` = hello/initialize 能力 ∪ 核心静态表，UI 不依赖 hello/ready 显示供应商选择；静态声明与适配器实现必须同批，不得先声明后实现。
2. 三官方运行时 `session_create.provider_id` **与 `model` 均语义必填**；缺失 provider → `invalid_value("请选择供应商")`，缺失 model → `invalid_value("请选择模型")`；`model` 必须属于该 provider 并启用；无默认、无自动切换。
3. 全局记忆使用 settings `ui.provider_selection`；供应商失效不自动清空、不自动回退；首次选择后预填视为已选。
4. 旧会话补选按 v0.9 穷尽真相表（21 行 + **未列组合默认 `invalid_value`**）：provider 切换仅在 `sessions.provider_id` 为空时；`sessions.model` 空/失效/不属于时允许一次性 `session_send.model` 补选（即使已有 provider_id）；**补选仅在会话空闲/无在途 run 时允许，有在途 run → `session_busy`（不排队）**；适配器准备成功后才持久化，失败不持久化；不静默重建原生会话。
5. provider 停用/删除后既有会话只读：`session_send`/`run_retry`/`session_resume` 拒绝；**`session_interrupt` 保持可用（仅当前在途 run）**；不中止当前 run；不自动切换。model 停用/删除/不属于该 provider 时 `model_state=invalid`；**provider_id 空时 model 在任一启用 provider 中存在且启用 → active，否则 invalid**；provider 失效时禁用所有补选（含 model），仅 provider 有效且 model 空/失效/不属于时才显示 model 补选。
6. `provider.catalog.get` 只返回非机密字段，**不得含 `api_key`/`api_key_ref`/token**；仅登记该一个 adapter→core 请求；adapter/core 请求 id 按方向独立；5s 超时、每适配器最多 1 个在途查询（并发 coalesce）、断连清理；未知方法 `-32601`、参数错误 `-32602`、内部/未就绪/超限 `-32603` + `error.data.aether_code`；响应 ≤2MiB，超限返回错误帧且不截断/不断连；不新增 IPC 命令/错误码。
7. broker 只监听本机，仅当前用户可连（Windows DACL / Unix socket 权限）；token 每次应用启动随机生成、同一启动内连接复用，不落盘、不进日志/诊断/导出/夹具。
8. broker 与适配器均不缓存密钥；key 只允许进入当前 run 的 CLI/工具子进程 env，不得写入适配器持久状态；下一次 run 必须重新获取。
9. 所有官方适配器必须把 `AETHER_CREDENTIAL_BROKER` 从 CLI/工具子进程 env 中剥离（共享 `stripBrokerEnv`）；适配器进程自身可见 token 属已知信任边界（§2.3）。
10. broker 校验 provider → protocol 兼容性；不返回不兼容 provider 的 key；Codex/DSH 并发会话必须 per-session 配置隔离，不得共享 `CODEX_HOME`/profile。
11. 端点只存 URL，不存密钥；仅 `http(s)`、长度、控制字符基本校验；**内置 OpenAI `base_url` 统一 `/v1`，历史值缺 `/v1` 且无显式 `endpoints.openai` 时解析层补全（`migrations/0003` seed 不改）**；不做 HTTPS/主机白名单/可信保证；UI 必须提示用户输入风险。
12. 缺少 provider/model/端点/key/broker/兼容性 → 在创建会话前拒绝（请求级 `invalid_value`；broker 未接线 `core_not_ready`），**不得创建会话行、不得发事件**。
13. broker 只写运行日志（tracing），**不新增 DB 写入、不新增事件类型、进程重启后不可查、不作为合规审计**；持久审计须另立 ADR。
14. 不新增命令/事件/错误码/迁移；命令面 63、错误码全集 32、迁移 `0009` 不变；设置键 8 → **9**（`ui.provider_selection`）。
15. 官方适配器白名单/注册表不变；不新增 runtime_id，不改 ADR-015 准入模型；第三方适配器仍 P3。

## 4. 实现落点

| 位置 | 内容 | v0.9 状态 |
|---|---|---|
| `migrations/0009_provider_endpoints.sql` | `providers.endpoints` JSON 列 | 已落地；不变 |
| `crates/aether-store/src/ops.rs` | `ProviderRecord.endpoints` + insert/update/select | 已落地 |
| `crates/aether-tauri/src/ipc/dto.rs` | provider endpoints + `session_create.provider_id/model`、`session_send.provider_id/model`（旧会话补选）、`SessionSummary.provider_id/provider_name/provider_state/model_state` | 部分（待补回显/补选字段） |
| `crates/aether-tauri/src/provider_control.rs` | endpoints 读写/回显 | 已落地 |
| 官方运行时静态清单（`aether-control`/内置常量表） | `claude-code` / `codex` / `deepseek-harness` 声明 `multi_provider`；`session_create` 以此判定 | 待整改 |
| `crates/aether-tauri/src/provider_env.rs` | 端点解析顺序 + 官方默认 + OpenAI `/v1` 修正；broker 注入；`broker_available()` / `preflight_credential()`；删除 legacy 自动选择 | 待整改 |
| `crates/aether-tauri/src/credential_broker.rs` | 应用级 token；`runtime_id` 必填；runtime → protocol + provider → protocol；ACL；仅 `tracing` | 待整改 |
| `crates/aether-tauri/src/session_backend.rs` + `lib.rs` | 三运行时必选 + `model` 必填；静态能力；`runtimes_list.capabilities` 合并静态表；旧会话真相表/补选持久化；`model_state`；失效只读 + 中断可用；创建前 fail-fast；`SessionSummary` 四字段 | 待整改 |
| D6 适配器↔核心 | `provider.catalog.get` 请求/响应与 adapter-sdk 客户端 | 待整改 |
| `packages/adapter-sdk/src/child-env.ts` | `stripBrokerEnv` 共享工具 | 待新增 |
| `packages/adapter-sdk/src/catalog.ts` | 目录查询客户端（不缓存密钥、不落盘） | 待新增 |
| `packages/adapter-claude-code/src/claude-cli.ts` | 已剥离；改为共享工具 + 最终 `delete` 兜底 | 部分 |
| `packages/adapter-codex/src/codex-adapter.ts` | per-session `CODEX_HOME`/`config.toml`；旧会话补选时复制/采用原生 thread 状态；`stripBrokerEnv`；目录查询 | 待整改 |
| `packages/adapter-dsh/src/dsh-acp-client.ts` | per-session profile；旧会话补选时复制/采用原生会话状态；`stripBrokerEnv`；目录查询 | 待整改 |
| `crates/aether-tauri/src/settings_control.rs` / `SETTINGS_KEY_ALLOWLIST` | 新增 `ui.provider_selection`（string JSON ≤64KiB） | 待整改 |
| `apps/desktop/src/SessionWorkbench.tsx` / `ModelSelector.tsx` / `providers.ts` / `ProvidersPage.tsx` | 三运行时供应商选择（不依赖 hello/ready）+ 模型必选；旧会话重选（复用 `session-provider-select`，`data-context="reselect"`）；记忆/失效只读/端点预览/风险提示；5 个锚点 | 待整改 |
| `docs/P1-实施计划与验收标准.md` M7-10 | v0.13 编辑性 P2、计数修正、DoD 重写 | 已同步 |
| `docs/M7-10-证据.md` | 三运行时 E2E 与整改证据 | 待新增 |

> 测试与证据的验收口径见 §7；未完成项在 §10 逐条登记，不得以“v0.2 首版实现”替代 v0.9 验收。

## 5. 影响（目标版本链）

### 5.1 《设计文档》v1.23 → v1.24（已同步）

- 头部/版本记录：升 v1.24，注明 ADR-028 v0.9 合入；状态保持“设计基线已冻结，实现/验收基线待代码/E2E/证据闭合”。
- D6 `session.create`：三运行时 `provider_id` + `model` 语义必填；D6 `session.send` 旧会话补选按 v0.9 **21 行穷尽真相表 + 未列组合默认 `invalid_value`**；**补选仅在空闲/无在途 run 时允许，有在途 run → `session_busy`**；适配器准备成功后才持久化。
- D6 方法表：`provider.catalog.get` 的 `error.data.aether_code` 登记为 D6 错误 data 约定（仅 `core_not_ready`/`invalid_value`，不新增错误码）。
- D6 能力项：`multi_provider` 静态表例外；`runtimes_list.capabilities` = hello ∪ 静态表；legacy 自动选择仅 P3/非官方/历史回退。
- D7 `session_create`/`session_send`：provider/model 必选、**与 ADR 同一份 21 行真相表**、补选并发 `session_busy`、旧会话适配器准备/失败不持久化、model 空/失效/不属于补选、`SessionSummary.model_state`（`active|invalid|missing`；provider 空时 model 在任一启用 provider 中**存在且启用** → active，否则 invalid）、只读态中断可用、**provider 失效禁用所有补选**。
- D10：三运行时按 run 取 key；legacy 限定 P3/非官方；旧会话补选适配器语义（Claude per-run env；Codex 复制/采用 thread 状态到 per-session home；DSH 复制/采用会话状态到 per-session profile；**仅无在途 run、独占锁、失败清理**；准备成功判定/run 失败不回滚）；内置 OpenAI `/v1` 归一化；扫描口径；拆分“keychain 写入”与“消费映射”措辞。
- §5 P1：M7-10 为三运行时直接实施项；退出标准“门禁任务（M5–M8 除 §8 直接实施项）全部 DoD 通过”。
- 附录 A UI-09：补三运行时多供应商、`provider.catalog.get`、全局记忆、失效只读/中断、`model_state`、ADR-028 v0.9/M7-10；§2.3 直接实施项注补多供应商。
- 附录 C：`0009` 不变；不新增迁移；设置键按 settings KV 处理。
- 版本记录：新增 v1.24 行。

### 5.2 《需求文档》v0.17 → v0.18（已同步）

- 头部版本行：升 v0.18，标注“已同步（2026-10-23）”。
- UI-05/UI-09：三官方运行时 provider 选择时 `model` 必填；`model` 空/失效/**不属于 provider** 允许一次性 `session_send.model` 补选；旧会话 provider/model 补选按 21 行真相表；**补选仅在空闲/无在途 run 时允许，有在途 run → `session_busy`**；provider 失效禁用所有补选；只读态中断可用。
- §3.8 UI-09 行：P1 范围与验收口径同步；首次用户选择、后续预填视为已选；停用/删除只读；不自动切换。
- §8 P1 行：P1 交付描述补多供应商。
- 版本记录：新增 v0.18 行。
- **需求口径（可验收）**：

  > 同一 Agent 可同时配置多个供应商；三个官方运行时新建会话时必须显式选择供应商与模型；首次选择后写入全局设置，后续新建会话预填并视为已选；旧会话按 21 行真相表补选（未列组合默认拒绝），已有 provider 且 model 空/失效/不属于该 provider 时只允许一次性补选 model；补选仅在空闲/无在途 run 时允许；补选遇在途 run → `session_busy`，UI 使用补选专用上下文文案（不复用队列满文案）；密钥不在会话创建时写入配置，按会话/按 run 向本地凭据代理获取；供应商停用/删除后已有会话只读且禁用所有补选，但当前在途 run 仍可中断；不自动切换。`model_state` 在 provider 为空时：model 在任一启用 provider 中**存在且启用** → active，否则 invalid。

### 5.3 `docs/P1-实施计划与验收标准.md` v0.12 → v0.13（已同步；M7-10 编辑性 P2 清理）

M7-10 已在计划中（约 line 359），本次为“编辑性 P2 清理 + 计数修正”，**不是新增任务；不得把 M7-10 重复计入任务总量**：

| 位置 | 修改 |
|---|---|
| 头部基线（line 6–8） | 设计 v1.24、需求 v0.18、P1 计划 v0.13、UI-UX v0.14；补 ADR-028 v0.9；设置键 9 / UI-UX 锚点 20 |
| M7-10 执行状态（line 362） | 改为 **“待实现（ADR-028 v0.9 已通过）”**；删除“Phase 1 完成” |
| M7-10 描述 | 三运行时、显式 provider+model、21 行真相表/补选并发、provider 空 model_state（含 model 停用样本）、UI 优先级、旧会话适配器状态复制约束、model 空/失效/不属于补选、目录查询、只读（中断可用）、端点风险、三适配器 token 剥离、E2E |
| M7-10 DoD（line 363–369） | 按 ADR §7 重写（P1 计划 8 条）；补 21 行逐行、补选并发 `session_busy` 与补选上下文文案、provider 空 model_state（含 model 停用样本）、UI 优先级、状态复制锁/回滚、准备成功判定、旧会话三运行时补选/失败回滚、目录协议一致性、冷启动/只读中断/自动路由过滤 |
| §1.3 依赖图（line 94、109） | G6 增加 M7-10；标注“直接实施项，不占关键路径” |
| §1.5 门禁一览（line 115） | “M7-07/08/09” → “M7-07/08/09/10” |
| §1.6 任务总量（line 119） | 20 → **21**（含 M7-10 一次，不得重复累加）；M7 9 → **10**；直接实施 3 → **4**；量级 L 12 → **13**（M7-10 暂定 L；动工前复核，超出 L 按计划规则调整并记录证据）；门禁 17 不变 |
| §7 需求映射（line 455） | 新增一行：UI-09 三运行时多供应商/按需凭据/全局记忆/只读 → M7-10；原 M7-06 行保留（消费映射） |
| §8 直接实施项（line 465+） | 表格增加 M7-10 行 |
| §9/§10 引用 | 所有“M7-07/08/09”按语义补 M7-10 |
| M8-03（line 392/398） | 直接实施项证据索引补 M7-10；M7-10 证据归档后补索引 |
| M8-04 发布说明 | 补 M7-10 交付与已知边界（共享 token 暂定接受、端点用户输入风险、三运行时） |
| 证据 | 新增 `docs/M7-10-证据.md` 为必交项；`verify:m7-10` 实现时新增，若用等价命令须在证据记录 |

### 5.4 《UI-UX 设计规格》v0.13 → v0.14（已同步）

- 头部版本/基线：升 v0.14；设计 v1.24、需求 v0.18、P1 计划 v0.13。
- §2.2 新建会话表单：三运行时均显示供应商选择 + 模型选择（provider 选定后 model 必填；自动路由时 provider 按候选运行时协议过滤）；默认来自全局 settings 记忆；无有效选择时创建按钮禁用；预填视为已选。
- §2.4 底部输入区：供应商失效/删除禁止发送但**保留中断按钮**；**provider 失效 → 只读，禁用所有补选（含 model）**；仅 provider 有效且 model 空/失效/不属于时显示“补选模型”入口；旧会话 provider 补选入口（`session-provider-select`，`data-context="reselect"`）；**补选命中在途 run → `session_busy`，使用补选专用文案（不复用队列满文案）**。
- §2.5/§3.3 会话头：显示当前供应商与模型状态（`session-provider-current` + `data-model-state="active|invalid|missing"`；provider 空时 model 在任一启用 provider 中**存在且启用** → active，否则 invalid）。
- §5 文案：新增 6 条（请选择供应商；请选择模型；供应商已停用/删除当前会话只读且可中断；模型已停用/删除或不属于所选供应商，请补选模型；端点由用户填写、Key 将发送至该地址；上次选择已预填）。
- §7.3 锚点：5 组不变，`session-provider-select` 增 `data-context="create|reselect"`，`provider-required-notice` 增 `data-field="provider|model"`，`session-provider-current` 增 `data-model-state`；计数 20。
- §8：新增供应商选择/记忆/只读/模型补选的 P1 行。
- 版本记录：新增 v0.14 行。

### 5.5 `AGENTS.md`（已同步）

- §1 基线：需求 v0.18、设计 v1.24、P1 计划 v0.13、UI-UX v0.14、ADR-028 v0.9。
- 阶段口径：三官方运行时均支持多供应商选择；第三方仍 P3。
- 安全边界：应用级共享 token 暂定接受；端点用户输入风险；broker ACL 与子进程剥离保留。
- 行为口径：三官方新建会话 provider+model 必填；旧会话按 v0.9 21 行真相表补选（未列组合默认拒绝；补选仅空闲/无在途 run，否则 `session_busy`；补选 `session_busy` 的 UI 使用上下文文案，不复用队列满文案）；model 空/失效/不属于允许一次性补选；provider 空时 model 需在任一启用 provider 中**存在且启用**；provider 失效禁用所有补选；只读态中断可用；状态复制仅空闲/独占锁。
- §8 目录表同步设计文档当前版本。

### 5.6 不影响的

- IPC 命令面 **63** 不变；不新增命令、事件、错误码、迁移。
- D6 线协议 major / 帧上限 / `artifact_ref` 契约不变；新增一个适配器→核心只读方法。
- D4 事件管线、D9 权限门、D5 进程监督不变。
- 迁移 `0009_provider_endpoints.sql` 内容不变。
- 设置键 8 → **9**（`ui.provider_selection`，settings KV，无迁移）。
- UI-UX 锚点 15 → **20**（新增 5 组）。
- 官方适配器白名单/注册表与 ADR-015 准入模型不变；第三方适配器仍 P3。

## 6. 回退条件

1. broker 导致任何密钥落盘/日志/诊断包/导出/夹具泄露 → **立即停用 broker 与受影响运行时的 `multi_provider` 能力**，清理产物并复盘；multi_provider 会话创建失败，**不得以单供应商 env 静默顶替**。
2. 官方适配器无法稳定获取凭据或并发会话串 provider → 按运行时关闭 `multi_provider` 能力（静态清单移除该运行时），保留 `providers.endpoints` 与未声明运行时的单供应商路径；必要时回退版本。不得改回“broker 不可用则回退单供应商 env”。
3. Codex/DSH 的 per-session 配置隔离无法在 P1 内验证 → **回到用户重新裁定范围/工期**；不得静默退回“仅 Claude”或“Codex/DSH 后续”。
4. 端点/选择/只读校验导致未声明 `multi_provider` 的旧路径回归 → 回退该路径的校验改动，保留端点字段与 multi_provider 校验；不得放宽三运行时必选/可解析要求。
5. 安全评审要求 per-adapter token，或出现越权事件 → 另立 ADR，改为 per-adapter token + runtime/protocol 范围；不得在未评审情况下静默扩展 token 权限。
6. 新错误码/协议语义需求 → 先按 ADR-006 登记，不得自造；命令面/事件类型不得因本 ADR 增加。

## 7. 验证与 DoD

1. 迁移 `0009` + `providers.endpoints` 读写/回显；既有库 `{}` 回退按类型（DeepSeek 双端点可解析）；**内置 OpenAI `base_url` `/v1` 归一化（历史缺 `/v1` 且无显式 `endpoints.openai` 时补全；0003 seed 不改；内置 OpenAI `{}` 用例）**；`m3_11_providers` 回归。
2. 三运行时静态 `multi_provider` 声明与适配器实现同批；`runtimes_list.capabilities` = hello 能力 ∪ 静态表；UI 对三官方运行时始终显示供应商选择（不依赖 hello/ready；含 cold/connecting 测试）；新建会话 provider+model 必选；UI 预设端点 + 空端点自动补全预览 + 端点风险提示 + 全局记忆预填。
3. `session_create.provider_id` + `model`：三运行时必填（缺失 provider → `invalid_value("请选择供应商")`，缺失 model → `invalid_value("请选择模型")`）、provider/model/endpoint/capability/兼容性校验、无默认、失败**无部分会话**；**旧会话按 v0.9 21 行穷尽真相表 + 未列组合默认拒绝**（provider 切换仅 provider_id 空；model 空/失效/不属于允许一次性 model 补选；已选且 model 有效再传 → `invalid_value`；**补选仅空闲/无在途 run，有在途 run → `session_busy`**）；适配器准备成功后持久化，失败不持久化；`SessionSummary.model_state` 三态（provider 空时 model 在任一启用 provider 中**存在且启用** → active，否则 invalid）；补选后 send/retry/resume 恢复；provider 失效只读且禁用所有补选、model 失效/缺失/不属于可补选、中断可用。
4. 目录查询 `provider.catalog.get`：仅一个 adapter→core 请求；请求 id 按方向独立、5s 超时、待决表/断连清理、每适配器最多 1 个在途（并发 coalesce）；未知方法 `-32601`、参数错误 `-32602`、内部/未就绪/超限 `-32603` + `error.data.aether_code`（D6 错误 data 约定，不新增错误码）；≤2MiB，超限返回错误帧不截断/不断连；仅非机密字段、无 key/`api_key_ref`/token；命令面 63 不变；记录目录规模/查询延迟。
5. broker：应用级 token 语义（启动轮换、连接复用）、`runtime_id` 必填、runtime → protocol + provider → protocol、ACL、按需读取且不缓存、日志不含密钥；**三个官方适配器 CLI env 均无 `AETHER_CREDENTIAL_BROKER`**（需 key 的 CLI 仍含对应 key）；无 DB 写入、无事件。
6. **应用级 E2E**（新增 `crates/aether-tauri/tests/m7_10_provider_e2e.rs`）：
   - 走 `session_create` / `session_send` 命令层 + 真 broker + 真 `ProviderEnvHook` + 真适配器进程 + 假 CLI + 本地 mock 上游；
   - **三运行时**分别验证 per-session provider + `model` 必填；两会话并发、两 provider、不同 key/base_url；
   - **旧会话补选正向 E2E（三运行时分别）**：无 `provider_id` 的旧会话首次 `session_send` 携带 `provider_id`（必要时 `model`）→ 适配器准备（Claude per-run env / Codex 复制/采用 thread 状态 / DSH 复制/采用会话状态）→ 持久化 → 后续 send/retry/resume 可用，原生上下文保留；已选会话再传 `provider_id`/`model` → `invalid_value`；
   - **v0.2 边界**：`provider_id` 有、`model` 空/失效/不属于 provider 的样本首次 `session_send.model` 补选成功并持久化；失败（归属/停用/配置物化）不持久化、保持只读；无 provider 正常 send（无 send 字段）→ `invalid_value("请选择供应商")`；**provider 空 + model 停用 → `model_state=invalid`（不得判 active）**；
   - **真相表逐行**：第 1–21 行允许/拒绝/错误码逐行断言；未列组合默认拒绝；
   - **状态复制约束**：Codex/DSH 仅在无在途 run/空闲时复制，独占锁；锁/复制失败清理新目录、不持久化；真实 CLI 冒烟或明确 SKIP 理由；
   - **准备成功判定**：配置物化 + 原生状态采用 + ack 前完成；run 启动失败不回滚已持久化的 provider/model，按失效/重试路径处理；
   - 断言：各自 base_url/key/model 正确、无串用、CLI env 无 token、adapter spawn env 无 legacy key；Codex 两会话 `CODEX_HOME` 不同、DSH profile 不同；dispose 清理 per-session 目录且 legacy 目录不动；
   - 负向：无 `provider_id`/`model`、broker 未启动、provider 无 key、key 不可读、provider 停用/删除、模型不属于 provider、目录查询失败、旧会话未补选、适配器准备失败（不持久化、不重建）；均无会话行/无事件（只读场景除外）；
   - 冷启动/未 ready：UI 仍显示供应商选择；目录查询协议一致性（`-32601/-32602/-32603`、超时、断连清理、并发 coalesce）；自动路由时 provider 按候选运行时协议过滤；
   - 密钥轮换：两次 run 之间改 key，第二次取到新值；
   - 只读：停用/删除后 `messages_page` 可读，`session_send`/`run_retry`/`session_resume` 拒绝，**`session_interrupt` 可用**，当前 run 不中止；**provider 空 `model_state`：model 在任一启用 provider 中存在且启用 → active，否则 invalid（含 model 停用样本）；UI 优先级：`provider_state=invalid` → 只读、禁用所有补选（含 model），仅 provider 有效且 model 空/失效/不属于才显示 model 补选**；补选并发有在途 run → `session_busy`（不排队），**UI 使用补选专用文案「当前任务仍在运行，暂不能补选供应商/模型，请等当前任务结束后重试。」，不复用队列满文案**；model 失效/缺失/不属于补选后恢复。
7. 证据与回归：
   - **证据文件 `docs/M7-10-证据.md`**：环境/提交/版本（ADR v0.9、设计 v1.24、需求 v0.18、P1 计划 v0.13、UI-UX v0.14）；v0.8 复审编辑性 P2×3、补选 `session_busy` 上下文文案确认与 v0.6/v0.7 历史复审关闭对照；三运行时正向日志；旧会话补选（三运行时）/v0.2 model 空/失效/不属于/**provider 空 + model 停用**样本日志；21 行真相表逐行结果；**补选并发 `session_busy` 与补选上下文文案**；**provider 空 model_state/UI 优先级**；状态复制锁/失败回滚；准备成功判定/run 失败状态；目录查询协议一致性/目录规模/延迟；CLI env 脱敏快照；密钥扫描（非测试源码/运行产物 0 命中，测试代码 + 合成 key 白名单除外）；M7-06/M3-11/会话创建/设置回归；已知边界（共享 token 暂定接受、端点用户输入风险、per-adapter 触发条件）；执行命令与结果；
   - 回归：`cargo test --workspace`、`cargo clippy --workspace -- -D warnings`、`pnpm -r test`、`tsc`、bindings check、`verify:m7-10` 全绿；**实现时必须新增 `verify:m7-10` 脚本；若使用等价命令，证据文件记录等价命令与结果**；
   - M7-06/M3-11/会话创建/设置测试不回归。

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-10-22 | 创建：ProviderCatalog + Credential Broker + `providers.endpoints` + `session_create.provider_id`；Claude Code 接入；端点层已落地 | 文档维护（AI） |
| v0.2 | 2026-10-22 | 首版实现完成（仅 Claude Code 接入）：`session_create.provider_id`（DTO/校验/持久化/下发）、本地 broker（named pipe/Unix socket + token + 运行日志）、adapter-sdk 客户端、Claude Code `multi_provider` per-run env、UI provider 归属与透传；补并发 mock 上游 E2E。该版结论已被评审整改覆盖 | 文档维护（AI） |
| v0.3 | 2026-10-22 | 按 `docs/ADR-028-修订清单.md` 整改（P0×2/P1×7）：应用级 token 信任边界、按会话/run 取密钥、fail-fast、必选与失效处理、端点补全、仅 tracing、M7-10 登记、三适配器 CLI token 剥离、应用级 E2E。状态改为待复审 | 文档维护（AI） |
| v0.4 | 2026-10-22 | 按 `docs/ADR-028-评审意见.md` §2 的 10 项用户裁定整改：① 应用级共享 token 暂定接受并记录签署要求；② 多供应商转为 P1 正式需求，三官方运行时同步支持；③ `multi_provider` 静态能力、不依赖 hello/ready；④ 三运行时 `provider_id` 必选、无自动选择；⑤ 停用/删除后已有会话只读；⑥ 端点用户输入、不做 HTTPS/主机/可信保证；⑦ 新增 `provider.catalog.get` 只读目录查询；⑧ 全局设置记忆 `ui.provider_selection`、失效不自动清空；⑨ 密钥扫描排除测试代码 + 合成 key 白名单；⑩ 登记 5 个 UI 锚点、UI-UX v0.9。同步设计 v1.19/需求 v0.13/P1 计划 v0.8/AGENTS；代码/E2E/证据仍待承接 | 文档维护（AI） |
| v0.5 | 2026-10-23 | 按 `docs/ADR-028-复审意见.md` 整改：**P0-1** 旧会话补选改由 `session_send.provider_id`（+ 必要时 `model`）承载，不新增命令；**P1-1** 补 adapter→core 请求 id 命名空间/待决表/coalesce/超时/断连/错误映射（`-32601/-32602/-32603` + `error.data.aether_code`）；**P1-2** `runtimes_list.capabilities` = hello ∪ 静态表，UI 不依赖 ready；**P1-3** D6 判定时点改为“除 multi_provider 外 ready 后”；**P1-4** §5 P1 退出标准改“门禁任务（除直接实施项）”；**P1-5** 附录 A UI-09 与 §2.3 注补多供应商；**P1-6** 三官方 provider_id 时 `model` 必填（旧会话补选同规则）；**P1-7** 只读态 `session_interrupt` 可用；**P1-8** 设计 D10 补 OpenAI `/v1` 归一化；P2 状态/措辞/legacy/认证方式/超限帧/自动路由同步。版本链 v1.20/v0.14/v0.9/v0.10；状态维持附条件通过 | 文档维护（AI） |
| v0.6 | 2026-10-23 | 按 `docs/ADR-028-复审意见-v0.5.md` 整改：**P1-1** 旧会话补选适配器语义（Claude per-run env；Codex 复制/采用 thread 状态到 per-session `CODEX_HOME`；DSH 复制/采用会话状态到 per-session profile；准备成功才持久化、失败不重建/保持只读；dispose 清理）；**P1-2** v0.2 “provider_id 有、model 空”允许一次性 `session_send.model` 补选，model 失效同理；**P2-1** `provider_id × model × sessions.model` 真相表 14 行；**P2-2** `SessionSummary.model_state`（active/invalid/missing）与 UI 提示；**P2-3/P2-6** D10 扫描口径与“写入/消费”措辞；**P2-4** model 失效补选；**P2-5** `verify:m7-10`/M8-03 索引；D6 `error.data.aether_code` 约定。版本链 v1.21/v0.15/v0.10/v0.11；状态维持附条件通过 | 文档维护（AI） |
| v0.7 | 2026-10-23 | 按 `docs/ADR-028-复审意见-v0.6.md` 整改：**P1-1** 真相表统一为 21 行穷尽表 + 未列组合默认 `invalid_value`，覆盖 provider 空/有/失效 × model 空/有效/失效 × send provider/model 有/无；ADR 与设计同一份表；**P2-1** `model_state` 增“不属于 provider”=invalid；**P2-2** Codex/DSH 状态复制仅无在途 run/空闲、独占锁、失败清理、真实 CLI 待验证；**P2-3** `verify:m7-10`/M8-03 索引；**P2-4** M7-10 暂定 L、动工前复核；**P2-5** 准备成功判定（配置物化+状态采用+ack）与 run 失败不回滚。版本链 v1.22/v0.16/v0.11/v0.12；状态维持附条件通过 | 文档维护（AI） |
| v0.8 | 2026-10-23 | 按 `docs/ADR-028-复审意见-v0.7.md` 清理 P2：统一版本标签至 v0.8/v1.23/v0.17/v0.12/v0.13；`model_state` 补 provider 空语义（任一启用 provider 存在→active，否则 invalid）；UI 优先级 provider 失效禁用所有补选；补选并发有在途 run→`session_busy`；证据对照覆盖 v0.6 复审 1×P1+5×P2 与 v0.7 复审 P2×6；`verify:m7-10`/M8-03/L/token 签署保留待实施。状态维持附条件通过 | 文档维护（AI） |
| v0.9 | 2026-10-23 | 按 `docs/ADR-028-复审意见-v0.8.md` 清理编辑性 P2：① provider 空 `model_state` 改为“model 在任一启用 provider 中**存在且启用** → active，否则 invalid”，补 model 停用样本；② UI-UX §7.3 锚点日期改 v0.14；③ §10 顶部注记历史行按当时版本；④ 补选 `session_busy` 使用上下文文案（不复用队列满文案）。版本链 v1.24/v0.18/v0.13/v0.14；文档侧通过 | 文档维护（AI） |

## 9. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-10-22 | 用户确认设计 | 确认（v0.2） | 首版实现完成（仅 Claude Code）；该结论已被同日评审整改覆盖 |
| 2026-10-22 | 评审（`docs/ADR-028-修订清单.md`） | 整改（P0×2/P1×7），待复审 | v0.3 承接；共享 token 残余风险未签认 |
| 2026-10-22 | 评审（`docs/ADR-028-评审意见.md`） | **附条件通过** | 用户 10 项裁定见该意见 §2；三文档同步、三运行时、目录查询、设置记忆、只读、E2E/证据为复审条件；共享 token **暂定接受**；未标记“已通过” |
| 2026-10-22 | 用户裁定 | 暂定接受应用级共享 token | 正式发布前仍需产品/安全负责人签署，或按 §2.3 触发条件转 per-adapter token ADR |
| 2026-10-22 | 文档维护（AI） | v0.4 整改稿提交复审 | 文档侧已同步 ADR/设计/需求/P1 计划/UI-UX/AGENTS；代码、E2E、证据待承接（§10），复审前不标记“已通过” |
| 2026-10-23 | 复审（`docs/ADR-028-复审意见.md`） | **附条件通过（维持）** | 文档侧 10 项裁定已落文；P0-1 旧会话补选无路径 + P1×8（目录协议/静态 UI 通路/D6 判定/§5 退出标准/附录 A/model/只读中断/OpenAI `/v1`）待闭合；代码/E2E/证据未完成 |
| 2026-10-23 | 文档维护（AI） | v0.5 整改稿提交复审 | 文档侧已按 1×P0 + 8×P1 + 9×P2 整改并同步设计 v1.20/需求 v0.14/P1 计划 v0.9/UI-UX v0.10/AGENTS；代码/E2E/证据待承接，复审前不标记“已通过” |
| 2026-10-23 | 复审（`docs/ADR-028-复审意见-v0.5.md`） | **附条件通过（维持）** | 上一轮 1×P0 + 8×P1 文档侧闭合；新增 P1-1 旧会话补选适配器语义、P1-2 v0.2 provider_id+model 空会话、P2×6；代码/E2E/证据未完成 |
| 2026-10-23 | 文档维护（AI） | v0.6 整改稿提交复审 | 文档侧已按 P1-1/P1-2/P2 整改并同步设计 v1.21/需求 v0.15/P1 计划 v0.10/UI-UX v0.11/AGENTS；代码/E2E/证据待承接，复审前不标记“已通过” |
| 2026-10-23 | 复审（`docs/ADR-028-复审意见-v0.6.md`） | **附条件通过（维持）** | 上一轮 2×P1 已闭合、P2 大部分闭合；残留 P1-1 真相表不完整/ADR 与设计不一致 + P2×5；代码/E2E/证据未完成 |
| 2026-10-23 | 文档维护（AI） | v0.7 整改稿提交复审 | 文档侧已按 P1-1/P2 整改并同步设计 v1.22/需求 v0.16/P1 计划 v0.11/UI-UX v0.12/AGENTS；代码/E2E/证据待承接，复审前不标记“已通过” |
| 2026-10-23 | 复审（`docs/ADR-028-复审意见-v0.7.md`） | **附条件通过（维持）** | 文档侧 P0/P1 已闭合；残留 P2×6（版本标签、provider 空 model_state、UI 优先级、补选并发、verify/M8-03/L、token 签署）；代码/E2E/证据未完成 |
| 2026-10-23 | 文档维护（AI） | v0.8 P2 清理稿提交复审 | 文档侧已清理 P2 并同步设计 v1.23/需求 v0.17/P1 计划 v0.12/UI-UX v0.13/AGENTS；代码/E2E/证据待承接，复审前不标记“已通过” |
| 2026-10-23 | 复审（`docs/ADR-028-复审意见-v0.8.md`） | **附条件通过（维持）** | 文档侧无阻塞性 P0/P1/P2；上一轮 P2×6 已清理；剩余 3 个编辑性 P2（`model_state` 补“且启用”、UI-UX 锚点表日期、§10 历史行注记）；代码/E2E/证据未完成 |
| 2026-10-23 | 文档维护（AI） | v0.9 编辑性 P2 清理稿提交复审 | 文档侧已清理 3 个编辑性 P2 并同步设计 v1.24/需求 v0.18/P1 计划 v0.13/UI-UX v0.14/AGENTS；代码/E2E/证据待承接，复审前不标记“已通过” |
| 2026-10-23 | 复审（`docs/ADR-028-复审意见-v0.9.md`） | **附条件通过（维持）** | 文档侧闭合，无阻塞性 P0/P1/P2；仅 1 条可选 UX P2（补选 `session_busy` 上下文文案）；代码/E2E/证据未完成 |
| 2026-10-23 | 用户确认 | **通过（文档侧）** | 补选 `session_busy` 使用上下文文案「当前任务仍在运行，暂不能补选供应商/模型，请等当前任务结束后重试。」，不复用队列满文案；用户确认后标记通过；代码/E2E/证据与共享 token 签署另行跟踪 |

## 10. 复审整改承接（v0.9）

> 编号按 `docs/ADR-028-复审意见-v0.8.md` §2 与 `docs/ADR-028-复审意见-v0.9.md` §3；文档侧无阻塞性 P0/P1/P2（见 §9），本表记录 v0.9 编辑性 P2 清理及用户补选 `session_busy` 文案裁定。
>
> **历史行说明**：本表保留 v0.5/v0.6/v0.7 等历史整改记录（如“14 行真相表”“2×P1”），均按当时版本记录，不代表当前口径；当前行为以 v0.9 正文为准。

| 来源/项 | 整改要点 | 文档落点 | 状态（截至 v0.9） |
|---|---|---|---|
| P0-1 旧会话补选 | 采用 `session_send.provider_id`（+ 必要时 `model`），仅对无 `provider_id` 的旧会话生效；校验后持久化；不新增命令；补正向 E2E | §2.1-4、§3-4、§5.1、§7-3/6 | 文档已改；**代码/E2E 待整改** |
| P1-1 目录查询协议 | adapter→core 请求 id 命名空间/待决表/coalesce/超时/断连清理；`-32601/-32602/-32603` + `error.data.aether_code`；不新增错误码 | §2.1-6、§3-6、§5.1、§7-4 | 文档已改；**协议一致性测试待补** |
| P1-2 静态能力 UI 通路 | `runtimes_list.capabilities` = hello ∪ 静态表；UI 不依赖 hello/ready；cold/connecting 仍显示供应商选择 | §2.1-4、§5.1、§5.4、§7-2/6 | 文档已改；**代码/UI 测试待补** |
| P1-3 D6 判定时点 | 除 `multi_provider` 外，其余能力 ready 后判定；legacy 路径仅 P3/非官方/历史回退 | §2.1-4、§5.1、§2.2 | 文档已改；**代码待整改** |
| P1-4 设计 §5 退出标准 | 改为“门禁任务（M5–M8 除 §8 直接实施项）全部 DoD 通过” | §5.1 | 文档已同步 |
| P1-5 附录 A UI-09 | 补三运行时多供应商、`provider.catalog.get`、全局记忆、失效只读、ADR-028 v0.9/M7-10；§2.3 注补多供应商 | §5.1 | 文档已同步 |
| P1-6 `model` 省略 | 三官方运行时设置 `provider_id` 时 `model` 必填；旧会话补选若 `sessions.model` 为空则 `model` 必填；UI 未选模型禁止创建 | §2.1-4、§3-2、§5.1-5.4、§7-3/6 | 文档已改；**代码/UI/E2E 待整改** |
| P1-7 只读态中断 | 只读态 `session_interrupt` 保持可用（仅当前在途 run）；发送/重试/恢复禁用 | §2.1-4、§5.4、§7-6 | 文档已改；**UI/E2E 待补** |
| P1-8 OpenAI `/v1` | 设计 D10 补内置 OpenAI `base_url` `/v1` 归一化；0003 seed 不改；补 `{}` 用例 | §2.1-3、§5.1、§7-1 | 文档已改；**代码/测试待补** |
| P2-1 ADR-018/010 状态 | 标注 ADR-018 v0.3 / ADR-010 v0.10 为 ADR-028 横切登记，随复审/合入，不单独批准 | ADR-018/ADR-010 状态行 | 文档已同步 |
| P2-2 设计基线措辞 | 设计头部已标注“设计基线已冻结；实现/验收待代码/E2E/证据闭合”（当前 v1.24 延续） | 设计头部 | 文档已同步 |
| P2-3 需求状态 | 需求头部已标注“已同步（2026-10-23）”（当前 v0.18 延续） | 需求头部 | 文档已同步 |
| P2-4 `verify:m7-10` | 实现时必须新增；若用等价命令，证据文件记录 | §7-7、P1 计划 DoD 8 | 文档已改；**脚本待实现** |
| P2-5 M8-03 索引 | M7-10 证据归档后补入 M8-03 直接实施项索引；M8-04 发布说明补已知边界 | P1 计划 M8-03/M8-04 | 文档已改；**证据待补** |
| P2-6 legacy 路径 | 明确 legacy 仅 P3/非官方/历史回退；P1 官方不使用自动选择 | §2.2、§3、设计 D6/D10 | 文档已改 |
| P2-7 认证方式 | 认证方式为派生字段（`protocol → auth_env`），不进入目录响应 | §2.1-1、§2.1-6 | 文档已改 |
| P2-8 超限错误帧 | >2MiB 返回 `-32603` + `aether_code=core_not_ready`；不截断/不断连；适配器按失败处理；记录目录规模/延迟 | §2.1-6、§3-6、§7-4 | 文档已改；**协议测试/证据待补** |
| P2-9 自动路由 + provider | 自动路由时 provider 按至少一个候选运行时协议过滤；无候选 `no_candidate_runtime`；不兼容 `invalid_value` | §2.1-4、§5.4、§7-6 | 文档已改；**测试待补** |
| **P1-1（v0.5 复审）旧会话补选适配器语义** | Claude per-run env；Codex 首次补选创建 per-session `CODEX_HOME` 并复制/采用 thread 状态；DSH 复制/采用会话状态到 per-session profile；准备成功才持久化，失败不持久化/不重建/保持只读；dispose 清理 | §2.1-4、§2.1-5、§3-4、§7-6 | 文档已改；**三运行时 E2E 待补** |
| **P1-2（v0.5 复审）v0.2 model 空会话** | `provider_id` 有、`model` 空时允许一次性 `session_send.model` 补选；model 失效同理；已选且 model 有效再传 → `invalid_value` | §2.1-4、§3-4、§5.1/5.2、§7-3/6 | 文档已改；**样本测试待补** |
| P2-1（v0.5 复审）补选真相表 | `provider_id × model × sessions.model` 14 行真值表，允许/拒绝/错误码明确 | §2.1-4 | 文档已改；**E2E 逐行待补** |
| P2-2（v0.5 复审）`model_state` | `SessionSummary.model_state`（active/invalid/missing）+ UI 提示/补选 | §2.1-4、ADR-010 B.4、设计 D7、UI-UX §2.4/§7.3 | 文档已改；**代码/UI 待补** |
| P2-3（v0.5 复审）D10 扫描口径 | 改为“非测试源码/运行产物 0 命中；测试代码/合成 key 白名单除外” | 设计 D10 | 文档已同步 |
| P2-4（v0.5 复审）model 失效 | model 停用/删除后可一次性补选 model；不按 provider 失效处理 | §2.1-4、§3-5、§7-6 | 文档已改；**测试待补** |
| P2-5（v0.5 复审）verify/M8-03 | `verify:m7-10` 实现时新增；M7-10 证据归档后补 M8-03 索引 | §7-7、P1 计划 M8-03 | 文档已改；**实现/证据待补** |
| P2-6（v0.5 复审）D10 措辞 | “keychain 写入不在 provider 命令内注入；消费映射见下一条” | 设计 D10 | 文档已同步 |
| **P1-1（v0.6 复审）真相表穷尽性** | 统一 21 行穷尽表 + 未列组合默认 `invalid_value`；覆盖 provider 空/有/失效 × model 空/有效/失效 × send provider/model 有/无；ADR 与设计同一份表；补第 6/12 行、provider 空+model 失效、无 provider 正常 send | §2.1-4、§3-4、§5.1、§7-3/6 | 文档已改；**E2E 逐行待补** |
| P2-1（v0.6 复审）model_state 不属于 | `model_state=invalid` 覆盖“不属于该 provider”；补旧数据样本测试 | §2.1-4、§3-5、ADR-010 B.4、设计 D7、UI-UX §2.5/§5.1 | 文档已改；**测试待补** |
| P2-2（v0.6 复审）状态复制约束 | Codex/DSH 仅无在途 run/空闲复制；独占锁；锁/失败清理；真实 CLI 待验证，不当已验证事实 | §2.1-5、§5.1、§7-6 | 文档已改；**真实冒烟/SKIP 待补** |
| P2-3（v0.6 复审）verify/M8-03 | `verify:m7-10` 实现时新增；M7-10 证据归档后补 M8-03 索引 | §7-7、P1 计划 M8-03 | 文档已改；**实现/证据待补** |
| P2-4（v0.6 复审）量级 | M7-10 暂定 L；动工前复核，超出 L 按计划规则调整并记录证据 | §5.3、P1 计划 §1.6 | 文档已改；**动工前复核** |
| P2-5（v0.6 复审）准备成功/run 失败 | 准备成功 = 配置物化 + 原生状态采用 + ack；run 失败不回滚已持久化 provider/model，按失效/重试处理 | §2.1-5、§5.1、§7-6 | 文档已改；**E2E 待补** |
| P2-1（v0.7 复审）版本标签 | 统一至 v0.8/v1.23/v0.17/v0.12/v0.13；ADR-010 v0.10；证据对照覆盖 v0.6 复审 1×P1+5×P2 与 v0.7 复审 P2×6 | 全文、§5、§7-7、§10 | 文档已清理 |
| P2-2（v0.7 复审）provider 空 model_state | provider 空时任一启用 provider 存在 → active，否则 invalid；选中 provider 后再按归属复核（历史口径；v0.9 已补“存在且启用”） | §2.1-4、§3-5、ADR-010 B.4、设计 D7、UI-UX §2.5 | 文档已改；**样本测试待补** |
| P2-3（v0.7 复审）UI 优先级 | provider 失效 → 只读，禁用所有补选（含 model）；仅 provider 有效且 model 空/失效/不属于才显示 model 补选 | §2.1-4、UI-UX §2.4/§5.1 | 文档已改；**UI/E2E 待补** |
| P2-4（v0.7 复审）补选并发 | 补选仅空闲/无在途 run；有在途 run → `session_busy`（不排队） | §2.1-4、§2.1-5、§3-4、§7-6 | 文档已改；**并发 E2E 待补** |
| P2-5（v0.7 复审）verify/M8-03/L | `verify:m7-10` 实现时新增；M8-03 证据归档后补索引；M7-10 暂定 L/动工前复核 | §7-7、P1 计划 M8-03/§1.6 | 文档已改；**实现/证据待补** |
| P2-6（v0.7 复审）共享 token 签署 | 正式发布前产品/安全签署；拒绝则按 §2.3 转 per-adapter ADR | §2.3、§9 | **待签署** |
| P2-1（v0.8 复审）provider 空 `model_state` | 改为“model 在任一启用 provider 中**存在且启用** → active，否则 invalid”；补 model 停用样本（不得判 active） | §2.1-4、§3-5、§7-6/7、ADR-010 B.4、设计 D7、UI-UX §2.5 | 文档已改；**停用 model 样本待补** |
| P2-2（v0.8 复审）UI-UX 锚点日期 | §7.3 `session-provider-select` 行日期由“2026-10-23 v0.11”改为 v0.14 | UI-UX §7.3 | 文档已改 |
| P2-3（v0.8 复审）§10 历史行 | §10 顶部注记“历史行按当时版本，当前口径以 v0.9 为准”，避免 14 行/2×P1 等历史记录被误读 | §10 顶部 | 文档已改 |
| 用户裁定（v0.9）补选 `session_busy` 文案 | 补选遇在途 run → `session_busy`；UI 使用「当前任务仍在运行，暂不能补选供应商/模型，请等当前任务结束后重试。」，不复用队列满文案 | §2.1-4、§5.4、§7-6、UI-UX §2.4/§5/§5.1、P1 DoD 7/8 | 文档已改；**UI/E2E 待补** |
| P0-2 共享 token 签署 | 正式签署/转 per-adapter ADR | §2.3、§9 | **待签署** |
| 代码/E2E/证据 | 三运行时应用级 E2E + `docs/M7-10-证据.md` + `verify:m7-10` | §7-6/7 | **待补** |

> 复审口径：按 `docs/ADR-028-复审意见-v0.8.md` §4 与 `docs/ADR-028-复审意见-v0.9.md` §3 逐项核对；文档侧已通过（2026-10-23 用户确认补选 `session_busy` 上下文文案）；实现/验收仍按 §7 执行，未闭合前不得声称实现验收通过。
