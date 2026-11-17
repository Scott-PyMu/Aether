# ADR-040：启动检查按指纹变化分片选择性执行（修订 ADR-039 D-039-2/D-039-3/D-039-5）

| 项 | 内容 |
|---|---|
| 编号 | ADR-040（v1.0） |
| 版本 | **v1.0（2026-11-17，四角色签署；v0.4 + 四审 P2 文本收口，无新增决策）**；v0.4（按 `docs/ADR-040-三审意见.md`）；v0.3（按 `docs/ADR-040-复审意见.md`）；v0.2（按首轮 `docs/ADR-040-评审意见.md`）；v0.1（2026-11-16 草案） |
| 状态 | **已签署（架构/安全/QA/产品，2026-11-17）；实施解锁**；M10-14 实施/G13 证据另验 |
| 决策类型 | **启动完整性检查范围 + 运行期周期检查调度/生命周期 + `health`/验收口径修订（安全/数据完整性相关）**；不是单纯实现优化 |
| 修订关系 | **修订 ADR-039 v1.0：D-039-2 第 2 项（启动检查范围/严格匹配表）与 §4 场景表、D-039-3 §1（`storageCheckCompletedAt` 指标）与 §2（S-00 状态表）、D-039-5 §1（`health.storage_check` 语义）与附录 C（health 状态矩阵）、附录 B（`background_check_completed` 匹配口径）、§6.3 验收矩阵、M10-10 DoD-3/4 表述**；不取代 ADR-039 的其他决策（分库/写模型/备份恢复/维护态等） |
| 取代/被取代 | **本 ADR 不取代 ADR-039**；仅修订上述条款。ADR-040 **v1.0 取代 v0.4/v0.3/v0.2/v0.1**；已签署生效。ADR-039 按 **v1.1（仅变更记录/状态，正文不改，amended by ADR-040）** 处理（§5） |
| 关联 | ADR-039 v1.0/v1.1；《设计文档》v1.36；`docs/P2-架构迁移-实施计划与验收标准.md` v0.15 M10-14/M10-13/G13；`docs/UI-UX-设计规格.md` v0.21 S-00/S-08；`docs/M10-10-证据.md`、`docs/M10-10-metrics.json`；`docs/ADR-040-评审意见.md`、`docs/ADR-040-复审意见.md`、`docs/ADR-040-三审意见.md`、`docs/ADR-040-四审意见.md` |
| 实施任务 | **M10-14（M10-10 补充）**；G13 前实施。任务顺序：M10-11 → **M10-14** → M10-13（最终收口/证据汇总）→ G13；若 M10-13 已先执行，M10-14 后必须重跑/补充其证据（§6） |
| 证据 | `docs/M10-10-补充证据.md`、`docs/M10-10-补充-metrics.json`、`scripts/test/m10-10/*`（含 `adr039-baseline.json`/`.sha256`）、`scripts/test/m10-14/*`、`package.json` 的 `verify:m10-14` 注册、`pnpm verify:m10-10`（ADR-039 基线模式）、`pnpm verify:m10-14`（ADR-040 当前/Level 1/Level 2） |
| 回退 | 见 §7（含回退范围表） |
| 产品确认 | 2026-11-17：**Q1=B（G13 前实施并纳入 M10-10 补充证据）/ Q2=A（保留干净启动后台全量 `quick_check`）/ Q3=A（x/8 + 已跳过 N 个未变化分片）/ Q4=A（接受残余风险但需安全/QA 签核）/ Q5=确认 §4.3 阈值并补齐 no-go / Q6=冷缓存 dirty1 25–30s 严格 no-go（P95 ≤25s，不作为评估例外）** |
| 说明 | **ADR-040 v1.0 已签署；仓库在 M10-14 实施前仍按 ADR-039 v1.0 全量口径执行**；§4 是签署后的实施/验证计划，不是现有证据；M10-14/G13 证据另验 |

---

## 0. 评审响应与产品确认（v1.0）

### 0.1 首轮评审响应（v0.2 已闭环，保留追溯）

| 编号 | 级别 | 问题摘要 | v0.2 落点 |
|---|---|---|---|
| P0-1 | P0 | 验证链路与现有实现相反；§4 引用已回退原型数值 | §0.6/§1.2 明确当前仍为 ADR-039 全量回退；§4 改为“签署后验证计划”；15.78/15.93s 标注为 `8b76410` 已回退的 8 并发全量原型，**不是选择性检查证据** |
| P0-2 | P0 | 主库单独变脏被漏检 | D-040-1：**任一指纹变化 → 主库 + 变化分片**；仅主库变化 → 只检查主库；补“仅主库变脏”单测/故障注入 |
| P0-3 | P0 | `background_check_completed`/`clean_start_count` 语义未定义 | D-040-3：选择性启动检查**不得**置 `background_check_completed=true`；该标志仅由全量覆盖检查成功后置位；`clean_start_count` 仅严格干净启动 +1 |
| P0-4 | P0 | 产品“G13 前实施”与 ADR“不进入 G13”冲突 | 头部 + §6：G13 前实施；新增 **M10-14（M10-10 补充）**；更新 G13 条件/M10-10 证据分册/`verify:m10-10` 复跑 |
| P1-1 | P1 | sidecar 前置条件不完整 | D-040-1 §2 给出与 `clean_shutdown_is_strict` 等价的完整匹配表；任一 sidecar 前置不满足 → 主库 + 全部分片；`CURRENT`/journal 不满足 → 拒绝启动/先恢复 |
| P1-2 | P1 | 周期完整检查口径与 ADR-039 冲突 | D-040-3 §3：**每 20 次干净启动或 7 天（先到者）**；`last_full_check_at=None` 基线语义；失败重置 0 |
| P1-3 | P1 | `health`/UI/验收指标部分检查语义未定义 | D-040-4：新增 `planned_shards`/`skipped_shards`/`full_coverage`；`storageCheckCompletedAt` 拆分为 `startupCheckCompletedAt` 与 `fullCoverageCheckCompletedAt`；UI 采用 x/8 + 已跳过 N 个未变化分片；M9-01 重录 |
| P1-4 | P1 | 指纹“记账”与“启动比对”未区分；`last_seq` 可能破坏 1.5s 预算 | D-040-5：启动只比对 `header_change_counter/file_size/wal_size/mtime` + 期望 `schema_version`；**不打开 SQLite、不做 `MAX(seq)`**；`last_seq` 仅记账；新增 sidecar `format_version` |
| P1-5 | P1 | 后台全量检查未写入 ADR | D-040-3 §1：选择性/严格跳过通过后**仍执行后台全量 `quick_check`**；`full_check_due` 时以周期完整 `integrity_check` 替代；双层兜底 |
| P1-6 | P1 | 决策分类/ADR 关系/任务/版本更新缺失 | 头部“修订关系/实施任务”；§5 设计 v1.36、P2 v0.15、UI-UX v0.21、M9-01 重录；M10-14 任务与 DoD |
| P1-7 | P1 | 缺少失效条件/回退触发器 | §7：漏检故障注入失败、后台/周期无法保证、性能不达标、安全/QA 不接受等 → 立即回退 ADR-039 全量；编译期常量回退 |
| P1-8 | P1 | 优化收益未证明、最坏情况未定义 | §4.3：dirty 1/2/4/8 分片矩阵、冷/热缓存、release daemon、5.22 GiB；收益阈值与 no-go；“多数异常退出只影响 1–2 片”列为**待验证假设** |
| P1-9 | P1 | 残余风险签核未完成、缺少“干净指纹 + 页级损坏”故障注入 | §3.2/§3.3：暴露窗口、最大损坏范围、写入行为、故障注入、安全/QA 签核要求 |
| P2-1 | P2 | `mtime` 假阳性/旧 sidecar 兼容未说明 | D-040-5：`mtime` 仅辅助字段；sidecar `format_version`；旧 sidecar 缺字段/版本不符 → 全量；记录假阳性率 |
| P2-2 | P2 | “多数异常退出只影响 1–2 个分片”无数据 | §1.3 改为待验证假设；§4.3 崩溃注入统计；正确性不依赖该假设 |
| P2-3 | P2 | “主库很小”缺容量边界 | D-040-1 §4：不假设主库小；按普通 DB 纳入；记录大小/占比/耗时；超 5s/25s 重新评估 |
| P2-4 | P2 | 术语/引用不精确 | 全文统一术语；附录 C；补 D-039-5、UI-UX S-00、P2 M10-10、M10-10 证据引用 |

### 0.2 对 v0.2 复审意见的响应（v0.3 已闭环，保留追溯）

| 编号 | 级别 | 复审问题摘要 | v0.3 落点 |
|---|---|---|---|
| P0-1 | P0 | 周期完整检查只在启动时评估 `full_check_due`，没有运行期 7 天调度；§3.2“最长 7 天”不成立 | **选择方案①：增加运行期调度**。D-040-3 §5：启动后运行低优先级调度器，按 `last_full_check_at + 7 天` 到期触发，`PERIODIC_CHECK_POLL_MS` 兜底重算；到期且空闲时执行后台 `integrity_check`，维护/检查中延后，失败 `persist_degraded`，成功更新 `last_full_check_at`；§3.2/DoD-3/4 同步；注入时钟测试 |
| P1-1 | P1 | `verify:m10-10` 仍断言 ADR-039 全量口径；UI fixture 未列入修改范围；DoD-9 复跑可能失效 | §4.1 把 `verify-m10-10.mjs`、`m10_10_storage_check.test.tsx`、metrics 兼容别名列入修改范围；**`verify:m10-10` 保留 ADR-039 基线模式（固定基线文件/`11cb438` 快照），`verify:m10-14` 验证 ADR-040 当前实现**；DoD-9 两者都跑 |
| P1-2 | P1 | `checked_shards`/`state=ok` 既有语义变化未做兼容说明 | D-040-4 新增 **`contract_version`**（缺失/1=ADR-039，2=ADR-040）与兼容矩阵；旧 daemon/旧客户端/新 UI 行为明确；`full_coverage` 缺失视为 unknown，不得推断全量；M9-01 重录 + M10-03 外部客户端回归 |
| P1-3 | P1 | “写入不得静默成功”与“发现前仍可写”矛盾，故障注入判据不可执行 | §3.2/§3.3 #7–#9/DoD-5 改为可执行判据：①写损坏页 → 错误或后台检查失败；②写同分片其他页 → 记录成功/失败与新增行数，不承诺绝不成功；③后台失败 → `persist_degraded`、停止写入；④记录范围/可恢复性。per-shard 写门另立 ADR |
| P1-4 | P1 | §4.3 目标与 no-go 不一致；Q5 未落实 | §4.3 按 Q5：**dirty 1–2 <30%、dirty 4 <10%、dirty 8 劣化 >5% 均 no-go**；去掉 20–30% 空档；补 main-only/冷 dirty 2/4/8 no-go；§7/§8 同步 |
| P1-5 | P1 | `last_full_check_at=None` 基线只绑定首个严格干净启动，持续异常启动可能饥饿 | D-040-3 §3：`last_full_check_at=None` 视为 **baseline pending**；**任意启动路径**的启动检查完成后都调度基线 `integrity_check`；进程提前退出则下次启动重试；运行期调度器也重试；补连续异常启动测试 |
| P2-1 | P2 | ADR-039 修订范围不完整（D-039-2 §4、D-039-3 §2、D-039-5 附录 C、§6.3、M10-10 DoD-3/4） | 头部“修订关系”逐项补列；§5 设计 v1.36 修改清单逐项列出；ADR-039 变更记录标注 amended-by 章节；P2 v0.15 同步 M10-10 DoD-3/4 |
| P2-2 | P2 | `health` 新字段可选性/缺省行为未定义 | D-040-4：新字段为可选增量；旧 daemon 缺失时新 UI 按 contract 1/unknown 处理，不推断全量；补旧 daemon fixture |
| P2-3 | P2 | M10-14 与 P2 v0.14 的 M10-13/G13 顺序未登记 | §6 任务顺序表：M10-11 → M10-14 → M10-13 → G13；若 M10-13 已先执行则重跑/补充；P2 v0.15 更新任务计数/依赖图；G13 同时列 `verify:m10-13`/`verify:m10-14` |
| P2-4 | P2 | `schema_version` 检查对象不清 | D-040-1 §2 改为“sidecar 记录的 schema_version 与期望常量一致；实际 DB schema 由启动不变量/迁移 checksum/指纹变化覆盖”；若要求实际 header 校验，需打开 SQLite + 1.5s 测量 + 另立 ADR |
| P2-5 | P2 | §5 “ADR-039 签署后”表述有误 | §5 改为“ADR-040 签署后”更新 ADR-039 变更记录/状态；**v0.4 固定为 ADR-039 v1.1（仅变更记录/状态，amended by ADR-040）** |
| P2-6 | P2 | `mtime` 假阳性率/性能矩阵测量方法未定义 | DoD-6/§4.3：每格至少 3 次运行；冷/热定义；P50/P95/max/失败数；mtime 假阳性定义（仅 mtime 与 sidecar 不一致、header/size/WAL/schema 一致）、20 次干净启动样本、报告率；>5% 触发调查 |
| P2-7 | P2 | §7 回退只关闭选择性，未定义 health/UI/指标/sidecar/verifier 回退范围 | §7 增加**回退范围表**：Level 1（选择性关闭、新代码保留）与 Level 2（完整回退 ADR-039）逐项列明 health/UI/metrics/sidecar/verifier/G13 |
| P2-8 | P2 | `background_check_completed` 字段名与“全量覆盖完成”语义不一致 | D-040-3 §2 明确其为**历史字段名，语义=全量覆盖检查完成**（全量回退启动/后台全量/周期完整）；不重命名；代码注释/证据写全语义；未来别名另评估 |

### 0.3 对 v0.3 三审意见的响应（本轮新增）

