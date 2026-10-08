# M3 Gate 3 验收报告（M3 阶段出口）

| 项 | 内容 |
|---|---|
| 门禁 | Gate 3（《实施计划与验收标准》v1.24 §4「出口门禁 Gate 3」） |
| 验收对象 | M3 工作台与数据韧性：M3-01…M3-12（含 ADR-010 的 M3-09/M3-10/M3-11、ADR-011 的 M3-12）及承接项（M3-01 生产补读、M3-08 执行器权限网关接线） |
| 基线 | 需求 v0.12、设计 v1.15（冻结，ADR-001–015）、实施计划 v1.24、`docs/P1-实施计划与验收标准.md` v0.5 |
| 被测提交 | CI 归档以 `5c9644f`（M3-10/M3-11/M3-12 批次，= origin/main）；本机复跑工作树 HEAD `a8ded0c`（仅文档批次，未推送；不含代码差异） |
| 执行环境 | Windows 10 本机；Rust 1.98.1 / Node v24.14.1 / Bun 1.4.x / pnpm 10.34.5；真实 WebView2 |
| 执行日期 | 2026-10-08 |
| **结论** | **Gate 3 通过**（可进入 M4）。通过条件 4 条全部成立；T4/T9/T12/T13/T14 全部通过且可复跑；三项承接项（M3-01 生产补读 / M3-08 网关接线 / M3-12）均已关闭并有证据。执行中的 1 项待裁定口径（M3-05 容量巡检）已由用户裁定「接受按需投影为 P0 口径」并回流（§6）。无回退动作触发。 |

---

## 1. 执行记录

### 1.1 CI 归档（推送提交 `5c9644f`，run 76）

