# M9 Gate G4 报告：Electron 壳

| 项 | 内容 |
|---|---|
| Gate | **G4 Electron 壳**（P2 计划 §2.15；ADR-029 v0.4 D-029-1/D-029-7） |
| 任务 | M9-07（Electron 壳）、M9-08（前端 typed client/类型生成） |
| 日期 | 2026-11-05（复跑；HEAD `994663713c8d4105486393f6f84fba5ec6a1790e`） |
| 计划/设计 | P2 v0.8；设计 v1.30；UI-UX v0.17；ADR-029 v0.4 |
| 证据 | `docs/M9-07-证据.md`、`docs/M9-08-证据.md`、`packages/protocol/src/{types,client,command-manifest,error_codes}`、`apps/desktop/electron/` |
| 验证 | `pnpm verify:m9-07`、`pnpm verify:m9-08`、`pnpm --filter @aether/desktop test`、`pnpm --filter @aether/desktop typecheck/build` |
| 结论 | **G4 通过（PASS）** |

## 1. 通过条件逐项核对

| # | G4 条件（P2 §2.15） | 本次证据 | 判定 |
|---|---|---|---|
| 1 | 生产 renderer 无 Tauri | `verify:m9-08`：`renderer invoke=0 / @tauri=0`；`@tauri-apps` 依赖为 0 | ✓ |
| 2 | preload 含 `pathForFile` | preload 白名单：`rpc/onEvent/offEvent/pathForFile/version`；`verify:m9-07` 文件 18 | ✓ |
| 3 | CSP / 导航拦截 | 生产 `connect-src 'none'`/`frame-src 'none'`；`will-navigate`/`setWindowOpenHandler`/`will-attach-webview`；`verify:security` 检查=213 | ✓ |
| 4 | 拖拽/选择器/单实例/退出 | `localCommands.test.ts` 8 passed；`daemonLifecycle`/`daemonTree`/`shellLocal` 测试；T12 单实例 | ✓ |
| 5 | 46 前端测试文件 | `verify:m9-08` 测试文件 46；本次全量 `pnpm --filter @aether/desktop test` **59 文件 / 364 tests passed** | ✓ |
| 6 | typed client | `client.ts` 63 方法；`types.ts` 重新生成 diff=0；错误 `AetherError` 32+9 | ✓ |
| 7 | S-01 不可达 / S-12 / S-13 | `verify:m9-07`：锚点 19、S-12 原因 6、路由 63；`verify:m9-08`：S-12/S-13 锚点 13 | ✓ |
| 8 | `data_dir_locked`/`db_open_failed` | S-12 原因池含 `data_dir_locked`/`db_open_failed`；`startup.test.ts` 10 tests | ✓ |
| 9 | UI-UX v0.17 | `docs/UI-UX-设计规格.md` v0.17（S-01/S-01a 不可达、S-12/S-13、锚点 22） | ✓ |

## 2. 本次复跑输出（2026-11-05）

```text
$ pnpm verify:m9-07
[m9-07] PASS 文件=18 锚点=19 S-12 原因=6 路由=63
AETHER_M9_07 PASS

$ pnpm verify:m9-08
[error-codes] 通过：ipc=32
[protocol-types] 通过：packages/protocol/src/types.ts 与重新生成结果一致
[m9-08] OK：命令 63（59+4；UI 61/保留 2）· client 方法 63 · 请求 DTO 59 · 错误码 32+9 · renderer invoke=0 / @tauri=0 · 测试文件 46 · S-12/S-13 锚点 13 · diff=0
AETHER_M9_08 PASS

$ pnpm --filter @aether/desktop test
Test Files  59 passed (59)
     Tests  364 passed (364)
  Duration  102.19s
```

## 3. 本轮整改

| 问题 | 整改 |
|---|---|
| `apps/desktop/src/permission.ts` 注释仍写「生产 = Tauri 生产实现」（G8 后已退役） | 改为「生产 = `@aether/protocol` typed client；M9-14/G8 后不再有 Tauri 实现」；无行为变更 |

- 无功能代码变更；G4 的 preload/CSP/隔离断言均未被放宽。
- `verify:m9-06` 的 renderer typed-client 静态检查同步修正（见 G3 报告），不影响 G4。

## 4. 限制与后续

- 本机只验证 Windows x64；macOS `pathForFile`/窗口行为由 CI run `41950594105` 的
  macOS Playwright E2E（13/13）与 DMG 冒烟覆盖。
- UI-06 完整交互（标签/分屏/下载/权限 UX）仍属 P4；M9 只交付
  `WebContentsView` PoC + 安全基线。
- `pnpm --filter @aether/desktop typecheck/build` 在 G7 全量回归中复跑；
  本次未修改生产逻辑。

## 5. 签核

| 角色 | 日期 | 结论 |
|---|---|---|
| 执行（AI 编码助手） | 2026-11-05 | G4 条件复跑通过；59 文件/364 前端测试全绿 |
| 项目负责人 | 2026-11-05 | **G4 通过** |
