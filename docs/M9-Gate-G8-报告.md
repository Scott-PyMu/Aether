# M9 Gate G8 报告：默认切换 / Tauri 退役

| 项 | 内容 |
|---|---|
| Gate | **G8 切换**（P2 计划 §2.15；ADR-029 v0.4 §10） |
| 任务 | M9-14 |
| 日期 | 2026-11-03（2026-11-05 条件通过） |
| 计划/设计 | P2 v0.8；设计 v1.30；AGENTS v1.30 |
| 证据 | `docs/M9-14-证据.md`、`docs/M9-14-失败处理演练报告.md`、`docs/M9-14-发布说明.md`、`docs/M9-14-macOS-签名授权方案.md` |
| 验证 | `pnpm verify:m9-14`、`pnpm m9-14:failure-drill`、`pnpm verify:m9-12`、`pnpm verify:m9-13` |
| 结论 | **G8 条件通过（`conditional_pass`）**：Electron 默认、Tauri 已退役、无回退包；**macOS 公开发布仍待人工授权（`macos_release_pending`），不阻塞 M9/M10**；**M10 可启动**（需 M10-00） |

## 1. DoD 对照

| DoD | 结果 | 证据 |
|---|---|---|
| **1. Electron 默认；不生成/不保留 Tauri 回退包；HEAD 不再构建 Tauri；旧内测包手工卸载（文档）** | **通过** | `README.md`、`docs/M9-14-发布说明.md`、`.github/workflows/release-desktop-installers.yml`；`verify:m9-14` 的「package.json 无 Tauri 构建/回退脚本」「CI/发布工作流无 Tauri 构建命令」「cargo metadata 无 aether-tauri」全 PASS |
| **2. 失败处理演练：G4/G5/G6/G7 失败 → 停止发布 → 修复 → 重跑；可保留上一 Electron 候选；不回装 Tauri；报告归档** | **通过** | `docs/M9-14-失败处理演练报告.md`；`scripts/test/.tmp/m9-14/failure-drill.json`（15 步全 PASS；`tauri_fallback=forbidden`） |
| **3. 退役检查：`crates/aether-tauri` 不参与默认 workspace/构建；`cargo tree -p aether-app -p aether-daemon` 无 Tauri；`@tauri-apps` = 0** | **通过** | `crates/aether-tauri` 已删除（127 文件）；`Cargo.lock`/`pnpm-lock.yaml` 清零；`cargo tree` 0 命中；`apps/desktop/src`/`packages/protocol` 0 命中；T4/预算/审计断言迁至 `aether-app`（2/7/8 passed） |
| **4. README/AGENTS/设计/P2 版本与命令更新；release notes 含旧内测包手工卸载与数据目录/Keychain 提示** | **通过** | README v1.29/P2 v0.7；AGENTS v1.29/v0.7；设计 v1.29；P2 v0.7；`docs/M9-14-发布说明.md`（旧内测包/手工卸载/数据目录/Keychain） |
| **5. `pnpm verify:m9-14` 绿；G8 报告签核** | **条件通过（用户裁定 2026-11-05）** | `[verify:m9-14] PASS 检查=51`；本报告（`conditional_pass`；macOS 公开发布待授权，不阻塞 M9/M10） |

## 2. 切换事实

- 默认壳：Electron（main/preload/renderer + `aetherd`）。
- 默认开发/打包/冒烟：`pnpm dev:desktop` / `pnpm package:desktop` / `pnpm smoke:installer`。
- 命令合同：63（daemon 59 / shell 4，UI 61）；`IpcErrorCode` 32 + `business_code`；事件 9 字段；设置键 9；迁移 0001–0009 **不变**。
- Tauri：`crates/aether-tauri` 删除；`bindings.ts`/`bindings.mjs` 删除；无 Tauri 安装器/回退包；CI 不再构建 Tauri。
- 数据：不新增迁移；旧库直接打开；卸载不删数据目录；双开由 `.aetherd.lock` 拒绝（`data_dir_locked`）。

## 3. 验证摘要

```text
$ pnpm verify:m9-14
...
PASS  cargo metadata 无 aether-tauri  aether-core,...,aether-daemon
PASS  cargo tree -p aether-app -p aether-daemon 无 Tauri  0 命中
PASS  apps/desktop/src + packages/protocol 的 @tauri-apps = 0  0 命中
PASS  CI/发布工作流无 Tauri 构建命令  0 命中
PASS  失败处理演练 passed  true
PASS  命令合同 63  63
PASS  daemon 59 / shell 4  59/4
PASS  UI 暴露 61  61
[verify:m9-14] PASS 检查=51
AETHER_M9_14 PASS
```

