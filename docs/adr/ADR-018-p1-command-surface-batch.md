# ADR-018：P1 命令面 / 错误码增量登记（D7 批次汇总；P1-A03）

| 项 | 内容 |
|---|---|
| 编号 | ADR-018（P1-A03） |
| 版本 | **v0.3（ADR-028 v0.4/v0.5 横切增量）**：v0.1 创建（批次稿）；v0.2 按 `docs/M5-01-ADR批次评审意见.md` 修订闭环；v0.3 增 settings 键 `ui.provider_selection`（设置键 8 → 9，不改命令/错误码） |
| 状态 | **已批准（2026-10-09；人工授权 AI 审批——独立评审通过即批准，见 `docs/M5-01-ADR批次评审意见.md` §7）**；已合入《设计文档》v1.16；**v0.3 为 ADR-028 v0.4/v0.5 的横切登记，随 ADR-028 复审/合入，不单独批准**（2026-10-22/2026-10-23） |
| 决策日期 | 2026-10-09；v0.3 增量 2026-10-22 |
| 决策载体 | 《设计文档》v1.15 → v1.16（D7 命令面 52 → 63、命令参数校验矩阵增量）；ADR-006 附录 B v0.7 → **v0.8**（错误码全集 23 → 32、登记行 11 → 20）；`docs/P1-实施计划与验收标准.md` v0.7（§9 行标注）；**ADR-028 v0.4/v0.5（设置键 `ui.provider_selection`，8 → 9）** |
| 关联 | 设计 D7、附录 A/B/C；ADR-004/006/007/009/010（命令面先例）、ADR-014 附录 C-1（15 命令基线）、ADR-015 附录 C（52 命令 / 全集 23 基线）；本批次 ADR-017/019/020/021/022/023/024/025/026 |
| 取代 / 被取代 | 无（新增；**登记边界**见 §1.2） |
| 回退条件 | 见 §6 |
| 实现状态 | 未开始（登记先行；命令分别由 M6-01/M6-03/M5-05 执行） |

## 1. 背景

### 1.1 触发

P1 计划 §9 P1-A03：「D7 P1 命令面增量登记（备份/保留/规则/导出/路由/审计等命令 + 错误码 + 警告码 + 事件类型如有）」；解锁 M5-02…M5-05、M6-01、M6-03、M7-02、M7-03。ADR-015 附录 A-4 已关闭 **P1-A03 双登记边界**：热插拔命令/错误码由 ADR-015 登记、P1-A03 不重复；计数以 ADR-015（52 命令 / 全集 23）为基线。

### 1.2 登记边界（本批次所有权划分）

| 主题 | 登记 ADR | 说明 |
|---|---|---|
| pause/resume 命令与错误码 | **ADR-017** | `session_pause` / `session_resume` / `pause_unsupported`；本 ADR 汇总计数，不重复逐条登记 |
| 规则 / 导出 / 归档命令 | **本 ADR（ADR-018）** | `rules_*`、`session_export`、`archive_export`、`audit_query`、`audit_export` + `rule_not_found` |
| 备份 / 保留 / 路由 命令 | 本 ADR 负汇总 | 三项主题**不新增命令**（设置键 + 既有命令响应增量；见 ADR-020/021/022）；错误码 `no_candidate_runtime`（ADR-022）、`provider_error`（ADR-024） |
| 供应商消费映射 / 连通性命令 | ADR-014（既有登记） | `provider_test` / `provider_models_fetch` 已在 ADR-014 C-1 登记；ADR-024 只补语义与投影 |
| 会话检索与管理 / 工作区 / 预设命令 | ADR-014（既有登记） | `session_search` / `session_rename` / `session_delete` / `workspaces_*` / `presets_*` 已在 ADR-014 C-1 登记；ADR-025/026 只补语义、错误码与迁移 |
| 热插拔命令与错误码 | ADR-015（既有登记） | 不重复 |

## 2. 决策

### 2.1 核心决策（五段式）

