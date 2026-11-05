# M9 Gate G7 报告：全量回归

| 项 | 内容 |
|---|---|
| Gate | **G7 全量回归**（P2 计划 §2.15；ADR-029 v0.4 §10） |
| 任务 | M9-12（CI/E2E/覆盖率/安全门禁）、M9-13（全量回归/性能/旧数据/HEAD 基线） |
| 日期 | 2026-11-05（复跑；HEAD `994663713c8d4105486393f6f84fba5ec6a1790e`） |
| 计划/设计 | P2 v0.8；设计 v1.30；ADR-029 v0.4；ADR-030 v0.2 |
| 证据 | `docs/M9-12-证据.md`、`docs/M9-13-证据.md`、`docs/M9-CI-41950594105-证据.md`、`scripts/test/.tmp/m9-13/`、`docs/M9-13-macOS-密钥登记.md` |
| 验证 | `pnpm verify:m9-12`、`pnpm verify:m9-13`、`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`pnpm --filter @aether/desktop test`、P1 三项复跑 |
| 结论 | **条件通过（conditional_pass）**：本机全量/性能/数据/P0/P1 通过；平台冒烟（CI run `41950594105`）macOS E2E/DMG 与 Linux build/xvfb 通过；修复后的 CI 全绿待推送复跑 |

## 1. 通过条件逐项核对

| # | G7 条件（P2 §2.15） | 本次证据 | 判定 |
|---|---|---|---|
| 1 | `cargo test --workspace` | 本次低内存串行复跑（见 §2.1） | ✓（本机） |
| 2 | `pnpm -r test/typecheck/build` | `pnpm -r --workspace-concurrency=1 test` 8 项目/620 tests passed；typecheck 8/9 Done；build Done | ✓ |
| 3 | clippy / deny | 本次 clippy 复跑；deny 既有 PASS（CI supply-chain gate success） | ✓ |
| 4 | 覆盖率 | G8 全量：Rust 行 74.42%（44382/65161）、前端 lines 90.87%；本次仅测试/脚本变更，未降低 | ✓ |
| 5 | P0/P1 映射 | P0 T1–T15、P1 三运行时/加密备份/规则误配率；本次 P1 三项 + T5a 复跑 | ✓ |
| 6 | 10 会话 × 50 delta/s | `performance.json`：control=20/20、lost=0、P95=1ms | ✓ |
| 7 | 硬阈值启动/RSS/体积 | 窗口 528ms、握手 332ms、UI ready 5932ms、会话创建 137ms、idle 567.41 MiB、10 会话 728.74 MiB、包体 160,227,328 B | ✓ |
| 8 | HEAD 基线对照 | `verify:m9-01-baseline` 通过；63 命令/交付物哈希/契约摘要一致；阈值 ADR-030 已登记 | ✓ |
| 9 | Win/mac/Linux | Windows 本机通过；macOS E2E 13/13 + DMG 冒烟 success；Linux build/xvfb success（CI `41950594105`） | ✓/部分 |
| 10 | 未跑项文档化 | macOS M9-13 全量未运行（工作流仅 Windows runner），标记 `ci-partial`；macOS 签名/公证 `macos_release_pending` | ✓（已登记） |

## 2. 本次复跑输出

### 2.1 全量 Rust 与 clippy

```text
$ cargo test --workspace -j 1 -- --test-threads=1
test result: ok（104 个测试套件；876 passed; 0 failed; 5 ignored）
exit=0；耗时约 204s
（完整日志：scripts/test/.tmp/m9-13/regression/workspace-rust.log）

$ cargo clippy --workspace --all-targets -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s) in 40.69s
exit=0（无告警）
```

> 本次新增 `crates/aether-app/tests/m5_03_encrypted_backup.rs`（8 用例）并修改
> 适配器测试 harness；全量结果以 `workspace-rust` / `clippy` 归档为准。
> Tauri 退役后不再构建 `aether-tauri`，测试套件数由迁移期 112 降为 104。

### 2.2 门禁脚本

```text
$ pnpm verify:m9-12
[verify:m9-12] PASS 检查=25 备注=0
AETHER_M9_12 PASS

