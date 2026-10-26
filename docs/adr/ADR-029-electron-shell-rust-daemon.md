# ADR-029：Electron 壳 + Rust aetherd 架构迁移（D1/D2/D7；M9）— v0.4

| 项 | 内容 |
|---|---|
| 编号 | ADR-029 |
| 状态 | **v0.4（2026-10-26，第三/四轮复审 R1–R17 整改；执行中）**；v0.3 = 终局裁定整改；v0.2 = 首轮整改；v0.1 = 初版 |
| 决策日期 | 2026-10-25（v0.1）；首轮整改 2026-10-26（v0.2）；终局裁定 2026-10-26（v0.3）；第三轮复审整改 2026-10-26（v0.4） |
| 决策者 | 项目架构/产品负责人 |
| 评审 | `docs/ADR-029-评审意见.md` 首轮（2×P0/12×P1/10×P2）+ 第二轮（R-P0-1/R-P0-2/R-P1-1…R-P1-5）+ §9 终局裁定 + §11 第三轮（R1–R9）；本版逐项整改，见 §0/§0.1/§0.2 |
| 取代/修订 | **D1 桌面壳**（Tauri 2 → Electron + Rust aetherd）；**D2 核心承载**（Tauri 进程内嵌 → `aetherd` 独立进程）；**D7 UI-核心通道**（Tauri IPC + tauri-specta → JSON-RPC 2.0 over JSONL stdio + Electron IPC + 生成式 TS 客户端）；D1 打包实现要点；§5 P2 退出标准第 4 项 |
| 关联 | ADR-001（identifier/MSI/DMG）、ADR-003/004/005（基线/存储/恢复）、ADR-006/007/009（命令面/启动/health/messages_page）、ADR-010/014/018/028（命令面/权限/多供应商）、ADR-016（事件监听）、ADR-017（暂停恢复）、ADR-020/023（备份/审计） |
| 不改变 | 命令面 **63**；`IpcErrorCode` **32 枚举**；EventEnvelope **9 字段**与附录 B；设置键 **9**；DB schema/迁移 `0001–0009`；适配器线协议 **1.0**；D9 权限网关语义。**新增** `business_code` 字段（不计数，登记见 §4.3） |
| 变更 | UI-UX v0.16 → **v0.17**（R6 启动门/状态图/`db_open_failed`）；设计文档 v1.27 → **v1.28**（R1 DB 可打开/`quick_check` 失败保留安全模式；R7 目录选择器）；P2 计划 v0.4 → **v0.5**（R2/R4/R5/R8/R9）；**无 Tauri 回退包**；`error_codes.json` 双集合；63 命令/32 枚举/9 字段事件/9 设置键/迁移 0001–0009 不变 |
| 文档变更 | `设计文档.md` **v1.28**；`docs/P2-架构迁移-实施计划与验收标准.md` **v0.5**；`docs/UI-UX-设计规格.md` **v0.17**；`docs/P1-实施计划与验收标准.md` **v0.14**（无结构变更）；ADR-010 **v0.11**、ADR-018 **v0.5**、ADR-028 **v0.10**、ADR-006 **v0.10**、ADR-009 **v0.3**、ADR-023 **v0.4**；`AGENTS.md`/`README.md` 同步。需求文档不改 |
| 证据 | `docs/M9-xx-证据.md`（逐任务）；Gate G0–G8 报告；`docs/M9-13-证据.md` 全量回归；安装包哈希/SBOM |

> 本 ADR 是 P2 架构迁移的唯一执行依据。**M9（架构迁移）全部 Gate 通过前，不启动 M10（P2 协作底座功能）**。**无 Tauri 回退包**（用户裁定）：任一门禁失败=停止发布并修复；Tauri 仅开发期对照，G8 直接退役。M9-01 先录 HEAD golden 基线。

## 0. 评审整改对照（v0.2）

> 本节保留首轮整改记录；凡与 §0.1/§0.2 冲突处（P1-1 跨壳锁、P1-9 冻结 Tauri 回退、P1-11 UI-UX v0.15），以 §0.1/R-P1-3、R-P1-5、§0.2/R1 为准：**无回退包、无跨壳锁、UI-UX v0.17**。

> 评审结论「不通过（有条件）：2×P0、12×P1、10×P2」。本版逐项整改；§ 列出落点。

| 编号 | 结论 | 整改落点（v0.2） |
|---|---|---|
| **P0-1** | 2MiB 帧上限与 `messages_page`/`audit_query` 无聚合上限冲突 | §4.1：区分**帧上限**与**响应预算**；`messages_page`/`audit_query` 增 `max_bytes`（默认 1MiB，上限 1.5MiB）+ `has_more`/`next_cursor`；超限只失败本请求、**不重启 daemon**；仅帧损坏/失序断连。ADR-009/023 同步修订 |
| **P0-2** | 关闭预算 ≥8s 与 `shutdown` 5s、T11 10s 冲突 | §3.6：单一退出预算表（daemon ≤8.2s，main 9.5s 强杀，T11 ≤10s）；`shutdown` 立即回执，完成信号 = 进程退出/锁释放；drain 先于 checkpoint |
| **P1-1** | 跨壳锁只覆盖 daemon；允许 `busy_timeout` 降级 | §3.5/§6：daemon OS 独占锁；**禁止** `busy_timeout` 替代。**v0.3 已取消跨壳锁/回退包（R-P1-3）** |
| **P1-2** | 窗口创建在 daemon 预热后，1.5s/2s 冲突 | §3.5：先创建窗口/启动页，再等 daemon；`initialize` 不等待适配器预热；启动口径表（窗口 ≤1.5s、握手 ≤2s、UI ready ≤5s、会话创建 <2s） |
| **P1-3** | 「32 错误码」与业务码混用；`as_str()` 非全集 | §4.3/§16.2：`aether_code` 只取 `IpcErrorCode` 32；业务码走 `business_code`；`error_codes.json` 双集合 + `kind`；G2 验收集合明确 |
| **P1-4** | `already_running`/`protocol_mismatch`/`cancelled` 未登记；预 `initialize` 失败无通道 | §4.3：三类命名空间；`data_dir_locked` 登记为稳定业务码；预 `initialize` 首帧 error + 退出码表；G1 fixture |
| **P1-5** | `params` 是否保留 `{payload}` 未定 | §4.3：**`params` = 内层 DTO，去掉 `payload` 包装**；无参 `omit`/`null`/`{}` 等价；golden fixtures 冻结 |
| **P1-6** | 枚举/能力词表/通知登记缺失；通道名三处不一致 | §4.2/§4.4/§16.4：`DaemonPhase` 枚举；client/daemon capabilities 交集/未知忽略；通道映射 `aether://event`（域）/`aether.event`（方法）/`aether:event`（IPC）；D7 增加通道登记表 |
| **P1-7** | preload 白名单与 `webUtils` 拖拽矛盾 | §5.1/§7：白名单增 `pathForFile(file)`（只返回路径字符串），preload 内用 `webUtils.getPathForFile`；M9-10 断言同步 |
| **P1-8** | 路径复核漏 `backup_create`/`export_diagnostics` | §5.3：所有含路径参数命令的生成式清单；补齐两条；M9-10/M9-13 断言 |
| **P1-9** | 回退对象/可构建性未定义；M9-08 移除 Tauri 依赖 | §9.3：HEAD 自 M9-08 起不再构建 Tauri；G4 改「生产 renderer 无 @tauri-apps」。**v0.3 已取消回退包（R-P1-3）** |
| **P1-10** | 控制队列满即重启 daemon，与 D8 journal 补读冲突 | §4.7：先丢 delta + journal 补读；仅连接重建；仅协议损坏/失序才重启 daemon；连续 N 次补读失败才升级 |
| **P1-11** | 壳级致命页/Keychain 提示未入 UI-UX | UI-UX **v0.16**（v0.15 基线 + A4/S-01 不可达/`data_dir_locked`）：S-12/S-13；锚点/文案/`data-*`；M9-07/M9-09 前置 |
| **P1-12** | G7 复跑含 T14/Tauri 基线不可执行项 | §10.1：P0/P1 门禁映射表（复跑/替代/退役）；T14 → protocol check，M1-08 → M9-10，M4-05/ADR-001 → M9-11 |
| **P2-1** | 决策缺逐条失效条件；风险表缺 owner | §2.1 决策失效条件表；§11 增 Owner 列 |
| **P2-2** | `bindings.ts` 923 → 实测 1003 | §1.1 改 **1003 行（含空行）**；M9-01 用脚本固化统计 |
| **P2-3** | §4.6 与 §16.1 分组计数不一 | §4.6 为唯一路由口径；§16.1 改为路由统计（daemon 59 + shell 4，artifacts 3/startup 2） |
| **P2-4** | `dialog.pick` 路由二义且 M9 无消费方 | §4.5：**仅协议预留**；`ref_pick`/`startup_pick_target` 固定壳本地；59/4 不变；fixture 只验形状 |
| **P2-5** | P2 引入 P3/P4 承载，需求未登记 | §1.2/§14：`WebContentsView` PoC 属 P2 迁移使能，**不构成 UI-06 前移**；`UtilityProcess` 不构成 MCP 交付；M9-01 归档裁定 |
| **P2-6** | universal 打包/嵌套签名/entitlements-Keychain 未决 | §8.1：M9 采用 **per-arch DMG（arm64/x64）**，不做 universal；嵌套签名顺序/entitlements/Team ID 断言；M9-01 冻结策略 |
| **P2-7** | 覆盖率排除 `aether-tauri` 与 T15 关系未登记 | §8.2：排除窗口 M9-02～G8；增量覆盖 + Tauri 删除文件清单；G8 恢复全量并对比 |
| **P2-8** | 锁陈旧/迁移后归属未定义 | §3.5 锁生命周期表：解析→取锁→迁移时**仍持旧锁获取目标锁**→写指针→释放旧锁；崩溃后 OS 锁自动释放；`.aetherd.lock` 不参与复制 |
| **P2-9** | `shell.log` 跨进程/脱敏/不可用场景 | §4.6/§5.4：main 脱敏后落盘+轮转；daemon 只读快照；壳级致命页提供**离线导出诊断** |
| **P2-10** | debug 探针处置未定义 | §7/§8.2：Tauri 探针退役；Playwright + `aetherd` `test-hooks`（release 不注册、manifest 排除） |

> 未关闭项：证书/公证凭据、Linux 发布签名、Electron 精确 patch、Keychain 真机可读性、体积/RSS 阈值实测——见 §15；这些是证据/发布前置，不阻塞 M9-02/M9-03 的代码开工，但阻塞对应 Gate。

### 0.1 第二轮复审/终局裁定整改（v0.3）

> 依据 `docs/ADR-029-评审意见.md` §8（R-P0-1/R-P0-2/R-P1-1…R-P1-5）与 §9 用户终局裁定；本节覆盖 §0 中与之冲突的旧表述。