```text
$ pnpm m9-14:failure-drill
PASS  基线：G4/G5/G6/G7 全绿 → 允许发布
PASS  G4/G5/G6/G7 失败 → 停止发布；修复后重跑通过；无 Tauri 回退
AETHER_M9_14_FAILURE_DRILL PASS
```

```text
$ pnpm verify:m9-12
PASS  Tauri 删除清单已清零（G8 已执行）  0 个文件
[verify:m9-12] PASS 检查=25
AETHER_M9_12 PASS

$ pnpm verify:m9-13
PASS  Tauri 退役后删除清单为 0
PASS  G8 全量覆盖率对比已完成（full.json）
[verify:m9-13] PASS 检查=136
AETHER_M9_13 PASS
```

覆盖率（G8 全量）：Rust 行覆盖 **74.42%**（TOTAL 44382/65161，`--fail-under-lines 70` 通过）；前端 lines **90.87%**（不下降）。

## 4. 遗留阻塞与限制（不得以「已知问题」带过）

| 项 | 状态 | 影响/后续 |
|---|---|---|
| **macOS Developer ID 签名 + 公证（G6）** | **仅阻塞 macOS 公开发布（`macos_release_pending`）** | Apple secrets 未申请（`M9_11_MAC_SIGNING_READY` 未设置）；按 **ADR-030 v0.2** 不阻塞 M9/M10。公开发布前按 `docs/M9-14-macOS-签名授权方案.md` 完成人工授权；发布工作流对 macOS 仍失败关闭。 |
| macOS E2E/安装冒烟（M9-14 变更后） | 未在本机验证 | 本机无 macOS runner；历史 CI run 41602087592 的 macOS E2E/unsigned 安装冒烟通过，但 M9-14 删除 Tauri 后需推送 CI 复跑。 |
| Linux 构建/冒烟（M9-14 变更后） | 未在本机验证 | 本机无 Linux；历史 CI `m9-12-linux-build-smoke` 通过；需 CI 复跑。 |
| M9-14 新 Electron 安装包冒烟 | **通过（`--skip-build` 复用 stage daemon/runtime-bundle）** | 新 Electron shell/图标配置打包 + MSI 安装/首启/卸载/数据保留全通过（窗口 864ms / UI ready 6224ms / idle 514.57 MiB）；完整 release 构建由 CI `m9-11-package` 复跑。 |
| 真实 G4–G7 故障注入 | 未验证 | 失败处理演练为发布决策模型模拟；真实 Gate 失败仍由各 verifier 阻断（不修改真实证据/阈值/断言）。 |
| M9-13 回归结果 | 历史证据 + 本机 G8 覆盖率 | `scripts/test/.tmp/m9-13/regression/results.json` 为 M9-13 历史运行；G8 后 T4 已在 `aether-app` 单独复跑通过，全量回归由 CI `m9-13-windows` 复跑。 |
| `verify:m9-04` / `verify:m9-06` 静态/基线漂移 | pre-existing，非 G8 阻塞 | `verify:m9-04` 仅剩 ADR-030 阈值未登记差异（M9-13 证据已登记）；`verify:m9-06` 静态 `renderer 未使用 permission_resolve` 与当前 renderer 结构漂移，实际权限回环测试全通过。M9-14 未改权限桥/阈值。 |
| M2-10 Tauri 验证器 | 已退役 | 原 `aether-tauri --test m2_10_permission_loop` 随 G8 删除；权限回环由 M9-06 测试与 M9-12 E2E（allow/deny/timeout）承接。 |
| 本地门禁复跑限制 | 非发布阻塞 | `cargo deny` 首次通过、复跑因 GitHub advisory DB 不可达失败；`cargo fmt --check` 全仓 pre-existing 差异；`check:similarity` 因 `dist-out/win-unpacked` 产物误报。CI 干净 checkout 复跑。 |

## 5. 签核

| 角色 | 日期 | 结论 |
|---|---|---|
| 执行（AI 编码助手） | 2026-11-03 | M9-14 DoD-1～DoD-5 已执行；`verify:m9-14`/失败处理演练通过；提交 G8 报告 |
| 项目负责人裁定（用户） | 2026-11-05 | **M9/M10 不因 macOS 阻塞**；G8 **条件通过**（`conditional_pass`）；macOS 公开发布走人工授权方案（ADR-030 v0.2） |
| macOS 公开发布 | **未放行** | `macos_release_pending`；完成 `docs/M9-14-macOS-签名授权方案.md` 后才可发布 DMG；Windows/Linux 可按 ADR-030 发布 |
| M10 | **可启动** | 需先完成 M10-00 ADR 批次；macOS 授权不阻塞 |

> **G8 结论**：切换与退役已完成，仓库/依赖树/文档已清零 Tauri；**G8 条件通过**。macOS 签名/公证是**公开发布门禁**，不是 M9/M10 阻塞项；**M10 可启动**（需 M10-00 ADR 批次）。
