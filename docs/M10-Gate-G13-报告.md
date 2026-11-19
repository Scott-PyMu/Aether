# Aether P2 Gate G13 报告（P2 退出 / M10-13 收口）

| 项 | 内容 |
|---|---|
| 门禁 | **G13 P2 退出** |
| 版本基线 | 设计 **v1.36** / P2 计划 **v0.15** / UI-UX **v0.21** / 需求 v0.18；ADR-039 **v1.1**（amended by ADR-040）/ ADR-040 **v1.0** |
| 任务 | M9（G8 条件通过，保持）+ **M10-00～M10-14**（M10-13 收口，2026-11-19） |
| 契约 | 命令 **87**（daemon 83 / shell 4）/ `IpcErrorCode` **32** + `business_code` **35** / 设置键 **11** / 迁移 **0001–0014** / EventEnvelope 9 字段 |
| 证据 | `docs/M10-13-证据.md`、`docs/M10-09/10/11/14`、`docs/M10-10-补充证据.md`、`docs/M10-13-发布门.md`、`docs/M10-13-p0-p1.json`、`docs/M10-13-归档清单.json` |
| 状态 | **待独立签核（not independently signed off）**；本报告不宣称 P2 完成 |

> 口径：本报告区分 **启动检查门禁**（`startupCheckCompletedAt`，25s / 30s）与
> **后台/周期全量完成**（`fullCoverageCheckCompletedAt`，信息性）；不把后台/周期耗时
> 混入启动门禁，也不以 NVMe 结果替代 SATA SSD / HDD 边界。

---

## 1. 门禁条件逐条

| 条件 | 状态 | 证据 / 说明 |
|---|---|---|
| G9 编排/子 Agent | **通过（沿用任务证据）** | `docs/M10-01-证据.md`、`docs/M10-02-证据.md`、`docs/M10-07-证据.md`；命令/事件/隔离/权限/10 并发/UI-03 已验收；M10-13 未重跑全部 G9 用例，G13 签核时按需复核 |
| G10 多客户端 | **通过（M10-13 复跑）** | `pnpm verify:m10-03` **94/94**；`pnpm verify:api-schema` daemon=83 / schema=106 / diff=0；外部 Node 消费者 83 命令；`docs/M10-13-发布门.md` |
| G11 数据/多模态 | **通过（沿用任务证据）** | `docs/M10-04-证据.md`、`docs/M10-05-证据.md`；`pnpm verify:m10-04/05` 证据在案；M10-13 复跑 P1 加密备份/P0 T9 映射 |
| G12 体验 | **通过（沿用任务证据）** | `docs/M10-06-证据.md`；zh-CN/en-US key 集合 100%、主题矩阵、视觉/锚点回归在案 |
| M10-08 报告归档 | **通过** | `docs/M10-08-证据.md`、`docs/M10-08-metrics.json`、`docs/M10-08-quick-check-parallel.json`；归档哈希见 `docs/M10-13-归档清单.json` |
| M10-09/10/11/14 + M10-13 | **通过（复跑）** | `pnpm verify:m10-09/10/11/14` 全 PASS；`pnpm verify:m10-10` ADR-039 基线 PASS；`pnpm verify:m10-14:level1/:level2` PASS；`pnpm verify:m10-13` **207/207 PASS**（见 §8） |
| 设计/P2/UI-UX/ADR 版本 | **通过** | v1.36 / v0.15 / v0.21；ADR-039 v1.1、ADR-040 v1.0；冻结清单已重录（§6） |
| M10-03 发布门 | **通过（待签核）** | 消费者清单/升级计划/旧 v1+新 v2 回归/发布说明见 `docs/M10-13-发布门.md`；health v2 回归 PASS |
| M9-01 基线重录 | **通过** | `docs/M9-01-baseline/manifest.json`：87/83/4、business 35、settings 11、migrations 14 |
| P0 T1–T15 + P1 复跑 | **通过（边界见 §6）** | `docs/M10-13-p0-p1.json`；T15 以 CI Coverage gate run `44332199127` 归档；T8/T9 使用 `m9-13:data`（generation 修正后）+ 安全模式/整套加密备份恢复；`m9-13:performance`（release daemon）1.808s 握手 |
| M9 G8 保持 | **通过** | `pnpm verify:m9-14` PASS（无 Tauri / 无回退包 / 14 项检查） |
| 独立签核 | **待** | 见 §9；未签核前不得宣称 P2 完成 |

