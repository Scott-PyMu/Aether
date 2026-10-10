# ADR-024：供应商消费映射、连通性测试与引用注入（D10/D7/D6；P1-A10）

| 项 | 内容 |
|---|---|
| 编号 | ADR-024（P1-A10） |
| 版本 | **v0.2（批次评审通过稿）**：v0.1 创建（批次稿）；v0.2 按 `docs/M5-01-ADR批次评审意见.md` 修订闭环（P0/P1/P2 相关条目全部闭环；不改决策） |
| 状态 | **已批准（2026-10-09；人工授权 AI 审批——独立评审通过即批准，见 `docs/M5-01-ADR批次评审意见.md` §7）**；已合入《设计文档》v1.16 |
| 决策日期 | 2026-10-09 |
| 决策载体 | 《设计文档》v1.15 → v1.16（D6/D7/D10 增量、附录 A UI-07/UI-09 行、附录 E 补行）；`docs/P1-实施计划与验收标准.md` v0.7（§9 行标注）；UI-UX §7.3 锚点 `provider-test-result`（既有）/`file-reference`（本批次登记） |
| 关联 | 设计 D6/D7/D10、附录 A UI-07/UI-09、附录 C-3/C-4/C-5（ADR-014）、附录 E；需求 UI-07/UI-09；ADR-002（DSH 插件）、ADR-008（三运行时/DSH provider 分层）、ADR-010（密钥写入路径与未决项 §5-2/§5-4/§5-7/§5-9）、ADR-014（命令/错误码/超时/投影登记）、ADR-015（注册清单）、ADR-018（`provider_error` 汇总登记）、ADR-022（模型路由条件）；P1 计划 M7-06 |
| 取代 / 被取代 | 无（新增；ADR-014 附录 C-3/C-4/C-5 的**实测义务**由本 ADR 承接） |
| 回退条件 | 见 §6 |
| 实现状态 | 未开始（登记先行；由 M7-06 执行） |

## 1. 背景

- ADR-010 §5-2/§5-4/§5-7 未决项：供应商记录 → 适配器消费映射（env/配置注入、Codex/DSH 配置物化、模型路由）、引用注入（`session.send.references` 载荷形态）、真实连通性测试（命令、网络请求、密钥使用、错误呈现、超时）。
- ADR-014 已登记：命令 `provider_test`/`provider_models_fetch`、超时 10s/连接 5s、错误码 `provider_unreachable`/`provider_auth_failed`（C-5）、档位 → 适配器启动参数投影表（C-3，待实测断言）、出站不做限制（R6，风险留痕）、引用注入并入 M7-06（`session.send.references`）。
- P1 计划 M7-06 DoD：消费映射形态逐条登记；密钥注入路径与 0 命中脱敏；三运行时真实消费证据；DSH 按会话选模型（核验 + 未实现则实现）；`provider_test`/`provider_models_fetch` 五路径错误码；引用注入（≤32、canonicalize、`artifact_path_rejected`、三运行时映射、引用 ≠ 预授权）；UI 三态与幂等；投影映射实测断言（不等价 → 置灰/降级）。

## 2. 决策

### 2.1 核心决策（五段式）