| 编号 | 终局裁定 | v0.3 落地 |
|---|---|---|
| **R-P0-1** | **A4 阻断 = 致命退出，不提供迁移入口**（未发布/内测数据可丢） | §3.5-7 改为致命/S-12；A4 → 首帧 error（退出码 21）→ S-12；§4.2 预初始化桶不含 A4/迁移；§4.3 退出码 23 标注「M9 不触发」；设计 v1.27 A4/D7/T13、UI-UX v0.16 S-01/S-01a、P2 M9-03/07/13/G1/G5 同步 |
| **R-P0-2** | **业务码走 `error.data.business_code`**，`aether_code` 只取 32 枚举 | §4.3/§16.2 改为双字段；`error_codes.json` 双集合（`ipc`/`business`）；`data_dir_locked` 首登记；ADR-018 A-7 增行；UI-UX v0.16 §5 判定顺序 |
| **R-P1-1** | 预算/cursor 按评审建议定稿 | §4.1/§4.7：`max_bytes` 64KiB–1.5MiB；**至少 1 条**；`next_cursor` 不透明 `{v,stream,pos,filter_hash}`；`messages_page` cursor 仅 events + `messages_has_more`；`audit_query` cursor/offset 互斥；G2「0 差异」仅未改契约命令 |
| **R-P1-2** | `initialize` 2s 是目标，不是硬超时 | §3.5：目标 ≤2s、硬超时 ≥10s；超时先重连/重试一次，再 S-12；指标记录超时率 |
| **R-P1-3** | **取消回退包**（无旧版本可回退） | §9.3/D-029-12/§10 G4–G8/§10.1/§3.5/§6/M9-09/M9-11/M9-14：删除冻结包/重建/跨壳回退；失败=停止发布并修复；Tauri 仅开发对照，G8 直接退役；旧内测包手工卸载（文档说明） |
| **R-P1-4** | `error_codes.json` 宿主/CI | `packages/protocol/src/error_codes.json`（与 types/client/manifest 同宿主、禁手改）；`pnpm verify:error-codes`：diff=0、ipc=32、business 集合/去重/kind；AGENTS §2.8/§8/§5、ADR §7/§8.2、P2 M9-04/§5.1 同步 |
| **R-P1-5** | UI 新契约 | UI-UX **v0.16**：`data_dir_locked` 文案/锚点、预算续页 UX（自动/手动/终止/已到最新）、S-12 渲染载体（独立最小 renderer）+ 数据目录路径/手动移动说明；M9-08 增断言 |
| **§9.4-1** | golden 基线必须先录 | M9-01 证据包含 `docs/M9-01-baseline/`：63 命令合法/非法/错误/分页边界 golden、32 枚举 + 业务码现状、messages/audit 样本、启动/关闭时序、事件样本；**录完才开工 M9-02**；M9-04/13 只新增不改期望 |
| **§9.4-2** | 数据可丢 ≠ 可改冻结项 | `migrations/0001–0009`、63 命令、32 枚举、9 字段、9 设置键、附录 B 不变；business_code 只是新增字段；§15 增「发布前重评数据兼容/回退策略」 |

**放行顺序**：录基线（§9.4-1）→ M9-02 抽层 → v0.4/v1.28/v0.5/v0.17 文档落地 → G0 复核 → M9-03。

### 0.2 第三轮复审整改（v0.4）

> 依据 `docs/ADR-029-评审意见.md` §11（1×P1 + 9×P2）。R1 采用 §11.4-R1 **建议①（保留安全模式）**；其余按建议落地。

| 编号 | 结论 | v0.4 落地 |
|---|---|---|
| **R1** | `quick_check` 失败≠致命：仅 **DB 无法打开**才致命 22；打开成功但校验失败 → **只读安全模式** + `persist_degraded` + S-08 + 导出/备份 | §3.5-4/6、§4.2、§4.3、§10 G1；设计 v1.28 D2/D3/D7；P2 M9-03/M9-13/G1/G5；UI-UX v0.17 S-08/S-12/§5 |
| **R2** | 分开「命令合同 63/63」与「UI 暴露 61/63」 | §10 G2；P2 M9-04 DoD1/2/3、G2、§4；M9-04 参数矩阵恢复 63 条（RPC 层） |
| **R3** | ADR-006/009/023 升版并补 `filter_hash` 重校验句 | ADR-006 **v0.10**、ADR-009 **v0.3**、ADR-023 **v0.4**；ADR-009 §9/ADR-023 A-4 |
| **R4** | M9-01 只做生成器骨架 + `ipc=32`；business 集合校验 M9-03 起 | P2 M9-01 DoD4/10、M9-03 DoD4、G0；`verify:error-codes` 分阶段 |
| **R5** | M10-00 版本引用不得写死当前版本 | P2 M10-00 DoD1 改「届时的下一版本（当前 v1.28 / v0.5）」 |
| **R6** | UI-UX 启动门/状态图/§5 清理 | UI-UX v0.17：M9 无启动门、S-01/S-01a 标不可达、拆 `startup_blocked`/`sync_dir_blocked`、补 `db_open_failed` |
| **R7** | `startup_pick_target` 仍是通用目录选择器 | 设计 v1.28 D7、P2 M9-01 DoD2/M9-06 DoD3 注明：备份/导出/审计/归档/工作区复用；仅不用于启动迁移 |
| **R8** | 基线包补 manifest/比对工具 | P2 M9-01 DoD9：`manifest.json` + `verify:m9-01-baseline`；G0 增校验 |
| **R9** | 体积/RSS 阈值冻结为硬阈值 | M9-01 冻结；P2 M9-11/M9-13/G6/G7、§8、ADR §15-10 去掉「建议/评审」歧义 |

### 0.3 第四轮复审整改（v0.4 closure，R10–R17）

| 编号 | 结论 | 落地 |
|---|---|---|
| **R10** | 采用 **A（推荐）**：安全模式允许 `backup_restore` 维护例外 | §3.5-6；设计 D4/D13；UI-UX S-08/S-06；P2 M9-13 DoD4/G5 |
| **R11** | 已应用迁移 checksum 不符 → **致命 22 + `schema_checksum_mismatch`**；迁移执行失败回滚+安全模式（M9 不触发） | §3.5-4、§4.3；设计 D3；ADR-018 v0.5 A-7；ADR-006 v0.10；UI-UX S-12/§5；P2 M9-03/G1 |
| **R12** | 握手指标含 DB 打开（不含 `quick_check`/预热） | §3.5 指标表 |
| **R13** | UI-UX S-01 信息架构/启动门锚点标 M9 不可达 | UI-UX §1.1/§7.2 |
| **R14** | CI 增 `verify:m9-01-baseline` | §8.2；G0 |
| **R15** | 证据模板去掉「回到 Tauri」 | §16.5-6 |
| **R16** | Linux 签名改「P2 之后的阶段（另立计划）」 | §8.1/§11/§15-1 |
| **R17** | P1 版本表述改为「ADR-029 v0.4 无需 P1 计划变更（保持 v0.14）」 | §14；AGENTS §1/§8 |

## 1. 背景与触发

### 1.1 当前架构

- 桌面壳：Tauri 2.11.5；`tauri-plugin-single-instance` + `tauri-plugin-dialog`；MSI/DMG 由 Tauri bundler 产出（ADR-001）。
- 核心：5 个 crate（`aether-core/store/adapters/control/security`）共约 **3.77 万行**，**不依赖 Tauri**；`aether-tauri` 约 **2.29 万行**，含 **63 条对外命令**、事件桥、启动/退出、备份/审计/供应商等应用层。
- 前端：React 18 + Vite 5，约 **2.27 万行**；Tauri API 只有 **70 处 `invoke`** 与 **1 处** `getCurrentWebview().onDragDropEvent`；事件经 `packages/protocol/src/bindings.ts`（**1003 行（含空行）**，tauri-specta 生成；M9-01 用脚本固化统计）。
- 测试：Rust 122 个测试文件/5.34 万行，其中约 9 个使用 `tauri::test`；前端 46 个测试文件；E2E 使用真实 WebView2 + debug probe。
- 数据：SQLite WAL，单写队列；数据目录 `%APPDATA%\Aether` / `~/Library/Application Support/Aether`；迁移 `0001–0009`。

### 1.2 触发条件（均已成立）

1. **UI-06 内嵌浏览器（P4；用户 2026-10-25 裁定 B 形态）**：单窗口内可交互浏览器 / 多 WebView，Agent 需要浏览、分屏、多标签。Tauri 2 的多 WebView 仍在 `unstable` feature flag 后，官方 issue #2975 明确「短期内没有计划」；Electron 的 `WebContentsView` + `BaseWindow` 是成熟官方方案。
2. **macOS WKWebView / Linux WebKitGTK**：与 Windows WebView2 存在渲染/性能差异；Tauri CEF 仍是实验/分支路径（已有应用试用，但社区有迁回案例），不能作为生产承诺。
3. **Electron 生态 / Node / MCP**：P3 的 MCP 接入、P4 的浏览器自动化、以及未来 Node 库，Electron 的 `UtilityProcess`/Node 主进程提供现成承载；Tauri 需要额外 Node sidecar。
4. **P2 `aetherd` / HTTP/WS / CLI 与 P5 SDK**：D2 的失效条件（核心需独立重启/外部程序共享核心）与 D7 的失效条件（出现非 UI 消费者）已触发；P5 的 SDK/CLI/IDE 客户端需要 headless 核心。

### 1.3 设计文档自身的失效路径

本 ADR 不是对冻结决策的任意反转，而是执行设计文档已写明的失效路径：

- **D1 失效条件**：「出现必须依赖 Chromium 独有能力且无法用独立浏览器进程替代的需求；或 WebView 系统性不可用……届时切 Electron，仅替换 `aether-tauri` 与打包，核心 crate / 线协议 / 数据零改动。」
- **D2 失效条件**：「出现『核心需独立重启』或『外部程序共享核心』的真实需求（≥3 次）→ 触发 P2 aetherd 迁移：同一核心库换承载，线协议与 IPC 契约不变。」
- **D7 失效条件**：「出现非 UI 消费者（脚本/CLI/浏览器插件）→ P2 aetherd + HTTP/WS；届时本决策保留为『UI 专用通道』，API 为并行新增而非替换。」

### 1.4 行业先例（用于验证模式，不用于替代本仓库决策）

- **OpenAI Codex**：Desktop 是 Electron；核心是 Rust `codex app-server`；**JSON-RPC 2.0 over JSONL stdio**；同一 App Server 同时服务 CLI、VS Code 扩展、Web、macOS 桌面端与第三方 IDE；Rust 侧可 `generate-ts` 生成 TypeScript 类型；权限确认使用 **server-initiated request**。这是本 ADR 的直接参照。
- **OpenCode 2.0**：Desktop 从 Tauri 迁到 Electron；原因是其核心/服务全是 TypeScript、Tauri 只是包在 `opencode serve` 外面的薄壳，且 macOS/Linux WebKit 渲染不一致。它验证了「TS-only + 需要 Chromium 一致性 → Electron」，**不适用于** Aether 的 Rust 核心路径。
- **Goose**：Rust 核心 + React UI，现为 Electron，官方路线图正在迁 Tauri；说明「Rust 核心 + 轻壳」也可以走 Tauri，但其 UI 没有 UI-06 B 类多 WebView 约束。Aether 选择 Electron 是为了 B，而不是否定 Tauri。

## 2. 决策

