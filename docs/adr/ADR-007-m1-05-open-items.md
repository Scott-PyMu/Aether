# ADR-007：M1-05 未决项收口——`health` 命令、写失败重试口径、ULID 实现

| 项 | 内容 |
|---|---|
| 状态 | **已批准**（2026-09-18，签署见 §7）；含**增量修订 1**（2026-09-18，M1-05 遗留收口，见 §9，v0.2）、**增量修订 2**（T12 顺序 `core_not_ready` 与 `health.runtimes` 语义，见 §10，v0.4）与**评审修订**（C1/C2 收敛与决策 3 例外登记，v0.3） |
| 决策日期 | 2026-09-18 |
| 决策载体 | 已合入：《设计文档》v1.5 → **v1.7**（含 ADR-006 的 v1.6 与本 ADR 增量；ADR-006 文档合入文本先应用，见 §9.6）；《实施计划与验收标准》v1.11 → **v1.13**（同理）。合入文本见附录 C/D，增量修订以 §9/§10 为准；2026-09-18 随签署冻结 |
| 关联 | 设计文档 D2/D4/D7、§2.2 选型表、附录 B/E；实施计划 M1-05、M1-08 DoD3、M2-07、M3-01、M3-03、M3-05、M3-06、M4-04、Gate 2、§6 #10、§7、§8；ADR-003（背压边界）、ADR-004（降级状态机/命令面）、ADR-006（启动迁移命令面，版本协调） |
| 取代 | 无（增量登记：D7 命令面 +1；D4 重试措辞澄清；ULID 实现回归 §2.2 选型，不新增机制） |
| 被取代 | 无 |
| 回退条件 | 见 §6（增量修订补充见 §9.7） |

## 1. 背景

M1-05（事件管线）已完成并进入证据归档，但交付说明遗留三个未决项；三者均属「设计文档未定义或与实现不一致」的接口/口径问题，按 AGENTS §3 必须先走 ADR 并升版，再回流实施计划：

1. **D7 命令面无 `health`**：D2 明写「UI 每 5s 调 `health`；15s 无响应显示『核心未响应』+ 重启入口」，D4 降级期语义 4 要求降级通知经 `health` 返回 `storage_state=persist_degraded`；但 v1.5 的 D7 命令面（ADR-004 登记 20 条可调用命令）没有 `health`。M1-05 已交付管线侧 `EventPipeline::health()`（含 `storage_state`/`degrade_trigger`/诊断计数/写队列深度），但命令层缺登记、M1-08 校验矩阵缺样本、M2-07 缺接线任务与 E2E 口径。
2. **写失败重试口径歧义**：D4 失败场景表写「重试 3 次；仍失败 → `persist_degraded`」，状态机图写「写事务重试 3 次均失败」，M1-05 实现取「总尝试 3 次（含首次；即重试 2 次）」并记录于任务证据。该歧义会导致验收时对「注入 3 次失败应否降级」「重试次数是 2 还是 3」各执一词，必须在设计文档层面统一措辞。
3. **ULID 实现偏离选型**：§2.2 选型表明确核心栈使用 `ulid` crate；M1-05 为避免当时无法确认的供应链/网络风险，采用了 std-only 自研生成器（规格兼容，但存在「全局去重键」自实现风险与长期维护面）。需评估并落地最终方案。

**现状与边界**：

- 工作区已清理（M1-05 代码在库），`aether-tauri` 当前无并发未提交改动，可安全做命令骨架登记；真实数据接线仍归属 M2-07（后端持有 `EventPipeline` 的组件尚未存在）。
- ADR-006 仍在「提议（待评审）」，拟升 v1.6/v1.12；本 ADR 与其**无决策冲突**，仅存在版本号与 D7/D8 落点的合入顺序问题（§3.1/附录 C/D 给出与顺序无关的合入文本）。
- 本 ADR **不修改**已冻结的《设计文档》/《实施计划》原文：按 ADR-006 先例，合入文本放附录，评审通过后再执行升版（避免伪造评审批准与版本抢占）。

## 2. 决策

| # | 决策 | 落地位置 |
|---|---|---|
| 1 | **D7 命令面新增 `health`（无参数）**：参数严格解析（`null`/缺省/空对象合法；任何成员返回 `unknown_field`）；返回 `HealthReport`（至少含 `storage_state` / `write_queue_depth` / `runtimes` 摘要 / `ts`）；语义为**仅本地 IPC 查询：不落库、不产生事件**；UI 每 5s 调用，15s 无响应显示「核心未响应」+ 重启入口；真实数据接线归属 **M2-07**（映射 `EventPipeline::health()` 与监督器状态）；M1-08 校验矩阵纳入 `health`。契约明细见附录 A | 设计文档 D2/D7；实施计划 M1-08 DoD3、M2-07（新增 DoD5）、M4-04 命令面登记、§7 |
| 2 | **写失败重试口径统一为「总尝试 3 次（含首次）」**：常量 `MAX_WRITE_ATTEMPTS = 3`，即重试 2 次；**连续 3 次写事务尝试失败**（含首次）→ `persist_degraded` + 只读；第 1、2 次失败、第 3 次成功**不降级**；每次失败尝试输出日志 `attempt=1/3`、`2/3`、`3/3`（`tracing::warn!`，§2.2 选型；验证由测试侧 tracing 捕获，见 §9.3）。D4 失败场景表/状态机图/状态定义措辞按附录 C.3 统一 | 设计文档 D4；实施计划 M1-05 DoD2/新增 DoD7、§6 #10、§7；代码 `crates/aether-control/src/pipeline.rs` |
| 3 | **ULID 实现恢复设计选型 `ulid` crate**：`ulid = "=1.1.3"`（MIT；精确固定以复用既有 `rand 0.8.8`/`getrandom 0.2.17` 依赖图，不引入 rand 0.9+ 传递依赖；升版须走依赖评审）；删除 std-only 自研实现，生成/解析统一经 crate（内部 `generate()` 包装，调用点不散落）；补生成/唯一性/往返/非法输入用例；记录 1.1.3 的溢出边界行为（首字符 >7 不报错、非规范往返）为已知 crate 语义，生成侧首字符恒 ≤7 不受影响。例外——`crates/aether-tauri/src/startup/state.rs::new_ulid` 生成的迁移状态标识（`migration_state.json`，壳层私有文件，不进线协议与数据库映射表，见 ADR-006 决策 7）不属于「事件/协议标识」，登记为决策 3 的显式例外，不视为「调用点散落」；是否后续统一随 M3-01/T14 复查评估 | 设计文档 §2.2（选型表内容不变，实现回归）；实施计划 M1-05 新增 DoD8；代码 `crates/aether-control/{Cargo.toml,src/ulid.rs}`；`cargo deny check` |

