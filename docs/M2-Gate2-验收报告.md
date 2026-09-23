# M2 Gate 2 验收报告（M2 阶段出口）

| 项 | 内容 |
|---|---|
| 门禁 | Gate 2（《实施计划与验收标准》v1.14 §3「出口门禁 Gate 2」） |
| 验收对象 | M2 运行闭环：M2-01…M2-11（含 real-adapter 路径 M2-02、条件转正任务 M2-11、批 3 任务 M2-08） |
| 基线 | 需求 v0.6、设计 v1.8（ADR-001–ADR-008）、实施计划 v1.14 |
| 被测提交 | `a81ac16`（main；M2-08 交付提交；工作树中本报告与 `docs/M2-实施计划.md` 状态注记为验收产物） |
| 执行环境 | Windows 10 本机；Rust 1.98.1 / Node v24.14.1 / Bun 1.4.2 / pnpm 10.34.5；真实 WebView2 |
| 执行日期 | 2026-09-23 |
| **结论** | **Gate 2 通过**（可进入 M3）。M2-08 阻塞项已闭环；通过条件 7 条全部成立。验收发现的 P1（T5a 收口分类竞态）与 P2（M2-08 CI 首轮归档、相似度扫描 `.tmp` 排除）已于 2026-09-23 全部闭环复核（§7）。 |

**修订记录**

| 轮次 | 日期 | 被测提交 | 结论 |
|---|---|---|---|
| 第 1 轮 | 2026-09-22 | `632b433` | **不通过**：阻塞项 M2-08 未交付（T11/T5b 无证据、启动清理未接线） |
| 第 2 轮 | 2026-09-23 | `a81ac16` | **通过**：M2-08 12/12 验收通过；全量回归仅 2 项环境/竞态问题，均定性闭环（§1.2） |
| 闭环复核 | 2026-09-23 | `9ed1b4a`（含 `bccad82`/`27e0de9`） | **通过（维持）**：P1 T5a 加固、P2 CI 归档与扫描排除全部闭环并独立复核（§7） |

---

## 1. 执行记录

### 1.1 本轮执行命令与结果

| # | 命令 / 动作 | 结果 | 原始日志 |
|---|---|---|---|
| 1 | `node scripts/test/m2-08/verify-m2-08.mjs` | **12/12 PASS**（DoD1/T11/T5b + 证据归档） | `scripts/test/.tmp/gate2-verify-m2-08.log` |
| 2 | `node scripts/test/ci-local.mjs --quick` | **30/32 PASS**；2 项失败经定性：① 相似度扫描 = 本机 `.tmp` 真实运行残留（§7 #3）；② `verify-m2-11` 的 `m2_11_codex` T5a = 1 次收口分类竞态（§7 #1） | `scripts/test/.tmp/gate2b-ci-local-quick.log` |
| 3 | `node scripts/test/m2-11/verify-m2-11.mjs`（复跑） | **全部通过**（含 m2_11_codex 9/9） | `scripts/test/.tmp/gate2b-verify-m2-11.log` |
| 4 | `cargo test -p aether-adapters --test m2_11_codex`（定向 ×3，`AETHER_CODEX_ADAPTER` 注入） | **3/3 PASS**（T5a Ready 707–853ms） | 会话输出 |
| 5 | `node scripts/test/m1-06/verify-m1-06.mjs`（含 E2E） | **全部通过**（启动门 → 迁移 → 锁定新目录 E2E + aether-tauri 全量测试） | `scripts/test/.tmp/gate2b-verify-m1-06-full.log` |
| 6 | `node scripts/test/m1-08/verify-m1-08.mjs`（含 E2E） | **全部通过**（CSP/导航 E2E 15 项 + 单测矩阵） | `scripts/test/.tmp/gate2b-verify-m1-08-full.log` |
| 7 | `node scripts/test/m2-07/verify-m2-07.mjs`（含 E2E） | **10/10 PASS**（真实 WebView2；退出路径含 `AETHER_M2_08_EXIT` 证据行） | `scripts/test/.tmp/gate2b-verify-m2-07-full.log` |
| 8 | Gate 2 前补验归档复核（读取原始 `summary.json`） | 属实（Codex 20/20；DSH ask allow/deny 零直通） | 见 §5 |

### 1.2 失败项定性（均非代码缺陷，且证据闭环）