- **D-029-1 桌面壳**：采用 **Electron**（main + preload + React renderer），锁定精确版本；Tauri 仅保留为迁移期开发对照，G8 后退役。
- **D-029-2 核心承载**：新增 **`aetherd`** Rust 独立进程，承载全部业务逻辑、SQLite、事件管线、权限网关、备份/审计、适配器监督；Electron 主进程只做窗口/生命周期/原生对话框/浏览器视图/Node 服务托管。
- **D-029-3 UI-核心通道**：`aetherd` 与 Electron main 之间使用 **JSON-RPC 2.0 over JSONL stdio**；renderer 只经 preload 的白名单 API 访问 main；不使用 localhost 端口。
- **D-029-4 命令面**：**63 条命令名称/参数/返回保持向后兼容**；其中 **59 条由 `aetherd` 处理**，**4 条为壳本地命令**（`app_exit`、`app_restart`、`ref_pick`、`startup_pick_target`）。**已登记例外**：`messages_page`/`audit_query` 增可选 `max_bytes`（64KiB–1.5MiB，默认 1MiB）/至少 1 条/不透明 `next_cursor`/`messages_has_more`；audit cursor/offset 互斥；错误 data 新增 `business_code`。旧客户端省略 `max_bytes` 按默认。命令路由清单由代码生成并在 CI 校验。
- **D-029-5 事件**：单通道 `aether://event`（域）/ `aether.event`（协议）/ `aether:event`（Electron IPC）；EventEnvelope 9 字段、会话内 `seq` 单调唯一、先写日志后广播全部不变；`aetherd` 以 JSON-RPC notification 推送，main 按序转发 renderer，UI 仍以 `last_seq` 补读。
- **D-029-6 权限**：D9 网关、300s 超时、审计、零直通全部留在 Rust；Electron main 只是传输代理，不做权限判定。
- **D-029-7 浏览器**：UI-06 使用 `WebContentsView` + `BaseWindow`；独立 session partition、sandbox、无 preload/Node、权限/导航/下载/证书拦截；与主 UI 不共享存储与 IPC。**M9 的 PoC 属 P2 迁移使能，不构成 UI-06 阶段前移；完整交互仍 P4。**
- **D-029-8 Node/MCP**：Node/MCP 服务放 **`UtilityProcess`** 或独立 Node sidecar，**不放入 Electron main**；一个 MCP 服务只设一个 owner，避免重复连接。**M9 的 `UtilityProcess` 是通用承载，不构成 MCP（P3）交付。**
- **D-029-9 数据兼容**：数据目录、DB schema、迁移、备份/恢复、设置键、密钥引用全部不变；**M9 不做数据迁移**；daemon 对 `<data_dir>/.aetherd.lock` 取 OS 独占锁；**无 Tauri 回退包/跨壳锁**；内测旧包手工卸载。
- **D-029-10 打包**：`electron-builder` 产出 MSI/DMG；M9 **macOS 采用 per-arch DMG（arm64/x64），不做 universal**；`aetherd` 与 `runtime-bundle` 作为 `extraResources`（不进 asar）；保持 appId `dev.aether.desktop`、产品名 `Aether`、签名身份连续；**不做老 Tauri MSI 升级链**（未发布过旧版本）。
- **D-029-11 迁移顺序**：先 **M9 架构迁移**（G0–G8），再 **M10 P2 协作底座功能**；M9 Gate 未过，不启动 M10。
- **D-029-12 失败处理**：**无回退包**；HEAD 自 M9-08 起不再构建 Tauri；任何 G4–G7 失败=停止发布并修复，可保留上一 Electron 候选构建；不以「部分迁移」发布。

### 2.1 决策失效条件与 Owner（评审 P2-1）

| 决策 | 失效条件（触发即回退/停止） | 动作 | Owner |
|---|---|---|---|
| D-029-1 Electron 壳 | G4 安全基线失败；G6 无法签名/安装；WebContentsView 阻断性缺陷 | **停止发布；修问题**（无回退包） | 壳/安全 |
| D-029-2 aetherd 承载 | daemon IPC RTT >50ms 且不可优化；or 双进程生命周期无法收敛 | 评估 in-process 宿主插件（需 ADR）；**不得回装 Tauri** | 核心 |
| D-029-3 stdio 传输 | 帧/背压/重连不可证明；M10 HTTP/WS 需要替代 UI 通道 | 仅 UI 通道保留 stdio；API 并行；必要时另立 ADR | 协议 |
| D-029-4 63 命令 | G2 任一命令不可达/参数矩阵不等价 | 停止 G2；修 `dispatch`；禁止豁免 | 应用层 |
| D-029-5 事件 | seq/顺序/补读/10×50 控制丢失 | 停止发布；修 transport；不得回装 Tauri | 事件 |
| D-029-6 权限 | 零直通/300s/审计任一失败 | 停止 G3/G5；修网关；不得回装 Tauri | 安全 |
| D-029-7 WebContentsView | 隔离/权限/导航/证书任一失败 | 关闭 PoC；UI-06 仍 P4；不影响 M9 主链 | 浏览器 |
| D-029-8 UtilityProcess | 出现两个 MCP owner 或重复连接 | 只保留一个 owner；P3 再落地 MCP | Node/MCP |
| D-029-9 数据/锁 | 旧库不可开/双开/Keychain 不可恢复 | 停止发布；修锁/Keychain | 数据 |
| D-029-10 打包 | 安装/签名/公证失败/体积超阈值 | 停止发布；修打包（无旧 MSI 升级链） | 发布 |
| D-029-11 顺序 | G8 未过即启动 M10 | 立即停止 M10；回 M9 门禁 | 计划 |
| D-029-12 失败处理 | 任一 Gate 失败/出现「部分迁移」 | 停止发布；修复后重跑；**无回退包** | 发布 |

## 3. 目标架构

### 3.1 组件图

```text
┌──────────────────────────── Electron main（薄） ────────────────────────────┐
│  app lifecycle / single-instance / BrowserWindow / native dialog            │
│  WebContentsView（UI-06 浏览器，独立 partition）                            │
│  UtilityProcess（Node MCP / Node 库，按需）                                 │
│  preload：contextBridge 白名单（rpc / onEvent / offEvent / version）         │
└───────────────────────────────┬──────────────────────────────────────────────┘
                                │ JSON-RPC 2.0 over JSONL stdio
┌───────────────────────────────▼──────────────────────────────────────────────┐
│ aetherd（Rust，独立进程）                                                     │
│  aether-rpc（JSONL framing / 2MiB 上限 / initialize / error）                 │
│  aether-app（63 命令 router、DTO、严格校验、错误映射、IpcState、AppHost）       │
│  aether-core / store / adapters / control / security（业务不变）              │
│  EventPipeline（先日志后广播）/ PermissionService / Backup / Audit            │
│  AdapterSupervisor（Job Object / 进程组 / PID 台账 / 心跳 / 重启）            │
└───────────────────────────────────────────────────────────────────────────────┘
```

### 3.2 职责边界

| 层 | 负责 | 不负责 |
|---|---|---|
| React renderer | UI、EventStore、本地视图状态、用户输入 | 业务规则、DB、密钥、进程管理、权限判定 |
| Electron main | 窗口/生命周期/单实例/原生 dialog/`WebContentsView`/`UtilityProcess`/传输代理 | 业务状态、SQLite、适配器、审计、权限决策 |
| `aetherd` | 全部业务、数据、事件、权限、备份、审计、适配器监督 | 窗口、DOM、浏览器 UI、系统托盘 |
| 适配器 | 运行时协议、CLI 子进程、流式输出 | 核心状态、DB、UI |

**单一事实源**：`aetherd` 是会话/运行/权限/设置/备份/审计的唯一事实源；Electron main 不持有业务状态，renderer 只是投影。禁止 main 与 daemon 各自维护一份会话状态。

### 3.3 crate 与目录布局

```text
crates/aether-rpc        # 新：JSON-RPC 2.0 类型 + JSONL framing + 2MiB 上限 + initialize
crates/aether-app        # 新：transport-agnostic 命令 router / DTO / 校验 / IpcState / AppHost
crates/aether-daemon     # 新：aetherd bin + stdio transport + 生命周期
crates/aether-tauri      # 迁移期：Tauri adapter（依赖 aether-app；G8 后退役）
crates/aether-core       # 不变
crates/aether-store      # 不变
crates/aether-adapters   # 不变（线协议 1.0）
crates/aether-control    # 不变
crates/aether-security   # 不变
apps/desktop/electron    # 新：main / preload / builder 配置
apps/desktop/src         # 现有 React renderer（改传输层，不改 UI 规格）
packages/protocol        # 生成式 TS 类型/客户端（替代 bindings.ts）
```

依赖方向（修订 AGENTS §2.1）：

```text
aether-core
  → aether-store / aether-adapters / aether-control / aether-security
  → aether-app
  → aether-daemon / aether-tauri（迁移期）
aether-rpc 为叶子基础设施，可被 aether-daemon / aether-tauri / aether-adapters 使用
```

- `aether-app` **不得**依赖 Tauri、Electron、Node、窗口/对话框实现。
- `aether-daemon` **不得**依赖 `aether-tauri`。
- `aether-tauri` 只做 Tauri 适配（命令注册、dialog、退出），业务全部调用 `aether-app`，不得复制命令逻辑。
- 迁移期允许 `aether-tauri` 存在；G8 后从默认构建与 workspace 移除（或降级为 `dev-only` 对照 crate，需另立 ADR）。

### 3.4 `AppHost` 抽象

`aether-app` 通过 trait 访问壳能力，避免把 Tauri/Electron 类型带进业务层：

```rust
pub trait AppHost: Send + Sync + 'static {
    fn pick_path(&self, kind: PickKind) -> Result<Option<PathBuf>, PickError>;
    fn request_exit(&self) -> Result<(), HostError>;
    fn request_restart(&self) -> Result<(), HostError>;
    fn shell_info(&self) -> ShellInfo; // 版本、平台、日志路径；用于诊断包
}
```

- Tauri 实现：`tauri-plugin-dialog` + `AppHandle`（迁移期）。
- Electron 实现：通过 server-initiated JSON-RPC `dialog.pick` / `app.exit` / `app.restart` 回到 Electron main。
- CLI/测试实现：固定路径 / 立即退出 / 固定信息。
- **M9 路由口径**：4 条壳本地命令（`app_exit`、`app_restart`、`ref_pick`、`startup_pick_target`）由 **Electron main 直接处理**，不进入 daemon；其余 59 条进入 `aether-app::dispatch`。`AppHost` 为未来 CLI/HTTP 客户端预留，M9 不强制走它。
- **`dialog.pick` 仅协议预留**：M9 的 `ref_pick`/`startup_pick_target` 固定壳本地，**不**改走 daemon 路由；59/4 计数不变。M9-06 只交付协议形状 fixture（正常/超时/取消/迟到丢弃），不接产品路径；未来 CLI 若需要选择器再启用。

### 3.5 启动序列

> **P1-2 修正**：窗口/启动页先于 daemon ready 创建；`initialize` 只表示「进程可用 + 数据目录/阶段可读」，**不等待适配器预热**。

1. Electron `app.requestSingleInstanceLock()`；失败则聚焦已有窗口并退出。
2. main 创建 `BrowserWindow` 并加载启动页（S-00），窗口可见即计入启动指标；此时不等待 daemon。
3. spawn `aetherd --stdio`，`stdio: ['pipe','pipe','pipe']`，`windowsHide: true`；main 持续 drain stderr（避免 pipe 满阻塞 daemon）写平台临时日志；数据目录解析后转写 `<data_dir>/logs/aetherd.log`。
4. daemon 在读取 `initialize` 前完成**致命启动检查**：解析数据目录 → 获取 OS 文件锁（`<data_dir>/.aetherd.lock`，独占句柄）→ **A4 检测** → **DB 打开**（不含 `quick_check`）→ **已应用迁移 checksum 校验**（不匹配 → 22 + `schema_checksum_mismatch`）。失败写唯一首帧 error（`business_code` + 退出码 20/21/22/24）并退出；main 显示 S-12。`db_open_failed` **仅指无法打开**（缺失/权限/加密头损坏）；`schema_checksum_mismatch` 指已应用迁移与文件不符（R11）。**M9 不执行迁移。**
5. `initialize` 握手：**目标 ≤2s、硬超时 ≥10s**；协议/app/daemon/core 版本、数据目录、`DaemonPhase`、capabilities；**不等待适配器预热**。超时先断开重连/重试 1 次，仍失败 → S-12；记录超时率。
6. **`quick_check` 在 DB 打开后执行**：失败 → **只读安全模式**（`persist_degraded` + S-08；可读数据导出 + 从备份恢复；不预热适配器、不接受写/新 run），**不退出进程**；成功 → 孤儿清理 → 适配器后台预热。**维护例外（R10-A）**：安全模式允许 `backup_restore`——先停写/关闭连接、恢复后由 main 重启 daemon（复用 §3.6 关闭序列），不视为新写入。向 main 推送 `daemon.status`；renderer 用 `health` 呈现进度/降级。
7. **A4 阻断 = 致命**：已在第 4 步退出；main 显示 **S-12**（重试/退出/离线导出诊断 + 数据目录路径/手动移动说明），**不出现 S-01/S-01a、不提供迁移按钮**；`startup_get`/`startup_migrate` 命令保留在 63 内但 M9 不暴露、不验收。
8. daemon 无法启动/握手失败：main 显示 S-12（含日志路径、离线导出诊断、重试、退出），不加载主 UI。

