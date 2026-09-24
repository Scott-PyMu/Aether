# ADR-010：P0 UI 能力扩展登记（文件面板 / 思考深度 / 模型供应商配置）

| 项 | 内容 |
|---|---|
| 编号 | ADR-010 |
| 状态 | **已批准（2026-09-24 全部文档评审通过）**（评审裁定附 7 项修订，见 §8 评审记录与 §9 变更记录；v0.4 为合入前一致性修订（警告交付口径 / DTO 归属）、v0.5 为合入后维护性修订（决策载体转「已合入」+ 复核 6 项收口）、v0.6 为评审通过收口，均不改决策；**已随批合入**设计文档 v1.10 / 实施计划 v1.17 / 需求文档 v0.7 / UI-UX 规格 v0.2） |
| 决策日期 | 2026-09-24 |
| 决策载体 | **已合入（2026-09-24 随批）**：《设计文档》v1.9 → **v1.10**；《实施计划与验收标准》v1.16 → **v1.17**；《需求文档》v0.6 → **v0.7**；《UI-UX 设计规格》v0.1 → **v0.2**（草案回流已完成，非冻结基线）；《ADR-006》附录 B 增行（错误码 +4）并**新增「警告码」子表**（登记位规则见决策 4 / 附录 B.3） |
| 关联 | 设计文档 D3、D4、D6、D7、D9、D10、D12、§2.3、§4、附录 B/C/E；实施计划 M1-03/M1-06/M1-07/M1-09/M2-01/M2-02/M2-11/M3-01/M3-02/M3-05/M4-04、Gate 3、§1/§6/§7/§8；ADR-003/ADR-004（迁移策略与命令面先例）、ADR-005（payload 可选字段扩展先例）、ADR-006（命令/错误码/警告码登记规则）、ADR-007（`health` 命令登记先例）、ADR-009（响应契约与错误码登记先例） |
| 取代 | 无（增量登记：D6 两个方法新增可选参数与能力项；D7 命令面 +11 条（文件引用 3 + 供应商 7 见决策 4 + 选择器 `ref_pick`；`provider_test` 已随 2026-09-24 评审裁定移出 P0，见决策 3）；错误码 +4、警告码 +1；供应商密钥写入路径随本 ADR 登记（`api_key` 明文仅传输 → 核心写 keyring → 返回 `api_key_ref`）；迁移 0003（3 张表 + `sessions`/`runs` 各 1 列）；不改既有命令参数/校验语义、不改事件类型、不改线协议 major） |
| 被取代 | 无 |
| 回退条件 | 见 §6 |
| 范围对价 | 见 §7（2026-09-24 评审裁定：豁免《设计文档》§4 范围纪律的三项对价登记） |
| 实现状态 | 未开始（本 ADR 为登记先行；实现随 v1.17 新增任务 M3-09 / M3-10 / M3-11 执行） |
| 原型依据 | `deepseek_html_V0.0.1.html`（UI/UX v0.0.1 封板原型）。UI 风格与交互以原型为准；P0 裁剪、边界与未登记元素以本 ADR 决策为准（对照表见附录 G） |

## 1. 背景

UI/UX v0.0.1 封板原型包含三项超出设计文档 v1.9 P0 范围的界面能力：

1. **右侧文件面板**（`文件`/`改动` tab + 添加文件/附加文件夹）——§2.3 明确 MVP 为「单窗口 + 会话列表切换」，工作区文件读写经权限门工具调用，无文件面板；UI-UX 规格 v0.1 §0.2 亦明确「不采纳右侧多标签文件/改动/预览」；
2. **思考深度滑块**（输入区大脑图标 + 5 档滑块）——D6 线协议方法集无对应字段；`runs`/`sessions` 无对应列；
3. **模型与供应商配置**（供应商列表 + 添加/编辑/删除 + 表单 + 输入区模型选择器）——D10 只定义密钥存 OS 凭据库、配置只存引用，无管理 UI；D7 命令面（24 条目 / 25 命令，ADR-004/006/007 全集）无供应商命令。

若不登记，Gate 3 验收会与 §2.3 / D6 / D7 / D10 冲突；若直接实现，则违反「命令面冻结 / 变更走 ADR」（AGENTS §3、UI-UX 规格 C10）。

**登记边界（本 ADR 只解决什么）**：为三项能力登记 P0 口径、数据模型、协议字段、命令面扩展（决策 4）、错误码/警告码、UI 裁剪与验收锚点；**不**登记原型的其余元素（权限选择器、Chat 模式、工作区树、主题/语言、预设、语音/附件等，见附录 G）。本 ADR 通过前不得实现对应代码。

**与既有登记先例的一致性**：命令面扩展走 ADR（ADR-004/006/007 先例）；响应契约扩展走 ADR（ADR-009 先例）；错误码新增走 ADR-006 附录 B 登记规则；payload 可选字段扩展按附录 B「允许新增可选字段」声明（ADR-003/ADR-005 先例）。

## 2. 决策

| # | 决策 | 落地位置 |
|---|---|---|
| 1 | **文件面板登记为 P0 只读引用面板（会话引用持久化）**：`artifacts` 表 + `artifacts_list` / `artifact_add`（canonicalize + 同步盘检测）/ `artifact_remove`；只展示与增删引用，不列目录、不展开树、不预览内容；新增选择器命令 `ref_pick` | D3（`artifacts` 表）、D7（命令面）、D9（不绕过权限门）、附录 C/E；实施计划 M3-09 |
| 2 | **思考深度登记为会话级参数**：档位 0–4（默认 2）；`session.create`/`session.send` 新增可选 `thinking_depth`（线协议与 IPC 同名）；适配器以 capabilities 字符串项 `thinking_depth` 声明支持；`sessions.thinking_depth` + `runs.thinking_depth`（迁移 0003）；不支持时核心忽略 + 非阻断警告码 `thinking_depth_unsupported`，UI 置灰 | D4（payload 扩展）、D6（方法表）、D7（命令参数/响应）、附录 C/E；实施计划 M3-10 |
| 3 | **模型与供应商配置登记为 P0 UI + 数据模型扩展**：`providers` / `provider_models`（迁移 0003，播种 4 条内置供应商）+ 7 条命令（`providers_list` / `provider_create` / `provider_update` / `provider_delete`（内置拒绝）/ `provider_toggle` / `provider_model_add` / `provider_model_toggle`）；**密钥写入路径随本 ADR 登记（2026-09-24 评审裁定）**：表单 `api_key` 明文输入（仅传输），核心写入 keyring（A3 降级走加密文件，M1-07 口径）后返回/落库 `api_key_ref`；编辑态表单显示 `api_key_ref` 只读，可重新输入新密钥覆盖；模型选择器从启用供应商+启用模型派生，经既有 `session.create.model` 透传 | D3/D10（密钥写入路径）、D7（命令面）、附录 C/E；实施计划 M3-11 |
| 4 | **新增 IPC 命令面与错误码**：文件引用类 3 条 + 供应商类 7 条（清单与校验见决策 4）；新增错误码 `builtin_provider_undeletable` / `artifact_path_rejected` / `provider_not_found` / `provider_model_not_found`（ADR-006 附录 B 增行，16 → 20）；**新增警告码命名空间 `warnings[].code` 与登记规则**：`thinking_depth_unsupported` 登记于 ADR-006 附录 B 新增「警告码」子表（非阻断，不入 `IpcErrorCode`；未来新增警告码须走 ADR 在该子表增行）；思考深度类不新增命令（扩展既有 `session_create` / `session_send` 参数） | D7、ADR-006 附录 B（错误码 + 警告码子表）、附录 B/C；实施计划 M3-09/M3-10/M3-11 |

> **命令面计数**：D7 P0 命令面 24 条目 / 25 个可调用命令（ADR-004/006/007 全集）→ 本 ADR 新增 11 条（决策 4 的 10 条 + 决策 1 的 `ref_pick`）→ **35 条目 / 36 个可调用命令**（M4-04 DoD4 计数同步；`ref_pick` 沿用 `startup_pick_target` 先例——Rust 侧选择器，不新增 WebView capability；`provider_test` 已随 2026-09-24 评审裁定移出 P0 命令面，P1 真实连通性测试须先登记命令再开放入口）。
>
> **错误码计数**：ADR-006 登记 3 个、ADR-007 增量 2 登记 1 个、ADR-009 增量登记 1 个 → 全集 16 个（`IpcErrorCode` 枚举取值口径）→ 本 ADR +4 → **20 个**；另新增警告码 1 个（`warnings[].code` 命名空间，登记于 ADR-006 附录 B 新增「警告码」子表，非 `IpcErrorCode`）。
>
> **迁移编号**：0002 已发布（`0002_unique_keys.sql`，ADR-004/005）。本 ADR 新增迁移为 **`0003_p0_ui_extensions.sql`**（不含 PRAGMA/BEGIN/COMMIT/`schema_migrations` 写入，与 0002 契约一致；新库按 0001 → 0002 → 0003 顺序得到一致终态）。

### 决策 1：右侧文件面板登记为 P0 只读引用面板（会话引用持久化）

**P0 范围（逐项）**：

- 面板存在（主工作台右栏，可折叠；断点规则沿用 UI-UX 规格 §2.1：≥1280 展开、960–1279 折叠为抽屉），但**只读**：
  - 子 tab「会话文件」：当前会话的**文件**引用列表（`kind=file`）；
  - 子 tab「项目文件」：当前工作区根路径（未绑定工作区时为空态）+ **目录**引用列表（`kind=directory`，不递归）；
  - **不提供文件浏览**：不列目录、不展开文件夹树、不预览内容、不读文件字节；
- `文件` / `改动` tab：P0 只实现 `文件`，`改动` tab **隐藏**（无入口）；
- `＋` 号菜单：只有 `打开文件`；
- `添加文件`：`ref_pick({ kind: "file" })` → `artifact_add({ session_id, path })`；
- `附加文件夹`：`ref_pick({ kind: "directory" })` → `artifact_add({ session_id, path })`（目录登记为引用，不递归）；
- 删除引用：`artifact_remove({ session_id, artifact_id })`（原型未含删除入口；P0 提供行内删除，避免死引用无法清理——登记为本 ADR 的 UI 增补）；
- 搜索框：仅过滤当前列表（不检索磁盘）；
- 空态：`目录为空`（原型文案）。

**数据承载（会话引用持久化）**：

- 会话引用落 `artifacts` 表（迁移 0003；DDL 见附录 A）：`UNIQUE(session_id, path)`；删除会话级联删除（`ON DELETE CASCADE`）；
- 引用路径经 `artifact_add` **canonicalize + 可访问性检查**（2026-09-24 评审裁定：不复用 A4 同步盘检测——A4 口径面向数据目录完整性（D3），引用仅存路径字符串、无落库数据体，不存在同步盘损坏风险；canonicalize 失败（不存在/不可解析）或 stat 探测失败（不可访问）→ `artifact_path_rejected`；引用不预授权、不做工作区前缀比较，D9 不变）；重复添加同一路径 → 幂等返回既有引用；
- 不产生事件（附录 B 不变）、不落 `events`/`messages`；跨重启保留（`artifacts_list` 读取）；
- 引用路径仅作展示与后续引用输入：**不预授权、不自动进入 Agent 上下文**；Agent 读取仍走 `fs.read` 权限门、写入仍走 `fs.write` 审批（D9 不变）。

**新增命令（3 条 + 选择器 1 条，详见附录 B）**：

- `artifacts_list({ session_id })` → `{ artifacts: [{ id, path, kind, size_bytes, created_at }] }`；
- `artifact_add({ session_id, path })` → 新增或既有引用（canonicalize + 可访问性检查；`artifact_path_rejected`）；
- `artifact_remove({ session_id, artifact_id })` → `{ removed: boolean }`（不存在为幂等 `removed=false`）；
- `ref_pick({ kind: "file" | "directory" })` → `{ path: string | null }`（`null` = 取消）；Rust 侧系统选择器（复用 M1-06 `DirectoryPicker` 抽象并扩展文件选择；E2E 注入替身），**不新增 WebView capability 权限面**（与 `startup_pick_target` 先例一致）；路径原样返回，不做 canonicalize（校验在 `artifact_add`）。

