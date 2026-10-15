# ADR-023：完整审计范围与查询/导出格式（SE-03 P1；P1-A08）

| 项 | 内容 |
|---|---|
| 编号 | ADR-023（P1-A08） |
| 版本 | **v0.3（M6-03 实现批次修订）**：v0.1 创建（批次稿）；v0.2 按 `docs/M5-01-ADR批次评审意见.md` 修订闭环（P0/P1/P2 相关条目全部闭环；不改决策）；**v0.3 按 §6-3 失效路径修订索引/分页设计**——审计三索引末位追加 `id`（覆盖 `ts DESC, id DESC` 稳定排序）、查询不执行 `COUNT(*)` 改以「多取一行」判定 `has_more`；P95 断言不变（≤300ms；实测 P95 5.2ms，见 M6-03 证据） |
| 状态 | **已批准（2026-10-09；人工授权 AI 审批——独立评审通过即批准，见 `docs/M5-01-ADR批次评审意见.md` §7）**；已合入《设计文档》v1.16；**v0.3 为 §6-3 显式授权的实现批次修订（2026-10-14，M6-03 落库，随任务证据归档）** |
| 决策日期 | 2026-10-09（v0.3 修订：2026-10-14） |
| 决策载体 | 《设计文档》v1.15 → v1.16（D9 审计段扩展、§4#15 落实、附录 A SE-03 行、附录 C 迁移规划位）；`docs/P1-实施计划与验收标准.md` v0.7（§9 行标注）；**v0.3：设计文档附录 C `0005` 终态 DDL（三索引含 `id` 尾列）与 M6-03 任务证据** |
| 关联 | 设计 D9/D10、§2.1 第 5 条、§4#15、附录 A SE-03；需求 SE-03；ADR-003（P0 最小审计口径）、ADR-004/007/009（命令面先例）、ADR-018（`audit_query`/`audit_export` 登记）、ADR-021（events 保留不联动审计）、本批次各 ADR 的审计动作登记；P1 计划 M6-03 |
| 取代 / 被取代 | 无（新增；不修改 P0 最小审计语义）；v0.3 修订 §2.1-1/§2.1-3 的实现设计，不改记录边界、动作台账与不可改约束 |
| 回退条件 | 见 §6 |
| 实现状态 | **已实现（M6-03，2026-10-14）**：迁移 `0005`（三索引，含 `id` 尾列）、`audit_query`/`audit_export` 命令、`tool.call` 聚合、静态不可改检查；证据 `docs/M6-03-证据.md` |

## 1. 背景

- P0 最小审计：会话生命周期 + 权限请求/决议 + 适配器状态变化（ADR-003/004）；**不承诺记录所有 Agent 操作**；完整审计（查询/导出/哈希链）延后 P1/P3（§2.1 第 5 条、§4#15）。
- P1 §5 关联交付：「完整审计查询/导出」（哈希链 P3）；M6-03 DoD：记录完备性（经线协议上报范围 100% 落审计）、查询过滤/分页与 ≥1e6 行 P95 阈值、导出（外部路径 + 脱敏 + 完整性校验，与 M5-05 同口径）、`audit_log` 无 UPDATE/DELETE 静态检查、审计不被 M5-04 保留策略静默删除、IPC 命令登记与参数校验矩阵、仅本地 IPC。
- 现状：`audit_log` 表（`id/session_id/runtime_id/actor/action/resource/detail/result/ts`）+ `idx_audit_ts`；仅在核心侧写入；无查询/导出命令。

## 2. 决策

### 2.1 核心决策（五段式）

