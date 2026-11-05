# M9 Gate G5 报告：数据/安全

| 项 | 内容 |
|---|---|
| Gate | **G5 数据/安全**（P2 计划 §2.15；ADR-029 v0.4 §5/§10；设计 D3/D9/D10/D13） |
| 任务 | M9-09（生命周期/恢复/数据锁）、M9-10（安全基线） |
| 日期 | 2026-11-05（复跑；HEAD `994663713c8d4105486393f6f84fba5ec6a1790e`） |
| 计划/设计 | P2 v0.8；设计 v1.30；ADR-029 v0.4；ADR-030 v0.2 |
| 证据 | `docs/M9-09-证据.md`、`docs/M9-10-证据.md`、`docs/M9-13-证据.md`、`scripts/test/.tmp/m9-13/data/data-drill.json`、`scripts/test/.tmp/m5-03/` |
| 验证 | `pnpm verify:m9-09`、`pnpm verify:security`、`pnpm m9-13:data`（既有证据）、`pnpm verify:m5-03:drill` |
| 结论 | **G5 通过（PASS）** |

## 1. 通过条件逐项核对

| # | G5 条件（P2 §2.15） | 本次证据 | 判定 |
|---|---|---|---|
| 1 | 旧库直接打开 / 无 schema 变更 | `data-drill`：32 表、9 迁移记录前后一致；会话/消息/设置/预设/工作区/审计/供应商/备份均可用 | ✓ |
| 2 | `quick_check` 失败 → 只读安全模式 | `m9_13_safe_mode_restore`（1 passed）；S-08 导出/备份；不退出 | ✓ |
| 3 | 安全模式 `backup_restore` 维护例外 | 停写 → 恢复 → main 重启 daemon；`data.safe_mode.exit=0` | ✓ |
| 4 | 加密备份/恢复 + 密钥零泄漏 | `pnpm verify:m5-03:drill` 2 轮 × 8 用例全绿；AEBK1 头/两模式/失败回滚/诊断审计响应 0 命中 | ✓ |
| 5 | Keychain preflight / S-13 | `data.keychain.mode=ok`；M9-09 用例 exit=0；失败走重授权/口令降级，不删旧条目 | ✓ |
| 6 | 全部路径命令 canonicalize/白名单 | `verify:security`：路径命令全集、0 绕过；`m9_10_path_security` 用例 | ✓ |
| 7 | 密钥不出现在响应/日志/诊断/导出/错误 data | `verify:security` 检查=213；M5-03 扫描 0 命中（含阳性对照） | ✓ |
| 8 | WebContentsView 隔离 | `verify:security`：独立 partition、无 preload/Node、cookie/localStorage 互不可读、下载确认 | ✓ |
| 9 | daemon OS 锁（无跨壳） | `.aetherd.lock` OS 独占；`data_dir_locked`；T13 A4 同步盘门禁 | ✓ |
| 10 | kill 恢复 / 无孤儿 | `verify:m9-09` 检查=93；T11 退出、T12 单实例、T13 A4 均在 G7 复跑 | ✓ |

## 2. 本次复跑输出（2026-11-05）

```text
$ pnpm verify:m9-09
[m9-09] PASS 检查=93
AETHER_M9_09 PASS

$ pnpm verify:security
[verify:security] PASS 检查=213
AETHER_M9_10_SECURITY PASS

$ pnpm verify:m5-03:drill
PASS  演练第 1/2 轮（加密备份 → 清库 → 恢复 → 一致；无外部凭证）
PASS  演练第 1 轮证据复核（七步/一致/加密标记/来源 id/扫描 0 命中）
PASS  演练第 2/2 轮（加密备份 → 清库 → 恢复 → 一致；无外部凭证）
PASS  演练第 2 轮证据复核（七步/一致/加密标记/来源 id/扫描 0 命中）
----- 全部通过 -----
```

## 3. 本轮整改

| 问题 | 整改 |
|---|---|
| M5-03 加密备份演练仍调用已退役的 `cargo test -p aether-tauri --test m5_03_encrypted_backup`（CI P1 失败） | 将测试按 G8 迁移到 `crates/aether-app/tests/m5_03_encrypted_backup.rs`（命名空间改为 `aether_app`，断言不变），演练脚本改用 `-p aether-app`；2×8 用例复跑通过 |
| 旧 Tauri 测试宿主删除后 P1 证据失效 | 以 `aether-app` 宿主重新归档演练证据；加密/恢复/回滚/脱敏断言未放宽 |

## 4. 限制与后续

- `m9-13:data` 的运行证据为 2026-11-02/03 本机 Windows 运行；本次未重跑数据演练，
  但 M5-03 加密演练与 `verify:m9-09`/`verify:security` 已按当前 HEAD 复跑。
- macOS Keychain 真机/S-13 由 G6/G7 平台矩阵覆盖；本次本机 Windows Keychain
  preflight `mode=ok`。
- 全量 `cargo test --workspace` 由 G7 覆盖；本次未以 `cargo check --tests` 代替。

## 5. 签核

| 角色 | 日期 | 结论 |
|---|---|---|
| 执行（AI 编码助手） | 2026-11-05 | G5 条件复跑通过；M5-03 已迁移到 `aether-app` 并重新归档 |
| 项目负责人 | 2026-11-05 | **G5 通过** |