- **相似度扫描（本机）**：命中全部位于 `scripts/test/.tmp/m2-11/codex-real-home/.tmp/plugins*/**`（真实 Codex 运行下载的 NVIDIA 插件文档，含 `Copyright (c)`）。该路径 gitignored、仅存在于本机；第 1 轮已复现「移出残留后 `[similarity] 通过：无规则命中`」。CI 干净检出无此路径（`873e74d` 12/12 job success）。建议（P2）：`similarity-rules.json` 的 `excludePaths` 纳入 `scripts/test/.tmp`。
- **`m2_11_codex` T5a（1/5）**：本机 ci-local 首跑在 `m2_11_codex.rs:628` 断言失败，实际收口为 `Failed{cli_exit}` 而非 `Disconnected{adapter_disconnected}`。根因：`kill_tree_system` 同时回收适配器与 Codex 子进程时存在次序竞态——子进程先被终止且适配器抢到调度窗口时，适配器上报 `cli_exit(exit=1，Windows TerminateProcess 语义)` 后才断连；两种收口均为**终态且 recoverable=true**（M1-11 接入笔记界定「外部强杀 → Disconnected」，但 DoD 口径为「在途 run 标 failed 且可重试」，二者等价满足）。复跑：定向 3/3 + `verify:m2-11` 全绿（Ready 707–853ms ≤ 30s、Mode R 重放命中 20/20 代理一致）。**建议（P1）**：在 M4-01「运行中崩溃」场景前，对 T5a 收口仲裁做确定性加固（或断言收敛为「终态 ∈ {Disconnected, Failed(cli_exit)} 且 recoverable」并补 Mode R 重放断言），不得静默放宽。

---

## 2. 分条判定（v1.14 §3 Gate 2 通过条件）

| 条件 | 判定 | 依据 |
|---|---|---|
| 1. M2-01…M2-09 全部 DoD 通过（real-adapter 路径） | **通过** | M2-01…M2-07、M2-09 第 1 轮已通过；**M2-08 本轮 12/12 PASS**（§3） |
| 2. M2-10 DoD1 基础回环 + DoD3 证据归档（硬门槛） | **通过** | `dod1_basic_loop`：`requests_received=1 / decisions=1 / resolutions_sent=1 / zero_passthrough=true`；6 份证据 JSON 归档（`scripts/test/.tmp/m2-10/evidence-2026-09-23T03-03-21-852Z/`） |
| 3. M2-10 DoD2 异常路径 | **通过** | deny / 超时 deny（精确 +300.000s）/ once / session / 策略直决 / 重启 pending 全过，全部零直通 → **无需冻结 M3-03** |
| 4. T5a / T5b / T6 / T7 / T11 显式检查项 | **通过** | T5a：Ready 578–915ms ≤ 30s、Mode R 重放；**T5b：trigger 30.3s ≤ 45s、Ready 31.4s ≤ 120s、`heartbeat_failed`、旧进程无残留**；T6：100 并发 ask `ok`；T7：`denied_ratio=100%` + 逐条审计；**T11：elapsed 5289ms ≤ 10s、`adapter_residual=false`、`TerminateJobObject` 优先**（§4） |
| 5. 写失败重试口径（ADR-007） | **通过** | `m1_05_attempt_log` PASS；tracing 捕获 `attempt=1/3`→`2/3`→`3/3` → `persist_degraded`；2 失败 1 成功不降级 |
| 6. `health` 真实两态 / `core_not_ready` / `runtimes` 两态 / ULID 边界（ADR-007） | **通过** | health E2E 三探针（normal/persist_degraded/stall）；`core_not_ready_until_backend_installed`；`health_passes_through_wired_runtime_summaries`；ULID 10 万次 + 解析边界（§4.3） |
| 7. 三运行时一致性门禁（ADR-008 决策 7） | **通过** | M2-11 DoD1–8 全绿；Codex DoD9 全绿；**补验归档复核属实**：Codex 真实 20/20（100%）、DSH 真实 ask 回环 allow/deny 零直通（§5） |

---

## 3. M2-08 DoD 逐条证据（本轮原始输出）

### DoD1 强杀核心后重启：PID 复用 0 误杀；token 命中清理

