# ADR-028：多供应商目录、协议端点与按需凭据代理（D6/D7/D10；M7-10）

| 项 | 内容 |
|---|---|
| 编号 | ADR-028 |
| 版本 | **v0.2（Phase 1 实现完成）** |
| 状态 | **已通过（2026-10-22，用户确认设计）；Phase 1 已实现（2026-10-22）**；Codex/DSH per-session 为 Phase 2/3 |
| 决策日期 | 2026-10-22 |
| 决策载体 | 《设计文档》v1.17 → v1.18（D6/D7/D10 增量）；`docs/P1-实施计划与验收标准.md` M7-10（任务进度，结构变更随本 ADR）；迁移 `0009_provider_endpoints.sql` |
| 关联 | D6/D7/D10、ADR-006（错误码）、ADR-008（三运行时）、ADR-010（供应商/密钥）、ADR-014（命令面）、ADR-015（运行时注册）、ADR-024（供应商消费映射）、M7-06/M7-09/M7-10 |
| 取代 / 被取代 | 扩展 ADR-024 的供应商选择与注入规则；不改 ADR-024 的密钥只经 env、不落盘原则 |
| 回退条件 | 见 §6 |
| 实现状态 | Phase 1 已落地：端点层、`session_create.provider_id`、Credential Broker、Claude Code `multi_provider` + per-run env；Codex/DSH 待后续增量 |

## 1. 背景

- 用户要求：**同一个 Agent（如 Claude Code）可以同时搭配多个供应商**（Anthropic / DeepSeek / 其他 Anthropic 兼容网关），而不是切换供应商必须重启运行时。
- 现有实现（ADR-024）：`ProviderEnvHook` 按运行时类型选一个 provider，在适配器 **spawn 时**把该 provider 的密钥注入适配器进程 env。一个适配器进程只有一套供应商环境，无法同时服务多个 provider。
- 现有 provider 只有 `type + base_url`；主流供应商可能同时提供 OpenAI 格式与 Anthropic 格式端点（例如 DeepSeek：`https://api.deepseek.com` 与 `https://api.deepseek.com/anthropic`）。Claude Code 必须使用 Anthropic 端点，Codex 使用 OpenAI 端点。
- 若把全部 provider 密钥一次性注入每个适配器，会扩大泄露面（适配器崩溃/内存 dump/子进程 env 继承均可暴露全部 key）。需要把“目录元数据”和“密钥”分离。

## 2. 决策

### 2.1 核心决策

1. **ProviderCatalog（非机密单例）**
   - 核心维护唯一的供应商目录：provider id/name/vendor/enabled、协议端点（`providers.endpoints`）、模型清单、认证方式；**不含密钥值**。
   - 所有官方适配器可读取同一份目录（spawn 时注入非机密快照，或经非机密控制 RPC 查询）。
   - 目录变更（新增 provider/端点/模型）不要求重启适配器；密钥永远按需获取。

2. **Credential Broker（按需密钥单例）**
   - 核心在本地启动凭据 broker（Windows named pipe / Unix domain socket；仅本机、仅当前用户）。
   - 适配器 spawn 时获得 broker 地址 + 一次性 token（`AETHER_CREDENTIAL_BROKER`，仅含地址/token，不含供应商密钥）。
   - 适配器在每次 run 启动时按 `provider_id + protocol` 请求凭据；broker 校验 token/适配器身份/provider 启用状态后，从 OS 凭据库或 A3 加密文件读取**该一家** provider 的 key，返回 `{base_url, api_key, auth_env}`。
   - 适配器只把该 key 放入本次 CLI 子进程 env，启动后即丢弃；不缓存全部 key。
   - broker 审计只记录 `provider_id/runtime_id/protocol/结果`，不记录密钥；密钥不进入日志、诊断包、导出、测试夹具。
   - 仅官方适配器可获得 broker token；第三方适配器（P3 沙箱后）另行评审。

