# ADR-039：M10-08 事件分库与启动完整性检查优化（A：分库 + 并行检查）

| 项 | 内容 |
|---|---|
| 编号 | ADR-039（M10-08 触发；v1.0 取代 v0.1–v0.5） |
| 版本 | **v1.0（2026-11-14，四角色签署）** |
| 状态 | **已签署（架构/安全/QA/产品，2026-11-14）**；v0.4 四审 Conditional Pass，v0.5 落实 P2-1～P2-4，v1.0 完成四角色签核 |
| 决策日期 | 2026-11-14（v1.0 签署）；2026-11-14（v0.5）；2026-11-14（v0.4）；2026-11-14（v0.3）；2026-11-14（v0.2）；2026-11-12（v0.1 草案） |
| 决策载体 | 《设计文档》**v1.35（待升版）**；`docs/P2-架构迁移-实施计划与验收标准.md` **v0.14（待升版）**；`docs/ADR-039-评审意见.md`；`docs/ADR-039-复审意见.md`；`docs/ADR-039-三审意见.md`；`docs/ADR-039-四审意见.md` |
| 关联 | ADR-038（M10-08 方法与 30s 分库阈值）、ADR-014 §7 R16、ADR-021（保留/归档）、ADR-020（备份/加密）、ADR-029（Electron 壳 + `aetherd` 启动/生命周期）、ADR-034（维护窗口/导入导出原子性）、ADR-035（附件边界）、ADR-003/ADR-004（存储降级/写队列边界）、《设计文档》D3/D4/D7/D13/A10/附录 B/附录 C、M10-06 启动体验补充口径、`docs/M10-08-证据.md` |
| 取代 / 被取代 | 本 ADR **v1.0 取代 v0.1–v0.5**；**不取代 ADR-038**。分库实施落在 M10-09/M10-10/M10-11，本 ADR 冻结决策、规格、契约与验收；导入/备份扩展需同步修订 ADR-034/ADR-020（见 D-039-7） |
| 回退条件 | 见 §7 |
| 实现状态 | **已解锁**（可进入设计 v1.35 / P2 v0.14 升版与 M10-09/10/11 实施；G13 仍需实施证据与独立验收）；**M10-08 不重开**，保持“只验证不实施”证据不变 |

> 本稿依据四份评审意见改写，保留唯一方案 **A：事件分库 + 启动并行完整性检查**；v0.1 的“候选/首选/优先”矛盾表述已删除，“仅启动优化、不分库”仍只作为 §5 已否决备选。v0.4 四审文本通过；v0.5 落实 P2-1～P2-4；**v1.0 = v0.5 文本 + 架构/安全/QA/产品四角色签署，无内容变更**。

---

## 0. 评审裁定与 v1.0 响应

| 编号 | 2026-11-14 用户裁定 | v1.0 落点 |
|---|---|---|
| R1 | **选 A：分库 + 并行完整性检查** | §2.0、D-039-1～D-039-3；§5 仅保留为已否决备选 |
| R2 | UI ready 预算 **1.5s**；定义为“壳层可见 + 首个 `health` 成功” | D-039-3、§6.1、§6.3 |
| R3 | 分库写模型 = **单写任务 + 每片连接** | D-039-1 |
| R4 | **没有线上版本，不做数据迁移** | D-039-6、§7 |
| R5 | D13 多库备份/恢复 = **维护态备份**：操作独占，除 `health`/`app_exit` 外其他命令不可用 | D-039-5、§4、§6.3 |
| R6 | 干净关闭判据 = **严格 sidecar + 混合策略** | D-039-2 |
| R7 | 保留清理与收缩 = **两阶段 + 受控批事务 + incremental auto_vacuum** | D-039-4 |
| R8 | 性能 **真实复测**；参数按评审意见建议值 | D-039-2、§6.4 |
| R9 | `health` 增加 `storage_check`/`maintenance` **可选字段**；**不新增设置键**；实现放 M10-09/M10-10，不重开 M10-08 | D-039-5、§3、§6.1 |
| R10 | 后续任务 **M10-09 / M10-10 / M10-11 + G13**；**独立架构/安全/QA/产品签核** | §6.2、§6.6、§8 |

> 以上裁定为本次评审的最终产品/架构输入。修改者不得自行改变；如实现中发现不可行，按 AGENTS §7 停下来报告并另立 ADR，不得静默降级。

### 0.2 v0.2 复审问题与 v0.3 响应

| 复审编号 | 问题摘要 | v0.3 落点 |
|---|---|---|
| P0-1 | `uiReadyAt` 定义与 S-00/写门冲突 | D-039-3：`uiReadyAt=壳层可见+首个 health`（不要求可写）；新增 `mainInteractiveAt`/`writesEnabledAt`；S-00 保留；§6.3 分列验收 |
| P0-2 | 检查中写门复用 `storage_backpressure`，与 D4/UI S-08 冲突 | D-039-3/D-039-5：新增 `storage_check.write_gate`（`checking` / `open`）；D4 明确为启动准入阶段而非降级；UI 以 `storage_check` 优先；错误映射表定稿 |
| P0-3 | 跨库导入/会话删除原子性未设计 | D-039-8：`sessions.deletion_pending` tombstone + 启动重试；导入 staging 整套 generation + 指针原子切换；故障注入 |
| P0-4 | `backup_list`/`backup_restore` 契约未冻结 | D-039-5：冻结 `backup_create`/`backup_list`/`backup_restore` 请求/响应字段、旧 `.db` 拒绝策略、M9-01 重录 |
| P1-1 | `initialize` 2s 与 `uiReadyAt` 1.5s 冲突 | D-039-3：5.22GiB 路径 `initialize` 目标 ≤1.5s、硬超时 10s；删除 2s 目标；P2 矩阵同步 |
| P1-2 | §5 把未越线指标说成单库瓶颈 | §1.1/§5：备份/恢复/补读改为“支持性收益”，硬触发仍是 `quick_check` >30s；物理高水位独立风险 |
| P1-3 | 整套恢复原子切换未定义 | D-039-8：generation 目录 + `CURRENT` 指针 + 同卷 staging + fsync + journal + 整套回滚；恢复中断注入 |
| P1-4 | 启动不变量错误映射未定义 | D-039-6：条件 → `IpcErrorCode`/`business_code`/exit 22/S-12 `data-reason`/UI 文案表；不新增 reason |
| P2-1 | 读连接数公式矛盾 | D-039-1：主库 4 + 分片 ≤16，总 ≤20 |
| P2-2 | 分片库 schema/0014 表述歧义 | D-039-1/附录 A/E：分片库执行完整 0001–0014（所有表物理存在），只读写 `events`/`schema_migrations` |
| P2-3 | `clean_start_count` 重置未定义 | D-039-2/附录 B：异常/不匹配/失败/恢复/导入/迁移重置 0；仅干净匹配 +1 |
| P2-4 | `checked_shards` 不含主库的 UI 语义 | D-039-5/附录 C：新增 `main_checked`；主库失败显示“主库校验失败” |
| P2-5 | 维护态表缺 `persist_degraded` 行 | D-039-3/附录 C：增加 `persist_degraded + maintenance` 行；仍仅 `health`/`app_exit`，恢复后重启 |

> v0.2 的 A 方向、N=8/1.5GiB、单写任务 + 每片连接、无数据迁移、严格 sidecar、两阶段清理、`health` 可选字段、0 新增设置键、M10-09/10/11 任务划分与四角色签核要求保持不变（见复审意见 §5.2）。

### 0.3 v0.3 三审问题与 v0.4 响应

| 三审编号 | 问题摘要 | v0.4 落点 |
|---|---|---|
| P0-1 | “数据目录为空”与 Electron 预先创建 `logs/` 冲突，首启被误判为旧布局 | D-039-1/D-039-6/附录 A：改为 **DB 布局工件**为空才新装；忽略 `logs/backups/attachments/staging/run`；补 fresh userData（含 logs）首启测试 |
| P1-1 | `backup_restore` 外部路径仍只接受 `.db`/`.aebk` 文件 | D-039-5 §4：定义 `external.path` 为备份集目录或 `manifest.json`；`validate_external_backup_set`；旧 `.db`/`.aebk` 显式拒绝；DTO/路径校验/M10-09 测试 |
| P1-2 | 旧 generation 缺省保留/自动清理/容量告警未定义 | D-039-8 §1/§7：成功后保留最近 1 份旧 generation；更早 `.pre-*` 安全自动清理；容量巡检与诊断包；故障注入 |
| P1-3 | `CURRENT.generation`/`shards.json.path`/manifest 路径缺少目录约束 | D-039-1/D-039-5/D-039-8：generation 正则 + `generations/` 前缀；相对路径、无 `..`/绝对路径；symlink/junction 拒绝；错误映射与故障注入 |
| P2-1 | 附件不在 generation/备份集，恢复后可能引用缺失附件 | D-039-5/D-039-8：恢复成功页/审计/诊断明确“附件未随库恢复”；附件备份另立 ADR，不静默成功 |
| P2-2 | `deletion_pending` 非 0/1、`CURRENT.format_version` 不支持、`shards_sha256` 不匹配未入映射 | D-039-6 表 + 0014 `CHECK` + 启动断言：统一 `db_open_failed`；补测试 |
| P2-3 | 旧 `single_file` 记录仅展示、UI 可能仍可恢复 | D-039-5 §4：UI S-06 禁用恢复并提示“请使用整套备份或重建”；契约/UI 测试 |
| P2-4 | Windows `CURRENT` 原子替换语义未固定 | D-039-8 §1：`ReplaceFileW`/`MoveFileExW` + `FlushFileBuffers`；禁止删除后 rename；M10-09 证据固定 API/失败注入 |
| P2-5 | `shards.json`/manifest 的 generation 字段未统一 | D-039-1/D-039-5/D-039-8：`shards.json` 不含 `generation`，`CURRENT` 为唯一绑定；备份 manifest 仅记 `source_generation`（信息性）；`clean_shutdown.generation` 必须匹配 `CURRENT` |

> 三审结论为“仅差 1 项 P0 + 3 项 P1；修正后可签署”。v0.4 不改变 A 方向、任务划分与 v0.3 已闭环内容。

### 0.4 v0.4 四审 P2 与 v0.5 响应

| 四审编号 | 问题摘要 | v0.5 落点 |
|---|---|---|
| P2-1 | `backup_restore` staging 空间预检未明确 | D-039-5 §5：恢复前预检 `<data_dir>/staging` 可用空间 ≥ 备份 generation 大小 ×1.1；不足拒绝并保留当前库；M10-09 DoD |
| P2-2 | `.failed-*` generation 未纳入清理/诊断 | D-039-8 §1：`.failed-*` 与 `.pre-*` 同等纳入安全清理/诊断；保留最近 1 份或标记可删；不得删除当前/journal 引用 |
| P2-3 | `archive_export` 对 `deletion_pending=1` 会话处理未明确 | D-039-8 §2：`archive_export` 排除 pending 会话（或标记 `pending_deletion`）；`session_export` 已隐藏；补测试 |
| P2-4 | 首启“有 `generations/`、无 `CURRENT`”仅 fail-closed，无提示/清理入口 | D-039-6/附录 A：S-12 文案/诊断提示删除半成品 generation 或重建；诊断标记 `incomplete_generation`；不自动切换/删除 |

> 四审结论为 **Conditional Pass**：无剩余 P0/P1；P2-1～P2-4 已在 v0.5 落实。**四角色签核已于 2026-11-14 完成（§8），v1.0 已签署**；文本/签署通过不等于 G13 通过，M10-09/10/11 实施证据与 G13 报告仍需独立验收。

---

## 1. 背景与触发

### 1.1 M10-08 实测证据

M10-08 按 ADR-038 在 **5.22 GiB / 20,000,000 行 / 100 会话** 合成 `events` 数据集上完成压测（Windows x64；Intel i5-13400 10C/16T；16 GiB RAM；NVMe NTFS；Node `node:sqlite` 3.51.2；产品侧 `rusqlite 0.41.1` bundled）。数据集 `sha256=00834075d129ac808d4292610cdafc6e9864420ca6d1eaf5fef5ae4dbcc93895`；`db=5,604,716,544 B`（5.22 GiB），`wal=0`，平均 280.24 B/行。

**关键实测（启动/单库/分片为 3 次中位；保留清理为 run1 的 50,000 行采样；完整数据见 `docs/M10-08-证据.md`、`docs/M10-08-metrics.json`、`docs/M10-08-quick-check-parallel.json`）：**

| 指标 | 单库 | 8 分片 | 说明 |
|---|---:|---:|---|
| 启动 `quick_check`（冷） | **97,849.350 ms**（83,038.132–112,095.784 ms） | 顺序总和 **72,861.791 ms**；并行墙钟 **19,357.664 ms**（15,649.815–20,621.302 ms） | 单库/顺序均 >30s；并行 <30s |
| 写 P95 | 19 ms | 19 ms | 不变；不作为分片理由 |
| 补读 P95 | 95.719 ms | 62.284 ms | −34.93% |
| 备份（`VACUUM INTO`） | 163,850.679 ms | 51,407.972 ms | −68.63% |
| 恢复 | 82,257.841 ms | 38,016.256 ms | −53.78% |
| 启动 `windowShownAt - startedAt` | 中位 **1,536 ms** / max **1,780 ms** | — | >1.5s 预算 |
| 启动 `uiReadyAt - startedAt` | 中位 **101,065 ms** / max **127,249 ms** | — | >1.5s；被 5.22 GiB `quick_check` 阻塞 |
| 保留清理批（5,000 行） | P95 **4,891.750 ms** / max 4,911.754 ms（run1 50k 采样） | — | >D3 单事务 500ms；约 1,122.5 行/s |
| 物理库 / live 投影 | **5.22 GiB** / 1.387 GiB | 每片约 0.65 GiB | 物理高水位不自动收缩 |

**加速比口径（必须区分）：**

- **5.35×** = 并行运行内部“8 片顺序检查总和” / “并行墙钟” = 103,471.248 ms / 19,357.664 ms（均为 3 次中位）；它衡量“并行相对串行”的内部收益，**不是**与单库 `quick_check` 的同口径加速比。
- **3.76×** = 同口径顺序基线 72,861.791 ms / 并行墙钟 19,357.664 ms。**产品/验收表述统一使用 3.76×，不得用 5.35× 代表产品整体加速。**
- 8 分片并行墙钟中位 19.36s、max 20.62s；这是本机 NVMe、冷-ish（未清 OS 文件缓存）参考值，必须按 §6.4 用 release daemon / 真实 Electron 复测。
- **判定口径**：ADR-038 的硬触发只有 `quick_check` >30s；备份/恢复/补读是分库的**支持性收益**，三项在 M10-08 判定表中均通过阈值（备份 163.85s ≤600s、恢复 82.26s ≤900s、补读 95.7ms <150ms）；物理高水位是独立容量风险，不作为 ADR-038 硬触发。

**其他触发事实：**

1. **ADR-038 D-038-2/D-038-3**：5 GiB 单库冷启动 `quick_check` >30s，判定“需分库”。
2. **M10-06 启动体验补充口径（2026-11-12）**：真实 Electron + `aetherd` 5 GiB 冷启动 `windowShownAt` 中位 1,536ms、`uiReadyAt` 中位 101,065ms，均 >1.5s，必须另立优化 ADR。M10-08 启动 E2E 使用 `target/debug/aetherd.exe`，**不能作为 release 验收证据**。
3. **保留清理**：5,000 行/批 P95 4.89s、1,122.5 行/s、全量 14,684,946 行线性外推约 3.63h；产品 `select_retention_batch` 的 `ORDER BY rowid` 在 5 GiB 上走 TEMP B-TREE（O(N²)），是独立性能风险。
4. **容量高水位**：物理库 5.22 GiB >5 GiB；30 天保留后 live rows 投影约 1.387 GiB，但**不 `VACUUM` 时物理文件保持高水位**；`VACUUM` 耗时/空间未测。
5. 写/恢复/`quick_check` 失败计数均为 0；未以平均值掩盖长尾。