- **决策**：以 ADR-015（52 命令 / 全集 23 / 登记行 11）为基线，本批次登记 **+11 命令**（ADR-017 两条 + 本 ADR 九条）与 **+9 错误码**（ADR-017/018/022/024/025/026 分属），**不新增警告码、不新增事件类型**；命令面无旁路：所有新增命令纳入 M1-08 参数校验框架（`deny_unknown_fields` + 类型/枚举/长度/路径校验，失败返回结构化错误且不落库/不产生事件）。
- **实现要点**：
  1. 新增命令（本 ADR 九条）与参数见附录 A-1；命令面合计 **52 → 63**；
  2. 错误码增行逐行文本见附录 A-2（本 ADR 一条）+ 各归属 ADR 附录（汇总台账见附录 A-3）；全集 **23 → 32**、登记行 **11 → 20**；
  3. 既有命令响应/参数增量（不新增命令）：`backup_create`/`backup_restore` 可选 `passphrase`（ADR-020）、`backup_list` 增 `auto` 段（ADR-020）、`session_create.runtime_id` 改可选（ADR-022）、`session_send` 响应可选 `queue`（ADR-022）、`health` 增 `concurrency_gate`（ADR-022）、`session_create` 可选 `preset_id`（ADR-026）、`SessionSummary` 增 `preset_id/preset_name`（ADR-026）；均属 minor 增量，线协议 major 不变；**设置键增行台账（M5-01 DoD5② 横切承接，含 M7-04 布局持久化 `ui.layout`）见附录 A-6**；
  4. 生成物：所有定型 DTO 变更命令须在实现任务内复跑 `pnpm --filter @aether/protocol generate` 与 `check`（AGENTS §2.8 / T14）；本 ADR 为登记先行，**不改 bindings.ts**；
  5. 迁移：本 ADR 不新增表；`permission_rules`（ADR-019）、检索 FTS（ADR-025）、`presets`（ADR-026）按附录 C 规划位由各任务落库；
  6. 事件类型：不新增；新增发射点见 ADR-017/025（均为附录 B 既有类型）。
- **主要风险**：命令面膨胀（63 条）带来文档/契约漂移；错误码命名重复或语义重叠；某命令被双登记（计数漂移）；参数校验遗漏导致「未登记路径可写」。
- **缓解措施**：单一所有权表（§1.2）+ 台账（附录 A-3）+ M5-01 验证脚本对计数与命名做静态核对；错误码语义去重（能复用既有码的不新增）；全部新命令纳入 M1-08 畸形参数矩阵（实现任务 DoD）。
- **失效条件**：命令面出现**同一能力双命令**或**错误码自造**（不经 ADR-006 附录 B）→ 该命令/码作废重登记；计数台账与设计文档不一致且一个版本内未修复 → M5-01 验证红，不得合入实现。

## 3. 影响

### 3.1 《设计文档》v1.15 → v1.16

- D7：命令面计数行更新为 **52 → 63**；新增命令清单（本 ADR 附录 A-1 + ADR-017 附录 A-1）；命令参数校验清单补九条；登记边界表转写为「计数台账见各 ADR」。
- ADR-006 附录 B：按附录 A-2/A-3 增行至 **全集 32 / 登记行 20**（v0.8 变更记录）。
- 附录 B 事件表：注明「P1 批次不新增事件类型；新增发射点 = `session.status_changed`（暂停/恢复）、`session.updated`（改名）、`session.closed`（删除）、`runtime.status_changed`（热插拔）」。

### 3.2 `docs/P1-实施计划与验收标准.md` v0.6 → v0.7

- §9 P1-A03 行标注正式编号 **ADR-018（2026-10-09）**；§10 第 4/5/7 项闭合落点同步（ADR-020/021/022）。

### 3.3 不影响的

- 不新增依赖；不改迁移 0001–0003；不改线协议 major、帧上限（2MiB / `artifact_ref` <1MiB）；不改 P0 T1–T15 与 Gate 1–4。

## 4. 版本（拟合入）