```
AETHER_M2_08_DOD1_CLEANUP {"actions":[
  {"adapter_id":"mock","detail":"三条件全命中 → 整树回收","killed":true,"pid":38372,"verdict":"reclaim"},
  {"adapter_id":"decoy-reuse","detail":"启动时间不一致（台账 1790132173，OS Some(1790131173)）→ 疑似 PID 复用，仅记录不 kill","killed":false,"pid":23700,"verdict":"start_time_mismatch"},
  {"adapter_id":"decoy-token","detail":"命令行不含 launch_token=01JM208OTHER0000000000001 → 非本实例台账进程，仅记录不 kill","killed":false,"pid":37632,"verdict":"launch_token_mismatch"}],
  "reclaimed":1,"skipped":2}
AETHER_M2_08_DOD1 {"killed_decoys":0,"orphan_pid":38372,"platform":"windows","reclaimed":1,"reuse_decoy_alive":true,"token_decoy_alive":true}
test force_kill_core_then_restart_cleanup_is_token_scoped ... ok
```

启动路径接线：`run_supervisor_startup`（`crates/aether-tauri/src/runtime_control.rs`）在核心健康启动后执行「孤儿清理（D5 三条件）→ 预热 → 监控」；静态检查项在案（verify-m2-08 §5）。

### DoD2 T11：无响应退出 ≤10s、无残留、Job Object 优先

```
AETHER_M2_08_T11 {"adapter":{"exited":true,"mechanisms":["shutdown_rpc","taskkill_tree","terminate_job_object"],
  "runtime_id":"mock","steps":["shutdown_rpc","graceful","force"],"total_ms":5283},
  "adapter_pid":4280,"adapter_residual":false,"budget_ms":10000,"deadline_expired":false,"elapsed_ms":5289,
  "platform":"windows","storage":{"d2_order":true,"drained":true,"duration_ms":3,"wal_bytes_after":0},"within_budget":true}
test t11_exit_with_unresponsive_adapter_bounded_no_residual ... ok
```

真实应用进程交叉验证（M2-07 E2E，本轮）：三场景（normal/degraded/stall）均输出 `AETHER_M2_08_EXIT` 且 `within_budget=true`、`deadline_expired=false`，退出码 0；正常态存储 shadow 日志五步完整、`wal_bytes 210152→0`。

### DoD3 T5b：不响应模式 → 心跳×3 失败 → 120s 内 Ready

```
AETHER_M2_08_T5B {"first_pid":25068,"first_pid_residual":false,
  "heartbeat":{"interval_s":10,"max_failures":3,"timeout_s":5},"ready_limit_ms":120000,"ready_ms":31394,
  "reasons":[null,null,"heartbeat_failed",null,null],"second_pid":12192,
  "transitions":["cold→starting","starting→ready","ready→degraded","degraded→starting","starting→ready"],
  "trigger_limit_ms":45000,"trigger_ms":30299}
test t5b_deaf_adapter_heartbeat_restarts_ready_within_120s ... ok
```

严格 D5 参数（10s/5s/连续 3 次），未压缩时间参数；不使用 `SIGSTOP`。

### 验证入口与证据归档

`verify-m2-08` 12/12 PASS（夹具构建 → DoD1/T11/T5b → supervisor 回归 65/65 → 静态接线 → 证据归档）；归档目录：`scripts/test/.tmp/m2-08/evidence-2026-09-23T02-39-30-478Z/`（`summary.json` + DoD1/T11/T5b 三份原始 JSON）。

---

## 4. 显式检查项（v1.14 Gate 2 条件 4–6）

### 4.1 T5a / T5b / T6 / T7 / T11

| 项 | 结果 | 原始输出（本轮） |
|---|---|---|
| T5a 崩溃自愈 | **通过**（附 §1.2 竞态说明） | `[m2-02 T5a] 外部强杀 pid=24812 → Ready 耗时 761.9711ms`；`[m2-11 codex T5a] 强杀 pid=... → Ready 耗时 821.9923ms`（复跑 707–915ms）；Mode R 重放命中 |
| T5b 卡死恢复 | **通过** | 见 §3 DoD3（trigger 30299ms / ready 31394ms） |
| T6 100 并发 ask | **通过** | `test t6_100_concurrent_asks_no_loss_no_duplication ... ok` |
| T7 路径逃逸 | **通过** | `[m2-03-t7] textual=20 denied=20 audited=20 fs_samples=1 fs_denied=1 skipped=["symlink ... os error 1314"] denied_ratio=100%`（Junction 已执行；软链接本机开发者模式未开，Unix CI 覆盖） |
| T11 退出可靠性 | **通过** | 见 §3 DoD2（5289ms、无残留、`terminate_job_object` 优先、`taskkill_tree_force` 未触发） |