> **命令面计数（决策 1 影响）**：ADR-004 后 P0 可调用命令 20 条；ADR-006 拟 +4（启动迁移）＝ 24 条；本 ADR +1（`health`）＝ **25 条**（条目口径 24 条，`settings_get/set` 计 1 条目）。§7 与 M4-04 命令面完整性按此口径登记。

## 3. 影响

### 3.1 《设计文档》v1.5 → v1.6/v1.7（拟，合入文本见附录 C）

- 头部：文档版本行与状态行按合入批次升版；修订记录追加一行（ADR-006 合入行在前、本 ADR 行在后；同批合入则合并为一行，内容并集）。
- D2：UI 健康条目补交叉引用「命令契约见 D7」。
- D4：失败场景表、状态机图、状态定义统一为「连续 3 次写事务尝试失败（含首次；`MAX_WRITE_ATTEMPTS = 3`，重试 2 次）」。
- D7：命令面追加 `health`（无参数、`HealthReport`、仅本地 IPC、不落库/不产生事件、UI 5s/15s 口径）；与 ADR-006 的启动迁移命令块并列，合入顺序无关。
- §2.2 选型表：**不变**（`ulid` 原已在列；本 ADR 使实现回归选型，并记录精确固定版本的理由于本文档）。
- 附录 E：无变更（`health` 不读写数据库）。

### 3.2 《实施计划与验收标准》v1.11 → v1.12/v1.13（拟，合入文本见附录 D）

- 头部/§8：计划版本、状态、基线引用按合入批次升版（含 ADR-001–007）。
- M1-05：DoD2 写入重试口径与 attempt 日志；新增 DoD7（重试口径行为断言）；新增 DoD8（ULID 选型与测试）。
- M1-08 DoD3：校验矩阵纳入 `health`（无参数严格解析：缺省/空对象合法、任何成员 `unknown_field`）。
- M2-07：新增 DoD5（`health` 接线、UI 5s 轮询、15s「核心未响应」+ 重启入口、E2E 覆盖 `normal`/`persist_degraded`）。
- Gate 2：新增显式检查项（M1-05 重试口径/ULID 证据；M2-07 `health` 两态 E2E 证据）。
- §6 #10：持久化失败行措辞同步「连续 3 次尝试失败（含首次）」。
- §7：M1-05/M1-08/M2-07 验收项同步；任务总量与关键路径不变（无新任务）。

### 3.3 实现对齐状态（评审通过后关闭）

| 项 | 状态 | 证据 |
|---|---|---|
| `health` 管线侧数据源 | **已实现（M1-05）**：`EventPipeline::health()` 返回 `storage_state`（`normal`/`persist_degraded`）、`journal_queue_depth`、诊断计数等 | `crates/aether-control/src/pipeline.rs`；`tests/m1_05_degraded.rs::adr007_health_reports_both_storage_states` |
| `health` 命令真实接线 | **已实现（增量修订 1；契约修订见增量 2）**：`core_health` 模块（Provider/Report/CoreHealthBackend/boot）+ `run()` 启动接线；启动失败按 `persist_degraded` 呈现；延迟后端注入前返回 `core_not_ready`，`runtimes` 为 `array | null` | `crates/aether-tauri/src/core_health.rs`、`src/lib.rs`；`tests/health_command.rs`（normal/degraded/启动失败/core_not_ready/wired-runtimes 5 用例） |
| `health` 参数校验矩阵 | **已实现（M1-08）**：无参 DTO + `parse_no_params` + debug/release 注册 + 未知成员 `unknown_field` 样本 | `crates/aether-tauri/src/ipc/*`；`tests/ipc_validation.rs` |
| 重试口径常量与日志 | **已实现**：`MAX_WRITE_ATTEMPTS = 3`（含首次）；`attempt=n/3` 经 `tracing::warn!`，由测试侧捕获 | `pipeline.rs`；`tests/m1_05_attempt_log.rs` |
| 健康面不含验证日志 | **已实现（增量修订 1）**：`persist_attempt_log` 已从 `PipelineHealth` 移除 | `pipeline.rs`；verifier 静态检查 |
| ULID 选型与边界 | **已实现**：`ulid = "=1.1.3"`；生成侧 10 万次首字符 ≤7；解析侧 >7 显式断言（`crates/aether-tauri/src/startup/state.rs::new_ulid` 的迁移状态 ULID 为决策 3 显式例外，见 §2 决策 3） | `crates/aether-control/{Cargo.toml,src/ulid.rs}` |
| 设计文档/实施计划合入 | **已按任务指示合入**：先应用 ADR-006 文本（v1.6/v1.12），再应用本增量（v1.7/v1.13）；2026-09-18 评审批准并随 v1.7/v1.13 冻结（§7） | 附录 C/D、§9.6 |

### 3.4 不影响的

- 不新增/修改事件类型（附录 B 清单不变）；不改数据库 schema（无迁移）；`health` 不落库、不产生事件。
- 不改 ADR-004 的 7 命令与 ADR-006 的 4 命令契约；不改 `capabilities` 最小 allowlist 口径（应用命令不受 ACL 门控，沿用 ADR-006 决策 4 结论）。
- 不改 P0–P6 阶段划分、Gate 条件结构与任务总量（37 条目/36 互斥执行）。
- 不回改 M1-05 已交付的其他语义（先日志后广播、delta 合并、补读、降级状态机）。

## 4. 版本（拟）

| 文档 | 修订前 | 修订后（分批评审） | 修订后（与 ADR-006 同批） |
|---|---|---|---|
| 《设计文档》 | v1.5 | **v1.7** | **v1.6**（含 ADR-006 + ADR-007） |
| 《实施计划与验收标准》 | v1.11 | **v1.13** | **v1.12**（含 ADR-006 + ADR-007） |
| 《需求文档》 | v0.5 | 不变（无需求项变更；`health` 属 D2 既有 UI 健康要求） | 不变 |

## 5. 后续（未决项）