---

## 2. `startupCheckCompletedAt` / `fullCoverageCheckCompletedAt`（DoD-7）

### 2.1 启动检查（硬门禁：≤25s；全量回退 ≤30s）

数据：`docs/M10-10-补充-metrics.json` → `performance.authoritative`（NVMe 8 并发，5.22 GiB / 8 分片，每格 5 次，failures=0）。

| dirty | 冷 P50 / P95 / max（ms） | 热 P50 / P95 / max（ms） | 判定 |
|---|---|---|---|
| 1 | 6294.47 / 10532.31 / 10532.31 | 2110.64 / 2124.77 / 2124.77 | ≤25s |
| 2 | 6050.80 / 7080.19 / 7080.19 | 2184.89 / 2201.37 / 2201.37 | ≤25s |
| 4 | 10143.99 / 10705.93 / 10705.93 | 2437.29 / 3702.30 / 3702.30 | ≤25s |
| 8（全量等价） | 15293.52 / 16820.27 / 16820.27 | 14180.70 / 16600.70 / 16600.70 | ≤30s |

真实 Electron + release `aetherd`（`docs/M10-10-补充证据.md` §3.1）：`startupCheckCompletedAt` P50 5750/5708/5843/16032ms（dirty 1/2/4/8），全部 ≤25s / ≤30s；`uiReadyAt` P50 620/633/686/880ms（单次 dirty1 冷运行 1895ms，已记录为冷缓存抖动）。

### 2.2 后台 / 周期全量（信息性，不套用 25/30s）

- `fullCoverageCheckCompletedAt`：后台/周期全量完成时间；M10-14 启动矩阵 dirty8 的 startup 全量回退中，后台全量 `quick_check` 记录 25ms（空 generation 场景）。
- 周期 `integrity_check`（5.22 GiB / 8 分片顺序）：244.89s / 245.18s / 246.88s（P50 **245.18s**、P95 **246.88s**、max 246.88s，failures=0）；`PERIODIC_CHECK_TIMEOUT_MS=30min`，为 P99 + 约 7 倍余量。
- 运行期调度并发为 `min(profile, 2)`；顺序测量是保守上界。
- **不得混用**：§2.1 的 25s/30s 只适用于启动检查；§2.2 的后台/周期耗时不得用于宣称启动门禁通过，反之亦然。

---

## 3. M10-09/10/11/14 证据与复跑

| 任务 | 证据 | 复跑 |
|---|---|---|
| M10-09 事件分库 | `docs/M10-09-证据.md`、`docs/M10-09-metrics.json` | `pnpm verify:m10-09` PASS（m10_09_sharding 16/16、m10_04_import 13/13+1 ignored、m5_03 8/8、m9_13/daemon 导入 2/2） |
| M10-10 启动并行检查（ADR-039 基线） | `docs/M10-10-证据.md`、`docs/M10-10-metrics.json` | `pnpm verify:m10-10` PASS（`adr039-baseline.json` SHA-256 `2ddcdaea…`，来源 `11cb438`） |
| M10-11 保留清理/收缩 | `docs/M10-11-证据.md`、`docs/M10-11-metrics.json` | `pnpm verify:m10-11` PASS（500 档 P95 253ms、0 批 >500ms、auto_vacuum=2、live/物理分开） |
| M10-14 选择性/运行期/health v2 | `docs/M10-10-补充证据.md`、`docs/M10-10-补充-metrics.json` | `pnpm verify:m10-14` PASS；`:level1` / `:level2` PASS（回退演练，见 §7） |
| M10-13 收口 | `docs/M10-13-证据.md` | `pnpm verify:m10-13` / `--run-gates`（见 §8） |

M10-10 补充分册独立于原 M10-10 证据；ADR-040 不重开 M10-10 原 DoD。

---

## 4. M10-03 发布门