| 文档 | 修订前 | 修订后 |
|---|---|---|
| 《设计文档》 | v1.15 | **v1.16** |
| ADR-006 | v0.7 | **v0.8**（附本文件合入文本） |
| 《P1-实施计划与验收标准》 | v0.6 | **v0.7** |

## 5. 后续（未决项）

1. `session.delete` 类 D6 方法集（运行期上下文管理）维持 P2（P1 计划 §10 第 9 项）。
2. 命令面在 P2 的承载迁移（aetherd）不在本 ADR。

## 6. 回退条件

1. **本 ADR 被否决** → 九条命令分别随其归属任务撤回；错误码 `rule_not_found` 撤回；计数回落至「52 + ADR-017 是否通过」。
2. **某命令实现中发现语义重复** → 先停实现、回报并另立 ADR 修订，不得并存两条命令。
3. **新错误码/警告码需求** → 先按 ADR-006 附录 B 登记（增行），不得自造。
4. **计数漂移**（设计文档 / ADR-006 / 本 ADR / 验证脚本任一处不一致）→ 以 ADR-006 附录 B 与设计 D7 为准修正本 ADR 与脚本；修正前不得合入对应实现。

## 7. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-10-09 | 文档维护（AI） | v0.1 创建 | 按 P1-A03 主题与 ADR-015 A-4 边界；登记先行，代码未改 |
| 2026-10-09 | 独立评审（M5-01 批次，`docs/M5-01-ADR批次评审意见.md`） | 通过（含修订闭环） | 批次复核 17/17 闭环；本 ADR 相关条目见该意见 §6。未另行修改决策 |

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-10-09 | 创建：九条命令 + 计数台账 + ADR-006 v0.8 合入文本；所有权边界表 | 文档维护 |
| v0.2 | 2026-10-09 | 批次评审闭环（不改决策）：按 `docs/M5-01-ADR批次评审意见.md` 修订（含该意见 §6 闭环核验所列条目） | 文档维护 |
| v0.3 | 2026-10-22 | ADR-028 v0.4/v0.5 横切增量：附录 A-6 增 `ui.provider_selection`（string JSON ≤64KiB，全局按运行时映射 provider id），设置键 8 → 9；不改命令/错误码/迁移；随 ADR-028 复审/合入，不单独批准 | 文档维护 |

---

## 附录 A：命令 / 错误码登记

**A-1 新增命令清单（本批次 +11：ADR-017 +2 / 本 ADR +9；命令面 52 → 63）**

| # | 命令 | 参数（严格解析） | 校验矩阵要点 | 响应/语义归属 |
|---|---|---|---|---|
| 53 | `session_pause` | 见 ADR-017 附录 A-1（**归属 ADR-017，本表仅作计数指针**） | — | ADR-017 |
| 54 | `session_resume` | 见 ADR-017 附录 A-1（**归属 ADR-017，本表仅作计数指针**） | — | ADR-017 |
| 55 | `rules_list` | 无（`null`/缺省/空对象合法） | 无参数 | ADR-019 |
| 56 | `rule_add` | `{ name, resource, action, target_kind, target?, decision, priority? }` | `name` ≤64 字符非空；`resource` ∈ `fs.read/fs.write/exec/net`；`action` 与 resource 匹配（`read/write/exec/connect` 枚举，未知 → `invalid_enum`）；`target_kind` ∈ `path/command/host/any`；`path` 类 target 须 canonicalize 校验形态（不做存在性）；`command` 类 ≤256 字符；`host` 类域名/通配后缀校验；`decision` ∈ `allow/deny`；`priority` 0–100（缺省 0，越界 `out_of_range`） | ADR-019 |
| 57 | `rule_update` | `{ rule_id, name?, resource?, action?, target_kind?, target?, decision?, priority? }` | `rule_id` 存在（否则 `rule_not_found`）；至少 1 个变更字段；字段校验同 `rule_add`；`resource` 变更须同时提供匹配的 `action` | ADR-019 |
| 58 | `rule_delete` | `{ rule_id }` | `rule_id` 存在（否则 `rule_not_found`）；删除前审计 | ADR-019 |
| 59 | `rule_toggle` | `{ rule_id, enabled: boolean }` | `rule_id` 存在；`enabled` 非布尔 → `invalid_type` | ADR-019 |
| 60 | `session_export` | `{ session_id, target_dir? }` | ULID；`target_dir` 经 Rust 侧选择器/路径校验 + 可写 + 空间检查（M3-04 同口径，不足 `invalid_value`）；脱敏扫描 0 命中；原子写（临时文件 + rename） | ADR-021（DS-04 归档路径） |
| 61 | `archive_export` | `{ target_dir? }` | 同 `session_export` 路径口径；全库归档（events 全量 + messages 终稿 + sessions/runs 摘要） | ADR-021（UI-UX §8#16 一键归档） |
| 62 | `audit_query` | `{ session_id?, actor?, action?, resource?, from_ts?, to_ts?, limit?, offset? }` | `actor` ∈ `user/agent/system`；`action` ≤64 字符、字符集白名单；`from_ts`/`to_ts` 非负且 from ≤ to；`limit` ≤500（缺省 100，越界 `out_of_range`）；`offset` ≥0；未知字段拒绝 | ADR-023 |
| 63 | `audit_export` | `{ target_dir?, session_id?, actor?, action?, resource?, from_ts?, to_ts? }` | 过滤字段同 `audit_query`；路径/空间同 `session_export`；导出 JSONL + manifest（行数/sha256） | ADR-023 |