1. **`tracing` 输出端未接线**：P0 尚无日志汇聚/订阅器（`tracing` 仅按 §2.2 选型引入并发出事件）。`attempt=n/3` 当前可通过 `health.persist_attempt_log` 无订阅验证；日志汇聚（含诊断包）在 M2-07/M3-05 统一接线，本 ADR 登记为已知状态，不视为缺陷。
2. **`runtimes` 摘要字段对齐**：附录 A 给出目标形状；与 M1-10 监督器快照（`runtimes.status/status_reason`）的字段映射在 M2-07 实现时冻结为命令契约。
3. **`ulid` 升版评估（P1）**：3.x 依赖 rand 0.10；升级需依赖评审并复跑全部 ULID 用例（生成/解析/互操作）。
4. **ADR-006 版本协调**：若评审选择合并升版，按附录 C/D 注明合并执行；若分批次，先 ADR-006 后本 ADR。
5. **`health` 启动门口径复核（M2-07）**：当前命令走 `backend_ready()`（与业务命令一致）；拒绝启动页场景由 `startup_get` 承担。若评审要求 `health` 在启动门阻断期仍可达，在 M2-07 记录并调整（不涉数据面）。

## 6. 回退条件

1. **评审否决新增 `health` 命令** → 保留 `EventPipeline::health()` API，UI 健康/降级通知改由既有事件通道 + `startup_get` 扩展承载；须另立 ADR 并同步 M2-07/M3-06。
2. **重试口径评审改判为「首次 + 3 次重试」** → 仅改常量 `MAX_WRITE_ATTEMPTS = 4` 与断言/文档措辞（不涉结构）；须在同一 ADR 评审记录中留痕。
3. **`ulid` crate 过不了 `cargo deny`（许可证/漏洞/版本）** → 回退 std-only 自研实现，并按本 ADR 决策 3 的备选要求补齐：26 字符格式、48 位时间戳、80 位随机来源说明、解析拒绝（非法/大小写/溢出）、单调性策略、互操作测试（dev-dependency 的 `ulid`）、随机源审计；未能满足则在 Gate 2 前替换。
4. **`health` 与启动门交互冲突**（阻断期要求可达）→ 在 M2-07 记录并调整命令门控，不放宽参数校验。

## 7. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-09-18 | AI 评审（opencode/GLM） | 通过 | 评审范围：决策 1–3 + 增量修订 1（§9，v0.2）+ 评审修订（v0.3）+ 增量修订 2（§10，v0.4）；评审意见 C1/C2/B1–B3/A1–A6 已闭环；`core_not_ready` 与 `runtimes array\|null` 经实质评审接受（瞬态/终态分离，`startup_*` 契约不受影响）；startup 迁移状态 ULID 显式例外确认；v1.7/v1.13 随本签署冻结 |

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-09-18 | 创建：收口 M1-05 三个未决项（`health` 命令登记与校验矩阵、`MAX_WRITE_ATTEMPTS = 3` 含首次与 attempt 日志、ULID 恢复 `ulid` crate）；给出设计文档/实施计划合入文本与实现对齐状态；随附命令骨架/重试日志/ULID 替换代码与测试 | （文档维护） |
| v0.2 | 2026-09-18 | **增量修订 1（M1-05 遗留收口，§9）**：`health` 撤销「M2-07 才接线」改为**真实接线**（managed state 持 `EventPipeline` 句柄 + 监督器摘要接缝；启动失败按 `persist_degraded` 呈现，不回退 `not_implemented`）；撤销 `health.persist_attempt_log`，attempt 验证改为**测试侧 tracing 捕获**（dev-dependency `tracing-subscriber`；运行期日志汇聚端归 M2-07/M3-05）；`ulid 1.1.3` 边界补充（生成侧 10 万次首字符 ≤7 属性测试、解析侧 >7「丢弃高位」显式断言、不升 3.x）；随文应用 ADR-006 的前置合入文本并给出 v1.7/v1.13 增量合入文本 | （文档维护） |
| v0.3 | 2026-09-18 | 评审修订：C1/C2 对应文档已另行修订；决策 3 登记 startup 迁移状态 ULID 为显式例外（§3.3）；正文与 §9 冲突点已按 §9 收敛（见 A1） | （文档维护） |
| v0.4 | 2026-09-18 | **增量修订 2（§10）**：新增错误码 `core_not_ready`（Builder 延迟后端注入前过渡窗口；门命令仍可达；T12 第二实例不触发）+ 测试；`health.runtimes` 改为 `array \| null`（null=监督器未接线；[]=已接线无 runtime），`RuntimeSummarySource` 返回 `Option`，生产 boot 用 `unwired`；设计文档 D7 与实施计划 M1-05 DoD9/Gate 2 同步 | （文档维护） |

---

## 9. 增量修订 1（M1-05 遗留收口，2026-09-18）

> 与 §2 决策及附录冲突处以本节为准；本节同时记录两份 ADR 的合入顺序（ADR-006 文本先应用为 v1.6/v1.12，本增量再合入为 v1.7/v1.13）。

### 9.1 背景与基线差异披露

1. **任务指示的基线为设计 v1.6 / 计划 v1.12**；仓库实际为 **v1.5 / v1.11**（ADR-006 仍为「提议（待评审）」，其附录 C/D 的合入文本尚未应用）。为达到指示的目标版本且不重排 ADR 编号，本增量按「**先应用 ADR-006 已拟定的合入文本（v1.6/v1.12），再合入本增量（v1.7/v1.13）**」执行；两份 ADR 的评审状态各自保留（ADR-006 §7：有条件通过待复评；本增量：待评审），不伪造批准。
2. **`health` 接线条件已具备**：命令骨架、`EventPipeline::health()`、无并发冲突（`aether-tauri` 工作区干净）均已满足，v0.1 「M2-07 才接线」的安排撤销。
3. **`health` 承载验证日志属接口污染**：v0.1 的 `health.persist_attempt_log` 把验证用日志混入只读状态查询，撤销；attempt 验证回归测试侧（`tracing` 捕获）。
4. **`ulid 1.1.3` 非规范往返边界**需固化为显式断言，避免未来升级 crate 时静默改变解析语义。

### 9.2 决策 1 修订：`health` 现阶段真实接线