| 编号 | 级别 | 三审问题摘要 | v0.4 落点 |
|---|---|---|---|
| P1-1 | P1 | §4.3 冷缓存 dirty1 25–30s 的目标表/no-go/注记三处不一致；Q6 已确认严格 no-go | §4.3 统一为 **P95 ≤25s（含冷缓存 dirty1）**；no-go #4 “dirty 1/2/4 P95 >25s（含冷缓存 dirty1）→ no-go”；删除“25–30s 评估例外”；§7 #5/§8/DoD-6 同步；Q6 写入头部/§0.5/§9 |
| P2-1 | P2 | 运行期调度器生命周期/时序加固未定义（panic/重启、超时取消/迟到结果、sleep/wake/时钟跳变/最大延迟） | D-040-3 §5：受监督任务 + 退避重启/降级；检查 epoch/取消令牌 + 迟到结果丢弃；`PERIODIC_CHECK_TIMEOUT_MS`；sleep/wake/clock jump 重算与最大延迟；DoD-3/5 + §3.3 #11/#14 补 panic/重启/迟到结果/时钟用例 |
| P2-2 | P2 | `verify:m10-10` 基线模式边界不清（快照来源/哈希/检查项；Level 1/2 回退验证） | §4.1/DoD-9：`adr039-baseline.json` 含 `source_commit`/文件哈希/断言，配套 `.sha256` 只读；`verify:m10-10` 校验哈希与断言而非文件存在；`verify:m10-14` 增加 `--rollback=level1` / `--rollback=level2` 行为测试 |
| P2-3 | P2 | 旧客户端破坏性变更发布门未前置 | §4.2 DoD-9/§6：M10-03 外部消费者必须升级或显式声明只支持 v2；发布说明标注 `contract_version`/语义变化；旧 v1/新 v2 回归；G13 条件 |
| P2-4 | P2 | ADR-039 变更方式与 P2 任务计数仍待签署时决定 | §5 固定 **ADR-039 v1.1（仅变更记录/状态，正文不改，amended by ADR-040）**；§6.1 提议 **14 项任务**（M10-00～M10-11 + M10-13 + M10-14；M10-12 保留），P2 v0.15 唯一任务表/依赖图 |
| P2-5 | P2 | shutdown/app_exit/import 维护窗口与运行期 `integrity_check` 交互未定义 | D-040-3 §5：取消令牌/epoch、`SHUTDOWN_CHECK_GRACE_MS`、迟到结果丢弃、未完成不写 clean sidecar（false/不写）；导入内部 `import_in_progress` 检测与延后；DoD-3/5 + §3.3 #14 补 app_exit/导入用例 |
| P2-6 | P2 | `verify:m10-14` 脚本注册/`adr039-baseline.json` 生成/哈希规则未列入 | §4.1 补 `package.json` 的 `verify:m10-14` 注册；`generate-adr039-baseline.mjs` 从 `11cb438` 生成 JSON+`.sha256`，只读、需评审更新；DoD-9 命令写全 |

### 0.4 对 v0.4 四审 P2 的响应（本轮新增，签署前文本收口）

| 编号 | 级别 | 四审 P2 摘要 | v1.0 落点 |
|---|---|---|---|
| P2-1 | P2 | §3.2 与 §5 的 7 天窗口措辞不一致（7 天 vs 7d+1h） | 统一为 **“7 天目标；正常 ≤7 天；sleep/wake/时钟跳变/轮询兜底最大 7d+1h”**；§3.2/§5/DoD-3/§8 同步；四角色签核接受最大 7d+1h（§9） |
| P2-2 | P2 | 调度器监督失败/超时后的 `health.storage_check` 状态/写回未明确 | D-040-3 §5：监督失败/超时 → `storage_check.state=failed`、`error=storage_check_scheduler_supervision_failed`/`storage_check_timeout`、`background_check_completed=false`、释放 in-progress、`storage_state=persist_degraded`；`degrade_trigger=integrity_failure` 复用并在 detail/诊断标注；UI S-08；DoD-3/5 补测试 |
| P2-3 | P2 | DoD 引用编号不存在（§4.2 DoD 只有 1–10；11/14 是 §3.3 编号） | 统一改为 `DoD-3/5 + §3.3 #11/#14`；§0.3/§4.1/§8 全文清理，不再使用不存在的 DoD 编号 |
| P2-4 | P2 | `PERIODIC_CHECK_TIMEOUT_MS=30min` 未按 5.22 GiB/HDD P99 验证 | DoD-6 实测 5.22 GiB SSD/HDD `integrity_check` P50/P95/P99/max；`PERIODIC_CHECK_TIMEOUT_MS` 按 P99+余量调整并记录；HDD 无法满足时明确支持边界/回退，不得静默放宽；§7/§8 同步 |
| P2-5 | P2 | `import_in_progress` 来源/复位未列入修改范围 | §4.1 增 `crates/aether-app/src/import_control.rs`：导入协调器持有并复位 `import_in_progress`（Staging/Validating/Applying/AwaitingRestart 置位；Completed/Cancelled/Failed/启动恢复复位）；`storage_check.rs` 只读；DoD-5 + §3.3 #14 补导入崩溃/回滚复位 |
| P2-6 | P2 | `verify:m10-14:level2` 基线构建/fixture 覆盖边界不清 | §4.1.2/DoD-9/§7.2 定义 Level 2 最小行为集：启动全量回退、严格跳过、旧 health 语义、sidecar 兼容、旧 metrics；基线构建不可用时至少运行固定 fixture/旧二进制并记录差异；不得只用静态 `verify:m10-10` 快照替代 |

### 0.5 产品确认（2026-11-17，Q1–Q6）

| 编号 | 产品裁定 | ADR-040 落点 |
|---|---|---|
| Q1 | **B. G13 前实施并纳入 M10-10 补充证据** | 头部、§4、§6；新增 M10-14；G13 条件更新 |
| Q2 | **A. 保留干净启动后台全量 `quick_check`** | D-040-3；残余风险窗口按“写门打开 → 后台全量完成”表述，运行期/周期检查为外层兜底 |
| Q3 | **A. UI 采用 x/8 + 已跳过 N 个未变化分片** | D-040-4；`planned_shards`/`skipped_shards`/`full_coverage` |
| Q4 | **A. 接受残余风险，但需安全/QA 签核并写入证据** | §3.2/§3.3/§9；未签核不得实施或通过 G13 |
| Q5 | **确认 §4.3 阈值，并补齐 no-go（dirty 1–2 <30%、dirty 4 <10%、dirty 8 劣化 >5%）** | §4.3/§7/§8；见 P1-4 |
| Q6 | **冷缓存 dirty1 25–30s 严格 no-go（P95 ≤25s，不作为评估例外）** | §4.3 目标/no-go/注记、§7 #5、§8、DoD-6；见 P1-1 |

### 0.6 当前实现状态（必须与验证证据一致）

- **ADR-040 v1.0 已于 2026-11-17 四角色签署，但 M10-14 尚未实施**；因此截至 2026-11-17，仓库仍按 ADR-039 v1.0 全量口径执行：`crates/aether-store/src/layout.rs` 的 `plan_startup_checks` 在任一指纹不匹配时返回 `full_startup_plan`（主库 + 全部分片）；`crates/aether-store/tests/m10_09_sharding.rs::mismatch_check_plan_covers_main_and_all_shards` 断言 `plan.shards == 0..8`、`full_fallback=true`。
- 选择性检查曾在提交 `8b76410` 实现，随后在 `11cb438` 回退为 ADR-039 全量口径；当前**没有**可复跑的选择性检查证据。
- `8b76410` 的 15.78/15.93s 是 **8 并发、mmap 512MiB 的全量检查原型**数据，不是“仅变化分片”的选择性结果；本 ADR 不将其作为选择性收益证据。
- 当前 M10-10 证据的 5.22 GiB 全量检查：`storageCheckMs=16207.84ms`、`checked_shards=8/8`、`main_checked=true`、`main_ms≈52.5ms`、`initializeMs=199.10ms`（4 并发 SSD）。
- **当前没有运行期 7 天周期检查调度**：`full_check_due` 只在 `boot_core_full_with_gate` 启动时计算，`spawn_periodic_full_check` 只在启动分支调用；持续运行超过 7 天不会触发 `integrity_check`（复审 P0-1 的直接依据）。
- **当前验证资产仍是 ADR-039 口径**：`scripts/test/m10-10/verify-m10-10.mjs` 断言 `!layout.includes("dirty_shards")` 与 `full_startup_plan(...)`；`apps/desktop/src/m10_10_storage_check.test.tsx` fixture 只有 `checked_shards`/`total_shards`。M10-14 必须更新。
- 因此，**§4 的测试/数值均为签署后 M10-14 的实施与验证计划**；在 M10-14 证据生成前，不得把本 ADR 当作“已实现/已验证”。

---

## 1. 背景与问题

### 1.1 ADR-039 的现行口径

ADR-039 v1.0 D-039-2 规定：

- 严格 sidecar 全匹配 → 跳过启动阻塞 `quick_check`，启动后空闲时后台 `quick_check`；
- **任一不匹配（含 WAL 非 0/缺失但 size 变化、manifest/分片缺失、文件头变化、版本升级）→ 主库 + 全部 N 个分片并行检查**；
- 每 20 次干净启动或 7 天（先到者）执行完整 `integrity_check`；
- 启动检查范围为**主库 + N 个分片**；主库仍含 `messages`/`runs`/`audit_log`，不能省略。

在 5.22 GiB / 8 分片数据集上，全量并行检查当前为 **16.21s**（4 并发 SSD，M10-10 证据），异常启动 `writesEnabledAt=20.54s`；冷缓存/系统 I/O 叠加时曾出现 34.6s，超过 30s 硬上限（ADR-039 §1.1）。

### 1.2 为什么提出选择性检查

**待验证假设**：异常退出通常只影响最近写入的少量分片（分布见 §1.3，由 M10-14 崩溃注入统计）。若能在**不牺牲数据完整性**的前提下，只检查指纹变化的分片，同时始终检查主库，可缩短异常启动的写门打开时间。

**但必须明确：**

1. 这不是无条件优化；它改变 ADR-039 的启动检查范围/写门/证据口径，属于安全/数据完整性相关决策；
2. 选择性检查**不能**替代后台全量检查/周期完整检查；
3. 当前实现仍是 ADR-039 全量回退，选择性检查尚未实现；
4. **正确性不依赖“多数异常退出只影响 1–2 片”这一经验判断**；该判断只影响性能收益（见 §1.3）。

### 1.3 数据、假设与边界

| 项 | 当前事实/口径 |
|---|---|
| 数据集 | 5.22 GiB / 20,000,000 行 / 100 会话 / 8 分片；主库含 `sessions`/`messages`/`runs`/`audit_log` |
| 全量检查 | 当前 4 并发 SSD `storageCheckMs=16207.84ms`；并发矩阵 2/4/8 = 26.01s/16.45s/8.46s；失败数 0 |
| 主库 | 当前 `main_ms≈52.5ms`（100 会话/20M 事件场景）；**本 ADR 不假设“主库永远很小”**；主库增长后按普通 DB 纳入预算，必要时重新评估 |
| 异常退出的 dirty 分布 | **未验证假设**：在少量并发写/少量会话写入的异常退出中，指纹变化分片通常为 1–2 个；8 片批量写时可能全部变化。该分布由 M10-14 崩溃注入统计，不作为正确性前提 |
| 15.78/15.93s | `8b76410` 的 8 并发全量原型，已被 `11cb438` 回退；**不是**选择性检查证据 |
| 平台 | 参考环境 Windows x64 / NVMe / NTFS；SATA SSD/HDD 只做冒烟；macOS/Linux 有环境则补，无法执行必须写明 |

### 1.4 周期完整检查调度现状（复审 P0-1）

- 当前 `full_check_due` 在 `boot_core_full_with_gate` 启动时计算一次；`spawn_periodic_full_check` 只在启动检查分支调用。
- **没有运行期定时器**：若应用持续运行超过 7 天且不重启，周期完整 `integrity_check` 不会触发；v0.2 §3.2 的“最长 7 天”承诺不成立。
- v0.3 选择**增加运行期调度**（D-040-3 §5），而不是把窗口改为“仅启动时评估”；后者需要重新取得产品/安全/QA 对更长窗口的接受，且与产品 Q4 的签核依据冲突。
- 同时修复 `last_full_check_at=None` 基线只绑定“首个严格干净启动”的饥饿问题（D-040-3 §3）。

---

## 2. 决策

**核心决策（一句话）**：在 ADR-039 严格 sidecar 前置条件全部满足时，启动阻塞检查只检查**主库 + 指纹变化分片**；任一指纹变化主库必查，8 片全变等同全量；选择性通过**不得**置 `background_check_completed=true`；严格跳过/选择性路径仍保留后台全量 `quick_check`，全量回退不重复；周期完整 `integrity_check` 为第二层，并增加**运行期 7 天调度器**与**任意启动路径的基线**；`health` 增加 `contract_version`/覆盖字段，验收指标按启动计划与全量覆盖拆分。

### D-040-1 选择性启动检查计划（范围与算法）

#### 1. 术语

- **指纹变化（fingerprint mismatch）**：启动快速比对字段中任一字段与 sidecar 不一致。
- **指纹变化分片**：指纹发生变化的分片。
- **选择性启动检查**：启动阻塞阶段只检查“主库 + 指纹变化分片”。
- **全量回退（full fallback）**：sidecar 前置条件不满足，或指纹变化覆盖全部分片；启动检查为主库 + 全部分片。
- **后台全量检查**：写门打开后，后台 `quick_check` 主库 + 全部分片（`mode=background`）。
- **周期完整检查**：每 20 次干净启动或 7 天触发的后台 `integrity_check` 主库 + 全部分片；7 天由**启动评估 + 运行期调度器**共同保证。

#### 2. sidecar 前置匹配表（任一不满足 → 全量回退）

`plan_startup_checks` 只在启动不变量（`CURRENT`/`shards.json`/主库 `events=0`/`shard_id`/`deletion_pending`/schema 等，ADR-039 D-039-6/D-039-8）通过后执行。下表与当前 `clean_shutdown_is_strict` 等价，并补齐漏项：

| 条件 | 要求 | 不满足时 |
|---|---|---|
| sidecar 存在/可解析 | JSON 可读、`format_version=1` | 全量回退 |
| generation | `sidecar.generation == CURRENT.generation` | 全量回退 |
| app_version | `sidecar.app_version == 当前 daemon 版本` | 全量回退 |
| 关闭阶段 | `close_sequence_completed=true` 且 `write_queue_drained=true` | 全量回退 |
| 后台检查 | `background_check_completed=true`（上一次全量覆盖检查已完成） | 全量回退 |
| 分片集合 | 恰好 8 个（`shard_id` 0–7），无重复、缺失、未知 `shard_id` | 全量回退 |
| 主库指纹 | `sidecar.main` 存在且字段完整 | 全量回退 |
| 每片指纹 | 每个分片都有对应指纹 | 全量回退 |
| WAL | 主库与每片 `wal_size=0` | 全量回退 |
| schema | **sidecar 记录的** `schema_version` 与当前期望常量一致（当前 14；不打开 SQLite） | 全量回退 |
| 快速指纹 | `header_change_counter`/`file_size`/`mtime` 与 sidecar 一致 | 全匹配 → 严格跳过；任一变化 → 选择性计划 |
| `CURRENT`/`shards.json`/journal | 按 ADR-039 D-039-6/D-039-8 校验 | 拒绝启动/先恢复 journal，不走检查计划 |