**与工作区绑定（`workspace_set`/D14）的关系**：面板「附加文件夹」仅登记引用，**不调用 `workspace_set`**、不改变权限基准目录与记忆注入；工作区根路径展示取自 `session_list` 响应（见附录 B 的 `workspace_root` 增量）。

**与 D9 的关系（登记文本）**：面板不绕过权限门。用户通过面板添加的文件作为会话引用进入 `artifacts` 表；Agent 读它仍走 `fs.read` 权限门，写它仍走 `fs.write` 审批。权限门边界声明不变（仅约束经线协议上报的工具调用）。

**不在 P0**：文件树浏览、内容预览/编辑、`改动` tab、拖拽排序、拖拽到输入框引用、`@` 引用与上下文注入（§5-4）。

### 决策 2：思考深度登记为会话级参数

**档位定义（与原型一致）**：

| 值 | 显示 | 语义 |
|---|---|---|
| 0 | 关闭 | 不启用扩展思考 |
| 1 | 低 | 最小思考预算 |
| 2 | 高 | 默认 |
| 3 | 极高 | 较大思考预算 |
| 4 | 最大 | 最大思考预算 |

具体 token 预算由各适配器映射（映射表写入任务证据；跨适配器统一口径须另立 ADR），协议只传档位值。

**线协议扩展（D6；不改 major，未知字段忽略策略不变）**：

- `session.create` 新增可选字段 `thinking_depth: 0|1|2|3|4`（缺省 2）；
- `session.send` 新增可选字段 `thinking_depth`（覆盖会话级，仅本次 run）；
- EventEnvelope 9 字段不变；**不新增事件类型**（附录 B 不变；`session.created` payload 的 `SessionSummary` 新增可选字段按附录 B「允许新增可选字段」声明——事件 payload 策略）；
- 能力声明（补全草稿表述）：适配器在 `hello.runtime.capabilities` 与 `initialize` 响应中以**字符串能力项 `thinking_depth`** 声明支持（capabilities 为 `string[]`，存在即支持；不引入布尔映射，保持数组形状不变）；核心以 `runtimes.capabilities`（现有落库路径）判定。

**IPC 扩展（D7；不新增命令）**：

- `session_create` 请求新增可选 `thinking_depth`（0–4；越界 → `out_of_range`；类型非法 → `invalid_type`）；
- `session_send` 请求新增可选 `thinking_depth`（仅本次 run 覆盖）；
- **能力门判定时机（2026-09-24 评审裁定）**：`session_create` 在 runtime 未 ready（cold/starting）时**接受请求**，会话按既有状态机置 `creating`，请求的 `thinking_depth` 随会话落库保留；runtime ready 后核心以 `runtimes.capabilities` 校验能力项——声明支持 → 维持请求值并透传适配器；未声明 → **改写为缺省值 2（`sessions.thinking_depth=2`）、不透传该字段，并返回非阻断警告**（`session_create`/`session_send` 响应新增可选字段 `warnings: [{ code: "thinking_depth_unsupported", field: "thinking_depth", runtime_id, message }]`，无警告时省略）；**警告交付口径（v0.4）**：`warnings` 为**尽力而为**——判定在命令处理路径内同步完成时随该命令响应返回；延迟判定路径（`session_create` 在 cold/starting 接受请求、`session_send` 的 run 启动判定）**不产生响应警告**（响应已返回），以 `SessionSummary.thinking_depth` 回显实际生效值与 UI 能力预判为准；**不得为等待判定阻塞响应**；`session_send` 覆盖请求在 run 启动时按同一门再判定（仅影响本次 run，不回写会话级值）；run 正常执行；
- **落库与重放口径**：`runs.thinking_depth` 落该 run 的生效值（会话级值或本次覆盖；能力未支持时同样落缺省 2，仅记录口径、不透传）；`run_retry` / Mode R/N 重放按**会话级值**恢复，run 启动时重新执行能力门判定（重放行为与新建 run 一致，ADR-005 机制不变）；
- UI 侧以 `runtimes_list` 能力预判为主（滑块置灰 + tooltip「当前运行时不支持思考深度」），警告为**同步判定路径的兜底**（延迟判定路径以生效值回显为准，见上）；`thinking_depth_unsupported` 为**警告码**（非 `IpcErrorCode`，登记于 ADR-006 附录 B「警告码」子表，见决策 4）。

**数据模型（补全草稿缺口；迁移 0003）**：

- `sessions.thinking_depth INTEGER NOT NULL DEFAULT 2`——会话级默认值；用于核心在 `session.send` 缺省时应用、以及会话恢复/UI 回显；能力门未支持时落缺省 2（判定口径见上）；（`Session` 域模型与 IPC 侧 `SessionSummary` DTO 增可选字段——IPC DTO 为**新增登记位**，见附录 B.4；`session.created` payload 扩展按附录 B「允许新增可选字段」声明）；
- `runs.thinking_depth INTEGER`——该 run 的**生效值**（会话级或本次覆盖）；迁移前历史 run 为 `NULL`（不回溯填充）。

### 决策 3：模型与供应商配置登记为 P0 UI + 数据模型扩展

**数据模型（迁移 0003，DDL 见附录 A；`type` 枚举应用层校验，不加 CHECK，与 D12 事件类型同口径）**：

- `providers`、`provider_models`（草稿 DDL 原文；`provider_id` 外键 `ON DELETE CASCADE`；`UNIQUE(provider_id, model_id)` 自带索引）；
- **播种 4 条内置供应商**（`anthropic` / `openai` / `deepseek` / `google`；`is_builtin=1`、`enabled=0`、无密钥、无模型；固定 ULID 见附录 A）——内置供应商**不可删除**（`builtin_provider_undeletable`），可更新/启用/停用。

**命令面（7 条，详见附录 B）**：

| 命令 | 参数 | 要点 |
|---|---|---|
| `providers_list` | 无参数（严格解析） | 返回供应商+模型清单；**不返回 `api_key` 本体**，以 `api_key_ref`（引用，可回显）呈现 |
| `provider_create` | `{ name, type, base_url?, api_key?, enabled }` | `type` ∈ {anthropic, openai, deepseek, google, custom}；`base_url` 可选（`type=custom` 必填）；`api_key` 可选**明文（仅传输）**，非空时核心写入 keyring 并返回 `api_key_ref` |
| `provider_update` | `{ id, name, base_url?, api_key?, enabled }` | 整体更新；**`type` 不可改**（创建后固定）；`api_key` 三态（缺省=不变、空串=清除、非空=覆盖明文写入） |
| `provider_delete` | `{ id }` | **内置拒绝**（`builtin_provider_undeletable`）；删除行 + 级联模型 + 删除 keychain 条目（删除规则见密钥节）；UI 侧二次确认（命令层不做 confirm） |
| `provider_toggle` | `{ id, enabled }` | 快速启用/停用（内置可停用） |
| `provider_model_add` | `{ provider_id, model_id, display_name }` | 新增模型（默认启用）；`(provider_id, model_id)` 重复 → `invalid_value` |
| `provider_model_toggle` | `{ provider_id, model_id, enabled }` | 模型启用/停用；不存在 → `provider_model_not_found` |

> **`provider_test` 已移出 P0**（2026-09-24 评审裁定）：P0 命令面不登记连通性测试命令；UI 保留「测试连接」按钮，点击显示 toast「连通性测试将在 P1 开放」（无 IPC 调用）。P1 实做时须先按 ADR 流程登记命令（涉及网络请求、密钥使用与错误呈现），再开放入口。

**密钥（D10 不变；写入路径随本 ADR 登记——2026-09-24 评审裁定）**：

- **写入路径**：设置页表单 `api_key` 字段为**明文输入（仅传输）**；核心经 `aether-security` 写入 keyring（keyring 自检与 A3 降级加密文件路径为 M1-07 已交付能力），写入目标引用为 `keychain://aether/provider/<provider_id>`，命令响应**只返回 `api_key_ref`（引用）**，`api_key` 本体不落库、不进响应/日志/诊断包/导出/测试夹具（D10 脱敏口径，扫描断言含 `sk-`/`eyJ`/PEM 模式）；
- **编辑态**：表单显示 `api_key_ref` **只读**；用户可重新输入新密钥覆盖（`provider_update` 携带 `api_key` 非空 → 覆盖写入同一引用）；
- `providers.api_key_ref` 取值固定 `keychain://aether/provider/<provider_id>`（核心生成，不由用户输入）；
- **keychain 条目删除规则**：`provider_delete` / `provider_update`（空串清除）仅当 `api_key_ref` 命中自身命名空间 `keychain://aether/provider/<provider_id>` 时删除 keychain 条目；**自定义/共享引用不删**（避免破坏其他消费者）；keychain 不可用或条目不存在 → 忽略并记诊断告警，**不阻断**命令本体；
- A3 降级（keychain 不可用）时核心写入降级加密文件存储（M1-07 口径），命令与配置页其余功能可用；降级与正常路径均**不得明文落库**（D10）。

**UI（设置页「模型与供应商配置」+ 输入区模型选择器；以原型为准）**：

- 供应商列表：卡片（图标/名称/描述「N 个模型已启用」）+ 启用开关（`provider_toggle`）+ 编辑/删除（删除二次确认；内置删除按钮置灰）；
- 新建：`添加供应商`（官方预设：Anthropic/OpenAI/DeepSeek/Google，预填名称与 Base URL）/ `添加自定义供应商`（`type=custom`，Base URL 必填）；
- 表单：供应商名称、Base URL（带请求路径预览）、**API Key（`api_key` 明文输入，掩码 + 显示/隐藏；仅传输——核心写入 keyring 后只存引用）**、启用开关、已启用模型、可用模型（启用/停用，`provider_model_toggle`）、手动添加模型（`provider_model_add`，缺省显示名 = 模型 ID）；**编辑态显示 `api_key_ref` 只读 + 可选「重新输入新密钥」覆盖输入**；
- `测试连接`：保留入口（原型一致），**点击显示 toast「连通性测试将在 P1 开放」**（P0 无命令调用、不发网络请求）；P1 实做时先登记命令（ADR）；
- `从供应商获取`：P0 **不提供入口**（P1）；
- 模型选择器：从**启用供应商的启用模型**派生（分组 + 搜索 + 空态文案与原型一致）；选择结果经既有 `session.create.model` 透传（UI-05）；**运行期不支持切换**（无 update 命令；另立 ADR，见 §5-1）；
- 官方预设颜色/缩写为 **UI 侧常量**（原型 `officialProviders`），不落库。

**P0 边界（防范围蔓延）**：

- 模型删除命令未登记（P0 仅启用/停用；原型亦无删除入口，见 §5-8）；
- 连通性测试无命令（P0 入口仅 toast；P1 实做须登记命令，见 §5-7）；
- **供应商记录 → 适配器消费映射（env/配置注入、模型路由）不在 P0**（须另立 ADR；D10 现有注入路径与各运行时配置形态耦合，见 §5-2）；
- 设置键白名单（`SETTINGS_KEY_ALLOWLIST`）不变（供应商配置不走 `settings_get/set`）。

### 决策 4：新增 IPC 命令面

**文件引用类（3 条）**：

| 命令 | 参数 | 语义与校验 |
|---|---|---|
| `artifacts_list` | `{ session_id }` | `session_id` ULID（`invalid_format`）；不存在会话 → `invalid_value`；返回按 `created_at` 升序 |
| `artifact_add` | `{ session_id, path }` | `path` 非空、≤4096 字符（`too_large`）；**canonicalize + 可访问性检查**（canonicalize 解析软链接/存在性；stat 探测 `kind` 与文件大小；任一失败 → `artifact_path_rejected`；2026-09-24 评审裁定：**不复用 A4 同步盘检测**，不做工作区前缀比较——引用不预授权，D9 不变）；同会话同路径幂等（`UNIQUE` 兜底）；可加文件或目录，不递归、不读内容 |
| `artifact_remove` | `{ session_id, artifact_id }` | `artifact_id` ULID；不存在 → 幂等 `{ removed: false }`（不新增错误码） |

