# ADR-019：用户自定义权限规则引擎与 `always` 作用域持久化（D9 扩展；P1-A04）

| 项 | 内容 |
|---|---|
| 编号 | ADR-019（P1-A04） |
| 版本 | **v0.2（批次评审通过稿）**：v0.1 创建（批次稿）；v0.2 按 `docs/M5-01-ADR批次评审意见.md` 修订闭环（P0/P1/P2 相关条目全部闭环；不改决策） |
| 状态 | **已批准（2026-10-09；人工授权 AI 审批——独立评审通过即批准，见 `docs/M5-01-ADR批次评审意见.md` §7）**；已合入《设计文档》v1.16 |
| 决策日期 | 2026-10-09 |
| 决策载体 | 《设计文档》v1.15 → v1.16（D9 扩展、附录 A SE-02 行、附录 C 迁移规划位）；`docs/P1-实施计划与验收标准.md` v0.7（§9 行标注、§10 第 3 项闭合） |
| 关联 | 设计 D9（ADR-014 三档矩阵与 12 条不变量）、§2.3/§4#16、附录 A SE-02、附录 B/C；需求 SE-02；ADR-003/004（审计/存储）；ADR-014 附录 B（矩阵/不变量 1–12）、附录 C-1（`rules_*` 待登记）；ADR-018（命令与错误码登记）；P1 计划 M6-01/M6-02 |
| 取代 / 被取代 | 无（新增；不修改 ADR-014 附录 B 的档位矩阵与不变量，规则引擎在矩阵之上叠加） |
| 回退条件 | 见 §6 |
| 实现状态 | 未开始（登记先行；由 M6-01 执行，M6-02 抽查） |

## 1. 背景

- P0：D9 固定策略矩阵 + 阻塞式审批；无用户自定义规则、无 `always` 作用域（仅 `once`/`session`）。§4#16（P1）、附录 A SE-02「P1 规则引擎」。
- ADR-014 §5-2 定界：**权限模式与用户自定义规则的优先级由 P1-A04 正式 ADR 定界**；并明确「规则不得超越档位判定」。
- `permissions` 表 DDL 已含 `scope CHECK IN ('once','session','always')` 与 `resolver TEXT`（`user | rule:<id>`，P1）——`always` 与 `resolver` 均为 P1 启用，无需改表。
- 需定界问题（P1 计划 M6-01 DoD2/3/8）：规则模型、决策顺序、`always` 持久化与失效、规则不得超越档位、误配率方法（§10 待确认 3）。

## 2. 决策

### 2.1 核心决策（五段式）

- **决策**：P1 引入用户自定义权限规则（`permission_rules` 表，迁移 `0004+`）与 `always` 作用域持久化：
  1. **规则模型**：规则 = `(name, resource, action, target_kind, target, decision, priority, enabled)`；`resource` ∈ `fs.read/fs.write/exec/net`；`decision` ∈ `allow/deny`（**规则不产生 `ask`**——`ask` 仅由档位矩阵产生）；`target_kind` ∈ `path/command/host/any`。
  2. **决策顺序（不可逆序）**：
     - ① **档位矩阵为硬边界**（ADR-014 附录 B）：`read_only` 写、任意低档 `exec`/`net` → **直接 `deny`，规则不参与**（不可放宽）；
     - ② 矩阵结果 = `allow` 时：命中任一 `deny` 规则（enabled）→ `deny`（**收紧恒允许**）；否则保持 `allow`；
     - ③ 矩阵结果 = `ask` 时（仅 `workspace_write` 档区外 `fs.write`）：命中 `deny` → `deny`；否则命中 `allow` → `allow`（免逐次审批）；否则 `ask`（完整审批流 + 300s → deny + 审计）；
     - ④ `danger_full_access` 档：矩阵对区外读写与 `exec`/`net` 为 `allow`，规则可收紧为 `deny`（②），不可再放宽（已全放行）。
  3. **冲突规则**：同场景多规则命中 → **deny 优先于 allow**；同决定内 `priority` 高者先；`priority` 相同按创建时间旧者先（仅影响审计呈现的命中顺序，不改变结果）。
  4. **`always` 作用域**：绑定 **canonicalize 后的具体 target**（`permissions.scope='always'` 持久行），按 `(session_id, resource, action, canonical_target)` 精确匹配。**落库形态（不新增列）**：授权行在决议时以 `ticket.canonical_target`（决议评估得到的规范化形态）覆写 `permissions.target`——`scope IN ('session','always')` 的授权行 `target` 即为 canonical 形态；命中判定使用同一 canonical 值（`crates/aether-control/src/permission.rs:406-415` 既有 `GrantKey.canonical_target` 路径）。**重启重建**：核心启动时加载 `scope IN ('session','always') AND status='resolved' AND decision='allow'` 的行重建匹配集；历史行（旧口径存的原始 `target`）加载时尝试 canonicalize，**解析失败 → 该行不命中并告警**（fail-closed）。target 变更/路径逃逸样本（`..`、软链接、Junction、UNC/扩展前缀、8.3 短名、ADS、尾随点/空格、保留设备名）不命中（T7 全量回归）；**`read_only` 档与档位变更后按新档位重新求值（不变量 9），`always` 不越过硬边界**；工作区外目标仍按矩阵（`workspace_write` 档仍 `ask`/`deny`）。
  5. **审计**：规则命中路径写 `resolver=rule:<id>`；`always`/`session` 授权写 `resolver=user`；规则增删改/启停与档位变更入审计（动作字典见附录 A-3）。