3. **协议端点（`providers.endpoints`）**
   - 迁移 `0009_provider_endpoints.sql` 新增 `providers.endpoints TEXT NOT NULL DEFAULT '{}'`，JSON：`{"anthropic":"…","openai":"…","google":"…","dsh":"…"}`。
   - `type` 仅表示厂商标识；运行时按协议选择端点：
     - Claude Code → `endpoints.anthropic`
     - Codex → `endpoints.openai`
     - DSH → `endpoints.dsh`（或 DSH profile 端点）
   - 端点缺失/不可用时，创建会话前返回结构化错误（P1 先以 `invalid_value` + 稳定 message；新增专用错误码须另按 ADR-006 登记）。
   - 内置/主流供应商预设端点由 UI 预填（DeepSeek/Anthropic/OpenAI/Google 等）；用户只需填 Key，可覆盖端点。
   - 一个 provider 暂只保存一把 Key（ADR-028 v0.1；DeepSeek 已验证同一 key 同时服务 OpenAI/Anthropic 端点）。若未来网关要求每协议独立 Key，再扩展 `endpoints` 项结构。

4. **会话级 provider 选择**
   - `session_create` 新增可选 `provider_id`（D6 跨版本只增字段）；核心校验 provider 存在/启用、model 属于该 provider 且启用、运行时协议端点存在。
   - `provider_id` 随 `session.create` 下发适配器，并持久化到 `sessions.config.aether.provider_id`（供重试/恢复/审计）。
   - 缺省 `provider_id` 沿用现有单供应商路径（向后兼容）。
   - 模型选择器按 provider 分组；同一模型 ID 出现在多个 provider 时，必须以 `provider_id` 消歧。

5. **Claude Code 先行（M7-10 Phase 1）**
   - Claude Code 适配器声明 `multi_provider` 能力；session 记录 `provider_id`；每次 run 向 broker 取 Anthropic 协议凭据，按 run 覆盖 `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY`（必要时 `ANTHROPIC_AUTH_TOKEN`）。
   - 两个并发会话可使用不同 provider（已用假 CLI spike 验证 per-run env 可切换）。
   - Codex/DSH 的 per-session provider 接入列为 Phase 2/3，需各自验证配置物化/ACP profile 语义，不在本 ADR 首次实现内。

6. **刷新与重启**
   - 目录/端点变更可即时生效（适配器查询目录）；
   - broker 每次读取最新 provider/key，密钥轮换无需重启适配器；
   - 若适配器持有旧的非机密目录快照，提供控制 RPC 或在下一次会话创建时刷新；不得要求重新注入全部密钥。

### 2.2 明确不做

- 不把全部供应商密钥注入所有适配器进程；
- 不让适配器直接读取 OS 凭据库/A3 加密文件；
- 不把密钥写入任何配置文件、日志、诊断包、导出或测试夹具；
- 不新增事件类型；不改 D9 权限门；不改 D4 管线；
- 不在本期实现 Codex/DSH 的 per-session provider 切换（后续增量）。

## 3. 硬约束

1. broker 只监听本机（named pipe/Unix socket）；token 每次应用启动随机生成，仅注入官方适配器 env。
2. broker 返回的 key 只允许进入当前 run 的 CLI 子进程 env；不得写入适配器持久状态。
3. 任何日志/诊断/导出/夹具扫描 `sk-`/`eyJ`/PEM 必须 0 命中（D10 既有守门）。
4. `providers.endpoints` 只存 URL，不存密钥；URL 校验 http(s) 且控制字符拒绝。
5. 缺少 provider/端点/模型归属时，命令层在创建会话前拒绝；不得把错误推迟到 CLI/API 层。
6. 现有单供应商路径（无 `provider_id`）必须保持兼容；既有测试与 M3-11/M7-06 行为不回归。
7. 官方适配器白名单/注册表不变；不新增 runtime_id，不改 ADR-015 准入模型。

## 4. 实现落点