**供应商类（7 条）**：见决策 3 命令表（`providers_list` / `provider_create` / `provider_update` / `provider_delete` / `provider_toggle` / `provider_model_add` / `provider_model_toggle`；`provider_test` 已移出 P0）。

**思考深度类**：**无新命令**，扩展 `session_create` / `session_send` 参数（决策 2）。

**新增错误码（ADR-006 附录 B 增行，16 → 20；登记表见附录 B.3）**：

| code | 语义 | 触发点 | 前端行为 |
|---|---|---|---|
| `builtin_provider_undeletable` | 内置供应商禁止删除 | `provider_delete` | 提示「内置供应商不可删除」（删除入口预置灰） |
| `artifact_path_rejected` | 引用路径校验失败（canonicalize / 可访问性 / 探测失败） | `artifact_add` | 提示路径不可用 + 原因 |
| `provider_not_found` | 供应商不存在 | `provider_update` / `provider_delete` / `provider_toggle` / `provider_model_add` / `provider_model_toggle` | 刷新供应商列表 + 提示 |
| `provider_model_not_found` | 模型不存在 | `provider_model_toggle` | 刷新表单模型列表 + 提示 |

> `artifact_path_rejected` 与既有 `path_rejected` 的边界（2026-09-24 评审裁定登记）：`path_rejected` 面向数据目录/工作区/备份路径（A4/`workspace_set`/`startup_migrate`，含同步盘拒绝）；`artifact_path_rejected` 为会话引用命令面专属码（前端差异化提示「路径不可用 + 原因」；**不含同步盘语义**）。

**警告码命名空间与登记规则（新增）**：`session_create`/`session_send` 响应新增可选 `warnings: [{ code, field, runtime_id, message }]`（无警告省略）；`warnings[].code` 为独立命名空间（非 `IpcErrorCode`，不入 ADR-006 错误码表）；**登记位 = ADR-006 附录 B 新增「警告码」子表**（与「错误码」表并列）；新增警告码必须走 ADR 在该子表增行，不得自造（与错误码登记规则同构）；本 ADR 首个登记项：`thinking_depth_unsupported`（见附录 B.3 警告码子表）。

## 3. 影响

### 3.1 《设计文档》v1.9 → v1.10（已合入；合入文本见附录 D）

- 头部：文档版本行、状态行升 v1.10、纳入 ADR-010；修订记录追加 v1.10 行；执行摘要「当前状态」同步。
- §2.3：多视图行更新（单窗口 + 会话列表切换 + **右栏只读文件引用面板**，ADR-010）。
- D3：`artifacts` 表登记（会话引用持久化；迁移 0003）。
- D6：方法表 `session.create`/`session.send` 说明补 `thinking_depth` 可选参数；能力项 `thinking_depth` 登记；能力门判定时机（creating 期接受请求、ready 后判定）。
- D7：命令面 +11（决策 4 的 10 条 + `ref_pick`；`provider_test` 移出 P0）；`session_create`/`session_send` 参数与 `warnings` 响应、`session_list` 响应 `workspace_root` 增量登记；计数 25 → 36。
- D10：**供应商密钥写入路径**（表单 `api_key` 明文仅传输 → 核心写 keyring → 返回/落库 `api_key_ref`，`keychain://aether/provider/<id>`）登记；`api_key` 本体不落库/不进日志/诊断/导出；`api_key_ref` 引用可回显（编辑表单只读）；keychain 条目删除规则（自身命名空间才删）。
- 附录 C：迁移 0003 终态 DDL（`artifacts`、`providers`、`provider_models`、`sessions.thinking_depth`、`runs.thinking_depth`）。
- 附录 E：字段映射新增（`Artifact.*`、`Session.thinking_depth`、`Run.thinking_depth`、`Provider.*`、`ProviderModel.*`）。
- 附录 B：**不变**（无新增事件类型；`session.created` payload 允许新增可选字段为既有策略）。

### 3.2 《实施计划与验收标准》v1.16 → v1.17（已合入；合入文本见附录 E）

- 头部/§8：计划版本、状态、基线引用升 v1.17 / 设计文档 v1.10（含 ADR-001–ADR-010）；修订记录追加 v1.17。
- 新增 3 个任务（M3 内，G8 并行组）：**M3-09 文件引用面板（只读）**、**M3-10 思考深度（会话级参数）**、**M3-11 模型与供应商配置**；Gate 3 通过条件同步（M3 全部 DoD 含新任务）。
- §1 汇总：文档条目 37 → 40；实际执行口径 36 → 39；量级分布 S 3 / M 19 / L 14 → S 3 / M 20 / L 16。
- M4-04 DoD4 命令面计数：24 条目 / 25 命令 → **35 条目 / 36 命令**。
- §7 映射表追加 M3-09/M3-10/M3-11 行。

### 3.3 《需求文档》v0.6 → v0.7（已合入；合入文本见附录 F）

- 头部版本/状态与修订记录追加 v0.7。
- §3.3 新增 UI-07 文件引用面板（P0 只读，引用持久化）、UI-08 思考深度（P0 会话级参数）、UI-09 模型与供应商配置（P0 部分：UI + 数据模型 + 密钥写入路径）。
- §3.8 新增三行阶段归属与 P0 验收口径；RA-04 行补能力项 `thinking_depth`。
- §6 数据模型概要：新增 Artifact / Provider / ProviderModel；Session 补 `thinking_depth`。

### 3.4 《UI-UX 设计规格》v0.1 → v0.2（草案回流，非冻结基线；已合入）

- §0.2 采纳表：「右侧工作区多标签」处置由「不采纳」改为「**采纳（裁剪为只读引用面板，ADR-010）**」；「左栏 + 主输出 + 右侧工作区三段式」行右栏表述同步（含文件引用面板）；「模型选择器信息密度」行补运行期只读口径。
- §0.1 C10 硬约束命令计数：25 → **36 个可调用命令**（ADR-010）。
- §1.2 界面清单：新增 S-11 文件引用面板；§2.1 线框右栏、§2.4 输入区（思考深度 + 模型选择器）、§2.5 右栏分区同步。
- §5 文案表：思考深度不支持 tooltip、`artifact_path_rejected` / `builtin_provider_undeletable` / `provider_not_found` 文案、供应商配置空态/删除确认文案、「连通性测试将在 P1 开放」toast、表单 API Key 明文输入（掩码）与编辑态 `api_key_ref` 只读说明。
- §7 待实现锚点：新增本 ADR 登记的 testid（见附录 G）。
- §8 范围冻结声明与 §9 开放问题 Q2/Q4/Q15 修订。
- 合入状态（2026-09-24 随批）：§0.1/§0.2/§1.1/§1.2/§2.1/§2.4/§2.5/§2.6/§5/§7.3/§8/§9 与附录追溯表均已回流完成。

### 3.5 实现对齐状态与不影响的

| 项 | 状态 |
|---|---|
| 命令面（11 条：决策 4 的 10 条 + `ref_pick`） | **未实现**（M3-09/M3-11；本 ADR 登记后实施） |
| 供应商密钥写入路径（`api_key` → keyring） | **未实现**（M3-11；`aether-security` 写入路径随本 ADR 登记） |
| 线协议 `thinking_depth`（D6）与能力项 | **未实现**（M3-10） |
| 迁移 0003（3 表 + 2 列 + 内置播种） | **未实现**（M3-09/M3-10/M3-11；0001/0002 不改） |
| UI（文件面板 / 思考深度滑块 / 供应商配置页 / 模型选择器） | **未实现**（M3-09/M3-10/M3-11） |

**不影响的**：

- 不新增/修改事件类型（附录 B 清单不变；`session.created` 可选字段扩展按既有策略）；
- 不改既有 25 命令的参数/校验语义（新增字段均可选，`deny_unknown_fields` 不变）；
- 不改 D9 策略矩阵与权限回环；不改线协议 major 与帧上限（2MiB / `artifact_ref` <1MiB）；
- 不改 Gate 1/Gate 2 条件结构；不改 M3-06 关键路径与 M4-01/02/03 演练矩阵；
- 不改 `tauri.conf.json` CSP/capabilities 基线（`ref_pick` 走 Rust 侧选择器，不新增 WebView 权限面）。

### 3.6 后果与风险（决策 4 影响汇总）

**正面**：

- UI 原型与后端契约对齐；Gate 3 有明确口径；模型选择器与供应商配置真正联动；思考深度有协议承载。

**负面**：

- D3/D4/D6/D7/D9/D10 均需小幅扩展；新增 **3 张表**（`artifacts` / `providers` / `provider_models`）+ `sessions`/`runs` 各 1 列（迁移 0003；决策 4 原文「2 张表」未计 `artifacts` 表，按命令契约修正）；IPC 命令面从 25 条扩到 **36 条**（35 条目）。

**风险与缓解**：

| 风险 | 缓解 |
|---|---|
| 供应商配置与 P1 密钥托管增强冲突 | `SecretProvider` 抽象已预留（D10 失效条件）；`api_key` 明文仅传输、`api_key_ref` 语法不变，P1 换实现不改 UI |
| 文件面板被误认为文件管理器 | UI 明确标注「只读引用」；不提供目录浏览/预览（§4 不做清单不变） |
| 引用路径校验误拒（可访问性检查误杀） | 校验仅 canonicalize + 可访问性（2026-09-24 裁定：不复用 A4 同步盘检测，无同步盘误杀面）；误拒走 §6-4 回退路径 |
| 内置供应商被误删/误改 | 命令层 `builtin_provider_undeletable` 硬拒绝 + UI 删除入口置灰（不得仅靠 UI 隐藏） |
| `api_key` 明文在传输/内存驻留期泄露 | 仅本地 IPC 传输；核心立即写入 keyring 后丢弃；日志/诊断/导出/夹具脱敏扫描 0 命中（M3-11 DoD 断言，D10） |

### 3.7 与现有决策的关系

| 决策 | 变更 |
|---|---|
| D3 | 新增 `artifacts` / `providers` / `provider_models` 三表（迁移 0003；0001/0002 不改） |
| D4 | `sessions`/`runs` 新增 `thinking_depth` 列；EventEnvelope 9 字段不变；`session.created` payload 可选字段扩展（附录 B 允许） |
| D6 | `session.create` / `session.send` 参数扩展；方法集不变；能力项 `thinking_depth`；能力门判定时机（creating 期接受、ready 后判定） |
| D7 | 命令面扩展（+11）；M1-08 校验矩阵扩展；错误码 +4、警告码 +1（ADR-006 附录 B 警告码子表） |
| D9 | 文件面板不绕过权限门（引用登记 ≠ 预授权） |
| D10 | 密钥写入路径登记（`api_key` 明文仅传输 → 核心写 keyring → `api_key_ref`）；keychain 仍为唯一存储；引用可回显；keychain 条目删除规则 |
| §2.3 | 三项登记为 P0 UI 扩展，表格新增对应行 |
| §4 | 不做清单不变（文件树浏览、diff 视图、真实连通性测试仍在 P1+） |

## 4. 版本（拟合入）

| 文档 | 修订前 | 修订后 |
|---|---|---|
| 《设计文档》 | v1.9 | **v1.10** |
| 《实施计划与验收标准》 | v1.16 | **v1.17** |
| 《需求文档》 | v0.6 | **v0.7** |
| 《UI-UX 设计规格》（草案） | v0.1 | v0.2 |
| 《ADR-006》附录 B | 错误码 16 个 | 错误码增行 4 个 → 20 个；**新增「警告码」子表**（首项 `thinking_depth_unsupported`；后续新增警告码须走 ADR 增行） |