$ pnpm verify:m9-13
[verify:m9-13] PASS 检查=134 备注=2
AETHER_M9_13 PASS

$ pnpm -r typecheck
8 of 9 workspace projects；adapter-sdk/protocol/desktop/adapter-* 全部 Done

$ pnpm -r build
apps/desktop renderer + Electron main/preload/shell 构建完成

$ pnpm -r --workspace-concurrency=1 test
8 个 workspace 项目全绿（620 tests：adapter-sdk 62 / protocol 10 / desktop 364 /
claude 36 / codex 38 / dsh 50 / mock 56 / host 4）
```

### 2.3 P0/P1 修复后复跑

```text
T5a（m2_02_t5a）                         2 passed
P1 三运行时（m2_02_consistency/m2_11_codex/m2_11_dsh）  7 + 10 + 7 passed
P1 加密备份（verify:m5-03:drill）         2 轮 × 8 passed
P1 规则误配率（verify:m6-02）             PASS（误配 1.00% < 5%）
```

### 2.4 CI run `41950594105`（HEAD `9946637`）

```text
✓ M9-12 Playwright Electron E2E (macOS)          13/13 passed
✓ M9-12 DMG mount/copy/launch smoke (macOS)      success
✓ M9-12 Linux build + xvfb smoke                 success（deb/AppImage）
✓ M9-12 MSI silent install/uninstall smoke       success
✗ Coverage gate (>=70% lines)                    permission.cancelled 审计偶发（已修复）
✗ M9-13 regression (Windows)                     P1/T5a 旧 Tauri/单适配器引用（已修复）
✗ m9-11-package / Windows MSI                    runner 失去通信（基础设施）
```

## 3. 本轮整改

| 问题 | 整改 | 复跑 |
|---|---|---|
| 权限取消审计异步窗口 | `wait_audit_action` 轮询（15s），断言不变 | m9_06 bridge 6/6 |
| M5-03 演练引用已退役 `aether-tauri` | 测试迁移到 `aether-app`，脚本改 `-p aether-app` | 2×8 PASS |
| M6-02 零直通引用已退役 `aether-tauri` | 改用 `aether-app --test m9_06_permission_bridge` | PASS |
| 共享适配器宿主与旧单适配器路径不兼容 | 测试 harness 自动前置 `--adapter`；regression 指向宿主 | T5a/三运行时 PASS |
| 平台矩阵仍标历史 run | 回填 `41950594105`：macOS `ci-partial`、Linux `passed` | `verify:m9-13` PASS |

## 4. 条件与遗留

| 项 | 状态 | 影响 |
|---|---|---|
| 修复后的 GitHub Actions 全绿 | **待推送** | 本次未提交/推送；推送后需复跑 `ci.yml`/`m9-11-package.yml` 并归档新 run id |
| macOS M9-13 全量回归 | 未运行 | CI 工作流仅 Windows runner；macOS 以 M9-12 E2E + DMG 冒烟覆盖，平台矩阵标 `ci-partial` |
| macOS 签名/公证 | `macos_release_pending` | 仅阻塞公开发布；ADR-030 v0.2 不阻塞 M9/M10 |
| 覆盖率重跑 | 未重跑 | G8 全量 74.42% 已归档；本次仅测试/脚本变更，生产代码未变（`permission.ts` 仅注释） |

> G7 不因「修复后 CI 未推送复跑」而宣称全绿；本报告按 `conditional_pass` 记录，
> 待 CI 新 run 全绿后升级为 `pass`。

## 5. 签核

| 角色 | 日期 | 结论 |
|---|---|---|
| 执行（AI 编码助手） | 2026-11-05 | 本机全量/性能/数据/P0/P1 复跑通过；CI 失败根因已修复 |
| 项目负责人 | 2026-11-05 | **G7 条件通过**；修复后 CI 全绿待推送复跑 |