- **实现要点**：
  1. 迁移 `0004+`：`permission_rules` 表（附录 C 规划位，终态 DDL 随 M6-01 落库；`verify:m1-03` 与 `m1_03_schema` 同批复跑绿）；
  2. 判定入参：`crates/aether-control/src/permission.rs` 评估路径新增「规则层」（矩阵 → 规则 → ask），不改矩阵本体；
  3. 命令面：`rules_list`/`rule_add`/`rule_update`/`rule_delete`/`rule_toggle`（ADR-018 附录 A-1）；错误码 `rule_not_found`；
  4. `session_workspace_assign`/`workspace_set` 引起的权限基准语义按 R5/R15（ADR-014 不变量 11）不受规则影响；
  5. UI：规则管理页（锚点 `rules-page`/`rule-item`，ADR-014 已登记）+ 审批弹窗「记住」入口写 `always`（复用既有 scope 枚举）；
  6. 合法规则校验失败**拒绝且不落库**；规则表读取为读连接（决策路径只读，不阻塞写队列）。
- **主要风险**：allow 规则被用于持久放宽区外写（审批疲劳的缓解面，但同时扩大自动放行面）；`command` 前缀匹配过宽（如 `rm` 命中 `rmdir`）；`host` 后缀匹配被伪造域名绕过；规则数据损坏导致决策异常；顶档 `deny` 规则产生的「收紧」与用户预期不符。
- **缓解措施**：硬边界不可越过；`command` 匹配按「命令名 + 词边界」（非裸前缀；可执行名精确匹配，参数不参与）；`host` 匹配按标签边界（`example.com` 不命中 `evil-example.com`）；规则加载失败 → 该规则置无效并告警（**fail-closed 至矩阵**，不无限放开）；审计全量留痕；误配率抽查（§2.2）。
- **失效条件**：出现规则导致的**越权放行**（硬边界被绕过/逃逸样本命中 allow）→ 立即停用规则层（回落固定矩阵 + 建议模式）并另立 ADR；**误配率 ≥5%**（M6-02）→ 自动应用降级为「建议模式」（规则仅在审批弹窗中作为建议展示，不自动应用），修复后重新抽查。

### 2.2 误配率抽查方法（闭合 P1 计划 §10 待确认 3；M6-02 预注册基线）