## 5. 后续（未决项）

1. **运行期模型/运行时切换**（原型输入区选择器）：P0 口径 = 选择器作用于新建会话（`session.create`），会话内只读展示；运行期切换须扩 D6 方法集（`session.update` 类）与 IPC 命令，**另立 ADR**（不得以 UI 兜底伪造）。
2. **供应商记录 → 适配器消费映射**：env/配置注入（Claude Code `ANTHROPIC_API_KEY` 类）、Codex/DSH 供应商配置物化、模型路由——P1 另立 ADR（与 ADR-008 DSH provider 分层配置对齐）。
3. ~~**供应商密钥写入触发路径（阻塞项）**~~ **已随本 ADR 登记（2026-09-24 评审裁定，§7 对价 3）**：`provider_create`/`provider_update` 携带 `api_key`（明文，仅传输）→ 核心经 `aether-security` 写 keyring → 返回/落库 `api_key_ref`；表单编辑态显示 `api_key_ref` 只读 + 可重新输入覆盖（见决策 3 密钥节）。
4. **文件引用上下文注入**（`@` 引用、拖拽、随消息附带引用列表）：持久化已由 `artifacts` 表承载；注入面须另立 ADR（涉及 D6 `session.send` 载荷形态）。
5. **原型未登记元素**（权限选择器 read/write/full、Chat 模式、工作区树、基本设置/主题/语言、Agent 预设、语音/附件按钮）：不随本 ADR 进入 P0（对照表见附录 G）；权限选择器与 D9 固定矩阵冲突，如要落地须另立 ADR（P1 规则引擎时点）。
6. **思考深度档位 → token 预算映射表**：各适配器在任务证据内登记；若需跨适配器统一口径，另立决策。
7. **真实连通性测试**：P0 无命令（入口 toast）；P1 实做须**先按 ADR 流程登记命令**（网络请求、密钥使用、错误呈现与超时定义），再开放入口；「从供应商获取模型列表」P1。
8. **模型删除命令**：P0 未登记（仅启用/停用）；如需要须另立 ADR。
9. **模型选择器空态与手动输入共存口径**：播种内置供应商默认 `enabled=0` 且无模型 → 新装用户选择器为空；未定义是否保留手动输入模型 ID 入口（UI-05 P0 会话级透传已由 M3-02 交付），以及所选模型与当前会话 runtime 不匹配时的校验/呈现——**M3-11 实现前必须登记**（防 UI-05 验收回归）。
10. **载荷边界复核**：`providers_list` 响应 ≤1MiB（D7 单条上限口径）、`artifacts_list` 单会话引用数上限（P0 未设，实现时按 500 量级评估并登记）、`provider_create`/`provider_update` 请求 ≤64KiB（含 `api_key` 明文）——实现后由 M4-04 基准复核，超限再评估。

## 6. 回退条件

1. **评审否决任一决策** → 该决策从合入文本移除，其余决策不连带；不得以 UI 兜底伪造被否决能力（如无命令面不得提供运行期模型切换入口）。
2. **keychain 写入不可用**（A3 降级路径亦失败）→ 供应商配置页密钥字段禁用 + 显示「安全级别：降级」；**不得明文落库**（D10）；配置页其余功能可用；既有降级写入（M1-07 加密文件）可用时命令正常。
3. **`thinking_depth` 导致任一官方运行时失败率上升**（Gate 2/Gate 3 指标）→ 移除该适配器能力声明（`thinking_depth`），UI 由能力驱动自动置灰；协议字段保留（不删）。
4. **引用路径校验误拒**（可访问性检查误杀合法路径）→ 修复检查实现或放宽为「canonicalize + 存在性」并另立 ADR 修订；**不得静默引入 A4 同步盘检测**（2026-09-24 裁定：引用路径不做同步盘检测）。
5. **文件面板被要求读取/预览内容** → 停止实现并另立 ADR；不得绕过 D9、不得提前多模态（§4#19）。
6. **内置供应商约束被要求放宽**（可删除）→ 先改 D12/附录 C 语义并另立 ADR；`builtin_provider_undeletable` 为硬约束，不得以 UI 隐藏代替。
7. **「测试连接」被要求 P0 真实联网** → 停止，P1 须**先登记命令**（另立 ADR，涉及密钥使用与网络错误呈现），不得以无命令面提供联网功能。
8. **新命令实现中发现需要新错误码/警告码** → 先按 ADR-006 附录 B 登记（错误码增行 / 警告码子表增行，ADR 修订），不得自造；不得放宽既有校验。
9. **迁移 0003 在既有库升级失败** → 单版本单事务回滚（不产生半迁移状态）；禁止修改 0001/0002（AGENTS §8）。

## 7. 范围对价登记（2026-09-24 评审裁定）

《设计文档》§4 范围纪律要求「任何新增 MVP 需求必须同时给出『砍掉列表中的哪一项』或『延后哪个里程碑』」。本 ADR 新增三项 P0 能力（文件引用面板 / 思考深度 / 模型与供应商配置）与三个任务（M3-09/M3-10/M3-11），经 2026-09-24 评审裁定**豁免砍一项换一项的对价**，理由与登记如下：

| # | 新增能力 | 对价口径 | 裁定依据 |
|---|---|---|---|
| 1 | 右栏只读文件引用面板（UI-07，M3-09，M 级） | 豁免 | 原型封板（UI/UX v0.0.1）为既有交付承诺；面板为**只读引用**（不列目录/不预览/不读内容），P0 新增面收敛为 1 表 + 3 命令 + 1 选择器；§2.3 多视图补全（原 P1 项）以裁剪形态提前，其余多视图（标签/分屏）仍维持 P1 不变 |
| 2 | 思考深度会话级参数（UI-08，M3-10，L 级） | 豁免 | 原型封板能力；落地为**协议可选字段 + 能力项 + 2 列**，不新增命令、不新增事件类型、不改线协议 major；不支持运行时自动置灰（能力门），无新增约束面 |
| 3 | 模型与供应商配置（UI-09，M3-11，L 级） | 豁免 | 原型封板为既有交付承诺；P0 裁剪明确：无连通性测试命令（入口 toast）、无模型删除、无远程获取、**无适配器消费映射**（§5-2 P1 另立 ADR）——P0 仅 UI + 数据模型 + 密钥写入路径，D7 命令面 +7 |

**约束**：本豁免仅覆盖上表三项；后续任何新增 MVP 需求仍须按 §4 范围纪律给出对价，不得援引本节作为先例。G8 并行组并行于关键路径（M3-01→M3-02→M3-06 不变），M4 里程碑依赖面不变。

## 8. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-09-24 | （待评审） | — | 本 ADR 为提议稿；v0.2 合入评审输入「决策 4：新增 IPC 命令面」（11 命令 + 4 错误码 + 1 警告码 + 后果/关系表） |
| 2026-09-24 | 评审方（架构评审 Agent 复核 + 用户裁定） | **通过（附 7 项修订）** | 修订清单：① 范围对价豁免并新增 §7；② 密钥写入路径本 ADR 内补登记（`api_key` 明文仅传输 → keyring → `api_key_ref`；编辑态只读引用 + 覆盖输入；keychain 删除规则）；③ `artifact_add` 改 canonicalize + 可访问性检查，不复用 A4 同步盘检测；④ `provider_test` 移出 P0 命令面（入口保留，toast 提示 P1）；⑤ 建立 `warnings[].code` 命名空间规则，登记位 = ADR-006 附录 B「警告码」子表；⑥ `session_create` 未 ready 时接受请求（`creating`），ready 后判定能力，不支持按缺省 2 + 警告；⑦ `SessionSummary` 登记为新增 IPC DTO（字段清单见附录 B.4，T14 生成，不援引附录 B） |
| 2026-09-24 | AI 复核（opencode，合入前一致性核对） | **通过（附 2 项 v0.4 修订，不改决策）** | ① 警告交付口径（P1）：`warnings` 明确为尽力而为——延迟判定路径（cold/starting create、run 启动判定）不产生响应警告，以 `SessionSummary.thinking_depth` 生效值回显 + UI 能力预判为准，不得阻塞响应；② DTO 归属（P2）：明确 M3-09 先引入 `SessionSummary` DTO（含 `workspace_root`）、M3-10 追加 `thinking_depth`（并行先手/后手口径）。其余核对通过：计数 35 条目/36 命令、错误码 16→20、`provider_test` 移出、`artifact_add` 去 A4 复用、密钥写入路径、警告码子表、§7 对价；B.4 与代码一致（`ReadPool::sessions` 返回 `Vec<Session>`） |
| 2026-09-24 | AI 复核（opencode，合入后收口核对） | **通过（附 6 项维护性修订，v0.5）** | ① 设计文档「对应需求」行升 v0.7（P1，冻结基线引用漂移）；② 实施计划 §1.2 补「ADR-010 支路：M3-09/M3-10/M3-11 → 汇入 Gate 3」；③ G8 说明补任务详单依赖口径（M3-10 另依赖 M2-11/M2-02）；④ 需求 §8 P0 交付物补三项 ADR-010 能力（裁剪口径；**记录更正 2026-09-24：原「裁定 3：设计文档 §2.1 一句话范围直接补齐」经用户裁定维持 §2.1 最小改动、不补三项——该口径由 §2.3/附录 A/需求 §8 三处承载，更正为「待确认 3 落点 = 需求 §8」，不与问题 4 重复计账**）；⑤ M3-11 DoD6 挂钩 §5-9「空态/手动输入口径登记先行」（UI-05 回归防护）；⑥ 本 ADR 决策载体转「已合入」+ §3.4 补记回流完成。均不改决策 |
| 2026-09-24 | 评审（用户） | **通过（全部文档评审通过）** | 评审范围：本 ADR v0.5 与合入结果（设计文档 v1.10 / 实施计划 v1.17 / 需求文档 v0.7 / UI-UX 规格 v0.2 / ADR-006 v0.5）；§2.1 最小改动口径与 v0.5 记录更正一并确认；本 ADR 转 v0.6 评审通过收口 |

## 9. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-09-24 | 创建：三决策登记（文件面板/思考深度/供应商配置）+ 合入文本（设计文档 v1.10、计划 v1.17、需求文档 v0.7、UI-UX 规格 v0.2）+ 新任务 M3-09/M3-10/M3-11 + 原型对照与 testid 登记 | （文档维护，待评审） |
| v0.2 | 2026-09-24 | 合入评审输入「决策 4」：命令面清单（文件引用 3 + 供应商 8）与错误码/警告码登记；决策 1 改为会话引用持久化（`artifacts` 表 + 3 命令；原「纯 UI 客户端状态」废止）；决策 3 改为 8 命令（`api_key_ref`、内置拒绝与播种、`provider_test` P0 仅参数校验）；命令面 25 → 37；新增 §3.6 后果与风险、§3.7 与现有决策关系；迁移 0003 修正为 3 表 + 2 列 | （文档维护，待评审） |
| v0.3 | 2026-09-24 | 合入评审裁定 7 项：① §7 范围对价登记（豁免三项）；② 密钥写入路径登记（`api_key` → keyring → `api_key_ref`；keychain 删除规则；§5-3 关闭）；③ `artifact_add` 改 canonicalize + 可访问性（去 A4 同步盘复用；§6-4/风险表同步）；④ `provider_test` 移出 P0（命令面 12 → 11 条；35 条目 / 36 命令；入口 toast；§5-7/§6-7/附录 B/D/E/G 同步）；⑤ 警告码命名空间规则（登记位 = ADR-006 附录 B「警告码」子表）；⑥ `session_create` creating 期判定时机 + 落库缺省 2 + 重放口径（决策 2/附录 C 同步）；⑦ `SessionSummary` 登记为新增 IPC DTO（附录 B.4 字段清单；T14 生成；事件 payload 引用附录 B 保留） | 评审裁定（用户）合入 |
| v0.4 | 2026-09-24 | 合入前一致性修订 2 项（不改决策）：① `warnings` 尽力而为交付口径（同步判定随响应；延迟判定不产生响应警告，以生效值回显 + UI 预判为准；不得阻塞响应）——决策 2/B.2/B.3/附录 C/D 与 M3-10 DoD 同步；② `SessionSummary` DTO 归属明确（M3-09 引入含 `workspace_root`、M3-10 追加 `thinking_depth`；并行先手/后手口径）——B.4/M3-09/M3-10 DoD 同步；§8 增复核记录 | （AI 复核，合入前） |
| v0.5 | 2026-09-24 | 合入后维护性修订（不改决策）：决策载体行转「已合入（随批）」（设计 v1.10 / 计划 v1.17 / 需求 v0.7 / UI-UX v0.2 / ADR-006 v0.5）；§3.4 补记 UI-UX 回流已完成；§8 增合入后收口复核行；随批复核 6 项收口——设计文档「对应需求」升 v0.7（P1）、计划 §1.2 补 ADR-010 支路、G8 依赖口径补注（M3-10 另依赖 M2-11/M2-02）、需求 §8 P0 交付物补三项能力（记录更正 2026-09-24：裁定 3 落点 = 需求 §8——设计文档 §2.1 维持最小改动、不补三项）、M3-11 挂钩 §5-9 登记先行 | （AI 复核，合入后收口；含同日记录更正） |
| v0.6 | 2026-09-24 | 评审通过收口（不改决策）：§8 增「评审（用户）通过」行；状态行标注「全部文档评审通过」；§3.1/§3.2/§3.3 与附录 D/E/F 的「拟定合入文本」标记转「已合入」；UI-UX 规格状态转「已评审通过」 | （文档维护，评审通过） |

