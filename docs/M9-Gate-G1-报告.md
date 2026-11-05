# M9 Gate G1 报告：daemon 协议

| 项 | 内容 |
|---|---|
| Gate | **G1 daemon 协议**（P2 计划 §2.15；ADR-029 v0.4 D-029-3/D-029-6） |
| 任务 | M9-02（`aether-app` 抽层）、M9-03（`aether-rpc` + `aetherd` stdio） |
| 日期 | 2026-11-05（复跑；HEAD `994663713c8d4105486393f6f84fba5ec6a1790e`） |
| 计划/设计 | P2 v0.8；设计 v1.30；ADR-029 v0.4；G8 已执行（Tauri 退役） |
| 证据 | `docs/M9-02-证据.md`、`docs/M9-03-证据.md`、`scripts/test/m9-02/`、`scripts/test/m9-03/fixtures/` |
| 验证 | `pnpm verify:m9-02`、`pnpm verify:m9-03`、`cargo test -p aether-app --test ipc_validation`、`cargo test -p aether-rpc`、`cargo test -p aether-daemon --test m9_03_stdio` |
| 结论 | **G1 通过（PASS）** |

## 1. 通过条件逐项核对

| # | G1 条件（P2 §2.15） | 本次证据 | 判定 |
|---|---|---|---|
| 1 | `aether-app` 无 Tauri | `verify:m9-02`：`Cargo.toml` 无 tauri/wry/webkit；49 个 app 源文件去注释后无 `use tauri`/`tauri::` | ✓ |
| 2 | `aetherd` headless | `crates/aether-daemon` 仅依赖 `aether-app`/`aether-rpc`/核心库；`verify:m9-03` daemon 源文件 5（无 tauri） | ✓ |
| 3 | initialize 目标 2s / 硬超时 10s / retry 1 | `docs/M9-03-证据.md` §initialize；`m9_03_stdio` 进程级用例；`verify:m9-03` 结构断言 | ✓ |
| 4 | phase / capabilities | `DaemonPhase` 8 产出值 + `migrating`/`blocked` 保留；capabilities 三值 + client-only（`verify:m9-03`） | ✓ |
| 5 | 59 方法 | `COMMANDS` 63 = 59 daemon + 4 shell；daemon 路由 59 条；`m9_04_routes` 覆盖 | ✓ |
| 6 | 2MiB / 非法帧 | `MAX_FRAME_BYTES = 2 * 1024 * 1024`；有界读取器；`aether-rpc` golden/oversized/畸形帧用例；超限只失败本请求 | ✓ |
| 7 | params 内层 DTO | 15 行 golden fixtures / 16 用例（有参/无参/空对象/`payload` 包装拒绝） | ✓ |
| 8 | A4 致命 21 / DB 无法打开·checksum 22 / quick_check 安全模式 | `docs/M9-03-证据.md` 三组 fixture；退出码 0/10/20/21/22/23/24；安全模式不退出 | ✓ |
| 9 | stdout 仅 JSONL / 干净退出 | `aetherd --stdio`/`--version`/`--help`；`m9_03_stdio` 12 用例 | ✓ |
| 10 | 63 命令名称与基线一致 | `verify:m9-02` 与 `docs/M9-01-baseline/manifest.json` 63 条逐项一致 | ✓ |

## 2. 本次复跑输出（2026-11-05）

```text
$ pnpm verify:m9-02
[m9-02] OK：命令 63（59 daemon + 4 shell）· Tauri 已退役（G8，无回退包） · app 源文件 49（无 tauri）· 基线一致 · 测试迁移齐备

$ pnpm verify:m9-03
[m9-03] OK：rpc 源文件 6（叶子）· daemon 源文件 5（无 tauri）· 传输码 12 · DaemonPhase 8+2 保留 · 退出码 0/10/20/21/22/23/24 · fixture 15 行/16 用例 · 进程级用例 12 · 业务码 9 与生成物一致
AETHER_M9_03 PASS
```

动态测试（Windows x64，低内存串行）：

```text
cargo test -p aether-app --test ipc_validation        # 15 passed（63 命令路由/严格拒绝/清单冻结）
cargo test -p aether-rpc                              # lib + golden_frames + m9_06_dialog_pick 全绿
cargo test -p aether-daemon --test m9_03_stdio        # 12 passed
```

## 3. 本轮整改（仅恢复可复跑性，不放宽 DoD）

| 问题 | 整改 |
|---|---|
| `verify:m9-02` 仍读取已退役的 `crates/aether-tauri/src/ipc/commands.rs` / `tests/ipc_validation.rs`，G8 后 ENOENT | 退役态断言：crate 不存在、workspace/`Cargo.lock` 无 `aether-tauri`、M9-14 证据含退役标记；`aether-app/tests/ipc_validation.rs` 的 63 命令矩阵仍为硬条件；若未来恢复 Tauri（禁止）自动回到原 wrapper/`collect_commands!` 检查 |
| AGENTS 依赖方向已更新为 `core → … → app → daemon`（无 tauri） | 脚本同步断言新方向，并新增「禁止重新引入 `aether-tauri`」检查 |

## 4. 限制与后续

- G1 的进程级动态证据以 `cargo test` 为准；本次由 `cargo test --workspace`（G7）覆盖，
  未以 `cargo check --tests` 代替。
- macOS/Linux 的 daemon 行为由 G7 平台矩阵覆盖；本次本机仅 Windows x64。
- 本报告不改变 M9-03 的退出码/致命检查口径；A4 仍是致命、M9 不迁移。

## 5. 签核

| 角色 | 日期 | 结论 |
|---|---|---|
| 执行（AI 编码助手） | 2026-11-05 | G1 条件逐项复跑通过；脚本已恢复可复跑 |
| 项目负责人 | 2026-11-05 | **G1 通过**；M9-02/M9-03 证据保持有效 |