### 1.2 判定与触发

- M10-08 按 ADR-038 D-038-3 二选一判定为 **“需分库”**；该结论与证据保持不变，**M10-08 不重开**。
- 同时触发 M10-06 的 1.5s 启动预算，因此本 ADR 的决策不是“只分库”或“只优化启动”，而是 **A：分库 + 并行完整性检查**，并把保留清理、维护态备份、契约与验收一并冻结。
- 本 ADR 只冻结决策与实施规格；M10-09/M10-10/M10-11 完成后，G13 才具备退出条件。**ADR-039 v1.0 已由四角色签署**，可进入设计 v1.35 / P2 v0.14 升版与 M10-09/10/11 实施；G13 仍需实施证据与独立验收。

### 1.3 开发阶段约束与证据边界

- **没有线上版本，不做数据迁移。** 当前为开发阶段，不存在需要兼容的已发布用户库；只做**新装初始化**与**开发库重建**，不做一次性/惰性迁移、不做双读、不保留运行时单库回退窗口。0001–0013 已发布迁移不得修改；新增 0014 只服务新装主库。
- **平台边界**：参考环境仅 Windows x64 / NVMe / NTFS；Windows SATA SSD/HDD 只做冒烟，HDD 上 10 并发写 P95 66–81ms 已越线，不作为参考判定环境。macOS/Linux 未执行；有环境则补，无法执行必须在证据中写明。
- **冷启动边界**：未执行 OS 文件缓存清空，参考值为“生成/复制后首次检查”的冷-ish 值；Rust 真实 daemon 独立启动仍约 86–127s，缓存不足以把结论拉回 30s/1.5s 内。
- **实现差异**：压测子进程用 Node `node:sqlite` 3.51.2；产品 `rusqlite 0.41.1` bundled。真实 daemon 启动与子进程结果同量级，未发现实现差异导致结论反转；release 复测仍必须执行。
- **字段语义**：`docs/M10-08-metrics.json` 的 `startupRequiresAdr:false` 仅表示“当时缺少独立优化 ADR”，`withinTarget:true` 仅指 `initialize` 目标；两者**不改变** M10-08 的“需分库/启动越线”判定，也不改变 G13 对 ADR-039 的要求。M10-08 证据只补充字段说明，不重跑、不改判定。

---

## 2. 决策

### 2.0 唯一决策 A：分库 + 并行完整性检查

**采用 A，且只采用 A。** 具体包含五组不可分割的决策：

1. **事件分库**：主库保留 `sessions`/`messages`/`runs`/`audit_log` 等；`events` 按会话路由到 8 个分片库；主库 `events` 表永久为空。写模型为**单写任务 + 每片连接**（D-039-1）。
2. **启动并行检查**：启动不再阻塞等待 `quick_check`；异常退出/严格 sidecar 不匹配时并行检查主库 + 全部分片，干净关闭时跳过阻塞检查并后台补检；目标 25s、硬上限 30s（D-039-2）。
3. **启动预算与可见状态**：`windowShownAt` 与 `uiReadyAt` 均 ≤1.5s；`mainInteractiveAt`/`writesEnabledAt` 单独度量；`health` 在检查完成前返回 `storage_check.state=checking`，检查中不是降级（D-039-3、D-039-5）。
4. **保留清理、文件收缩与维护态备份**：两阶段清理、受控批事务、`incremental auto_vacuum`；备份/恢复维护态独占；契约只增加 `health` 可选字段、`backup_*` 字段增量和 0014 迁移（D-039-4、D-039-5、D-039-6）。
5. **跨库维护协议**：代际目录 + `CURRENT` 指针；会话删除 tombstone、导入 staging 整套切换、恢复整套原子切换；失败可重试/整套回滚（D-039-8）。

> “仅启动优化、不分库”已在 §5 明确否决；它不是并行实施分支，也不得在实现中作为降级路径。

### D-039-1 分片布局、写模型与读路由

#### 1. 文件布局与分片数

- **代际布局**：`<data_dir>/CURRENT`（原子指针，JSON）→ `<data_dir>/generations/<generation_id>/`；每个 generation 内是完整一套：`aether.db`（主库）、`events/events-0.db` … `events-7.db`、`events/shards.json`、`clean_shutdown.json`。备份/恢复/导入以 generation 为原子单位（D-039-8）。
- `CURRENT` 至少包含 `format_version=1`、`generation`、`shards_sha256`（`events/shards.json` 的哈希）、`switched_at`；启动必须校验指针指向的 generation 存在且完整。
- **路径约束（P1-3）**：`generation` 必须匹配 `^gen-[0-9A-Z]{26}$`；`<data_dir>/generations/<generation>` canonicalize 后必须前缀匹配 `<data_dir>/generations/`；symlink/junction/reparse point 解析到根外一律拒绝（解析后仍在根内才允许）。`shards[].path` 只允许相对 generation 根的 `events/events-<0..7>.db`；禁止绝对路径、`..`、空段。
- **generation 绑定（P2-5）**：`shards.json` **不含** `generation` 字段；`CURRENT.generation` 是唯一绑定。`clean_shutdown.json.generation` 必须与 `CURRENT` 一致；备份 manifest 的 `source_generation` 仅信息性，不作为路径/绑定依据。
- **首启判定（P0-1）**：以 **DB 布局工件**为准，不以“数据目录是否为空”为准。DB 布局工件 = `<data_dir>/aether.db`、`<data_dir>/CURRENT`、`<data_dir>/generations/` 任一存在；仅存在 `logs/`、`backups/`、`attachments/`、`staging/`、`run/` **不算** DB 布局工件。三类均不存在 → 新装初始化；存在任一 DB 布局工件但 `CURRENT` 缺失/损坏 → 拒绝启动（错误映射见 D-039-6）。Electron 在 spawn 前创建 `logs/shell.log` 不得触发 S-12。
- **N = 8** 固定；5.22 GiB 数据集每片约 0.65 GiB；**单片设计上限 1.5 GiB**。达到/超过 1.5 GiB 时触发容量告警并另立 **resharding ADR**，不自动拆分、不在本 ADR 内扩容。
- 主库保留 `sessions`、`messages`、`runs`、`audit_log`、`settings`、`providers` 等；**事件只写分片库**。
- 主库保留 `events` 表（兼容迁移链/诊断/《设计文档》附录 C），但**永远为空**。启动断言主库 `events` 行数为 0；若不为 0 → **拒绝启动**（S-12；`IpcErrorCode::Internal` + `business_code=db_open_failed` + exit 22 + S-12 `data-reason=db_open_failed`；完整映射见 D-039-6），不得静默忽略、不得回退读主库 `events`。M10-09 证据必须固定该行为。

#### 2. 分片初始化与 0014

- 新装初始化：创建 generation 目录，主库 + 8 个分片全部创建、迁移、打开成功，原子写 `events/shards.json`，最后原子写 `CURRENT` 指针；任一步失败 → 删除半成品 generation/staging 并报错；不得留下“半个库”。详见 D-039-6。
- 分片库执行**完整 0001–0014**（所有表物理存在，含 `sessions`）；分片业务只读写 `schema_migrations` 与 `events`（含 `idx_events_session_seq_uq`、`idx_events_type_ts`）；其余表不参与分片业务；**不建专用 shard schema、不跳过 0014、不建第二套迁移框架**。
- 新增 **0014**（仍为 1 个迁移）：`sessions.shard_id INTEGER` + `sessions.deletion_pending INTEGER NOT NULL DEFAULT 0 CHECK (deletion_pending IN (0,1))` + 两个索引。`shard_id` 新会话写入，启动校验非空且 `0..7`；`deletion_pending` 只服务 D-039-8 的跨库删除 tombstone。分片库物理执行 0014（列存在但不参与分片业务），以保持 `schema_version` 一致；《设计文档》附录 C 必须写明。**不得修改 0001–0013。**
- 无线上数据迁移；开发库直接重建（D-039-6）。不保留一次性/惰性迁移、双读、单库回退窗口。

#### 3. 路由与会话不跨片

- `shard_id = shard_for(session_id)`；使用**稳定哈希**（`session_id` 的 UTF-8 字节做 FNV-1a 64 mod N；不得使用进程随机化哈希），算法与固定测试向量写入《设计文档》D3/附录 C。
- 一个会话的**全部事件永远在同一片**；**会话不跨片**。新会话由 `shard_for` 计算，并在**与 `sessions` 插入同一事务**中写入 `sessions.shard_id`；写入时断言 `shard_for(session_id) == shard_id`；既有会话以该列为准；未来 resharding 更新列与事件位置须另立 ADR。
- `UNIQUE(session_id, seq)` 为**片内约束**；保留**会话 sequencer**；**不需要也不得声称“全局单调 seq”**。事件广播顺序仍按会话内 `seq`，不因分片改变。

#### 4. 写模型（R3：单写任务 + 每片连接）

- 保留全局 `mpsc` 写队列（容量 4096）与**单一写任务**；单写任务持有 N 个分片写连接。**不做每片独立写者**，保持 D3 单写者语义。
- 按 `session_id` 路由；每片 group commit（16ms 或积压 ≥256 条）；**不做跨片事务**；不存在跨片原子提交需求。
- 事件写入分片成功后才广播（D4“先日志后广播”不变）；主库 `messages` 终稿写入是独立事务；**事件先落盘，消息终稿随后**；失败按现有写失败路径处理（连续 3 次尝试失败 → `persist_degraded`）。
- 单写任务仍是所有分片的唯一写者；不得以“分片”为由引入第二写者或跨片长事务。

#### 5. 读模型

- `events_page` / `events_latest` / `max_seq` 按 `session_id` 路由到片；`messages_page` = 主库 `messages` + 分片 `events`；`last_seq` / `complete` 语义不变。
- 读连接：主库 4；分片每片 1–2、合计 ≤16；**总连接数 ≤20**（4 + 16）；读连接池按片管理。
- **禁止任何代码回退读主库 `events`**；启动断言 + 单测/集成测试覆盖。任何 `events` 查询都必须经路由层。

#### 6. 保留/归档/备份/诊断路由

- 保留清理逐片执行；归档文件按片 + 全局 manifest；`session_export`/`archive_export` 按片读取并汇总；`audit_log` 不清理。**M10-09 只做分片路由/口径打通；批选/吞吐/收缩优化归 M10-11。**
- 会话删除/级联：按 D-039-8 tombstone 协议执行（`deletion_pending` → 分片 events → 主库行），可重试、启动恢复、不得留下孤儿事件或可见空会话；M10-09 覆盖测试。
- 备份/恢复按 D-039-5 维护态与 D-039-8 整套切换；诊断包包含当前 generation 的主库 + 全部分片 + `events/shards.json` + manifest + 每片 `quick_check` 摘要。
- 备份/恢复、导入等维护操作与分片清理互斥；不得在维护态并行清理。

#### 7. 契约

- 命令 87（0 新增）/ 事件类型 0 新增（EventEnvelope 9 字段不变）/ 设置键 11（0 新增）/ 迁移 0014 +1；`health` 增加可选字段（D-039-5）；M9-01 基线重录。
- 不新增 `events_*` 命令；不新增设置键；不新增事件类型；不新增业务码。

### D-039-2 启动并行完整性检查（R6/R8）

#### 1. 启动序列（不得阻塞 `quick_check`）

DB 打开（PRAGMA/迁移，**不含** `quick_check`）→ `initialize` → `health` 可用 → 后台并行 `quick_check`。**不得在 `initialize`/`health` 前阻塞 `quick_check`**；`uiReadyAt` 不能等 `quick_check` 完成。启动检查范围为**主库 + N 个分片**（主库仍含 `messages`/`runs`/`audit_log`）；`storageCheckCompletedAt` 覆盖主库 + 全部分片。

#### 2. 干净关闭判据（严格 sidecar + 混合策略）

- 关闭序列完成、写队列 drain、全部库 `wal_checkpoint(TRUNCATE)` 成功、全部读连接关闭后，原子写 `<generation>/clean_shutdown.json`（临时文件 + fsync + rename；目录 fsync 按平台能力）。
- sidecar **至少**包含：`generation`、`app_version`、`closed_at`、`close_sequence_completed`、`write_queue_drained`、`background_check_completed`、`last_full_check_at`、`clean_start_count`、`main`、`shards[]`；每个库记录 `schema_version`、`header_change_counter`（SQLite 文件头偏移 24 的 4 字节 change counter，**不是**连接级 `PRAGMA data_version`）、`file_size`、`wal_size`、`last_seq`；分片另记 `shard_id`。字段样例见附录 B。
- `clean_start_count` 语义（P2-3）：**仅**“干净启动且 sidecar 全匹配”时 `+1`；异常启动/sidecar 不匹配/检查失败/`persist_degraded`/恢复/导入/迁移/版本升级/resharding 后一律重置为 `0`；每 20 次或 7 天触发的强制完整检查完成后也重置为 `0`。
- 下次启动逐库逐片比对；**全部匹配**（含 `generation` 与 `CURRENT` 一致、`wal_size=0`、`schema_version` 一致、文件大小/头 change counter 一致、分片集合一致、`close_sequence_completed=true`、`write_queue_drained=true`）才跳过阻塞 `quick_check`。任一不匹配（含 WAL 非 0/缺失但 size 变化、manifest/分片缺失、文件头变化、版本升级）→ **全量并行检查**。
- sidecar 缺失/损坏/写入失败 → 视为不干净，执行全量并行检查；不得猜测“大概干净”。
- 关闭时若 `storage_state=persist_degraded` 或完整性检查失败，**不得写 clean sidecar**（写 `invalid` 或删除）；下次启动必须全量并行检查，不得用“关闭序列完成”掩盖降级。
- 干净关闭路径：跳过阻塞检查，启动后空闲时后台 `quick_check`；**每周 `integrity_check` 保留**；采用“**每 20 次干净启动或 7 天（先到者）**强制一次完整检查”，记录 `last_full_check_at`；常量为可调参数，调整须记录证据。
- 异常退出路径：并行 `quick_check`，检查期间启用写门（D-039-3）；失败进 `persist_degraded`。
- 恢复/导入/迁移/版本升级/resharding 后 sidecar 必须失效（删除或写 `invalid`）；下一次启动不得跳过。
- **残余风险必须写入证据并由安全/QA 签核接受**：sidecar 全匹配只能证明“干净关闭且文件未被改写”，不能证明页级内容未损坏；页级损坏只能靠后台/周期检查发现。若故障注入发现“sidecar 匹配但损坏漏检”，立即回退为每次启动阻塞 `quick_check`。

#### 3. 并行检查

- 启动时并行检查**主库 + N 个分片**；worker **默认 4、最大 8**；按磁盘类型：NVMe 8、SATA SSD 4、HDD 2；按平台/磁盘可降级；限制并发 I/O；主库较小，优先调度。
- **30s 硬阈值**；19.36s 仅为参考值，必须按 §6.4 复测；目标 **≤25s**，给中位/长尾留 5s 余量。
- 记录单分片/主库/总耗时/失败数；不得只报平均；不得静默跳过失败样本。
- 真实复测：5.22 GiB / 20,000,000 行、release `aetherd`、真实 Electron、fresh `userData`/registry、3 次中位/P95/max；平台矩阵见 §6.4。
- `scripts/test/m10-08/parallel-quick-check.mjs` 只作脚本参考；M10-10 必须在**真实 `aetherd` 启动路径**上复测并行检查（含 HDD 冒烟），不得用独立脚本结果代替 daemon 证据。

#### 4. 干净关闭/异常启动/周期检查的关系

| 场景 | 阻塞 `quick_check` | 启动后检查 | 写门 | 失败处理 |
|---|---|---|---|---|
| 干净关闭且 sidecar 全匹配 | 跳过 | 后台 `quick_check`（主库 + 分片；`mode=background`） | 不设写门（`write_gate=open`；写入可继续） | 失败 → `persist_degraded` |
| 异常退出/sidecar 不匹配/版本升级 | 不阻塞启动，但启动后**并行检查** | 并行 `quick_check`（主库 + 分片；`mode=startup`） | **启用写门**（`write_gate=checking`）：读可用，写/新 run 拒绝 | 失败 → `persist_degraded` + S-08 |
| 每 20 次干净启动或 7 天 | 不阻塞启动 | 后台完整 `integrity_check`（主库 + 分片；限速） | 不设写门（`write_gate=open`） | 失败 → `persist_degraded` |