| 项 | 修订后内容 |
|---|---|
| 接线时点 | **M1-05 收口即接线**（撤销「M2-07 才接线」）；M2-07 的 DoD 收窄为 UI 侧轮询/超时/两态 E2E（见 §9.6），不再包含命令接线 |
| 数据源 | `storage_state` / `write_queue_depth` 直接来自 `EventPipeline::health()`（`PipelineHealthSource` 最小句柄；D4 唯一降级事实源）；`runtimes` 来自 `RuntimeSummarySource`（监督器状态摘要；增量修订 2：未接线 → `null`，见 §10）；`ts` 为命令层当前毫秒时间 |
| 形态 | `aether-tauri::core_health`：`HealthProvider`（只读报告）+ `CoreHealthBackend`（实现 `IpcBackend::health`，持有 `StoreRuntime` 生命周期锚点）；`IpcState` 经既有 managed state 的 backend 持有（不新增并行状态容器） |
| 生产启动 | `run()`：Tauri `setup` 内（**单实例插件初始化之后**，保持 T12「第二实例不建写连接」语义）执行 D2 启动序列的「库打开 + `quick_check`」（`StoreRuntime::open`）→ `EventPipeline::start` → 注册 `CoreHealthBackend`；空间护栏原生探针未接线（ADR-006 §5-2 口径：未知 → 不阻断），启动自检 `free_bytes` 按护栏下限传入 |
| 启动失败 | 安全模式/打开失败 → `degraded_backend`（`persist_degraded` + `integrity_failure` + `detail` 原因 + `degraded_since_ms`），**不回退 `not_implemented`**；启动门阻断（A4）时核心不启动，`health` 经 `backend_ready` 返回 `startup_blocked` |
| 语义不变 | 无参数严格解析（未知成员 `unknown_field`）；仅本地 IPC、不落库、不产生事件；UI 5s/15s 口径（M2-07） |
| 契约扩展 | `HealthReport` 增补可选 `detail`（启动/降级原因，面向诊断，不含密钥）；`degrade_trigger`/`degraded_since_ms` 仍为可选 |

### 9.3 决策 2 修订：撤销 `health.persist_attempt_log`，改测试侧 tracing 捕获

1. **移除**：`PipelineHealth.persist_attempt_log` 字段、`PERSIST_ATTEMPT_LOG_CAPACITY` 常量、计数环与 `record_persist_attempt`；相关测试断言同步删除。`health` 只表达存储状态与诊断计数，不承载日志内容。
2. **验证方式**：新增 `crates/aether-control/tests/m1_05_attempt_log.rs`，以 dev-dependency `tracing-subscriber` 注册内存捕获层（线程本地 `set_default` + 当前线程 runtime），捕获生产路径原样发出的 `tracing::warn!`；断言：
   - 2 失败 + 1 成功 → 恰有 `attempt=1/3`、`attempt=2/3`，无 `3/3`；
   - 连续 3 次失败 → `1/3`、`2/3`、`3/3` 各一次并含「进入 persist_degraded」；`attempt=4/3` 恒为 0。
   - `tracing-subscriber` 仅 dev-dependency（MIT），不进入运行期依赖图。
3. **P0 运行期日志汇聚端**：仍推迟到 **M2-07（与 M3-05 诊断包协同）**；P0 期间 `tracing::warn!` 无汇聚端不影响可验证性（由测试侧捕获完成），也不影响写失败重试口径语义。
4. `MAX_WRITE_ATTEMPTS = 3`（含首次；重试 2 次）口径不变（§2 决策 2）。

### 9.4 决策 3 补充：`ulid 1.1.3` 边界固化

1. **接受 1.1.3，不升级 3.x**（3.x 引入 rand 0.10 传递依赖变更，列为后续依赖评审项，见 §5-3）。
2. **生成侧**：新增属性测试 `first_char_fits_128_bits_over_100k_generations`——连续 10 万次生成，首字符恒 ≤ `7`、长度恒 26。
3. **解析侧**：接受 1.1.3「丢弃溢出高位、不报错」行为，并在单测中**显式断言**（`8…` → 全零 ULID；`9…` → 首位 `1` + 25 个 `0`）——升级 crate 时若语义变化，用例必须失败。
4. **已知非规范往返边界**：首字符 >7 的输入不是规范 ULID，解析后 `to_string()` ≠ 输入；生成侧首字符恒 ≤7 不受影响。若未来需要严格拒绝首字符 >7 的输入，须升级 crate 或加解析前校验（不得静默放宽）。

### 9.5 落地与证据（增量）

| 项 | 路径 / 证据 |
|---|---|
| 健康模块（Provider/Report/Backend/boot） | `crates/aether-tauri/src/core_health.rs`（含 2 个单测） |
| 生产启动接线 | `crates/aether-tauri/src/lib.rs`（`run()`：启动门 Ready → 库打开 → 管线 → backend） |
| health 命令真实返回 | `crates/aether-tauri/tests/health_command.rs`（normal / persist_degraded / 启动失败降级；3 用例） |
| 无参严格解析 | `crates/aether-tauri/tests/ipc_validation.rs`（`health` 未知成员 → `unknown_field`；缺省/空对象到达后端） |
| attempt 日志捕获 | `crates/aether-control/tests/m1_05_attempt_log.rs`（`tracing-subscriber` 捕获） |
| ULID 边界 | `crates/aether-control/src/ulid.rs`（10 万次生成属性测试 + 溢出显式断言） |
| 验证入口 | `node scripts/test/m1-05/verify-m1-05.mjs`、`node scripts/test/m1-08/verify-m1-08.mjs --skip-e2e` |

### 9.6 增量对文档的合入文本（v1.7 / v1.13）

> 先应用 ADR-006 附录 C/D（v1.5→v1.6 / v1.11→v1.12），再应用本节；两版状态行均按「待评审」标注，不伪造批准。

**设计文档（v1.6 → v1.7）**

- 头部：版本行/状态行升 **v1.7**（含 ADR-001–ADR-007；ADR-006/ADR-007 评审状态见各自 ADR）。
- 修订记录追加 v1.7：ADR-007 增量——`health` 真实接线提前至 M1-05 收口；撤销 `health.persist_attempt_log`；`ulid 1.1.3` 边界记录。
- D7：`health` 说明补充「P0 真实实现返回 `HealthReport`（含启动失败时的 `persist_degraded` 呈现），不返回 `not_implemented`；`detail` 为可选字段」。
- D2：交叉引用 `health` 由 D7 提供，UI 轮询在 M2-07。
- §2.2 选型表：`ulid` 行补注 1.1.3 解析侧边界与生成侧断言（实现回归选型，不新增决策点）。

**实施计划（v1.12 → v1.13）**

