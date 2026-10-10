# ADR-026：工作区管理与 Agent 预设（D14/D7；P1-A12）

| 项 | 内容 |
|---|---|
| 编号 | ADR-026（P1-A12） |
| 版本 | **v0.2（批次评审通过稿）**：v0.1 创建（批次稿）；v0.2 按 `docs/M5-01-ADR批次评审意见.md` 修订闭环（P0/P1/P2 相关条目全部闭环；不改决策） |
| 状态 | **已批准（2026-10-09；人工授权 AI 审批——独立评审通过即批准，见 `docs/M5-01-ADR批次评审意见.md` §7）**；已合入《设计文档》v1.16 |
| 决策日期 | 2026-10-09 |
| 决策载体 | 《设计文档》v1.15 → v1.16（D14/D3/D7 增量、附录 C 迁移规划位、附录 E 补行）；`docs/P1-实施计划与验收标准.md` v0.7（§9 行标注） |
| 关联 | 设计 D3/D7/D14、附录 A；需求 ADR-010 G.1（工作区树 P1 多视图、Agent 预设 P1+）；ADR-010（工作区绑定 `workspace_set`、`workspaces` 表）；ADR-014 附录 C-1（命令已登记）、附录 B 不变量 11（R5/R15）；ADR-018（错误码/响应增量汇总）；ADR-021/023（审计联动）；P1 计划 M7-08/M7-09（**直接实施项，不构成 P1 门禁**） |
| 取代 / 被取代 | 无（新增） |
| 回退条件 | 见 §6 |
| 实现状态 | 未开始（登记先行；由 M7-08/M7-09 执行；**非门禁**） |

## 1. 背景

- 工作区：现状单工作区绑定（`workspace_set` + `workspaces` 表 + S-05）；P1 要求「多工作区管理 + 左栏树」（ADR-010 G.1「P1 多视图」）。命令 `workspaces_list`/`workspace_create`/`workspace_update`/`workspace_delete`/`session_workspace_assign` 已在 ADR-014 附录 C-1 登记。
- 权限基准（R5/R15，ADR-014 附录 B 不变量 11）：**按会话创建固定**；`workspace_set` 换根仅对新会话生效；`session_workspace_assign` 不重算基准（防改派放大权限）。
- Agent 预设：原型设置页「Agent 预设」（`deepseek_html_V0.0.1.html:1310-1359`）；三条内置系统提示词 + 编辑；命令 `presets_list`/`preset_add`/`preset_update`/`preset_delete` 已在 ADR-014 附录 C-1 登记。
- ADR-014 §5-4 未决项：**工作区切换时在途 run 处置**与**预设提示词 ↔ 工作区记忆注入的合成顺序**由 P1-A12 定界；M7-08 DoD5 义务：`workspace_set` 与 `session_workspace_assign` 入口语义与去重由 P1-A12 定界登记；`workspaces.memory_files` 语义落定。
- 现状：记忆注入（D14，M3-08）写 `sessions.system_prompt`；`workspaces.memory_files` 恒写空数组。

## 2. 决策

### 2.1 工作区（M7-08）

- **决策**：
  1. **实体与命令语义**（命令已登记；本 ADR 补语义）：
     - `workspaces_list`：返回工作区（id/name/root_path/会话计数（active/total）/时间戳）；未分配会话（`workspace_id IS NULL`）以合成组「默认（数据目录）」呈现，**不建 `workspaces` 行**。
     - `workspace_create { name, root_path }`：`root_path` canonicalize + 存在目录 + 同步盘拒绝（A4 口径）+ 唯一性（同根路径已存在 → `invalid_value`）；名称 ≤64 字符。
     - `workspace_update { workspace_id, name }`：仅改名；`root_path` 不可变（防权限基准漂移）。
     - `workspace_delete { workspace_id }`：**存在会话引用（`sessions.workspace_id`）→ `workspace_in_use`（拒绝，不级联、不改派）**；无引用 → 删除行（单写队列）；**只删行，不删磁盘文件**（断言）；写审计。
     - `session_workspace_assign { session_id, workspace_id }`：改派会话；在途 run（`running`/`queued`）或 `waiting_permission` → **`session_busy`**（拒绝）；改派后**记忆注入源即时切换（对新 run 生效）**；**权限基准不重算**（R15，不变量 11）；UI 提示「权限基准不变，如需变更请新建会话」；写审计。
  2. **入口关系与去重（DoD5 义务）**：`workspace_set`（S-05 设置页）语义 = **设置默认工作区绑定（仅对新会话生效）**；`session_workspace_assign`（左栏树）语义 = **对既有会话改派**；左栏树不再提供第二个全局绑定入口（避免双入口冲突）。
  3. **在途 run 处置**：改派仅允许空闲会话（见 2.1-1）；不存在「运行中切换工作区」（如需，P2 另议）。
  4. **`workspaces.memory_files` 字段**：**P1 不启用**（维持恒空数组；D14 文件优先级 `AGENTS.md` > `AETHER.md` > `CLAUDE.md` 不变，记忆注入仍按工作区根解析）；本 ADR 登记该口径，字段保留待 P4 记忆检索时重议。
