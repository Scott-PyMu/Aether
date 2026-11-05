# M9 Gate G3 报告：事件/权限等价

| 项 | 内容 |
|---|---|
| Gate | **G3 事件/权限等价**（P2 计划 §2.15；ADR-029 v0.4 D-029-5/D-029-6/D-029-9） |
| 任务 | M9-05（事件桥）、M9-06（权限/对话框桥） |
| 日期 | 2026-11-05（复跑；HEAD `994663713c8d4105486393f6f84fba5ec6a1790e`） |
| 计划/设计 | P2 v0.8；设计 v1.30；ADR-029 v0.4 |
| 证据 | `docs/M9-05-证据.md`、`docs/M9-06-证据.md`、`scripts/test/m9-05/fixtures/`、`scripts/test/m9-06/fixtures/` |
| 验证 | `pnpm verify:m9-05`、`pnpm verify:m9-06`、`cargo test -p aether-app --test m9_05_event_bridge --test m9_06_permission_bridge --test m9_06_permission_notice`、`cargo test -p aether-daemon --test m9_05_events --test m9_06_permission_pending` |
| 结论 | **G3 通过（PASS）** |

## 1. 通过条件逐项核对

| # | G3 条件（P2 §2.15） | 本次证据 | 判定 |
|---|---|---|---|
| 1 | EventEnvelope 9 字段 / 附录 B | `verify:m9-05`：字段=9、事件类型=23、fixtures=2 | ✓ |
| 2 | seq 会话内单调唯一 / DB UNIQUE | `docs/M9-05-证据.md`；`events UNIQUE(session_id, seq)`；补读按 `last_seq` | ✓ |
| 3 | 先日志后广播 | 故障注入写失败 → 不广播、不产生 UI 事件；控制事件不丢（M9-05 证据） | ✓ |
| 4 | 10 会话 × 50 delta/s 控制事件 0 丢失 | `docs/M9-05-证据.md`；G7 `m9-13` 10×50 报告 control=20/20、lost=0、P95=1ms | ✓ |
| 5 | 控制队列满 → journal / 不重启 daemon | `docs/M9-05-证据.md`；连续补读失败升级审计 | ✓ |
| 6 | 权限回环 allow/deny/300s/取消 + 审计 | `m9_06_permission_bridge` 6 passed；deny/allow/timeout/cancel 全通过 | ✓ |
| 7 | 暂停/恢复/中断/run_retry 语义 + 能力门 | 同上 `pause_resume_and_capability_gate_are_unchanged`；`pause_unsupported` | ✓ |
| 8 | `ref_pick`/`startup_pick_target` canonical/取消/非法路径 | `localCommands.test.ts` 8 passed；`artifact_add` 复核 | ✓ |
| 9 | `dialog.pick` 仅协议 fixture | `verify:m9-06` 静态断言产品路径 0 引用；RPC fixture 正常/超时/取消/迟到丢弃 | ✓ |
| 10 | 轮询 + 通知并存不重复决议 | `permission.pending` 通知仅提示；唯一入口 `permission_resolve`；daemon 进程级用例 1 passed | ✓ |

## 2. 本次复跑输出（2026-11-05）

```text
$ pnpm verify:m9-05
[m9-05] PASS 字段=9 事件类型=23 fixtures=2 Rust 用例=5 TS 用例=5
AETHER_M9_05 PASS

$ pnpm verify:m9-06
PASS  aether-rpc：dialog.pick 协议预留单测（8 passed）
PASS  aether-rpc：dialog.pick fixture 回放（1 passed）
PASS  aether-app：permission.pending 通知桥（3 passed）
PASS  aether-app：权限回环 E2E（6 passed）
PASS  aether-daemon：permission.pending 进程级通知 + 唯一决议路径 + 审计落库（1 passed）
PASS  desktop：ref_pick/startup_pick_target 壳本地单测（8 passed）
PASS  静态契约：dialog.pick 仅预留 / 59+4 / 通知唯一决议路径 / 选择器复核在案
AETHER_M9_06 PASS
```

## 3. 本轮整改

| 问题 | 整改 |
|---|---|
| `verify:m9-06` 静态检查仍要求 renderer 字面量 `"permission_resolve"`，M9-08 typed client 后为 `client.permissionResolve` | 去注释后接受 `permission_resolve` 或 `client.permissionResolve`；投影层仍禁止自行决议 |
| Linux CI 覆盖率作业中 `permission_loop_cancel_and_run_retry_keep_channel_semantics` 偶发审计断言失败 | 新增 `wait_audit_action`（15s）等待审计落库；断言内容（`permission.cancelled`）不变，未放宽终态/零直通/决议次数 |

## 4. 限制与后续

- 10×50 的当前运行证据来自 G7 性能证据（`scripts/test/.tmp/m9-13/performance/performance.json`）；
  事件桥单测由 `m9_05_event_bridge` 覆盖，纳入 `cargo test --workspace`。
- `dialog.pick` 仍仅为 P3 MCP elicitation 预留，不进入 63 命令面/产品路径。
- macOS/Linux 权限行为由 G7 平台矩阵与 M9-12 E2E 覆盖；本次本机 Windows x64 全绿。

## 5. 签核

| 角色 | 日期 | 结论 |
|---|---|---|
| 执行（AI 编码助手） | 2026-11-05 | G3 条件逐项复跑通过；静态误报与 CI 时序问题已修复 |
| 项目负责人 | 2026-11-05 | **G3 通过** |