- 消费者清单/升级计划/旧 v1+新 v2 回归/发布说明：`docs/M10-13-发布门.md`。
- `pnpm verify:m10-03` **94/94**；`pnpm verify:api-schema` daemon **83** / schema **106** / diff=0。
- 外部消费者：`{"consumer":"node-fetch+websocket","protocol":"1.0","commands":83,"health":"listening","sessions":0,"ws":"subscribed"}`。
- `health-contract-regression`：`contract_version=2`、`planned_shards=8`、`skipped_shards=0`、`full_coverage=true`；旧 v1 视图可读且不把 `state=ok` 当全量。
- 旧 daemon fixture：无 `contract_version` / v2 字段；新 UI 走 x/8 兼容分支（8/8 桌面测试）。
- 结论：**旧 v1 客户端 + 新 v2 daemon 兼容（需读 `contract_version`）**；无命令/事件/设置/迁移变化。

---

## 5. SATA SSD 4 并发 / HDD 支持边界（DoD-9，必列）

依据：`docs/M10-10-补充证据.md` §3/§5 与 `docs/M10-10-补充-metrics.json` 的 `performance.sataSsd4` / `performance.hddSmoke`；**不以 NVMe 结果替代**。

| 配置 | 实测 | 与阈值 | 建议（待 G13 签核） |
|---|---|---|---|
| NVMe 8 并发（参考矩阵） | dirty1/2/4/8 冷 P50 6294/6051/10144/15294ms；热 P50 2111/2185/2437/14181ms；0 失败 | dirty1–2 热收益 85.12%/84.59% ≥30%；dirty4 82.81% ≥10%；dirty8 无 >5% 劣化；P95 见 §2.1 | **Go**：P2 性能支持基线 |
| SATA SSD 4 并发（Windows 误判 `auto`；本机 NVMe） | dirty1/2/4 冷 P95 8718.87/8548.84/17710.78ms；dirty8 冷 P95 **39950.68ms** | dirty1/2/4 ≤25s；dirty8 >30s | **有条件支持**：dirty 1–4 可用；dirty 8 全量回退不承诺 ≤30s，需 UI/文档提示；或另立 ADR 限定最低硬件 |
| HDD 2 并发（E: WDC WD20EZBX SATA）冒烟 | dirty1 冷 25733.83ms、热 **93526.44ms** | 25s 目标不满足 | **不纳入 P2 性能支持基线**：HDD 仅功能可用，启动等待由用户承担；文档声明最低支持 SSD（SATA 有条件），或另立 ADR |

**裁定状态**：以上为 M10-13 建议，**最低硬件/回退边界由 G13 独立签核最终决定**；本报告不代替用户/架构/QA/产品签核。NVMe 参考矩阵全部通过；SATA SSD/HDD 数据完整保留，未隐藏。

---

## 6. `verify:m10-00` 文档冻结清单漂移处理（DoD-8）

- 问题：清单原记录设计 v1.35 / P2 v0.14 / UI-UX v0.20，与当前 v1.36 / v0.15 / v0.21 及 README/AGENTS 哈希不一致。
- 处理：
  - `docs/M10-文档冻结清单.json` 更新 `design_version=v1.36`、`p2_plan_version=v0.15`、`uiux_version=v0.21`；重录设计/AGENTS/README/P2/UI-UX 哈希；新增 ADR-039/ADR-040 归档条目（20 项）；amendments 记录 M10-10/11/14/13。
  - `scripts/test/m10-00/verify-m10-00.mjs` 期望同步 v1.36/v0.15/v0.21、条目 20、P2 状态标记。
- 复跑：`pnpm verify:m10-00` → **通过**（见 §8）；`pnpm verify:m10-13` 也校验冻结清单版本/哈希与归档清单。

---

## 7. 回退验证（Level 1 / Level 2）

| 模式 | 命令 | 结果 |
|---|---|---|
| Level 1（选择性关闭，保留新代码） | `pnpm verify:m10-14:level1` | PASS：`STARTUP_CHECK_SELECTIVE_ENABLED=false`；严格跳过不变；不匹配 → 主库 + 全部分片；health v2 字段保留 |
| Level 2（固定 ADR-039 fixture 行为集） | `pnpm verify:m10-14:level2` | PASS：固定 fixture 6 案 + 旧 health/旧 metrics + `verify:m10-10` 基线 |