> **schema 检查对象（复审 P2-4）**：本行只比较 **sidecar 记录值**与期望常量，不打开 SQLite、不校验实际 DB 文件头。实际 DB schema 由启动不变量、迁移 checksum、`app_version` 变化和快速指纹变化覆盖；若要求实际 DB header schema 校验，必须打开 SQLite 并给出 1.5s 启动预算测量，另立 ADR。

> **兼容性**：开发阶段无线上版本；旧 sidecar 缺 `format_version`、缺 `mtime` 或无法解析 → 全量回退（fail-closed）。下一次干净关闭写入新格式 sidecar 后恢复优化。
>
> **对 ADR-039 附录 B 的修订**：附录 B 原文“`background_check_completed` 不参与跳过判定”与当前 `clean_shutdown_is_strict` 实现不一致；ADR-040 统一为：**它参与严格匹配**，为 `false` 时不得进入严格跳过/选择性路径。

#### 3. 检查计划算法

```text
sidecar = read_clean_shutdown(layout)
if sidecar 缺失/不可解析/前置表任一不满足:
    return full_plan(reason = "sidecar_not_strict:<field>")   # 主库 + 全部 8 片（0..=7）

main_changed = fingerprint_mismatch(sidecar.main, read_fast_fingerprint(main_db))
changed = [shard_id for shard_id in 0..8
           if fingerprint_mismatch(sidecar.shard(shard_id), read_fast_fingerprint(shard_path(shard_id)))]

if !main_changed && changed.is_empty():
    return skip_plan(strict_ok = true, main = false, shards = [], full_fallback = false)

return selective_plan(
    strict_ok = false,
    main = true,                 # 任一指纹变化，主库一律检查
    shards = changed,            # 仅指纹变化分片；可为空（仅主库变化）
    full_fallback = (changed.len() == 8),
    reason = if main_changed && changed.is_empty() {
        "main_fingerprint_mismatch"
    } else if main_changed {
        "main_and_shard_fingerprint_mismatch"
    } else {
        "shard_fingerprint_mismatch"
    }
)
```

**关键规则：**

1. **主库单独变脏**：`main_changed=true, changed=[]` → 只检查主库；这是首轮 P0-2 的修复点。
2. **任一分片变脏**：主库 + 变化分片（主库始终检查）。
3. **8 片全变**：主库 + 8 片，等价全量；`full_fallback=true`，但正确性与 ADR-039 全量一致。
4. **全部匹配**：跳过启动阻塞检查；启动后仍执行后台全量 `quick_check`（D-040-3）。
5. `full_fallback` 表示“本轮计划覆盖主库 + 全部分片”，不区分“前置条件失败”还是“8 片全变”；`reason` 记录具体原因。
6. 不打开 SQLite、不做 `MAX(seq)`；只做文件头/大小/WAL/mtime 的元数据读取。

#### 4. 主库容量边界

- 主库始终按普通数据库纳入计划；**不得**以“主库很小”为由省略。
- M10-14 证据必须记录 5.22 GiB 数据集中主库 `db+wal` 大小/占比、主 `quick_check` P50/P95/max。
- **容量评估触发**：主库 `quick_check` P95 >5s，或全量回退 `startupCheckCompletedAt` >25s 目标/接近 30s 硬上限 → 重新评估主库容量/检查策略，必要时另立 ADR；不得放宽 30s。
- 若主库单独检查导致全量回退超过 30s 硬上限，按 §7 回退/另立 ADR，不得放宽 30s。

### D-040-2 启动状态机与写门

| 场景 | 启动阻塞检查 | 写门 | 失败处理 |
|---|---|---|---|
| 全部指纹匹配（严格 sidecar） | 无 | 立即 `write_gate=open` | 后台全量检查失败 → `persist_degraded` |
| 选择性计划（主库 + 变化分片） | 主库 + 变化分片并行 | 检查期间 `write_gate=checking`；全部通过后 `open` | 任一项失败 → `persist_degraded` + S-08；不得打开写门 |
| 全量回退 | 主库 + 全部分片并行 | 检查期间 `write_gate=checking`；全部通过后 `open` | 同上 |
| 前置条件不满足 | 全量回退 | 同上 | 同上 |

- `storage_check.state=checking` 时 `storage_state` 必须保持 `normal`；检查中不是降级（ADR-039 D-039-3 不变）。
- 选择性检查通过只代表“启动计划完成”，不等于“全量覆盖完成”；`write_gate=open` 后仍需后台全量检查（D-040-3）。
- 检查失败才进入 `persist_degraded`，`degrade_trigger=integrity_failure`；不热恢复。

### D-040-3 后台全量、周期完整检查、运行期调度与 sidecar 标志

#### 1. 双层兜底（产品 Q2）

| 启动路径 | 启动阻塞检查 | 写门打开后 | 周期检查 |
|---|---|---|---|
| 严格跳过 | 无 | **后台全量 `quick_check`**（主库 + 全部分片，`mode=background`，`write_gate=open`） | `full_check_due` 时以**周期完整 `integrity_check`** 替代本次后台 `quick_check` |
| 选择性计划通过 | 主库 + 变化分片 | **后台全量 `quick_check`**（覆盖未检查分片） | 同上 |
| 全量回退通过 | 主库 + 全部分片 | 不重复后台 `quick_check`（启动已全量覆盖） | `full_check_due` 时执行周期完整 `integrity_check` |
| 任一检查失败 | 失败 | `persist_degraded`，不继续后台检查 | 不执行 |

- **后台全量 `quick_check` 保留**，是选择性检查的第一层兜底；周期完整 `integrity_check` 是第二层兜底。
- 后台检查在首屏/交互稳定后启动（当前实现约 10s 空闲窗口）；不阻塞 `uiReadyAt`/`mainInteractiveAt`/`writesEnabledAt`。
- 后台全量检查失败 → `persist_degraded`，不得继续写入。
- 周期完整检查与后台 `quick_check` 不并发：`full_check_due` 时只运行周期完整检查。

#### 2. `background_check_completed` 写回规则（首轮 P0-3 / 复审 P2-8）

- **选择性启动检查通过不得置 `background_check_completed=true`**。
- 该标志**仅**在以下全量覆盖检查成功后置 `true`：
  1. **全量回退启动检查**（主库 + 全部分片；它本身就是一次完整覆盖，等价于全量检查）；
  2. 后台全量 `quick_check`（主库 + 全部分片）成功；
  3. 周期完整 `integrity_check`（主库 + 全部分片）成功（含运行期调度器触发的周期检查）。
- 严格跳过路径本身不执行检查，不改变内存标志；如果本轮后台全量未完成，关闭时写 `false`。
- 启动时该标志必须为 `true` 才允许进入“严格跳过/选择性”路径；为 `false` → 全量回退。
- **内存标志启动初值固定为 `false`**：不从 sidecar 的 `true` 继承；sidecar 的 `true` 只用于本次启动的严格匹配判定。只有本轮上述全量覆盖检查成功后才在内存中置 `true`，关闭时写回 sidecar。这样“严格跳过/选择性通过后立即退出、后台未完成”不会把全量完成状态传给下一次启动。
- 关闭时 sidecar 写入当前标志值：
  - 选择性检查通过 + 后台全量成功 → `true`；
  - 选择性检查通过 + 后台全量未完成/失败 → `false`（或按失败路径不写 clean sidecar）；
  - 下一次启动看到 `false` → 全量回退，**不会**连续选择性跳过。
- **不新增 `startup_check_completed` sidecar 字段**：选择性启动检查的完成只体现在本次 `health.storage_check` 快照；下次启动重新按指纹判定，不需要跨会话记忆。`background_check_completed` 仍只表示“全量覆盖检查已完成”。
- **字段名说明（P2-8）**：`background_check_completed` 是**历史字段名**；ADR-040 语义 = **全量覆盖检查完成**（全量回退启动检查/后台全量/周期完整），不是只看后台。为兼容不重命名；代码注释、ADR、证据必须写全语义；若未来新增 `full_coverage_check_completed` 别名，按契约变更另评估，不改变 sidecar 兼容。

> 评审首轮 P0-3 的“仅由后台/周期置位”针对**选择性启动检查**；全量回退启动检查不属于选择性路径，且本身就是主库 + 全部分片的完整覆盖，若不置位会导致每次异常启动后永久全量回退、与 ADR-039 的干净启动优化冲突，因此列为第 1 种 setter。任何情况下都不得由“只检查了部分分片”的路径置位。
>
> **取舍**：选择性检查的收益成立前提是后台全量在本次会话关闭前完成。若用户在后台全量完成前频繁退出，下一次启动会全量回退；这是“安全优先”的有意取舍，不是逻辑缺陷。M10-14 证据需记录“选择性通过后及时关闭”的负例。

#### 3. `clean_start_count`、`full_check_due` 与基线（首轮 P1-2 / 复审 P1-5）

- `clean_start_count` **仅**在“严格干净启动且 sidecar 全匹配”时 `+1`；严格匹配包含 `background_check_completed=true`。
- 以下路径一律重置为 `0`：异常启动、sidecar 不匹配、选择性启动检查路径（因为不是严格全匹配）、全量回退、检查失败、`persist_degraded`、恢复/导入/迁移/版本升级/resharding。
- `full_check_due(now)` 定义为：
  ```text
  clean_start_count >= 20
  || last_full_check_at == None            # baseline pending
  || now - last_full_check_at >= 7 天
  ```
- **`last_full_check_at=None` 的基线不再只绑定“首个严格干净启动”**：
  - 视为 **baseline pending**；在**任意启动路径**（严格跳过/选择性/全量回退）的启动检查完成、写门 open 后，调度一次后台完整 `integrity_check` 作为基线；
  - 若同时命中 20 次/7 天规则，只运行一次，不重复；
  - 成功后写入 `last_full_check_at=now`、`clean_start_count=0`；
  - 若进程在基线完成前退出，`last_full_check_at` 保持 `None`，**下一次启动重试**；运行期调度器也会在空闲时重试（§5）。持续异常启动不再导致“永不触发”。
  - 若启动检查失败/`persist_degraded`，不调度基线；下次启动重试。
- 每 20 次干净启动或 7 天（先到者）触发的完整检查成功后：`clean_start_count=0`、`last_full_check_at=now`。
- 失败不更新 `last_full_check_at`，`clean_start_count` 保持 `0`，按失败路径处理。
- `full_check_due` 在启动时评估一次；若为 `true`，在启动检查成功后按上表调度周期完整检查。运行期调度器按 §5 继续评估 7 天/基线。

#### 4. sidecar 写入条件

- 仅在干净关闭序列完成、写队列 drain、全部库 `wal_checkpoint(TRUNCATE)` 成功后写 sidecar。
- `persist_degraded`/完整性检查失败/后台检查失败 → 不写 clean sidecar（或写 invalid/删除）；下次启动全量回退。
- 恢复/导入/迁移/版本升级/resharding 后 sidecar 必须失效；不得用旧 sidecar 进入选择性路径。

#### 5. 运行期 7 天调度器（复审 P0-1）

**目的**：保证应用持续运行超过 7 天时仍能触发周期完整 `integrity_check`，使 §3.2 的“最长 7 天”承诺成立。

**设计：**

- 启动完成后创建低优先级后台调度任务；维护 `next_due = last_full_check_at + 7 天`（`last_full_check_at=None` 时立即到期）；**到期点触发**；每 `PERIODIC_CHECK_POLL_MS`（默认 **1 小时**，内部常量，可调但须记录证据；不新增设置键）仅作兜底重算（时钟变化、维护结束、状态变化），正常空闲不额外等待轮询周期。
- **7 天目标**：正常空闲时 `last_full_check_at + 7 天` 到期点触发；维护/检查忙碌时延后（维护期间写入被拒绝，不扩大暴露窗口）；**sleep/wake/时钟跳变/轮询兜底最大 7d+1h**（四角色签核接受）；若安全/QA 要求更严格的定时精度，可缩短轮询/使用精确定时，M10-14 证据记录实际触发时间。
- 满足以下全部条件才执行：
  1. `full_check_due(now)==true`（20 次/7 天/`None` 基线）；
  2. `storage_state != persist_degraded`；
  3. 当前无 `storage_check.state=checking`（启动/后台/其他周期检查）；
  4. 无维护态（`health.maintenance` 为 `backup`/`restore`）或**导入维护窗口**（内部 `import_in_progress` 标志；`health.maintenance` 不覆盖导入，见下）；
  5. `full_check_in_progress` 未被占用（与启动周期检查共用一个原子/互斥标志）。