`ci` run [36693961491](https://github.com/Scott-PyMu/Aether/actions/runs/36693961491)（run_number 76，2026-09-30，结论 **success**，**22/22 job 全绿**）：

| 类别 | job（结论均为 success） |
|---|---|
| M3 专属 | Bindings contract + M3-01 (T14) / Session workbench (M3-02) / Permission center (M3-03) / Crash recovery (M3-06) / Audit minimal set (M3-07) / Workspace memory (M3-08) / File reference panel (M3-09) / Thinking depth (M3-10) / Model & provider config (M3-11) / Waiting state + grouping (M3-12) |
| 门禁 | Coverage gate (>=70% lines) / Supply chain gate |
| 平台与底座 | Linux cargo check + vite build / Windows build + smoke / macOS build + smoke / Adapters tests (ubuntu/macos/windows) / Tauri security baseline (M1-08) / Data dir guard (M1-06) / macOS data dir samples (M1-06) / Wire protocol + Mock adapter (M1-09) |

> 上下文：run 74（`ef87981`）failure（M3-09 job 的夹具路径 8.3 短名同源问题）已由 `ef87981`→`863bcfb`（run 75 success）修复闭环；后续无失败轮次。
> 注：**M3-04/M3-05 无专属 CI job**，其测试由 `Coverage gate`（`cargo llvm-cov` 全量测试）与本地 `pnpm ci:local` 批次覆盖；本轮以本机 `verify:m3-04/05` 独立复跑补足（§1.2）。

### 1.2 本机复跑（逐任务 verify 入口，原始日志留存 `scripts/test/.tmp/gate3/`）

| # | 命令 | 结果 |
|---|---|---|
| 1 | `node scripts/test/m3-01/verify-m3-01.mjs`（复跑） | **7/7 PASS**（T14：`AETHER_BINDINGS_CHECK PASS changed=false bytes=26237`；EventStore 去重/乱序/补读/压力/gap；事件桥慢消费不反压） |
| 2 | `node scripts/test/m3-02/verify-m3-02.mjs` | **5/5 PASS**（含**属主承接项**「缺口 >10k → 确认 → 清缓存重载最近 N 条」生产路径 E2E；`messages_page` 真实后端 + `aetherStore` 注入） |
| 3 | `node scripts/test/m3-03/verify-m3-03.mjs` | **5/5 PASS**（审批卡原文/规范化对照；allow/deny/超时/once/session/重启 pending 经 IPC 回环到适配器，`zero_passthrough=true`；7 份证据 JSON 归档） |
| 4 | `node scripts/test/m3-04/verify-m3-04.mjs` | **7/7 PASS**（**T9** 行数+哈希一致；恢复七步/WAL·SHM 改名/失败回滚；kill 中断可回退；外部候选矩阵；空间护栏 ×1.2；保留 10） |
| 5 | `node scripts/test/m3-05/verify-m3-05.mjs`（复跑） | **8/8 PASS**（诊断包脱敏 0 命中；容量三档；7 天提醒时钟注入；日志汇聚含 `attempt=n/3`/`persist_degraded`） |
| 6 | `node scripts/test/m3-06/verify-m3-06.mjs` | **10/10 PASS**（**T4**：kill -9 ×20 → `loss=[] false_completions=[] unreconciled=[]`；`run_retry` Mode R/N；真实 WebView2 降级恢复 E2E） |
| 7 | `node scripts/test/m3-07/verify-m3-07.mjs` | **8/8 PASS**（会话生命周期/权限决议/适配器状态三类 1:1 审计；`audit_log` 仅 INSERT 静态守门） |
| 8 | `node scripts/test/m3-08/verify-m3-08.mjs` | **12/12 PASS**（**属主承接项**：执行器权限网关接线，探针 `requests=3 decisions=3 resolves=3 zero_passthrough=true aggregate_zero_passthrough=true`；注入/工具/原子写/跨会话/冲突/`workspace_set` 换根） |
| 9 | `node scripts/test/m3-09/verify-m3-09.mjs` | **9/9 PASS**（迁移 0003 约束/级联；4 命令校验矩阵；面板 E2E；边界守门：不列目录/不读内容/不触发 `workspace_set`/不产生事件） |
| 10 | `node scripts/test/m3-10/verify-m3-10.mjs` | **18/18 PASS**（透传/同步与延迟能力门/重放；三适配器档位映射；校验矩阵 0–4 合法、5/-1 `out_of_range`、1.5/"高" `invalid_type`） |
| 11 | `node scripts/test/m3-11/verify-m3-11.mjs` | **8/8 PASS**（七命令矩阵；内置删除拒绝；密钥三态/命名空间；明文 0 命中；选择器 E2E） |
| 12 | `node scripts/test/m3-12/verify-m3-12.mjs` | **10/10 PASS**（等待态置位/回程守卫 + 四路径 + 重启 no-op + 多会话并发；生产组合路径集成 E2E `zero_passthrough=true`、`from/to` 精确） |
| 13 | `node scripts/test/m1-06/verify-m1-06.mjs` | **全部 PASS**（**T13**：拒绝启动 → 迁移 → 锁定新目录 E2E；**T12**：第二实例退出、首实例聚焦已有窗口、无写连接前置；本地样本 20/20 放行） |
| 14 | `pnpm --filter @aether/protocol check`（含于 #1） | **PASS**（**T14** 生成物 diff 一致；生成物未手改，工作树 `git status` 干净） |

### 1.3 环境性抖动记录（非功能缺陷，均有复跑证据）

| 项 | 现象 | 定性 |
|---|---|---|
| `workbenchScroll` 帧预算计时（M3-02 DoD4） | 首跑 verify-m3-01 时 15 次样本最小值 17.72ms > 16ms 预算 | 已登记口径（M3-02 证据边界 7 / M3-05 证据 §5 末行）：并行负载下计时抖动；隔离复跑最小 2.55ms 通过；其后 6+ 轮全量套件 148/148 通过；CI coverage job 以 `AETHER_COVERAGE_LINES` 插桩模式跳过计时断言且 run 76 全绿。真实 WebView 帧时间归 M4-04 基线 |
| verify-m3-05 首跑桌面套件 2 项超时 + `describeIpcError is not a function` 未处理拒绝（跨文件症状） | 同套件在前后 6 轮均 148/148 全绿；失败两用例（m3_03/m3_11 错误面）单独与全量复跑均通过；verify-m3-05 复跑 8/8 | 判定为负载下 vitest 模块初始化/时序瞬时异常（非产品缺陷）。登记为 P2 观察项（§7），M4-03「慢 UI 消费者/并发」演练覆盖同源风险 |

---

## 2. 分条判定（v1.24 §4 Gate 3 通过条件）

| 条件 | 判定 | 依据 |
|---|---|---|
| 1. M3 全部 DoD（含 ADR-010 的 M3-09/M3-10/M3-11；ADR-011 的 M3-12）通过 | **通过** | §1.2 #1–#12 逐任务 verify 全绿；各任务证据文档 `docs/M3-01-证据.md`…`M3-12-证据.md` 齐备（DoD 逐条可执行验证 + 证据 JSON 归档） |
| 2. T4 / T9 / T12 / T13 / T14 已通过 | **通过** | T4：`m3_06_t4` 2/2（20 次 kill -9 已确认零丢失、未确认不误显示完成）；T9：`m3_04_backup::t9_*` + IPC 恢复链一致；T12/T13：`m1-06` E2E（第二实例退出+聚焦 / 拒绝启动→迁移→锁定）；T14：`AETHER_BINDINGS_CHECK PASS` + CI `Bindings contract + M3-01 (T14)` success |
| 3. 若 Gate 2 带风险通过，M2-10 异常路径遗留项须先修复 | **不适用（N/A）** | Gate 2 正常通过（`docs/M2-Gate2-验收报告.md` §2 条件 3：deny/超时 deny/once/session/重启 pending 全过，无需冻结 M3-03）。M3-03 交付并在 IPC 面重放覆盖上述异常路径（§1.2 #3） |
| 4. M3-01 生产补读承接项必须关闭并有证据；M3-08 属主承接项与 M3-12 必须关闭并有证据 | **通过（三项均关闭）** | ① M3-01→M3-02：`messages_page` 真实后端 + `aetherStore` 注入 `EventBackfillSource` + 生产路径 E2E（`workbenchBackfillE2E` 2/2、`m3_02_session_backend` 6/6）；② M3-08：执行器权限网关由 `None` 切至核心 `PermissionService`，零直通集成断言通过；③ M3-12：等待态写入 + 分组 + 生产组合路径集成 E2E 10/10 |

**附加核验**：`git status` 工作树干净（仅两项与任务无关的既有未跟踪文件）；三平台 CI 22/22 全绿；覆盖率门禁（Rust/TS ≥70%）随 run 76 success。

---

## 3. 承接项关闭证据（Gate 3 硬条款）

| 承接项 | 关闭证据 | 证据位置 |
|---|---|---|
| M3-01 生产补读（`messages_page` 真实后端 + 前端注入 + 「>10k → 确认重载最近 N 条」生产 E2E） | `verify-m3-02` #2 PASS；`workbenchBackfillE2E.test.tsx` 2 用例（核心 `readback_gap_too_large` 同码透传 → 重载 500 条、不重启核心/应用）；契约按 ADR-009 登记 | `docs/M3-02-证据.md`「属主承接项」A/B/C；`scripts/test/.tmp/gate3/verify-m3-02.log` |
| M3-08 执行器权限网关接线（关闭 M3-02 边界 9） | `m3_08_memory` 零直通探针聚合 `zero_passthrough=true aggregate_zero_passthrough=true`；工作区内 ask → IPC 允许 → 适配器写入；`SessionMappingGate` 归属映射 | `docs/M3-08-证据.md` DoD7；`docs/M3-03-遗留项处置草案.md` §9 L1/L2 |
| M3-12 会话等待态写入与等待审批分组（ADR-011） | 置位/回程守卫（仅 running↔waiting_permission 生效、非适用态 no-op）、deny/超时/取消/run 失败四路径、重启 no-op、多会话并发、分组出现/消失 E2E、生产组合路径集成 E2E（`from/to` 精确 + 零直通） | `docs/M3-12-证据.md` DoD1–7；`scripts/test/.tmp/m3-12/`（本机）与 `scripts/test/.tmp/gate3/verify-m3-12.log` |

---

## 4. 回退动作对照

| Gate 3 回退触发 | 状态 |
|---|---|
| T9 失败 → 冻结发布路径 | 未触发（T9 一致 + 恢复七步全过） |
| T4 失败 → 最高优先级阻塞 | 未触发（20 次 kill -9 零丢失 + 重启收口幂等） |
| UI 性能不达标 → 按 D8 失效路径降级 | 未触发（`workbenchScroll` 为已登记环境性抖动，隔离复跑与多轮全量均通过；真实帧时间由 M4-04 基线覆盖） |
| 承接项未关闭 | 未触发（§3 三项全部关闭） |
| Gate 2 风险遗留（M2-10 异常路径）未修复 | 不适用（Gate 2 正常通过） |

---

## 5. 判定结论

- **Gate 3 通过**：M3（M3-01…M3-12）全部 DoD 通过；T4/T9/T12/T13/T14 通过；三项承接项关闭；无回退触发。
- M4 进入条件（Gate 3 通过）成立；M4-01/02/03 演练与 M4-04/05 验收可按计划启动。
- 本报告为 Gate 3 评审记录；容量巡检裁定（§6）随本报告生效并已回流任务证据。

---

## 6. 待裁定项裁定记录（M3-05 §6：容量巡检口径）

| 项 | 内容 |
|---|---|
| 事项 | D13 实现要点写明「每小时容量巡检」；M3-05 实际交付为**按需投影**（打开/刷新备份页、导出诊断前按 `db+wal` 即时统计），阈值语义（2GB 警告 / 5GB 强提示、`ok\|warn\|critical`）与横幅联动已通过 DoD2 |
| 裁定（2026-10-08，用户） | **接受按需投影为 P0 口径**——D13「每小时」解释为采集频率目标而非硬性 P0 要求；不新增机制、不升设计文档版本；该口径以本报告记录并回流 `docs/M3-05-证据.md` §6 |
| 实施影响 | 无代码变更；容量查询在页面打开/刷新与诊断导出前触发；若后续出现「长期不查询导致告警延迟」的实际风险，按 D13 失效条件（库 >5GB / 事件增长 >500MB·月）在 P1 保留策略/自动备份（M5 系列）一并评估 |

---

## 7. 风险与待办（均非阻塞）

| # | 级别 | 事项 | 说明 / 承接 |
|---|---|---|---|
| 1 | P2 | M3-04/M3-05 无专属 CI job | 其测试由 `Coverage gate`（全量 cargo 测试）与本地 `ci:local` 批次覆盖；本轮已本机独立复跑。建议后续补 job 或归档说明（不影响门禁） |
| 2 | P2 | 桌面套件瞬时抖动（m3-05 首跑，`describeIpcError` + 超时；见 §1.3） | 已多轮复跑全绿；M4-03「慢 UI 消费者/并发」演练覆盖同源风险；若再复现则排查 vitest mock/时序 |
| 3 | 记录 | 真实 WebView 内联权限回环 E2E 与窗口化冒烟（M3-03/04/05/08/12 分层口径） | 统一归 **M4-05**（v1.18 承接显式化；M4-04 仅索引） |
| 4 | 记录 | M3-04 安全模式（启动失败）备份入口 | 随 **M4-02**（T8 演练）复核；运行期 `persist_degraded` 已可用 |
| 5 | 记录 | M3-10 真实端点档位效果（Claude/Codex/DSH 映射） | 随 **M4-05** 实机冒烟；失败率上升按 ADR-010 §6-3 移除声明（能力门自动置灰） |
| 6 | 记录 | M3-11 `providers_list` ≤1MiB / 请求 ≤64KiB 护栏复核 | 随 **M4-04**（ADR-010 §5-10） |
| 7 | 记录 | 本地未推送提交 `a8ded0c`（ADR-013/014/015 文档批次） | 仅文档；CI 归档以已推送 `5c9644f` 为准；推送由项目方按流程执行（不影响门禁） |

---

## 8. 证据索引

| 证据 | 路径 | 状态 |
|---|---|---|
| CI run 76（22/22 success） | [actions/runs/36693961491](https://github.com/Scott-PyMu/Aether/actions/runs/36693961491) | 归档 |
| 本机逐任务 verify 日志（m3-01…m3-12 / m1-06） | `scripts/test/.tmp/gate3/verify-*.log` | gitignored，本机留存，可复现 |
| M3 任务 DoD 证据文档 | `docs/M3-01-证据.md` … `docs/M3-12-证据.md` | 已提交 |
| M3-03 遗留项处置与 ADR-011 链 | `docs/M3-03-遗留项处置草案.md`（含 §9 分层归档）+ `docs/adr/ADR-011-*.md` | 已提交 |
| ADR-009（messages_page 契约） | `docs/adr/ADR-009-*.md` | 已提交 |
| ADR-010（P0 UI 能力登记） | `docs/adr/ADR-010-*.md` | 已提交 |
| Gate 2 报告（M2-10 无遗留） | `docs/M2-Gate2-验收报告.md` | 已提交 |
| 本报告 | `docs/M3-Gate3-验收报告.md` | 本次新增 |