> `mode=startup` 与 `mode=background` 的区别只影响写门/UI；两者共用 `storage_check` 字段，具体契约见 D-039-5。

### D-039-3 启动策略、1.5s 预算与可见状态（R2 / 复审 P0-1、P0-2、P1-1、P2-5）

#### 1. 指标与目标

| 指标 | 定义 | 目标 |
|---|---|---|
| `windowShownAt` | 壳层窗口可见（自 `startedAt`） | **≤1.5s（硬）** |
| `uiReadyAt` | 壳层可见 + **首个 `health` 成功**（取较晚者；自 `startedAt`）；**不要求写可用，S-00 可仍在显示** | **≤1.5s（硬）** |
| `initializeMs` | DB 打开（PRAGMA/迁移）+ `initialize` 完成 | **≤1.5s（5.22GiB 路径）/ 硬超时 10s** |
| `mainInteractiveAt` | S-02 主界面可交互（S-00 已退出） | 干净启动 ≤1.5s；异常启动 = 检查完成（≤25s/≤30s） |
| `writesEnabledAt` | 写/新 run 可被接受 | 干净启动 ≤1.5s；异常启动 = 检查完成（≤25s/≤30s） |
| `storageCheckCompletedAt` | 主库 + 全部分片完整性检查完成 | **≤25s 目标 / ≤30s 硬** |
| `quickCheckPerShardMs` | 单分片 `quick_check` | 记录 P50/P95/max |
| `mainQuickCheckMs` | 主库 `quick_check` | 记录 P50/P95/max |
| `checkConcurrency` | 并行 worker 数 | 默认 4；NVMe 8 / SATA SSD 4 / HDD 2；最大 8 |

- `STARTUP_REVEAL_GRACE_MS = 1300ms`（给 1.5s 留 200ms 余量）；当前实现为 1500ms，M10-10 必须改为 1300ms 并复测。
- **`initialize` 预算对齐（P1-1）**：5.22GiB 启动路径 `initialize` 目标 **≤1.5s**，硬超时仍为 **10s**；**删除旧的 2s 目标**。P2 计划 v0.14 性能矩阵同步为 `initialize ≤1.5s / 硬超时 10s`；不得同时保留 2s 与 1.5s 两个硬目标。若 release 复测无法满足，按 AGENTS §7 报告，由产品/架构另立 ADR 调整预算。
- **`uiReadyAt` 定义（P0-1）**：只要求“壳层可见 + 首个 `health` 成功”，**不要求写可用**；异常启动时 S-00 可仍在显示。`mainInteractiveAt`/`writesEnabledAt` 单独度量：干净启动两者 ≤1.5s（后台检查不设写门）；异常启动两者在检查完成、写门打开后才成立（≤25s/≤30s）。失败路径：S-08 只读主界面可交互，`mainInteractiveAt` 以 S-08 呈现为准；`writesEnabledAt` 不成立，修复 + 重启后重新度量。
- **旧的“UI ready ≤8s”不适用于 5 GiB 启动场景**；P2 计划 v0.14 性能矩阵按本表更新。若产品对其他场景另有 8s 预算，须单独登记，不得覆盖本 ADR 的指标。

#### 2. 启动/检查/维护状态机

| 阶段 | `storage_state` | `storage_check.state` | `write_gate` | 写/新 run | UI |
|---|---|---|---|---|---|
| 启动检查中（`mode=startup`） | `normal` | `checking` | `checking` | **拒绝**（`storage_backpressure` + `detail.reason=storage_check`） | S-00“正在校验数据（x/8，主库校验中/已完成）”；`uiReadyAt` 可成立，`mainInteractiveAt`/`writesEnabledAt` 不成立 |
| 后台检查中（`mode=background`） | `normal` | `checking` | `open` | 允许 | 可选轻提示/诊断页；不阻塞主界面；`mainInteractiveAt`/`writesEnabledAt` 已成立 |
| 检查通过 | `normal` | `ok` | `open` | 允许 | 横幅消失 |
| 检查失败 | `persist_degraded` | `failed` | `open` | 拒绝（`persist_degraded`） | S-08 只读安全模式 + 导出/备份；主库失败显示“主库校验失败” |
| 维护态（`normal` + 备份/恢复） | `normal` | 保持上次结果 | `open` | **除 `health`/`app_exit` 外全部拒绝**（`storage_backpressure` + `maintenance_reason`） | S-06 维护页/横幅；“维护中，其他功能暂不可用” |
| 维护态（`persist_degraded` + 备份/恢复） | `persist_degraded` | 保持上次结果 | `open` | **仅维护例外**：除 `health`/`app_exit` 外全部拒绝；恢复成功后重启 | S-06 维护页 + S-08 降级标识；不得把维护本身误报为降级 |

- **检查中 ≠ 降级**：`storage_check.state=checking` 时 `storage_state` 必须保持 `normal`；不得提前写 `persist_degraded`，不得用 S-08 呈现正常校验。
- **D4 单一口径（P0-2）**：D4 状态机仍只有 `normal` / `persist_degraded`；`storage_state` 仍是**降级**唯一事实源；`storage_check.write_gate`（`checking`/`open`）是**启动准入阶段**，不新增第三种降级状态。`write_gate=checking` 只表示“检查未完成、暂不接受写/新 run”，不写入 `persist_degraded`。
- **错误映射定稿（P0-2）**：

| 场景 | 错误 | UI 判定 |
|---|---|---|
| `write_gate=checking` 时写/新 run | `business_code=storage_backpressure`，`detail.reason=storage_check` | 以 `health.storage_check.write_gate=checking` 优先 → S-00；**不得**仅凭 `storage_backpressure` 显示 S-08 |
| `persist_degraded` 时写/新 run | `business_code=persist_degraded` | S-08 |
| 维护态（除 `health`/`app_exit`） | `business_code=storage_backpressure` + `maintenance_reason=backup` / `restore` | S-06 |
| L2 队列背压（非检查/非维护） | `business_code=storage_backpressure` | S-08（既有语义） |

- **UI 优先级**：`maintenance` → S-06（若同时 `persist_degraded`，附 S-08 标识）；`storage_check.write_gate=checking` → S-00；`storage_state=persist_degraded` 或 `storage_check.state=failed` → S-08；L2 `storage_backpressure` → S-08。`storage_check` 与 `maintenance` 的 `health` 字段优先于错误码；不得出现“校验中显示 S-08”或“维护本身显示为降级”。
- 检查失败才进入 `persist_degraded`，`degrade_trigger=integrity_failure`；恢复仍按 D4“修复外部条件 + 重启核心 + 启动自检通过”，不热恢复。
- 检查完成：`storage_check.state=ok`，`write_gate=open`，横幅消失，写恢复。
- UI 映射：校验中复用 **S-00**（文案“正在校验数据（x/8，主库校验中/已完成）”；主库失败显示“主库校验失败”）；失败复用 **S-08**；维护态复用 **S-06 备份与恢复页 + 维护横幅**，不得把维护本身显示为 `persist_degraded`。若 UI-UX v0.20 评审认为必须新增锚点，按 UI-UX 流程登记，不得静默新增 testid。

#### 3. 启动预算验收

- 3 次运行取中位/P95/max；`windowShownAt`、`uiReadyAt`、`initializeMs` 均以中位 ≤1.5s 为硬条件；`mainInteractiveAt`/`writesEnabledAt` 按干净/异常路径分别验收（干净 ≤1.5s；异常 = 检查完成 ≤25s/≤30s）；max 必须报告，不得只报中位。
- 真实 Electron + release `aetherd`；fresh `userData`/registry；5.22 GiB 数据集；记录 `initializeMs`、`mainInteractiveAt`、`writesEnabledAt`、`storageCheckCompletedAt`、`quickCheckPerShardMs`、`mainQuickCheckMs`、`checkConcurrency`。
- debug 二进制、Node 子进程、无窗口脚本结果只能作为开发参考，**不得作为 G13/性能矩阵验收证据**。

### D-039-4 保留清理与文件收缩（R7）

#### 1. 两阶段清理

1. **选 rowid**：`SELECT rowid FROM events WHERE type IN (?,?,?) AND ts < ? ORDER BY type, ts LIMIT 5000`；使用 `idx_events_type_ts`；先 `EXPLAIN QUERY PLAN` 确认**无 TEMP B-TREE**。
2. **归档**：按 rowid 取全列 → `archive_retention_batch` 写 JSONL + fsync + manifest + 校验；失败跳过该批（ADR-021 不变）。
3. **删除**：`DELETE ... WHERE rowid IN (...)`，单事务；按 **500 / 1000 / 2000 / 5000** 四档实测，选“P95 <500ms”的最大档；当前预期 500–1000 行。
4. 不得以放宽 D3 `<500ms` 代替优化；`RETENTION_BATCH_MAX=5000` 可保留为**选批上限**，删除事务可更小。
5. **最终清理集合不变；单批成员/归档顺序可变**（P2-3）；不得以“批顺序变化”为由改变清理白名单或保留窗口。
6. M10-11 基准必须在**产品 `select_retention_batch`/`archive_retention_batch` 路径**上执行，不得用 M10-08 采样脚本代替；`EXPLAIN QUERY PLAN` 与实际产品路径耗时同时归档。

#### 2. 文件收缩

- 新装主库与全部分片在建表前设置 `PRAGMA auto_vacuum=INCREMENTAL`；保留后执行有界 `PRAGMA incremental_vacuum(N)`。
- 物理文件仍 >5 GiB 时，提供**维护态手动 `VACUUM`**；预检可用空间 ≥ 当前 `db+wal` ×2；记录耗时/停写窗口；维护态按 D-039-5 独占。
- 容量口径：**逻辑 live bytes 与物理 db+wal 分开报告**；不得用 1.387 GiB live 代表物理达标；物理 >5 GiB 必须告警并给出 VACUUM/重建/分片评估。
- generation 总量（当前 + 旧 `.pre-*` + `.failed-*`）纳入容量巡检；默认保留 1 份旧 generation；超过 `1.5×` 当前 live 告警；详见 D-039-8 §1。
- `auto_vacuum` 在既有开发库上可能不可启用；开发库重建时启用；若新库实测不可用/无效，回退为维护态 `VACUUM` + 明确告警，不得宣称自动收缩已生效。

#### 3. 总时长

- D3 只硬约束**单事务 <500ms**；全量历史清理作为后台 KPI：**≤4h**；日常增量 **≤5min**；不阻塞启动/UI。
- 若产品要求更短，另立优化任务；不得降低单事务目标。

### D-039-5 `health` 契约与维护态备份/恢复（R5/R9）

#### 1. `health` 可选字段

`health` 命令计数不变（87），无参数、仅本地 IPC、不落库、不产生事件；在现有 `HealthReport` 上增加**可选**字段：

```json
{
  "storage_state": "normal",
  "write_queue_depth": 0,
  "runtimes": [],
  "ts": 1760000000000,
  "storage_check": {
    "state": "checking",
    "mode": "startup",
    "write_gate": "checking",
    "main_checked": false,
    "checked_shards": 3,
    "total_shards": 8,
    "started_at": 1760000000000
  },
  "maintenance": {
    "kind": "backup",
    "started_at": 1760000000000,
    "progress": { "phase": "vacuum_into", "completed_shards": 2, "total_shards": 8 }
  }
}
```

- 示例仅展示字段形状；`storage_check` 与 `maintenance` 可单独出现，不必同时存在。
- `storage_check` 可选对象：`state` = `checking` / `ok` / `failed`；`mode` = `startup` / `background`；`write_gate` = `checking` / `open`（启动准入，不是降级状态）；`main_checked` = 主库 `quick_check` 是否完成；`checked_shards` / `total_shards` 只计分片（x/8）；主库失败同样置 `state=failed`；可选 `started_at` / `completed_at` / `error`。UI 用 `checked_shards/total_shards` 显示“x/8”，主库未完成/失败时分别显示“主库校验中/主库校验失败”（P2-4）。
- `maintenance` 可选对象：`kind` = `backup` / `restore`；`started_at`；可选 `progress`（阶段/完成分片数/总分片数；`phase` 取值 `checkpoint` / `vacuum_into` / `manifest` / `copy` / `verify`）。**不得**用 `persist_degraded` 表示维护态。
- `storage_state` 仍是**降级**唯一事实源；`storage_check.write_gate` 是**启动准入**唯一事实源；二者不是同一维度的双事实源。`storage_check.state=failed` 必须伴随 `storage_state=persist_degraded` 与 `degrade_trigger=integrity_failure`；`checking` 不改变 `storage_state`。
- 字段为可选：旧客户端忽略未知字段；新客户端在字段缺失时按“无检查/无维护”处理。**“可选”仅指无检查/无维护时可省略；一旦进入检查/维护，`health` 必须返回对应对象，不得省略。** 启动检查（含干净关闭后的后台检查）必须在首个 `health` 可用前登记 `storage_check={state:"checking",mode:"startup"|"background",write_gate:"checking"|"open",main_checked:false,checked_shards:0,total_shards:N}`。
- 生成物（`types.ts`/`client.ts`/`command-manifest.json`/`error_codes.json`）由 `aether-app` 清单生成，禁止手改。
- M9-01 baseline 因 `health` 响应变化必须重录；`pnpm verify:m9-01-baseline`、`pnpm --filter @aether/protocol check`、`pnpm verify:error-codes` 必须通过。

#### 2. 维护态（备份/恢复独占）

- 备份/恢复期间进入维护态；**除 `health`、`app_exit` 外全部命令拒绝**（包括读/会话/设置/导入/导出）。复用 ADR-034 维护窗口机制，但使用**严格 allowlist（仅 `health`/`app_exit`）**；导入维护态的宽松 allowlist 不适用于备份/恢复。错误走现有 `storage_backpressure` + `maintenance_reason=backup|restore`（不得新增业务码；若实现发现语义不可接受，停下来报告）。
- 进入维护态前要求 `storage_check` 不在 `checking`；备份/恢复不得与启动检查/周期完整检查重叠。
- `persist_degraded` 下备份/恢复沿用 D3/D13 维护例外：允许启动；进入维护态后仍仅 `health`/`app_exit`；恢复完成后重启 daemon。
- `health.maintenance` 返回 `{kind, started_at, progress?}`；UI 显示 S-06 维护页/横幅，文案明确“备份维护中/恢复维护中，其他功能暂不可用”；**不得把维护态本身显示为 `persist_degraded`**。维护态本身不改变 `storage_state`；若备份/恢复过程中发生真实存储失败，按既有失败路径进入 `persist_degraded`，并在 `maintenance`/审计中保留失败原因。
- 自动备份：仅在无 active run/写入空闲时启动；不抢占在途 run；手动备份需确认进入维护态；备份/恢复/导入互斥。
- 导入（`session_import`/`archive_import`）适配：按 D-039-8 的 **generation staging + `CURRENT` 指针原子切换**执行（ADR-034 的 staging 模型扩展为多库 generation；需同步修订 ADR-034 v1.1）；导入 session 写入 `shard_id`、`deletion_pending=0`，事件按会话路由写对应分片；主库 `events` 保持为空；失败/回滚整套 generation；导入完成后旧 sidecar 随指针切换失效；导入与备份/恢复/清理互斥。
- `app_exit` 在维护态允许，但恢复/导入中断后下次启动必须读取 `restore-journal.json`/`import-journal.json` 并按 D-039-8 整套提交或回滚；不得留下半套 generation。

#### 3. 备份步骤（`backup_create`）

备份集布局：`<target>/<backup_id>/manifest.json` + `manifest.sha256` + `generation/`（`aether.db`、`events/events-0..7.db`、`events/shards.json`）。停写 + drain + checkpoint 后逐库快照；快照期间不得恢复写入，整套构成同一一致点。