- 修订记录追加 v1.13（同步 ADR-007 增量）。
- M1-05 追加 DoD：`health` 真实返回（非 `not_implemented`；无参数；未知字段拒绝；`storage_state` 来自 `EventPipeline::health()`）；attempt 日志由测试侧 tracing 捕获且 `HealthReport` 不含日志字段；ULID 生成/解析边界断言。
- M1-08：IPC 校验矩阵中 `health` 用例更新为「真实命令，拒绝未知字段」。
- M2-07：DoD5 收窄为 **UI 每 5s 轮询 + 15s 超时「核心未响应」+ 两态 E2E**（不含命令接线）；新增 DoD6：P0 运行期日志汇聚端接线（与 M3-05 协同）。
- M3-05：新增 DoD——日志汇聚端与诊断包整合（与 M2-07 对齐）。
- Gate 2 显式检查项：`health` 真实返回证据；attempt 日志测试捕获证据；ULID 边界断言证据。
- §7 任务映射表同步。

### 9.7 增量回退条件

1. **生产启动构造管线带来启动时延/失败面扩大** → `run()` 保持「失败即降级呈现」；若评审认为启动不应打开存储，撤销 `run()` 接线并保留 `core_health` 模块与测试，接线任务精确挂 M2-01/M2-07（须记录理由）。
2. **`tracing-subscriber` dev-dependency 不合规** → 改用手写 `tracing::Subscriber` 捕获（无第三方 dev-dep），断言不变。
3. **`ulid` 边界断言在升级时失败** → 说明 crate 语义已变，按 §9.4-4 先补解析前校验再升级，不得放宽断言。

---

## 10. 增量修订 2（T12 顺序 `core_not_ready` 与 `health.runtimes` 语义，2026-09-18）

### 10.1 背景

1. 增量修订 1 将存储/管线打开推迟到 Tauri `setup`（单实例插件初始化之后，T12 语义），Builder 阶段以「延迟后端」`manage` 状态。`setup` 注入前的过渡窗口原先返回 `internal`：语义模糊、不可区分「核心未启动」与其他内部错误，且缺可执行测试。
2. `health.runtimes` 原先恒为数组：`[]` 同时表示「监督器未接线」与「已接线但无 runtime」，UI 无法区分（空数组语义歧义）。

### 10.2 决策

| # | 决策 | 落地位置 |
|---|---|---|
| 1 | **新增稳定错误码 `core_not_ready`**：Builder 阶段延迟后端注入前，业务命令（`backend_ready`）返回 `{code:"core_not_ready", message:"核心后端未就绪：启动序列尚未完成（存储/管线注入前）"}`；门命令（`startup_*`）不依赖后端，该窗口内仍可达；`setup` 注入后端后恢复正常；**第二实例在 `setup` 前退出、不触发该窗口**（T12 不变） | 设计文档 D7（health 行与错误码登记）；实施计划 M1-05 DoD9；`crates/aether-tauri/src/ipc/{error,mod}.rs`；`tests/health_command.rs::core_not_ready_until_backend_installed` |
| 2 | **`health.runtimes` 改为 `array \| null`**：`null` = 监督器**未接线**（`RuntimeSummarySource::summaries() → None`）；`[]` = 已接线且无 runtime；`[...]` = 状态快照。**不新增并行 `runtimes_source` 字段**——`Option` 直接编码「未知 vs 空」，避免双字段漂移；`boot_core_health` 采用 `unwired`（壳层尚未启动监督器，输出诚实状态） | 设计文档 D7；实施计划 M1-05 DoD9；`crates/aether-tauri/src/core_health.rs`（trait / `HealthReport` / `StaticRuntimeSummaries`）；`tests/health_command.rs`；`core_health.rs` 单测 |

### 10.3 影响

- 契约扩展：新增 1 个错误码 + 收窄 1 个字段语义；不改任何命令参数校验、事件类型、数据库与线协议语义；不新增运行期依赖（`StaticRuntimeSummaries` 替换此前的 `EmptyRuntimeSummaries`，属于同增量 1 内的内部改名）。
- 实施计划 M1-05 DoD9 措辞同步；Gate 2 显式检查项 6 覆盖（`core_not_ready` 过渡窗口 + `runtimes` 两态）。
- 设计文档 v1.7 草案内同步（D7 一行），不触碰 ADR-006 的启动命令契约。

### 10.4 证据

| 项 | 路径 / 输出 |
|---|---|
| `core_not_ready` 过渡窗口 | `health(core_not_ready) = {"code":"core_not_ready","message":"核心后端未就绪：启动序列尚未完成（存储/管线注入前）"}`；`tests/health_command.rs::core_not_ready_until_backend_installed`（含 `startup_get` 在窗口内可达、注入一次成功/重复注入拒绝） |
| `runtimes` 三态 | `health(normal) = {"runtimes":null,...}`；`health(wired-runtimes) = {"runtimes":[{"id":"mock","status":"ready"}],...}`；`core_health.rs::runtime_summaries_distinguish_unwired_and_wired`（null / `[]` / 条目透传） |

### 10.5 回退条件

1. **评审要求显式 `runtimes_source` 字段**（而非 `null` 语义）→ 在本增量内替换实现并在附录 A 修订契约（保持 `[]` 与 null 二选一的兼容说明），不得静默改变；两种表达不可并存漂移。
2. **`core_not_ready` 与既有 `internal` 分支冲突**（调用方已按 `internal` 分支）→ 保留 `internal` 并在附录 A 标注过渡口径（须记录理由）；不得删除已发布错误码。

---

## 附录 A：`health` 命令契约（目标契约；接线状态见 §3.3）

> 与增量修订 1（§9.2）冲突处以 §9 为准：真实接线已在 M1-05 收口完成；`detail` 为可选字段；启动失败返回 `persist_degraded` 而非错误。

### A.1 请求

- 命令名：`health`
- 参数：**无**（严格解析：`null`/缺省/空对象合法；任何成员 → `unknown_field`；非对象 → `invalid_json`）。
- 语义：仅本地 IPC 查询；**不落库、不产生事件**；不受存储降级影响（`persist_degraded` 时仍可查询，这正是降级通知路径）。

### A.2 返回 `HealthReport`

