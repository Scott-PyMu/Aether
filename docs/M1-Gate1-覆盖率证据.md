# M1 批次出口覆盖率证据（Gate 1）

| 项 | 内容 |
|---|---|
| 目的 | Gate 1 覆盖率证据：以批次出口 `pnpm ci:local` 口径判定（不按单任务卡） |
| 执行日期 | 2026-09-18 |
| 代码版本 | `a0af5c6`（`a0af5c673135f68f77ae4c762d3736fa2c1f3aed`，工作区干净） |
| 执行命令 | `pnpm ci:local`（全量，未使用 `--quick`；未改脚本、未改阈值） |
| 结果 | **exit=0，22/22 检查全部 PASS**（含覆盖率门禁正/反向用例） |
| 判定 | Rust 行覆盖 **88.75%** ≥ 70%；TS 各包行覆盖 88.6%–100% ≥ 70% → **达标** |

执行环境：Windows x64；rustc 1.98.1（host `x86_64-pc-windows-msvc`）；cargo-llvm-cov 0.9.1；
Node v24.14.1；pnpm 10.34.5；vitest 2.1.9（v8 provider）。

---

## 1. `pnpm ci:local` 结果

```
===== ci-local =====
PASS  cargo fmt --check
PASS  cargo clippy --workspace -- -D warnings
PASS  cargo check --workspace
PASS  cargo test --workspace（排除壳层）
PASS  pnpm version:check
PASS  pnpm typecheck
PASS  pnpm test
PASS  pnpm build
PASS  相似度扫描
PASS  npm 许可证检查
PASS  cargo deny check licenses/bans/sources
PASS  verify-version-sync
PASS  verify-m1-02
PASS  verify-m1-03
PASS  verify-m1-04（单写队列/group commit/读连接池；含 release 基准）
PASS  verify-m1-05（事件管线：sequencer/先日志后广播/delta/补读/降级与重试口径；含证据归档）
PASS  verify-m1-07（密钥自检/脱敏/降级演练）
PASS  verify-m1-06（目录检测/迁移流/单实例；E2E 需真实 WebView，见 Windows CI）
PASS  verify-m1-08（壳与安全基线单测矩阵/静态检查；E2E 需真实 WebView，见 Windows CI）
PASS  verify-m1-10（监督器：状态机/退避熔断/心跳/台账三条件/终止序列/retry·enable）
PASS  verify-coverage-gate
PASS  smoke-desktop
----- 全部通过 -----
```

全量运行日志（含 ANSI 输出）存档：`scripts/test/.tmp/m1-gate1/ci-local.log`（736,178 字节；
`scripts/test/.tmp/` 为 gitignored 本地目录，可由同命令复现）。

## 2. Rust 行覆盖率（cargo-llvm-cov；workspace 排除壳层 aether-tauri）

命令：`cargo llvm-cov --workspace --exclude aether-tauri --fail-under-lines 70`（门禁阈值 70，**PASS**）

```
TOTAL   13189  1542  88.31%  |  1083  151  86.06%  |  8881  999  88.75%  |  0  0  -
        （Regions / Missed / Cover | Functions | Lines / Missed / Cover | Branches）

Rust 行覆盖率 = 8881 行，999 行未覆盖 = 88.75%（≥70%）
```

> 壳层 `aether-tauri` 排除沿用 M1-01 既有常量级决策（该 crate 无业务逻辑，以 Windows/macOS
> 构建 + 冒烟验证），CI 与本地 `verify-coverage-gate` 口径一致。

行覆盖最低的模块（**观测项，全域达标，无 <70% 产品缺口**）：

| 文件 | 行覆盖 | 说明 |
|---|---|---|
| `aether-adapters/src/bin/aether-adapter-fixture.rs` | 0.00%（154/154 未覆盖） | 测试夹具 bin：仅被集成测试以子进程方式执行，父进程 llvm-cov 默认不回收子进程计数；非产品模块 |
| `aether-control/src/journal.rs` | 72.53%（25/91 未覆盖） | 已 ≥70%（M2-01/M3-06 自然扩展） |
| `aether-adapters/src/process.rs` | 75.00%（61/244 未覆盖） | 已 ≥70%（M2-02/M2-08 真实适配器与故障注入扩展） |
| `aether-control/src/source.rs` | 77.78%（8/36 未覆盖） | 已 ≥70% |
| `aether-adapters/src/supervisor/runtime.rs` | 80.28%（153/776 未覆盖） | M1-10 主体；批次口径达标（M2-01 接线后进一步覆盖） |
| `aether-security/src/encrypted_file.rs` | 80.92%（50/262 未覆盖） | 已 ≥70%（M4-02/M4-03 故障注入扩展） |
| `aether-store/src/store.rs` | 81.25%（45/240 未覆盖） | 已 ≥70% |
| `aether-security/src/keyring_store.rs` | 82.50%（7/40 未覆盖） | 已 ≥70% |