| 项 | 定值 |
|---|---|
| 样本来源 | 抽查执行时最近的用户规则全集（不足 100 条取全集；执行窗口内新增规则全部纳入） |
| 样本量 | 100 条（不足则全集；>100 按创建时间倒序取前 100） |
| 判定流程 | 双人独立判定（执行人 1 = 实现维护者；执行人 2 = 独立评审）；分歧样本由第三人复核 |
| 误配定义 | ① 规则命中结果与档位矩阵/本 ADR 决策顺序不符（实现缺陷）；② 规则对**硬边界**产生放宽（严重误配）；③ 规则语义与用户登记意图不符（表达误配） |
| 计算 | 误配率 = 误配条数 / 样本量；**<5% 通过**；严重误配单条即触发降级 |
| 记录 | 原始抽样清单、双人判定表与计算脚本随 M6-02 证据归档，可复跑 |

## 3. 影响

### 3.1 《设计文档》v1.15 → v1.16

- D9 实现要点：补「规则引擎（P1，ADR-019）」段（规则模型、决策顺序、`always` 持久化、误配降级）。
- 附录 A `SE-02` 行：「P1 规则引擎」补注「见 ADR-019（决策顺序/硬边界/误配降级）」。
- 附录 C：迁移规划位增 `permission_rules`（M6-01）。
- D7：`rules_*` 五命令登记（ADR-018 汇总计数）。

### 3.2 `docs/P1-实施计划与验收标准.md` v0.6 → v0.7

- §9 P1-A04 行标注正式编号 **ADR-019（2026-10-09）**；§10 第 3 项闭合（方法见 §2.2）。

### 3.3 不影响的

- 不改 ADR-014 附录 B 的矩阵与 12 条不变量；不改 D9 档位语义、投影与信任级边界声明；不新增事件类型；不改线协议 major；不引入新依赖；不改迁移 0001–0003。

## 4. 版本（拟合入）

| 文档 | 修订前 | 修订后 |
|---|---|---|
| 《设计文档》 | v1.15 | **v1.16** |
| 《P1-实施计划与验收标准》 | v0.6 | **v0.7** |

## 5. 后续（未决项）

1. 规则的导入/导出与共享——P2 评估（不阻塞）。
2. 规则命中统计/建议（基于历史审批生成建议规则）——P2+；P1 只做「建议模式」降级路径。
3. 规则对 `browser`/`mcp` 资源的扩展——功能未启用（矩阵 n/a），P3 随 MCP 另议。

## 6. 回退条件

1. **本 ADR 被否决** → 规则引擎不落地；`rules_*` 命令与 `rule_not_found` 撤回；D9 维持 ADR-014 矩阵 + `once`/`session`。
2. **误配率 ≥5% 或严重误配** → 触发 §2.1 失效条件（建议模式降级）；不得带过 M6-02/M8-03。
3. **规则越权放行** → 立即停用规则层并另立 ADR；涉及路径逃逸 → 同时触发 P3 前置（D9 原失效条件）。
4. **迁移 `0004+` 失败** → 单版本单事务回滚；禁止修改 `0001`–`0003`。
5. **新错误码/警告码需求** → 先按 ADR-006 附录 B 登记，不得自造。

## 7. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-10-09 | 文档维护（AI） | v0.1 创建 | 按 ADR-014 §5-2 与 M6-01/M6-02 DoD；登记先行，代码未改 |
| 2026-10-09 | 独立评审（M5-01 批次，`docs/M5-01-ADR批次评审意见.md`） | 通过（含修订闭环） | 批次复核 17/17 闭环；本 ADR 相关条目见该意见 §6。未另行修改决策 |

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-10-09 | 创建：规则模型、决策顺序（矩阵 → 规则 → ask）、冲突与匹配口径、`always` 持久化、误配率方法；迁移规划位 | 文档维护 |
| v0.2 | 2026-10-09 | 批次评审闭环（不改决策）：按 `docs/M5-01-ADR批次评审意见.md` 修订（含该意见 §6 闭环核验所列条目） | 文档维护 |

---

## 附录 A：模型 / 迁移 / 审计登记

**A-1 `permission_rules` 表（迁移 `0004+` 规划位；终态 DDL 随 M6-01 落库）**