```json
{
  "storage_state": "normal | persist_degraded",
  "write_queue_depth": 0,
  "runtimes": [                                                    // array | null（增量修订 2）
    { "id": "mock", "status": "ready", "status_reason": null }     // null = 监督器未接线；[] = 已接线无 runtime
  ],
  "ts": 1758092000000,
  "degrade_trigger": "write_failure | space_guard | integrity_failure",   // 可选：仅降级时返回
  "degraded_since_ms": 1758091000000,                                     // 可选：仅降级时返回
  "detail": "启动/降级原因（面向诊断，不含密钥）"                          // 可选（增量修订 1）
}
```

| 字段 | 类型 | 来源（M1-05 收口接线，§9.2） |
|---|---|---|
| `storage_state` | `normal` / `persist_degraded` | `EventPipeline::health().storage_state`（`StorageState::as_str()`；ADR-004 决策 1 的唯一降级事实源） |
| `write_queue_depth` | int | `EventPipeline::health().journal_queue_depth`（D3 写队列深度） |
| `runtimes` | array / null | 监督器快照（M1-10）：`id` + `status`（`cold/starting/ready/degraded/disabled`）+ `status_reason`（可空）；**`null` = 监督器未接线**（增量 2），`[]` = 已接线且无 runtime |
| `ts` | int（Unix epoch 毫秒） | 命令层取当前时间（附录 E 时间口径） |
| `degrade_trigger` | string（可选） | `EventPipeline::health().degrade_trigger`（仅降级时返回） |
| `degraded_since_ms` | int（可选） | `EventPipeline::health().degraded_since_ms`（仅降级时返回） |
| `detail` | string（可选） | 启动/降级原因（增量修订 1：启动失败降级时携带；不含密钥） |

### A.3 UI 口径（D2）

- 每 5s 调用一次；连续 15s 无响应（3 次失败或调用超时）→ 显示「核心未响应」+ 重启入口；同时在正常/降级两态下分别渲染健康态与只读横幅（M3-03/M3-06 消费 `storage_state`/`degrade_trigger`）。

### A.4 校验矩阵（M1-08 DoD3）

| 样本 | 期望 |
|---|---|
| 缺省载荷（`null`）/ 空对象 `{}` | 通过校验，到达后端（真实接线后返回 `HealthReport`，§9.2） |
| 任意成员（如 `{"unexpected":1}`） | `unknown_field`（`field=unexpected`），不调用后端 |
| 非对象（数组/标量） | `invalid_json`，不调用后端 |

> 状态类错误（非参数校验）：启动序列过渡窗口（延迟后端注入前）返回 `core_not_ready`（ADR-007 增量 2；门命令 `startup_*` 不依赖后端仍可达）；启动门阻断返回 `startup_blocked`。

## 附录 B：写失败重试口径与测试矩阵

### B.1 口径

> 与增量修订 1（§9.3）冲突处以 §9 为准：attempt 日志验证改由测试侧 tracing 捕获；`health` 不含 `persist_attempt_log`。

| 项 | 取值 |
|---|---|
| 常量 | `MAX_WRITE_ATTEMPTS = 3`（`crates/aether-control/src/pipeline.rs`） |
| 语义 | **总尝试次数含首次**；重试次数 = `MAX_WRITE_ATTEMPTS - 1` = **2** |
| 降级条件 | 连续 3 次写事务尝试失败（含首次，逐次尝试间隔 `PERSIST_RETRY_DELAY`，默认 25ms） |
| 不入降级 | 第 1/2 次失败后成功；写队列临时高水位（`storage_backpressure`，ADR-004）；`evt.id` 幂等命中 |
| 日志 | `tracing::warn!` 输出 `attempt=1/3`、`2/3`、`3/3`（含错误详情；第 3 次附带「进入 persist_degraded」）；测试侧捕获见 `tests/m1_05_attempt_log.rs`（dev-dependency `tracing-subscriber`） |
| 降级后 | 未落盘事件不广播；在途 run 转 `cancelled`；拒绝新写入/新 run；仅修复 + 重启 + 自检恢复（P0 无热恢复） |

### B.2 测试矩阵（`crates/aether-control/tests/m1_05_degraded.rs`）

| 用例 | 断言 |
|---|---|
| `dod2_two_failures_then_success_does_not_degrade` | 3 次尝试（2 失败 1 成功）→ 落盘 seq=1；`storage_state=normal`；`persist_retries=2`；日志止于 `attempt=2/3`；`dropped_events=0` |
| `dod2_write_failure_retries_three_times_then_persist_degraded` | 恰好 3 次 append；日志 `attempt=1/3`→`3/3`；降级 + 只读；失败事件零广播；在途 run 收到中断；`persist_degraded` 拒绝后续写入/新 run |
| `dod6_temporary_high_water_stays_normal` | 临时高水位重试后回落正常落盘、持续高水位仅拒绝准入；两分支均 `storage_state=normal` |
| `m1_05_store_integration.rs::real_store_write_failure_degrades_while_reads_stay_available` | 真实写队列关闭 → 3 次尝试失败 → 降级；读路径可用、无部分写入 |

## 附录 C：拟议《设计文档》v1.6/v1.7 合入文本

> 与 ADR-006 的合入顺序无关：C.1/C.4 按批次选择行；同批合入时 C.1 两版合并为 v1.6，C.4 的 health 行追加到 ADR-006 的启动迁移命令块之后。

**C.1 头部（第 7 行、第 10 行；分批评审场景）**

```diff
-| 文档版本 | **v1.5（冻结）**；冻结后任何修改须走 ADR 并升版（v1.x）；本版含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005 |
+| 文档版本 | **v1.7（冻结）**；冻结后任何修改须走 ADR 并升版（v1.x）；本版含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005、ADR-006、ADR-007 |
-| 状态 | v1.5 已冻结（实现基线，2026-09-16 评审批准；含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005） |
+| 状态 | v1.7 已冻结（实现基线，评审批准日期见《ADR-007》§7；含 ADR-001–ADR-007） |
```

同批合入变体：两行版本号写 **v1.6**、ADR 列表含 ADR-006/ADR-007。

**C.2 修订记录（第 26 行之后追加；同批合入时与 ADR-006 的 v1.6 行合并）**

```diff
 | v1.5 | ADR-005：……（详 `docs/adr/ADR-005-…md`） |
+| v1.7 | ADR-007：M1-05 未决项收口——D7 新增 `health` 命令（无参数；`HealthReport`；仅本地 IPC、不落库/不产生事件；UI 5s/15s 口径）；D4 写失败重试口径统一为「连续 3 次写事务尝试失败（含首次；重试 2 次；日志 `attempt=n/3`）」；ULID 恢复 §2.2 选型 `ulid` crate（详 `docs/adr/ADR-007-m1-05-open-items.md`） |
```