- 执行方式：复用 `spawn_periodic_full_check`（主库 + 全部分片 `integrity_check`，`mode=background`，`write_gate=open`）；运行期调度使用**较低并发**（默认 `min(startup_check_profile.concurrency, 2)`，可调但须记录证据），避免抢占交互/启动 I/O。
- 成功：`last_full_check_at=now`、`clean_start_count=0`、`background_check_completed=true`；释放 `full_check_in_progress`。
- 检查失败：`storage_check.state=failed`、`error`、`background_check_completed=false`、释放 `full_check_in_progress`；`persist_degraded`、写门停止、调度器停止（这是检查失败路径；调度器任务 panic/退出走下方监督/重启，不得混淆）。
- **调度器监督/重启（P2-1/P2-2）**：调度器由受监督的长驻任务承载；任务 panic/意外退出时记录 `tracing`/诊断，按 `SCHEDULER_RESTART_BACKOFF_MS = 1s/5s/30s` 退避重启；连续 3 次失败/5 分钟内无法恢复 → `storage_check.state=failed`、`error=storage_check_scheduler_supervision_failed`、`background_check_completed=false`、释放 `full_check_in_progress`、`storage_state=persist_degraded`、`degrade_trigger=integrity_failure`（复用；detail/诊断标注为调度器监督失败，不表示数据页损坏）、UI S-08，并停止；不得静默死亡。不新增事件类型/命令/设置键。
- **检查 epoch/取消令牌（P2-1）**：每次检查启动时捕获 `check_epoch`；超时、`app_exit`/shutdown、导入维护或降级时递增 epoch 并设置取消令牌。`spawn_blocking` 不能 abort，**迟到结果必须按 epoch 丢弃**：不得更新 `last_full_check_at`、`background_check_completed`、`health.storage_check` 或 sidecar；不得清除新检查的 `full_check_in_progress`。
- **检查超时（P2-2/P2-4）**：`PERIODIC_CHECK_TIMEOUT_MS` 默认 **30 分钟（暂定）**；DoD-6 必须实测 5.22 GiB/SSD/HDD `integrity_check` P50/P95/P99/max，按 **P99 + 余量**调整并记录证据；必须大于实测最大值。超时 → 递增 epoch、释放本轮 in-progress、`storage_check.state=failed`、`error=storage_check_timeout`、`background_check_completed=false`、`storage_state=persist_degraded`、`degrade_trigger=integrity_failure`（detail 标注超时）、迟到结果丢弃；HDD 无法满足时明确支持边界/回退 ADR-039，不得静默放宽。
- **sleep/wake/时钟跳变（P2-1）**：每次唤醒/轮询重算 `full_check_due(now)`；平台支持时用单调时钟计算间隔，`last_full_check_at` 仅持久化墙钟。检测到墙钟后退 >5 分钟或前进 >1 天时，保守地视为 due 一次（避免永不触发）；成功后重置时间戳，不重复触发。**最大延迟**：正常 ≤7 天；sleep/wake/时钟跳变后 ≤7d+1h（四角色签核接受）；维护/检查忙碌时延后，但维护期间写入被拒绝。
- **shutdown/app_exit（P2-5）**：收到 `app_exit`/daemon shutdown 后停止新检查、设置取消令牌；等待 `SHUTDOWN_CHECK_GRACE_MS = 5s`（内部常量，可调但须记录证据）。若检查在取消前已完成并成功提交，可按正常成功写回；若取消已发出或未在 grace 内完成，**不写 clean sidecar / `background_check_completed=false`**，迟到结果丢弃；下次启动按全量回退。不得无限等待检查导致退出卡顿。
- **导入维护窗口（P2-5）**：导入使用 ADR-034 维护窗口，`health.maintenance` 仅表示 `backup`/`restore`；调度器必须读取导入协调器的内部 `import_in_progress`（与导入 job 共享），在导入期间延后；导入提交/回滚后重新评估。不得对 staging generation 执行检查；只检查当前 `CURRENT` generation。
- **进程生命周期限制**：若每次启动都在基线/周期检查完成前被强制终止，则任何后台检查都无法完成；这不是调度饥饿（每次启动都会重新调度），证据必须记录该场景及运行窗口。若运行窗口足以完成检查，则至少一次成功。
- 无墙钟门禁：M10-14 通过注入时钟/缩短测试间隔验证 7 天触发；不得以真实等待 7 天作为验收条件（ADR-013）。
- **若运行期调度器未实现或无法保证，§3.2 不得承诺“最长 7 天”，按 §7 回退/另立 ADR。**

### D-040-4 `health`/UI/验收指标语义（首轮 P1-3 / 复审 P1-2、P2-2）

#### 1. `health.storage_check` 字段

在 ADR-039 D-039-5 §1 基础上**增加**字段（命令数 87 不变；仅响应字段增量）：

```json
{
  "storage_check": {
    "contract_version": 2,
    "state": "checking",
    "mode": "startup",
    "write_gate": "checking",
    "main_checked": false,
    "checked_shards": 1,
    "total_shards": 8,
    "planned_shards": 2,
    "skipped_shards": 6,
    "full_coverage": false,
    "started_at": 1760000000000,
    "completed_at": null
  }
}
```

| 字段 | 语义 |
|---|---|
| `contract_version` | **2 = ADR-040 语义**；缺失或 `1` = ADR-039 语义（旧 daemon）。新 daemon 在 `storage_check` 出现时总是输出 `2` |
| `state` | `checking` / `ok` / `failed`；**v2 中 `ok` 表示本轮计划完成，不必然等于全量覆盖** |
| `mode` | `startup` / `background` |
| `write_gate` | `checking` / `open`（启动准入；不是降级状态） |
| `main_checked` | 本轮主库 `quick_check` 是否完成 |
| `checked_shards` | **v2 中为本轮实际完成的分片检查数**（不是计划数，也不是总数）；v1 中为全部分片完成数 |
| `total_shards` | 固定 8 |
| `planned_shards` | 本轮计划检查的分片数（0..8）；v1 缺失 |
| `skipped_shards` | 本轮计划跳过（指纹未变化）的分片数 = `total_shards - planned_shards`；v1 缺失 |
| `full_coverage` | 本轮计划是否覆盖主库 + 全部分片；选择性/严格跳过为 `false`，全量回退/后台全量/周期完整为 `true`；v1 缺失 |
| `started_at`/`completed_at` | 本轮检查起止；`ok` 时设置 `completed_at` |
| `error` | 失败摘要 |

- `storage_state` 仍是降级唯一事实源；`storage_check.write_gate` 是启动准入唯一事实源（ADR-039 D-039-3 不变）。
- `state=ok` 且 `full_coverage=false` 是合法状态（选择性/严格跳过）；UI/客户端不得把它当作“全量完成”或“检查失败”。
- `state=ok` 且 `main_checked=false` 只表示**本轮未检查主库**（严格跳过），不得视为“主库校验失败/未完成”；写门已 open，S-00 应退出。主库失败只在 `state=failed` + `error` 中表达。
- 严格跳过：`main_checked=false`、`checked_shards=0`、`planned_shards=0`、`skipped_shards=8`、`full_coverage=false`、`state=ok`、`write_gate=open`；后台全量开始后更新为 `mode=background`、`planned_shards=8`、`skipped_shards=0`、`full_coverage=true`。

**状态矩阵（实现/UI/QA 共用；所有行 `contract_version=2`）：**

| 场景 | `state` | `mode` | `write_gate` | `main_checked` | `checked_shards` | `planned_shards` | `skipped_shards` | `full_coverage` |
|---|---|---|---|---|---|---|---|---|
| 严格跳过 | `ok` | `startup` | `open` | `false` | 0 | 0 | 8 | `false` |
| 选择性检查中 | `checking` | `startup` | `checking` | `false/true` | 0..planned | 1..7 | 8-planned | `false` |
| 仅主库检查中 | `checking` | `startup` | `checking` | `false/true` | 0 | 0 | 8 | `false` |
| 选择性通过（含仅主库） | `ok` | `startup` | `open` | `true` | planned | 0..7 | 8-planned | `false` |
| 全量回退检查中 | `checking` | `startup` | `checking` | `false/true` | 0..8 | 8 | 0 | `true` |
| 全量回退通过 | `ok` | `startup` | `open` | `true` | 8 | 8 | 0 | `true` |
| 后台全量检查中 | `checking` | `background` | `open` | `false/true` | 0..8 | 8 | 0 | `true` |
| 后台全量通过 | `ok` | `background` | `open` | `true` | 8 | 8 | 0 | `true` |
| 检查失败 | `failed` | `startup`/`background` | `checking`/`open` | 保持已完成项 | 已完成项 | 本轮计划 | 本轮跳过 | 本轮计划 |

> `checked_shards` 是**实际完成数**；`planned_shards` 是本轮计划数；`skipped_shards` 是本轮计划跳过数。三者不得互相替代。

#### 2. 兼容性（复审 P1-2 / P2-2）

| 组合 | `contract_version` | 语义/行为 |
|---|---|---|
| 新 daemon + 新 UI/客户端 | `2` | 读 `full_coverage`/`planned_shards`/`skipped_shards`；`state=ok` 只表示本轮计划完成；全量覆盖必须 `full_coverage=true` |
| 旧 daemon + 新 UI/客户端 | 缺失或 `1` | 按 ADR-039 语义：`checked_shards`=全部分片完成数、`state=ok`=全量检查通过；新字段显示“未知”，不得显示“已跳过 N”或声称全量覆盖 |
| 新 daemon + 旧客户端 | `2`（旧客户端忽略） | 旧客户端可能把 `2/8 + ok` 误读为“未完成/已完成全量”；**外部消费者必须升级或读取 `contract_version`/`full_coverage`**；M10-03 HTTP/WS/CLI 回归 |
| 新 daemon + 新 UI，但新字段局部缺失 | `2` 但字段缺失 | 按 unknown 处理：`full_coverage` 不得视为 true；`planned_shards`/`skipped_shards` 显示“未知”（不是 0）；只展示可确认的 `checked_shards`/`state` |

- **新字段为可选增量**：`planned_shards`/`skipped_shards`/`full_coverage`/`contract_version` 在旧 daemon 响应中缺失；新 UI 必须能处理缺失。
- **旧客户端限制**：服务端不能阻止旧客户端误读既有字段；`contract_version` 是检测手段。M10-03 外部消费者必须回归并在发布说明中标注。
- **M9-01 重录**；生成物由清单生成；`pnpm --filter @aether/protocol check` diff=0。
- 外部客户端回归：HTTP/WS/CLI 读取 `contract_version=2` 时使用新语义；缺失时按 v1；不得把 `checked_shards` 单独当全量覆盖。
- **不新增 `plan_complete` 兼容字段**：`full_coverage`/`planned_shards`/`skipped_shards` 已覆盖计划/覆盖语义；若外部消费者要求新的兼容别名，按契约变更另评估。

#### 3. UI 文案（产品 Q3）

- S-00 校验进度：**“正在校验数据（x/8；已跳过 N 个未变化分片；主库校验中/已完成）”**。
- `x` = `checked_shards`，`N` = `skipped_shards`；`N=0` 时可省略“已跳过”子句。
- 仅主库变化时显示：“正在校验数据（0/8；已跳过 8 个未变化分片；主库校验中/已完成）”；不得显示“8/8 完成”。
- 选择性检查完成（`state=ok, full_coverage=false`）后 S-00 退出（写门已开）；后台检查可选轻提示/诊断页，不阻塞主界面。
- 检查失败 → S-08；主库失败文案“主库校验失败”；校验中不得显示 S-08。
- **旧 daemon（无 `contract_version`/新字段）**：沿用 ADR-039 x/8 文案，不显示“已跳过/本次检查”字样；不得声称全量覆盖。
- **不新增锚点/testid**：复用 `storage-check-progress`；如 UI-UX 评审认为必须新增，按 UI-UX 流程登记，不得静默新增。

#### 4. 验收指标拆分（首轮 P1-3）

ADR-039 D-039-3 的 `storageCheckCompletedAt`（“主库 + 全部分片完整性检查完成”）在 ADR-040 下拆分为：

| 指标 | 定义 | 预算/用途 |
|---|---|---|
| `startupCheckCompletedAt` | 进程启动 → 启动阻塞检查计划完成、`write_gate=open`（严格跳过/选择性/全量回退均适用） | **全量回退 ≤25s 目标/≤30s 硬**；选择性检查记录实际值，必须满足 §4.3 收益阈值；用于 `writesEnabledAt`/G13 启动预算 |
| `fullCoverageCheckCompletedAt` | 进程启动 → 任一轮 `full_coverage=true` 的检查完成（全量回退启动检查、后台全量 `quick_check`、周期完整 `integrity_check`） | 信息性指标；不阻塞启动、不作为 25/30s 启动门禁；报告 P50/P95/max |
| `backgroundFullCheckDurationMs` | 后台全量 `quick_check` 从检查开始到完成 | 报告 P50/P95/max；参考全量检查 25/30s 目标；不阻塞启动、不作为启动硬门禁 |
| `periodicFullCheckDurationMs` | 周期完整 `integrity_check`（含运行期调度）从检查开始到完成 | 报告 P50/P95/max；不设启动预算 |

- 旧名 `storageCheckCompletedAt` 在 ADR-040 文档/新证据中**不再作为主指标**；M10-10 既有证据保留旧名并注明“ADR-039 口径”。
- **metrics 兼容别名（复审 P1-1）**：`docs/M10-10-metrics.json` 保留 `storageCheckCompletedAt` 作为历史别名（值映射到 `startupCheckCompletedAt`，并标注 `deprecated: true` 与 `metric_semantics: "adr039_ambiguous"`）；新证据同时输出拆分后的四个指标。别名只保证字段形状兼容，不保证与旧数值逐项等价。
- P2 计划 v0.15 / G13 报告必须使用拆分后的指标；不得把后台/周期检查耗时混入 25/30s 启动门禁。
- `writesEnabledAt`/`mainInteractiveAt` 在选择性路径 = `startupCheckCompletedAt`（选择性计划完成、写门 open），**不是** `fullCoverageCheckCompletedAt`；后台全量检查不关闭写门。干净启动仍按 ADR-039 ≤1.5s；异常全量回退 = 全量检查完成 ≤25s/≤30s。

### D-040-5 sidecar 格式与指纹字段（首轮 P1-4/P2-1）

#### 1. sidecar 增加 `format_version`

```json
{
  "format_version": 1,
  "generation": "gen-01J...",
  "app_version": "<version>",
  "closed_at": 1760000000000,
  "close_sequence_completed": true,
  "write_queue_drained": true,
  "background_check_completed": true,
  "last_full_check_at": 1759990000000,
  "clean_start_count": 12,
  "main": {
    "schema_version": 14,
    "header_change_counter": 123,
    "file_size": 123456,
    "wal_size": 0,
    "modified_at_ms": 1760000000000,
    "last_seq": null
  },
  "shards": [
    {
      "shard_id": 0,
      "schema_version": 14,
      "header_change_counter": 456,
      "file_size": 789012,
      "wal_size": 0,
      "modified_at_ms": 1760000000000,
      "last_seq": 12345
    }
  ]
}
```

- `format_version=1` 是 **sidecar 格式版本**，不是 `CURRENT.format_version`；缺失/不支持 → 全量回退。
- 旧 sidecar 缺字段/版本不符 → 全量回退；下一次干净关闭写入新格式。
- `background_check_completed` 为历史字段名，语义=全量覆盖检查完成（D-040-3 §2）。

#### 2. 启动快速比对字段（不打开 SQLite）

| 字段 | 启动是否比对 | 说明 |
|---|---|---|
| `header_change_counter` | 是 | SQLite 文件头偏移 24 的 4 字节 change counter；不是 `PRAGMA data_version` |
| `file_size` | 是 | 主库/分片文件大小 |
| `wal_size` | 是 | 必须为 0；`<db>-wal` 大小 |
| `mtime`（序列化字段 `modified_at_ms`） | 是（辅助） | 检测仅原位更新、size/header 未变的变化；**永不是唯一判据**；假阳性只导致多检查，不影响安全性 |
| `schema_version` | 是（sidecar 记录值 vs 期望常量） | 不打开 SQLite；实际 DB schema 由启动不变量/迁移 checksum/指纹变化覆盖（P2-4） |
| `last_seq` | **否** | 仅记账/诊断；启动不重算 `MAX(seq)`，避免破坏 `initialize ≤1.5s` |
| `format_version` | 是 | sidecar 格式版本 |

