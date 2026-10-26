# M9-01 HEAD golden 基线

> 录制于 **M9-02（抽取 `aether-app`）之前**，HEAD = `4558d06f4222fd469ac4246d9068382e30dcc2f1`。
> 依据：ADR-029 v0.4 §9.4-1（「golden 基线必须先录」）、P2 计划 M9-01 DoD-8/DoD-9。

## 这套基线冻结什么

| 文件 | 冻结内容 |
|---|---|
| `manifest.json` | `recorded_at/head_commit/platform/version/commands[63]/entries[{path,sha256}]`、计数、`contract_digest` |
| `commands/NN-<name>.json` × 63 | 每条命令的路由（`daemon`/`shell`）、UI 暴露（61 可达 + 2 保留）、请求 DTO 形状、**可证明的拒绝用例**、分页/时间窗语义边界 |
| `contract/error_codes.json` | 32 个 `IpcErrorCode` + 9 个 `business_code` 现状（与 `packages/protocol/src/error_codes.json` 同源） |
| `contract/responses.json` | 响应侧契约：63 条命令统一返回自由 JSON（`response.type = null`）+ 从 HEAD 源码解析的序列化类型（字段/可选性/枚举线上值），含 `IpcError`、`MessagesPageResponse`、`SessionSummary`、`HealthReport` 与域对象 |
| `contract/event_payloads.json` | 附录 B 23 类事件 → `EventPayload` 结构体 → 字段可选性（MVP 19 项标记）+ 信封 9 字段 |
| `contract/stats.json` | DoD-3 统计归档：63 命令、`invoke` 调用点、前端测试文件、`bindings.ts` 行数（1003）、核心行数、Rust 测试文件数 |
| `contract/events.json` | EventEnvelope 9 字段、附录 B 事件类型（含 MVP 19 项标记）、seq/先日志后广播规则 |
| `contract/settings.json` | 设置键白名单 9 项（未登记键一律 `invalid_enum`） |
| `contract/migrations.json` | 迁移 `0001–0009` 清单（已发布不可修改） |
| `contract/lifecycle.json` | 启动/关闭时序预算（窗口 1.5s、握手 2s/硬超时 10s、UI ready 5s、会话创建 2s） |
| `contract/diagnostics.json` | `export_diagnostics` 响应字段与 13 个诊断段、脱敏规则 |
| `contract/risks.json` | DoD-5 风险登记 8 项（Electron 安全面/孤儿进程/事件顺序/Keychain/体积启动/WebContentsView/契约漂移/双维护），每项含 owner/缓解/触发 |
| `contract/decisions.json` | DoD-6 Linux 范围裁定 + DoD-7 阶段边界裁定（WebContentsView / UtilityProcess） |
| `freeze/electron.json` | Electron 精确 patch（`45.0.0`，禁止范围表达式；安全更新 ≤30 天） |
| `freeze/thresholds.json` | 硬阈值（安装包 ≤1.5× HEAD 且 ≤450MiB、idle RSS ≤500MiB、10 会话 ≤1.2GiB、事件 P95 <150ms） |

## 这套基线**不**声明什么

- `accepted_shape` 只是**合法请求形态的契约文档**，不声明后端执行成功、也不含响应体。
  状态相关命令（`session_*`、`backup_*`、`providers_*` 等）的真实响应由 **M9-04 的 RPC 层 golden** 追加，
  且**只新增、不改本基线的期望值**（P2 计划 M9-04 DoD-3）。
- `rejects` 只收录**确定性可证明**的拒绝：`deny_unknown_fields` 的 `unknown_field`、缺必填字段的
  `missing_field`、类型不符的 `invalid_type`、非对象载荷的 `invalid_json`，以及
  `messages_page` / `audit_query` 的分页与时间窗边界（`out_of_range` / `invalid_value`）。
  语义校验里依赖运行状态的分支（例如按 id 查无）不在此列——它们要真实后端，归 M9-04。
- `contract/responses.json` 冻结的是**响应侧类型清单**，不是按命令的响应 schema：HEAD 的 63 条命令统一返回
  `JsonPayload`（自由 JSON），`ipc/dto.rs` 只有请求 DTO、没有按命令的响应结构体。该文件如实记录
  `commands[].response = {kind:"json", type:null}`，再用「派生 `Serialize` 的类型」把响应里出现的字段与
  可选性冻住（`Option<T>` / `skip_serializing_if` / `serde(default` 三种可选来源逐项标注）。
- 基线不包含安装包体积、RSS、启动耗时的**实测值**以外的数据：体积基线见
  `freeze/thresholds.json#head_baseline`（`dist-out/Aether_0.1.0_x64_en-US.msi`，
  M8-04 批次产物，M9-01 不重打 Tauri 包）。
- `contract/stats.json` 是**录制时点的统计快照**，不随代码变化自动更新；需要复核漂移时跑
  `pnpm stats:m9-01`（重算并覆盖）或 `node scripts/ci/m9-01-stats.mjs --check`（只对比、不写）。

## 校验

```bash
pnpm verify:m9-01-baseline   # 存在性 / 哈希 / 条目数=63 / 计数自洽 / 契约摘要 / 冻结面护栏
```

`verify:m9-01-baseline` 会**重算** `contract_digest`：如果 `commands.rs` / `dto.rs` / `error.rs` /
`event.rs` / `session_backend.rs` / `core_health.rs` / `domain.rs` / `migrations/` 的契约面被改动却没重录基线，
脚本直接失败——这是「禁止静默漂移」的硬闸门。此外还校验响应/事件 payload 形状（`response.type` 恒为 null、
类型 id 唯一、事件 23 类 / MVP 19、字段可选性齐备）、DoD-3 统计关键值、DoD-5 八项风险与 DoD-6/7 两项裁定的完整性。

## 变更规则

1. **M9-04/M9-13 只新增**：可以新增 RPC golden、可以新增对照报告，**不得修改**本目录已冻结的期望值。
2. 任何契约变更（命令增删、错误码增删、事件类型增删、设置键增删、迁移修改）**先走 ADR**，
   升设计文档版本，再重录基线（`pnpm baseline:m9-01:record`）并在证据中记录 diff。
3. `freeze/electron.json` 与 `freeze/thresholds.json` 的数值受 `verify:m9-01-baseline` 硬校验，
   调阈值等于改门禁，必须走评审。

## 重录（仅 M9-01 录制期）

```bash
pnpm generate:error-codes     # 先产出 error_codes.json（契约同源）
pnpm baseline:m9-01:record    # 重录 commands/、contract/、manifest.json
```

重录会改写 `commands/`、`contract/`（不含 `freeze/`）与 `manifest.json`；
`freeze/*.json` 与 `README.md` 为人工冻结文件，重录只对它们取哈希、不覆盖内容。