- **决策**：
  1. **记录边界（P1 完整口径；信任级边界不变）**：审计范围 = ① 既有最小集（会话生命周期/权限/运行器状态）；② **所有经线协议上报的工具调用**（`tool.call_*` 生命周期聚合为审计行：动作 `tool.call`，resource=工具名，result=成功/失败）；③ 本批次新增的 P1 管理面动作（规则/路由/备份/保留/归档/导出/会话管理/工作区/预设/暂停恢复/档位变更/供应商测试）。**不含适配器进程内行为**（设计 §2.1 第 4 条；任何表述不得写成「已隔离」）。哈希链**不在 P1**（P3，声明保留）。
  2. **动作字典**：P1 全量审计动作以**附录 A-1 为唯一台账**（既有点 + 本批次 ADR-017/019/020/021/022/024/025/026 登记项）；新增动作必须先在本台账增行（ADR 修订），不得自造。
  3. **查询**：`audit_query`（过滤：会话/时间/actor/action/resource；`limit` ≤500 缺省 100、`offset` 分页；按 `ts DESC, id DESC` 稳定排序）。**性能目标：1e6 行下已过滤分页查询 P95 ≤300ms**（任务内定值；基准环境 16GB 与库内数据构造记录于 M6-03 证据）。**v0.3（§6-3 修订）**：响应为 `{ rows, limit, offset, has_more }`——不执行 `COUNT(*)`（多谓词过滤下需逐行回表，1e6 行实测 0.36–3.4s，突破目标），以「多取一行」探测 `has_more`；页查询由索引逆向扫描覆盖（实测 P95 5.2ms）。
  4. **导出**：`audit_export` — 外部路径（选择器 + 可写 + 空间检查 M3-04 口径）+ 脱敏（`sk-`/`eyJ`/PEM 0 命中）+ JSONL + manifest（行数/sha256）+ 原子写（临时文件 + rename；中断不留半成品）；过滤条件与查询一致。
  5. **不可改**：应用层**无 UPDATE/DELETE `audit_log` 路径**（静态检查：源码扫描 SQL 文本 + 存储层 API 白名单）；`audit_log` 不参与 M5-04 保留清理（ADR-021 §2.1-7 断言）。
  6. **仅本地 IPC**：不提供网络/远程访问面（D11）；导出产物由用户自行处置。
- **实现要点**：
  1. 迁移 `0004+`：审计查询索引 `idx_audit_session_ts(session_id, ts, id)`、`idx_audit_actor_ts(actor, ts, id)`、`idx_audit_action_ts(action, ts, id)`（终态 DDL 随 M6-03 落库；`EXPECTED_EXPLICIT_INDEXES` 6 → 9 同批同步）；保留既有 `idx_audit_ts`。**v0.3（§6-3 修订）**：末位追加 `id`，使 `ts DESC, id DESC` 稳定排序在等值前缀下由索引逆向扫描全覆盖（v0.2 的 `(col, ts)` 形态不覆盖 `id` 尾键，低选择度过滤触发全量排序，实测 P95 3.2s）。
  2. 写入路径：复用既有单写队列（不新增写者）；`tool.call_*` 聚合为审计行的时机 = 终态事件落库后（网络忙时不得丢行；失败计入诊断）。
  3. 命令：`audit_query`/`audit_export`（ADR-018 附录 A-1 登记 + 参数校验矩阵）。
  4. UI：审计页（锚点 `audit-page`/`audit-filter`/`audit-export`，ADR-014 已登记）。
  5. 记录完备性核验（M6-03 DoD1）：脚本比对「线协议上报操作事件 ↔ 审计行」覆盖率 100%（范围声明 = 协议上报面）。
- **主要风险**：记录膨胀（1e6+ 行）与查询性能；审计写入与主写队列竞争；`tool.call_*` 面聚合时机遗漏；导出泄漏（detail 含路径/参数）；「完整审计」被误表述为覆盖适配器内部行为；索引更新遗漏导致 `verify:m1-03` 红。
- **缓解措施**：复合索引 + 分页上限；审计写入复用写队列批量提交（与事件同批，避免第二写者）；聚合失败重放计数入诊断；导出统一脱敏管道（与诊断包同一实现）；文案纪律（§2.1-1）；索引同步纳入 M6-03 DoD（复跑 `verify:m1-03`）。
- **失效条件**：覆盖率 <100%（协议上报面）→ 不得进入 M8-01；P95 超标一个迭代未修复 → 复审索引/分页设计并修订本 ADR；出现 UPDATE/DELETE `audit_log` 路径 → 严重缺陷，立即修复并回归静态检查。