- 指纹读取不得打开 SQLite、不得执行 `MAX(seq)` 或其他查询；`quick_check` 只由检查计划执行，`FingerprintStats` 只做文件元数据读取。
- 任一快速指纹字段不一致 → 进入选择性/全量计划；**绝不因单一 `mtime` 匹配而跳过**。
- **`last_seq` 不参与启动比对**；若未来要比较 `last_seq`，必须先给出 5.22 GiB 启动测量并另立 ADR，不得在本 ADR 内静默加入。
- `mtime` 假阳性来源：杀毒、索引、备份、时钟调整、文件复制；M10-14 证据按 §4.2 DoD-6 的方法记录假阳性率；若假阳性导致优化失效，按 §4.3 no-go/§7 处理，不得静默删除 `mtime` 字段（改变判定语义需 ADR）。

---

## 3. 安全论证与残余风险

### 3.1 安全性质（fail-closed）

1. **干净关闭 + 本次变化必检**：指纹与 sidecar 匹配（在快速指纹可观测范围内）= 自上次干净关闭后文件未被改写；sidecar 只有在关闭序列完成、写队列 drain、`wal_checkpoint(TRUNCATE)` 成功后才写。若指纹发生变化，主库与全部分片中的变化项必须本次检查；不存在“已变化但被跳过”的分片。
2. **全量覆盖存在性**：`background_check_completed=true` 证明上一会话至少完成过一次全量覆盖检查（全量回退启动检查、后台 `quick_check` 或周期 `integrity_check`）。它不覆盖该检查之后的正常写入；这些写入由干净关闭 sidecar 证明“关闭后未被改写”，其页级完整性属于 §3.2 残余风险。
3. **主库不遗漏**：任一指纹变化 → 主库必查；仅主库变化 → 主库必查。
4. **前置不满足即全量**：sidecar 缺失/格式不符/字段缺失/WAL 非 0/schema 不符/分片集合不一致/后台未完成 → 主库 + 全部分片。
5. **选择性不写全量标志**：选择性启动检查不置 `background_check_completed=true`；若后台全量未完成，下一次启动全量回退，避免连续跳过。
6. **写门**：启动检查未完成前 `write_gate=checking`；选择性检查通过只打开本次启动的写门，不声称全量覆盖；后台全量失败 → `persist_degraded`。
7. **运行期兜底**：应用持续运行时，运行期调度器按 7 天/`None` 基线触发完整 `integrity_check`（20 次计数由启动路径评估；运行期也会读取但计数不变）；维护/检查中延后，失败 `persist_degraded`；不存在“持续运行永不完整检查”。
8. **不改变权限/事件/备份契约**：不新增外部信任边界、命令、事件、设置键、迁移；只改变本地完整性检查范围、运行期调度与 `health` 字段。

### 3.2 残余风险（产品 Q4 接受，安全/QA 必须签核）

| 项 | 内容 |
|---|---|
| 风险 | 指纹全匹配但页级内容已损坏（位翻转、外部篡改、未改变 header/size/WAL/mtime 的损坏）不会被选择性启动检查发现 |
| 主要暴露窗口 | **启动写门打开 → 本轮后台全量 `quick_check` 完成前**；参考 4 并发 SSD 约 10s 空闲窗口 + 16s 检查 ≈26s；2 并发/HDD 可能更长（后台检查不受 30s 启动门禁约束）。若后台检查未完成/未运行，**运行期调度器**以 7 天为目标触发完整 `integrity_check`（正常 ≤7 天；sleep/wake/时钟跳变/轮询兜底最大 7d+1h，四角色签核接受）；后台检查失败则立即 `persist_degraded`，不等待 7 天；维护/检查忙碌时延后，但维护期间写入被拒绝 |
| 长期运行窗口 | **应用持续运行**：运行期调度器按 `last_full_check_at + 7 天` 到期点触发；正常 ≤7 天；sleep/wake/时钟跳变/轮询兜底最大 7d+1h（四角色签核接受）；维护/检查忙碌时延后，但维护期间写入被拒绝，不扩大暴露窗口。**应用未运行**：无写入，不存在新的损坏暴露；下次启动评估 `full_check_due` 并触发。`last_full_check_at=None` 时任意启动路径调度基线，不依赖严格干净启动 |
| 与 ADR-039 的差异 | ADR-039 干净启动同样跳过启动检查并依赖后台全量；ADR-040 对异常启动把“未变化分片的立即复检”推迟到后台/周期检查。7 天窗口由新增运行期调度器保证；新增风险是异常启动下未变化分片的即时复检缺失 |
| 最大损坏范围 | 任一分片库（该片 `events`）和/或主库（`messages`/`runs`/`audit_log`）；最坏可涉及多个分片；在损坏被发现前仍可写 |
| 写入行为（可执行判据，复审 P1-3） | **不承诺“任何写入都失败”**。承诺：①写入**损坏页**范围的页 → 必须返回 SQLite 错误或后续 `quick_check` 失败；②写入**同分片其他页** → 可能成功，记录成功/失败与新增行数，不声称“绝不静默成功”；③后台/周期检查失败 → `persist_degraded`、停止接受写入；④证据记录损坏页/其他页/写入范围/可恢复性。若产品要求“检测到损坏前禁止写该分片”，需 per-shard 写门，另立 ADR |
| 缓解 | 后台全量 `quick_check` 每次干净/选择性启动保留；运行期 + 启动周期完整 `integrity_check`（20 次/7 天/None 基线）；**调度器受监督/重启、检查超时/取消/迟到结果丢弃、shutdown/导入延后且未完成不写 clean sidecar；调度器失败/超时 → `storage_check.state=failed`/`error`/`background_check_completed=false`/`persist_degraded`/S-08**；失败立即 `persist_degraded`；备份/恢复可用；故障注入验证“损坏页失败、其他页记录、检查失败停止写入” |
| 接受条件 | 安全/数据 + QA 在 §9 签署；未签核不得实施，不得通过 G13 |

### 3.3 必须执行的故障注入（M10-14）

**启动/计划：**

1. **干净指纹 + 分片页级损坏**：翻转页字节但保持 size/header/mtime 与 sidecar 一致；选择性路径应跳过该片，后台全量 `quick_check` 必须发现并 `persist_degraded`。
2. **干净指纹 + 主库页级损坏**：同上；严格跳过路径由后台全量发现；全量回退路径由启动检查发现。
3. **WAL 非 0/缺失但 size 变化、sidecar 缺字段、`format_version` 不符、schema 变化、分片集合不一致、主库缺指纹** → 全量回退。
4. **仅主库变脏** → 计划为主库 + 0 分片；启动检查只查主库。
5. **dirty 1/2/4/8 分片** → 计划分别为主库 + 1/2/4/8 片；8 片时 `full_fallback=true`，性能等同全量。
6. **恢复/导入/迁移/版本升级后 sidecar 失效** → 不得进入选择性路径。

**写入/残余风险（按 §3.2 可执行判据）：**

7. **写损坏页**：向已损坏页范围写入 → 断言 SQLite 错误或后续 `quick_check` 失败；记录错误码/范围。
8. **写同分片其他页**：向同一分片未损坏页写入 → 记录成功/失败、新增行数、写入范围；**不得**把“写入成功”本身判为失败；后台检查失败后必须 `persist_degraded`、停止写入。
9. **后台/周期失败停止写入**：损坏被后台/周期检查发现后，断言 `persist_degraded`、写门/新 run 拒绝、无进一步写入；记录可恢复性（备份/导出）。

**标志/调度/基线：**

10. **后台全量未完成关闭**：选择性通过后立即关闭（后台未完成）；下次启动必须全量回退（`background_check_completed=false`）。
11. **运行期 7 天触发**：注入时钟/缩短轮询间隔，模拟持续运行 ≥7 天；断言恰好触发一次后台 `integrity_check`，成功后 `last_full_check_at` 更新、`clean_start_count=0`；维护/检查中延后，不并发。
12. **基线饥饿**：连续 N 次异常/选择性启动（无严格干净启动）；断言至少一次基线 `integrity_check` 被调度；若进程在完成前退出，每次启动重试；不得因“没有严格干净启动”永不触发。
13. **20 次 / 7 天 / None 组合**：20 次严格干净启动触发；7 天从 `last_full_check_at` 触发；None 基线在任意启动路径触发；不重复触发。
14. **调度器生命周期/迟到结果/shutdown/import**：注入调度器 panic/退出 → 监督重启/退避/连续失败降级；`integrity_check` 超时 + 迟到成功/失败 → epoch 丢弃、不更新 flag/timestamp/health/sidecar；sleep/wake/clock jump → 重算且最大延迟符合 §5；`app_exit`/shutdown → 取消/grace/sidecar false；导入 `import_in_progress` → 开始置位、提交/回滚/崩溃复位、切换后调度恢复。

> 若第 1/2/7/8 项发现“干净指纹但损坏漏检”或“写入扩大损坏/不可恢复”，按 §7 **立即回退 ADR-039 全量检查**，不得继续选择性方案。

---

## 4. 实施与验证计划（签署后，M10-14）

### 4.1 任务与修改范围

**任务：M10-14（M10-10 补充）**；依赖 M10-10；量级 M；G13 前完成；顺序见 §6。

| 文件/区域 | 修改 |
|---|---|
| `crates/aether-store/src/layout.rs` | `plan_startup_checks` 选择性计划；`format_version`；严格匹配表；主库单独变脏；`StartupCheckPlan` 语义（`full_fallback`/`reason`） |
| `crates/aether-store/src/write_queue.rs` | `clean_start_count` 仅严格干净 +1；选择性/异常清零；`background_check_completed` 写回规则；`full_check_due` 的 `None` 基线语义；`last_full_check_at` 更新 |
| `crates/aether-app/src/core_health.rs` | 选择性启动执行；主库 + 变化分片；后台全量保留/周期替代；**运行期 7 天调度器 + 受监督重启 + epoch/取消令牌 + shutdown/import 交互**；`health.storage_check` 新字段/`contract_version`；`complete_ok` 不得伪造 8/8；指标拆分 |
| `crates/aether-app/src/storage_check.rs` | planned/skipped/full_coverage/contract_version 诊断字段（非 IPC 契约）；`full_check_in_progress`、`check_epoch`/取消令牌协调；**只读**导入 `import_in_progress` |
| `crates/aether-app/src/import_control.rs` | `ImportControlBackend` 持有/复位 `import_in_progress`：job `Staging/Validating/Applying/AwaitingRestart` 置位；`Completed/Cancelled/Failed` 复位；启动 journal 恢复时无 active job 强制复位；暴露只读句柄给调度器 |
| `crates/aether-app/tests/m10_14_plan.rs` | 计划/前置表负例/主库单独变脏/dirty 1/2/4/8 |
| `crates/aether-app/tests/m10_14_startup.rs` | 选择性启动/health 字段/后台全量 |
| `crates/aether-app/tests/m10_14_periodic.rs` | 20 次/7 天/None 基线/运行期调度（注入时钟）/sleep-wake/时钟跳变/维护延后/不重复 |
| `crates/aether-app/tests/m10_14_lifecycle.rs` | 调度器 panic/退出→重启/降级；检查超时 + 迟到结果丢弃；`app_exit`/shutdown grace；导入 `import_in_progress` 延后；sidecar false/不写 |
| `crates/aether-app/tests/m10_14_flags.rs` | `background_check_completed`/`clean_start_count`/及时关闭负例 |
| `crates/aether-app/tests/m10_14_fault.rs` | 干净指纹+页级损坏/写损坏页/写其他页/停止写入 |
| `crates/aether-store/tests/m10_09_sharding.rs` | 替换 `mismatch_check_plan_covers_main_and_all_shards` 为选择性断言（**仅在 M10-14 实施后**） |
| `apps/desktop/src/health.ts`、`HealthMonitor.tsx`、i18n | 新字段类型；`contract_version` 分支；S-00 x/8 + 跳过 N；旧 daemon 兼容；不显示 8/8 假完成；无新增 testid |
| `apps/desktop/src/m10_10_storage_check.test.tsx` | **更新 fixture**：`planned_shards`/`skipped_shards`/`full_coverage`/`contract_version`；新增旧 daemon fixture（无新字段）；选择性/仅主库/全量/后台场景 |
| `apps/desktop/electron/app.ts` | `startupCheckCompletedAt`/`fullCoverageCheckCompletedAt`/`backgroundFullCheckDurationMs`；保留 `storageCheckCompletedAt` 历史别名 |
| `scripts/test/m10-10/generate-adr039-baseline.mjs` | 从固定提交 `11cb438` 生成 `adr039-baseline.json`：`source_commit`/各文件 sha256/期望断言；生成后写 `.sha256`；仅基线变更时运行并评审 |
| `scripts/test/m10-10/adr039-baseline.json` / `.sha256` | **只读基线**：JSON 内容 + 自身哈希；`verify:m10-10` 校验哈希/提交/断言，不得只检查文件存在 |
| `scripts/test/m10-10/verify-m10-10.mjs` | **ADR-039 基线模式**：校验 `adr039-baseline.json` 的哈希/`source_commit`/断言（`!dirty_shards`、`full_startup_plan`、`storageCheckCompletedAt`）；**不得**断言当前源码是 ADR-039 |
| `scripts/test/m10-14/verify-m10-14.mjs` | **ADR-040 当前实现 + Level 1/2 回退模式**：默认当前；`--rollback=level1`（选择性关闭）与 `--rollback=level2`（固定 ADR-039 基线构建/fixture）行为测试；选择性计划/health/contract_version/运行期调度/基线/标志/UI/metrics 别名 |
| `package.json`（根） | 注册 `verify:m10-14`（当前 + `--rollback=level1` / `--rollback=level2`）；保留 `verify:m10-10`（基线模式）；CI 脚本入口 |
| `docs/M10-10-metrics.json` | 新增拆分指标 + `storageCheckCompletedAt` 历史别名/`deprecated`/`metric_semantics` |
| `docs/M10-10-补充证据.md`、`docs/M10-10-补充-metrics.json` | 证据与指标；基线文件哈希/生成命令/Level 1-2 回退演练 |
| `docs/P2-架构迁移-实施计划与验收标准.md` v0.15 / UI-UX v0.21 / M9-01 | 签署后升版与重录；P2 v0.15 更新 M10-10 DoD-3/4、M10-13/M10-14 顺序/计数 |