**C.3 D4 措辞统一（第 315、322、327、330 行）**

```diff
-| 持久化失败（盘满/损坏） | 写队列报错 | 重试 3 次；仍失败 → 进入 `persist_degraded` + 只读（状态机见下）；拒绝新写入/新 run；**不广播未落盘事件**；在途 run 按取消处理 | 修复磁盘/目录后重启核心（P0 无热恢复） | D3 空间监控；只读导出/备份可用 |
+| 持久化失败（盘满/损坏） | 写队列报错 | 连续 3 次写事务尝试失败（`MAX_WRITE_ATTEMPTS = 3`，含首次；重试 2 次；日志 `attempt=n/3`）→ 进入 `persist_degraded` + 只读（状态机见下）；拒绝新写入/新 run；**不广播未落盘事件**；在途 run 按取消处理 | 修复磁盘/目录后重启核心（P0 无热恢复） | D3 空间监控；只读导出/备份可用 |
```

```diff
-normal ──写事务重试 3 次均失败──→ persist_degraded（只读）
+normal ──连续 3 次写事务尝试失败（含首次）──→ persist_degraded（只读）
```

```diff
-- 与写队列临时背压的边界（ADR-004）：写队列 >4096（L2）是**临时背压**——事务批量提交、毫秒级回落，队列回落即恢复，**不改变存储状态**；`persist_degraded` 只能由写事务连续失败、空间护栏（剩余 <500MB）或完整性失败触发，……
+- 与写队列临时背压的边界（ADR-004）：写队列 >4096（L2）是**临时背压**——事务批量提交、毫秒级回落，队列回落即恢复，**不改变存储状态**；`persist_degraded` 只能由写事务连续 3 次尝试失败（含首次）、空间护栏（剩余 <500MB）或完整性失败触发，……
```

```diff
-  - `persist_degraded`：写事务连续失败（重试 3 次）触发；语义等价只读模式。……
+  - `persist_degraded`：写事务连续 3 次尝试失败（含首次）触发；语义等价只读模式。……
```

**C.4 D7 命令面（第 435 行后追加；与 ADR-006 的启动迁移命令块并列）**

```diff
   - 命令面（P0 全集，ADR-004）：`runtimes_list`、……、`export_diagnostics`、`app_restart`；
+  - `health`（ADR-007）：无参数（严格解析：任何成员拒绝；`null`/缺省/空对象合法）；返回 `HealthReport`（`storage_state` / `write_queue_depth` / `runtimes` 摘要 / `ts`，降级时附 `degrade_trigger`/`degraded_since_ms`）；**仅本地 IPC，不落库、不产生事件**；UI 每 5s 轮询，15s 无响应显示「核心未响应」+ 重启入口；真实数据接线见 M2-07；
```

**C.5 D2 交叉引用（第 255 行）**

```diff
-  - UI 健康：UI 每 5s 调 `health`；15s 无响应显示「核心未响应」+ 重启入口。
+  - UI 健康：UI 每 5s 调 `health`（命令契约见 D7；ADR-007）；15s 无响应显示「核心未响应」+ 重启入口。
```

**C.6 §2.2 选型表：无文本变更**（`ulid` 原已在核心栈行；本 ADR 使实现回归选型，精确固定 1.1.3 的理由记录于 ADR-007 §2 决策 3；v0.2 增量按 §9.6 在选型表补注 1.1.3 边界，以 §9.6 为准）。

## 附录 D：拟议《实施计划与验收标准》v1.12/v1.13 合入文本

> 分批评审时在 ADR-006 合入后应用；同批合入时版本号写 v1.12、修订记录与 ADR-006 行合并、其余文本直接应用。

**D.1 头部与 §8 基线（第 5–8 行、第 607 行）**

```diff
-| 计划版本 | v1.11 |
-| 状态 | v1.11 已冻结（2026-09-16 评审批准）；任务进度可更新，结构与门禁变更走 ADR |
-| 基线 | 设计文档 v1.5（冻结，含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005） |
+| 计划版本 | v1.13 |
+| 状态 | v1.13 已冻结（评审批准日期见 ADR-007 §7）；任务进度可更新，结构与门禁变更走 ADR |
+| 基线 | 设计文档 v1.7（冻结，含 ADR-001–ADR-007） |
```

```diff
-- **变更控制**：设计文档已冻结 v1.5（含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005）；……
+- **变更控制**：设计文档已冻结 v1.7（含 ADR-001–ADR-007）；……
```

**D.2 修订记录（第 30 行 v1.11 行之后追加）**

```diff
 | v1.11 | 评审 C1/C2 修订（ADR-005）：…… |
+| v1.13 | 同步设计文档 v1.7（ADR-007）：M1-05 补写失败重试口径（`MAX_WRITE_ATTEMPTS = 3` 含首次 + `attempt=n/3` 日志）与 ULID 选型测试（DoD7/DoD8）；M1-08 DoD3 校验矩阵纳入 `health`（无参数严格解析）；M2-07 新增 DoD5（`health` 接线/UI 5s/15s/E2E 两态）；Gate 2 增写失败重试口径与 `health` 显式检查项；§6 #10、§7 同步 |
```

**D.3 M1-05（第 155–161 行）**

```diff
 - DoD：
   1. 乱序/重复注入（10k 事件属性测试）：seq 单调唯一；
-  2. 故障注入 journal 写失败：重试 3 次失败 → `persist_degraded` + 只读；拒绝新写入/新 run；**未落盘事件不广播**（调用序断言）；在途 run 转 `cancelled`；
+  2. 故障注入 journal 写失败：**连续 3 次写事务尝试失败（`MAX_WRITE_ATTEMPTS = 3` 含首次；重试 2 次；日志 `attempt=1/3`…`3/3`）** → `persist_degraded` + 只读；拒绝新写入/新 run；**未落盘事件不广播**（调用序断言）；在途 run 转 `cancelled`；
   3. delta 合并阈值生效；`message.completed` 终稿不受合并影响；
   4. 补读：last_seq 缺口补齐；>10k 拒绝自动补发并返回明确错误码（单测）；
   5. sequencer 崩溃恢复：重启后 seq = max+1，无重复无缺口（单测）；
   6. 降级状态机：进入/退出转移断言（进入 = 写失败/空间护栏/完整性失败；退出 = 修复外部条件 + 重启核心 + 启动自检）；**写队列临时高水位（≤L2）不得进入本状态**（与 D8 背压区分）；降级通知经 `health` 返回 `storage_state=persist_degraded`（落盘成功时才广播 `error` 事件）；P0 无热恢复（单测/集成）。
+  7. 重试口径行为断言：第 1、2 次失败、第 3 次成功 → **不降级**且日志止于 `attempt=2/3`；连续 3 次失败 → 降级且日志含 `attempt=1/3`…`3/3`（单测/故障注入）；
+  8. ULID（ADR-007 决策 3）：实现使用 §2.2 选型 `ulid` crate（生成/解析统一经 crate）；生成 26 字符 Crockford、唯一性、时间戳在界、解析往返与非法输入拒绝（长度/非法字符/溢出边界）（单测）。
```