1. 进入维护态；停止新写/新 run；drain 写队列。
2. 主库 + 全部分片 `wal_checkpoint(TRUNCATE)`。
3. 在目标目录同卷创建临时备份集；逐片 `VACUUM INTO`（并发 2–4）+ 主库 `VACUUM INTO` 到 `generation/`；预检目标空间 ≥ 当前 `db+wal` ×2。
4. 写 `manifest.json`：`backup_id`、`source_generation`（信息性）、时间、app/schema 版本、分片数、每片 `shard_id/path/size/sha256/max_seq`、主库 schema/大小、总行数、`encrypted`/`artifact_mode`、每文件相对路径/哈希；写 `manifest.sha256`；路径必须相对备份集根、无绝对路径/`..`/空段，允许 `generation/aether.db`、`generation/events/events-<n>.db`、`generation/events/shards.json`；复制前 canonicalize + 前缀校验，不写绝对路径/用户名。
5. `fsync` 文件/目录后，原子 rename 临时集为 `<backup_id>`；退出维护态；记录审计。

异常：备份中 `app_exit` → 中止备份，删除或标记未完成备份集（不得出现在 `backup_list`），写审计；下次启动清理未完成备份集/staging；不得留下可被误恢复的半套备份。

#### 4. `backup_*` 契约冻结（P0-4）

`backup_create` 请求不变：`{label?, target_dir?, passphrase?}`；响应不变：`{backup: <record>, pruned: [<record>]}`。`backup_list` 响应不变：`{backups: [<record>], capacity, auto?}`；`capacity` 统计当前 generation 主库 + 全部分片 `db+wal`，`auto` 段不变。**`<record>` 在既有字段 `id/path/size_bytes/encrypted/kind/created_at` 上增加可选字段**：

- `format`: `"sharded_set"`（新备份）或 `"single_file"`（旧记录）；
- `set_path`: 备份集根目录（新备份；`path` 同步指向该目录）；
- `manifest_path` / `manifest_sha256`: 备份 manifest 路径与哈希；
- `shard_count`: 分片数（新备份 = 8）；
- `integrity`: `{status: "ok"|"failed"|"unverified", checked_at?}`。

`backup_restore` 请求不变：`{source: {internal: {id}} | {external: {path}}, passphrase?}`；语义冻结：

- `internal.id`：内部整套备份 id；`external.path`：包含 `manifest.json` 的目录，或 `manifest.json` 文件路径；
- **外部路径校验落地（P1-1）**：`crates/aether-app/src/ipc/dto.rs` 的 `BackupSource::External` 文档、`CommandRequest::validate`、`crates/aether-app/src/ipc/path.rs` 与后端路径校验统一改为 `validate_external_backup_set`：`canonicalize` 后必须是“含 `manifest.json` 的目录”或文件名恰为 `manifest.json` 的文件；其他目录/文件拒绝。不得继续复用只接受 `.db`/`.aebk` 文件的 `validate_external_backup_candidate`。备份集根 `canonicalize` 后作为前缀根；`manifest.path` 解析后必须落在根内；symlink/junction 解析到根外拒绝（根内才允许）。
- 开发阶段**不接受**旧单文件 `.db`/`.aebk` 路径：即使路径存在且后缀合法，也在路径校验/后端显式拒绝（`invalid_value` + 消息含 `backup_candidate_invalid`），提示使用整套备份或重建；不得只靠后缀判断；
- 响应保留 `restoring`/`restart_required`/`source`/`db_path`，`candidate` 保留 `path/size_bytes/schema_version/encrypted/artifact_mode/source_backup_id` 并增加 `format:"sharded_set"`、`set_path`、`manifest_path`、`manifest_sha256`、`shard_count`；`candidate.path`/`set_path` 均指向备份集根，`manifest_path` 指向 `manifest.json`，`db_path` 指向当前 generation 主库；
- 不新增命令/业务码；M9-01 因 `health` 与 `backup_*` 响应字段变化重录；生成物由清单生成；UI S-06 按套展示/恢复；旧 `single_file` 记录仅展示、**恢复按钮禁用**并提示“请使用整套备份或重建”；按 `created_at` 降序（既有行为）。
- 本契约同步写入《设计文档》D7/D13/附录 C（如涉及迁移登记）与 §6.1；UI-UX v0.20 的 S-06 适配。
- 外部路径测试：备份集目录（含 `manifest.json`）、`manifest.json` 文件、旧 `.db`、旧 `.aebk`、不存在路径、非备份目录；前两者接受，后四者拒绝；写入 M10-09 DoD/M9-01 重录。

#### 5. 恢复步骤（`backup_restore`）

1. 进入维护态；停写。
2. 校验 `manifest.json` + `manifest.sha256` + `shard_count=8` + app/schema 版本（高于当前程序拒绝）+ 所有分片/主库 `quick_check` + 迁移版本；任一失败拒绝恢复，当前库不变。
3. **空间预检（P2-1）**：复制前检查 `<data_dir>/staging` 可用空间 ≥ 备份 generation（解密后）总大小 ×1.1；不足则拒绝恢复并保留当前库，不得进入复制阶段。通过后在 `<data_dir>/staging/restore-<id>/generation/`（与数据目录同卷）完整复制备份集 generation，逐文件校验/`fsync`；不得跨卷直接 rename。
4. 写 `restore-journal.json`（old/new generation、manifest 哈希、phase）；由启动序列在无写者窗口按 D-039-8 原子切换 `CURRENT`、重新打开/`quick_check`/迁移；成功提交并清理 journal；旧 generation 保留为 `.pre-restore-<ts>`。
5. 任一步失败：`CURRENT` 回指旧 generation，旧 generation 保持完整；staging/新 generation 标记 `.failed-<ts>`；重启后可用旧 generation。
6. 删除/失效新 generation 的 `clean_shutdown.json`；重启 daemon；写审计；退出维护态。
7. 恢复输入为整套（目录 + manifest）；旧单文件 `.db` 不再作为恢复输入；参数/响应按 §4 冻结，不新增命令。
8. **附件提示（P2-1）**：恢复成功页、审计与诊断包必须明确“附件内容未随库恢复（ADR-035），请单独保留/恢复 attachments”；不得把恢复显示为完整成功；附件备份/恢复另立 ADR。

#### 6. 归档/诊断/加密

- 归档逐片文件 + 全局 manifest；`archive_export` 汇总（排除 `deletion_pending=1` 会话）；`audit_log` 不清理。
- 诊断包包含当前 generation 的主库 + 全部分片 + `events/shards.json` + manifest + 每片 `quick_check` 摘要；不得只含主库。
- 加密备份/恢复（ADR-020）在**整套**备份上执行：同一密钥/模式覆盖全部文件，`manifest.json` 记录 `encrypted`/`artifact_mode` 与全部密文哈希；不得逐片单独加密导致 manifest 不一致；恢复先解密到 staging generation 再按 §5 切换。

### D-039-6 无迁移初始化、代际指针与启动错误映射（R4 / 复审 P1-4）

- **开发阶段没有线上版本，不做数据迁移**：不写一次性/惰性迁移脚本，不做双读，不保留运行时单库回退窗口。只支持新装初始化与开发库重建。
- **新装初始化**：创建 `<data_dir>/generations/gen-<ulid>/`，主库 + 8 分片 → 执行 0001–0014（分片库完整物理执行）→ 打开/校验 → 原子写 `events/shards.json`（`shard_count=8`、`shards[]`、`schema_version`、`app_version`、`created_at`）→ 原子写 `CURRENT` 指针（`generation` + `shards_sha256`）。初始化前清理 `<data_dir>/staging/` 中无 journal 的半成品；不得删除 `backups/`/`attachments/`/`logs/`。全部成功才生效；失败删除半成品 generation/staging，清理失败则拒绝启动并报告路径。
- **开发库重建**：退出应用 → 删除/移动数据目录 → 重启触发新装初始化。不得尝试把旧单库“就地升级”为分片库。
- **旧布局检测（fail-closed，P0-1）**：DB 布局工件 = `<data_dir>/aether.db`、`<data_dir>/CURRENT`、`<data_dir>/generations/` 任一存在；`logs/`、`backups/`、`attachments/`、`staging/`、`run/` **不参与**首启判定。三类均不存在 → 新装初始化；存在任一 DB 布局工件但 `CURRENT` 缺失/损坏、`<data_dir>/aether.db` 旧扁平布局存在但无 `CURRENT`、`generations/<id>` 缺失/不完整、主库 `events` 非空、`sessions.shard_id` 为空/越界、`shards.json` 缺失/无效 → **拒绝启动**；不得猜测 generation、不得静默回退单库。提示“开发阶段不支持旧库迁移，请重建数据目录”。
- **半成品 generation 提示（P2-4）**：若 `<data_dir>/generations/` 存在、`CURRENT` 缺失且无 journal（典型：初始化/切换前崩溃），仍按 fail-closed 拒绝启动；S-12 文案/诊断包必须标记 `incomplete_generation` 并提示“删除半成品 generation 或重建数据目录”。**不自动删除、不自动切换、不猜测 generation**；可在诊断包中列出候选目录/大小，由用户按说明处理。
- **0014**（仍为 1 个迁移）：`sessions.shard_id INTEGER`（SQLite `ADD COLUMN` 允许 NULL，但运行时不变式要求新会话创建时写入非空；NULL 仅可能来自旧开发库/未完成写入，启动拒绝）+ `sessions.deletion_pending INTEGER NOT NULL DEFAULT 0 CHECK (deletion_pending IN (0,1))`（D-039-8 tombstone）+ `idx_sessions_shard_id` + `idx_sessions_deletion_pending`；0001–0013 不改；分片库完整执行 0014（列存在但不参与分片业务）；《设计文档》附录 C 记录终态 DDL，本 ADR 附录 E 给出参考 DDL。
- **启动不变量错误映射（P1-4，定稿）**：daemon bootstrap 首帧 error 统一 `code=internal` + 下表 `business_code`，退出码 22；main 按 `business_code` 映射 S-12 `data-reason`；**不得只返回 `internal` 而声称 S-12**。

| 条件 | `IpcErrorCode` | `business_code` | exit | S-12 `data-reason` / UI |
|---|---|---|---|---|
| DB 布局工件存在但 `CURRENT` 缺失/损坏（含旧扁平布局） | `internal` | `db_open_failed` | 22 | `db_open_failed` / “数据目录布局无效，请重建或恢复整套备份” |
| 主库 `events` 非空 | `internal` | `db_open_failed` | 22 | `db_open_failed` / “主库 events 非空，数据布局无效” |
| 分片缺失/无法打开、`shards.json` 缺失/无效 | `internal` | `db_open_failed` | 22 | `db_open_failed` / “分片缺失或无法打开” |
| `sessions.shard_id` 为空/越界 | `internal` | `db_open_failed` | 22 | `db_open_failed` / “会话分片映射无效” |
| `sessions.deletion_pending` 非 0/1 | `internal` | `db_open_failed` | 22 | `db_open_failed` / “删除 tombstone 无效” |
| `CURRENT.format_version` 不支持 | `internal` | `db_open_failed` | 22 | `db_open_failed` / “数据目录格式版本不支持” |
| `CURRENT.shards_sha256` 与 `shards.json` 不匹配 | `internal` | `db_open_failed` | 22 | `db_open_failed` / “分片清单哈希不匹配” |
| `generation` 正则/`generations/` 前缀/`shards[].path` 越界 | `internal` | `db_open_failed` | 22 | `db_open_failed` / “路径无效” |
| 迁移 checksum/schema 版本不符 | `internal` | `schema_checksum_mismatch` | 22 | `schema_checksum_mismatch` / “schema 校验失败” |
| 分片 `quick_check` 失败（可打开） | 不退出 | 无 | 不退出 | `persist_degraded` / S-08 |

- **未来发布**：若出现线上用户，必须另立 ADR 定义迁移/双读/回退；本 ADR 不授权任何线上迁移。resharding 同样另立 ADR；当前 N=8、单片上限 1.5 GiB。

### D-039-7 设计升版清单（复审通过后执行）

1. **D2**：启动序列/错误映射（DB 布局工件首启判定、`CURRENT` 校验、首帧 error、exit 22、S-12 `data-reason`、半成品 generation 的 `incomplete_generation` 提示、initialize ≤1.5s/硬超时 10s）；`logs/` 等非 DB 目录不影响首启。
2. **D3**：代际布局 + `CURRENT`；`shards.json` 不含 `generation`（`CURRENT` 唯一绑定）；单写任务 + 每片连接；`0014 sessions.shard_id` + `deletion_pending`；主库 `events` 空表断言；N=8/单片 1.5 GiB；启动检查（主库 + 分片/干净关闭/异常）；`auto_vacuum=INCREMENTAL`；维护态；D3 失效条件更新。
3. **D4**：按会话路由；会话不跨片；每片 `UNIQUE(session_id,seq)`；会话 sequencer；广播顺序不变；先日志后广播；`events_page`/`events_latest`/`max_seq` 路由；主库 `events` 空表；**`storage_check.write_gate` 为启动准入、`storage_state` 为降级唯一事实源**。
4. **D7**：`health.storage_check`（state/mode/write_gate/main_checked/checked_shards/total_shards）与 `health.maintenance` 可选字段；`backup_create`/`backup_list`/`backup_restore` 契约冻结；`BackupSource::External` 路径校验改为目录/`manifest.json`，旧 `.db`/`.aebk` 显式拒绝；命令计数 87 不变；M9-01 重录。
5. **D13**：generation 目录 + `CURRENT` 指针；维护态备份/恢复；逐片 `VACUUM INTO`；manifest/哈希；整套恢复/导入原子切换；恢复 staging 空间预检（≥生成集 ×1.1）；归档/诊断包/加密备份联动（`archive_export` 排除 `deletion_pending=1`）；旧 generation（`.pre-*`/`.failed-*`）保留 1 份/清理/容量告警；备份期间其他功能不可用；会话删除 tombstone。
6. **《设计文档》附录 C**：`0014_session_shard.sql`（`sessions.shard_id` + `sessions.deletion_pending` + 索引）；0001–0013 不改；分片库完整执行 0014（不跳过）。
7. **ADR-034 v1.1**：导入 staging 从单 DB 扩展为多库 generation staging + `CURRENT` 原子切换；维护窗口/作业/进度命令不变。
8. **ADR-020 v1.1**：备份产物从单文件扩展为整套备份集/manifest；加密覆盖整套（同一密钥/模式）；恢复先解密到 staging generation。
9. **§5 P2/G13**：M10-08 报告归档；M10-09/10/11；设计 v1.35；M9 G8 保持；P0/P1 复跑；独立签核。
10. **UI-UX v0.20**：增加“数据校验中”“备份维护中”“主库校验中/失败”状态/文案；旧 `single_file` 备份恢复禁用并提示“请使用整套备份或重建”；恢复成功页提示“附件未随库恢复”；S-12 半成品 generation 提示“删除半成品 generation 或重建数据目录”；S-00/S-08/S-06 优先级以 `health.storage_check`/`maintenance` 为准；若新增锚点须登记；备份期间其他功能不可用的提示。
11. **P2 计划 v0.14**：M10-08 保持“已执行、需分库、只验证不实施”；新增 M10-09/10/11；G13 条件、性能矩阵（`initialize` ≤1.5s/硬超时 10s；`uiReadyAt` 1.5s；`storageCheckCompletedAt` 25/30s）、迁移计数更新。
12. **AGENTS.md / README.md**：设计 v1.35 / P2 v0.14 / ADR-039 v1.0（已签署）/ ADR-034 v1.1 / ADR-020 v1.1；命令 87 / 业务码 35 / 设置键 11 / 迁移 0014；当前阶段 M10-09/10/11；证据链接。
13. **M10-08 证据**：仅补充字段说明（5.35× 口径、`startupRequiresAdr`/`withinTarget` 语义）；不改变判定；不重跑除非复跑命令需要。