**启动指标口径（M9-09/G7）**：

| 指标 | 口径 | 目标 |
|---|---|---|
| 窗口可见 | 进程启动 → BrowserWindow `ready-to-show`（含启动页，不含 daemon/适配器） | ≤1.5s |
| daemon 握手 | spawn → `initialize` 响应（含数据目录解析/锁/**DB 打开**，不含 `quick_check`/预热） | **目标 ≤2s；硬超时 ≥10s** |
| UI ready | renderer 可交互 + 首个 `health` 成功（预热可后台继续） | ≤5s |
| 首个会话创建 | UI ready → `session_create` 完成 | <2s |
| 适配器预热 | 后台；`health.runtimes` 显示 `starting/cold`，不阻塞窗口 | 按 D5 预算 |

**锁生命周期（P2-8；M9 不做迁移）**：

| 阶段 | 行为 |
|---|---|
| 启动 | 解析数据目录 → 获取 OS 独占锁（handle-based；崩溃由 OS 自动释放） |
| A4 阻断 | **致命**：首帧 error + 退出码 21 + S-12；不提供迁移入口 |
| 迁移 | **M9 不触发**（`startup_migrate` 不暴露；代码/命令保留，后续版本另立 ADR 再验收） |
| 崩溃 | OS 释放句柄；无「存在性锁」陈旧问题；`.aetherd.lock` 不参与数据复制 |
| Tauri | 无回退包；Tauri 仅开发期对照，G8 直接退役；旧内测包手工卸载（文档说明） |

### 3.6 关闭序列

> **P0-2 修正**：`shutdown` 请求**立即回执**（`{accepted:true, shutdown_id}`），回执 ≠ 关闭完成；完成信号 = daemon 进程退出 + 锁释放。单一预算表如下，drain **先于** checkpoint。

| 阶段 | 预算（硬超时） | 说明 |
|---|---:|---|
| 停止新工作 / 广播 shutdown | 0.2s | 拒绝新 run/命令 |
| 适配器 `shutdown` RPC（并行，best-effort） | 3.0s | 超时进入强杀；不再单独等 5s |
| 进程组/Job Object 整树回收 + 校验 | 1.0s | `TerminateJobObject` / `kill -KILL -pgid` |
| 写队列 drain（控制事件保留；delta 可放弃） | 2.0s | 保证已确认消息落盘 |
| 关闭全部读连接 | 0.2s | checkpoint 前必须无读锁 |
| `wal_checkpoint(TRUNCATE)` | 1.5s | 失败记录诊断但不阻塞退出 |
| 关闭写连接 / 释放锁 / exit | 0.3s | 锁由 OS 句柄释放 |
| **daemon 合计** | **8.2s** | 阶段超时继续下一阶段 |
| main 等待进程退出 | 9.5s | 到点强杀进程树 |
| **T11 总上限** | **≤10s** | 进程退出/无孤儿 |

1. Electron `before-quit` 拦截；发送 `shutdown` RPC，收到 `accepted` 后继续等待进程退出。
2. daemon 按上表执行；任一阶段超时进入下一阶段，最终 `exit`。
3. main 9.5s 未退出 → Windows `taskkill /PID <pid> /T /F`；Unix `SIGTERM` → 0.5s → `SIGKILL`（进程组）；10s 内完成。
4. daemon 释放数据目录锁并退出；main 退出。
5. **反向孤儿保护**：daemon 监视 stdin EOF；Electron 崩溃/被强杀时，stdin 关闭触发 daemon 优雅关闭；daemon 再按 D5 回收适配器。
6. Windows：daemon 为适配器创建 Job Object（`KILL_ON_JOB_CLOSE`）；daemon 句柄关闭即整树回收。Unix：适配器独立进程组；daemon 退出时按 D5 终止；Linux 可加 `PR_SET_PDEATHSIG`，macOS 依赖启动台账清理。

**故障注入（M9-09/G5）**：适配器不响应 `shutdown`、checkpoint 卡住、写队列阻塞、daemon 卡死、Electron 先崩；断言 10s 内退出、无孤儿、已确认消息零丢失、锁释放。

### 3.7 崩溃恢复

| 故障 | 行为 | 恢复 |
|---|---|---|
| daemon panic/exit | main 标记 `core_not_ready`，显示横幅；保留窗口 | 退避 1/2/4/8/16/30s 重启；60s 内 ≥5 次 → 停止自动重启，显示手动重试 |
| renderer 崩溃 | main 重建窗口；daemon 不受影响 | UI 重新 `session_list` / `messages_page` 同步 |
| Electron 崩溃 | stdin EOF → daemon 关闭；适配器回收 | 下次启动孤儿清理 + DB WAL 恢复 |
| `WebContentsView` 崩溃 | 重建该 view；主 UI 不受影响 | 重新加载 URL / 恢复浏览器会话 |
| 控制投递队列满 | **不重启 daemon**：丢 delta + 切 journal 补读 + 诊断计数；必要时仅重建 transport 连接 | UI 用 `last_seq` 补读；连续 N 次（默认 3）补读失败才升级为 daemon 重启并审计 |
| daemon 重启后 | 从 SQLite/事件日志 reconcile；在途 run 按现有失败收口 | UI 重新拉取会话/运行/权限 |

## 4. 协议设计

### 4.1 传输与帧（P0-1 / P1-10 修正）

- JSON-RPC 2.0 over **JSON-Lines（UTF-8，一行一个 JSON 对象）**，走 `aetherd` 的 stdin/stdout；stderr 只放日志。
- **帧上限 2MiB**：任一 JSON-RPC 请求/响应/通知帧 ≤2MiB；`artifact_ref` 引用帧 <1MiB；读取器有界（超限立即拒绝，不整行读入内存）。
- **命令响应预算**（与帧上限分离；R-P1-1 定稿）：
  - `max_bytes`：默认 **1MiB**、上限 **1.5MiB**、下限 **64KiB**；越界 → `out_of_range`；旧客户端省略按默认。
  - **最小推进**：每页**至少返回 1 条**（允许略超预算）；整帧仍须 ≤2MiB，否则 `too_large`（提示减小 `limit`/`max_bytes`），**不断连**。
  - `next_cursor`：**不透明 base64url**，内容 `{v:1, stream, pos, filter_hash}`；客户端不得解析/构造；服务端每次重算并校验 `filter_hash`，不匹配 → `invalid_value`（防篡改/越权）。
  - `messages_page`：`next_cursor` **仅作用于 events 补读流**；另给 `messages_has_more`（首页 `messages` 被截断时 true）；`complete`/`last_seq`/`readback_gap_too_large` 语义不变。
  - `audit_query`：`next_cursor` 与 `offset` **互斥**（同时传 → `invalid_value`）；`has_more` = 「还有更多行 **或** 被预算截断」；`limit` 仍 ≤500。
  - ADR-009 §9、ADR-023 A-4 同步；旧字段向后兼容。
- **超限处置**：请求/响应超预算 → 仅本请求返回 `too_large`（`data.aether_code`），**不重启 daemon**；仅帧损坏/无法解析/失序等协议错误才断连，main 重建连接（不杀在途 run）。
- 不使用 localhost HTTP/WS；无端口、无 CORS、无防火墙提示。M10 的 HTTP/WS 是**并行新增**，另立鉴权设计，不替换 stdio。
- G2/G3 增 golden fixture 与故障注入：单帧 2MiB 边界、>2MiB 行、`messages_page`/`audit_query` 预算分页/零推进/游标篡改、超限不重启、断连重连续读。**M9-04 的「0 差异」仅适用于未改契约命令；预算字段/截断行为列为已登记差异（基线 §9.4-1）。**

### 4.2 `initialize` 握手

```json
// request
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocolVersion":"1.0",
  "appVersion":"0.1.0",
  "client":"electron",
  "clientCapabilities":["events","permissions","dialogs","browser-view"],
  "requiredCapabilities":["events","permissions"]
}}
// response
{"jsonrpc":"2.0","id":1,"result":{
  "protocolVersion":"1.0",
  "daemonVersion":"0.1.0",
  "coreVersion":"0.1.0",
  "dataDir":"C:\\Users\\...\\AppData\\Roaming\\Aether",
  "phase":"starting",
  "daemonCapabilities":["events","permissions","dialogs"]
}}
```

- `initialize` 必须是连接上的第一个请求；之前任何业务请求 → `core_not_ready`。
- major 不匹配 → 传输错误 `-32001 protocol_mismatch`（`data.reason`），main 拒绝加载并提示版本不一致；minor 允许增量字段。
- **能力协商**：`clientCapabilities` 与 `daemonCapabilities` **取交集**为共享能力；未知项忽略 + 诊断记录；`requiredCapabilities` 未满足 → `-32006 capability_missing`；`browser-view` 是 **client-only** 能力，不要求 daemon 声明。
- **`phase` 枚举**（`DaemonPhase`）：`starting` / `detecting` / `opening` / `warming` / `ready` / `degraded` / `shutting_down` / `failed`；`migrating`/`blocked` 保留为未来值（M9 不产生）。`daemon.status` 使用同一枚举。
- **`DaemonPhase` ↔ `startup_get.phase` 映射（R-P2-1）**：`starting/detecting/opening/warming` → `startup_get.phase=ready`（过渡态，`detail.phase` 给细值）；`ready` → `ready`；`degraded` → `ready`（`health.storage_state=persist_degraded`，S-08）；`failed` → `blocked_error`；未来 `blocked` → `blocked_sync_dir`。M9 的 A4 走 S-12，不产生 `blocked_sync_dir`。
- app/daemon/core 版本由同一 workspace 版本注入；`initialize` 返回实际版本，main 记录并在诊断包中展示。
- 重复 `initialize` → `invalid_request`。
- **预 `initialize` 失败**（数据目录锁/A4/**DB 无法打开**/内部）：daemon 写唯一首帧 `{"jsonrpc":"2.0","id":null,"error":{"code":-32005,"message":"...","data":{"aether_code":"internal|startup_blocked","business_code":"data_dir_locked|sync_dir_blocked|db_open_failed|schema_checksum_mismatch","phase":"...","log_path":"..."}}}`，随后按 §4.3 退出码表退出；main 读到首帧即显示 S-12。**M9 不把迁移失败归入此桶**；**`quick_check` 失败不属此桶**（只读安全模式，见 §3.5-6）。

### 4.3 请求、响应与错误

- `method` = 63 条命令的**原名**（如 `session_create`）。
- **`params` = 内层请求 DTO，去掉 Tauri `{ payload: ... }` 包装**（P1-5 裁定）。无参方法：`params` 省略、`null`、`{}` 三者等价；有参方法不得再包一层 `payload`。前端 typed client 负责从现有调用形态转换；M9-03/M9-04 用 golden fixtures 冻结有参/无参/空对象。
- 成功：`result` = 现有响应 JSON（`JsonPayload` 解包后的值）；无返回值为 `null`。
- 失败：