| 位置 | 内容 |
|---|---|
| `migrations/0009_provider_endpoints.sql` | `providers.endpoints` JSON 列（已落地） |
| `crates/aether-store/src/ops.rs` | `ProviderRecord.endpoints` + insert/update/select（已落地） |
| `crates/aether-tauri/src/ipc/dto.rs` | provider endpoints + `session_create.provider_id`（已落地，含 ULID 校验） |
| `crates/aether-tauri/src/provider_control.rs` | endpoints 读写/回显（已落地） |
| `crates/aether-tauri/src/provider_env.rs` | 端点回退消费 + broker 地址/token 注入 + `resolve_credential`（已落地） |
| `crates/aether-tauri/src/credential_broker.rs` | 本地 named pipe/Unix socket broker、一次性 token、按需读取（已落地） |
| `packages/adapter-sdk/src/credential.ts` | broker 客户端（已落地；不缓存/token 不落盘） |
| `packages/adapter-claude-code/src/claude-adapter.ts` | `provider_id` + per-run provider env + `multi_provider`（已落地） |
| `apps/desktop/src/providers.ts` / `ProvidersPage.tsx` / `ModelSelector.tsx` / `SessionWorkbench.tsx` | 端点预设/表单 + 模型选择器 provider 归属 + 新建会话透传（已落地） |
| `docs/P1-实施计划与验收标准.md` M7-10 | 任务与 DoD（已登记，Phase 1 完成） |

## 5. 影响

- 设计文档 v1.17 → v1.18：D6 增加 `session_create.provider_id` 可选字段；D7 增加 `providers.endpoints` 语义；D10 增加凭据 broker 与 per-run 密钥边界；§5 P1 段登记 ADR-028/M7-10；附录 C 追加迁移 0009。
- P1 计划：新增 M7-10（直接实施项，不构成 P1 门禁）。
- 迁移：`0009`（新增列，向后兼容；既有库 `{}` 回退）。
- 命令面：`provider_create`/`provider_update` 增加可选 `endpoints`；`session_create` 增加可选 `provider_id`；不新增命令。
- UI：供应商表单双协议端点 + 预设；模型选择器按 provider 分组并在新建会话时显式透传 `provider_id`（仅 `multi_provider` 运行时）；无密钥展示。
- 兼容：未传 `provider_id` 的旧调用继续走原单供应商 env（`ProviderEnvHook` 按协议端点选择 provider）；Codex/DSH 显式传 `provider_id` 会被拒绝（能力未声明），自动路由/旧调用不受影响。

## 6. 回退条件

1. broker 导致任何密钥落盘/日志/诊断包泄露 → 立即停用 broker，回退到单供应商 env 注入并复盘。
2. 官方适配器无法稳定获取凭据或并发会话串 provider → 回退 `multi_provider` 能力，保留端点字段与单供应商路径。
3. 端点/模型校验导致既有单供应商路径回归 → 回退 `session_create.provider_id` 强制校验为可选旁路，保留端点字段。
4. 新错误码/协议语义需求 → 先按 ADR-006 登记，不得自造。

## 7. 验证与 DoD

1. 迁移 0009 + `providers.endpoints` 读写/回显；既有库 `{}` 回退（`m3_11_providers`）。
2. UI 预设端点（DeepSeek 双端点等）可预填/覆盖；用户只填 Key（前端 255 项 + `tsc`/`build`）。
3. `session_create.provider_id` 校验（provider/model/endpoint/能力）与单供应商兼容（`session_backend::tests::provider_id_validates_model_endpoint_and_capability`）。
4. broker 本地通道 + token 校验 + 按需读取；日志/诊断/导出 0 密钥（`m7_10_credential_broker`；broker 日志不含密钥）。
5. Claude Code 两个并发会话不同 provider 的 E2E：适配器级已用本地 mock Anthropic 上游验证 per-run `base_url`/key 隔离（`claude-adapter.test.ts`「concurrent providers」）；完整应用级 + 真实 DeepSeek 为可选本地冒烟（需用户自备 key，不共享）。
6. 回归：M3-11/M7-06/会话创建/设置测试全绿；`cargo clippy --workspace -- -D warnings`、前端 `tsc`/`vitest`/`build`。

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-10-22 | 创建：ProviderCatalog + Credential Broker + `providers.endpoints` + `session_create.provider_id`；Claude Code 先行；端点层已落地 | 文档维护（AI） |
| v0.2 | 2026-10-22 | Phase 1 实现完成：`session_create.provider_id`（DTO/校验/持久化/下发）、本地 broker（named pipe/Unix socket + token + 审计日志）、adapter-sdk 客户端、Claude Code `multi_provider` per-run env、UI provider 归属与透传；补并发 mock 上游 E2E | 文档维护（AI） |