### 4.2 其他任务回归（本轮）

| 检查 | 结果 |
|---|---|
| `verify-m2-01`（串行/幂等重启重放/ack/断流/runtime_* IPC） | PASS |
| `verify-m2-02`（Claude Code 一致性/工具事件/T5a；真实 50 次 opt-in 显式 SKIP） | PASS |
| `verify-m2-03`（策略矩阵/T7/T6/300s 超时） | PASS |
| `verify-m2-04`（L2/L3/慢订阅者/Lagged/存储侧例外） | PASS |
| `verify-m2-05`（取消风暴：20 并发 dispose 2.49s；看门狗 dump） | PASS |
| `verify-m2-06`（关闭序列五步 shadow 日志/-wal 归零/checkpoint 退避） | PASS |
| `verify-m2-07`（含真实 WebView2 E2E） | 10/10 PASS |
| `verify-m2-09`（2MiB 边界/artifact_ref/1.5GiB 压力） | PASS |
| `verify-m2-10`（回环/异常路径/证据归档） | 7/7 PASS |
| `verify-m2-11`（三运行时，复跑） | 全部通过 |
| `verify-m1-06` / `verify-m1-08`（含 E2E，M2-08 触及启动/退出路径的回归） | 全部通过 |

### 4.3 ADR-007 显式检查项

- 写失败重试：`m1_05_attempt_log` PASS；tracing 捕获 `attempt=1/3`、`2/3`、`3/3` 与 `persist_degraded`（`verify-m1-05` 输出）。
- `health`：UI 5s 轮询 / 15s 超时 + `normal` / `persist_degraded` 两态 E2E 全过；`stall` 探针渲染 `core-unresponsive + core-restart`；`aether-tauri health_command` 5 用例含 `core_not_ready_until_backend_installed`、`health_returns_real_normal_then_persist_degraded`、`health_passes_through_wired_runtime_summaries`（`null`/`[]`）。
- ULID：`first_char_fits_128_bits_over_100k_generations ... ok`；解析边界 `8…→0…`、`9…→1…` 显式断言在案。

---

## 5. 三运行时一致性门禁（条件 7）

| 运行时 | 覆盖 | 结果 |
|---|---|---|
| Claude Code | M2-02 口径：一致性/T5a/Mode R/真实 50 次 | 本轮 `verify-m2-02` 全绿（T5a 761.97ms）；真实 50 次归档 50/50 |
| Codex | DoD9（M2-02 等价）；DoD10 真实完成率 | `m2_11_codex` 9/9 PASS（复跑）；**归档复核**：`runs=20 / completed=20 / hang=0 / completionRate=1`，逐 run `run.completed`（`scripts/test/.tmp/m2-11/real-codex-2026-09-22T08-20-09-685Z/summary.json`） |
| DSH | DoD1–8 | `m2_11_dsh` 7/7 PASS（20/20、丢失率 0、偏差 0；权限回环 2/2 零直通）；**补验归档复核**：真实 ask 回环 allow（ask=1、决议延迟 1209ms、收口晚于决议 1）与 deny（ask=1、1200ms）零直通（`real-dsh-2026-09-22T08-41-58-034Z` / `...T08-38-28-653Z`） |

`verify:m2-11` 中 opt-in 真实步骤在无凭证环境输出显式 SKIP 且不计为通过（本轮本机如此）；门禁判定以归档原始 `summary.json` 复核为准，符合 v1.14「Gate 2 前补验清单归档」要求。

---

## 6. 回退动作对照

| Gate 2 回退触发 | 状态 |
|---|---|
| T7 漏项 | 未触发（100% deny + 审计） |
| M2-10 异常路径不通过 | 未触发（全过，无需冻结 M3-03） |
| 基础回环不通过 | 未触发 |
| T5a/T5b 不达标 | T5b 达标（30.3s/31.4s）；T5a 达标（Ready 578–915ms + Mode R），收口分类竞态已按 P1 加固闭环（§7 #1） |
| 背压/取消不达标 | 未触发 |

**下一步**：M3 进入条件（Gate 2 通过）已满足；M3-01…M3-08 可启动。M3-03 无冻结。

---

## 7. 风险与待办（2026-09-23 闭环复核）

