# M1-06 macOS 数据目录检测降级说明（iCloud 路径前缀 + 手动确认）

| 项 | 内容 |
|---|---|
| 依据 | 设计文档 A4「验证（实现级检测，评审修订 #2）」；实施计划 M1-06 DoD2 与风险条款；评审 #9；Gate 1 通过条件 |
| 决策性质 | **任务风险条款内的降级实现**，不构成新设计决策（A4 的「命中即拒绝、无覆盖开关」语义不变） |
| 适用范围 | macOS 数据目录检测（`crates/aether-tauri/src/startup/detect.rs`）与拒绝启动界面（`apps/desktop/src/StartupGate.tsx`） |
| 编制日期 | 2026-09-17 |

## 1. 结论

- macOS 两类样本中：
  - **② File Provider（`~/Library/CloudStorage/*`）**：按 A4 原口径实现（File Provider 挂载点本身即路径形态）；
  - **① iCloud ubiquitous 标记**：按 M1-06 风险条款**降级**为 `~/Library/Mobile Documents`（含 `com~apple~CloudDocs`）路径前缀匹配 **+ 用户手动确认**，不绑定 `NSURLIsUbiquitousItemKey`（CoreFoundation）。
- 精度限制在 UI（启动门 `startup-precision-note`）与检测报告（`DetectionReport.note`）中明示，文案常量为 `MAC_PRECISION_NOTE`：

  > macOS 同步盘检测精度受限（iCloud 采用路径前缀近似，未绑定 NSURLIsUbiquitousItemKey）；请确认目录不在 iCloud/CloudStorage 下。

- 不提供任何「仍要在此目录运行」的覆盖开关（评审 #9 不变）。

## 2. 降级理由（成本评估）

- 本仓库自动化环境为 Windows/Linux，无 macOS 宿主：CoreFoundation/objc2 绑定代码无法在本地编译验证，引入不可验证 FFI 的风险高于收益；风险条款明确允许降级。
- `resolve_existing_prefix()` 先解析真实路径再比较前缀：iCloud「桌面与文稿」同步目录在 Finder 中表现为 `~/Desktop` / `~/Documents`，实际是指向 `~/Library/Mobile Documents/com~apple~CloudDocs/...` 的符号链接，canonicalize 后同样命中，覆盖绝大多数 iCloud 场景。
- 「手动确认」由迁移流满足：迁移目标须由用户手动输入/选择（`startup-pick` / `startup-target`），并由 `startup_migrate` 再做一次 A4 复核。

## 3. 覆盖范围

| 样本 | 路径 | 判定 | 精度 |
|---|---|---|---|
| iCloud 容器 | `~/Library/Mobile Documents/**` | 命中拒绝 | 路径前缀（降级） |
| iCloud Drive 根 | `~/Library/Mobile Documents/com~apple~CloudDocs/**` | 命中拒绝 | 路径前缀（降级） |
| Dropbox | `~/Library/CloudStorage/Dropbox/**` | 命中拒绝 | 路径判定 |
| Google Drive | `~/Library/CloudStorage/GoogleDrive/**` | 命中拒绝 | 路径判定 |
| OneDrive | `~/Library/CloudStorage/OneDrive/**` | 命中拒绝 | 路径判定 |
| 其他 File Provider | `~/Library/CloudStorage/<任意 Provider>/**` | 命中拒绝 | 路径判定 |
| 本地对照 | 如 `~/AetherTest/local` | 放行 | — |

## 4. 漏报边界（已知限制）

1. 位于非 `~/Library/Mobile Documents` 路径下但仍带 ubiquitous 扩展属性的项目（例如被移动后保留同步标记的目录）——路径前缀无法识别，**属已知漏报**。
2. 非 File Provider 形态的第三方同步目录（如旧式 `~/Dropbox` 自建文件夹 + 选择性同步）不在 A4 macOS 两类检查范围内。
3. macOS 网络盘（SMB/NFS 挂载）不在 A4 macOS 检查清单内（A4 仅对 Windows 列网络盘粗筛）；如需覆盖应走设计变更/ADR。
4. 目标目录不存在且最深已存在祖先不可解析（极端权限/挂载异常）时，前缀比较退化为原始路径比较，可能漏报。
5. 用户手动指定迁移目标时，目标会再次通过同一检测（`startup_migrate` 内复核），但同样受本说明的漏报边界限制。

## 5. UI 明示与命令层

- 启动门：`apps/desktop/src/StartupGate.tsx` 渲染 `data-testid="startup-precision-note"`（阻塞态且 `detection.note` 非空；macOS 上下文始终非空）。
- 命令层：`startup_get` / `startup_migrate` 返回的 `detection.note` 携带同文案；`startup_migrate` 在复制完成后对目标目录重新检测。
- 拒绝启动无覆盖开关（评审 #9；`verify-m1-06` 静态断言）。