### D-039-8 跨库维护协议（复审 P0-3/P1-3；三审 P1-2/P1-3/P2-1/P2-4/P2-5）

#### 1. 代际目录、`CURRENT` 指针与启动恢复

- 布局：`<data_dir>/CURRENT` + `<data_dir>/generations/<generation_id>/`（`aether.db`、`events/events-0..7.db`、`events/shards.json`、`clean_shutdown.json`）；`<data_dir>/staging/` 用于恢复/导入暂存；`backups/`、`attachments/`、`logs/`、`run/` 不在 generation 内。
- `CURRENT` 原子写（P2-4）：先写 `CURRENT.tmp` 并 `fsync`/`FlushFileBuffers`；同卷替换：POSIX 用 `rename(2)` + 目录 `fsync`；Windows 已有 `CURRENT` 用 `ReplaceFileW`（或 `MoveFileExW(MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH)`）替换，首次创建用不带 replace 的 `MoveFileExW`；目录 `fsync` 不支持时记录限制。**禁止“删除旧文件再 rename”伪装原子**；`CURRENT` 至少含 `format_version/generation/shards_sha256/switched_at`；禁止跨卷 rename。M10-09 证据必须固定 Windows API 与失败注入。
- **路径约束（P1-3）**：`generation` 正则 `^gen-[0-9A-Z]{26}$`；canonicalize 后必须前缀匹配 `<data_dir>/generations/`；`shards[].path` 只允许相对 generation 根的 `events/events-<0..7>.db`，禁止绝对路径/`..`/空段；manifest `path` 只允许相对备份集根的 `generation/...`，禁止绝对路径/`..`；恢复复制前 canonicalize + 前缀校验；symlink/junction/reparse point 解析到根外拒绝；失败映射 `db_open_failed`（内部）/`backup_candidate_invalid`（外部备份集）。
- `restore-journal.json` / `import-journal.json` 位于数据根，记录 `kind`、`old_generation`、`new_generation`、`shards_sha256`（可选 `backup_manifest_sha256`）、`phase`（`staged`/`switched`/`committed`）、`requested_at`；journal 原子写。
- 启动恢复顺序：读取 `CURRENT` → 读取 journal（若有）→ 校验/提交或回滚 generation → 校验 `shards.json`/主库不变量 → sidecar 检查 → 处理 pending 删除 → `health` 可用 → 检查调度。
- journal 恢复：
  - `CURRENT` 指向 new 且 new 校验通过 → 提交：清理 journal；old generation 保留为 `.pre-<kind>-<ts>`。
  - `CURRENT` 指向 new 但 new 校验失败 → 将 `CURRENT` 回指 old，校验 old；new 标记 `.failed-<ts>`；清理 journal。
  - `CURRENT` 指向 old → 清理 staging/new；清理 journal；当前库不变。
  - journal 损坏/新旧 generation 均不可用 → 拒绝启动（`db_open_failed`），不得猜测。
- 切换后的 generation **不得携带** staging 的 `clean_shutdown.json`；旧 sidecar 随 `CURRENT` 切换失效；下一个干净关闭才写新 sidecar。
- **旧 generation 保留/清理（P1-2 / P2-2）**：成功后保留 **当前 generation + 最近 1 份旧 generation 回滚点**（`.pre-*`）；`.failed-*` 与 `.pre-*` 纳入同一安全清理池：`.failed-*` 至少保留到本次诊断包/证据生成后，之后保留最近 1 份或标记可删；更早的 `.pre-*`/`.failed-*` 在启动成功/备份成功/恢复提交后的安全窗口自动清理（内部常量，不新增设置键）。清理前再次确认目标不是 `CURRENT`、不被 journal 引用；删除失败仅告警并保留，不得影响当前 generation。恢复/导入进行中不清理。
- **容量巡检/诊断**：容量统计包含当前 generation + `.pre-*` + `.failed-*` 总量；超过 `1.5×` 当前 live（主库+分片 `db+wal`）触发告警；诊断包列出各 generation 的路径/大小/状态（`current`/`pre`/`failed`/`incomplete`）/是否被 journal 引用；清理前后记录磁盘占用。清理中断不得破坏当前 generation。

#### 2. 会话删除 tombstone 协议（P0-3）

- 0014 增加 `sessions.deletion_pending INTEGER NOT NULL DEFAULT 0 CHECK (deletion_pending IN (0,1))` + `idx_sessions_deletion_pending`；不新增表/迁移。
- 删除前的 active run/权限门禁沿用既有 `session_delete` 语义；不因 tombstone 绕过。
- 步骤：
  1. 主库事务：`UPDATE sessions SET deletion_pending=1 WHERE id=?`；无行 → `session_not_found`；已为 1 → 继续（幂等重试）。
  2. pending 会话从 `session_list`/`session_search`/`messages_page`/`session_export` 隐藏/拒绝；`archive_export` 排除 `deletion_pending=1` 会话（或标记 `pending_deletion`，默认排除），不得导出已请求删除数据；`session_send` 等按 `session_not_found` 拒绝；`session_delete` 可重复调用以重试。
  3. 按 `sessions.shard_id` 删除该片 `events`（单分片事务，幂等）。
  4. 主库事务：`DELETE FROM sessions WHERE id=? AND deletion_pending=1`（`messages`/`runs` 级联）；成功后 tombstone 随行删除。
- 任一步失败：保留 `deletion_pending=1`，返回错误；重试/启动恢复继续；不得先删主库（会孤儿事件），也不得在分片删除失败后暴露空会话。
- 启动恢复：扫描 `deletion_pending=1`，要求分片可打开；缺席 → 拒绝启动（`db_open_failed`）；`quick_check` 失败 → `persist_degraded`；pending 保留，修复后重试；恢复期间不对外提供该会话。
- 审计：记录 requested/completed/failed；不含事件 payload/密钥。
- 导入 session 必须写 `shard_id` 且 `deletion_pending=0`；`preserve` 冲突拒绝，`remap` 重新计算 shard_id。

#### 3. 导入 staging 与整套切换（P0-3）

- ADR-034 的单 staging DB 扩展为**多库 generation staging**；维护窗口/作业/进度命令不变；需同步修订 ADR-034 v1.1。
- 步骤：
  1. 进入导入维护窗口；停止新写/新 run；drain；保持 ADR-034 既有只读命令可见性（与备份/恢复严格 allowlist 不同）。
  2. 在 `<data_dir>/staging/import-<job_id>/generation/` 复制当前 generation（`VACUUM INTO` 逐库 + `shards.json`）；不得跨卷 staging。
  3. 在 staging 上执行导入：session 写 `shard_id`/`deletion_pending=0`，事件按会话路由到 staging 分片；主库 `events` 恒空；`preserve`/`remap` 按 ADR-034。
  4. 校验 staging：schema 14、8 分片、主库/分片 `quick_check`、行数/哈希、main events=0；写 staging `manifest.json`（计数/哈希）；失败丢弃 staging，当前库不变。
  5. 写 `import-journal.json`；`CURRENT` 原子切换；重新打开/校验；成功提交、清理 journal、旧 generation `.pre-import-<ts>`、重新打开/必要时重启 daemon；失败按 §1 回滚整套。
  6. `dry_run=true`：只构建+校验 staging，不切换、不写 journal，随后丢弃；返回既有 dry-run 报告。
- 导入/备份/恢复/清理互斥；导入完成后旧 `clean_shutdown.json` 随指针失效。
- 不新增命令/事件/设置键；`session_import`/`archive_import` 请求/响应保持既有契约，仅内部 staging 多库化。

#### 4. 恢复整套切换（P1-3）

- 复用 §1 的 generation/journal/指针流程：`backup_restore` 校验+staging 后返回 `restart_required`；启动序列在无写者窗口切换；失败整套回滚到 old generation。
- 任何失败不得留下“主库新/分片旧”或“分片新/主库旧”；`CURRENT` 是唯一提交点。
- 恢复中断/`app_exit`：journal 保留；下次启动按 §1 提交或回滚；`.pre-restore` 整套保留。

#### 5. 故障注入（必须覆盖）

- 会话删除：tombstone 写入后崩溃、分片删除后崩溃、主库删除失败、分片缺失、重复 `session_delete`、pending 会话读取/发送。
- 导入：staging 复制中断、导入中崩溃、校验失败、journal 写入后崩溃、指针切换后崩溃、旧 generation rename 失败、`dry_run` 不切换。
- 恢复：staging 中断、指针切换后新库校验失败、回指旧库失败、journal 损坏、跨卷 staging 拒绝。
- 期望：启动恢复到 old 或 new 的**完整一套**；无半套、无孤儿事件、无可见空会话；失败可重试；证据归档。

#### 6. 边界

- 附件内容（ADR-035）不在 generation/DB 备份集内；恢复成功页/审计/诊断必须提示“附件未随库恢复”，不得静默当完整成功；附件备份/恢复另立 ADR。
- 日志/诊断/缓存不在 generation 内；不参与原子切换。
- 仍无线上数据迁移；旧扁平布局只支持重建。

---

## 3. 影响

| 影响面 | 变化 | 说明 |
|---|---|---|
| 设计文档 | D2/D3/D4/D7/D13/《设计文档》附录 C/§5 升 **v1.35** | 复审通过后执行；本 ADR 不改冻结设计 |
| ADR-034 / ADR-020 | **ADR-034 v1.1 / ADR-020 v1.1** | 导入 staging 多库化；备份集/manifest/整套加密 |
| P2 计划 | **v0.14** | 新增 M10-09/10/11；G13 条件；性能矩阵；迁移计数 |
| 命令 | **87（0 新增）** | 不新增 `events_*`/维护命令 |
| 事件 | **0 新增；EventEnvelope 9 字段不变** | 不新增事件类型；广播顺序不变 |
| 设置键 | **11（0 新增）** | 不新增设置键 |
| 迁移 | **0014 +1**（`sessions.shard_id` + `sessions.deletion_pending`）；0001–0013 不改；其余规划位不变 | 分片库完整执行同一链；不跳过 0014 |
| 业务码 | **35（0 新增）；`IpcErrorCode` 32 不变** | 复用 `storage_backpressure` + `maintenance_reason`；启动不变量复用 `db_open_failed`/`schema_checksum_mismatch` |
| `health` 响应 | 增加 `storage_check`（含 `write_gate`/`main_checked`）/`maintenance` 可选字段 | 响应契约变化；M9-01 基线重录；生成物由清单生成 |
| `backup_*` 响应 | 记录/候选增加 `format/set_path/manifest_path/manifest_sha256/shard_count/integrity` 等字段；恢复 staging 空间预检 ≥×1.1 | 契约冻结于 D-039-5 §4；旧 `.db` 恢复拒绝；M9-01 重录 |
| 归档/导出 | `session_export` 隐藏 pending；`archive_export` 排除 `deletion_pending=1` | 不新增命令/字段；M10-09 测试 |
| UI | 校验中/维护中/主库失败状态；S-00/S-08/S-06 优先级；旧 `single_file` 恢复禁用；S-12 `incomplete_generation` 提示 | UI-UX v0.20；新增锚点须登记 |
| 存储布局 | `CURRENT` + `generations/<id>/`（主库 + 分片 + `shards.json` + sidecar）；旧 generation（`.pre-*`/`.failed-*`）保留 1 份/清理/容量告警 | 备份/恢复/导入以 generation 为原子单位；附件不随库 |
| 性能 | 启动 1.5s；检查 25s/30s；写/读/备份/恢复目标 | 见 §6.3/§6.4 |
| 任务 | M10-09（L）/M10-10（M）/M10-11（M）+ G13 | 见 §6.2；M10-09 不得并入 M10-08 |
| 安全/数据 | 不新增外部信任边界/命令面/第三方依赖 | 主要风险是完整性检查绕过、跨库删除/导入/恢复原子性；见 §4/D-039-8 |

---

## 4. 主要风险与缓解

| 风险 | 缓解 | 失效条件 |
|---|---|---|
| **sidecar 漏检残余风险**：干净关闭全匹配但页级损坏未被发现 | 严格 sidecar（header change counter/size/WAL=0/schema）+ 每次干净启动后台 `quick_check` + 每周 `integrity_check` + 每 20 次/7 天强制完整检查 + 故障注入 | 故障注入发现漏检 → **立即回退每次启动阻塞 `quick_check`**，不得继续用 sidecar 跳过 |
| **维护态备份可用性**：备份/恢复期间其他功能不可用，用户可能强退/中断 | 维护态独占、UI 明确提示、`health` 进度、`app_exit` 允许、generation/journal 整套回滚、审计 | 备份无法完成/恢复失败 → 整套 generation 回滚；仍失败 → 拒绝启动并报告，不得半套库 |
| **多库 manifest 失败**：分片/主库哈希或版本不一致 | 逐文件 sha256 + `manifest.sha256` + `CURRENT.shards_sha256`；恢复前全量 `quick_check` + 迁移版本；整套验收 | manifest 不一致 → 拒绝恢复；不得跳过校验 |
| **跨库删除/导入/恢复半套**：主库与分片不同步 | D-039-8 tombstone、generation staging、`CURRENT` 唯一提交点、journal 启动恢复、故障注入 | 出现半套/孤儿事件/可见空会话 → 停止实施；按整套备份恢复并另立 ADR |
| **旧 generation 累积**：重复恢复/导入/失败切换填满磁盘 | 默认保留 1 份旧 generation（`.pre-*`/`.failed-*`）；安全窗口自动清理；`>1.5×` live 告警；诊断列出各 generation；清理中断不影响当前 | 磁盘被旧 generation 填满或清理损坏当前 → 停止自动清理、告警并人工处理 |
| **staging 空间不足**：恢复复制整套 generation 失败 | 恢复前预检 staging ≥ generation ×1.1；不足拒绝且当前库不变；不进入复制 | 空间不足仍复制/覆盖 → 拒绝合入 |
| **已请求删除数据被导出** | `session_export` 隐藏 pending；`archive_export` 排除 pending 或标记；测试 | pending 仍被导出 → 缺陷，阻止合并 |
| **路径穿越/符号链接**：`CURRENT`/`shards.json`/manifest 指向目录外 | generation 正则 + `generations/` 前缀；相对路径/无 `..`；canonicalize + 前缀；symlink/junction 根外拒绝；故障注入 | 越界打开/复制/覆盖 → 拒绝合入，安全评审阻断 |
| **检查中写门/UI 双事实源**：`storage_backpressure` 被误判降级 | `health.storage_check.write_gate` 为启动准入唯一事实源；UI 优先级定稿；错误映射表；S-08 不得仅凭错误码 | 校验中显示 S-08/维护中显示降级 → UI 缺陷，阻止合并 |
| **并行检查平台差异**：HDD/macOS/Linux 墙钟可能 >30s | NVMe 8 / SATA SSD 4 / HDD 2；平台矩阵；release 复测；按磁盘降级 | 参考环境 >30s 硬阈值 → 降低并发/调整 N 或另立 ADR；**不得放宽 30s** |
| **`auto_vacuum` 兼容性**：既有库/平台可能未生效 | 新装建表前设置 `INCREMENTAL`；开发库重建；`PRAGMA` 校验；维护态 `VACUUM` 兜底 | 实测未生效 → 维护态 `VACUUM` + 告警；不得宣称自动收缩 |
| 主库 `events` 非空/回退读主库 | 启动断言 + 测试 + 代码审查；非空拒绝启动 | 出现静默回退 → 拒绝合入；数据修复后重建/恢复 |
| 会话跨片 / seq 冲突 | 稳定哈希 + `sessions.shard_id` + 会话 sequencer + 片内 `UNIQUE` + 固定测试向量 | 发现跨片 → 拒绝合入；已发生 → 停写、恢复整套备份 |
| 单写任务成为分片写热点 | 每片 group commit；队列深度/写 P95 监控；不引入每片写者 | 写 P95 >50ms 持续 → 调查路由/批量；不得以多写者绕过 D3 |
| 保留清理拖慢/超时 | `(type,ts)` 覆盖索引选批 + 两阶段 + 500/1000/2000/5000 矩阵 + 删除事务 P95 <500ms | 单事务 >500ms → 缩小批；不得放宽 D3 |
| 启动预算被错误放宽/用 debug 验收 | release daemon + 真实 Electron + fresh userData/registry；`initializeMs`/`windowShownAt`/`uiReadyAt`/`mainInteractiveAt`/`writesEnabledAt`；中位/P95/max | `initialize`/`windowShownAt`/`uiReadyAt` 中位 >1.5s 或检查 >30s → G13 不通过；不得改回 2s/8s |
| 维护态/检查中被误显示为降级 | `storage_check.write_gate` + `maintenance.kind`；S-00/S-06；UI 优先级；测试 | 校验中 S-08 或维护中 `persist_degraded` → UI 缺陷，阻止合并 |
| 启动不变量错误映射错误 | D-039-6 映射表；首帧 error/exit/S-12 reason 断言；M9-03 风格测试 | 映射缺失/只返回 `internal` → 拒绝合入 |
| 物理高水位误判为容量达标 | live/物理分开报告；incremental auto_vacuum + 维护态 VACUUM；>5 GiB 告警 | 物理 >5 GiB 且无告警 → 证据无效 |