---

## 附录 A：数据模型与迁移 0003

### A.1 迁移文件（`migrations/0003_p0_ui_extensions.sql`，拟定全文）

```sql
-- 0003_p0_ui_extensions.sql —— ADR-010（P0 UI 能力扩展）增量迁移。
--
-- 背景：0001/0002 已发布（禁止修改，AGENTS.md §8）。本迁移只新增列与表：
--   1) sessions.thinking_depth / runs.thinking_depth：会话级思考深度（0–4）与 run 生效值；
--   2) artifacts：会话文件/目录引用（canonicalize 后路径；D9 权限门不受影响）；
--   3) providers / provider_models：模型与供应商配置（密钥只存 keychain 引用，D10）；
--      播种 4 条内置供应商（is_builtin=1，enabled=0，无密钥/模型；不可删除）。
--
-- 契约（同 0002）：不含 PRAGMA、不含 BEGIN/COMMIT/ROLLBACK（单事务由迁移框架包裹）、
--   不含 schema_migrations 版本写入。

ALTER TABLE sessions ADD COLUMN thinking_depth INTEGER NOT NULL DEFAULT 2;
ALTER TABLE runs ADD COLUMN thinking_depth INTEGER;

CREATE TABLE artifacts (
  id          TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  path        TEXT NOT NULL,          -- canonicalize 后的绝对路径（artifact_add 校验后写入）
  kind        TEXT NOT NULL DEFAULT 'file',  -- file | directory（应用层校验；目录不递归）
  size_bytes  INTEGER,                -- 文件字节数；目录为 NULL
  created_at  INTEGER NOT NULL,
  UNIQUE (session_id, path)
);

CREATE TABLE providers (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  type         TEXT NOT NULL,        -- anthropic | openai | deepseek | google | custom（应用层校验，D12 口径）
  base_url     TEXT,
  api_key_ref  TEXT,                 -- keychain://aether/<service>/<key>，只存引用（D10）
  enabled      INTEGER NOT NULL DEFAULT 1,
  is_builtin   INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);

CREATE TABLE provider_models (
  id           TEXT PRIMARY KEY,
  provider_id  TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  model_id     TEXT NOT NULL,
  display_name TEXT NOT NULL,
  enabled      INTEGER NOT NULL DEFAULT 1,
  created_at   INTEGER NOT NULL,
  UNIQUE (provider_id, model_id)
);

-- 内置供应商播种（固定 ULID；ts = 迁移执行时刻）
INSERT INTO providers (id, name, type, base_url, api_key_ref, enabled, is_builtin, created_at, updated_at) VALUES
  ('01J00000000000000000000B01', 'Anthropic', 'anthropic', 'https://api.anthropic.com',                NULL, 0, 1, CAST(strftime('%s','now') AS INTEGER) * 1000, CAST(strftime('%s','now') AS INTEGER) * 1000),
  ('01J00000000000000000000B02', 'OpenAI',    'openai',    'https://api.openai.com',                    NULL, 0, 1, CAST(strftime('%s','now') AS INTEGER) * 1000, CAST(strftime('%s','now') AS INTEGER) * 1000),
  ('01J00000000000000000000B03', 'DeepSeek',  'deepseek',  'https://api.deepseek.com',                  NULL, 0, 1, CAST(strftime('%s','now') AS INTEGER) * 1000, CAST(strftime('%s','now') AS INTEGER) * 1000),
  ('01J00000000000000000000B04', 'Google Gemini', 'google','https://generativelanguage.googleapis.com', NULL, 0, 1, CAST(strftime('%s','now') AS INTEGER) * 1000, CAST(strftime('%s','now') AS INTEGER) * 1000);
```

### A.2 语义

| 项 | 语义 |
|---|---|
| `sessions.thinking_depth` | 会话级默认档位（0–4，默认 2）；`session.send` 缺省时应用；恢复/UI 回显来源 |
| `runs.thinking_depth` | 该 run 生效值（会话级或本次覆盖）；迁移前历史行为 `NULL` |
| `artifacts.path` | `artifact_add` canonicalize 后的绝对路径；`UNIQUE(session_id, path)` 幂等兜底 |
| `artifacts.kind` | `file`（「会话文件」）/ `directory`（「项目文件」，不递归）；应用层校验 |
| `providers.api_key_ref` | 只存引用；写入路径 = `provider_create`/`provider_update` 的 `api_key` 明文参数（仅传输，核心经 `aether-security` 写 keyring；评审裁定 2026-09-24）；密钥本体在 OS Keychain（A3 降级时为加密文件） |
| `providers.type` | 枚举应用层校验（不加 CHECK；新增类型不触发迁移）；创建后不可改 |
| `providers.is_builtin` | 内置标记（迁移播种 4 条）；`provider_delete` 硬拒绝（`builtin_provider_undeletable`） |
| `provider_models` | `UNIQUE(provider_id, model_id)`；删除供应商级联删除模型 |

## 附录 B：IPC 命令面登记（+11 条 / 35 条目 / 36 命令）

### B.1 新增命令

| 命令 | 参数（严格解析） | 校验 | 响应 | 说明 |
|---|---|---|---|---|
| `ref_pick` | `{ kind: "file" \| "directory" }` | `kind` 枚举白名单（`invalid_enum`）；未知成员拒绝 | `{ path: string \| null }`（null=取消） | Rust 侧系统选择器（扩展 `DirectoryPicker` 抽象；经 `state.picker()`，不依赖后端注入）；不新增 WebView capability；路径原样返回（不 canonicalize）；启动门阻断期由 UI 不可达兜底 |
| `artifacts_list` | `{ session_id }` | `session_id` ULID（`invalid_format`）；不存在会话 → `invalid_value` | `{ artifacts: [{ id, path, kind, size_bytes, created_at }] }` | 按 `created_at` 升序 |
| `artifact_add` | `{ session_id, path }` | `session_id` ULID；`path` 非空、≤4096 字符（`too_large`）；canonicalize + 可访问性检查（2026-09-24 裁定：不复用 A4 同步盘检测）→ 失败 `artifact_path_rejected` | 新增或既有引用（形状同列表元素） | 可加文件或目录（`kind` 探测）；不递归、不读内容；同会话同路径幂等（`UNIQUE` 兜底） |
| `artifact_remove` | `{ session_id, artifact_id }` | 两者 ULID（`invalid_format`） | `{ removed: boolean }` | 不存在 → 幂等 `removed=false`（不新增错误码） |
| `providers_list` | 无参数（`null`/缺省/空对象合法；任何成员拒绝） | `parse_no_params` | `{ providers: [{ id, name, type, base_url, api_key_ref, enabled, is_builtin, created_at, updated_at, models: [{ id, model_id, display_name, enabled }] }] }` | 按 `created_at` 升序；**不含 `api_key` 本体**；`api_key_ref` 为引用（非密钥，D10 允许回显；编辑表单只读展示） |
| `provider_create` | `{ name, type, base_url?, api_key?, enabled }` | `type` 枚举白名单（`invalid_enum`）；`name` 非空 ≤128 字符、无控制字符；`base_url` 可选、`https?://` 前缀、≤2048 字符（`type=custom` 必填）；`api_key` 可选明文（仅传输）、非空 ≤8192 字符（`too_large`） | 新建供应商（形状同列表元素，含 `api_key_ref`） | 核心生成 ULID；`api_key` 非空 → 经 `aether-security` 写 keyring（`keychain://aether/provider/<id>`；A3 降级走加密文件，M1-07 口径）→ 返回 `api_key_ref`；写入失败 → 命令整体失败（`internal`），不落库；`api_key` 本体不进日志/诊断/响应（D10）；`enabled` 必填 bool |
| `provider_update` | `{ id, name, base_url?, api_key?, enabled }` | `id` ULID；不存在 → `provider_not_found`；其余同 create（**不含 `type`**） | 更新后供应商（形状同列表元素） | 整体更新；`api_key` 三态（缺省=不变、空串=清除、非空=覆盖明文写入；空串=清除 ref + 删自身命名空间 keychain 条目，规则见决策 3）；`type` 不可改 |
| `provider_delete` | `{ id }` | `id` ULID；不存在 → `provider_not_found`；`is_builtin=1` → `builtin_provider_undeletable` | `{ deleted: true }` | 删除行 + 级联模型 + 删除 keychain 条目（仅自身命名空间 `keychain://aether/provider/<id>`；不可用/失败忽略 + 诊断，不阻断；见决策 3 密钥节）；UI 二次确认（命令层不做 confirm） |
| `provider_toggle` | `{ id, enabled }` | `id` ULID；不存在 → `provider_not_found` | `{ id, enabled }` | 内置可停用 |
| `provider_model_add` | `{ provider_id, model_id, display_name }` | `provider_id` ULID；不存在 → `provider_not_found`；`model_id` 复用 `validate_model`（≤128）；`display_name` 非空 ≤128 字符；重复 `(provider_id, model_id)` → `invalid_value` | 新建模型（默认启用） | 核心生成 ULID |
| `provider_model_toggle` | `{ provider_id, model_id, enabled }` | `provider_id`/`model_id` 格式校验（`model_id` 复用 `validate_model`）；供应商不存在 → `provider_not_found`；模型不存在 → `provider_model_not_found` | `{ provider_id, model_id, enabled }` | — |

### B.2 既有命令增量（不新增命令）

| 命令 | 增量 | 校验/契约 |
|---|---|---|
| `session_create` | 请求 +可选 `thinking_depth`（0–4） | 越界 → `out_of_range`；类型非法 → `invalid_type`；缺省 = 2；runtime 未 ready（cold/starting）时**接受请求**（会话置 `creating`），ready 后判定能力项（决策 2） |
| `session_send` | 请求 +可选 `thinking_depth`（0–4） | 同上；仅本次 run 覆盖；run 启动时按同一门再判定 |
| `session_create` / `session_send` | 响应 +可选 `warnings: [{ code, field, runtime_id, message }]`（无警告省略） | 目前唯一 code：`thinking_depth_unsupported`（非阻断；`IpcErrorCode` 不增；登记位 = ADR-006 附录 B「警告码」子表）；**尽力而为交付**：同步判定路径随响应返回，延迟判定路径不产生响应警告（见决策 2 v0.4） |
| `session_list` | 响应元素定型为新增 DTO `SessionSummary`（见 B.4），元素 +可选 `workspace_root`（未绑定省略）、+可选 `thinking_depth` | `workspace_root` **只出现在响应**（不进事件 payload）；DTO 由 tauri-specta 生成（T14） |