- **实现要点**：复用 `UpsertWorkspace`/`workspace_by_root`/`workspaces_latest`；新增写路径走单写队列；命令参数校验矩阵（M7-08 DoD1）；锚点 `workspace-tree`/`workspace-item`/`workspace-create`（ADR-014 已登记）。
- **主要风险**：改派导致记忆注入与权限基准「错位」（记忆已切换、权限基准未变）造成用户困惑；删除被绕过（直接改库）；双入口残留；在途竞态。
- **缓解措施**：UI 明确提示基准不变；删除前置检查在事务内复核；S-05 与左栏树入口职责在 UI 文案中区分；`workspace_set` 对既有会话不改行为（R15 回归项）。
- **失效条件**：改派造成**权限放大**（基准被重算或记忆/基准错配被利用）→ 立即停用改派入口并另立 ADR；`workspace_delete` 删除了磁盘文件 → 严重缺陷，立即修复并回归断言。

### 2.2 Agent 预设（M7-09）

- **决策**：
  1. **表 `presets`**（迁移 `0004+`）：`id/name/system_prompt/is_builtin/created_at/updated_at`；**播种 3 条内置**（默认助手 / 代码专家 / 代码审查；`is_builtin=1`；提示词文案以原型 `:1310-1359` 为准，M7-09 落库时定稿并记入证据）。
  2. **命令语义**（命令已登记）：`presets_list`（含内置标记）；`preset_add { name, system_prompt }`（名称 ≤64、提示词 ≤32KiB）；`preset_update { preset_id, name?, system_prompt? }`；`preset_delete { preset_id }`——**内置不可删**（`builtin_preset_undeletable`）；**内置可编辑**（名称/提示词可改，与 `providers` 内置语义对齐：仅删除受保护，`is_builtin=1` 保持并在 UI 标注「内置」；编辑内容差异记入审计以便追溯基线漂移）；全部写审计。
  3. **合成口径（禁止静默覆盖）**：`session_create` 可选 `preset_id`；会话的 `sessions.system_prompt` = **合成结果**：
     - 顺序：① 预设提示词块（若选择；置前）；② 工作区记忆注入块（D14 既有，后置并带「工作区约定」标记）；
     - 两者边界以显式分隔标记分隔：预设块头 `<!-- aether:preset:<preset_id> -->`（恒带 preset_id，形态见附录 A-4）；记忆块沿用 D14/M3-08 既有标记；**任何一方不得覆盖另一方**；
     - 未选预设 → 仅记忆注入（与 P0 行为一致）；工作区无记忆文件 → 仅预设块。
  4. **生效来源回显**：`sessions.preset_id`（迁移 `0004+`；`REFERENCES presets(id) ON DELETE SET NULL`）；`SessionSummary` 增可选 `preset_id`/`preset_name`；会话摘要与创建响应可查「生效来源」。
  5. **下发**：合成后的系统提示词随 `session.create` 下发适配器（Mock + ≥1 真实运行时断言，M7-09 DoD4）。
- **实现要点**：迁移文件 + 附录 C 终态 DDL + `verify:m1-03`/`m1_03_schema` 同批复跑（M7-09 DoD1）；预设列表页锚点 `presets-page`/`preset-item`/`preset-edit`（ADR-014 已登记）；新建会话入口可选预设。
- **主要风险**：合成顺序错误导致记忆注入被预设覆盖（或反向）；内置提示词被修改导致不可复现；`preset_delete` 后既有会话来源悬空（`ON DELETE SET NULL` 已处理，会话提示词保持原样）；提示词注入攻击面（预设为本地用户数据，信任级）。
- **缓解措施**：合成分隔标记 + 断言（DoD3）；内置不可删（可编辑，差异入审计便于追溯）；删除后回显「来源已删除」；长度上限。
- **失效条件**：出现静默覆盖（记忆或预设被无声丢弃）→ 停用预设选择入口并修复；M3-08 记忆注入 12 项回归失败 → 预设功能停用至修复。

## 3. 影响

### 3.1 《设计文档》v1.15 → v1.16

- D14：补「预设合成（P1，ADR-026）」段（顺序/分隔/禁止覆盖）；`memory_files` 字段口径补注「P1 不启用」。
- D7：`workspaces_*`/`session_workspace_assign`/`presets_*` 语义补全；`session_create` 可选 `preset_id`；`SessionSummary` 增 `preset_id`/`preset_name`（ADR-018 附录 A-5 汇总）。
- 附录 C：迁移规划位增 `presets` 表 + `sessions.preset_id`（M7-09）。
- 附录 E：补 `Preset.*` 行（不经线协议；session.create 合成下发）。

### 3.2 `docs/P1-实施计划与验收标准.md` v0.6 → v0.7