---

## 5. 备选方案（未采纳）

| 方案 | 否决理由 |
|---|---|
| **仅启动优化、不分库** | ADR-038 的硬触发只有 `quick_check` >30s（D-038-2），M10-08 已判“需分库”；备份/恢复/补读在阈值内，属于分库的**支持性收益**，不是越线项/瓶颈；5.22 GiB 物理高水位是独立容量风险；G13 要求闭环“需分库”结论。仅跳过/后台检查不能改变总检查字节数和容量。**保留为已否决备选，不作为实施分支。** |
| 每片独立写者 | 违反 D3 单写者语义；SQLite 多写者锁竞争/跨片一致性复杂；R3 已明确“单写任务 + 每片连接”。 |
| 在线迁移 / 双读（均不采用） | 开发阶段无线上版本（R4）；会扩大范围、引入双写/双读不一致与回退风险；只做新装初始化/开发库重建。 |
| 只凭 mtime/大小跳过 `quick_check` | 无法发现页级损坏；必须严格 sidecar（header change counter/size/WAL=0/schema）+ 周期完整检查。 |
| 关闭 `quick_check` | 违反 D3 完整性检查；只能以“干净关闭跳过 + 后台/并行检查”替代。 |
| 放宽 D3 单事务 <500ms | 以降低正确性/稳定性换取通过测试，违反 AGENTS §4/§6；必须优化批选/删除。 |
| 单库 + 仅后台检查 | `quick_check` 仍 97.8s、`uiReadyAt` 仍 101s；无法满足 1.5s/30s；且不解决容量与备份/恢复。 |
| 时间分片作为在线主分片 | 读放大、会话跨片、补读复杂；会话哈希为主，时间只作归档辅助。 |
| 压缩/加密换取更小库 | 改变存储格式与备份契约；需另立 ADR；不解决 `quick_check`/启动预算。 |
| 新增维护命令/设置键 | 契约冻结（R9）；`health` 可选字段 + 维护原因复用已足够；新增须另立 ADR。 |

---

## 6. 契约登记、实施任务与验收

### 6.1 契约登记汇总

| 契约面 | 登记 | 说明 |
|---|---|---|
| 命令 | **0 新增 / 87 不变** | 不新增 `events_*`、备份/维护命令 |
| 事件 | **0 新增 / EventEnvelope 9 字段不变** | 不新增事件类型；广播顺序不变 |
| 设置键 | **0 新增 / 11 不变** | 不新增设置键 |
| 迁移 | **1 新增：0014 `sessions.shard_id` + `sessions.deletion_pending`** | 0001–0013 不改；0014 +1；其余规划位不变；分片库完整执行 0014 |
| 业务码 | **0 新增 / 35 不变；`IpcErrorCode` 32 不变** | 复用 `storage_backpressure` + `maintenance_reason`；启动不变量复用 `db_open_failed`/`schema_checksum_mismatch` |
| `health` | **响应字段增量**：`storage_check`（含 `write_gate`/`main_checked`）/ `maintenance` 可选 | 非新命令；M9-01 基线重录；生成物由清单生成 |
| `backup_*` | **响应字段增量**：`format/set_path/manifest_path/manifest_sha256/shard_count/integrity` 等；`external.path` 目录/`manifest.json` | 冻结于 D-039-5 §4；旧 `.db`/`.aebk` 拒绝；`single_file` UI 禁用；M9-01 重录 |
| 归档/导出 | `session_export` 隐藏 `deletion_pending=1`；`archive_export` 排除 pending | 语义约束，不新增命令/字段；M10-09 测试 |
| UI-UX | **v0.20**：校验中/维护中/主库失败；旧 `single_file` 恢复禁用；S-12 半成品 generation 提示 | S-00/S-08/S-06 优先级以 `health.storage_check`/`maintenance` 为准；新增锚点须登记 |
| 脚本 | `scripts/test/m10-09/*`、`m10-10/*`、`m10-11/*` | 初始化/路由/删除/导入/备份/恢复/故障注入/并行检查/保留矩阵 |
| 证据 | `docs/M10-09-证据.md`、`M10-09-metrics.json`；M10-10/M10-11 同名 | P50/P95/max、失败计数、复跑命令、未验证项 |
| 门禁 | `pnpm verify:m10-09/10/11`、`verify:m9-01-baseline`、`verify:error-codes`、`verify:m1-03`、`protocol check` | 生成物 diff=0；迁移/契约计数通过 |

**本 ADR 文档自身登记：0 命令 / 0 事件 / 0 设置键 / 1 迁移（0014 `sessions.shard_id` + `sessions.deletion_pending`）。**

### 6.2 实施范围、任务与 DoD

| 任务 | 量级 | 依赖 | 范围 | 证据/验证 |
|---|---|---|---|---|
| **M10-09 事件分库实施** | L | M10-08、M10-00 | generation/`CURRENT` 初始化；0014 `sessions.shard_id` + `deletion_pending`；单写任务路由；读路由；会话删除 tombstone；导入 staging 整套切换；备份/恢复外部路径校验；旧 generation 保留/清理/容量；附件提示；保留/归档/诊断；维护态备份/恢复；`health.storage_check`（分片汇总）；主库 `events` 空表断言；启动错误映射；fail-closed 启动不变量 | `docs/M10-09-证据.md`、`docs/M10-09-metrics.json`、`scripts/test/m10-09/*`、`pnpm verify:m10-09` |
| **M10-10 启动并行检查与预算** | M | M10-09 | 主库 + 分片并行 `quick_check`；并发上限；干净关闭 sidecar；异常启动 `write_gate`；`health.storage_check`（write_gate/main_checked）+ `health.maintenance`；`initialize`/`uiReadyAt`/`mainInteractiveAt`/`writesEnabledAt`；UI 校验/维护态；release 真机复测 | `docs/M10-10-证据.md`、`docs/M10-10-metrics.json`、`scripts/test/m10-10/*`、`pnpm verify:m10-10` |
| **M10-11 保留清理与文件收缩** | M | M10-09 | `(type,ts)` 选批；归档+删除两阶段；500/1000/2000/5000 批矩阵；`incremental auto_vacuum`；维护态 `VACUUM` 兜底 | `docs/M10-11-证据.md`、`docs/M10-11-metrics.json`、`scripts/test/m10-11/*`、`pnpm verify:m10-11` |
| **G13 P2 退出** | Gate | G9–G12、M10-09/10/11 | 设计 v1.35；ADR-034 v1.1/ADR-020 v1.1；M10-08 报告归档；M9-01 基线；P0/P1 复跑；M9 G8 保持；独立签核 | `docs/M10-Gate-G13-报告.md` |

- **M10-08 不重开**：保持“已执行、需分库、只验证不实施”；只允许补充字段说明（P2-1/P1-2 口径）与“A 已选定，实施见 M10-09/10/11”，不改 DoD、不重跑、不改变判定。
- **M10-09 不得并入 M10-08。**
- 排期紧张时可将 M10-10/M10-11 合并为 M10-10“启动与保留优化”，但**证据分册、DoD 分开验收**。

**M10-09 DoD（必须可执行验证）：**

1. 新装初始化：`generations/<id>/` + 主库 + 8 分片 + `shards.json` + `CURRENT`；主库成功 + 8/8 分片成功；任一步失败无残留；半成品清理有故障注入证据；**fresh userData 预创建 `logs/`（模拟 Electron）仍能初始化，仅 `logs/` 不触发 S-12**；无 `CURRENT` + 单 generation + 无 journal 时 S-12/诊断标记 `incomplete_generation` 并提示手动删除/重建，不自动切换/删除。
2. 0014 生效；`sessions.shard_id` 非空且 0–7；`deletion_pending` 默认 0 且仅 0/1；稳定哈希固定测试向量；会话不跨片；主库 `events` 行数 = 0 断言；非空拒绝启动；`shards.json` 不含 `generation`（`CURRENT` 唯一绑定）；`clean_shutdown.generation` 匹配 `CURRENT`；manifest `source_generation` 仅信息性。
3. 单写任务 + 每片连接；无跨片事务；事件先落分片后广播；`messages` 终稿独立事务；写 P95 ≤50ms（10 并发、600s）。
4. 读路由正确；`events_page` P95 <150ms；无主库 `events` 回退；补读 `last_seq`/`complete` 语义不变。
5. 保留/归档/导入/备份/诊断按 generation/分片路由；备份套 manifest + `manifest.sha256` 完整；整套恢复/导入成功或整套回滚；**备份/恢复维护态**除 `health`/`app_exit` 外全部拒绝；导入维护窗口按 ADR-034 既有 allowlist；旧 generation（含 `.pre-*`/`.failed-*`）默认保留 1 份、清理中断不破坏当前、容量 >1.5× live 告警、诊断列出各 generation；恢复前 staging 可用空间 ≥ 备份 generation ×1.1，不足拒绝且当前库不变；恢复成功页/审计/诊断提示“附件未随库恢复”。
6. 会话删除 tombstone：pending 隐藏、分片 events 删除、主库行删除、崩溃/重试/启动恢复；无孤儿事件/可见空会话；`archive_export` 排除 `deletion_pending=1` 会话（或标记 `pending_deletion`），补测试。
7. 导入：多库 staging generation、事件路由、主库 events 空、`dry_run` 不切换、失败整套回滚；`session_import`/`archive_import` 契约不变。
8. 启动错误映射：DB 布局工件存在但 `CURRENT` 缺失/损坏、主库 events 非空、分片缺失、`shard_id` 越界、`deletion_pending` 非 0/1、`format_version` 不支持、`shards_sha256` 不匹配、`generation`/路径越界 → `internal` + `db_open_failed` + exit 22 + S-12 reason；schema 不符 → `schema_checksum_mismatch`；首帧 error 断言。
9. 契约：87/35/11/0014；无新增命令/事件/设置键；`backup_*` 字段按 D-039-5 §4；`external.path` 接受备份集目录/`manifest.json`，旧 `.db`/`.aebk`/不存在/非备份目录显式拒绝；`single_file` 记录 UI 恢复按钮禁用并提示；契约/UI 测试；`pnpm --filter @aether/protocol check` diff=0；`pnpm verify:error-codes`、`pnpm verify:m9-01-baseline`、`pnpm verify:m1-03` 通过。
10. 故障注入：单分片缺失/损坏、主库 `events` 非空、`shard_id` 越界、会话跨片、删除各阶段崩溃、导入各阶段崩溃、WAL 未 checkpoint、manifest 不一致、备份中写入、恢复中断、旧 generation 清理中断、路径穿越（`../`）/绝对路径/symlink/junction 越界、无效 generation/format_version/shards_sha256、Windows `ReplaceFileW`/`MoveFileExW` 失败 → 安全模式/拒绝/整套回滚；当前 generation 不受损。
11. `docs/M10-09-证据.md`、`docs/M10-09-metrics.json`、`scripts/test/m10-09/*`、`pnpm verify:m10-09` 归档。

**M10-10 DoD：**

1. 启动序列不含阻塞 `quick_check`；`health` 在检查完成前可用并返回 `storage_check.state=checking` + `write_gate=checking` + `main_checked`。
2. `initializeMs` ≤1.5s、`windowShownAt` ≤1.5s、`uiReadyAt` ≤1.5s、`storageCheckCompletedAt`（主库 + 全部分片）≤25s 目标/≤30s 硬；干净启动 `mainInteractiveAt`/`writesEnabledAt` ≤1.5s；异常启动二者 = 检查完成 ≤25s/≤30s；3 次中位/P95/max；release `aetherd` + 真实 Electron + fresh `userData`/registry。
3. 严格 sidecar 全匹配才跳过阻塞检查；异常/sidecar 不匹配/版本升级走并行检查 + 写门；恢复/导入/迁移后 sidecar 失效。
4. 干净关闭后台检查、每周 `integrity_check`（主库 + 分片）、每 20 次/7 天强制完整检查；`last_full_check_at`/`clean_start_count` 记录与重置。
5. `health.storage_check`（state/mode/write_gate/main_checked/checked_shards/total_shards）与 `health.maintenance`（backup/restore）契约；检查中不是降级；失败才 `persist_degraded`；UI 以 `storage_check` 优先，不得校验中显示 S-08。
6. UI：S-00 校验进度、S-08 失败、S-06 维护页、主库失败文案；无新增锚点或已按 UI-UX v0.20 登记；维护态不显示为降级。
7. 并发 4/8/HDD 2；单分片/主库/总耗时/失败数记录；不得只报平均。
8. 故障注入：sidecar 不匹配、WAL 非 0、页级损坏、恢复中断、迁移后 sidecar 失效、备份中写入、write_gate/UI 映射。
9. `docs/M10-10-证据.md`、`docs/M10-10-metrics.json`、`scripts/test/m10-10/*`、`pnpm verify:m10-10` 归档。

**M10-11 DoD：**

1. `EXPLAIN QUERY PLAN` 确认 `(type,ts)` 选批无 TEMP B-TREE；不再 `ORDER BY rowid`。
2. 归档先行、失败跳过该批；删除事务 P95 <500ms；500/1000/2000/5000 四档矩阵；选最大合规档（预期 500–1000）。
3. 最终清理集合不变；单批成员/归档顺序可变；`audit_log` 不清理；补读契约不变。
4. 全量历史清理 ≤4h、日常增量 ≤5min；不阻塞启动/UI。
5. 新装主库/分片 `auto_vacuum=INCREMENTAL`；有界 `incremental_vacuum`；物理 >5 GiB 时维护态 `VACUUM` + 空间预检 + 告警。
6. live/物理分开报告；不得用 1.387 GiB live 宣称物理达标。
7. `docs/M10-11-证据.md`、`docs/M10-11-metrics.json`、`scripts/test/m10-11/*`、`pnpm verify:m10-11` 归档。

### 6.3 验收矩阵