```json
{"jsonrpc":"2.0","id":42,"error":{
  "code":-32000,
  "message":"会话正在运行，暂不能补选供应商/模型",
  "data":{"aether_code":"invalid_value","business_code":"session_busy","details":{...}}
}}
```

- **命名空间分离（R-P0-2）**：
  - JSON-RPC 传输/类别码：`-32700` parse、`-32600` invalid request、`-32601` method not found、`-32602` invalid params、`-32603` internal、`-32001` protocol mismatch、`-32002` core not ready、`-32003` too large、`-32004` shutting down、`-32005` bootstrap failure、`-32006` capability missing。
  - `aether_code`：**只取 `IpcErrorCode` 32 枚举**（`invalid_json`/`unknown_field`/…/`session_not_found`），`IpcErrorCode::as_str()` 是唯一来源。
  - `business_code`（**新增字段，不改变 32 枚举**）：`session_busy`、`session_paused`、`storage_backpressure`、`memory_conflict`、`permission_not_pending`、`data_dir_locked`、`sync_dir_blocked`、`db_open_failed`（**仅 DB 无法打开；`quick_check` 失败用 `persist_degraded`**）、`schema_checksum_mismatch`（**已应用迁移与文件不符，致命 22**）；登记于 **ADR-018 A-7**；`error_codes.json` 的 `business` 集合收录。
  - 前端判定顺序：`business_code`（若存在）→ `aether_code`；`message` 仍只用于展示；`business_code` 省略时不改变既有逻辑。
  - 内部 outcome 字符串（如 `already_running`）不得直接作为 `aether_code` 或 `business_code`。
- `error_codes.json` 路径 **`packages/protocol/src/error_codes.json`**（与 types/client/manifest 同宿主、禁手改）；结构 `{"ipc":[...32],"business":[...]}`，每项含 `code/kind/owner/first_adr`；`pnpm verify:error-codes` 校验 diff=0、ipc=32、business 集合/去重/kind。
- `message` 只用于展示，不参与逻辑；`details` 保留现有结构化字段（计数、路径、原因等）。
- 未知方法、未知字段、类型错误、长度/枚举/路径失败：不调用后端、不落库、不透传下游（与 D7 一致）。
- **退出码表（预 `initialize`/启动失败）**：`0` 正常；`10` 协议/参数初始化失败；`20` `data_dir_locked`；`21` A4 同步盘/数据目录不可用；`22` **DB 无法打开 / 已应用迁移 checksum 不符（`schema_checksum_mismatch`）**（不含 `quick_check` 失败）；`23` 迁移执行失败（**M9 不触发**，码位保留）；`24` 内部错误。首帧 error + 退出码必须同时存在；G1 fixture 分「致命」「checksum 不符」「`quick_check` 失败→安全模式」三组。

### 4.4 通知与事件

- `aether.event`：params = `AetherEvent`（9 字段：`v/id/session_id/run_id/runtime_id/seq/ts/type/payload`）。
- **先日志后广播**不变：`EventPipeline` 写 DB 成功后，才向 daemon 的 transport writer 投递。
- `seq` 由单一 sequencer 分配；DB `UNIQUE(session_id, seq)` 兜底；UI 仍以 `last_seq` 补读。
- `daemon.status`：`{phase, reason?, since_ms}`；仅用于壳层健康/启动页，不进入附录 B 事件类型；`phase` 使用 §4.2 的 `DaemonPhase`。
- `permission.pending`：`{count, session_id?}`；用于即时刷新 UI；实际决议仍走 `permissions_pending` / `permission_resolve`，不新增第二条决策路径。
- 事件转发顺序：daemon stdout → main 单通道 FIFO → `webContents.send('aether:event')`；main 可按 16ms 批处理，但批内顺序不得改变。
- **通道命名映射（P1-6）**：`aether://event` = 域事件通道名（UI EventStore 使用，保持 Appendix B 语义）；`aether.event` = JSON-RPC notification method；`aether:event` = Electron IPC channel。三者一一映射，不得混用。
- **登记表**：M9 的 UI 通道方法/通知/服务端请求见 §16.4；新增通知或服务端请求必须先登记 D7 + 本 ADR（或后续 ADR），不得临时发明。

### 4.5 服务端发起请求（预留 + 对话框）

协议支持 server-initiated request（带 `id` 的 daemon→client 请求）；**M9 仅做协议预留，不接产品路径**：

- `dialog.pick`：`{kind:"file"|"directory", title?}` → client 返回 `{path:string|null}`。**M9 的 `ref_pick`/`startup_pick_target` 固定为壳本地命令，不使用该路径**；M9-06 只交付形状 fixture（正常/超时/取消/迟到丢弃），59/4 计数不变（P2-4）。
- 未来的 `mcp.elicit` / `permission.request`（P3）；M9 不改变现有权限轮询语义。

约束：服务端请求有超时（默认 300s，对话框 120s）；client 取消/超时返回内部原因 `cancelled`/`timeout`（**不是 `aether_code`**，见 §4.3）；迟到的响应按 id 丢弃并记录。

### 4.6 命令路由清单

> **P2-3 口径**：§4.6 是唯一路由口径；§16.1 按路由统计。壳本地 4 条**不进入**业务分组；daemon 侧 artifacts = 3（不含 `ref_pick`）、startup = 2（不含 `startup_pick_target`）。

| 路由 | 数量 | 命令 |
|---|---:|---|
| `aetherd` | 59 | runtime 4、session 12、messages 1、permissions 2、rules 5、settings 2、backup 3、health 1、run 1、workspace 5、diagnostics 1、export 2、audit 2、artifacts 3、providers 9、presets 4、startup 2 |
| Electron 壳本地 | 4 | `app_exit`、`app_restart`、`ref_pick`、`startup_pick_target` |
| **合计** | **63** | 与 D7 命令面一致 |

- `app_exit` / `app_restart`：main 执行 Electron 生命周期（退出前先走 3.6 关闭序列）；daemon 不直接退出进程。
- `ref_pick` / `startup_pick_target`：main 调 `dialog.showOpenDialog`；返回 canonical 路径或 `null`；路径仍由 daemon 在后续 `artifact_add` / `startup_migrate` 中复核。
- `health`：daemon 方法；daemon 不可用时 main 返回合成 `core_not_ready`，UI 走既有「核心未响应」路径。
- `export_diagnostics`：daemon 收集核心日志/DB 诊断，并读取 main 写入 `<data_dir>/logs/shell.log`（main 负责脱敏/轮转，daemon 只读快照）；**daemon 不可用时由 S-12 壳级页提供离线导出**（P2-9）。
- 路由表由 `aether-app` 的 `COMMANDS` 清单生成 `packages/protocol/src/command-manifest.json`；CI 校验「63/59/4」计数与名称集合，禁止手工漂移。
- main 的 `aether:rpc` 先查路由清单：4 条 shell-local 本地处理，其余 59 条转发 daemon；renderer/typed client 对 63 条命令的调用方式统一，不需要知道路由。

### 4.7 背压与补读

- daemon 内部 `EventPipeline` 仍为 `broadcast(4096)`；`Lagged(k)` 记录并让 UI 按 `seq` 补读。
- daemon → main 的 transport writer 使用有界队列（建议 4096）：
  - 队列满时，**可丢** `message.delta` / `usage` / `log`（与 D13 允许丢失/补读的 delta 类一致），累加 `transport_dropped` 诊断计数；
  - **控制事件**（`run.*`、`permission.*`、`session.*`、`runtime.*`、`artifact.*` 等终态/控制类）不得丢；优先按 D8 切 **journal 补读**，**不重启 daemon**。
  - 仅当「无法保序/无法补读」或连续 3 次补读失败时，才重建 transport 连接；仍不杀 daemon。只有协议损坏/失序/无法解析才升级为 daemon 重启，并记审计（P1-10）。
- main → renderer 使用单通道 + 16ms 批处理；同样区分可丢 delta 与不可丢控制事件；renderer 15s 无 `health` 响应显示「核心未响应」；断连重连后 UI 从 `last_seq` 续读。
- `messages_page` 按 ADR-009 §9 + 本 ADR §4.1 修订：`last_seq` 补读 events 表，缺口 >10k → `readback_gap_too_large`；`next_cursor` 仅 events 流，另给 `messages_has_more`；`max_bytes` 默认 1MiB/上限 1.5MiB/下限 64KiB；至少 1 条。`audit_query` 按 ADR-023 A-4：`next_cursor` 与 `offset` 互斥，`has_more` 含预算截断，limit ≤500。

### 4.8 幂等、超时与取消

- `client_msg_id` 幂等、`session_send` 重放语义、`run_retry` 限制不变（ADR-005）。
- 每个方法在客户端配置超时（沿用现有命令超时；无显式超时的默认 30s；备份/导出/迁移按现有基线）。
- 长命令（备份/恢复/导出/迁移）不因 JSON-RPC 连接断开而自动取消；daemon 继续执行并在重连后通过 `health`/列表查询恢复；需要取消时走既有 `session_interrupt` 或后续 `aether.cancel`（M10 评估）。
- 重复 request id → `invalid_request`；取消后的迟到响应按 id 丢弃。

## 5. 安全设计

### 5.1 Electron 主基线（强制）

- `BrowserWindow.webPreferences`：`nodeIntegration:false`、`contextIsolation:true`、`sandbox:true`、`webSecurity:true`、`allowRunningInsecureContent:false`、`experimentalFeatures:false`、`webviewTag:false`、`spellcheck:false`（按需）。
- `preload` 只通过 `contextBridge.exposeInMainWorld('aether', ...)` 暴露：
  - `rpc(method, params)`：只允许命令清单内的方法；payload ≤2MiB；并发上限 64。
  - `onEvent(cb)` / `offEvent(cb)`：只订阅 `aether:event`。
  - `pathForFile(file: File): string`：**唯一**文件路径 API；preload 内用 `webUtils.getPathForFile(file)`，只返回路径字符串（P1-7）；renderer 不得直接 `require('electron')`。
  - `getVersion()`：app/daemon/core 版本。
  - **不暴露** `ipcRenderer`、`require`、`process`、`fs`、`child_process`、任意 channel。
- main 的 `ipcMain.handle('aether:rpc')` 先校验 method 白名单/大小/并发，再转发 daemon；daemon 再按 D7 严格解析参数（**双端校验，daemon 为最终权威**）。
- `app.on('web-contents-created')` 统一加固：`will-attach-webview` 阻止；`setWindowOpenHandler` 拒绝；`will-navigate` 只允许应用自身 URL；外部 http/https 交系统浏览器。
- CSP（生产）：`default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'`。dev 仅额外允许 Vite HMR 的 localhost。
- 主 UI 不加载任何远程脚本/iframe/字体；不使用 `eval`/`new Function`（构建产物检查）。
- `session.setPermissionRequestHandler` 默认拒绝；`setPermissionCheckHandler` 默认 false；主 UI 不需要摄像头/麦克风/定位/剪贴板读。
- DevTools 仅 debug 构建；生产禁用远程调试端口。
- `@electron/fuses`：打包后关闭 `runAsNode`、`enableNodeCliInspectArguments`、`enableNodeOptionsEnvironmentVariable`，开启 `onlyLoadAppFromAsar`（按 electron-builder 支持情况）；构建脚本校验 fuses。
- 单实例锁在创建窗口/启动 daemon 之前；数据目录锁由 daemon 独占。

### 5.2 WebContentsView（UI-06）