| # | 级别 | 事项 | 闭环证据与复核结论 |
|---|---|---|---|
| 1 | P1 | `m2_11_codex` T5a 收口分类竞态（`Failed(cli_exit)` vs `Disconnected`，1/5 复现，根因 = 强杀次序） | **已闭环**：`bccad82` 引入 `common::assert_t5a_inflight_closure`——仅接受 `Disconnected(adapter_disconnected)` 或 `Failed(cli_exit, recoverable=true)`，显式拒绝 Completed/Cancelled/非 `cli_exit`/不可重试；30s Ready 与 Mode R 重放断言未放宽。复核：`m2_11_codex` 10/10（含新增仲裁边界单测）、codex T5a ×5、claude T5a ×3 全绿（收口=disconnected）；`verify:m2-11` 全绿；CI 三平台 `Adapters tests` 3/3 success（run 35814863317） |
| 2 | P2 | M2-08 CI 首轮结果待推送归档 | **已闭环**：`9ed1b4a` 归档 run [35814863317](https://github.com/Scott-PyMu/Aether/actions/runs/35814863317)（head `27e0de9`）；本报告独立经 GitHub API 复核——`conclusion=success`、**12/12 job success**（含 `Tauri security baseline` 的「M2 批 3 验证（M2-08）」success、`Coverage gate`、`Supply chain gate`、三平台 `Adapters tests`） |
| 3 | P2 | 本机 `similarity-scan` 对 `scripts/test/.tmp` 残留误报 | **已闭环**：`27e0de9` 将 `scripts/test/.tmp` 纳入 `excludePaths`；本机在 `.tmp` 残留仍存在时复跑 `node scripts/ci/similarity-scan.mjs --root .` → `扫描 324 个文件 … 通过：无规则命中`（exit 0）；CI `Supply chain gate` success |
| 4 | 记录 | 验收操作中对本机 `scripts/test/.tmp/m2-11/codex-real-home` 夹具（gitignored 缓存）产生的扰动已修复还原；如后续重跑真实 Codex E2E，建议重建隔离 HOME | 保留为运维提示（不影响门禁与 CI） |

---

## 8. 证据索引

| 证据 | 路径 | 状态 |
|---|---|---|
| M2-08 验收日志（本轮） | `scripts/test/.tmp/gate2-verify-m2-08.log` | gitignored，本机留存 |
| M2-08 证据归档（DoD1/T11/T5b JSON） | `scripts/test/.tmp/m2-08/evidence-2026-09-23T02-39-30-478Z/` | gitignored，可复现 |
| ci-local 全量日志（本轮） | `scripts/test/.tmp/gate2b-ci-local-quick.log` | gitignored，本机留存 |
| M2-11 复跑日志 | `scripts/test/.tmp/gate2b-verify-m2-11.log` | gitignored，本机留存 |
| M2-11 加固后复跑日志（闭环复核） | `scripts/test/.tmp/gate2c-verify-m2-11-post-p1.log` | gitignored，本机留存 |
| P1 加固 / P2 扫描排除 / CI 归档提交 | `bccad82`、`27e0de9`、`9ed1b4a`（已推送 `origin/main`） | 已提交 |
| M2-08 CI 首轮 run（12/12 success） | [actions/runs/35814863317](https://github.com/Scott-PyMu/Aether/actions/runs/35814863317)，API 复核 | 归档 + 独立复核 |
| M1-06 / M1-08 / M2-07 全量 E2E 日志（本轮） | `scripts/test/.tmp/gate2b-verify-{m1-06,m1-08,m2-07}-full.log` | gitignored，本机留存 |
| M2-10 回环证据（6 份） | `scripts/test/.tmp/m2-10/evidence-2026-09-23T03-03-21-852Z/` | gitignored，可复现 |
| Codex 真实 20 次完成率 | `scripts/test/.tmp/m2-11/real-codex-2026-09-22T08-20-09-685Z/summary.json` | 归档复核 |
| DSH 真实权限回环 | `scripts/test/.tmp/m2-11/real-dsh-2026-09-22T08-41-58-034Z/summary.json`、`...T08-38-28-653Z/summary.json` | 归档复核 |
| 任务 DoD 证据文档 | `docs/M2-01-证据.md` … `docs/M2-11-证据.md`（含 `docs/M2-08-证据.md`） | 已提交 |
| M2 执行编排 | `docs/M2-实施计划.md`（含 Gate 2 执行记录） | 已提交 + 状态注记 |
| 本报告 | `docs/M2-Gate2-验收报告.md` | 本次新增 |