| 场景 | 指标 | 目标 | 证据 |
|---|---|---|---|
| 新装初始化 | `generations/<id>` + 主库 + 8 分片 + `CURRENT`；半成品清理 | 主库成功 + 8/8 分片；失败无残留 | `M10-09-证据.md` |
| 首启判定 | fresh userData 预创建 `logs/`；DB 布局工件为空 | 成功初始化；仅 `logs/` 不触发 S-12 | `M10-09-证据.md` |
| 半成品 generation | 有 `generations/`、无 `CURRENT`、无 journal | S-12/诊断标记 `incomplete_generation`；提示手动删除/重建；不自动切换/删除 | 同上 |
| 10 并发写 | 写 P95 | ≤50ms | `M10-09-metrics.json` |
| 补读 | `events_page` P95 | <150ms | 同上 |
| `quick_check` 并行 | 主库 + 分片墙钟 | ≤25s 目标 / ≤30s 硬 | `M10-10-metrics.json` |
| 干净启动 | `initializeMs` / `windowShownAt` | ≤1.5s / ≤1.5s | `M10-10-metrics.json` |
| 干净启动 | `uiReadyAt`（壳层 + 首个 `health`，不要求可写） | ≤1.5s | 同上 |
| 干净启动 | `mainInteractiveAt` / `writesEnabledAt` | ≤1.5s / ≤1.5s | 同上 |
| 干净启动 | `storageCheckCompletedAt`（主库 + 分片） | ≤25s / ≤30s | 同上 |
| 异常启动 | 并行检查 + `write_gate=checking` + 失败降级 | 无半写/无漏检；`mainInteractiveAt`/`writesEnabledAt`=检查完成；失败 S-08 | 故障注入 |
| 会话删除 | tombstone/崩溃/重试/启动恢复；`archive_export` 排除 pending | 无孤儿事件/可见空会话；pending 不被导出；幂等 | `M10-09-证据.md` |
| 导入 | staging generation/整套切换/dry-run | 成功或整套回滚；dry-run 不切换 | 同上 |
| 备份维护态 | 其他命令拒绝；备份 ≤600s | 100% 拒绝；manifest+`manifest.sha256` 完整 | `M10-09-证据.md` |
| 外部备份路径 | 目录（含 manifest）/`manifest.json`/旧 `.db`/旧 `.aebk`/不存在/非备份目录 | 前两者接受；后四者拒绝 | 同上 |
| 恢复 | 整套校验/回滚；≤900s；`CURRENT` 唯一提交点 | 成功或整套回滚；无半套 | 同上 |
| 恢复空间预检 | staging 可用空间 ≥ 备份 generation（解密后）×1.1 | 不足拒绝；当前库不变；不进入复制 | 同上 |
| 旧 generation | 默认保留 1 份（`.pre-*`/`.failed-*`）；清理中断；容量告警；诊断列表 | 当前 generation 不受损；>1.5× live 告警；各 generation 可见 | 同上 |
| 附件提示 | 恢复成功页/审计/诊断 | 明确“附件未随库恢复”；不显示为完整成功 | 同上 |
| 保留清理 | 删除事务 P95；单批 500/1000/2000/5000 | P95 <500ms；归档先行 | `M10-11-metrics.json` |
| 文件收缩 | `incremental auto_vacuum` / `VACUUM` | 物理 <5GiB 或明确告警 | 同上 |
| 契约 | 命令 87 / 业务码 35 / 设置键 11 / 迁移 0014 | 计数不变；生成物 diff=0 | `verify:m9-01-baseline`、`protocol check`、`verify:error-codes`、`verify:m1-03` |
| M9-01 | `health`/`backup_*` 新字段 | 基线重录；`verify` 通过 | `M9-01-baseline` |
| 启动错误 | DB 布局/`CURRENT`/主库 events/分片缺失/`shard_id`/`deletion_pending`/`format_version`/`shards_sha256`/schema | 首帧 error + exit 22 + S-12 reason 符合 D-039-6 | M10-09 证据 |
| 路径安全 | generation 正则/`generations/` 前缀；shards/manifest 相对路径；`../`/绝对路径/symlink | 越界拒绝；内部 `db_open_failed`/外部 `backup_candidate_invalid` | M10-09 证据 |
| UI | 校验中/维护中/主库失败 | 文案/锚点无回归；校验中不显示 S-08 | UI-UX v0.20 证据 |
| UI/S-06 | 旧 `single_file` 备份记录 | 恢复按钮禁用；提示“请使用整套备份或重建” | M10-09 证据 |
| 故障注入 | 单分片损坏/缺失、WAL 未 checkpoint、sidecar 不匹配、备份中写入、恢复/导入/删除中断、主库 `events` 非空、write_gate 映射、路径穿越/symlink、旧 generation 清理中断、Windows `CURRENT` 替换失败 | 安全模式/拒绝/整套回滚；当前 generation 不受损 | 故障注入报告 |

### 6.4 性能复测参数（R8）

- 数据集：5.22 GiB / 20,000,000 行 / 100 会话 / 固定 `sha256`；每次 run 复制。
- N=8；单片约 0.65 GiB；上限 1.5 GiB；超过评估 resharding。
- 并行 worker：默认 4，最大 8；NVMe 8，SATA SSD 4，HDD 2。
- 启动：`STARTUP_REVEAL_GRACE_MS=1300`；`initializeMs` ≤1.5s/硬超时 10s；主库 + 全部分片并行检查；`uiReadyAt` 1.5s；干净 `mainInteractiveAt`/`writesEnabledAt` 1.5s，异常 = 检查完成；`storageCheckCompletedAt` 25/30s；release `aetherd`；真实 Electron；fresh `userData`/registry；3 次中位/P95/max。
- 写/读：10 并发、600s；写 P95 ≤50ms、补读 P95 <150ms。
- 备份/恢复：3 次；≤600s / ≤900s；manifest 校验。
- 保留：500/1000/2000/5000 批矩阵；删除事务 P95 <500ms；归档 fsync 单独计时。
- 平台：Windows NVMe 参考；Windows SATA SSD/HDD 冒烟；macOS/Linux 有环境则补，无法执行必须写明。
- **5.35× 只用于说明并行内部收益；产品/验收统一用同口径 3.76×（72,861.791 / 19,357.664）。**

### 6.5 证据与验证要求

- 每个任务必须有：指标 JSON（P50/P95/max、失败计数）、原始日志/哈希、复跑命令、未验证项说明。
- 不得只写“完成/已实现”；不得只报平均；不得静默跳过失败样本。
- 证据脱敏：无用户名/绝对路径/密钥；使用 `<home>`/`<external>` 占位。
- 不绕过权限网关；不先广播后写日志；不在日志/诊断/备份 manifest/证据中泄露密钥。
- 生成物：`types.ts`/`client.ts`/`command-manifest.json`/`error_codes.json` 由 `aether-app` 清单生成；禁止手改；`pnpm --filter @aether/protocol check` diff=0。
- `pnpm verify:m9-01-baseline`、`pnpm verify:error-codes`、`pnpm verify:m10-00`、`pnpm verify:m1-03` 必须通过。
- 迁移：0001–0013 不改；0014 新增（`sessions.shard_id` + `sessions.deletion_pending` + 索引）并登记《设计文档》附录 C；`verify:m1-03` 复跑。
- generation/`CURRENT`/journal/`manifest.sha256` 的哈希/路径/恢复矩阵必须进入证据；路径约束（generation 正则/前缀、`shards[].path`/manifest 相对路径、symlink/junction、`../`/绝对路径）、旧 generation（`.pre-*`/`.failed-*`）清理、staging 空间预检、`archive_export` 排除 pending、半成品 generation 提示、附件提示、旧 `single_file` UI 禁用均须有测试/证据；旧扁平布局与旧 `.db` 备份只允许拒绝并提示重建/整套恢复。

### 6.6 独立签核要求

- 不得只有 M10-08 执行者自评；ADR-039 §8 已记录**架构/安全/QA/产品**四角色独立签核。
- G13 报告必须引用四角色签核与 M10-09/10/11 证据；任一 P0 条件未闭环 → G13 不通过。
- **v1.0 四角色签核已完成**：可升设计 v1.35 / P2 v0.14，并可启动 M10-09/10/11 实现；G13 仍需实施证据。

---

## 7. 回退条件

1. 本 ADR v1.0 已签署；若后续复审撤销/回退或签核被撤回 → 停止分库/启动优化实施；M10-08 结论保留“需分库”记录；G13 等待，不得以 v0.1–v0.5 或口头裁定代替。
2. 启动优化导致数据损坏/漏检 → **立即回退为每次启动完整 `quick_check`**，删除/失效 sidecar；在根因修复前不得恢复跳过。
3. sidecar 故障注入发现“匹配但损坏漏检” → 同上，回退阻塞检查；安全/QA 签核不通过则不得发布。
4. 分库实施未通过 M9 基线/迁移/恢复演练 → 不发布；开发阶段关闭分库开关（仅开发构建）并**重建开发库**，或从整套备份恢复；**不保留运行时单库回退窗口**。
5. `CURRENT`/generation 不完整（含 `incomplete_generation`）、路径/`format_version`/`shards_sha256`/`deletion_pending` 无效、主库 `events` 非空、分片缺失、`shard_id` 越界、会话跨片 → fail-closed：拒绝启动/停止写入；按整套 generation 备份恢复或重建开发库；不得静默修复；半成品 generation 只提示手动删除/重建，不自动切换。
6. 维护态备份无法保证整套一致性/恢复失败/导入切换失败/旧 generation 清理失败/staging 空间不足 → 停止相关功能上线；按 D-039-8 回指旧 generation；不得以“部分分片备份”或半套 generation 替代整套；清理失败仅告警保留；staging 不足直接拒绝且当前库不变。
7. 会话删除 tombstone 无法重试/启动恢复失败 → 停止删除功能上线；保留 pending 并修复；不得先删主库或暴露空会话。
8. `backup_*` 契约/旧 `.db` 拒绝策略未按 D-039-5 §4 落地 → 不发布；不得让 UI/客户端自行猜测响应结构。
9. 阈值调整 → 记录常量级调参证据；改变判定语义（30s、1.5s、500ms）须另立 ADR，不得静默放宽。
10. 发布后（若未来有线上用户）不得无 ADR 回退/降级；回退必须走新 ADR + 整套备份恢复 + 独立签核。

---

## 8. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-11-12 | M10-08 执行（AI） | v0.1 草案 | 证据驱动；未批准、未实施；等待设计/架构评审 |
| 2026-11-14 | 架构/安全/QA/交付评审（AI，OpenCode） | **方向通过（A）；v0.1 文本不通过** | 见 `docs/ADR-039-评审意见.md`；要求按 §3/§4 改写 v0.2 后独立复审 |
| 2026-11-14 | 用户 | **R1–R10 裁定** | A / 1.5s / 单写模型 / 无迁移 / 维护态备份 / 严格 sidecar / 两阶段清理 / 参数按建议 / `health` 可选字段 / M10-09/10/11 |
| 2026-11-14 | ADR-039 v0.2 修改者（AI） | 按首轮意见改写；**提交 v0.2 复审** | 见 `docs/ADR-039-评审意见.md` |
| 2026-11-14 | 架构/安全/QA/交付复审（AI，OpenCode） | **v0.2 暂不通过；要求 v0.3** | `uiReadyAt`/S-00、写门、跨库原子性、`backup_*` 契约、恢复/错误映射/P2 |
| 2026-11-14 | ADR-039 v0.3 修改者（AI） | 按复审意见改写；提交 v0.3 三审 | 见 `docs/ADR-039-复审意见.md` |
| 2026-11-14 | 架构/安全/QA/交付三审（AI，OpenCode） | **v0.3 暂不通过；要求 v0.4** | 1 P0 + 3 P1 + 5 P2；见 `docs/ADR-039-三审意见.md` |
| 2026-11-14 | ADR-039 v0.4 修改者（AI） | 按三审意见改写；提交 v0.4 四审 | 见 `docs/ADR-039-三审意见.md` |
| 2026-11-14 | 架构/安全/QA/交付四审（AI，OpenCode） | **文本评审通过（Conditional Pass）；无剩余 P0/P1** | P2-1～P2-4 随 v0.5/M10-09 处理；见 `docs/ADR-039-四审意见.md` |
| 2026-11-14 | ADR-039 v0.5 修改者（AI） | 落实四审 P2-1～P2-4；提交四角色签核 | 见 `docs/ADR-039-四审意见.md` |
| 2026-11-14 | 架构/安全/QA/产品四角色 | **通过（v1.0 签署）** | 四角色独立签核完成；可进入设计 v1.35/P2 v0.14 与 M10-09/10/11 |

**四角色独立签核（v1.0 签署）：**

| 角色 | 关注点 | 结论/日期 |
|---|---|---|
| 架构 | 分库写模型、seq/顺序、D13 多库一致性、N/resharding、D2/D3/D4/D7/D13 升版 | **通过 / 2026-11-14** |
| 安全/数据 | 干净关闭判据、完整性检查绕过、跨库删除/导入/恢复、路径约束、备份/恢复、密钥 0 泄漏、审计 | **通过 / 2026-11-14** |
| QA | 验收矩阵、5.22 GiB 复测、故障注入、证据可复跑 | **通过 / 2026-11-14** |
| 产品 | 1.5s/initialize 预算、检查中/维护中 UX、设置键 0 新增、G13 范围 | **通过 / 2026-11-14** |

> **v0.1 首轮不通过，v0.2 复审暂不通过，v0.3 三审暂不通过；v0.4 四审文本通过（Conditional Pass），v0.5 落实 P2；v1.0 四角色签核完成，已签署。** 可进入设计 v1.35 / P2 v0.14 升版与 M10-09/10/11 实施；文本/签署通过不等于 G13 通过，G13 仍需实施证据与独立验收。

---

## 9. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v1.0 | 2026-11-14 | **四角色签署**：架构/安全/QA/产品通过；状态改为“已签署”，解锁设计 v1.35/P2 v0.14 与 M10-09/10/11（G13 仍需实施证据）；v1.0 = v0.5 文本，无内容变更 | ADR-039 v1.0 签署记录（AI 代记） |
| v0.5 | 2026-11-14 | 按 `docs/ADR-039-四审意见.md` 落实 P2：恢复 staging 空间预检 ≥ generation ×1.1；`.failed-*` 纳入清理/诊断；`archive_export` 排除 `deletion_pending=1`；半成品 generation 的 S-12 `incomplete_generation` 提示；新增附录 H | ADR-039 v0.5 修改者（AI） |
| v0.4 | 2026-11-14 | 按 `docs/ADR-039-三审意见.md` 修订：首启改为 DB 布局工件判定（忽略 `logs/` 等）；`backup_restore` 外部路径校验（目录/`manifest.json`，旧 `.db`/`.aebk` 拒绝）；旧 generation 保留 1 份/自动清理/容量告警；`CURRENT.generation`/`shards.json.path`/manifest 路径约束与 symlink 拒绝；Windows `ReplaceFileW` 原子替换；`deletion_pending` CHECK/`format_version`/`shards_sha256` 错误映射；附件提示；旧 `single_file` UI 禁用；P2-5 generation 绑定；新增附录 G | ADR-039 v0.4 修改者（AI） |
| v0.3 | 2026-11-14 | 按 `docs/ADR-039-复审意见.md` 修订：`uiReadyAt` 与 S-00 解耦并新增 `mainInteractiveAt`/`writesEnabledAt`；`storage_check.write_gate`/`main_checked` 与 D4/UI 单一口径；`sessions.deletion_pending` tombstone；generation/`CURRENT`/journal 整套删除/导入/恢复；`backup_*` 契约冻结；`initialize` ≤1.5s；启动错误映射；P2-1～P2-5；新增 D-039-8 与附录 F | ADR-039 v0.3 修改者（AI） |
| v0.2 | 2026-11-14 | 按 `docs/ADR-039-评审意见.md` 收敛为唯一 A（分库 + 并行检查）；补写模型（单写任务 + 每片连接）、读路由、无迁移初始化、维护态备份/恢复、严格 sidecar、1.5s 三指标、两阶段清理、`health` 可选字段；补任务/DoD/验收矩阵/四角色签核；修正 5.35×/3.76× 口径；删除“候选/首选/回退单库”表述 | ADR-039 v0.2 修改者（AI） |
| v0.1 | 2026-11-12 | 创建草案：分库候选 + 启动检查优化（干净关闭跳过/后台检查/并行分片）+ 判定与风险 | M10-08 执行 |

---

## 附录 A：分片布局、初始化与启动不变量

**布局：**