- 每个浏览器视图独立 `session.fromPartition('persist:aether-browser:<workspace_or_session_id>')`；不与主 UI 共享 cookie/localStorage/缓存。
- `webPreferences`：`sandbox:true`、`nodeIntegration:false`、`contextIsolation:true`、`webSecurity:true`、`allowRunningInsecureContent:false`、`webviewTag:false`；**不加载 preload**。
- `setPermissionRequestHandler` 默认拒绝；需要的能力（如剪贴板写）逐项 allowlist + 用户可见提示。
- `will-navigate`：只允许 `http:`/`https:` 顶层导航；阻断 `file:`、`javascript:`、`data:`、`blob:`；`setWindowOpenHandler` 拒绝或交系统浏览器。
- `will-download`：用户确认后保存到 Downloads；文件名清洗；不自动打开；不执行。
- 证书错误：默认拒绝；不提供「继续访问」后门（除非未来 ADR 明确）。
- 视图 bounds 由 renderer 通过 `browser.setBounds` 同步；main 做窗口边界 clamp + devicePixelRatio 换算；切换标签/分屏时 hide/show；视图崩溃重建。
- 浏览器内容视为**不可信**：不向其暴露 `window.aether`，不与其共享 IPC，不允许其直接访问 daemon。
- 验收必须包含：两个 partition 的 localStorage/cookie 互不可读；`window.require`/`process` 不可用；`window.open` 被拦截；下载需确认；`file://` 导航被拒。

### 5.3 daemon 侧

- `aether-app` 复用现有 `parse_strict`（`deny_unknown_fields`）、枚举/长度/格式校验、`validate_user_path`（canonicalize + 允许根前缀）、`SessionNotFound` 等错误码。
- 不信任 Electron main/renderer：所有参数在 daemon 重新校验；main 的白名单只是第一道。
- 密钥：`api_key` 只在 `provider_create/update` 请求中出现一次，写 OS Keychain 后只返回 `api_key_ref`；日志/诊断/导出/错误 data 不得含密钥；沿用 `aether-security::redact` 与扫描测试。
- 路径（P1-8）：**所有含路径参数的命令**走生成式清单，至少含 `artifact_add`、`workspace_set`、`startup_migrate`、`backup_restore`、**`backup_create`**、**`export_diagnostics`**、`session_export`、`archive_export`；新增的 dialog 返回值仍走同一 canonicalize/白名单/空间护栏（`db+wal` ×1.2）；`ref_pick`/`startup_pick_target` 返回的路径由后续命令复核。清单从 DTO 生成，CI 断言集合不漂移。
- 审计：权限/工具调用/运行时/备份/导出等审计路径不变；新增的壳层命令（`app_exit`/`app_restart`）不产生业务审计（与现状一致），但记录 shell 日志。

### 5.4 macOS Keychain 迁移

- 现有 Keychain 条目由旧 Tauri 可执行文件创建；新 `aetherd` 二进制/签名的 Keychain ACL 可能触发授权或不可读。
- M9-09 增加启动 preflight：读取 `aether/provider/...` 的已知测试项；失败 → **S-13**「需要重新授权/重新输入密钥」，不静默删除、不静默降级；**阻塞 G5**。
- 备份主密钥不可用时，按 A3/ADR-020 走口令模式或降级提示（S-13 `data-scope=backup`）；不得导致备份静默失效。
- Windows Credential Manager / Linux Secret Service 同样纳入矩阵；CI 用 keychain self-check，真机用迁移演练；签名 Team ID/嵌套签名顺序见 §8.1。

### 5.5 供应链

- Electron 锁定精确版本；安全更新窗口 ≤30 天；major 升级需冒烟 + 记录（可另立 ADR）。
- `electron-builder`、`electron-vite`、`@electron/fuses` 等 npm 依赖必须过 `license-checker`；禁止 GPL/AGPL/BSL。
- `cargo-deny` 继续覆盖 Rust；`deny.toml` 变更走 ADR。
- SBOM 同时包含 Rust crates、npm 包、Electron/Chromium/Node 版本；安装包哈希入发布证据。
- 不使用远程更新器（M9 不做自动更新）；P3 更新器另立 ADR。

## 6. 数据与兼容

- 数据目录不变：Win `%APPDATA%\Aether`，macOS `~/Library/Application Support/Aether`；`AETHER_DATA_DIR` 与 pointer 文件语义不变（D3/A4）。
- DB schema、迁移 `0001–0009`、`schema_migrations` checksum、`quick_check`、WAL、备份/恢复、加密备份全部不变；M9 **不新增迁移**。
- 设置键 9 个、预设、工作区、审计、供应商、密钥引用全部不变。
- daemon 单实例锁：daemon 启动时对 `<data_dir>/.aetherd.lock` 取 **OS 独占文件锁**（防 daemon 多开/残留）；**无 Tauri 回退包、不做跨壳锁**；旧内测包由运维手工卸载（文档说明）。
- **M9 不做跨壳数据往返验收**（无回退包、未发布过旧版本）；数据目录/DB 格式不变，Electron 直接打开内测数据。
- macOS DMG：同 bundle id、同签名身份；Keychain preflight 按 5.4 处理。
- Windows MSI：**不验证旧 Tauri MSI 升级链**；如内测机曾装旧包，手工卸载（文档说明）；不得出现两个 Aether 并存。

## 7. 前端迁移

- 新 `packages/protocol` 生成物：
  - `types.ts`：DTO/事件/错误类型（复用 Rust serde/specta/ts-rs）。
  - `client.ts`：63 个方法的 typed client；`rpc(method, params)` 底层；`onEvent`；错误类型 `AetherError { aether_code, business_code?, details? }`。
  - `command-manifest.json`：63/59/4 路由与名称集合；CI 校验。
  - `error_codes.json`：`ipc` 32 + `business` 清单；`verify:error-codes`。
- 现有 `apps/desktop/src/*.ts` 的 70 处 `invoke` 改为 `client.<method>(...)`；**除 ADR-009/023 已登记的预算字段外**，参数/返回类型不变；错误判定 `business_code` 优先、缺省 `aether_code`。
- `eventBridge.ts`：`listen` → `client.onEvent`；EventStore/`last_seq`/补读逻辑不变。
- 拖拽：`SessionWorkbench.tsx` 的 `getCurrentWebview().onDragDropEvent` → DOM `drop` + `window.aether.pathForFile(file)`（preload 内用 `webUtils.getPathForFile`）；坐标用 `getBoundingClientRect`；`kindForPath` 不变。
- 文件选择：`ref_pick` 选文件；`startup_pick_target` **仍作为通用目录选择器**（备份/导出/审计/归档/工作区复用），由 main 的 `dialog.showOpenDialog` 处理；M9 仅不用于启动迁移；返回路径由 daemon 复核。
- 单实例：Electron `requestSingleInstanceLock` + `second-instance` 聚焦；无 Tauri 插件。
- 退出/重启：`app_exit`/`app_restart` 由 main 处理；先走关闭序列，再 `app.quit()` / `app.relaunch()`。
- 测试：`health.test.ts`/`startup.test.ts` 的 `@tauri-apps/api/core` mock 改为 `@aether/protocol` client mock；其余 44 个测试基本不变。
- **UI-UX v0.17**：M9 无启动门；S-01/S-01a 标「M9 不可达」；S-12 数据目录路径/手动移动；`data_dir_locked`/`db_open_failed`；预算续页 UX；S-12 由**独立最小 renderer** 渲染（主 UI 不加载），提供离线导出诊断。M9-07/M9-08/M9-09 以该版本为前置。
- **debug 探针（P2-10）**：Tauri 的 5 个 `e2e_*` 探针退役；E2E 改用 Playwright + `aetherd --features test-hooks` 的测试方法（release 不注册、`command-manifest.json` 排除），不得混入 63 契约。

## 8. 打包与 CI

### 8.1 打包

- `electron-builder`：
  - `appId: dev.aether.desktop`；`productName: Aether`；`directories.output: dist-out`。
  - `files`: `apps/desktop/dist`、`apps/desktop/electron/main`、`preload`；`asar: true`。
  - `extraResources`: `bin/aetherd`（按平台/arch）、`runtime-bundle/`（3 个适配器 + `runtimes.json` + `digests.json`）。
  - Windows：MSI（x64/arm64 按发布目标）；per-user 安装；`requestedExecutionLevel: asInvoker`；签名。
  - macOS：**per-arch DMG（arm64/x64），不做 universal**（P2-6；避免 3×~96MB 适配器 lipo 翻倍）；`minimumSystemVersion: 12.0`；hardened runtime；公证；entitlements 最小化。
  - **嵌套签名顺序**：先签 `aetherd` 与适配器可执行文件（同一 Team ID），再签 `.app`，最后签 DMG；`codesign --verify --deep --strict` + `spctl`/`stapler` 断言；Keychain 访问身份与签名 Team ID 绑定，M9-09 真机验证。
  - Linux：M9 G6 **仅构建 + CI 冒烟（deb/AppImage），不纳入签名发布门禁**；Linux 发布签名/分发在 **P2 之后的阶段（另立计划）**。
- 版本：app/daemon/core 同源 `Cargo.toml [workspace.package].version`；`initialize` 校验；安装包文件名含版本/arch。
- 不引入自动更新器（P3 另立 ADR）。

### 8.2 CI

- 构建：`cargo build -p aether-daemon --release` + `pnpm --filter @aether/desktop build` + `electron-builder --dir` / `--publish never`。
- 测试矩阵：Windows/macOS 必跑；Linux 构建 + CI 冒烟必跑（不纳入签名发布门禁）。
- **覆盖率口径（P2-7）**：`cargo llvm-cov --workspace --exclude aether-tauri --fail-under-lines 70` 仅适用于 **M9-02～G8**；排除期必须以 `aether-app/rpc/daemon` 增量覆盖 + Tauri 删除文件清单核对替代；G8 恢复全量并给出排除前后对比表；不得用排除掩盖测试迁移遗漏。
- **debug 探针（P2-10）**：Playwright + `test-hooks`；release 不含、manifest 不计入 63；E2E 证据保留。
- 门禁：
  - `cargo test --workspace`、`cargo clippy --workspace -- -D warnings`、`cargo deny check`、`cargo llvm-cov ...`（按上条）。
  - `pnpm -r test`、`pnpm -r typecheck`、`pnpm -r build`。
  - `pnpm --filter @aether/protocol check`（生成物 diff = 0，含 `error_codes.json`）。
  - `pnpm verify:error-codes`（ipc=32、business 集合/去重/kind、diff=0）。
  - `pnpm verify:m9-01-baseline`（manifest/哈希/条目数=63）。
  - `pnpm verify:security`（Electron flags/CSP/nav/preload/fuses/WebContentsView）。
  - `pnpm e2e:m9`（Playwright Electron）。
  - `pnpm smoke:installer`（MSI/DMG 安装→启动→会话→卸载）。
  - `pnpm verify:m9-04`…`m9-14`（逐任务脚本）。
- 产物：安装包、哈希、SBOM、覆盖率、E2E trace、Gate 报告。

## 9. 迁移策略与任务

### 9.1 阶段

| 阶段 | 任务 | 结果 |
|---|---|---|
| P0 基线 | M9-01 | ADR-029 v0.4/设计 v1.28/P2 v0.5/UI-UX v0.17/路由清单/HEAD golden 基线 |
| P1 抽层 | M9-02～M9-04 | `aether-app`/`aether-rpc`/`aetherd`；63 命令等价；Tauri 仅开发对照 |
| P2 桥接 | M9-05～M9-08 | 事件/权限/对话框/Electron 壳/前端客户端 |
| P3 硬化 | M9-09～M9-12 | 生命周期/安全/打包/CI |
| P4 验收 | M9-13～M9-14 | 全量回归/性能/切换/退役/失败处理演练 |

### 9.2 任务总表（详细 DoD 见 P2 计划）