- **决策**：
  1. **消费映射形态（运行时 × 供应商；密钥只经 env，不落盘）**：
     - **Claude Code**：spawn 前经 `aether-security` 读取所选 provider 的 `api_key_ref` → 子进程 env **`ANTHROPIC_API_KEY`**（一次性注入）+ **`ANTHROPIC_BASE_URL`**（provider.base_url；缺省不注入）；模型经 `--model`（会话级 `model` 覆盖）。
     - **Codex**：物化隔离 `CODEX_HOME` 内的 `config.toml`——`model_providers` 段写 `base_url` 与 **env 键名**（`env_key`），**不写密钥值**；spawn env 注入对应的 `OPENAI_API_KEY`（或 provider 自定义 env 名）；模型写 `model` 字段/`-m`。
     - **DSH**：provider 分层配置（profile 目录 `config`：base_url/model/key env 名）；按会话选模型（`session.create.model`）生效（现状核验在 M7-06 DoD4；未实现则本任务内补齐并断言）。
     - **共性**：仅使用**已启用** provider/model；无匹配 provider → 不注入（运行时按各自官方默认）；env 白名单按「适配器 × provider」裁剪（D10）；**密钥值不进入任何落盘配置**（Codex config.toml 只含 env 键名）。
  2. **无密钥不发请求**：`provider_test`/`provider_models_fetch` 前置校验 `api_key_ref` 存在且可读（A3 降级路径可用）；缺失 → `invalid_value`（field=`api_key_ref`，提示先配置密钥），**不发任何请求**。
  3. **连通性测试（五路径）**：请求总超时 **10s**、连接超时 **5s**（常量入 M7-06 证据）；路径与错误映射：
     | 路径 | 结果码 |
     |---|---|
     | 成功（2xx + 可解析模型/响应） | 成功回执（含延迟与模型数） |
     | 密钥无效（401/403） | `provider_auth_failed` |
     | 网络不可达 / DNS 失败 / 连接超时 / 总超时 | `provider_unreachable` |
     | 供应商错误（其余 4xx/5xx） | **`provider_error`**（message 含供应商状态码；不重试） |
     | 请求未发出（无密钥） | `invalid_value`（field=`api_key_ref`） |
     出站不做限制（R6，风险接受留痕）；响应与日志不含密钥本体。
  4. **模型列表获取**：`provider_models_fetch` 拉取供应商模型 → **归一化**（id `trim`、去重、空串剔除、长度 ≤128 校验）→ 经 `provider_model_add` **幂等** upsert（`UNIQUE(provider_id, model_id)`；已存在仅刷新 `display_name`）；"从供应商获取"入口复用该命令。
  5. **引用注入**：`session_send.references: [{ path, kind }]`（≤32 条；canonicalize + 可访问性校验；失败 → `artifact_path_rejected`）；**统一映射 = 消息前缀引用块**（`<aether-references>` 路径清单 + 「按需读取」指令；三运行时同一形态，映射表见附录 A-3）；**核心不读取引用文件内容**；**引用 ≠ 预授权**（Agent 读取仍走 `fs.read` 权限门，D9 不变）。
  6. **档位投影实测义务**：ADR-014 附录 C-3 三运行时 × 三档映射表由 M7-06 DoD1 逐条**实测断言**；Claude `acceptEdits` 工作区边界、DSH headless preset 等价性**无实测证据 = 不等价** → 该运行时该档位置灰/降级并登记（ADR-014 §6-4），禁止静默按「部分等价」实现。
- **实现要点**：
  1. 密钥注入实现挂在既有 spawn 路径（D10「spawn 前读取并注入 env（一次性），子进程 env 白名单化」）；env 名清单见附录 A-1。
  2. `provider_test` 请求形态按 provider `type`（anthropic/openai/deepseek/google/custom）：`GET {base_url}/models`（无 base_url 时按类型默认端点）；响应仅取必要字段。
  3. 脱敏：日志/诊断包/导出/测试夹具扫描 `sk-`/`eyJ`/PEM **0 命中**（D10 既有守门复用）。
  4. UI：测试连接按钮改为真实调用（三态 + 锚点 `provider-test-result`，ADR-014 已登记）；引用注入入口（拖拽 + `@`；锚点 `file-reference`，本批次登记）。
  5. DSH 按会话选模型：核验结论与实现证据记入任务证据（不挂历史遗留出处）。
