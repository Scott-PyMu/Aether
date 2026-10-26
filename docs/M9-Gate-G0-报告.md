# M9 Gate G0 报告（设计基线与迁移门）

| 项 | 内容 |
|---|---|
| Gate | **G0 基线**（ADR-029 §10；P2 计划 §2.15） |
| 对应任务 | M9-01（`docs/P2-架构迁移-实施计划与验收标准.md` v0.5 §2 M9-01） |
| 评审依据 | `docs/ADR-029-评审意见.md` §16（第七轮复核）；本报告为其结论的正式签核 |
| 记录时间 | 2026-10-26T18:16+08:00（Asia/Shanghai） |
| 结论 | **G0 通过（PASS）** |
| 生效 | M9-02（抽取 `aether-app`）可开工；M9-03 仍受其自身进入条件约束 |

## 1. 通过条件逐项核对

| # | G0 条件（P2 §2.15） | 证据 | 判定 |
|---|---|---|---|
| 1 | ADR-029 **v0.4** 接受 | `docs/adr/ADR-029-electron-shell-rust-daemon.md` v0.4（状态行「第三/四轮复审 R1–R17」） | ✓ |
| 2 | 设计文档 **v1.28** | `设计文档.md` v1.28（A4 致命、`business_code`、预算/cursor、安全模式、无回退包） | ✓ |
| 3 | P2 计划 **v0.5** | `docs/P2-架构迁移-实施计划与验收标准.md` v0.5 | ✓ |
| 4 | UI-UX **v0.17** | `docs/UI-UX-设计规格.md` v0.17（S-01 不可达、S-12/S-13、锚点 22） | ✓ |
| 5 | 63/59/4 路由清单 | `docs/M9-01-baseline/manifest.json#commands[63]/counts`；`verify:m9-01-baseline` 对 `collect_commands!` 逐项比对（顺序+名称） | ✓ |
| 6 | 统计脚本 | `scripts/ci/m9-01-stats.mjs` + `docs/M9-01-baseline/contract/stats.json`（63 / 70 invoke / 46 测试 / 1003 bindings 行 / 122 Rust 测试文件）；`--check` 与归档一致 | ✓ |
| 7 | HEAD golden + `manifest.json` + `verify:m9-01-baseline` | `docs/M9-01-baseline/`（63 命令 + 11 contract + 2 freeze + manifest/README = 78 文件，实盘 191,362 字节；勘误：2026-10-26 提交前复核将原「77 文件」更正为 78）；校验器实跑通过 | ✓ |
| 8 | `error_codes.json` + `verify:error-codes`（ipc=32） | `packages/protocol/src/error_codes.json`（ipc 32 + business 9）；校验器实跑通过（diff=0 / schema / 去重 / kind） | ✓ |
| 9 | Electron 精确 patch | `docs/M9-01-baseline/freeze/electron.json`：**45.0.0**、`pin: exact`；护栏扫描全仓 `package.json`（当前 0 处声明，M9-07 首次声明即受约束） | ✓ |
| 10 | 硬阈值冻结 | `freeze/thresholds.json`：安装包 191,717,376 字节（=1.5×HEAD 127,811,584）/ 绝对上限 450 MiB；idle RSS 500 MiB；10 会话 1.2 GiB；窗口 1500 / 握手 2000·10000·retry1 / UI ready 5000 / 会话创建 2000 / P95 150 | ✓ |
| 11 | 风险登记（owner/缓解/触发） | `contract/risks.json`：8 项（Electron 安全面、孤儿进程、事件顺序、Keychain、体积启动、WebContentsView、契约漂移、双维护），三要素齐备 | ✓ |
| 12 | Linux / 阶段 / 打包裁定 | `contract/decisions.json`：`linux_scope`（构建+CI 冒烟，不纳入签名门禁）、`phase_boundary`（WebContentsView PoC 属 P2 迁移使能；UtilityProcess 不构成 MCP 交付）；打包口径见 ADR §8.1/§15-1（per-arch DMG、无回退包） | ✓ |
| 13 | 文档一致性 | `node scripts/test/verify-version-sync.mjs`：PASS 0/0、PASS 1/1、全部通过；`verify:error-codes`、`verify:m9-01-baseline` 通过 | ✓ |
| 14 | 签核 | 本报告 §4（用户授权代签） | ✓ |

## 2. 本次实跑结果（2026-10-26T18:16+08:00，工作区 `E:\Aether`）