| ID | 任务 | 依赖 | 量级 |
|---|---|---|---|
| M9-01 | 设计基线与迁移门（ADR-029 v0.4/设计 v1.28/P2 v0.5/UI-UX v0.17/路由清单/HEAD golden） | — | S |
| M9-02 | 抽取 `aether-app`（transport-agnostic 命令层） | M9-01 | L |
| M9-03 | `aether-rpc` + `aetherd` stdio 传输 | M9-02 | L |
| M9-04 | 63 命令路由与等价矩阵 | M9-03 | L |
| M9-05 | 事件桥（seq/顺序/背压/补读） | M9-04 | L |
| M9-06 | 权限/对话框桥 | M9-04 | M |
| M9-07 | Electron 壳（main/preload/window/CSP） | M9-04/M9-05 | L |
| M9-08 | 前端 typed client / 类型生成 / 迁移 | M9-04/M9-05 | M |
| M9-09 | 生命周期/单实例/退出/崩溃恢复/数据锁 | M9-07 | L |
| M9-10 | Electron + daemon 安全基线 | M9-07/M9-08 | L |
| M9-11 | 打包/签名/公证/安装冒烟（无旧 MSI 升级链） | M9-09/M9-10 | L |
| M9-12 | CI/E2E/覆盖率/安全门禁 | M9-11 | L |
| M9-13 | 全量回归/性能/旧数据/HEAD 基线对照 | M9-12 | L |
| M9-14 | 默认切换/Tauri 退役（无回退包） | M9-13 | M |

> M10（P2 协作底座）的详细任务、DoD 与 G9–G13 门禁见 `docs/P2-架构迁移-实施计划与验收标准.md` §3；本 ADR 只锁定承载架构与 M9 迁移。

### 9.3 失败处理（R-P1-3 裁定：无回退包）

- **取消回退包**：不冻结/不重建/不保存 Tauri 安装包；无跨壳回退验收（未发布过旧版本）。
- **HEAD 构建时点**：M9-08 起生产 renderer 移除 `@tauri-apps/api`，HEAD 不再产出可用 Tauri UI；Tauri 仅开发期对照，G8 直接退役。
- **任一门禁失败**：停止发布并修复，修好后重跑对应 Gate；可保留**上一 Electron 候选构建**，不得回装 Tauri。
- **内测机旧包**：如曾装旧 Tauri 包，手工卸载（文档说明）；不作为 G6 门禁，不做 MSI 升级链。
- **数据**：不修改 DB schema；「数据可丢」仅限内测，发布前按 §15 重评数据兼容/回退策略。

## 10. 验收门禁（G0–G8）

> 任务映射/可执行条件/证据/失败处理的明细表见 `docs/P2-架构迁移-实施计划与验收标准.md` §2.15；本节保留决策级摘要。