- §9 P1-A12 行标注正式编号 **ADR-026（2026-10-09）**；直接实施项性质不变（不构成 P1 门禁）。

### 3.3 不影响的

- 命令面计数不变（命令已登记于 ADR-014）；不新增事件类型（工作区/预设变更以命令响应与 UI 刷新呈现）；不改 D6 线协议 major；不改 D9 矩阵（预设/改派不改变权限模型）；不改迁移 0001–0003；M3-08 `workspace_set` E2E 按 R15 新口径复跑（设计变更驱动的断言更新，M7-08 DoD6）。

## 4. 版本（拟合入）

| 文档 | 修订前 | 修订后 |
|---|---|---|
| 《设计文档》 | v1.15 | **v1.16** |
| 《P1-实施计划与验收标准》 | v0.6 | **v0.7** |

## 5. 后续（未决项）

1. 在途 run 的工作区切换——P2+（若需要，另立 ADR）。
2. 预设分组/导入导出——P2+。
3. `memories` 表与记忆检索——P4（D14 失效条件路径）。

## 6. 回退条件

1. **本 ADR 被否决** → M7-08/M7-09 整体延后（直接实施项，不阻塞 P1 门禁，ADR-014 §6-7）；命令保持登记但无实现。
2. **改派权限事故** → 停用改派 + 另立 ADR；不得为通过测试放宽基准语义（AGENTS §6）。
3. **预设静默覆盖** → 停用预设入口并修复；M3-08 回归必须复跑。
4. **迁移 `0004+` 失败** → 单版本单事务回滚；禁止修改 `0001`–`0003`。
5. **新错误码需求** → 先按 ADR-006 附录 B 登记，不得自造。

## 7. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-10-09 | 文档维护（AI） | v0.1 创建 | 按 M7-08/M7-09 DoD 与 ADR-014 §5-4/R15；登记先行，代码未改 |
| 2026-10-09 | 独立评审（M5-01 批次，`docs/M5-01-ADR批次评审意见.md`） | 通过（含修订闭环） | 批次复核 17/17 闭环；本 ADR 相关条目见该意见 §6。未另行修改决策 |

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-10-09 | 创建：工作区命令语义/入口去重/删除安全/基准固定、预设表与播种/合成顺序与回显、错误码与迁移规划位 | 文档维护 |
| v0.2 | 2026-10-09 | 批次评审闭环（不改决策）：按 `docs/M5-01-ADR批次评审意见.md` 修订（含该意见 §6 闭环核验所列条目） | 文档维护 |

---

## 附录 A：迁移 / 错误码 / 审计登记

**A-1 迁移 `0004+` 规划位（随 M7-09 落库）**

```sql
-- 规划位（非终态；编号取落库时当前最大编号 +1）
-- CREATE TABLE presets (
--   id TEXT PRIMARY KEY, name TEXT NOT NULL, system_prompt TEXT NOT NULL,
--   is_builtin INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
-- );
-- INSERT INTO presets ... -- 3 条内置播种（M7-09 定稿文案）
-- ALTER TABLE sessions ADD COLUMN preset_id TEXT REFERENCES presets(id) ON DELETE SET NULL;
-- 终态 DDL 合入附录 C + verify:m1-03 / m1_03_schema 复跑（M7-09 DoD1）
```

**A-2 ADR-006 附录 B 错误码增行（逐行文本）**

| code | 语义 | 触发点 | 前端行为 |
|---|---|---|---|
| `workspace_not_found` | 工作区 id 不存在 | `workspace_update`/`workspace_delete`/`session_workspace_assign` | 刷新左栏树 + 提示 |
| `workspace_in_use` | 删除工作区时存在会话引用（不级联、不改派） | `workspace_delete` | 提示「仍有 N 个会话引用」；提供「查看会话」/「先改派」入口 |
| `preset_not_found` | 预设 id 不存在 | `preset_update`/`preset_delete`/`session_create`（preset_id 查无） | 刷新预设列表 + 提示 |
| `builtin_preset_undeletable` | 内置预设禁止删除 | `preset_delete`（`is_builtin=1`） | 提示「内置预设不可删除」（删除入口置灰） |

**A-3 审计动作**

| 动作 | actor | 说明 |
|---|---|---|
| `workspace.created` / `workspace.updated` / `workspace.deleted` | `user` | 工作区管理（detail 含 root_path/引用计数） |
| `session.workspace_assigned` | `user` | 会话改派（detail 含 from/to；显式记录「权限基准不重算」） |
| `preset.created` / `preset.updated` / `preset.deleted` | `user` | 预设管理 |

**A-4 合成标记契约（M3-08 记忆标记复用）**

- 预设块：`<!-- aether:preset:<preset_id> -->` … 预设提示词 …（块内不解析嵌套标记）；
- 记忆块：沿用 M3-08 既有「工作区约定」显式标记；
- 合成顺序固定为「预设块 → 记忆块」；两者同时存在时均完整保留（DoD3 断言）。