### 2.2 明确不做（P1 边界）

- 哈希链 / 防篡改证据链（P3；本 ADR 只声明）；
- 审计的远程汇聚/上报（隐私优先，§4#26）;
- 审计行级保留策略（P1 全量保留；容量由容量巡检覆盖；策略另议）。

## 3. 影响

### 3.1 《设计文档》v1.15 → v1.16

- D9 审计段：「完整 Agent 操作审计（查询/导出/哈希链）延后 P1/P3」更新为「P1 实现查询/导出（ADR-023）；哈希链 P3」。
- §4#15 行：补注「P1 查询/导出（ADR-023）；哈希链 P3」。
- 附录 A `SE-03` 行：「P1 完整，P3 审计强化」补注 ADR-023。
- 附录 C：迁移规划位增审计三索引（M6-03）。
- D7：`audit_query`/`audit_export` 命令（ADR-018 汇总计数）。

### 3.2 `docs/P1-实施计划与验收标准.md` v0.6 → v0.7

- §9 P1-A08 行标注正式编号 **ADR-023（2026-10-09）**。

### 3.3 不影响的

- 不改 P0 最小审计语义（仅扩展）；不新增事件类型；不新增依赖；不改迁移 0001–0003；导出不触发网络。

## 4. 版本（拟合入）

| 文档 | 修订前 | 修订后 |
|---|---|---|
| 《设计文档》 | v1.15 | **v1.16** |
| 《P1-实施计划与验收标准》 | v0.6 | **v0.7** |

## 5. 后续（未决项）

1. 哈希链设计——P3（P1 只冻结「不实现」声明）。
2. 审计可视化分析（时间线聚合）——P2+ 评估。
3. 审计保留策略（若容量成为问题时）——另立 ADR。

## 6. 回退条件

1. **本 ADR 被否决** → M6-03 退回「最小审计 + 只读导出」形态；`audit_query`/`audit_export`、三索引与锚点承接撤回。
2. **覆盖率不达标** → 补齐写入点后再验；不得放宽「协议上报面 100%」口径。
3. **P95 不达标** → 修订索引/分页设计（本 ADR 修订），不得改断言。**已应用（v0.3，2026-10-14，M6-03）**：索引末位追加 `id` + 分页不执行 `COUNT(*)`（`has_more` 探测）；断言不变，实测 P95 5.2ms（1e6 行；证据 `docs/M6-03-证据.md`）。
4. **新错误码/警告码需求** → 先按 ADR-006 附录 B 登记，不得自造。

## 7. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-10-09 | 文档维护（AI） | v0.1 创建 | 按 M6-03 DoD；登记先行，代码未改 |
| 2026-10-09 | 独立评审（M5-01 批次，`docs/M5-01-ADR批次评审意见.md`） | 通过（含修订闭环） | 批次复核 17/17 闭环；本 ADR 相关条目见该意见 §6。未另行修改决策 |
| 2026-10-14 | M6-03 实现批次（AI，按 §6-3 失效路径自修订） | v0.3 修订生效 | 索引/分页设计修订（不改断言）；实测 P95 5.2ms；随任务证据归档，供后续批次复核 |

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-10-09 | 创建：记录边界、动作台账、查询/导出契约、索引规划、不可改静态检查、哈希链留 P3 | 文档维护 |
| v0.2 | 2026-10-09 | 批次评审闭环（不改决策）：按 `docs/M5-01-ADR批次评审意见.md` 修订（含该意见 §6 闭环核验所列条目） | 文档维护 |
| v0.3 | 2026-10-14 | **§6-3 P95 不达标路径修订（不改断言）**：① 审计三索引末位追加 `id`（`(col, ts, id)`；覆盖 `ts DESC, id DESC`）；② `audit_query` 响应 `{ rows, limit, offset, has_more }`、不执行 `COUNT(*)`；③ 附录 A-2 索引规划位更新为终态 DDL；依据 = M6-03 基准实测（v0.2 形态 P95 3.2s → v0.3 形态 P95 5.2ms，阈值 300ms 不变） | M6-03 实现批次（AI，§6-3 授权） |