| Gate | 通过条件（摘要） | 证据 | 失败处理 |
|---|---|---|---|
| **G0 基线** | ADR-029 **v0.4**；设计 **v1.28**；P2 **v0.5**；UI-UX **v0.17**；63/59/4 路由清单；`docs/M9-01-baseline/manifest.json` + `verify:m9-01-baseline`；`error_codes.json` + `verify:error-codes`（ipc=32）；Electron 精确 patch；硬阈值冻结；风险登记 | 文档 diff、基线包/manifest、脚本、签核 | 不进入实现 |
| **G1 daemon 协议** | `aetherd` headless；initialize（目标2s/硬超时10s）/phase/capabilities；59 方法；2MiB/非法帧/未知方法/重复 id；**A4 致命 21**；锁/**DB 无法打开 / checksum 不符**致命 20/22/24；**`quick_check` 失败→只读安全模式，不退出**；stdout JSONL；干净退出 | golden fixtures、`cargo test`、M9-03 证据 | 修协议，不进入 G2 |
| **G2 命令等价** | **命令合同 63/63 覆盖（RPC 层矩阵/golden）**；**UI 暴露 61/63**；59 daemon + 4 shell；**32 枚举 + `business_code`**；与 **M9-01 HEAD golden** 对照；预算差异**已登记**（非 0 差异） | `verify:m9-04`、`error_codes.json`、基线对照报告 | 修等价，禁止豁免 |
| **G3 事件/权限等价** | 9 字段/附录 B 类型；seq 单调；顺序；10×50 delta/s 零控制丢失；权限回环/暂停/审计；控制队列满走 journal 不重启 | `verify:m9-05/06`、性能报告 | 修桥接，不进入 G4 |
| **G4 Electron 壳** | 生产 renderer 无 `@tauri-apps`；preload 白名单（含 `pathForFile`）；CSP/导航/权限；拖拽/选择器/单实例/退出；46 前端测试；S-01 不可达/S-12/S-13/`data_dir_locked`/`db_open_failed`/预算续页 | `verify:m9-07/08`、安全基线、UI-UX v0.17 | 不发布；修问题 |
| **G5 数据/安全** | 旧库可开；**`quick_check` 失败走只读安全模式（S-08/导出/`backup_restore` 维护例外）**；备份/加密；Keychain/S-13；**全部路径命令** 0 泄漏；WebContentsView 隔离；daemon 单实例锁 | `verify:m9-09/10`、数据演练 | 不发布；修问题 |
| **G6 打包** | MSI/per-arch DMG；sidecar/runtime-bundle；嵌套签名/公证；安装冒烟；Linux 构建冒烟；**硬阈值：≤1.5× HEAD 且 ≤450MB**；**不做旧 Tauri MSI 升级链** | `smoke:installer`、哈希/SBOM | 不发布；修问题 |
| **G7 全量回归** | cargo/pnpm/clippy/deny/coverage；**P0/P1 映射**；10×50；**硬阈值：启动预算、idle RSS ≤500MB、10 会话 ≤1.2GB、包体**；**与 HEAD 基线对照** | `verify:m9-13`、`docs/M9-13-证据.md` | 不发布；修问题 |
| **G8 切换** | Electron 默认；Tauri 从默认构建移除；**无回退包**；依赖树/文档/`@tauri-apps` 清零；README/AGENTS 更新 | `verify:m9-14`、退役报告 | 停止发布；修问题 |

**硬性停止条件**：G4 安全基线失败、G5 数据丢失/密钥不可恢复、G6 安装/签名/公证失败、G7 P0/P1 回归失败——任一出现即停止发布并修复，不得以「已知问题」带过。

### 10.1 P0/P1 门禁映射（P1-12）

| 原门禁 | 新架构处置 | 承接任务/证据 |
|---|---|---|
| T1–T13、T15 | **复跑**（命令/事件/性能/覆盖） | M9-13；`docs/M9-13-证据.md` |
| T14 bindings（tauri-specta） | **替代**：`protocol generate/check` + 63/59/4 manifest | M9-04/M9-08；`pnpm --filter @aether/protocol check` |
| M1-08 Tauri 安全基线（withGlobalTauri/capabilities） | **替代**：Electron 安全基线 | M9-10；`pnpm verify:security` |
| M4-05/ADR-001 Tauri MSI/DMG | **退役**：electron-builder 安装包 | M9-11；`pnpm smoke:installer` |
| Tauri debug 探针（`e2e_*`） | **替代**：Playwright + `test-hooks` | M9-07/M9-12；`pnpm e2e:m9` |
| 旧 Tauri 基线对照 | **复跑**：M9-01 迁移前 HEAD golden（§9.4-1） | M9-01/M9-13；`docs/M9-01-baseline/` |

## 11. 风险与缓解

| 风险 | 影响 | 缓解 | Owner |
|---|---|---|---|
| Electron 攻击面增大（Chromium+Node） | 安全降级 | 5.1/5.2 基线 + fuses + 双端校验 + 安全 Gate；不允许 `nodeIntegration`/通用 IPC | 安全 |
| daemon 孤儿/适配器孤儿 | 进程泄漏、数据锁 | stdin EOF、Job Object、进程组、PID 台账、启动清理、数据目录锁 | 核心/进程 |
| DB 可打开但 `quick_check` 失败 | 误杀进程/丢自救路径 | **只读安全模式**（`persist_degraded`/S-08/导出/备份）；仅 DB 无法打开才 22/S-12 | 存储/安全 |
| 事件乱序/丢失 | UI 状态错乱 | 单通道 FIFO、16ms 批处理、控制事件不丢、`seq` 补读、10×50 压测 | 事件 |
| macOS Keychain 旧条目不可读 | 密钥/备份不可用 | 5.4 preflight、重新授权/输入、口令模式、真机演练 | 安全/数据 |
| 内测机旧 Tauri 包残留 | 双安装/数据风险 | 文档说明手工卸载；不作为 G6 门禁；daemon OS 锁防并发写库 | 发布 |
| 安装包/内存上升 | 体验下降 | 记录基线；体积/启动/RSS 阈值；Sidecar 只打包必需文件；G7 门禁 | 发布/性能 |
| 命令契约漂移（HEAD 基线 vs daemon） | 命令不等价 | `aether-app::dispatch` 单一实现；M9-01 HEAD golden；生成路由清单；已登记差异独立 | 应用层 |
| 双维护期拉长 | 进度/缺陷 | 明确 M9-14 退役；Tauri 只修阻塞缺陷；G8 前不再加新功能 | 计划 |
| 团队同时维护 Rust/Electron | 人力分散 | M9 先迁移后功能；M10 在 G8 后启动；职责边界按 3.2 | 计划 |
| Linux 打包/签名未定 | G6 范围不清 | M9 G6 只做构建/冒烟；Linux 发布签名在 **P2 之后的阶段（另立计划）**；不阻塞 Win/mac | 发布 |
| 自动更新缺失 | 发布不便 | M9 明确不做；P3 更新器 ADR | 发布 |
| MCP 双 owner | 重复连接/状态分裂 | D-029-8：一个 MCP 服务一个 owner；P3 细化 | Node/MCP |
| WebContentsView bounds/焦点复杂 | UI-06 体验 | M9 只做 PoC；完整 UI-06 在 P4 实现；bounds 同步测试 | 浏览器 |

## 12. 备选方案与取舍

| 方案 | 结论 | 理由 |
|---|---|---|
| 留在 Tauri，等 multiwebview/CEF 稳定 | 不选 | UI-06 B 是已登记需求；Tauri 多 WebView 仍 unstable 且无近期计划；CEF 实验性 |
| 用 Tauri + 独立浏览器窗口替代 B | 不选 | B 要求单窗口内可交互浏览器/分屏，独立窗口不满足信息架构 |
| TS 重写核心 | 不选 | 3.77 万行 Rust、进程监督/加密/SQLite/审计；6–12 个月起步，失去 Rust 安全与性能 |
| napi-rs 把 Rust 编进 Electron main | 不选 | 失去崩溃隔离；Rust panic 会带崩 main；构建/ABI 复杂；sidecar 更符合 D2/D5 |
| 仅本地 HTTP/WS，不做 stdio | 不选 | 端口/鉴权/CORS/防火墙/本机攻击面；UI 通道不需要网络栈。M10 再为外部客户端加 HTTP/WS |
| Tauri CEF 作为生产依赖 | 不选 | 仍为实验/分支；Readest 等可用但社区有迁回案例；不能作为 G6 承诺 |
| Tauri（Windows）+ Electron（macOS）双壳 | 不选 | 双份壳/打包/测试/安全基线；数据锁/升级复杂；维护成本翻倍 |
| 保持 Tauri 到 P4 再迁 | 不选 | UI-06 会先在 Tauri 上开发再返工；M9 必须在 P4 前完成 |

## 13. 后果

**正面**：
- Chromium 全平台一致；`WebContentsView` 满足 UI-06 B；macOS/Linux 渲染风险消除。
- 保留 3.77 万行 Rust 核心；命令/事件/错误/数据语义不变（仅 ADR-009/023 已登记的预算字段与 `business_code` 增量）。
- daemon 化带来崩溃隔离、headless/CLI/HTTP/WS/P5 SDK 复用；对齐 Codex 的 app-server 模式。
- Node/MCP 有 `UtilityProcess` 承载；E2E 可用 Playwright。

**负面**：
- 安装包/内存增加；两个运行时/两套日志；IPC 与生命周期复杂度上升。
- Electron 安全基线/签名/公证/分发路径需重新证明；Electron 升级节奏带来长期维护。
- Tauri 退役需要一次迁移窗口；团队短期双维护。

**中性**：
- UI-UX v0.16→v0.17（启动门/状态图清理、`db_open_failed`）；命令面/事件/32 枚举/设置/DB/适配器协议不变；`business_code`/预算字段/安全模式为增量；M9 不做数据迁移。

## 14. 文档变更清单

| 文档 | 变更 |
|---|---|
| `设计文档.md` | v1.27 → **v1.28**；D1 Electron + aetherd；D2 DB 可打开/`quick_check` 失败→安全模式；D7 `business_code`/cursor + `startup_pick_target` 通用目录选择器；D1 per-arch DMG；附录 D T13；§5 P2 增 M9 |
| `docs/P2-架构迁移-实施计划与验收标准.md` | **v0.5**；M9-01～M9-14 + M10-00～M10-08；Gate G0–G13；**R1–R9**：63 合同/61 UI、生成器分阶段、基线 manifest、硬阈值 |
| `docs/P1-实施计划与验收标准.md` | **保持 v0.14**：ADR-029 v0.4 无需 P1 计划变更（P1 结构/命令/32 枚举不变） |
| `docs/UI-UX-设计规格.md` | v0.16 → **v0.17**：R6 启动门/状态图/§5 拆行 + `db_open_failed`；锚点 22 |
| `docs/adr/ADR-006-startup-commands-and-error-codes.md` | **v0.10**：业务码命名空间；A4 致命；DB 无法打开 vs `quick_check` 安全模式；`schema_checksum_mismatch` |
| `docs/adr/ADR-010-p0-ui-capability-registration.md` | **v0.11**：横切错误码口径（32 枚举 + `business_code`）、UI-UX 22 锚点；不改其决策 |
| `docs/adr/ADR-018-p1-command-surface-batch.md` | **v0.5**：A-1 `audit_query.max_bytes`；A-7 `business_code`/`data_dir_locked`/`schema_checksum_mismatch`；A-9 S-12/S-13 |
| `docs/adr/ADR-028-provider-catalog-credential-broker.md` | **v0.10**：横切锚点 22/`business_code` 口径；补选语义不变 |
| `AGENTS.md` | 基线 v1.28/ADR-029 v0.4/P2 v0.5/UI-UX v0.17；`error_codes.json` 禁手改 + `verify:error-codes`；目录边界 |
| `README.md` | 技术栈/目录/构建命令改为 Electron + aetherd；版本同步 |
| `需求文档.md` | **不改**：§2.2 已允许 Tauri/Electron + React；无新增需求 |
| `docs/adr/ADR-001-tauri-identifier-msi.md` | **不修改**：identifier/MSI/DMG 目标保留；生成方式由 electron-builder 覆盖 |
| `docs/adr/ADR-009-...md` | **v0.3 §9**：`max_bytes`、至少 1 条、`next_cursor`、**`filter_hash` 每次重算/校验**、`messages_has_more` |
| `docs/adr/ADR-023-...md` | **v0.4 A-4**：`audit_query` `max_bytes`/cursor-offset 互斥/`has_more`/`filter_hash` 校验 |
| `packages/protocol/src/error_codes.json` | **新增生成物**：`ipc` 32 + `business` 清单；`pnpm verify:error-codes`（diff=0/32/去重/kind） |
| `deny.toml` | 不新增 Rust 依赖；新增 npm 依赖走 `license-checker`；如新增 Rust crate 依赖需单独评审 |

**阶段边界追溯（P2-5）**：M9 的 `WebContentsView` PoC 属 **P2 迁移使能**，不构成 UI-06（P4）阶段前移；M9 的 `UtilityProcess` 是通用 Node 承载，不构成 MCP（P3）交付。需求文档不改；M9-01 证据归档该裁定，G5 只验 PoC 隔离，不验 UI-06 交互。

## 15. 开放问题

1. **Linux 范围（已裁定）**：M9 G6 **只做 Linux 构建 + CI 冒烟（deb/AppImage），不纳入签名发布门禁**；Linux 发布签名/分发在 **P2 之后的阶段（另立计划）**。Win/mac 为 M9 发布门禁。
2. **代码签名/公证证书**：Windows 证书、Apple Developer ID/notary 凭据是否就绪；**阻塞 G6**，无证书不得宣称 G6 通过（beta 说明不替代签名）。
3. **自动更新**：M9 不做；P3 更新器 ADR 决定 electron-updater / 其他方案。
4. **MCP owner**：Rust `rmcp` vs Node `UtilityProcess`；P3 决定，原则是「一个服务一个 owner」。
5. **UI-06 交互细节**：标签/分屏/下载/权限提示/会话隔离粒度；P4 规格，M9 只做 `WebContentsView` PoC。
6. **macOS Keychain 迁移**：旧条目重授权/重新输入的具体 UX；M9-09 真机验证；**阻塞 G5**。
7. **Tauri 退役时点**：G8 移除默认构建；**不保留回退包**；Tauri 仅开发期对照。
8. **Electron 版本策略**：M9-01 锁定当前稳定线精确 patch；安全更新窗口 ≤30 天；major 升级需冒烟。
9. **基线统计**：M9-01 用脚本输出命令数/调用点/测试数/bindings 行数（当前 **1003 行含空行**）/核心行数/测试文件数；ADR 引用脚本输出，不硬编码。
10. **体积/RSS 阈值（硬阈值，R9）**：M9-01 冻结 `安装包 ≤1.5× HEAD 基线且 ≤450MB`、`idle RSS ≤500MB（3 适配器）`、`10 会话 ≤1.2GB`；G6/G7 超限即**不通过**，不设自由裁量豁免（仅可停止发布并修复）。
11. **Keychain 真机可读性**：M9-09 在旧 Tauri 数据 + 新签名身份上实测；失败走 S-13 重授权/口令降级，不删除旧条目。
12. **发布前重评**：M9 无回退包、内测数据可丢仅是当前阶段口径；正式发布前必须重评数据兼容/回退策略（§9.4-2），不得沿用「未发布」假设。

## 16. 附录

### 16.1 命令路由分类（63）

| 组 | 数量 | 方法 |
|---|---:|---|
| runtime | 4 | `runtimes_list`、`runtime_retry`、`runtime_enable`、`runtime_disable` |
| session | 12 | `session_list`、`session_create`、`session_search`、`session_rename`、`session_delete`、`session_send`、`session_interrupt`、`session_pause`、`session_resume`、`session_dispose`、`session_permission_mode_set`、`session_workspace_assign` |
| messages | 1 | `messages_page` |
| permissions | 2 | `permissions_pending`、`permission_resolve` |
| rules | 5 | `rules_list`、`rule_add`、`rule_update`、`rule_delete`、`rule_toggle` |
| settings | 2 | `settings_get`、`settings_set` |
| backup | 3 | `backup_create`、`backup_list`、`backup_restore` |
| health | 1 | `health` |
| shell（壳） | 4 | `app_exit`、`app_restart`、`ref_pick`、`startup_pick_target` |
| run | 1 | `run_retry` |
| workspace | 5 | `workspace_set`、`workspaces_list`、`workspace_create`、`workspace_update`、`workspace_delete` |
| diagnostics | 1 | `export_diagnostics` |
| export | 2 | `session_export`、`archive_export` |
| audit | 2 | `audit_query`、`audit_export` |
| artifacts | 3 | `artifacts_list`、`artifact_add`、`artifact_remove`（`ref_pick` 归 shell） |
| providers | 9 | `providers_list`、`provider_create`、`provider_update`、`provider_delete`、`provider_toggle`、`provider_model_add`、`provider_model_toggle`、`provider_test`、`provider_models_fetch` |
| presets | 4 | `presets_list`、`preset_add`、`preset_update`、`preset_delete` |
| startup | 2 | `startup_get`、`startup_migrate`（`startup_pick_target` 归 shell） |
| **合计** | **63** | 59 daemon + 4 shell（与 §4.6 一致） |

### 16.2 错误码映射（R-P0-2 修正）

- JSON-RPC `error.code` 只表达传输/类别；业务分支按 **`error.data.business_code`（若存在）→ `error.data.aether_code`**。
- **`aether_code` = `IpcErrorCode` 32 枚举**：`as_str()` 是唯一来源；`invalid_json`/`unknown_field`/`missing_field`/`invalid_type`/`invalid_value`/`invalid_enum`/`too_large`/`out_of_range`/`invalid_format`/`path_rejected` 等语义不变。
- **`business_code`**（新增字段，不计数）：`session_busy`、`session_paused`、`storage_backpressure`、`memory_conflict`、`permission_not_pending`、`data_dir_locked`、`sync_dir_blocked`、`db_open_failed`、`schema_checksum_mismatch`；按 **ADR-018 A-7** 增行登记；省略时不改变既有逻辑。
- `error_codes.json` 路径 **`packages/protocol/src/error_codes.json`**，结构 `{"ipc":[...32],"business":[...]}`；`pnpm verify:error-codes` 校验 diff=0、ipc=32、business 集合/去重/kind。
- 内部 outcome 字符串（如 `already_running`）不得直接作为 `aether_code`/`business_code`；`cancelled`/`timeout` 只作服务端请求的内部原因。

### 16.3 事件/设置/迁移

- EventEnvelope 9 字段与附录 B 事件类型集合不变；`aether.event` 只改变传输，不改变 payload。
- 设置键 9 个不变；`SETTINGS_KEY_ALLOWLIST` 仍在 `aether-app`。
- 迁移 `0001–0009` 不变；M9 不新增迁移；`verify:m1-03` 继续作为 schema 门禁。

### 16.4 UI 通道登记表（P1-6）

| 类别 | 名称 | 方向 | 说明 |
|---|---|---|---|
| 命令 | 63 条（59 daemon + 4 shell） | client → daemon/main | 名称/参数见 §4.6、§16.1；`messages_page`/`audit_query` 增 `max_bytes`/`has_more`/`next_cursor`（messages 另 `messages_has_more`） |
| 通知 | `aether.event` | daemon → client | EventEnvelope 9 字段；IPC channel `aether:event`；域通道 `aether://event` |
| 通知 | `daemon.status` | daemon → client | `{phase, reason?, since_ms}`；`phase` 见 §4.2 |
| 通知 | `permission.pending` | daemon → client | 仅刷新提示；决议仍走 `permissions_pending`/`permission_resolve` |
| 服务端请求 | `dialog.pick` | daemon → client | **M9 仅预留**；`ref_pick`/`startup_pick_target` 固定 shell；59/4 不变 |
| 服务端请求 | `mcp.elicit` / `permission.request` | daemon → client | P3；M9 不实现 |

**通道映射**：`aether://event`（域事件名，UI EventStore）→ `aether.event`（JSON-RPC notification）→ `aether:event`（Electron IPC）。新增项必须先登记本表 + D7。

### 16.5 证据模板

每个 M9 任务证据文档至少包含：

1. 任务 ID/ADR 条目/依赖/量级；
2. 变更文件清单（`git diff --stat`）；
3. DoD 逐项结果 + 可复跑命令；
4. 失败注入/边界样本；
5. 跨平台结果（Win/mac/Linux，注明未跑原因）；
6. 失败处理（停止发布/修复/重跑 Gate；无 Tauri 回退包）；
7. 未决风险/开放问题。