完整逐文件报告：`docs/evidence/m1-gate1/rust-llvm-cov-summary.txt`。

## 3. TS 行覆盖率（vitest --coverage，v8；`AETHER_COVERAGE_LINES=70`）

命令：`pnpm -r --if-present coverage`（真实包，门禁阈值 70，**PASS**）

| 包 | Statements | Branch | Functions | **Lines** | 判定 |
|---|---|---|---|---|---|
| `packages/protocol` | 100% | 100% | 100% | **100%** | PASS |
| `packages/adapter-sdk` | 93.43% | 92.91% | 95.45% | **93.43%** | PASS |
| `packages/adapter-mock` | 88.6% | 68.79% | 90% | **88.6%** | PASS |
| `apps/desktop` | 99.05% | 95.83% | 100% | **99.05%** | PASS |

每包 JSON 汇总：`docs/evidence/m1-gate1/ts-{protocol,adapter-sdk,adapter-mock,desktop}-coverage-summary.json`。

## 4. 门禁有效性（反向夹具，证明阈值真实阻断）

`verify-coverage-gate` 的正/反向用例全部 PASS：

```
PASS  期望=0       实际=0  Rust：真实 workspace 覆盖率 >= 70（cargo llvm-cov）
PASS  期望=nonzero 实际=1  Rust：夹具覆盖率 < 70 时必须阻断（非零）
PASS  期望=0       实际=0  TS：真实包覆盖率 >= 70（vitest --coverage，AETHER_COVERAGE_LINES=70）
PASS  期望=nonzero 实际=1  TS：夹具覆盖率 < 阈值时必须阻断（非零）
```

- Rust 夹具 `uncovered-crate`：行覆盖 0.00% → `--fail-under-lines 70` 阻断（exit 1）；
- TS 夹具 `coverage-fail-ts`：行覆盖 20%（阈值 90%）→ `ERROR: Coverage for lines (20%) does not
  meet global threshold (90%)`、exit 1。

## 5. 结论与承接

- **批次出口覆盖率达标**（Rust 88.75% / TS 每包 ≥88.6%）：本文件与 §2–§4 附件作为 Gate 1
  覆盖率证据；
- **无 <70% 的缺口模块**，无需缺口补齐计划；上表观测项均 ≥70%（唯一 0% 为测试夹具 bin，
  非产品路径），相关进一步提升由 M2-01/M2-02/M2-08/M3-06/M4-02/M4-03 的自然测试扩展承接；
- Gate 1 通过条件的覆盖率口径已同步：《实施计划与验收标准》§1.4「Gate 1（M1 出口）」——
  「覆盖率以批次出口 `pnpm ci:local` 口径 ≥70% 为准」；
- 复现命令：`pnpm ci:local`（全量）；或单跑覆盖率门禁
  `cargo llvm-cov --workspace --exclude aether-tauri --fail-under-lines 70` +
  `AETHER_COVERAGE_LINES=70 pnpm -r --if-present coverage`。

---

## 6. 复验补记（2026-09-20，Gate 1 验收评审复跑，HEAD `0ba5fa6`）

| 项 | 内容 |
|---|---|
| 目的 | Gate 1 验收评审：原始证据（§1–§5）基于 `a0af5c6`，其后 HEAD 前进 3 个提交（含 2 个 M1-10 代码提交 `b8cb8b4`/`2dc6f5d`），在 HEAD 独立复跑批次出口门禁，闭合证据链 |
| 代码版本 | `0ba5fa6`（工作区干净） |
| 执行环境 | Windows x64；rustc 1.98.1（host `x86_64-pc-windows-msvc`）；cargo-llvm-cov 0.9.1；Node v24.14.1；pnpm 10.34.5 |
| 执行命令 | ① `pnpm ci:local`（全量，未使用 `--quick`）；② 定向复跑 `cargo llvm-cov -p aether-adapters --test m1_10_termination --no-report -- --nocapture`；③ 全量复跑 `cargo llvm-cov --workspace --exclude aether-tauri --fail-under-lines 70` |
| 判定 | **门禁在 HEAD 复跑通过**（Rust 行覆盖 88.50% / TS 各包 ≥88.6%）；首跑 1 项失败为不稳定性用例（见 §6.2），复跑即绿，不影响覆盖率达标结论 |

### 6.1 `pnpm ci:local`（全量）结果：21/22 PASS

fmt / clippy / check / test（排除壳层）/ version:check / typecheck / test / build / 相似度扫描 /
npm 许可证 / cargo deny / verify-version-sync / verify-m1-02 / 03 / 04 / 05 / 07 / 06 / 08 / 10 /
smoke-desktop 共 21 步 **PASS**；**FAIL 1 步**：`verify-coverage-gate`（详见 §6.2）。

其中 TS 覆盖率（`AETHER_COVERAGE_LINES=70`，真实包）在本次运行中实测：

