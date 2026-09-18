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