## 6. 验证与证据

| 层 | 方法与命令 | 位置 |
|---|---|---|
| 路径逻辑（任意宿主） | `cargo test -p aether-tauri --test m1_06_detection -- --nocapture`（注入 home；断言命中 + 降级文案） | `crates/aether-tauri/tests/m1_06_detection.rs::macos_samples_are_all_detected` |
| 命令层（状态码 + 提示文本） | `cargo test -p aether-tauri --test m1_06_startup_ipc`（`macos_blocked_snapshot_exposes_precision_note`） | `crates/aether-tauri/tests/m1_06_startup_ipc.rs` |
| 前端 UI | `pnpm --filter @aether/desktop test`（`startup-precision-note` 渲染断言） | `apps/desktop/src/StartupGate.test.tsx` |
| macOS 原生（DoD2） | CI `data-dir-guard-macos`（macos-14）→ `node scripts/test/m1-06/verify-m1-06-macos.mjs` | `.github/workflows/ci.yml` |
| 样本构造 | `bash scripts/test/macos_sync_samples.sh --json`（生成）；`--clean`（仅删空目录） | `scripts/test/macos_sync_samples.sh` |
| 证据归档 | 测试输出 + 样本清单 + 环境信息 → `scripts/test/.tmp/m1-06-macos/<stamp>/`；CI artifact `m1-06-macos-evidence` | `scripts/test/m1-06/verify-m1-06-macos.mjs` |

## 7. 验证状态（M1-06 DoD2 完成说明）

- 本机（Windows）注入样本、命令层提示与前端渲染断言：**通过**（`pnpm verify:m1-06 --skip-e2e` 全绿，含降级文案静态与运行时断言）。
- **macOS 原生样本（macos-14 CI，job `macOS data dir samples (M1-06)`）：自首次修复（run 35173358821）起连续 8 次跑绿，关键节点**：
  - run [35173358821](https://github.com/Scott-PyMu/Aether/actions/runs/35173358821)（首次修复非 Windows 编译后）
  - run [35174078102](https://github.com/Scott-PyMu/Aether/actions/runs/35174078102)
  - run [35174804590](https://github.com/Scott-PyMu/Aether/actions/runs/35174804590)
  - run [35175425557](https://github.com/Scott-PyMu/Aether/actions/runs/35175425557)
  - **run [35179304882](https://github.com/Scott-PyMu/Aether/actions/runs/35179304882)**（commit `bfafc5b`，**全 workflow 9/9 job 全绿**：macOS job 步骤全绿，Windows `data-dir-guard` 同步全绿）
  - run [35181029797](https://github.com/Scott-PyMu/Aether/actions/runs/35181029797)（commit `6f0df72`，E2E 慢机超时修复后）：两个 M1-06 job 均绿；整体 run 仅 `Coverage gate`（`cargo llvm-cov --workspace --exclude aether-tauri`）偶发失败——aether-tauri 被排除，与本任务改动无关，属并行任务（M1-04 基准）的在办项
  - 证据 artifact：`m1-06-macos-evidence`（每次上传；含 `samples.json`、`cargo-test-m1_06-detection.txt`、`evidence.json`；`bfafc5b` run 摘要 sha256 `9891603da83a7470cb268678a61f53c3c825fed48378ff78f55ba9a8db6f4e7f`）
- 本地证据归档（gitignore）：`scripts/test/.tmp/m1-06-ci/<run_id>/{run,jobs,check-runs,artifacts}.json`
- CI 连带修复（由 mac/Windows 样本暴露并已回归）：非 Windows 目标编译错误（`REGISTRY_ACCOUNTS_PATH` cfg 门控）、映射网络盘形态不一致漏判、CI `%TEMP%` 8.3 短名与 D9 拒绝样本的测试口径。
- 完成口径：**macOS DoD2 已由 macos-14 CI 验证通过；iCloud 标记 API 按风险条款降级为路径前缀 + 手动确认，UI 已明示。**
- Windows 侧（DoD1/DoD3/DoD4/DoD5）由同 workflow 的 `data-dir-guard` job 守护；本文件记录 macOS DoD2 的降级口径与证据。

## 8. 变更记录

| 日期 | 变更 |
|---|---|
| 2026-09-17 | 初版：降级实现、覆盖/漏报边界、UI 文案、CI 验证接线与证据索引 |
| 2026-09-17 | 追加：CI 慢机冷启动导致 E2E 等待窗口不足（观测到 ~90s 未就绪），E2E 超时上调（240–300s）并消除「恰好超时后到达」竞态；探针超时 180s→600s |