```text
$ node scripts/test/verify-error-codes.mjs
[error-codes] 通过：packages/protocol/src/error_codes.json 与重新生成结果一致（ipc=32）
[error-codes] 通过：ipc=32、business=9、schema/去重/kind 合规、生成物 diff=0
AETHER_ERROR_CODES PASS

$ node scripts/test/verify-m9-01-baseline.mjs
[m9-01-baseline] 通过：commands=63（daemon 59 / shell 4，可达 61 + 保留 2）、entries=63、artifacts=13、
响应类型=29、事件 payload=23、risk=8、裁定=2、electron=45.0.0（精确 patch）、阈值全冻结、contract_digest 一致
AETHER_M901_BASELINE PASS

$ node scripts/ci/m9-01-stats.mjs --check
[m9-01-stats] 与归档统计一致。

$ node scripts/test/verify-version-sync.mjs
PASS  期望=0 实际=0  仓库版本一致性（单一来源同步）
PASS  期望=1 实际=1  版本不一致夹具（门禁阻断）
----- 全部通过 -----
```

**环境受限未完成项（不构成 G0 失败）**：`node scripts/ci/bindings.mjs check` 需写 `target/debug`，本沙箱对
`E:\Aether\target\debug\.cargo-build-lock` 拒绝访问（`os error 5`），生成器退出 101。该项属 P0 T14/`protocol check`
的迁移期生成物校验，不是 G0 条件；`error_codes` 生成物已 diff=0，bindings 侧以既有 T14 绿基线与 CI 复跑为准。
建议在 G1 前由 CI/干净环境复跑一次 `pnpm --filter @aether/protocol check` 并归档。

## 3. 条件与风险接受（随 G0 生效）

1. **基线为「契约面 + 响应/事件形状」**：`contract/responses.json`（29 个序列化类型、逐字段可选性来源）与
   `contract/event_payloads.json`（23 类 payload 形状）已录；HEAD 的 63 命令统一返回 `JsonPayload`、无按命令响应 DTO
   （`responses.json#command_envelope` 已说明）。**运行时响应体 golden 由 M9-04 追加**，且必须以
   `manifest.head_commit = 4558d06f…cc2f1` 的源码为期望值，**只新增、不改期望**；不得用抽取后的 `aether-app` 自证。
   风险接受人：架构负责人（`docs/M9-01-证据.md` §12-1）。
2. **Electron 45.0.0 为声明冻结**：当前无 `electron` 依赖声明（`apps/desktop` 仍为 Tauri 开发对照），护栏目 0 命中；
   M9-07 首次声明时由 `verify:m9-01-baseline` 强制精确版本。
3. **安装包基线**取 M8-04 批次产物（2026-10-09，127,811,584 字节，sha256 `49bbef4f…446a9`）；
   M9-11 产出 Electron 安装包后按 G6 复核，超限即 G6 不通过。
4. **`contract/stats.json` 为快照**：发布前用 `node scripts/ci/m9-01-stats.mjs --check` 复核，不作为每次提交硬门禁。
5. **文档维护项（不阻塞 G0，要求 M9-02 证据归档前清理）**：
   - P2 计划 §5.3-6「回退验证」→「失败处理（停止发布/修复/重跑 Gate；无回退包）」；
   - UI-UX §5 `schema_checksum_mismatch` 行「按维护流程从备份恢复」→「退出后手工替换/修复库文件（S-12 无应用内恢复）」；
   - ADR-006（v0.9/v0.10）、ADR-009（v0.3）、ADR-018（v0.4/v0.5）、ADR-023（v0.4）的 §变更记录各补一行
     （头部已升版，变更记录未追加）。

## 4. 签核

| 项 | 内容 |
|---|---|
| 签署结论 | **G0 基线通过（PASS）**，M9-02 可开工 |
| 签署角色 | 项目架构/产品负责人 |
| 签署执行 | AI 评审 Agent **代签**（用户于 2026-10-26 18:16 +08:00 书面授权：「需要人工签署的话，你直接签署，我已授权」） |
| 签署时间 | 2026-10-26T18:18+08:00（Asia/Shanghai） |
| 签署依据 | 本报告 §1 逐项证据 + §2 实跑输出 + `docs/ADR-029-评审意见.md` §16 |
| 未通过项 | 无（5 项文档维护项见 §3-5，不构成 G0 条件） |
| 后续 | G0 通过 ≠ G1–G8 通过；M9-02 完成后按 G1 条件验收（daemon 协议）；签名/公证、Keychain 真机、RSS/体积实测仍阻塞 G5/G6/G7 |

> 备注：本报告只冻结 G0 的「设计基线与迁移门」；M9-02 的实现范围、DoD 与证据要求以 P2 计划 v0.5 与 ADR-029 v0.4 为准。