**禁止修改**：0001–0014 已发布迁移、事件类型、命令面（87）、设置键（11）、业务码（35）、生成物手改、Tauri 回退包、AGENTS 冻结基线（除签署后按流程同步）。

#### 4.1.1 `adr039-baseline.json` 基线规则（P2-2/P2-6）

- **来源**：由 `generate-adr039-baseline.mjs` 从固定提交 `11cb438`（记录完整 commit hash）生成；不得从当前分支/工作区生成。
- **内容**：`source_commit`、`generated_at`、`schema_version`；至少包含以下文件的 `<path, sha256, required_snippets>`：
  - `crates/aether-store/src/layout.rs`（`full_startup_plan`/`fingerprint_mismatch` 断言）
  - `crates/aether-store/src/write_queue.rs`（ADR-039 严格 sidecar 字段）
  - `crates/aether-app/src/core_health.rs`（启动分支/`storageCheckCompletedAt`）
  - `scripts/test/m10-10/verify-m10-10.mjs`（ADR-039 断言）
  - `apps/desktop/electron/app.ts`（`storageCheckCompletedAt`）
  - `apps/desktop/src/m10_10_storage_check.test.tsx`（旧 fixture）
- **只读/哈希**：`adr039-baseline.json` 与 `adr039-baseline.sha256` 均入库且只读；`.sha256` 是 JSON 文件的 SHA-256。更新必须走评审并记录生成命令/提交；CI 不得自动改写。
- **`verify:m10-10` 检查项**：① JSON 自身哈希匹配 `.sha256`；② `source_commit` 为 `11cb438`（git 可用时校验 commit 存在；否则校验记录哈希）；③ 每个 required snippet/断言存在且匹配；④ 禁止断言当前源码是 ADR-039。**不得只检查文件存在。**
- **边界**：该基线只证明 ADR-039 历史快照完整/可审计，不验证当前实现；当前实现由 `verify:m10-14` 验证。

#### 4.1.2 `verify:m10-14` 模式与脚本注册（P2-2/P2-6）

| 模式 | 命令 | 验证内容 |
|---|---|---|
| 当前实现 | `pnpm verify:m10-14` | ADR-040 选择性计划/health `contract_version`/运行期调度/基线/标志/UI/metrics |
| Level 1 回退 | `pnpm verify:m10-14:level1` | 当前代码 `STARTUP_CHECK_SELECTIVE_ENABLED=false`（测试构建/编译常量）：任何不匹配 → 全量；严格跳过不变；health 新字段可保留；无“已跳过” |
| Level 2 回退 | `pnpm verify:m10-14:level2` | 固定 ADR-039 基线构建/fixture（`11cb438`）**最小行为集**：①不匹配 → 主库 + 全部分片全量回退；②干净 sidecar → 严格跳过；③旧 health 语义（`checked_shards`=全部分片、`state=ok`=全量通过）；④sidecar 兼容（缺 `format_version` → 全量）；⑤旧 metrics 别名；同时 `verify:m10-10` 基线通过 |

- `package.json` 注册 `verify:m10-14`、`verify:m10-14:level1`、`verify:m10-14:level2`；不得要求用户手工拼接脚本路径。
- Level 1 使用测试专用编译开关/构建（不是设置键、不是运行时用户开关）；生产构建默认 `STARTUP_CHECK_SELECTIVE_ENABLED=true`（v1.0 已签署，M10-14 实施时启用）。
- Level 2 若基线构建不可用，必须在 M10-14 证据中记录回退演练（构建/运行命令、结果）并运行固定 fixture/旧二进制；**Level 2 是行为测试，不得只用 `verify:m10-10` 静态快照替代**；记录与基线构建的差异。
- DoD-9 复跑命令必须写全默认/Level 1/Level 2（Level 2 基线构建不可用时：回退演练证据 + 固定 fixture/旧二进制 + `verify:m10-10`）+ `verify:m10-10`。

### 4.2 DoD（必须可执行验证）

1. **计划单测**：干净 sidecar 严格跳过；仅主库变脏 → 主库 + 0 片；1/2/4/8 片变脏 → 主库 + 对应片；8 片 → `full_fallback=true`；sidecar 缺字段/`format_version` 不符/`background_check_completed=false`/WAL≠0/schema 变化/分片集合不一致/主库缺指纹 → 全量。
2. **启动集成**：选择性通过 → `write_gate=open`；`health` 返回 `contract_version=2`、`planned_shards`/`skipped_shards`/`checked_shards`/`full_coverage=false`；主库 + 变化分片实际被检查；未变化分片不在计划。
3. **后台全量 + 周期/运行期**：严格跳过/选择性通过后仍执行后台全量 `quick_check`（主库 + 8 片，`mode=background`，`write_gate=open`）；`full_check_due` 时以周期完整 `integrity_check` 替代；**运行期调度器在持续运行达到 7 天目标（注入时钟）触发一次；正常 ≤7 天，sleep/wake/时钟跳变/轮询兜底最大 7d+1h**；维护/检查中延后；不并发；检查超时/异常/失败 → `storage_check.state=failed`/`error`/`background_check_completed=false`/`persist_degraded` 并释放 in-progress；**调度器任务 panic/退出 → 监督重启/退避，连续失败 → `state=failed`/`error=storage_check_scheduler_supervision_failed`/`background_check_completed=false`/降级/UI S-08；超时/取消后迟到结果按 epoch 丢弃；sleep/wake/时钟跳变按 §5 最大延迟处理；app_exit/导入窗口按 §5 延后/取消**。
4. **标志/计数/基线**：选择性通过不置 `background_check_completed=true`；全量回退启动检查/后台全量/周期完整成功才置 true；选择性通过后立即关闭 → 下次全量回退；`clean_start_count` 仅严格干净 +1，选择性/异常/失败清零；20 次/7 天/`None` 基线触发与成功后重置；**连续异常启动仍调度基线**（不依赖严格干净启动）。
5. **故障注入**：§3.3 全部 14 项；包含“干净指纹+页级损坏”选择性与后台两条路径；**写损坏页/写同分片其他页/后台失败停止写入**按 §3.2 判据；仅主库变脏；8 片全脏；运行期 7 天；基线饥饿；维护延后；**调度器 panic/重启/连续失败降级；检查超时 + 迟到成功/失败；app_exit/shutdown 中途取消 + sidecar false；导入 `import_in_progress` 延后 + 切换后恢复**。
6. **性能矩阵**：dirty 1/2/4/8 × 冷/热缓存 × 5.22 GiB × release daemon；**每格至少 3 次运行**；记录 `startupCheckCompletedAt`、`fullCoverageCheckCompletedAt`、`backgroundFullCheckDurationMs`、`periodicFullCheckDurationMs`、失败数；P50/P95/max。**mtime 假阳性**：定义=仅 `mtime` 与 sidecar 不一致、header/size/WAL/schema 均一致；样本=20 次连续干净启动（或 3 天）；报告次数/率；>5% 触发调查。冷=数据集复制后首次运行；热=紧接第二次；记录是否清 OS 缓存。**冷缓存 dirty 1 按 Q6：P95 ≤25s；25–30s 同样 no-go，不设评估例外；>30s 硬上限失败。** **`integrity_check` 超时**：记录 5.22 GiB/SSD/HDD `periodicFullCheckDurationMs` 的 P50/P95/P99/max；据此设定 `PERIODIC_CHECK_TIMEOUT_MS`（P99+余量）并写入证据；HDD 无法满足时明确支持边界/回退 ADR-039，不得静默放宽。
7. **主库容量**：记录主库 `db+wal` 大小/占比、主 `quick_check` P50/P95/max；不假设“主库很小”。
8. **UI/兼容**：S-00 x/8 + 跳过 N；仅主库 0/8 + 跳过 8；选择性 `ok` 不显示 8/8；校验中不显示 S-08；旧 daemon fixture 沿用旧文案/不声称全量；无新增 testid。
9. **契约/验证资产**：87/0/11/0014/35 不变；`health` 字段增量/`contract_version`；生成物重新生成 diff=0；M9-01 重录；**`pnpm verify:m10-10`（ADR-039 基线模式，校验 `adr039-baseline.json` 哈希/`source_commit`/断言）通过**；**`pnpm verify:m10-14`（当前）、`pnpm verify:m10-14:level1` 通过；`pnpm verify:m10-14:level2` 通过，或在基线构建不可用时以“回退演练证据 + 固定 fixture/旧二进制 + `verify:m10-10`”替代（须记录；Level 2 必须是行为测试，不得只用静态快照）**；`package.json` 已注册三个脚本；`pnpm verify:m9-01-baseline`、`pnpm verify:error-codes`、`pnpm verify:m1-03`、`pnpm --filter @aether/protocol check` 通过；**M10-03 发布门**：证据列出 HTTP/WS/CLI 消费者清单与升级计划；消费者必须升级到 v2 或显式声明只支持 v2；发布说明标注 `contract_version`/`checked_shards`/`state` 语义变化；旧 v1 客户端 + 新 daemon 与 v2 升级回归均记录。
10. **证据**：`docs/M10-10-补充证据.md` + metrics + 脚本 + 复跑命令 + `verify` 模式说明（`verify:m10-10` 基线 / `verify:m10-14` 默认/Level 1/Level 2）+ 基线文件哈希/生成命令 + 调度器生命周期/shutdown/import 注入记录；未验证项写明（平台/冷缓存/WAL 行为/运行期注入时钟等）。

### 4.3 性能收益阈值与 no-go（首轮 P1-8 / 复审 P1-4、Q5 / 三审 P1-1、Q6）

参考基线：同机、同 5.22 GiB 数据集、release daemon、4 并发 SSD 的**全量回退启动门**（当前 `writesEnabledAt`/`startupCheckCompletedAt` ≈20.54s；其中 `quick_check` 16.21s、启动开销约 4s）。

**目标（相对全量回退中位）：**

| dirty 分片 | 目标 | 硬约束 |
|---|---|---|
| 1 | 降低 ≥30%，且 P95 ≤25s | P95 >25s → no-go；>30s 硬上限 |
| 2 | 降低 ≥30%，且 P95 ≤25s | 同上 |
| 4 | 降低 ≥10%，且 P95 ≤25s | 同上 |
| 8 | 劣化 ≤5%，P95 ≤30s | P95 >30s → no-go；不得放宽 30s |

**no-go（产品 Q5/Q6，必须全部落实）：**

1. dirty 1–2 中位收益 **<30%** → no-go（不存在 20–30% 的“有条件 Go”空档）。
2. dirty 4 中位收益 **<10%** → no-go。
3. dirty 8 中位劣化 **>5%** → no-go。
4. dirty 1/2/4 P95 **>25s（含冷缓存 dirty 1）**，或 dirty 8 P95 **>30s** → no-go。
5. 冷缓存 dirty 2/4/8 低于对应收益阈值（dirty 2 ≥30%、dirty 4 ≥10%、dirty 8 劣化 ≤5%）→ no-go。
6. main-only `startupCheckCompletedAt` **高于全量回退** → no-go。
7. 后台全量/运行期 7 天调度/基线无法保证（含维护延后导致不可接受窗口）→ no-go。
8. 安全/QA 不接受 §3.2 残余风险 → no-go。
9. `verify:m10-10` 基线模式，或 `verify:m10-14`（默认/Level 1/Level 2）失败 → no-go。

- **25s 目标 / 30s 硬上限**：25s 是目标，30s 是硬上限；不得用 30s 替代 25s 目标。若产品/架构要调整 25s，另立 ADR；不得在 M10-14 中静默放宽。
- **冷缓存 dirty 1**：按产品 Q6，P95 **≤25s**；25–30s **同样 no-go**，不再作为“评估例外”；>30s 为硬上限失败。不得用 warm 数据替代 cold 证据。
- **Go/No-Go**：ADR-040 v1.0 四角色签署 + 安全/数据与 QA 对 §3.2 残余风险签核 + 上述目标全部满足 → Go；任一 no-go 条件或 §7 回退条件命中 → No-Go/回退，不得以“部分收益”实施。
- 冷/热缓存：冷-ish = 数据集复制后首次检查；热 = 连续第二次；SATA SSD/HDD 至少冒烟；macOS/Linux 有环境则补。
- 崩溃 dirty 分布：用 1/2/4/8 片写负载 + 进程 kill 注入统计指纹变化分片数；报告分布；“多数只影响 1–2 片”若不被数据支持，只影响收益结论，不改变正确性。
- 每格至少 3 次运行；P50/P95/max + 失败数；不得只报平均。

### 4.4 证据边界

- 在 M10-14 证据生成前，**不得**把 §4.3 的目标值写成“已达成”，不得用 `8b76410` 原型数值充当选择性证据。
- 性能结果必须来自真实 `aetherd` 启动路径 + 真实 Electron；debug 二进制/独立脚本只作开发参考。
- 运行期 7 天调度使用注入时钟/缩短间隔验证，不得以真实等待 7 天作为门禁（ADR-013）；调度器 panic/重启、超时/迟到结果、sleep/wake/clock jump、shutdown/import 也必须注入验证。
- 证据必须包含 `adr039-baseline.json`/`.sha256` 的哈希与来源、`verify:m10-10` 基线断言、`verify:m10-14` 默认/Level 1/Level 2 复跑命令。
- 证据脱敏：无用户名/绝对路径/密钥；使用 `<home>`/`<external>` 占位。

---

## 5. 基线、版本与契约影响