| 路径 | 内容 |
|---|---|
| `<data_dir>/CURRENT` | 代际指针：`format_version/generation/shards_sha256/switched_at`；原子写 |
| `<data_dir>/generations/<generation_id>/aether.db` | 主库：`sessions`/`messages`/`runs`/`audit_log`/`settings` 等；`events` 表存在但恒空 |
| `<data_dir>/generations/<generation_id>/events/events-0..7.db` | 分片库：完整 0001–0014 schema；业务只读写 `schema_migrations`/`events`（含两个索引） |
| `<data_dir>/generations/<generation_id>/events/shards.json` | 分片清单：`shard_count=8`、`shards[]`（id/path/schema_version）、`schema_version`、`app_version`、`created_at`；**不含 `generation` 字段**（`CURRENT` 为唯一绑定） |
| `<data_dir>/generations/<generation_id>/clean_shutdown.json` | 干净关闭 sidecar（原子写；字段见附录 B） |
| `<data_dir>/staging/`、`<data_dir>/backups/` | 恢复/导入 staging；备份集；不在 generation 内 |

**初始化顺序：**

1. 创建 `<data_dir>/generations/gen-<ulid>/`；主库建表/迁移到 0014。
2. 创建 `events/` 与 8 个分片；每个分片建表前设置 `auto_vacuum=INCREMENTAL`；完整执行 0001–0014（不跳过）。
3. 打开每个分片，校验 `schema_migrations`、`events`、两个索引存在。
4. 原子写 `shards.json`；原子写 `CURRENT` 指针；成功即初始化完成。
5. 任一失败 → 删除半成品 generation/staging；清理失败 → 拒绝启动并报告路径。

**启动不变量（fail-closed）：**

- 首启只按 DB 布局工件判定：`aether.db`/`CURRENT`/`generations/` 均不存在 → 新装；存在任一但 `CURRENT` 缺失/损坏/旧扁平布局 → 拒绝启动（D-039-6 映射）；`logs/`、`backups/`、`attachments/`、`staging/`、`run/` 不影响首启；无 `CURRENT` + 单 generation + 无 journal 标记 `incomplete_generation`，提示手动删除/重建，不自动切换/删除。
- `CURRENT` 存在/有效且 generation 完整；`generation` 正则、`generations/` 前缀、`shards[].path`/manifest 相对路径与 symlink/junction 约束按 D-039-8 §1（P1-3）。
- 主库 `events` 行数 = 0；非空拒绝启动。
- `sessions.shard_id` 非空且 `0..7`；`deletion_pending` 只能为 0/1（0014 `CHECK` + 启动断言）；越界/非法拒绝启动。
- `CURRENT.format_version` 必须为支持版本；`CURRENT.shards_sha256` 必须与 `events/shards.json` 哈希一致；不支持/不匹配拒绝启动。
- 8 个分片文件均存在且可打开；缺失/无法打开拒绝启动。
- 主库/分片 `schema_version` 与 manifest 一致；不一致拒绝启动。
- sidecar 的 `generation` 与 `CURRENT` 一致且全匹配才跳过阻塞检查；否则并行检查。
- 恢复/导入/删除 journal 先于 `health` 处理；失败整套回滚/保留 pending。
- 备份/恢复/导入期间维护态独占；除 `health`/`app_exit` 外拒绝。
- 主库或任一分片 `quick_check` 失败 → `persist_degraded` + S-08；不得继续写。

## 附录 B：`clean_shutdown.json` 字段

```json
{
  "generation": "gen-01J...",
  "app_version": "<version>",
  "closed_at": 1760000000000,
  "close_sequence_completed": true,
  "write_queue_drained": true,
  "background_check_completed": true,
  "last_full_check_at": 1759990000000,
  "clean_start_count": 12,
  "main": {
    "schema_version": 14,
    "header_change_counter": 123,
    "file_size": 123456,
    "wal_size": 0,
    "last_seq": null
  },
  "shards": [
    {
      "shard_id": 0,
      "schema_version": 14,
      "header_change_counter": 456,
      "file_size": 789012,
      "wal_size": 0,
      "last_seq": 12345
    }
  ]
}
```

- `header_change_counter` = SQLite 文件头偏移 24 的 4 字节 change counter；**不是**连接级 `PRAGMA data_version`。
- `wal_size=0` 是跳过阻塞检查的必要条件；任一库 WAL 非 0/checkpoint 失败 → 不写 clean sidecar。
- `background_check_completed` 记录上次会话后台检查是否完成；**不参与跳过判定**，仅用于周期检查调度/证据。
- `last_full_check_at` 为最近一次完整检查完成时间；`clean_start_count` 为连续干净启动计数：仅 sidecar 全匹配且干净启动时 `+1`；异常/不匹配/检查失败/降级/恢复/导入/迁移/版本升级/resharding 时重置 `0`；达到 20 次或距 `last_full_check_at` ≥7 天触发强制完整检查，完成后重置 `0`。
- `generation` 必须与 `CURRENT` 指向一致；不一致按不干净处理。
- sidecar 原子写失败 → 下次启动全量并行检查。

## 附录 C：`health` 状态矩阵

| `storage_state` | `storage_check.state` | `write_gate` | `main_checked` | `maintenance` | 写/新 run | UI | 说明 |
|---|---|---|---|---|---|---|---|
| `normal` | `checking`（`mode=startup`） | `checking` | false/true | 无 | 拒绝（`storage_backpressure` + `detail.reason=storage_check`） | S-00“正在校验数据（x/8，主库校验中/已完成）” | 异常启动/不匹配；不是降级 |
| `normal` | `checking`（`mode=background`） | `open` | false/true | 无 | 允许 | 可选轻提示 | 干净关闭后台补检 |
| `normal` | `ok` | `open` | true | 无 | 允许 | 无 | 检查通过 |
| `persist_degraded` | `failed` | `open` | true/false | 无 | 拒绝（`persist_degraded`） | S-08；“主库校验失败”如主库失败 | `degrade_trigger=integrity_failure` |
| `normal` | 保持上次结果 | `open` | 上次 | `backup` / `restore` | 除 `health`/`app_exit` 外拒绝（`storage_backpressure` + `maintenance_reason`） | S-06 维护页/横幅 | 维护态本身不是降级 |
| `persist_degraded` | 保持上次结果 | `open` | 上次 | `backup` / `restore` | 仅维护例外，仍只 `health`/`app_exit`；恢复后重启 | S-06 维护页 + S-08 降级标识 | 维护不改变降级事实；不得误报 |

## 附录 D：评审问题闭环对照

| 首轮编号 | 问题摘要 | v0.2 处置（首轮） |
|---|---|---|
| P0-1 | 分库/启动优化未收敛，优先级矛盾 | §2.0 唯一 A；§5 仅保留“仅启动优化”为已否决备选 |
| P0-2 | “检查中”状态/写路径/`health` 未定义，与 D3/D4 冲突 | D-039-2/D-039-3/D-039-5；检查中 `normal` + `checking`，失败才 `persist_degraded` |
| P0-3 | 分库缺写模型/D13 多库一致性 | D-039-1 单写任务 + 每片连接；D-039-5 维护态整套备份/恢复 |
| P0-4 | 无验收指标/DoD/验证方案；1.5s/8s 未裁定 | §6.2 DoD、§6.3 验收矩阵、§6.4 参数；1.5s 三指标 |
| P1-1 | 干净关闭判据不足 | D-039-2 严格 sidecar + 周期检查 + 故障注入 |
| P1-2 | 5.35× 基准不一致；仅 NVMe/Windows | §1.1/§1.3 同口径 3.76×；§6.4 平台/复测 |
| P1-3 | 保留 4.89s 非产品路径；TEMP B-TREE | D-039-4 产品路径基准 + `EXPLAIN` + 两阶段 |
| P1-4 | 多库 D13 备份/恢复/归档未设计 | D-039-5 维护态 + 逐片 `VACUUM INTO` + manifest + 整套恢复 |
| P1-5 | 迁移/回退/分片数量/扩容未定 | D-039-1/D-039-6 无迁移、N=8/1.5GiB、resharding ADR |
| P1-6 | “0 新增”与 0014/health/设置键不一致 | §3/§6.1 明确 1 迁移、health 响应增量、设置键 0 新增 |
| P1-7 | G13/后续任务链路断裂 | §6.2 M10-09/10/11 + G13；§6.6 独立签核 |
| P1-8 | 启动预算口径冲突（1.5s vs 8s；debug） | D-039-3 1.5s 三指标、1300ms、release 复测 |
| P1-9 | live 1.387GiB 与物理 5.22GiB 未统一；VACUUM 未测 | D-039-4 live/物理分开 + auto_vacuum + 维护态 VACUUM |
| P2-1 | `startupRequiresAdr`/`withinTarget` 语义易误读 | §1.3 字段说明；不改变 M10-08 判定 |
| P2-2 | “全局 sequencer”与 D4 表述不一致 | D-039-1 会话 sequencer；片内 `UNIQUE`；不声称全局 seq |
| P2-3 | “删除集合不变”未区分最终集合/单批 | D-039-4 最终集合不变；单批成员/顺序可变 |
| P2-4 | UI “不新增锚点”与校验/维护提示冲突 | D-039-3/D-039-7 复用 S-00/S-08/S-06；新增须登记 |
| P2-5 | 缺独立架构/安全/QA 签核 | §8 四角色签核；§6.6 要求 |

## 附录 E：0014 参考 DDL

```sql
-- 0014_session_shard.sql（主库/分片库均完整执行；分片库仅物理存在）
ALTER TABLE sessions ADD COLUMN shard_id INTEGER;
ALTER TABLE sessions ADD COLUMN deletion_pending INTEGER NOT NULL DEFAULT 0 CHECK (deletion_pending IN (0,1));
CREATE INDEX IF NOT EXISTS idx_sessions_shard_id ON sessions(shard_id);
CREATE INDEX IF NOT EXISTS idx_sessions_deletion_pending ON sessions(deletion_pending);
```

- 主库与分片库均执行同一 SQL（完整 0001–0014 schema）；分片库的 `sessions`/`shard_id`/`deletion_pending` 不参与分片业务，仅保持 `schema_version` 一致。
- `shard_id` 允许 NULL 受 SQLite `ALTER TABLE ADD COLUMN` 限制；运行时不变式要求新会话创建时写入非空，启动校验 NULL/越界即拒绝。
- `deletion_pending` 仅服务 D-039-8 的跨库删除 tombstone；默认 0，不能由普通会话命令置位。
- 0001–0013 不改；终态以《设计文档》附录 C 为准。

## 附录 F：v0.2 复审问题闭环对照

| 复审编号 | 问题摘要 | v0.3 处置 |
|---|---|---|
| P0-1 | `uiReadyAt` 与 S-00/写门冲突 | D-039-3：`uiReadyAt=壳层可见+首个 health`（不要求可写）；`mainInteractiveAt`/`writesEnabledAt` 单独度量；S-00 保留；§6.3 分列干净/异常 |
| P0-2 | 检查中写门与 D4/UI S-08 冲突 | D-039-3/D-039-5：`storage_check.write_gate` 为启动准入唯一事实源；`storage_state` 仍为降级唯一事实源；错误/UI 优先级表定稿；删除条件句 |
| P0-3 | 跨库导入/删除原子性未设计 | D-039-8：`sessions.deletion_pending` tombstone + 启动重试；导入 staging generation + `CURRENT` 切换；故障注入；附件边界 |
| P0-4 | `backup_*` 契约未冻结 | D-039-5 §4：冻结请求/响应/记录字段、旧 `.db` 拒绝、M9-01 重录、UI S-06 |
| P1-1 | `initialize` 2s vs 1.5s | D-039-3：5.22GiB 路径 `initialize` ≤1.5s、硬超时 10s；删除 2s；P2 矩阵同步 |
| P1-2 | §5 把未越线指标说成瓶颈 | §1.1/§5：硬触发仅 `quick_check` >30s；备份/恢复/补读改为支持性收益；物理高水位独立风险 |
| P1-3 | 整套恢复原子切换未定义 | D-039-8：generation + `CURRENT` + 同卷 staging + fsync + journal + 整套回滚；恢复中断注入 |
| P1-4 | 启动不变量错误映射未定义 | D-039-6：条件→`IpcErrorCode`/`business_code`/exit 22/S-12 reason/UI 表；不新增 reason |
| P2-1 | 读连接数公式矛盾 | D-039-1：主库 4 + 分片 ≤16，总 ≤20 |
| P2-2 | 分片库 schema/0014 表述歧义 | D-039-1/附录 A/E：分片库完整 0001–0014，所有表物理存在；只读写 `events`/`schema_migrations` |
| P2-3 | `clean_start_count` 重置未定义 | D-039-2/附录 B：异常/不匹配/失败/恢复/导入/迁移重置 0；仅干净匹配 +1 |
| P2-4 | `checked_shards` 不含主库 | D-039-5/附录 C：新增 `main_checked`；主库失败显示“主库校验失败” |
| P2-5 | 维护态缺 `persist_degraded` 行 | D-039-3/附录 C：增加 `persist_degraded + maintenance` 行；仍仅 `health`/`app_exit`，恢复后重启 |

## 附录 G：v0.3 三审问题闭环对照

| 三审编号 | 问题摘要 | v0.4 处置 |
|---|---|---|
| P0-1 | 首启“数据目录为空”与 `logs/` 冲突 | D-039-1/D-039-6/附录 A：改为 DB 布局工件判定；忽略 `logs/backups/attachments/staging/run`；fresh userData（含 logs）测试 |
| P1-1 | `backup_restore` 外部路径仍只接受 `.db`/`.aebk` | D-039-5 §4：`validate_external_backup_set`；目录/`manifest.json` 接受；旧 `.db`/`.aebk`/不存在/非备份目录拒绝；DTO/path.rs/M10-09 测试 |
| P1-2 | 旧 generation 保留/清理/容量告警未定义 | D-039-8 §1/§7：保留 1 份旧 generation；安全窗口自动清理；`>1.5×` live 告警；诊断列表；清理中断不影响当前 |
| P1-3 | generation/shards/manifest 路径约束缺失 | D-039-1/D-039-5/D-039-8：generation 正则 + `generations/` 前缀；相对路径/无 `..`；canonicalize + 前缀；symlink/junction 根外拒绝；错误映射/故障注入 |
| P2-1 | 附件未随库恢复可能静默 | D-039-5 §5/D-039-8 §6：恢复成功页/审计/诊断提示“附件未随库恢复”；附件备份另立 ADR |
| P2-2 | `deletion_pending`/`format_version`/`shards_sha256` 未入映射 | D-039-6 表 + 0014 `CHECK` + 启动断言：统一 `db_open_failed`；补测试 |
| P2-3 | 旧 `single_file` 恢复入口未禁用 | D-039-5 §4：UI S-06 禁用恢复并提示；契约/UI 测试 |
| P2-4 | Windows `CURRENT` 原子替换语义未固定 | D-039-8 §1：`ReplaceFileW`/`MoveFileExW` + `FlushFileBuffers`；禁止删除后 rename；M10-09 证据 |
| P2-5 | `shards.json`/manifest 的 generation 字段未统一 | D-039-1/附录 A：`shards.json` 不含 `generation`，`CURRENT` 唯一绑定；manifest `source_generation` 信息性；`clean_shutdown.generation` 必须匹配 |

## 附录 H：v0.4 四审 P2 闭环对照

| 四审编号 | 问题摘要 | v0.5 处置 |
|---|---|---|
| P2-1 | `backup_restore` staging 空间预检未明确 | D-039-5 §5：恢复前预检 `<data_dir>/staging` ≥ 备份 generation ×1.1；不足拒绝且当前库不变；M10-09 DoD/验收矩阵 |
| P2-2 | `.failed-*` generation 未纳入清理/诊断 | D-039-8 §1：`.failed-*` 与 `.pre-*` 同池；保留最近 1 份/标记可删；诊断列出状态；不得删除当前/journal 引用 |
| P2-3 | `archive_export` 未处理 `deletion_pending=1` | D-039-8 §2：`archive_export` 排除 pending（或标记 `pending_deletion`）；`session_export` 已隐藏；M10-09 测试 |
| P2-4 | 半成品 generation 仅 fail-closed，无提示/入口 | D-039-6/附录 A：S-12/诊断标记 `incomplete_generation`，提示删除半成品 generation 或重建；不自动切换/删除；M10-09 DoD/验收矩阵 |