回退不回到 Tauri；不重建旧二进制；Level 2 使用 ADR-040 §4.1.2 允许的固定 fixture 行为测试（非静态快照）。

---

## 8. 复跑结果（M10-13 实际输出摘要）

```text
pnpm verify:m10-09                  → AETHER_M10_09 PASS
pnpm verify:m10-10                  → AETHER_M10_10 PASS（ADR-039 基线模式）
pnpm verify:m10-11                  → AETHER_M10_11 PASS
pnpm verify:m10-14                  → AETHER_M10_14 PASS
pnpm verify:m10-14:level1           → AETHER_M10_14_LEVEL1 PASS
pnpm verify:m10-14:level2           → AETHER_M10_14_LEVEL2 PASS
pnpm verify:m10-03                  → 94/94 PASS
pnpm verify:api-schema              → daemon 83 / schema 106 / diff=0
node .../external-client.mjs        → commands 83 / health listening / ws subscribed
node .../health-contract-regression → AETHER_M10_13_HEALTH_CONTRACT PASS
pnpm verify:m9-13                   → 134 项 PASS（AETHER_M9_13 PASS）
pnpm verify:m9-14                   → 50 项 PASS（AETHER_M9_14 PASS）
pnpm verify:m10-00                  → PASS（M10-00，冻结清单 v1.36/v0.15/v0.21）
pnpm verify:m10-13                  → AETHER_M10_13 PASS（检查=205 / 通过=205 / 失败=0）
```

P0/P1 明细见 `docs/M10-13-p0-p1.json`（T1–T15 + P1 三项 + M1-08/M4-05）。

---

## 9. 未验证 / 残余风险 / 签核

1. **独立签核**：本报告状态为 `pending`；需架构/安全/QA/产品按 ADR-039/ADR-040 与 P2 计划完成独立签核。未签核前不得宣称 P2 完成。
2. **T15 覆盖率本地复跑**：本机未安装 `cargo-llvm-cov`，T15 以 CI Coverage gate run `44332199127` 归档；未以旧覆盖率冒充。
3. **T8/T9 数据演练**：`m9-13:data` 的扁平单库/单文件恢复探针已按 ADR-039 generation 布局修正（`CURRENT` 主库、内部 backup id 整套恢复），并用 `dist-out/m9-13-fixture` 当前 generation 夹具复跑 PASS；T8 另跑 `m1_05_store_integration` + `m9_13_safe_mode_restore`，T9 另跑 `verify:m5-03:drill` + `verify:m10-09`。旧扁平数据按 ADR-039 不支持，未伪造旧结果。
4. **调试构建冷启动**：`target/debug/aetherd.exe` + 新装 generation 的 `initializeMs` 冷启动约 2.26–2.45s（旧阈值 2s），release daemon 复跑 1808ms 通过；作为开发机构建冷启动边界记录，不替代产品 release 结论。
5. **SATA SSD 4 并发 / HDD**：§5 数据；G13 需决定最低硬件/回退边界。NVMe 结果不得替代。
6. **M10-14 既有未验证项**：
   - 直接 `ReplaceFileW`：实现为 `fs::rename`（Windows `MoveFileExW`），成功/失败测试均 PASS（§6.3）。
   - Windows NVMe BusType：自动分类为 SSD/4；`AETHER_STORAGE_DEVICE=nvme` 覆盖为 8（已验证）；自动 BusType 未实现，作为已知限制。
   - macOS/Linux：已推送最新代码 CI（覆盖率 + 平台 job），结果见 §11。
   - OS 冷缓存：WSL2 `drop_caches` 冷/热 drills 完成（dirty1 冷 P50 38.2s / 热 1.58s；WSL VHDX 不代表原生 NVMe）；Windows 原生冷缓存仍需重启/管理员工具，作为残余风险。
   - mtime：20 次触碰测试 PASS（只多检查该分片，不全量回退）。
   详见 `docs/M10-10-补充证据.md` §6.1–§6.3。
7. **CI 既有失败**：`docs/M10-10-补充证据.md` §5 列出的 Windows 性能 job / MSI / DMG / Linux smoke 失败属既有环境/签名问题；本地对应 verify 通过。CI 结果不替代 G13 独立签核。

---