---

## 附录 A：审计动作台账（P1 全量）

**A-1 动作字典（P0 既有 + P1 新增；此表为 M6-03 完备性核验基准）**

*P0 既有动作（代码在案，M6-03 不得误判为「自造」；实现以 `docs/M3-07-证据.md` 与代码为准）*

| 动作 | actor | 说明 |
|---|---|---|
| `session.created` / `session.closed` | `user` | 会话创建/关闭（M3-07） |
| `session.status_changed` | `system` | 会话状态转移（M3-07；P1 增暂停/恢复发射点） |
| `permission.requested` / `permission.resolved` | `system` / `user` | 权限请求/决议（M3-07） |
| `permission.allowed_by_policy` / `permission.denied_by_policy` | `system` | 档位/规则策略直判（零直通口径） |
| `permission.session_grant_hit` | `system` | `session`/`always` 授权命中 |
| `permission.timeout` / `permission.cancelled` | `system` | 审批超时 deny / 票据取消 |
| `runtime.status_changed` | `system` | 运行器状态转移（含 `user_disabled`） |
| `runtime.<audit_kind>` | `system` | 监督器审计记录六类：`admission_rejected` / `circuit_broken` / `terminated` / `ledger_reclaimed` / `ledger_stale_record` / `ledger_skipped` |
| `backup.restore` | `user` | 备份恢复（M3-04） |

*P1 新增动作（本批次各 ADR 登记项；`tool.call` 由本 ADR 登记）*

| 动作 | actor | 来源 | 说明 |
|---|---|---|---|
| `session.permission_mode_changed` | `user` | ADR-014 | 档位变更 |
| `runtime.enable` / `runtime.disable` | `user` | ADR-015 | 启停请求与结果 |
| `runtime.registry_rejected` | `system` | ADR-015 | 注册清单拒绝 |
| `session.pause` / `session.resume` | `user` | ADR-017 | 暂停/恢复 |
| `tool.call`（聚合） | `agent` | 本 ADR | 经线协议上报的工具调用终态（resource=工具名；result=success/failed；detail 脱敏） |
| `rule.created` / `rule.updated` / `rule.deleted` / `rule.toggled` | `user` | ADR-019 | 规则管理 |
| `backup.auto.run` | `system` | ADR-020 | 自动备份结果（success/failed） |
| `retention.cleaned` / `retention.auto_armed` | `system` | ADR-021 | 清理执行/阈值自动启停 |
| `archive.exported` / `session.exported` | `user` | ADR-021 | 归档/会话导出（detail：路径/行数/manifest 摘要） |
| `route.decided` | `system` | ADR-022 | 自动路由决策 |
| `provider.tested` | `user` | ADR-024 | 连通性测试（结果码；不含密钥） |
| `session.renamed` / `session.deleted` | `user` | ADR-025 | 会话管理 |
| `workspace.created` / `workspace.updated` / `workspace.deleted` / `session.workspace_assigned` | `user` | ADR-026 | 工作区管理 |
| `preset.created` / `preset.updated` / `preset.deleted` | `user` | ADR-026 | 预设管理 |

**A-2 索引规划位（迁移 `0004+`；随 M6-03 落库；v0.3 末位含 `id`）**

```sql
-- 终态（M6-03 落库，迁移 migrations/0005_audit_indexes.sql；编号 = 当前最大 +1）
CREATE INDEX idx_audit_session_ts ON audit_log(session_id, ts, id);
CREATE INDEX idx_audit_actor_ts   ON audit_log(actor, ts, id);
CREATE INDEX idx_audit_action_ts  ON audit_log(action, ts, id);
-- 既有 idx_audit_ts 保留；EXPECTED_EXPLICIT_INDEXES 6 → 9（M6-03 落库时）
```

**A-3 导出格式**：与 ADR-021 附录 A-2 同构，`kind` ∈ `manifest|audit`；`source="audit-export"`。