| 包 | Lines | 判定 |
|---|---|---|
| `packages/protocol` | 100% | PASS |
| `packages/adapter-sdk` | 93.43% | PASS |
| `packages/adapter-mock` | 88.6% | PASS |
| `apps/desktop` | 99.05% | PASS |

门禁反向夹具 2 例（Rust `uncovered-crate` 阻断、TS `coverage-fail-ts` 阻断）均按预期非零退出。

### 6.2 首跑失败：`m1_10_termination` 在 llvm-cov 插桩环境下偶发 PID 断言失败（不稳定性记录）

- 现象：`cargo llvm-cov --workspace …` 中 `m1_10_termination` 2 个用例失败（exit 101）——
  - `termination_sequence_platform_mechanisms_reap_whole_tree`（`tests/m1_10_termination.rs:90`）：
    `assert_eq!(process.id(), Some(parent))`，left `Some(25572)` ≠ right `Some(25368)`；
  - `force_kill_reaps_tree_via_job_object_or_process_group`（`tests/m1_10_termination.rs:149`）：
    left `Some(30864)` ≠ right `Some(34008)`。
- 同一次 `ci:local` 内**未插桩**的 verify-m1-10（同一测试文件）数分钟前全绿；随后
  ② 定向复跑 4/4 PASS；③ 全量门禁复跑 PASS（见 §6.3）。三次复跑均未再现。
- 定性：**flaky**——仅在高并发/插桩负载下偶发一次，与覆盖率数值无关（同运行中 TS 门禁与
  反向夹具均正常）。已作为 Gate 1 验收记录事项登记：建议 M2-01 前排查修复
  （方向：高负载下 Job Object/`CREATE_SUSPENDED` spawn 路径或 pid-file 读写时序），
  修复前若批次出口门禁再遇此失败，按「定向复跑确认 + 全量重跑」处理并留痕。

### 6.3 全量门禁复跑结果（PASS）

命令：`cargo llvm-cov --workspace --exclude aether-tauri --fail-under-lines 70`（exit 0）

```
TOTAL   13424  1607  88.03%  |  1112  157  85.88%  |  9046  1040  88.50%  |  0  0  -
        （Regions / Missed / Cover | Functions | Lines / Missed / Cover | Branches）

Rust 行覆盖率 = 9046 行，1040 行未覆盖 = 88.50%（≥70%）
```

（较 §2 的 88.75% 略有回落，源于 `a0af5c6 → 0ba5fa6` 间新增的 M1-10 资源告警夹具/集成测试
代码与 `docs/evidence` 归档，仍显著高于门禁阈值。）

### 6.4 复验结论

- **Gate 1 覆盖率条件在 HEAD `0ba5fa6` 依然成立**：Rust 88.50%、TS 每包 ≥88.6%，均 ≥70%；
  §1–§5 原始证据与本节复验共同构成批次出口证据链；
- 记录事项（见 §6.2）移交 M2-01 跟踪，不阻塞 Gate 1 放行。

### 6.5 记录事项关闭（2026-09-20，M2-01 前）+ ci:local 批次出口补项

**① §6.2 flaky 根因查明并修复（非 Job Object/`CREATE_SUSPENDED` 路径）**

- 根因：测试临时目录跨运行同名复用 + 轮询接受旧 pid-file。失败轮测试进程 PID 16544 的目录
  `termination-16544-{0,1,2}` 创建于 2026-09-17 00:03:58，`tree.txt` 最后写入 2026-09-18 23:03:45
  ——Windows 回收测试进程 PID 且 `unique_counter()` 每轮归零，目录被复用；断言 right 值
  25368/34008 为上一轮夹具写入的旧 parent，left 25572/30864 为本轮夹具 PID（现存文件已被
  本轮覆盖为 left 值，闭环）。高负载/插桩只放大了读取窗口，与进程创建/终止机制无关。
- 修复（仅测试支持层，产品代码零改动）：`unique_temp_dir` 目录名追加 UNIX 纳秒 nonce +
  命中复用目录先清空；`spawn_tree_process` spawn 前删除残留 pid-file。
- 验证：定向连跑 5 次 4/4 PASS；llvm-cov 插桩复跑 4/4 PASS；`pnpm ci:local` 全绿。
  详见《M1-10-证据》§8。

**② ci:local 补入 verify-m1-09 具名步骤（闭合批次出口口径）**

- `scripts/test/ci-local.mjs` 新增 `verify-m1-09`（与 CI `wire-protocol-mock` job 等价入口：
  线协议握手/流式/中断/dispose/健壮性注入/2MiB 契约/吞吐基准；需 Bun）。
- 复验：`pnpm ci:local`（全量，含覆盖率门禁与冒烟）**23/23 PASS**，Rust 行覆盖 88.50%；
  日志 `scripts/test/.tmp/m1-gate1/ci-local-m2-01-precheck.log`。
- 结论：§6.2 记录事项已在 M2-01 前关闭，无需 M2-01 承接；批次出口检查项由 22 项增至 23 项
  （§6.1 为历史运行记录，保持原口径）。