**A-2 ADR-006 附录 B 本 ADR 增行（逐行文本）**

| code | 语义 | 触发点 | 前端行为 |
|---|---|---|---|
| `rule_not_found` | 权限规则不存在 | `rule_update` / `rule_delete` / `rule_toggle`（按 id 查无） | 刷新规则列表 + 提示 |

**A-3 本批次错误码台账（逐行文本见各归属 ADR 附录）**

| code | 归属 ADR | 语义摘要 |
|---|---|---|
| `pause_unsupported` | ADR-017 | 运行时未声明 `pause_resume`（拒绝暂停，不产生假暂停） |
| `rule_not_found` | ADR-018 | 规则 id 不存在 |
| `no_candidate_runtime` | ADR-022 | 自动路由无可用候选（全部禁用/能力不匹配） |
| `provider_error` | ADR-024 | 供应商返回非授权类错误（4xx/5xx，连通性测试路径） |
| `session_not_found` | ADR-025 | 会话不存在（查询/改名/删除目标） |
| `workspace_not_found` | ADR-026 | 工作区 id 不存在 |
| `workspace_in_use` | ADR-026 | 删除工作区时存在会话引用（不级联） |
| `preset_not_found` | ADR-026 | 预设 id 不存在 |
| `builtin_preset_undeletable` | ADR-026 | 内置预设禁止删除 |

**A-4 计数总账（与 ADR-006 附录 B / 设计 D7 联动）**

| 口径 | 基线（ADR-015） | 本批次增量 | 目标值 |
|---|---|---|---|
| 命令面 | 52 | +11（ADR-017 +2；ADR-018 +9） | **63** |
| 错误码全集 | 23 | +9 | **32** |
| 错误码登记行 | 11 | +9 | **20** |
| 警告码 | 1（`thinking_depth_unsupported`） | +0 | **1** |
| 事件类型 | 附录 B 既有清单 | +0（仅启用/发射既有类型） | 不变 |

**A-5 既有命令增量登记（不新增命令）**

| 命令 | 增量 | 归属 ADR |
|---|---|---|
| `backup_create` | 可选 `passphrase`（口令加密；不落盘、不入日志） | ADR-020 |
| `backup_restore` | 可选 `passphrase`（口令加密产物恢复） | ADR-020 |
| `backup_list` | 响应增 `auto` 段：`{ enabled, frequency, retention, last_run_at, last_result, consecutive_failures, next_run_at }` | ADR-020 |
| `session_create` | `runtime_id` 由必填改**可选**（缺省 = 自动路由；显式 = 手动优先）；可选 `preset_id` | ADR-022 / ADR-026 |
| `session_send` | 响应可选 `queue`：`{ position, limit }`（超闸门排队时） | ADR-022 |
| `health` | 响应增 `concurrency_gate`：`{ limit, active, queued }` | ADR-022 |
| `SessionSummary`（`session_list` 元素） | 增可选 `preset_id` / `preset_name` | ADR-026 |