- **主要风险**：密钥经 env 传给子进程（进程内可读，D10 已知边界）；Codex config.toml 误写密钥值；出站不受限（R6）被用于内网探测；三运行时映射不等价导致档位投影失效（假安全）；引用注入被误当预授权；供应商错误码语义漂移。
- **缓解措施**：落盘扫描守门（配置文件 0 密钥命中）；env 白名单裁剪；投影不等价置灰/降级；文档与 UI 明确「引用 ≠ 授权」；五路径错误矩阵逐条测试；风险接受留痕（R6）随本 ADR 归档。
- **失效条件**：任一运行时「配置供应商 → run 实际未使用该配置」→ 该运行时消费映射置灰并复审；投影实测不等价 → 按 ADR-014 §6-4 置灰/降级；密钥出现在任何落盘/导出产物 → 立即停用消费映射并另立 ADR；`session.send.references` 导致任一官方运行时失败率上升 → 撤回字段透传（协议字段保留），引用仅作 UI 登记（ADR-014 §6-8）。

### 2.2 明确不做（P1 边界）

- 不做模型删除命令（ADR-010 §5-8，P2 另议）；
- 不做运行期切模型/切运行时（ADR-010 §5-1，P2）；
- 不做出站限制/代理 ACL（R6 明确不限制；如产品反悔按 ADR-014 §6-5 处理）；
- 不做密钥轮换/多密钥池（P3+）。

## 3. 影响

### 3.1 《设计文档》v1.15 → v1.16

- D10：补「消费映射（P1，ADR-024）」要点（三运行时 env/配置物化、无密钥不发请求、五路径错误矩阵）。
- D6：`session.send.references` 映射形态登记（统一前缀引用块）；`session.create.model` 按会话生效口径（ADR-010 §5-9 延续）。
- D7：供应商连通性命令语义与错误码（ADR-018 汇总）；`provider_error` 增行。
- 附录 A `UI-07`/`UI-09` 行：补注 ADR-024 落点。
- 附录 E：补 `Provider 消费映射` 行（不经线协议；spawn 注入）与 `references` 载荷行。

### 3.2 `docs/P1-实施计划与验收标准.md` v0.6 → v0.7

- §9 P1-A10 行标注正式编号 **ADR-024（2026-10-09）**。

### 3.3 UI-UX 设计规格 v0.6 → v0.7

- §7.3 锚点 `file-reference` 登记（M7-06；十四组之一，见 ADR-021 §3.3）。**责任口径（批次评审 P1-5 统一）**：正式锚点契约由 **M5-01 批次登记**（UI-UX v0.7）；**实现与断言由 M7-06 承担**（重命名视为破坏性变更，§7.1 规则 5）。

### 3.4 不影响的

- 不改 D6 帧上限与 major；不改 D9 权限门（引用不产生预授权）；不改 D10 密钥存储面；不新增命令（`provider_test`/`provider_models_fetch` 已登记）；不新增依赖；不改迁移 0001–0003；不改 P0 T1–T15。

## 4. 版本（拟合入）

| 文档 | 修订前 | 修订后 |
|---|---|---|
| 《设计文档》 | v1.15 | **v1.16** |
| 《P1-实施计划与验收标准》 | v0.6 | **v0.7** |
| 《UI-UX 设计规格》 | v0.6 | **v0.7** |

## 5. 后续（未决项）

1. 模型命名归一化的跨供应商统一（别名表）——P2+。
2. 运行期切模型/切运行时与模型删除——P2（ADR-010 §5-1/§5-8）。
3. DSH 多 profile——P1 评估（评估记录；实施须 ADR）。

## 6. 回退条件

1. **本 ADR 被否决** → 消费映射与连通性测试撤回；UI 恢复占位（不得以 toast 谎称可用）；引用注入仅保留 UI 侧登记。
2. **投影不等价** → 该运行时该档位置灰/降级并登记（ADR-014 §6-4）；不得静默按部分等价实现。
3. **密钥泄露到任何产物** → 停止消费映射、清理产物、复盘并另立 ADR。
4. **提出限制出站（推翻 R6）** → 撤回 `provider_test`/`provider_models_fetch` 或加白名单，并同步修订 ADR-010 §5-7（ADR-014 §6-5）。
5. **新错误码/警告码需求** → 先按 ADR-006 附录 B 登记，不得自造。