### B.3 错误码与警告码登记

**错误码（ADR-006 附录 B 增行，`IpcErrorCode` 16 → 20）**：

| code | 语义 | 触发点 | 前端行为 |
|---|---|---|---|
| `builtin_provider_undeletable` | 内置供应商禁止删除 | `provider_delete`（`is_builtin=1`） | 提示「内置供应商不可删除」（删除入口预置灰） |
| `artifact_path_rejected` | 引用路径校验失败（canonicalize / 可访问性 / 探测失败；不含同步盘语义——2026-09-24 裁定不复用 A4 检测） | `artifact_add` | 提示路径不可用 + 原因 |
| `provider_not_found` | 供应商不存在 | 供应商类命令与 `provider_model_*`（按 id 查无） | 刷新供应商列表 + 提示 |
| `provider_model_not_found` | 模型不存在 | `provider_model_toggle` | 刷新表单模型列表 + 提示 |

**警告码（ADR-006 附录 B 新增「警告码」子表——独立登记位）**：

> `warnings[].code` 为独立命名空间（非 `IpcErrorCode`、不入错误码表）；**新增警告码必须走 ADR 在 ADR-006 附录 B「警告码」子表增行**，与错误码登记规则同构（稳定契约、`snake_case`、不得自造）。

| code | 语义 | 触发点 |
|---|---|---|
| `thinking_depth_unsupported` | 运行时未声明 `thinking_depth` 能力，按缺省 2 应用且字段不透传（非阻断） | `session_create`（ready 后判定）/ `session_send`（run 启动判定）；**尽力而为**：同步判定路径随响应返回，延迟判定路径不返回（以生效值回显为准，v0.4） |

> 命令面计数：24 条目 / 25 命令 → **35 条目 / 36 命令**（M4-04 DoD4 同步）。
> 错误码计数：16 → **20**（ADR-006 附录 B 增行 4 条）；警告码子表首项 1 条。

### B.4 `SessionSummary` DTO 登记（新增 IPC DTO；不援引附录 B）

> 裁定（2026-09-24）：`SessionSummary`（IPC DTO）为**新增登记位**——附录 B（设计文档）是事件 payload 扩展策略，不是 IPC DTO 登记位；本 DTO 由 tauri-specta 在 T14 生成（`packages/protocol/src/bindings.ts`，禁止手改）。与 core `event.rs` 的 `SessionSummary`（`session.created` payload 摘要，附录 B 策略承载）同名但不同位：**事件 payload 扩展按设计附录 B 声明（允许新增可选字段）；IPC DTO 以本表为准**。

`session_list` 响应元素（现行响应为 `Session` 域实体序列化；**M3-09 引入具名 DTO（含 `workspace_root`），M3-10 追加 `thinking_depth`**）：

| 字段 | 类型 | 来源 | 说明 |
|---|---|---|---|
| `id` | string（ULID） | `sessions.id` | 既有 |
| `runtime_id` | string | `sessions.runtime_id` | 既有 |
| `workspace_id` | string \| null | `sessions.workspace_id` | 既有 |
| `parent_session_id` | string \| null | `sessions.parent_session_id` | 既有 |
| `title` | string | `sessions.title` | 既有 |
| `status` | SessionStatus | `sessions.status` | 既有（CHECK 枚举） |
| `model` | string \| null | `sessions.model` | 既有（UI-05） |
| `system_prompt` | string \| null | `sessions.system_prompt` | 既有 |
| `config` | object | `sessions.config` | 既有（native_id 等私有映射） |
| `token_usage` | object | `sessions.token_usage` | 既有 |
| `created_at` / `updated_at` / `closed_at` | integer / integer / integer \| null | `sessions.*` | 既有（epoch 毫秒） |
| `thinking_depth` | number（0–4）**可选** | `sessions.thinking_depth`（ADR-010 0003 增） | 会话级档位；归属 M3-10 |
| `workspace_root` | string **可选**（未绑定工作区省略） | `sessions.workspace_id` → `workspaces.root_path`（canonicalize 结果） | UI 面（文件面板「项目文件」tab）；归属 M3-09 |

字段归属拆分（v0.4）：**M3-09 先引入 DTO（含 `workspace_root`），M3-10 在其上追加 `thinking_depth`**；两任务并行时以先落地者为 DTO 宿主，后落地者 rebase 后追加字段；两任务各自执行 T14 `git diff --exit-code` 校验。

## 附录 C：线协议登记（D6）

### C.1 方法参数增量（minor 兼容，major 不变）

| 方法 | 增量 | 缺省 | 不支持时 |
|---|---|---|---|
| `session.create` | 可选 `thinking_depth: 0..=4` | 2 | runtime 未 ready 时接受请求（会话 `creating`）；ready 后判定：未声明 → 不透传、`sessions`/`runs` 落缺省 2、返回 `warnings`（非阻断；**尽力而为**：同步判定随响应，延迟判定不返回，以生效值回显为准） |
| `session.send` | 可选 `thinking_depth: 0..=4`（覆盖会话级，仅本次 run） | 会话级值 | run 启动时判定：未声明 → 不透传、落会话级缺省值、返回 `warnings`（非阻断；**尽力而为**同上）；重放（run_retry/Mode R/N）按会话级值恢复并再判定 |

### C.2 能力项

| 能力项 | 形状 | 声明位置 | 判定 |
|---|---|---|---|
| `thinking_depth` | 字符串项（存在即支持） | `hello.runtime.capabilities` 与 `initialize` 响应 `capabilities`（一致声明） | 核心以 `runtimes.capabilities`（现有落库路径）判定；**判定时机 = runtime ready 后**（`session_create` 未 ready 时请求被接受、会话 `creating`，延迟判定；`session_send` 在 run 启动时判定）；UI 以 `runtimes_list` 判定（预判置灰） |

### C.3 兼容与边界

- 未知字段忽略策略不变（D6）；帧上限不变（2MiB；`artifact_ref` <1MiB）；
- 不新增事件类型（附录 B 不变）；`session.created` payload 可选字段扩展按附录 B 既有声明（事件 payload 策略；IPC 侧 `SessionSummary` DTO 为新增登记位，见附录 B.4）；
- 适配器未声明能力时不得报错或中断 run（不透传 + 警告 + 缺省值落库）。

## 附录 D：《设计文档》v1.10 合入文本（已合入）

**D.1 头部（第 7、10 行）与修订记录（第 30 行 v1.9 行之后追加）**

```diff
-| 文档版本 | **v1.9（冻结）**；…本版含 ADR-001、…、ADR-008、ADR-009 |
+| 文档版本 | **v1.10（冻结）**；…本版含 ADR-001、…、ADR-009、ADR-010 |
-| 状态 | v1.9 已冻结（…ADR-009 升版；…） |
+| 状态 | v1.10 已冻结（2026-09-24 评审裁定，ADR-010 升版；v1.9 = ADR-009、v1.10 = ADR-010；评审记录见各 ADR「评审记录」节） |
+| v1.10 | ADR-010：P0 UI 能力扩展登记——右栏只读文件引用面板（`artifacts` 表 + `artifacts_list`/`artifact_add`/`artifact_remove` + `ref_pick`）；思考深度会话级参数（`session.create`/`session.send` 可选 `thinking_depth`，能力项 `thinking_depth`，`sessions`/`runs` 列，能力门 creating 期延迟判定）；模型与供应商配置（`providers`/`provider_models` + 7 命令，内置播种与删除拒绝，密钥写入路径 `api_key`→keyring→`api_key_ref`，keychain 条目删除限自身命名空间）；命令面 25 → 36、错误码 16 → 20、ADR-006 附录 B 新增警告码子表（详 `docs/adr/ADR-010-p0-ui-capability-registration.md`） |
```

**D.2 执行摘要「当前状态」（第 40 行）**

```diff
-- **当前状态**：**v1.9 已冻结**（2026-09-23 评审裁定，ADR-009 升版，含 ADR-001–009）；实施计划见《实施计划与验收标准》v1.16…
+- **当前状态**：**v1.10 已冻结**（2026-09-24 评审裁定，ADR-010 升版，含 ADR-001–010）；实施计划见《实施计划与验收标准》v1.17…
```

**D.3 §2.3 表格（第 210 行多视图行）**

```diff
-| 多视图 | 单窗口 + 会话列表切换 | 无标签/分屏 | P1 | 日常使用多会话切换疲劳 |
+| 多视图 | 单窗口 + 会话列表切换 + 右栏只读文件引用面板（ADR-010：引用持久化；不列目录/不预览） | 无标签/分屏 | P1 | 日常使用多会话切换疲劳 |
```

**D.4 D6 方法表（第 399、400 行）与实现要点**

```diff
-    | `session.create` | 30s | 返回原生会话 id |
-    | `session.send` | 30s（ack 快返回，不等模型） | `client_msg_id` 幂等 |
+    | `session.create` | 30s | 返回原生会话 id；可选 `thinking_depth`（0–4，缺省 2，ADR-010） |
+    | `session.send` | 30s（ack 快返回，不等模型） | `client_msg_id` 幂等；可选 `thinking_depth`（覆盖会话级，仅本次 run，ADR-010） |
+  - 能力项（ADR-010）：适配器以 `hello.runtime.capabilities` / `initialize` 的字符串项 `thinking_depth` 声明支持；判定时机 = runtime ready 后（`session_create` 未 ready 时接受请求、会话 `creating`；`session_send` 在 run 启动时）；未声明 → 按缺省 2 应用、不透传，并返回非阻断警告（`warnings`，尽力而为交付：延迟判定路径不产生响应警告）。
```

**D.5 D7 命令面（第 448 行 `messages_page` 行之后追加；计数行同步）**

```diff
+  - `ref_pick`（ADR-010）：`{ kind: "file" | "directory" }`；Rust 侧系统选择器（复用 M1-06 `DirectoryPicker` 抽象并扩展文件选择；不新增 WebView capability）；返回 `{ path: string | null }`（null=取消）；
+  - `artifacts_list` / `artifact_add` / `artifact_remove`（ADR-010）：会话文件/目录引用（`artifacts` 表；`artifact_add` 走 canonicalize + 可访问性检查（2026-09-24 裁定：不复用 A4 同步盘检测），失败 `artifact_path_rejected`；`artifact_remove` 幂等）；
+  - `providers_list`（ADR-010）：无参数；返回供应商与模型清单（含 `api_key_ref` 引用；`api_key` 本体不回，D10）；
+  - `provider_create` / `provider_update` / `provider_delete` / `provider_toggle`（ADR-010）：供应商增改删与启停；删除内置拒绝（`builtin_provider_undeletable`）；`api_key` 明文仅传输 → 核心写 keyring（A3 降级走加密文件，M1-07 口径）→ 存/返回 `api_key_ref`；`type` 创建后不可改；keychain 条目删除限自身命名空间（ADR-010 决策 3）；
+  - `provider_model_add` / `provider_model_toggle`（ADR-010）：模型增改与启停（无删除命令）；
+  - 连通性测试（ADR-010）：P0 无命令（UI 入口 toast「连通性测试将在 P1 开放」）；P1 实做须先登记命令（ADR）；
+  - `session_create` / `session_send`（ADR-010）：请求新增可选 `thinking_depth`（0–4；越界 `out_of_range`）；响应新增可选 `warnings`（`thinking_depth_unsupported`，非阻断；登记位 = ADR-006 附录 B「警告码」子表；尽力而为交付，延迟判定路径不产生响应警告，见决策 2）；
+  - `session_list`（ADR-010）：响应元素定型为新增 DTO `SessionSummary`（ADR-010 附录 B.4；tauri-specta 生成），元素新增可选 `workspace_root` / `thinking_depth`（UI 面；事件 payload 不变）；
+  - 命令面计数（ADR-010）：24 条目 / 25 命令 → 35 条目 / 36 命令；错误码 16 → 20（ADR-006 附录 B 增行 4 条）+ 警告码子表新增。
```