| 面 | 变化 |
|---|---|
| 设计文档 | **v1.36（ADR-040 v1.0 已签署，2026-11-17 升版）**：D2 启动序列/运行期调度/**shutdown-app_exit 取消与 grace**/**导入窗口识别**；D3 选择性计划/严格匹配表/sidecar/基线/`storageCheckCompletedAt` 拆分；D4 状态表；D7 `health.storage_check` 的 `contract_version`/新字段/兼容；D13 无新增（备份/恢复契约不变） |
| ADR-039 | **ADR-040 v1.0 已签署；发布 ADR-039 v1.1（仅变更记录/状态，正文不改，amended by ADR-040）**，列明 D-039-2 §2/§4、D-039-3 §1/§2、D-039-5 §1/附录 C、附录 B、§6.3、M10-10 DoD-3/4；不再保留“v1.0 amended-by 待定”二选一 |
| P2 计划 | **v0.15（ADR-040 v1.0 已签署，2026-11-17 升版）**：唯一任务表 **14 项**：M10-00～M10-11（12）+ M10-13（收口）+ M10-14（选择性）；**M10-12 保留/未定义**；依赖图 M10-11 → M10-14 → M10-13 → G13；M10-10 DoD-3/4 的 ADR-040 口径；G13 条件 |
| UI-UX | **v0.21（ADR-040 v1.0 已签署，2026-11-17 升版）**：S-00 x/8 + 跳过 N；旧 daemon 兼容；仅主库/选择性完成文案；无新增锚点 |
| 外部消费者 | **发布门**：M10-03 证据列出 HTTP/WS/CLI 消费者清单/升级计划；消费者升级到 v2 或显式声明仅支持 v2；发布说明标注 `contract_version`/`checked_shards`/`state` 语义变化；旧 v1/新 v2 回归记录 |
| M9-01 | `health` 响应字段/`contract_version` 变化 → **重录**；`pnpm verify:m9-01-baseline` 通过 |
| 命令 | **87 不变**（0 新增） |
| 事件 | **0 新增**；EventEnvelope 9 字段不变 |
| 设置键 | **11 不变**（0 新增；`PERIODIC_CHECK_POLL_MS`/`PERIODIC_CHECK_TIMEOUT_MS`/`SCHEDULER_RESTART_BACKOFF_MS`/`SHUTDOWN_CHECK_GRACE_MS` 均为内部编译期常量，不是设置键） |
| 迁移 | **0014 不变**（0 新增） |
| 业务码 | **35 不变**；`IpcErrorCode` 32 不变 |
| 依赖 | **0 新增**；不引入新 crate/包；`cargo deny check` 通过 |
| 生成物 | `types.ts`/`client.ts`/`command-manifest.json`/`error_codes.json` 由 `aether-app` 清单生成；禁止手改；CI diff=0 |
| 验证资产 | `verify:m10-10` = ADR-039 基线模式（`adr039-baseline.json` + `.sha256`，校验来源/哈希/断言）；`verify:m10-14` 默认当前 + `:level1`/`:level2` 回退；`package.json` 注册；`m10_10_storage_check.test.tsx` 双 fixture；metrics 别名 |
| 发布说明 | 标注 `health.storage_check.contract_version=2`、`checked_shards`/`state` 语义变化、`full_coverage`/`planned_shards`/`skipped_shards`；M10-03 消费方升级/仅 v2 声明 |

### 5.1 设计 v1.36 修改清单（逐项，复审 P2-1 / 三审 P2-4）

1. **D2**：启动序列/运行期调度；`full_check_due` 的 None/7 天语义；不新增阻塞 `quick_check`；**shutdown/app_exit 取消/ grace/sidecar 规则；导入维护窗口识别**。
2. **D3**：选择性计划/严格匹配表/sidecar `format_version`/基线；**ADR-039 D-039-2 §4 场景表**（异常启动全量→选择性）；**ADR-039 D-039-3 §2 状态表**（S-00 x/8 + 跳过 N）；`storageCheckCompletedAt` 拆分；**导入 `import_in_progress` 由 `import_control.rs` 持有/复位、调度器只读**。
3. **D4**：`storage_check.write_gate` 语义不变；检查中不是降级；状态表随 D-039-3 §2 更新。
4. **D7**：`health.storage_check` 的 `contract_version`/`planned_shards`/`skipped_shards`/`full_coverage`/兼容矩阵；**ADR-039 D-039-5 §1 与附录 C** 同步；**M10-03 发布门**。
5. **D13**：备份/恢复/维护态契约不变；不新增迁移/命令/设置键。
6. **ADR-039 §6.3 验收矩阵**：`storageCheckCompletedAt` 拆分为启动门禁/全量覆盖；异常启动预算仍 25/30s；干净启动 1.5s 不变；冷缓存 dirty1 P95 ≤25s（Q6）。
7. **M10-10 DoD-3/4**：由 P2 v0.15 同步为 ADR-040 口径（选择性、运行期 7 天、基线、health 兼容、调度器生命周期）；M10-10 原证据不重开，补充分册。
8. **P2 v0.15**：唯一任务表 14 项（M10-00～M10-11 + M10-13 + M10-14；M10-12 保留）、M10-13/M10-14/G13 顺序、依赖图、G13 条件。

---

## 6. G13 边界与任务归属（首轮 P0-4 / 复审 P2-3 / 三审 P2-4）

### 6.1 任务顺序

| 任务 | 顺序 | 证据/说明 |
|---|---|---|
| M10-11 | 既有；可在 M10-14 前 | `verify:m10-11` 保持 |
| **M10-14** | M10-11 后、M10-13/G13 前 | M10-10 补充证据 + `verify:m10-14`/`:level1`/`:level2`；运行期调度/基线/选择性/health 兼容 |
| M10-13 | M10-14 后；若已先执行则重跑/补充 | `verify:m10-13`；最终收口/证据汇总按 P2 v0.15 定义；不得使用 M10-14 前的旧证据 |
| G13 | M10-13 + M10-14 后 | `verify:m10-13` + `verify:m10-14` + `verify:m10-10` 基线 + G13 报告 |

- P2 v0.15 必须：新增 M10-14；明确 M10-13/M10-14/G13 顺序；更新 M10 任务计数/依赖图；G13 条件同时列 `verify:m10-13`/`verify:m10-14`。
- **计数一致性（P2-4）**：P2 v0.14 正文按 M10-00～M10-11 计 12 项，但 G13 证据引用 M10-13；P2 v0.15 唯一任务表为 **14 项**：M10-00～M10-11（12）+ M10-13（收口/证据汇总）+ M10-14（选择性）；**M10-12 保留/未定义**。依赖图：M10-11 → M10-14 → M10-13 → G13。若 M10-12 后续定义，再计入并重排；ADR-040 不再保留“待定计数”。
- 若 M10-13 已在 M10-14 前执行，G13 前必须重跑或补充其证据；不得以“M10-13 已通过”跳过 M10-14 后的复跑。
- M10-10 已按 ADR-039 执行完毕，**不重开原证据**；新增 M10-10 补充分册并复跑 `verify:m10-10`（ADR-039 基线模式）。原证据中“ADR-040 仍为 Proposed/未实施”的说明由补充分册更新为“ADR-040 v1.0 已签署、M10-14 待实施”，不改原证据结论。M10-08/M10-09/M10-11 证据不重开。

### 6.2 G13 条件（追加）

1. ADR-040 v1.0（四角色签署）已归档；ADR-039 v1.1（amended by ADR-040）已发布；
2. M10-14 DoD/证据通过（dirty 1/2/4/8、冷/热、故障注入、运行期 7 天、基线饥饿、调度器监督/超时/迟到结果、shutdown/import、后台/周期、标志/计数、health 兼容）；
3. M10-10 补充证据 + `pnpm verify:m10-10`（ADR-039 基线模式，校验基线哈希/来源/断言）通过；
4. `pnpm verify:m10-14`、`pnpm verify:m10-14:level1` 通过；`pnpm verify:m10-14:level2` 通过，或基线构建不可用时以“回退演练证据 + 固定 fixture/旧二进制 + `verify:m10-10`”替代（须记录；Level 2 必须是行为测试）；`package.json` 脚本已注册；
5. M10-13 在 M10-14 后重跑/补充，`verify:m10-13` 通过；
6. UI-UX v0.21 / M9-01 重录 / **M10-03 发布门：消费者清单/升级计划；外部消费者升级 v2 或显式声明仅 v2；发布说明标注 `contract_version`/语义变化；旧 v1/新 v2 回归记录**；
7. 安全/数据 + QA 对 §3.2 残余风险签核；
8. G13 报告明确区分 `startupCheckCompletedAt`（启动门禁）与 `fullCoverageCheckCompletedAt`（信息性），不得混用。

- **ADR-040 已签署（2026-11-17）**；在 M10-14 实施/证据完成前，仓库仍按 ADR-039 v1.0 全量口径执行；G13 不得以“ADR-040 已签署”替代 M10-14 证据。
- 若产品后续改回“G13 后实施”，必须另记录裁定并恢复 ADR-039 口径；不得以口头裁定替代 ADR/证据。
- **不得**以“ADR-039 字面已满足”单独判定 G13（Q1=B 后）。

---

## 7. 失效条件与回退（首轮 P1-7 / 复审 P2-7 / 三审 P1-1、P2-1、P2-5）

出现任一条件，**立即回退 ADR-039 全量启动检查**（关闭选择性路径；回退范围见 §7.2）：

1. “干净指纹 + 页级损坏”故障注入在选择性或后台全量路径漏检；
2. 写损坏页/同分片其他页导致损坏扩大/不可恢复，或后台检查失败后仍继续写入；
3. `background_check_completed`/`clean_start_count` 规则无法保证（后台全量被绕过、错误置 true、下次未全量回退）；
4. 仅主库变脏/8 片全脏/前置条件负例测试失败；
5. 性能不达标：dirty 1–2 <30%、dirty 4 <10%、dirty 8 劣化 >5%、dirty 1/2/4 P95 >25s（含冷缓存 dirty 1）、dirty 8 P95 >30s、冷缓存 dirty 2/4/8 低于对应阈值、main-only 高于全量回退；
6. 运行期调度器无法保证：持续运行超过 7 天目标未触发、维护/导入延后不可接受、重复触发、失败未降级、**调度器 panic/退出无法重启或连续失败未降级、超时/取消后迟到结果污染状态、sleep/wake/时钟跳变超过 §5 最大延迟（7d+1h）、shutdown 未按 grace/未正确写 false/不写 clean sidecar**；**`PERIODIC_CHECK_TIMEOUT_MS` 低于实测 P99，或 HDD 无法满足且未定义支持边界/回退 → 回退/另立 ADR**；
7. `last_full_check_at=None` 基线在连续异常启动下仍饥饿；
8. `mtime` 假阳性导致优化失效（按 §4.3 记录），且无法在不改变安全语义的前提下解决；
9. 安全/数据或 QA 不接受残余风险，或签核缺失；
10. G13 证据无法复跑、与实现行为不一致，或 `verify:m10-10` / `verify:m10-14`（默认/Level 1/Level 2）失败。

### 7.1 回退方式

- **Level 1（选择性关闭，保留新代码）**：`STARTUP_CHECK_SELECTIVE_ENABLED=false`（编译期常量，不是设置键、不是用户开关）→ `plan_startup_checks` 对任何不匹配返回主库 + 全部分片；严格跳过路径保持 ADR-039；运行期调度/基线/health 新字段仍保留（用于 7 天兜底/兼容），但不再展示选择性跳过。
- **Level 2（完整回退 ADR-039）**：回退代码/提交至 ADR-039 口径；恢复旧 `health` 字段语义/UI/指标；默认 `verify:m10-14` 不运行；`verify:m10-10` 基线模式 + `verify:m10-14:level2`（固定 ADR-039 基线构建/fixture）运行（若基线构建可用；否则回退演练证据）。
- 回退提交/版本号、原因、影响范围写入 M10-14 证据与 G13 报告；不得只口头回退。

### 7.2 回退范围表（复审 P2-7 / 三审 P2-2、P2-5）

| 组件 | Level 1：选择性关闭（保留新代码） | Level 2：完整回退 ADR-039 |
|---|---|---|
| 启动计划 | `STARTUP_CHECK_SELECTIVE_ENABLED=false`；任何不匹配 → 主库 + 全部分片；严格跳过不变 | 回退代码/提交；恢复 ADR-039 `plan_startup_checks` |
| `health` | `storage_check.contract_version=2` 保留；新字段保留；所有不匹配路径 `full_coverage=true`；UI 可用新字段但无“已跳过” | 恢复 `contract_version=1`/旧字段语义（或省略新字段）；M9-01 重录回旧基线 |
| 指标 | `startupCheckCompletedAt`/`fullCoverageCheckCompletedAt` 保留；`storageCheckCompletedAt` 别名保留 | 恢复旧 `storageCheckCompletedAt` 主指标；新指标不再产出 |
| sidecar | `format_version` 保留（增量字段）；旧代码可忽略未知字段 | 旧代码写 sidecar 不带 `format_version`；新代码再次升级时缺字段 → 全量回退（安全）；记录兼容性 |
| verifier | `verify:m10-14:level1`（当前代码选择性关闭）+ `verify:m10-10` 基线；默认 `verify:m10-14` 不运行 | `verify:m10-10` 基线 + `verify:m10-14:level2`（固定 ADR-039 基线构建/fixture，覆盖全量回退/严格跳过/旧 health/sidecar/旧 metrics；不可用时固定 fixture/旧二进制 + 回退演练证据 + 基线）；默认 `verify:m10-14` 不运行 |
| G13 | 报告 Level 1 状态；不得声称选择性检查/G13 通过 | 报告 Level 2 状态；不得声称选择性检查/G13 通过；等待重新签署/实施 |
| UI | 新字段可显示，但无“已跳过 N”；S-00 回到 x/8 | 恢复 ADR-039 UI 文案/字段 |

- 回退后仍保留后台全量/周期/运行期完整检查；不得删除 sidecar 安全检查。
- 回退不得静默修改 `verify:m10-10` 的 ADR-039 历史断言；如需调整，记录证据。

---

## 8. 风险与缓解