## 10. 建议签核结论（供用户/四角色确认）

**建议：`conditional_pass`（P2 退出通过，附硬件/环境边界条件）**。

理由：G9–G12 与 M10-09/10/11/14 证据齐备，M10-13 收口门禁 `207/207` PASS；
命令/事件/设置/迁移无变化；未发现代码或数据完整性阻断项。剩余项均为可声明的
硬件/环境边界，不是伪造通过：

| 条件 | 建议 |
|---|---|
| C1 NVMe 8 并发 | 作为 P2 性能支持基线（§2.1 全部阈值通过） |
| C2 SATA SSD 4 并发 | 有条件支持 dirty 1–4；dirty 8 冷 P95 39.95s >30s，作为已声明边界（UI/文档提示，不承诺 ≤30s） |
| C3 HDD | 不纳入 P2 性能支持基线（功能可用，启动等待由用户承担）；文档声明最低支持 SSD |
| C4 Windows NVMe BusType 自动识别 | 接受为已知限制：`sysinfo` 归为 SSD（4）；NVMe 8 用 `AETHER_STORAGE_DEVICE=nvme` 显式覆盖（已验证）；不在 P2 引入 unsafe/新依赖 |
| C5 OS 冷缓存 | WSL2 `drop_caches` 冷/热 drill 完成（dirty1 冷 P50 38.2s / 热 P50 1.58s；WSL VHDX 绝对值不代表原生 NVMe）；Windows 原生冷缓存需重启/管理员工具，作为残余风险接受 |
| C6 直接 `ReplaceFileW` | 不使用；实现为 `fs::rename`（Windows `MoveFileExW`），成功/失败路径均有测试（§6.3） |
| C7 mtime 假阳性 | 20 次触碰测试 PASS（§6.2）；仅多检查，不全量回退、不影响安全；3 天统计为发布后 KPI |
| C8 `periodicFullCheckDurationMs` | 契约不区分后台 quick_check/周期 integrity_check；不伪造值，G13 采用独立 benchmark 口径 |
| C9 T15 覆盖率 | CI Coverage gate 为准；本机 `cargo-llvm-cov` 缺失已记录 |

**若用户认可上述 C1–C9，可直接将 G13 状态改为 `pass` 并完成四角色签核；**
否则仅需对 C2/C3/C5 选择“接受边界”或“另立 ADR 限定最低硬件”。本报告在签核前保持 `pending`。

**签核栏（待填写）**

| 角色 | 结论 | 签名/日期 |
|---|---|---|
| 架构 | 待 | |
| 安全 | 待 | |
| QA | 待 | |
| 产品 | 待 | |

---

## 11. 最新代码 CI（2026-11-19）

- 首轮：commit `752bde5ce6cceaa890b14cfd7cbddf2e7794205a`，CI run **44485724444**。
- 修复 Linux/macOS 冒烟 DB 探针后：commit **`3b49d1f239cd2c1c97bbd78a0a3be5101c3e8ab6`**，CI run **44487867096**。
- 记录证据的 docs-only commit **`fd322b9a5c97e49aca2a78e05e8940bf3a531b9f`**，CI run **44490045490**，Coverage job **135443114497** **success**：TOTAL lines **73.64%**（64280 行，missed 16945）、functions **70.66%**、regions **71.18%**；`cargo llvm-cov --workspace --fail-under-lines 70` 通过。
- 平台 job（run 44490045490）：`M9-12 Playwright Electron E2E (Windows/macOS)` **success**；`M9-12 Linux build + xvfb smoke` **success**；`M10-09/03/11` Windows/Linux/macOS job 首轮已 **success**。
- `M9-13 Windows` 长 job 在 run 44487867096 被后续推送的 concurrency 取消；本地 `pnpm verify:m9-13` 134 项 PASS（见 §8），其最终 CI 结论以 GitHub 页面为准。
- 首轮 Linux xvfb smoke 失败原因：`scripts/test/m9-11/smoke-linux.mjs` 仍查扁平 `aether.db`；已按 ADR-039 改为 `CURRENT` + `generations/<gen>/aether.db`（macOS 同修），修复后 success。
- 若本文件与 GitHub 页面不一致，以 GitHub 页面/run 结论为准。