**D.6 D10 实现要点（第 538 行后追加）**

```diff
+  - 供应商密钥（ADR-010；写入路径随本 ADR 登记）：设置页表单 `api_key` 明文输入（仅传输，掩码 + 显示/隐藏）；核心经 `aether-security` 写入 keyring（`keychain://aether/provider/<id>`；A3 降级走加密文件，M1-07 口径）后返回/落库 `api_key_ref`；响应只回 `api_key_ref`（引用，编辑表单只读展示）；`api_key` 本体不落库、不进日志/诊断包/导出/测试夹具（脱敏扫描 `sk-`/`eyJ`/PEM 0 命中）；keychain 条目删除限自身命名空间（`provider_delete` / `provider_update` 空串清除）；不经适配器注入（消费映射 P1 另立 ADR）。
```

**D.7 附录 C DDL（sessions 第 818 行 `model` 行后、runs 第 852 行 `input_message_id` 行后、settings 表后追加）**

```diff
   model             TEXT,                       -- UI-05 会话级模型覆盖
+  thinking_depth    INTEGER NOT NULL DEFAULT 2, -- ADR-010（0003 增）：会话级思考深度 0–4
@@
   input_message_id TEXT,
+  thinking_depth   INTEGER,                     -- ADR-010（0003 增）：该 run 生效值；历史行为 NULL
@@
+-- ===== ADR-010（0003 增）：会话引用与模型/供应商配置 =====
+CREATE TABLE artifacts (
+  id          TEXT PRIMARY KEY,
+  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
+  path        TEXT NOT NULL,                    -- canonicalize 后的绝对路径
+  kind        TEXT NOT NULL DEFAULT 'file',     -- file | directory（目录不递归；应用层校验）
+  size_bytes  INTEGER,
+  created_at  INTEGER NOT NULL,
+  UNIQUE (session_id, path)
+);
+CREATE TABLE providers (
+  id           TEXT PRIMARY KEY,
+  name         TEXT NOT NULL,
+  type         TEXT NOT NULL,                   -- anthropic | openai | deepseek | google | custom（应用层校验）
+  base_url     TEXT,
+  api_key_ref  TEXT,                            -- keychain://aether/<service>/<key>，只存引用（D10）
+  enabled      INTEGER NOT NULL DEFAULT 1,
+  is_builtin   INTEGER NOT NULL DEFAULT 0,      -- 内置（迁移播种 4 条）；不可删除
+  created_at   INTEGER NOT NULL,
+  updated_at   INTEGER NOT NULL
+);
+CREATE TABLE provider_models (
+  id           TEXT PRIMARY KEY,
+  provider_id  TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
+  model_id     TEXT NOT NULL,
+  display_name TEXT NOT NULL,
+  enabled      INTEGER NOT NULL DEFAULT 1,
+  created_at   INTEGER NOT NULL,
+  UNIQUE (provider_id, model_id)
+);
```

**D.8 附录 E 映射表（EventEnvelope 行（第 1044 行）之前追加行）**

```diff
+| **Artifact.*（会话引用）** | 不经线协议（P0 仅 IPC；D6 `artifact_ref` 为适配器附件引用，两者互不影响） | `artifacts.*` | 路径为 canonicalize 结果（ADR-010） |
+| **Session.thinking_depth** | `session.create` 参数；`session.created` payload 可选字段（附录 B 允许；IPC 侧 `SessionSummary` DTO 见 ADR-010 附录 B.4） | `sessions.thinking_depth` | 0–4；缺省 2（ADR-010） |
+| **Run.thinking_depth** | `session.send` 参数（可选覆盖） | `runs.thinking_depth` | 该 run 生效值；历史行 NULL（ADR-010） |
+| Provider.* | 不经线协议（P0 仅 IPC；适配器消费映射另立 ADR） | `providers.*` | `api_key_ref` 只存引用（D10）；`is_builtin` 不可删（ADR-010） |
+| ProviderModel.* | 不经线协议 | `provider_models.*` | `UNIQUE(provider_id, model_id)`（ADR-010） |
```

## 附录 E：《实施计划与验收标准》v1.17 合入文本（已合入）

**E.1 头部（第 1、3、7–9 行）与 §8（第 626 行）**

```diff
-# Aether 实施计划与验收标准 v1.16
+# Aether 实施计划与验收标准 v1.17
-（基线：《Aether 设计文档》 v1.9（冻结），含 ADR-001–ADR-009）
+（基线：《Aether 设计文档》 v1.10（冻结），含 ADR-001–ADR-010）
-| 计划版本 | v1.16 |
+| 计划版本 | v1.17 |
-| 状态 | v1.16 已修订（2026-09-23 ADR-009 合入：…） |
+| 状态 | v1.17 已修订（2026-09-24 ADR-010 合入：新增 M3-09/M3-10/M3-11；Gate 3/M4-04 计数同步；任务进度可更新，结构与门禁变更走 ADR） |
-| 基线 | 设计文档 v1.9（冻结，含 ADR-001–ADR-009）；需求文档 v0.6… |
+| 基线 | 设计文档 v1.10（冻结，含 ADR-001–ADR-010）；需求文档 v0.7… |
+- **变更控制**：设计文档已冻结 v1.10（含 ADR-001–ADR-010）；……
```

**E.2 修订记录（第 35 行 v1.16 行之后追加）**

```diff
+| v1.17 | 同步设计文档 v1.10（ADR-010）：新增 M3-09 文件引用面板（只读，`artifacts` 表）/ M3-10 思考深度 / M3-11 模型与供应商配置（G8 并行组，依赖 M3-01/M3-02）；Gate 3 覆盖新任务；M4-04 DoD4 命令面 35 条目/36 命令；§1 汇总 40 条目/实际 39；§7 映射追加 |
```

**E.3 新增任务（§4 M3，M3-08 之后追加）**

```markdown
#### M3-09 文件引用面板（只读）
- 对应决策：D3、D7、D9、ADR-010 决策 1/4｜依赖：M3-01、M3-02、M1-06（选择器）｜量级：M
- 描述：右栏「文件」只读引用面板（会话文件/项目文件子 tab、`artifacts_list`/`artifact_add`/`artifact_remove`、`ref_pick`、空态、折叠）；不列目录、不预览、不读内容。
- DoD：
  1. 迁移 0003 `artifacts` 表/约束断言（`UNIQUE(session_id, path)`、`ON DELETE CASCADE`）；写路径经单写队列（D3）；
  2. `artifact_add`：canonicalize + 可访问性检查（评审裁定：不复用 A4 同步盘检测；canonicalize 失败/stat 探测失败 → `artifact_path_rejected`；合法路径不误拒）；文件/目录 `kind` 探测；重复添加幂等；
  3. `artifact_remove` 幂等（不存在 `removed=false`）；`artifacts_list` 排序与形状断言；跨重启保留（重启后列表一致）；
  4. `ref_pick`：`{ kind }` 严格解析（未知 kind → `invalid_enum`、未知成员拒绝）；取消返回 `{ path: null }`；E2E 注入替身；
  5. E2E：添加文件 → 出现在「会话文件」；附加文件夹 → 出现在「项目文件」；切换会话隔离；空态文案；`改动` tab 无入口；
  6. 边界断言：无目录列举/文件读取 IPC 调用；不触发 `workspace_set`；不产生事件（事件表行数不变）；
  7. 折叠/断点行为按 UI-UX 规格（≥1280 展开、<1280 抽屉）；
  8. T14 生成物更新（`ref_pick`/`artifacts_list`/`artifact_add`/`artifact_remove` + 引入 `SessionSummary` DTO（含 `workspace_root`））。

#### M3-10 思考深度（会话级参数）
- 对应决策：D4、D6、D7、ADR-010 决策 2/4｜依赖：M1-03、M1-09、M2-01、M2-02、M2-11、M3-02｜量级：L
- 描述：协议可选字段（`session.create`/`session.send`）、`sessions`/`runs` 列（迁移 0003）、能力项 `thinking_depth`、IPC DTO/响应 `warnings`、UI 滑块（能力置灰）。
- DoD：
  1. 迁移 0003：列断言；0001→0003 幂等；历史 run 行为 NULL；
  2. 透传：`session_create`/`session_send` 带 `thinking_depth` → 适配器收到（Mock 回显断言）；缺省 = 2；`session_send` 覆盖仅本次 run；`runs.thinking_depth` 落库生效值；`sessions.thinking_depth` 恢复一致；
  3. 能力门：判定时机断言——runtime 未 ready 时 `session_create` 接受请求（会话 `creating`），ready 后判定；运行时未声明 `thinking_depth` → 字段不透传、`sessions`/`runs` 落缺省 2；**同步判定路径**响应含 `warnings[0].code="thinking_depth_unsupported"`（非阻断，run 正常终态）；**延迟判定路径**无响应警告、以 `SessionSummary.thinking_depth` 回显生效值 2（断言）；UI 滑块置灰 + tooltip（E2E）；
  4. 重放口径：`run_retry` / Mode R/N 重放按会话级值恢复，run 启动时重新执行能力门判定（断言）；
  5. 校验矩阵：0–4 合法；5 / -1 / 1.5 / "高" → `out_of_range` / `invalid_type`（不落库、不透传）；
  6. 三官方适配器能力声明与档位映射记录（支持者映射 token 预算；不支持者不声明）；Mock 声明支持供 CI；
  7. T14 生成物更新（在 M3-09 引入的 `SessionSummary` DTO 上追加 `thinking_depth`；`warnings` 响应）。

#### M3-11 模型与供应商配置（P0 UI + 数据模型）
- 对应决策：D3、D7、D10、ADR-010 决策 3/4｜依赖：M1-03、M1-07、M3-01、M3-02｜量级：L
- 描述：`providers`/`provider_models`（迁移 0003 + 内置播种）、7 条命令、密钥写入路径（`aether-security`）、设置页 UI（原型 renderModels/renderProviderForm）、模型选择器派生。
- DoD：
  1. 迁移 0003 表/约束断言（`UNIQUE(provider_id, model_id)`、`ON DELETE CASCADE`、内置 4 条播种且 `enabled=0`）；写路径经单写队列（D3）；
  2. 命令矩阵：未知成员 / 非法 type / base_url 格式 / custom 缺 base_url / `api_key` 非空 >8192（`too_large`）/ 重复 model_id → 结构化错误且不落库；
  3. 内置约束：`provider_delete` 内置 → `builtin_provider_undeletable`；`provider_toggle` 内置可停用；UI 删除入口置灰；
  4. 引用与密钥：`provider_create`/`provider_update` 携带 `api_key` → `aether-security` 写 keyring（A3 降级走加密文件）→ 响应/落库 `api_key_ref`（`keychain://aether/provider/<id>`）；IPC 响应/日志/诊断包/导出 `api_key` 明文 0 命中（扫描断言，含 `sk-`/`eyJ`/PEM）；`api_key` 三态（缺省/空串/覆盖）；`providers_list` 含 `api_key_ref` 且不含 `api_key`；keychain 条目删除限自身命名空间（共享引用不删；不可用忽略 + 诊断不阻断，断言）；
  5. 「测试连接」：点击 → toast「连通性测试将在 P1 开放」（E2E；断言无 IPC 调用、无网络请求）；
  6. 模型选择器 E2E：仅启用供应商的启用模型出现；停用后移除；选中失效回退；空态文案；
  7. 会话透传：选择模型 → `session.create.model` 断言（UI-05 既有路径）；运行期只读（无 update 入口）；
  8. T14 生成物更新。
```

**E.4 §1 同步**

```diff
 ├─ M3-07 审计最小集                      │
 ├─ M3-08 工作区记忆             M4-05 打包发布 ★