| 列 | 类型/约束 | 说明 |
|---|---|---|
| `id` | TEXT PRIMARY KEY | ULID |
| `name` | TEXT NOT NULL | ≤64 字符（应用层校验） |
| `resource` | TEXT NOT NULL CHECK (`resource IN ('fs.read','fs.write','exec','net')`) | 资源白名单 |
| `action` | TEXT NOT NULL CHECK (`action IN ('read','write','exec','connect')`) | 与 resource 匹配（应用层断言） |
| `target_kind` | TEXT NOT NULL CHECK (`target_kind IN ('path','command','host','any')`) | `any` 仅对 `exec/net/fs.*` 全匹配 |
| `target` | TEXT | `any` 时为 NULL；`path` 为 canonicalize 形态 |
| `decision` | TEXT NOT NULL CHECK (`decision IN ('allow','deny')`) | 规则不产生 ask |
| `priority` | INTEGER NOT NULL DEFAULT 0 CHECK (`priority BETWEEN 0 AND 100`) | 高者先（审计顺序） |
| `enabled` | INTEGER NOT NULL DEFAULT 1 | 启停 |
| `created_at` / `updated_at` | INTEGER NOT NULL | epoch 毫秒 |

**`resource × target_kind` 兼容矩阵（规则校验硬口径；非法组合 → `invalid_enum`，field=`target_kind`）**

| resource | 允许的 target_kind | target 语义 |
|---|---|---|
| `fs.read` / `fs.write` | `path` / `any` | `path`：canonicalize 形态的前缀（目录，以分隔符收尾）或精确文件路径 |
| `exec` | `command` / `any` | `command`：可执行名精确匹配（大小写按平台规则），参数不参与 |
| `net` | `host` / `any` | `host`：精确主机名或 `.` 前缀后缀（`example.com` 不命中 `evil-example.com`）；不含 IP 段通配 |
| 任意 | （不适用于 `fs.*`+`command`/`host`、`exec`+`path`/`host`、`net`+`path`/`command` 等组合） | 组合不匹配即拒绝 |

```sql
-- 规划位（非终态；M5-01 不预合入附录 C 终态 DDL、不预分配迁移编号）
-- CREATE TABLE permission_rules (...); -- 见上表列定义；M6-01 落库时同批合入终态 DDL
-- CREATE INDEX idx_permission_rules_enabled ON permission_rules(enabled); -- 索引随 M6-01 评估（EXPECTED_EXPLICIT_INDEXES 同步）
```

**A-2 `always` 作用域持久化（不新增列；落库形态见 §2.1-4）**

- 复用 `permissions` 表：`scope IN ('session','always')` + `status='resolved'` + `decision='allow'` 的行即持久授权；**授权行 `target` 以 canonical 形态写入**（决议时由 `ticket.canonical_target` 覆写；不新增 `canonical_target` 列）；按 `(session_id, resource, action, canonical_target)` 精确匹配（target 变更不命中）。
- 核心重启时加载上述行重建匹配集；历史行（旧口径原始 `target`）加载时尝试 canonicalize，失败不命中并告警。
- M6-01 实现注意：P0 `resolve()` 现对 `PermissionScope::Always` 做过滤（`permission.rs:590`，`always` 当前不落地）——M6-01 移除该过滤并接通本 A-2 路径；`session` 授权维持内存集 + 持久行同源口径。
- 档位变更/降级后按 ADR-014 不变量 9 重新求值；`read_only` 下不因任何既有授权放行写请求。

**A-3 审计动作**

| 动作 | actor | 说明 |
|---|---|---|
| `rule.created` | `user` | 规则创建（detail 含规则摘要） |
| `rule.updated` | `user` | 规则变更（detail 含变更前后） |
| `rule.deleted` | `user` | 规则删除 |
| `rule.toggled` | `user` | 启停 |
| `permission.allowed_by_policy` | `system` | 规则/档位 `allow` 路径（ADR-014 不变量 6，既有登记延续） |

**A-4 误配率执行记录（M6-02 归档契约）**

- 产物：`样本清单.json` + `双人判定表.csv` + `误配率计算输出.txt`（可复跑脚本 `scripts/test/m6-02/`）。