## 7. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-10-09 | 文档维护（AI） | v0.1 创建 | 按 P1 计划 M7-06 DoD 与 ADR-014 §5 定界；登记先行，代码未改 |
| 2026-10-09 | 独立评审（M5-01 批次，`docs/M5-01-ADR批次评审意见.md`） | 通过（含修订闭环） | 批次复核 17/17 闭环；本 ADR 相关条目见该意见 §6。未另行修改决策 |

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-10-09 | 创建：三运行时消费映射、密钥注入白名单、五路径错误矩阵（+`provider_error`）、模型归一化、引用注入映射、投影实测义务 | 文档维护 |
| v0.2 | 2026-10-09 | 批次评审闭环（不改决策）：按 `docs/M5-01-ADR批次评审意见.md` 修订（含该意见 §6 闭环核验所列条目） | 文档维护 |

---

## 附录 A：映射 / 错误码 / 载荷登记

**A-1 env 注入白名单（按适配器 × provider 裁剪；一次性注入）**

| 适配器 | env 名 | 来源 |
|---|---|---|
| Claude Code | `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL` | provider `api_key_ref`（keyring 读取）/ `base_url` |
| Codex | `OPENAI_API_KEY`（或 provider 自定义 env 名） | provider `api_key_ref` |
| DSH | 按 profile 配置的 env 名（`DEEPSEEK_API_KEY` 等） | provider `api_key_ref` |
| 其他 | 不注入 | — |

> 注入仅经 spawn env；**任何配置文件、日志、诊断包、导出、测试夹具不得出现密钥值**（Codex `config.toml` 只写 `env_key` 名）。

**A-2 ADR-006 附录 B 错误码增行（逐行文本）**

| code | 语义 | 触发点 | 前端行为 |
|---|---|---|---|
| `provider_error` | 供应商返回非授权类错误（其余 4xx/5xx；message 含供应商状态码） | `provider_test` / `provider_models_fetch` | 提示「供应商返回错误（{status}）」；展开可看详情；可重试；不自动重试 |

**A-3 引用注入映射（`session.send.references`）**

| 形态 | 映射 |
|---|---|
| 载荷 | `references: [{ path, kind }]`（≤32；canonicalize + 可访问性；失败 `artifact_path_rejected`） |
| Claude Code / Codex / DSH | 消息前缀块：`<aether-references>` + 路径清单 + 「以上为随消息附带的文件引用，按需读取」+ `</aether-references>`；核心不读内容 |
| 权限 | 引用 ≠ 预授权：Agent 读取仍经 `fs.read` 权限门（D9） |
| 失败率保护 | 任一官方运行时失败率上升 → 撤回透传（协议字段保留，ADR-014 §6-8） |

**A-4 投影实测表（复用 ADR-014 附录 C-3；M7-06 DoD1 逐条断言）**

| 档位 | Codex | Claude Code | DSH |
|---|---|---|---|
| `read_only` | `--sandbox read-only` | `--permission-mode plan`（或 `--tools ""`） | file policy `read-only` + approval `ask`/`never` |
| `workspace_write` | `--sandbox workspace-write`（resume：`-c sandbox_mode=workspace-write`） | `--permission-mode acceptEdits` | file policy `workspace-write` + approval `ask` |
| `danger_full_access` | `--sandbox danger-full-access` | `--permission-mode bypassPermissions`（root 下 `auto`） | file policy `danger-full-access` + approval `never` |

> 仅新会话按新档位启动（在途会话保持原参数）；不等价 → 置灰/降级并登记（ADR-014 §6-4）。

**A-5 超时常量**：请求总超时 **10s**、连接超时 **5s**（R9；常量入 M7-06 证据，调整按常量级调参登记）；出站不做限制（R6，风险接受留痕随本 ADR 归档）。