+├─ M3-09 文件引用面板（ADR-010）          │
+├─ M3-10 思考深度（ADR-010）              │
+├─ M3-11 供应商配置（ADR-010）            │
@@
-| G8 | M3-03、M3-04、M3-05、M3-07、M3-08 | 前端/数据面并行，并行于 M3-02；M3-08 依赖 M3-02、M2-03，可与 M3-03 并行 |
+| G8 | M3-03、M3-04、M3-05、M3-07、M3-08、**M3-09、M3-10、M3-11**（ADR-010） | 前端/数据面并行，并行于 M3-02；M3-08 依赖 M3-02、M2-03；M3-09/M3-10/M3-11 依赖 M3-01/M3-02 |
@@
-| Gate 3（M3 出口） | M3 全部 DoD + T4/T9/T12/T13/T14 通过 | … |
+| Gate 3（M3 出口） | M3 全部 DoD（含 ADR-010 的 M3-09/M3-10/M3-11）+ T4/T9/T12/T13/T14 通过 | … |
@@
-文档条目 **37 项 = M1 11 + M2 12 + M3 8 + M4 6**；…实际执行口径为 36 项…
+文档条目 **40 项 = M1 11 + M2 12 + M3 11 + M4 6**（ADR-010 增 M3-09/M3-10/M3-11）；…实际执行口径为 39 项…
+量级分布（互斥执行口径 39 项）：S 3 / M 20 / L 16（ADR-010 增 M3-09=M、M3-10=L、M3-11=L）。
```

**E.5 M4-04 DoD4（第 513 行）与 §7 映射（第 612 行后追加）**

```diff
-  4. 命令面完整性：D7 P0 命令面全集（24 条目 / 25 个可调用命令；…）逐条登记验收清单并验证可调用…
+  4. 命令面完整性：D7 P0 命令面全集（**35 条目 / 36 个可调用命令**；…；ADR-010 十一命令：`ref_pick`、`artifacts_list`/`artifact_add`/`artifact_remove`、`providers_list`/`provider_create`/`provider_update`/`provider_delete`/`provider_toggle`/`provider_model_add`/`provider_model_toggle`）逐条登记验收清单并验证可调用；错误码全集 20 个（ADR-006 附录 B 增行 4 条）逐条分支验证；警告码子表（ADR-006 附录 B 新增）首项 `thinking_depth_unsupported` 分支验证…
@@
+| M3-09 | D3、D7、D9、ADR-010 | UI-07 | 面板 E2E；`artifact_add` 路径拒绝；引用持久化；不触发 workspace_set |
+| M3-10 | D4、D6、D7、ADR-010 | UI-08、RA-04 | 透传/覆盖/能力门（creating 期延迟判定）/重放/校验矩阵；迁移 0003 |
+| M3-11 | D3、D7、D10、ADR-010 | UI-09 | 命令矩阵；内置删除拒绝；`api_key` 明文 0 命中；keychain 删除规则；选择器 E2E；迁移 0003 |
```

## 附录 F：《需求文档》v0.7 合入文本（已合入）

**F.1 头部（第 5、6 行）与修订记录（第 15 行 v0.6 行之后追加）**

```diff
-版本：v0.6（…同步设计文档 v1.8、实施计划 v1.14、ADR-008）
+版本：v0.7（…同步设计文档 v1.10、实施计划 v1.17、ADR-010）
-状态：v0.6 已冻结（2026-09-22 评审批准，ADR-008 升版：P0 三运行时口径）；后续变更一律走 ADR 流程
+状态：v0.7 已冻结（2026-09-24 评审裁定，ADR-010 升版：P0 UI 能力扩展登记——文件引用面板/思考深度/模型与供应商配置）；后续变更一律走 ADR 流程
+v0.7（2026-09-24）	同步设计文档 v1.10 / 实施计划 v1.17 / ADR-010：新增 UI-07 文件引用面板（P0 只读，引用持久化）、UI-08 思考深度（P0 会话级参数）、UI-09 模型与供应商配置（P0 部分）；§3.2 RA-04 补能力项 `thinking_depth`；§6 数据模型补 Artifact/Provider/ProviderModel 与 Session.thinking_depth
```

**F.2 §3.3 表格追加**

```diff
+| UI-07 | 文件引用面板 | 当前会话文件/目录引用与工作区根路径（只读展示 + 增删引用） | P0：`artifacts_list`/`artifact_add`（canonicalize + 可访问性检查）/`artifact_remove` + `ref_pick`；不列目录、不预览 |
+| UI-08 | 思考深度 | 会话级扩展思考档位（关闭/低/高/极高/最大） | P0：`session.create`/`session.send` 透传；运行时能力声明驱动 UI 置灰（ready 后判定，未支持按缺省 2 + 非阻断警告；警告尽力而为交付） |
+| UI-09 | 模型与供应商配置 | 供应商增删改查 + 模型增改与启停 + 模型选择器 | P0：UI + 数据模型（内置播种、内置不可删）+ 密钥写入路径（`api_key` 明文仅传输 → 核心写 keyring → `api_key_ref`）；连通性测试入口仅 toast（P1 登记命令）；无远程获取；适配器消费映射另立 ADR |
```

**F.3 §3.8 表格追加**

```diff
+| UI-07 | 文件引用面板 | P0（只读）/ P1+（浏览/预览） | 部分 | 引用持久化（`artifacts` 表）+ `ref_pick`；不预览、不绕过权限门（ADR-010） |
+| UI-08 | 思考深度 | P0 | 是 | 会话级 0–4（默认 2）+ 适配器能力项；不支持时置灰（ADR-010） |
+| UI-09 | 模型与供应商配置 | P0（UI+数据模型+密钥写入）/ P1（消费映射） | 部分 | `providers`/`provider_models` + 7 命令；内置不可删；密钥写入 `api_key`→keyring（D10）；连通性 toast（ADR-010） |
@@ RA-04 行 P0 口径补：握手上报（不用于路由）；能力项含 `thinking_depth`（ADR-010）
```

**F.4 §6 数据模型概要追加**

```diff
+Artifact：id, session_id, path（canonicalize）, kind（file/directory）, size_bytes, created_at
+
+Provider：id, name, type, base_url, api_key_ref（仅引用，D10）, enabled, is_builtin（内置不可删）
+
+ProviderModel：id, provider_id, model_id, display_name, enabled
+
+Session 补：thinking_depth（会话级思考深度 0–4，默认 2，ADR-010）
```

## 附录 G：原型对照表与 E2E 锚点登记

### G.1 `deepseek_html_V0.0.1.html` 元素 → P0 处置

| 原型元素 | P0 处置 | 依据 |
|---|---|---|
| 右栏 `文件` tab + 搜索 + 会话文件/项目文件 + 添加文件/附加文件夹 + `＋` 菜单 | **采纳（只读 + 增删引用）** | 决策 1 |
| 右栏 `改动` tab | **隐藏**（无入口） | 决策 1 |
| 拖拽文件到输入框提示 | **隐藏**（P1+） | 决策 1、§5-4 |
| 思考深度滑块（5 档 + 刻度点击） | **采纳**；不支持能力时置灰 + tooltip | 决策 2 |
| 输入区模型选择器（分组/搜索/空态/选中回退） | **采纳**；作用于新建会话（`session.create.model`），会话内只读 | 决策 3、§5-1 |
| 供应商列表（开关/编辑/删除确认/描述） | **采纳**；内置删除置灰（`builtin_provider_undeletable`） | 决策 3/4 |
| 添加供应商 / 添加自定义供应商 / 表单（名称/BaseURL/Key/启用/模型管理/添加模型） | **采纳**；`API Key` 字段 = `api_key` 明文输入（掩码 + 显示/隐藏；仅传输，核心写 keyring 后只存引用）；编辑态显示 `api_key_ref` 只读 + 可选覆盖输入 | 决策 3 |
| 表单内 `测试连接` | **采纳（入口）**；P0 无命令——点击显示 toast「连通性测试将在 P1 开放」（P1 实做须先登记命令） | 决策 3/4、§5-7 |
| 表单内 `从供应商获取` | **不提供入口**（P1） | §5-7 |
| 输入区权限选择器（仅查看/工作区内修改/完全权限） | **未登记**（D9 固定矩阵不变；如落地须另立 ADR） | §5-5 |
| Agent/Chat 模式切换（Chat 占位） | **未登记**（P2 多 Agent，§4#2） | §5-5 |
| 工作区树 / 新建工作区 | **未登记**（P1 多视图） | §5-5 |
| 设置页 基本设置（语言/主题） | **未登记**（§4#25，P2） | §5-5 |
| Agent 预设页 | **未登记**（P1+） | §5-5 |
| 语音 / 附件按钮 | **未登记**（P2 多模态，§4#19） | §5-5 |
| 关于页 | 既有 S-10（M3-05），不属本 ADR | UI-UX 规格 §1.2 |

### G.2 E2E testid 登记（UI-UX 规格 §7.3 待实现锚点增量）

| 界面 | data-testid | 关键状态属性 |
|---|---|---|
| 文件面板 | `file-panel` / `file-panel-empty` / `file-panel-session-tab` / `file-panel-project-tab` | — |
| 文件引用项 | `ref-item` / `ref-remove` | `data-ref-kind`（session/project）、`data-artifact-id`、`data-path` |
| 引用操作 | `ref-add-file` / `ref-add-folder` / `ref-pick-error` | — |
| 思考深度 | `thinking-slider` / `thinking-disabled-hint` / `thinking-popover` | `data-value`、`data-enabled` |
| 供应商页 | `providers-page` / `provider-card` / `provider-toggle` / `provider-edit` / `provider-delete` / `provider-test`（点击 toast「连通性测试将在 P1 开放」；无 IPC 调用） | `data-provider-id`、`data-enabled`、`data-is-builtin` |
| 供应商删除确认 | `provider-delete-confirm` | — |
| 供应商表单 | `provider-form` / `provider-name-input` / `provider-base-url-input` / `provider-api-key-input`（明文输入，掩码）/ `provider-api-key-ref-readonly`（编辑态只读引用）/ `provider-enabled-switch` / `provider-form-save` / `provider-form-back` | — |
| 供应商模型 | `provider-model-item` / `provider-model-toggle` / `provider-model-add` | `data-model-id`、`data-enabled` |
| 模型选择器 | `model-selector` / `model-selector-item` / `model-selector-empty` | `data-provider-id`、`data-model-id` |

> 命名与断言规则沿用 UI-UX 规格 §7.1/§7.4（kebab-case、状态入 `data-*`、生产构建存在）。

## 附录 H：任务与证据索引（实现后填）

| 内容 | 路径（预期） |
|---|---|
| 迁移 0003 | `migrations/0003_p0_ui_extensions.sql` |
| 命令层（11 命令 + DTO + 错误码/警告码） | `crates/aether-tauri/src/ipc/{commands,dto,backend,error}.rs`；`packages/protocol/src/bindings.ts`（生成物） |
| 思考深度（协议/能力/落库） | `crates/aether-adapters/src/{protocol,session_client}.rs`；`crates/aether-store/`；各适配器包 `packages/adapter-*/` |
| 会话引用（存储/路径校验） | `crates/aether-store/`；`crates/aether-tauri/src/ipc/path.rs`（canonicalize 复用；2026-09-24 裁定：不复用 A4 同步盘检测） |
| 供应商（存储/密钥写入） | `crates/aether-store/`；`crates/aether-security/`（keyring 写入/删除；A3 降级加密文件，M1-07 口径） |
| UI | `apps/desktop/src/`（文件面板 / 思考深度 / 供应商页 / 模型选择器） |
| E2E 与脚本 | `scripts/test/m3-09/`、`scripts/test/m3-10/`、`scripts/test/m3-11/`；`apps/desktop/src/*.test.tsx` |
| 任务证据 | `docs/M3-09-证据.md`、`docs/M3-10-证据.md`、`docs/M3-11-证据.md`（实现后归档） |