| 风险 | 缓解 | 失效条件 |
|---|---|---|
| 干净指纹但页级损坏漏检 | 后台全量 `quick_check` 每次保留；运行期/启动周期完整 `integrity_check`；故障注入；安全/QA 签核 | 漏检 → 回退 ADR-039 |
| 主库单独变脏漏检 | 任一指纹变化 → 主库必查；仅主库变化单测/故障注入 | 主库未查 → 阻止合并 |
| 选择性后误标全量完成 | 选择性不置 `background_check_completed`；`full_coverage` 区分覆盖；下次 `false` 全量回退 | 误标 → 阻止合并/回退 |
| 后台/周期检查被绕过 | D-040-3 双层兜底；`full_check_due` 替代后台 `quick_check`；失败 `persist_degraded` | 未执行 → 回退 |
| **持续运行无 7 天完整检查** | 运行期调度器（D-040-3 §5）；注入时钟测试；维护延后；失败降级；正常 ≤7 天/最大 7d+1h | 超过 7d+1h 未触发/重复触发 → 回退 |
| **调度器生命周期/迟到结果** | 受监督重启/退避/连续失败降级；`check_epoch`/取消令牌；迟到结果丢弃；失败/超时 `storage_check.state=failed`/`error`/`background_check_completed=false`/S-08；sleep/wake/时钟跳变测试 | 调度器死亡/迟到结果污染状态 → 回退 |
| **周期检查超时误判/HDD P99** | DoD-6 实测 5.22 GiB SSD/HDD `integrity_check` P99；`PERIODIC_CHECK_TIMEOUT_MS` 按 P99+余量；HDD 边界/回退 | timeout < P99 或 HDD 无边界 → 回退/另立 ADR |
| **shutdown/app_exit/import 交互** | 取消令牌/`SHUTDOWN_CHECK_GRACE_MS`；未完成不写 clean sidecar/false；导入 `import_in_progress` 由 `import_control.rs` 置位/提交/回滚/崩溃复位，调度器只读延后；DoD-3/5 + §3.3 #14 | 退出卡顿/误写 true/导入重叠/标志残留 → 回退 |
| **`last_full_check_at=None` 基线饥饿** | `full_check_due` 将 None 视为 due；任意启动路径调度；运行期重试；连续异常启动测试 | 仍饥饿 → 回退 |
| `clean_start_count` 被选择性/异常污染 | 仅严格干净 +1；选择性/异常清零；7 天/20 次/None 测试 | 计数错误 → 回退 |
| UI 把部分检查显示为 8/8 | `checked_shards`/`planned_shards`/`skipped_shards`/`full_coverage`；S-00 文案；测试 | 显示 8/8 假完成 → UI 缺陷 |
| 旧客户端误读 `health` | `contract_version` + 兼容矩阵；旧 daemon fixture；**M10-03 发布门（升级/仅 v2 声明/发布说明）** | 未回归/未升级 → 阻止发布 |
| `verify:m10-10` 仍断言 ADR-039 | 基线模式：`adr039-baseline.json` + `.sha256`，校验来源/哈希/断言；`verify:m10-14` 当前 + Level 1/2 | 文件漂移/仅存在性检查 → 阻止合并 |
| 基线文件漂移/不可审计 | `generate-adr039-baseline.mjs` 固定 `11cb438`；`.sha256` 只读；评审更新 | 哈希/来源不符 → 阻止合并 |
| Level 2 回退未真实验证 | 最小行为集（全量回退/严格跳过/旧 health/sidecar/旧 metrics）；固定 fixture/旧二进制；不得只用静态快照 | 只静态快照 → 阻止 |
| 残余风险写入判据不可执行 | §3.2 可执行判据：损坏页/其他页/停止写入/记录范围；per-shard 写门另立 ADR | 判据不可执行 → 安全/QA 不签核 |
| `mtime` 假阳性 | 仅辅助字段；按 DoD-6 方法记录率；旧 sidecar 全量回退 | 优化失效 → no-go/回退 |
| 主库增长导致超 30s | 不假设主库小；记录大小/耗时；超 30s → 回退/另立 ADR | 超硬上限 → G13 不通过 |
| 性能收益不足 | dirty 1/2/4/8 矩阵；Q5/Q6 no-go（1–2<30%、4<10%、8>5% 劣化；P95>25/30；冷 dirty1 >25） | 未达阈值 → 不实施 |
| 回退不彻底 | §7.2 回退范围表；Level 1/2；verifier 模式；证据记录 | 回退后仍声称选择性/G13 → 阻止 |
| G13 证据与实现不一致 | M10-14 先改代码/测试再出证据；§0.6 现状声明；`verify` 三模式复跑 | 不一致 → G13 不通过 |

---

## 9. 评审与签核记录

| 日期 | 角色/评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-11-16 | ADR-040 v0.1 作者（AI） | 草案 | 未实施；等待评审 |
| 2026-11-17 | 架构/安全/QA/产品一致性评审（AI，OpenCode） | **首轮暂不通过：4 P0 + 9 P1 + 4 P2** | 见 `docs/ADR-040-评审意见.md` |
| 2026-11-17 | 产品 | **Q1=B / Q2=A / Q3=A / Q4=A** | 首轮产品确认 |
| 2026-11-17 | ADR-040 v0.2 修改者（AI） | 按首轮意见修订；提交复审 | 见 `docs/ADR-040-复审意见.md` |
| 2026-11-17 | 架构/安全/QA/产品一致性复审（AI，OpenCode） | **暂不通过：首轮 P0 全闭环；新增 1 P0 + 5 P1 + 8 P2** | 见 `docs/ADR-040-复审意见.md` |
| 2026-11-17 | 产品 | **Q5=确认 §4.3 阈值并补齐 no-go** | Q1–Q5 已确认；待 ADR 文本复审 |
| 2026-11-17 | ADR-040 v0.3 修改者（AI） | 按复审意见修订；提交三审 | 见 `docs/ADR-040-三审意见.md` |
| 2026-11-17 | 架构/安全/QA/产品一致性三审（AI，OpenCode） | **有条件通过（Conditional Pass）：无新增 P0；1 P1 + 6 P2 需签署前闭环** | 见 `docs/ADR-040-三审意见.md` |
| 2026-11-17 | 产品 | **Q6=冷缓存 dirty1 25–30s 严格 no-go（P95 ≤25s）** | Q1–Q6 已确认 |
| 2026-11-17 | ADR-040 v0.4 修改者（AI） | 按三审意见修订；P1-1/P2-1～P2-6 文本闭环；提交四审 | 见 `docs/ADR-040-四审意见.md` |
| 2026-11-17 | 架构/安全/QA/产品一致性四审（AI，OpenCode） | **文本评审通过（Conditional Pass，附 6 P2）；无新增 P0/P1** | 见 `docs/ADR-040-四审意见.md` |
| 2026-11-17 | ADR-040 v1.0 修改者（AI） | 按四审 P2 文本收口；提交四角色签署 | 本文件 |
| 2026-11-17 | 架构 | **通过** | 计划/运行期调度/严格匹配/health/指标/回退 |
| 2026-11-17 | 安全/数据 | **通过** | 残余风险窗口（7 天目标/最大 7d+1h）、损坏页写入判据、运行期调度、shutdown/import |
| 2026-11-17 | QA | **通过** | dirty 1/2/4/8、冷/热、7 天/基线、后台全量、verify 三模式、故障注入 |
| 2026-11-17 | 产品 | **通过** | Q1–Q6 已确认 |

> **ADR-040 v1.0 已由架构/安全/QA/产品四角色签署（2026-11-17）。** 本签署不构成 M10-14 实施/G13 通过；M10-14 证据、5.22 GiB 复测、故障注入与 G13 报告另验。

---

## 10. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v1.0 | 2026-11-17 | 按 `docs/ADR-040-四审意见.md` 收口 6 项 P2：7 天窗口统一为“正常 ≤7 天、最大 7d+1h”；调度器失败/超时 `storage_check.state=failed`/`background_check_completed=false`/降级；DoD 引用改为 `DoD-3/5 + §3.3 #11/#14`；`PERIODIC_CHECK_TIMEOUT_MS` 按 SSD/HDD P99 证据调整；`import_in_progress` 由 `import_control.rs` 持有/复位；Level 2 最小行为集/固定 fixture；**架构/安全/QA/产品四角色签署通过** | ADR-040 v1.0 签署记录（AI 代记） |
| v0.4 | 2026-11-17 | 按 `docs/ADR-040-三审意见.md` 修订：Q6 冷缓存 dirty1 25–30s 统一严格 no-go（P1-1）；运行期调度器生命周期加固：监督/重启、检查 epoch/取消令牌/迟到结果丢弃、sleep/wake/时钟跳变/最大延迟（P2-1）；`adr039-baseline.json` 来源/哈希/只读与 `verify:m10-14` Level 1/2（P2-2/P2-6）；旧客户端发布门（P2-3）；ADR-039 v1.1 与 14 项任务表（P2-4）；shutdown/app_exit/import 交互（P2-5） | ADR-040 v0.4 修改者（AI） |
| v0.3 | 2026-11-17 | 按 `docs/ADR-040-复审意见.md` 修订：新增运行期 7 天调度器（P0-1）；`last_full_check_at=None` 任意启动路径基线（P1-5）；`verify:m10-10` ADR-039 基线模式 + `verify:m10-14` 当前实现（P1-1）；`health.contract_version`/兼容矩阵（P1-2/P2-2）；残余风险写入可执行判据（P1-3）；Q5 no-go（P1-4）；补 ADR-039 修订范围/ADR-039 变更记录/M10-13-M10-14 顺序/schema 检查对象/mtime 测量/回退范围/字段名说明（P2-1～P2-8） | ADR-040 v0.3 修改者（AI） |
| v0.2 | 2026-11-17 | 按 `docs/ADR-040-评审意见.md` 首轮修订：修订 ADR-039 D-039-2/3/5；主库单独变脏；完整 sidecar 匹配表；`background_check_completed`/`clean_start_count`；后台全量+周期双层兜底；`health`/UI/指标拆分；指纹字段/`format_version`；失效条件/回退；M10-14/G13 | ADR-040 v0.2 修改者（AI） |
| v0.1 | 2026-11-16 | 创建草案：选择性检查脏分片、附带主库、周检/20 次、安全论证与验证 | ADR-040 v0.1 作者（AI） |

---

## 附录 A：`clean_shutdown.json` 示例（v0.4 口径）

```json
{
  "format_version": 1,
  "generation": "gen-01J...",
  "app_version": "<version>",
  "closed_at": 1760000000000,
  "close_sequence_completed": true,
  "write_queue_drained": true,
  "background_check_completed": true,
  "last_full_check_at": 1759990000000,
  "clean_start_count": 12,
  "main": {
    "schema_version": 14,
    "header_change_counter": 123,
    "file_size": 123456,
    "wal_size": 0,
    "modified_at_ms": 1760000000000,
    "last_seq": null
  },
  "shards": [
    {
      "shard_id": 0,
      "schema_version": 14,
      "header_change_counter": 456,
      "file_size": 789012,
      "wal_size": 0,
      "modified_at_ms": 1760000000000,
      "last_seq": 12345
    }
  ]
}
```

- `format_version` 缺失/不支持 → 全量回退。
- `last_seq` 仅记账，不参与启动比对。
- `mtime` 仅辅助，不单独作为跳过依据。
- `background_check_completed` 是历史字段名，语义=全量覆盖检查完成（含全量回退启动/后台全量/周期完整），不是只看后台。

## 附录 B：`health.storage_check` 示例与兼容

**严格跳过 + 后台全量尚未开始（v2）：**

```json
{
  "contract_version": 2,
  "state": "ok",
  "mode": "startup",
  "write_gate": "open",
  "main_checked": false,
  "checked_shards": 0,
  "total_shards": 8,
  "planned_shards": 0,
  "skipped_shards": 8,
  "full_coverage": false,
  "completed_at": 1760000000000
}
```

**选择性计划（主库 + 2 片，完成，v2）：**

```json
{
  "contract_version": 2,
  "state": "ok",
  "mode": "startup",
  "write_gate": "open",
  "main_checked": true,
  "checked_shards": 2,
  "total_shards": 8,
  "planned_shards": 2,
  "skipped_shards": 6,
  "full_coverage": false,
  "completed_at": 1760000000000
}
```

**仅主库变化（0 片，完成，v2）：**

```json
{
  "contract_version": 2,
  "state": "ok",
  "mode": "startup",
  "write_gate": "open",
  "main_checked": true,
  "checked_shards": 0,
  "total_shards": 8,
  "planned_shards": 0,
  "skipped_shards": 8,
  "full_coverage": false,
  "completed_at": 1760000000000
}
```

**全量回退（主库 + 8 片，完成，v2）：**

```json
{
  "contract_version": 2,
  "state": "ok",
  "mode": "startup",
  "write_gate": "open",
  "main_checked": true,
  "checked_shards": 8,
  "total_shards": 8,
  "planned_shards": 8,
  "skipped_shards": 0,
  "full_coverage": true,
  "completed_at": 1760000000000
}
```

**后台全量进行中（v2）：**

```json
{
  "contract_version": 2,
  "state": "checking",
  "mode": "background",
  "write_gate": "open",
  "main_checked": true,
  "checked_shards": 3,
  "total_shards": 8,
  "planned_shards": 8,
  "skipped_shards": 0,
  "full_coverage": true
}
```

**旧 daemon（v1，无新字段）：**

```json
{
  "state": "ok",
  "mode": "startup",
  "write_gate": "open",
  "main_checked": true,
  "checked_shards": 8,
  "total_shards": 8
}
```

- 新 UI 读旧 daemon：按 v1 语义展示 x/8，不显示“已跳过/本次检查”，不得声称全量覆盖。
- 旧客户端读新 daemon：可能误读 `checked_shards`/`state`；必须升级或读取 `contract_version`/`full_coverage`；M10-03 回归。

## 附录 C：术语与引用

| 术语 | 含义 |
|---|---|
| 指纹变化分片 | 启动快速比对字段与 sidecar 不一致的分片（旧称“脏分片”，本 ADR 统一为“指纹变化分片”） |
| 选择性启动检查 | 启动阻塞阶段检查主库 + 指纹变化分片 |
| 全量回退 | 前置条件不满足或 8 片全变，检查主库 + 全部分片 |
| 后台全量检查 | 写门打开后后台 `quick_check` 主库 + 全部分片 |
| 启动周期完整检查 | 启动路径评估 `full_check_due` 后触发的后台 `integrity_check` |
| 运行期周期完整检查 | 持续运行期间由运行期调度器按 7 天/None 基线触发（20 次计数由启动路径评估；运行期也会读取但计数不变） |
| 启动计划完成 | `startupCheckCompletedAt`，写门打开；不必然全量覆盖 |
| 全量覆盖完成 | `fullCoverageCheckCompletedAt`，全量回退启动检查/后台全量/周期完整检查完成 |
| baseline pending | `last_full_check_at=None`；任意启动路径调度基线 `integrity_check`，成功后写入时间戳 |
| `contract_version` | `storage_check` 契约版本：2=ADR-040；缺失/1=ADR-039 |
| `background_check_completed` | 历史字段名，语义=全量覆盖检查完成（全量回退启动/后台全量/周期完整） |

**引用：** ADR-039 v1.0 D-039-2/D-039-3/D-039-5、附录 B/C、§6.3、M10-10 DoD-3/4；《设计文档》v1.35 D2/D3/D4/D7/D13；P2 计划 v0.14 M10-10/M10-13/G13；UI-UX v0.20 S-00/S-08；`docs/M10-10-证据.md`、`docs/M10-10-metrics.json`；`docs/ADR-040-评审意见.md`、`docs/ADR-040-复审意见.md`。
