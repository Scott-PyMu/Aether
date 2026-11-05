# M9 Gate G2 报告：命令等价

| 项 | 内容 |
|---|---|
| Gate | **G2 命令等价**（P2 计划 §2.15；ADR-029 v0.4 D-029-4） |
| 任务 | M9-04（63 命令路由与等价） |
| 日期 | 2026-11-05（复跑；HEAD `994663713c8d4105486393f6f84fba5ec6a1790e`） |
| 计划/设计 | P2 v0.8；设计 v1.30；ADR-029 v0.4；ADR-030 v0.2（阈值修订） |
| 证据 | `docs/M9-04-证据.md`、`packages/protocol/src/command-manifest.json`、`docs/M9-01-baseline/`、`scripts/test/m9-04/fixtures/` |
| 验证 | `pnpm verify:m9-04`、`pnpm verify:error-codes`、`pnpm verify:m9-01-baseline` |
| 结论 | **G2 通过（PASS）** |

## 1. 通过条件逐项核对

| # | G2 条件（P2 §2.15） | 本次证据 | 判定 |
|---|---|---|---|
| 1 | 命令合同 **63/63** | `command-manifest.json` total=63；`aether-app::COMMANDS` 与 M9-01 基线逐项一致 | ✓ |
| 2 | **UI 暴露 61/63** | reserved=2（`startup_get`/`startup_migrate`）；UI=61 | ✓ |
| 3 | 59 daemon + 4 shell | daemon=59、shell=4；壳命令 `app_exit`/`app_restart`/`ref_pick`/`startup_pick_target` | ✓ |
| 4 | 参数矩阵 63 | `scripts/test/m9-04/fixtures/command-matrix.json`：63 条合法样本 + 218 条 rejects（未知字段/类型/枚举/长度/ULID/路径/越界） | ✓ |
| 5 | **32 枚举 + `business_code`** | `error_codes.json` ipc=32、business=9；`IpcError.business` 走 `data.business_code`；逐项可达 | ✓ |
| 6 | 与 HEAD golden 对照 | 63 条基线条目哈希一致；仅已登记差异：`messages_page`/`audit_query` 预算字段 + `responses.json` | ✓ |
| 7 | 预算差异已登记 | `messages_page`/`audit_query` 差异只新增、不改期望值；`max_bytes` 64KiB–1.5MiB、不透明 cursor、`messages_has_more` | ✓ |
| 8 | 壳本地 E2E | `app_exit`/`app_restart`/`ref_pick`/`startup_pick_target` 用例；选择器 canonical/取消/非法路径 | ✓ |
| 9 | 生成物 CI diff=0 | `aether-app` 清单重新生成 `command-manifest.json` 一致；`verify:error-codes` diff=0 | ✓ |

## 2. 本次复跑输出（2026-11-05）

```text
$ pnpm verify:m9-04
[m9-04] OK：命令 63（59 daemon + 4 shell；UI 61 / 保留 2）· 基线条目 63（已登记差异 2 命令 + 1 响应类型）· 矩阵 63 条 / rejects 218 · 错误码 32+9 · golden 13/8 组 · 预算 64KiB–1.5MiB / 不透明游标 · RPC 路由与壳本地用例齐备
AETHER_M9_04 PASS

$ pnpm verify:error-codes
[error-codes] 通过：ipc=32、business=9、schema/去重/kind 合规、生成物 diff=0、Rust 业务常量一致
AETHER_ERROR_CODES PASS

$ pnpm verify:m9-01-baseline
[m9-01-baseline] 通过：commands=63（daemon 59 / shell 4，可达 61 + 保留 2）、entries=63、artifacts=13、响应类型=29、事件 payload=23、risk=8、裁定=2、electron=45.0.0（精确 patch）、阈值全冻结、contract_digest 一致
AETHER_M901_BASELINE PASS
```

## 3. 本轮整改

| 问题 | 整改 |
|---|---|
| `verify:m9-04` 报 `freeze/thresholds.json` 未登记差异（ADR-030 将 idle RSS 500→600 MiB、UI ready 5000→8000 ms） | 将阈值文件加入已登记差异白名单，并新增逐项值断言：只允许 ADR-030 两项修订，其余冻结值不变；`amended_by` 必须含 ADR-030 |
| 原证据复现命令仍含 `aether-tauri` 测试宿主 | G8 已把 `m3_02_session_backend`/`m6_03_audit` 迁移到 `aether-app`；本报告按新宿主复跑 |

## 4. 限制与后续

- 63 命令进程级路由由 `cargo test -p aether-daemon --test m9_04_routes`（2 用例）覆盖；
  参数/golden 由 `m9_04_contract`/`m9_04_golden` 覆盖，均纳入 G7 全量回归。
- 预算/截断的已登记差异不视为契约漂移；任何新增差异必须先更新 ADR/基线登记，
  否则 `verify:m9-04` 失败。

## 5. 签核

| 角色 | 日期 | 结论 |
|---|---|---|
| 执行（AI 编码助手） | 2026-11-05 | G2 条件逐项复跑通过；ADR-030 阈值差异已登记 |
| 项目负责人 | 2026-11-05 | **G2 通过** |