**A-6 设置键增行台账（M5-01 DoD5② 横切承接；现状 = `backup.reminder` 1 个 → 9 个；v0.3 增 `ui.provider_selection`）**

| 键名 | 类型 | 缺省值 | 语义 | 迁移影响 | 归属 |
|---|---|---|---|---|---|
| `backup.reminder` | bool | `true` | 7 天未备份提醒开关（M3-05，既有） | 无 | M3-05（已登记） |
| `backup.auto.enabled` | bool | `true` | 自动备份总开关 | 无（settings KV） | ADR-020 / M5-02 |
| `backup.auto.frequency` | string（`daily`/`weekly`） | `"daily"` | 自动备份频率 | 无 | ADR-020 / M5-02 |
| `backup.auto.retention` | int（1–100） | `10` | 自动产物保留份数（仅 `kind='auto'`） | 无 | ADR-020 / M5-02 |
| `backup.encryption.enabled` | bool | `true` | 加密备份开关（`true` 且密钥不可用 → 备份暂停并告警） | 无 | ADR-020 / M5-03 |
| `events.retention.enabled` | bool | `false` | 事件保留策略开关（阈值自动置 `true`） | 无 | ADR-021 / M5-04 |
| `events.retention.days` | int（7–365） | `30` | 保留窗口（天） | 无 | ADR-021 / M5-04 |
| `ui.layout` | string（JSON，≤64KiB） | `"{}"` | 工作台布局持久化（M7-04 DoD3 落点 = settings 键；损坏回退默认布局） | 无 | **本批次代登记（M7-04 无独立 P1-A ADR）** |
| `ui.provider_selection` | string（JSON，≤64KiB） | `"{}"` | 全局供应商选择记忆，按运行时映射 provider id（`claude-code` / `codex` / `deepseek-harness`）；预填视为已选；失效不自动清空 | 无（settings KV） | **ADR-028 v0.4（M7-10；2026-10-22 增行）** |

> 实现契约（M5-01 代码落点）：`SETTINGS_KEY_ALLOWLIST` 增行；`settings_set` 按类型校验（bool / 枚举 / 整数区间 / JSON 合法性），失败返回既有结构化码（`invalid_type`/`invalid_enum`/`out_of_range`/`invalid_value`）；`settings_get` 按缺省值回填。**不新增错误码**。

**A-7 稳定业务码口径（P2-4 澄清）**

- P0 先例：部分业务拒绝以 `invalid_value` + message 内的**稳定业务码前缀**表达（如 `session_busy`、`backup_not_found`；`crates/aether-tauri/src/session_backend.rs` / `backup_control.rs` 现状）。
- 本批次沿用：`session_busy`（ADR-025 删除前置 / ADR-026 改派前置）、`session_paused`（ADR-017 交互矩阵）等**不进入 `IpcErrorCode` 枚举、不计入错误码计数**；仅 ADR-006 附录 B 显式登记的错误码计入「全集/登记行」。
- 实现任务不得把上述业务码登记为新错误码；如未来需要前端按类型分支，须走 ADR-006 增行流程。

**A-8 UI-UX §5 文案行登记（P2-1）**

- 本批次 9 个新错误码在 UI-UX v0.7 统一登记文案行（含 `pause_unsupported`、`rule_not_found`、`no_candidate_runtime`、`provider_error`、`session_not_found`、`workspace_not_found`、`workspace_in_use`、`preset_not_found`、`builtin_preset_undeletable`）；`provider_unreachable`/`provider_auth_failed` 文案行已由 ADR-014 应用（本次仅核验存在，不重复登记）。