**D.4 M1-08 DoD3（第 188 行；与 ADR-006 的 D.1 文本合并应用）**

```diff
-  3. IPC 校验框架：畸形参数样本集（超长/未知字段/非法枚举）全部返回结构化错误且不落库（单测矩阵）；**新增命令 `backup_list`/…/`workspace_set` 全部纳入校验矩阵**（含 `run_id` ULID、终态、白名单、`confirm:true`、路径 canonicalize 与同步盘拒绝）；
+  3. IPC 校验框架：畸形参数样本集（超长/未知字段/非法枚举）全部返回结构化错误且不落库（单测矩阵）；**新增命令 `backup_list`/…/`workspace_set` 全部纳入校验矩阵**（含 `run_id` ULID、终态、白名单、`confirm:true`、路径 canonicalize 与同步盘拒绝）；**ADR-007：`health` 纳入校验矩阵（无参数严格解析：`null`/缺省/空对象合法；任何成员 `unknown_field`；非对象 `invalid_json`）**；
```

**D.5 M2-07（第 316 行 DoD4 之后追加 DoD5）**

```diff
   4. UI 15s 无响应 → 「核心未响应」+ 重启入口（E2E）。
+  5. `health` 命令接线（ADR-007）：返回 `HealthReport`（`storage_state` 映射 `EventPipeline::health()`、`write_queue_depth`、runtimes 摘要取监督器状态、`ts`；降级附 `degrade_trigger`/`degraded_since_ms`）；UI 每 5s 轮询；15s 无响应显示「核心未响应」+ 重启入口；E2E 覆盖 `storage_state=normal` 与 `persist_degraded` 两态（降级横幅与发送入口禁用联动 M3-03/M3-06）。
```

**D.6 Gate 2（第 361–364 行之后追加检查项）**

```diff
 - 通过条件（分条判定）：
   …
   4. T5a/T5b/T6/T7/T11 作为门禁显式检查项，需在评审时逐条出示证据（而非仅依赖任务 DoD 自检）。
+  5. **M1-05 重试口径**：`MAX_WRITE_ATTEMPTS = 3`（含首次）行为证据（2 失败 1 成功不降级；3 次失败降级）+ `attempt=n/3` 日志证据为显式检查项。
+  6. **`health` 两态**：M2-07 的 `health` 接线与 `normal`/`persist_degraded` E2E 证据为显式检查项（含 15s 无响应提示路径）。
```

**D.7 §6 覆盖矩阵 #10（第 535 行）**

```diff
-| 10 | D4 | 持久化失败 | M1-05、M3-06 | M4-02 | 重试 3 次 → `persist_degraded` + 只读 + 拒新写入/run + 未落盘不广播 + 在途 run cancelled；恢复 = 修复外部条件 + 重启核心 + 启动自检（无热恢复；与 #8 区分） |
+| 10 | D4 | 持久化失败 | M1-05、M3-06 | M4-02 | 连续 3 次写事务尝试失败（含首次；日志 `attempt=n/3`）→ `persist_degraded` + 只读 + 拒新写入/run + 未落盘不广播 + 在途 run cancelled；恢复 = 修复外部条件 + 重启核心 + 启动自检（无热恢复；与 #8 区分） |
```

**D.8 §7 映射表（第 567、570、581 行）**

```diff
-| M1-05 | D4 | CP-04 | 乱序/重复/补读/sequencer（D4 失败表 3 项） |
+| M1-05 | D4、ADR-007 | CP-04 | 乱序/重复/补读/sequencer（D4 失败表 3 项）；写失败重试口径（3 次含首次 + `attempt=n/3`）；ULID 选型测试 |
-| M1-08 | D1、D7、评审#7 | — | CSP/导航/IPC 校验；§2.4 底线 |
+| M1-08 | D1、D7、评审#7、ADR-007 | — | CSP/导航/IPC 校验（含 `health` 无参数矩阵）；§2.4 底线 |
-| M2-07 | D2 | — | panic 隔离；RSS 阈值（D2 失败表） |
+| M2-07 | D2、D7、ADR-007 | — | panic 隔离；RSS 阈值（D2 失败表）；`health` 接线 + UI 5s/15s + 两态 E2E |
```

## 附录 E：实现与证据索引

| 内容 | 路径 / 命令 |
|---|---|
| 管线健康数据源（`storage_state`/写队列深度/诊断环） | `crates/aether-control/src/pipeline.rs`（`PipelineHealth`/`health()`） |
| 重试口径与 attempt 日志 | `crates/aether-control/src/pipeline.rs`（`MAX_WRITE_ATTEMPTS`/`PERSIST_ATTEMPT_LOG_CAPACITY`/`write_with_retry`） |
| ULID（选型实现） | `crates/aether-control/Cargo.toml`（`ulid = "=1.1.3"`）、`src/ulid.rs` |
| 管线测试 | `cargo test -p aether-control`（`tests/m1_05_{pipeline,degraded,store_integration}.rs`） |
| M1-05 验证入口（含证据归档） | `node scripts/test/m1-05/verify-m1-05.mjs`（证据：`scripts/test/.tmp/m1-05/<ts>/`） |
| `health` 命令骨架与矩阵 | `crates/aether-tauri/src/ipc/{dto,backend,commands}.rs`；`tests/ipc_validation.rs`（`health` 样本） |
| M1-08 验证入口 | `node scripts/test/m1-08/verify-m1-08.mjs`（命令定义/注册静态检查含 `health`） |
| 依赖合规 | `cargo deny check licenses bans sources`（`ulid`/`tracing` 白名单内） |
