# ADR-006：启动迁移命令面与错误码扩展

| 项 | 内容 |
|---|---|
| 状态 | **已批准**（2026-09-18 复评通过，签署见 §7；实现对齐状态见 §3.3，`app_exit` confirm 挂 M1-08 DoD3） |
| 决策日期 | 2026-09-17 |
| 决策载体 | 已合入：《设计文档》v1.5 → v1.6（随 ADR-007 增量续升 v1.7）；《实施计划与验收标准》v1.11 → v1.12（随升 v1.13）；2026-09-18 随 v1.7/v1.13 冻结 |
| 关联 | 设计文档 D1/D7、A4、评审 #9、附录 B/E；实施计划 M1-06、M1-08 DoD3、M3-01、M4-04、Gate 1、§7 映射 |
| 取代 | 无（对 D7 命令面的**增量登记**，不修改既有命令语义） |
| 被取代 | 无 |
| 回退条件 | 见 §6 |
| 未对齐项 | 3 处（v0.2 已补齐 2 处：无参严格解析；待对齐 1 处：`app_exit` confirm），见 §3.3 |

## 1. 背景

1. **M1-06 引入启动阻断流程**：A4 实现级同步盘检测命中后，应用拒绝进入主界面，仅提供「迁移到本地目录」「退出」两个动作（A4 降级路径 / 评审 #9）。该流程需要一个**在主界面之前**即可工作的命令面。
2. **现有 D7 命令面未覆盖启动阶段**：v1.5 的 P0 命令面（ADR-004 补齐后共 19 条：`settings_get/set` 计 1 条目；按可调用命令为 20 条，+ ADR-006 四命令后全集 24 条）全部面向会话/运行时/备份等运行期能力；`runtimes_list` 等命令在启动门未就绪时被阻断，无法承担「读取启动状态、执行数据目录迁移、选择迁移目标、退出应用」职责。
3. **错误码契约扩展**：启动阻断与迁移失败需要独立的结构化错误码，供前端在拒绝启动页做分支（渲染门界面、提示「完成迁移」等）；内部错误需要兜底码。
4. **接口即决策**：按 AGENTS §3 与实施计划 §8「变更控制」，新增命令与错误码属于设计文档未定义的接口决策，必须先走 ADR 并升版，再回流实施计划；未登记前 M1-08 DoD3 的 IPC 校验矩阵不完整，Gate 1 不应放行。
5. **现状**：M1-06 已按上述需求实现 4 个命令与 3 个错误码（代码先于文档落地）。本 ADR 为**追溯登记**：以当前实现为契约基线，明确参数校验、capabilities 口径、bindings 与验收影响；未对齐项共 **3 处**（见 §3.3）：`app_exit` 的 `confirm` 参数；`startup_get`/`startup_pick_target` 的无参严格解析（unknown 成员拒绝，随本修订 v0.2 补齐）。

## 2. 决策

| # | 决策 | 落地位置 |
|---|---|---|
| 1 | **D7 命令面新增 4 个启动迁移命令**：`startup_get`（无参数，返回启动状态快照）、`startup_migrate`（`{ target_dir: string }`，执行迁移）、`startup_pick_target`（无参数，系统目录选择器，返回 `{ target_dir: string \| null }`）、`app_exit`（`{ confirm: true }`，拒绝启动页退出）。契约明细见附录 A | 设计文档 D7；实施计划 M1-06（证据）/M1-08 DoD3（校验矩阵）/M4-04（验收清单） |
| 2 | **错误码新增 3 个**：`startup_blocked`（启动门阻断）、`migration_failed`（迁移失败：复制/校验/原子替换/写指针/状态持久化/空间）、`internal`（内部不可达错误：序列化/任务调度/选择器调用）。语义与触发点见附录 B；迁移的**参数类**错误继续复用既有码（`path_rejected`/`invalid_value`/`invalid_format`/`missing_field`/`unknown_field`），不新增重复码 | 设计文档 D7；实施计划 M1-08 DoD3 |
| 3 | **参数校验统一走 D7 强制校验框架**：serde 严格反序列化（`deny_unknown_fields`）→ 语义校验（长度上限 / 枚举白名单 / 格式）→ 路径 canonicalize 与形态拒绝（Windows 特殊路径：UNC/verbatim/ADS/8.3 短名/尾随点或空格/保留设备名）→ 启动门与迁移专属校验链（见附录 A.2）。校验失败一律结构化错误，不落库、不透传下游 | 设计文档 D7；实施计划 M1-08 DoD3 |
| 4 | **capabilities 维持最小 allowlist（空权限集），不因本 ADR 变更**。技术依据：Tauri 2 权限（ACL）管辖**插件命令**与 `core:` 能力；**应用内命令（application commands）**在本地源（`tauri.localhost` / `tauri://localhost`）下不受 ACL 门控，仅当应用显式定义 app ACL manifest 或来源为远程时才启用校验（`tauri::ipc::authority` 行为）。若评审要求显式登记应用命令权限（启用 app manifest），须另立子决策并同步更新 `build.rs`（`tauri_build::Attributes::app_manifest`）与 `tests/security_baseline.rs` 断言（M1-08 DoD4）；**未来若将任一命令下沉为插件命令，必须先登记权限**（写入本 ADR 约束） | 设计文档 D7 安全基线；实施计划 M1-08 DoD4；约束本 ADR §5 未决项 3 |
| 5 | **M1-08 DoD3 校验矩阵扩展**：4 个新命令纳入畸形参数样本集（无参数命令的未知成员拒绝、`target_dir` 相对路径/不存在/文件/Windows 特殊形态、`app_exit` 的 `confirm` 缺失与 false）。**「无参数命令的未知成员拒绝」以严格无参数解析为前提**：`startup_get`/`startup_pick_target` 已按 `backup_list` 模式对齐（`payload: Option<Value>` + 空 DTO + `parse_no_params`，v0.2 修订补齐），该矩阵项可验证；`app_exit` 的 `confirm` 仍待对齐（见 §3.3 对齐动作）。样本先在 `crates/aether-tauri/tests/m1_06_*.rs` 落地（两无参命令已完成），M1-08 矩阵并入 `tests/ipc_validation.rs` | 实施计划 M1-08 DoD3（附录 D.1） |
| 6 | **M3-01 bindings 覆盖命令全集**：tauri-specta 生成物必须包含 4 个新命令；生成与 `git diff --exit-code` 校验（T14）在 M3-01 执行。bindings 未生成前，前端契约以 `apps/desktop/src/startup.ts` 的手工类型为准（M1-06 范围内） | 实施计划 M3-01（附录 D.2） |
| 7 | **附录 E 不改动**：4 个命令均不读写数据库（迁移在存储层打开之前执行；指针文件与 `migration_state.json` 为壳层私有文件，不进线协议与数据库映射表）。验收口径：M4-04 验收清单登记命令面全集（附录 D.3） | 设计文档 附录 E（结论：无变更）；实施计划 M4-04 |

## 3. 影响

### 3.1 《设计文档》v1.5 → v1.6（拟，合入文本见附录 C）

- 头部：文档版本行与状态行升 v1.6、纳入 ADR-006；修订记录追加 v1.6 行。
- D7：命令面清单追加 4 个启动迁移命令；新增条目描述各命令参数与校验链（对齐附录 A）；错误码新增 3 个（对齐附录 B）。
- A4/评审 #9：拒绝启动页的「迁移/退出」动作以 D7 命令面为唯一通道（无覆盖开关、无旁路）。

### 3.2 《实施计划与验收标准》v1.11 → v1.12（拟，合入文本见附录 D）

- 修订记录追加 v1.12；头部计划版本/基线引用升 v1.12 / 设计文档 v1.6（含 ADR-001–006）。
- M1-08 DoD3：七命令矩阵扩为十一命令；`app_exit` 的 `confirm:true` 校验；两无参命令（`startup_get`/`startup_pick_target`）的严格无参数解析样本（v0.2 修订已在 `m1_06_*.rs` 补齐，待并入 `ipc_validation.rs`）。
- M3-01：描述补「bindings 覆盖启动迁移命令并 diff 校验」。
- M4-04：**新增 DoD4「命令面完整性」**（不修改既有 DoD1–3 文本；见附录 D.3）。
- §8 变更控制基线引用同步 v1.6。

### 3.3 实现对齐状态（评审通过后关闭）

| 项 | 状态 | 证据 |
|---|---|---|
| `startup_migrate` | **已实现**：严格解析（`parse_strict`）+ 完整校验链/续跑，与附录 A 一致 | `crates/aether-tauri/src/ipc/commands.rs`（`startup_migrate`）；`tests/m1_06_startup_ipc.rs`、`tests/m1_06_migration.rs` |
| `startup_get` | **已实现**；无参严格解析按 v0.2 修订补齐（`payload: Option<Value>` + `parse_no_params`，同 `backup_list`） | `commands.rs`（`startup_get`）；`tests/m1_06_startup_ipc.rs`（新增 `startup_get_rejects_unknown_payload_members`） |
| `startup_pick_target` | **已实现**；无参严格解析按 v0.2 修订补齐（证据：`m1_06_picker.rs` 新增用例） | `crates/aether-tauri/src/picker.rs`；`tests/m1_06_picker.rs`（含 `startup_pick_target_rejects_unknown_payload_members`） |
| 3 个错误码 | **已实现** | `crates/aether-tauri/src/ipc/error.rs`（`startup_blocked`/`migration_failed`/`internal`） |
| `app_exit` | **待对齐**：`{ confirm: true }` 缺失/false 结构化拒绝 + 严格解析（当前为无参数，额外成员静默忽略） | `commands.rs`（`app_exit`）；对齐动作见下 |
| capabilities | **无需变更**（决策 4） | `tests/security_baseline.rs::capabilities_are_minimal_allowlist`（空权限集）持续通过；真实 E2E 在 `withGlobalTauri:false` 下调用应用命令成功 |
| bindings | **待 M3-01**（T14 未落地） | 计划 M3-01；`apps/desktop/src/startup.ts` 为当前前端契约 |

**对齐动作（v0.2 已执行第 1 项；剩余由 M1-08 DoD3 承接）**：

1. ✅（v0.2 修订）`startup_get` / `startup_pick_target`：命令签名改 `payload: Option<Value>`，以空 DTO（`#[serde(deny_unknown_fields)]`）经 `parse_no_params` 严格反序列化——与 `backup_list` 的实现完全一致（`commands.rs` 的 `backup_list` 分支：`payload: Option<Value>` + `parse_no_params(payload.unwrap_or(Value::Null))`）；任何成员返回 `unknown_field`。新增用例：`startup_get_rejects_unknown_payload_members`、`startup_pick_target_rejects_unknown_payload_members`。
2. ⏳ `app_exit`（挂 M1-08 DoD3）：新增 DTO `{ confirm: bool }`（`deny_unknown_fields`），`confirm != true` 返回 `invalid_value`；前端 `exitApp()` 改为 `invoke("app_exit", { payload: { confirm: true } })`。
3. ⏳ `tests/ipc_validation.rs` 校验矩阵并入其余畸形样本（4 组：两无参命令的额外成员样本随本修订已有；`confirm` 缺失与 false 待并入）。
4. 对齐完成后，§2 决策 5 与附录 D.1 的矩阵项方可验证；本表状态更新为「已实现」并关闭。

### 3.4 不影响的

- 不修改既有 19 条命令的契约与校验；不改事件类型（附录 B 清单不变）；不引入新依赖；不改数据目录/迁移的既有语义（复制→校验→原子替换→指针锁定；源保留；A4 默认拒绝）。
- 不改变 P0–P6 阶段划分与 Gate 1–4 条件结构（仅细化 M1-08/M3-01/M4-04 的验收内容）。

## 4. 版本（拟）

| 文档 | 修订前 | 修订后 |
|---|---|---|
| 《设计文档》 | v1.5 | **v1.6** |
| 《实施计划与验收标准》 | v1.11 | **v1.12** |
| 《需求文档》 | v0.5 | 不变（无需求项变更；启动阻断流程已在 A4/评审 #9 范围内） |

## 5. 后续（未决项）

1. **实现对齐（3 处，v0.2 已补齐 2 处）**：① `app_exit` 的 `confirm:true`（待对齐）；②③ `startup_get`/`startup_pick_target` 的严格无参数解析（`payload: Option<Value>` + 空 DTO（`deny_unknown_fields`）+ `parse_no_params`，与 `backup_list` 一致）——已按 v0.2 修订补齐（含新增用例，见 §3.3）。剩余项由 M1-08 DoD3 承接（`app_exit` DTO/命令/前端/样本 + 矩阵并入），完成后关闭 §3.3 待对齐项。
2. **空间护栏原生探针**：迁移命令的空间校验当前由可注入探针提供（原生实现返回「未知 → 不阻断」）；真实磁盘探针按 ADR-003 决策 19 在 **M3-04**（备份/导出外部路径）统一接线，届时迁移复用同一探针。登记于 M1-06 证据与本文档，不视为本 ADR 的遗漏。
3. **capabilities 显式登记诉求**：若评审要求启用应用 ACL manifest 并显式登记命令，须另立子决策（含 `build.rs`（`tauri_build::Attributes::app_manifest`）与 `tests/security_baseline.rs` 断言变更），不得静默修改。
4. **参数命名确认**：`startup_migrate` 采用 `target_dir`（与 `startup_pick_target` 返回字段、前端类型、E2E 一致）；任务草纲曾写作 `target_path`，以本 ADR 的实现契约为准。
5. **幂等续跑**：`startup_get` 快照的 `pending_migration` 字段与 `migration_state.json` 为 M1-06 残余风险修复（指针写入失败窗口）的登记内容，随本 ADR 一并纳入 D7 契约；不改数据库。

## 6. 回退条件

1. **`confirm` 对齐与既有交互冲突**（如前端无法携带参数）→ 保留无参数 `app_exit` 并在本 ADR 记录偏差（须评审确认），不得静默删改决策。
2. **capabilities 结论被评审否定**（要求显式 ACL）→ 按未决项 3 另立子决策，先改设计文档再回流计划。
3. **新命令参数校验与 D7 强制框架冲突** → 以 D7 框架为准调整命令定义并记录证据。
4. **bindings 生成器不支持无参数命令**（`startup_get`/`startup_pick_target`）→ 在 M3-01 记录适配方式（空对象参数或生成器配置），不得放宽校验。

## 7. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-09-17 | （留空待签） | 有条件通过；B1/B2 已修订，待复评 | 决策 4 口径采纳；`app_exit` confirm 挂 M1-08 DoD3；未决项 2/4 确认 |
| 2026-09-18 | AI 评审（opencode/GLM） | 通过（复评） | 条件项核验闭合：B1/B2 修订（严格无参解析用例 `m1_06_startup_ipc.rs`/`m1_06_picker.rs`、M4-04 DoD3 对齐、命令计数口径）；决策 4 空权限集断言（`security_baseline.rs::capabilities_are_minimal_allowlist`）维持；`app_exit` confirm 与未决项 2/4 维持原挂载（M1-08 DoD3 / M3-04 / 附录 A.2 契约）；随 v1.7/v1.13 冻结 |

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-09-17 | 创建：登记 M1-06 新增 4 命令与 3 错误码；拟设计文档 v1.6 / 实施计划 v1.12 修订；给出合入 diff 与实现对齐状态 | （文档维护） |
| v0.2 | 2026-09-17 | 评审修订：D.3 合入文本对齐 M4-04 实际 DoD3；startup_get/pick_target 补无参严格解析（B2）；命令计数口径、决策 4 改动点、快照可缺省字段注记 | （文档维护） |

---

## 附录 A：命令契约明细（目标契约；对齐状态见 §3.3）

### A.1 命令一览

| 命令 | 参数（严格） | 返回 | 可用阶段 | 说明 |
|---|---|---|---|---|
| `startup_get` | 无（`null`/缺省；**严格无参：任何成员拒绝**） | `StartupSnapshot`（下） | 启动门任意阶段（含阻断态） | 拒绝启动页唯一查询入口 |
| `startup_migrate` | `{ "target_dir": string }`（已严格） | `StartupSnapshot`（成功时 `phase=ready`、`data_dir_source=migrated`、`migration` 明细） | 仅 `blocked_sync_dir` | 迁移执行；幂等续跑 |
| `startup_pick_target` | 无（`null`/缺省；**严格无参：任何成员拒绝**） | `{ "target_dir": string \| null }`（null=取消） | 启动门任意阶段 | 系统目录选择器（Rust 侧，不经 WebView 权限面） |
| `app_exit` | `{ "confirm": true }`（决策 1；**当前无参数，待对齐**，见 §3.3） | `{ "exiting": true }` | 启动门任意阶段 | 请求退出应用 |

> **「严格无参数解析」口径**：命令签名声明 `payload: Option<Value>`，以空对象经 `parse_no_params` 严格反序列化（`deny_unknown_fields`），任何成员返回 `unknown_field`——与 `backup_list` 完全一致。`startup_get`/`startup_pick_target` 已按此实现（v0.2 修订补齐；用例见 §3.3）；`startup_migrate` 走 `parse_strict`（严格）；`app_exit` 的 `confirm` 与严格解析仍待对齐（见 §3.3）。

`StartupSnapshot`（`startup_get`/`startup_migrate` 返回）：

```json
{
  "phase": "ready | blocked_sync_dir | blocked_error",
  "data_dir": "C:\\Users\\me\\AppData\\Roaming\\Aether",
  "data_dir_source": "env_override | pointer | default | migrated",
  "detection": {
    "platform": "windows | macos | other",
    "candidate": "<原始候选路径>",
    "resolved": "<解析后路径>",
    "verdict": "allow | reject",
    "checks": [{ "id": "…", "label": "…", "hit": true, "detail": "…", "precision": "exact | path_prefix" }],
    "reasons": ["…"],
    "note": "（macOS 降级精度限制说明，仅 macOS 且为非空时出现）"
  },
  "message": "（blocked_error 时的原因，可缺省）",
  "migration": {                                   // （迁移成功后返回，可缺省）
    "source": "…", "target": "…",
    "entries": [{ "relative": "aether.db", "sha256": "…", "bytes": 123 }],
    "total_bytes": 456,
    "manifest_digest": "…"
  },
  "pending_migration": {                           // （存在可续跑迁移时返回，可缺省）
    "migration_id": "01J…（ULID）", "target": "…",
    "phase": "copying | verified", "started_at": 1758092000000
  }
}
```

### A.2 `startup_migrate` 参数校验链（按执行顺序）

| # | 校验 | 失败错误码 |
|---|---|---|
| 1 | DTO 严格反序列化（`deny_unknown_fields`、必填、类型、非空） | `unknown_field` / `missing_field` / `invalid_type` / `invalid_format` |
| 2 | 路径形态：绝对、≤4096 字符、无 NUL、Windows 特殊形态拒绝（UNC/`\\?\`/`\\.\`/ADS/8.3 短名/尾随点或空格/保留设备名） | `path_rejected` |
| 3 | canonicalize（解析软链接/Junction；剥离 verbatim 前缀）；必须存在且为目录 | `path_rejected` |
| 4 | 指针位置已知且可写（创建/删除临时探针文件）；启动门为 `blocked_sync_dir`；无并发迁移 | `migration_failed` / `invalid_value` |
| 5 | 源/目标关系：不同一、互不为祖先 | `path_rejected` |
| 6 | 目标 A4 复核（实现级同步盘检测；macOS 降级精度在 `note` 明示） | `path_rejected` |
| 7 | 目标可写（创建/删除探针文件） | `path_rejected` |
| 8 | 空间护栏：探针提供可用空间时要求 ≥ 源数据目录文件总量 ×1.2（原生探针当前返回未知 → 不阻断；M3-04 接线） | `migration_failed` |
| 9 | 目标为空 → 正常迁移；或存在本应用迁移状态（`migration_state.json` 源/目标一致）且仅含迁移残留（暂存目录 + 与源同名条目）→ 允许续跑；其余非空情况（含未知条目） | `path_rejected` |

迁移成功副作用：源保留；目标按「复制 → sha256 校验 → 原子替换（主库最后移动）」写入；数据目录指针原子替换（锁定新目录）；迁移状态文件按 `copying → verified → pointer_written → done` 记录；指针写入失败时状态保持 `verified`，`startup_get` 暴露 `pending_migration`，UI 提供「完成迁移」/「另选目录」。

### A.3 `startup_pick_target`

- 实现为 `DirectoryPicker` 抽象：生产 `TauriDialogPicker`（`tauri-plugin-dialog`，阻塞调用置于 `spawn_blocking`）；测试/E2E 以 `FixedDirectoryPicker` 注入（debug 探针环境变量 `AETHER_E2E_PICK_DIR` / `AETHER_E2E_PICK_CANCEL`）。
- 取消返回 `{ "target_dir": null }`（不是错误）；选择器不可用/失败返回 `internal`。
- 不新增 WebView capability 权限面（对话框在 Rust 侧调用）。

## 附录 B：错误码登记表

| code | 语义 | 触发点 | 前端行为 |
|---|---|---|---|
| `startup_blocked` | 启动门未就绪，业务命令被阻断 | 所有业务命令经 `ensure_ready`（`phase != ready`） | 渲染拒绝启动门（仅迁移/退出）；`main UI` 不可达 |
| `migration_failed` | 迁移失败（复制/校验/原子替换/写指针/状态持久化/空间/并发） | `startup_migrate` 迁移链与迁移后的目标复核 | 展示 `message`；指针写入失败时提示「完成迁移」并刷新快照呈现 `pending_migration` |
| `internal` | 内部错误（序列化失败、任务调度失败、目录选择器调用失败） | 命令层不可达路径 | 展示 `message`（不参与分支） |

说明：错误码为稳定契约（`snake_case` 序列化），新增取值须走 ADR；迁移的**参数类**失败沿用既有码，避免语义重复。

## 附录 C：拟议《设计文档》v1.6 合入文本

**C.1 头部（第 7 行、第 10 行）**

```diff
-| 文档版本 | **v1.5（冻结）**；冻结后任何修改须走 ADR 并升版（v1.x）；本版含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005 |
+| 文档版本 | **v1.6（冻结）**；冻结后任何修改须走 ADR 并升版（v1.x）；本版含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005、ADR-006 |
-| 状态 | v1.5 已冻结（实现基线，2026-09-16 评审批准；含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005） |
+| 状态 | v1.6 已冻结（实现基线，2026-09-17 评审批准；含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005、ADR-006） |
```

**C.2 修订记录追加一行**

```diff
 | v1.5 | ADR-005：`client_msg_id` 幂等持久化（`messages.client_msg_id` + `UNIQUE(session_id, client_msg_id)`，核心重启后重放不重复）；会话恢复两模式（Mode R 原生恢复 / Mode N 新会话重发 + UI 明示）并在 M1-11 spike 验证；附录 E 增补映射（详 `docs/adr/ADR-005-client-msg-id-persistence-and-session-recovery-spike.md`） |
+| v1.6 | ADR-006：D7 命令面新增 4 个启动迁移命令（`startup_get`/`startup_migrate`/`startup_pick_target`/`app_exit`）与 3 个错误码（`startup_blocked`/`migration_failed`/`internal`）；登记迁移命令参数校验链与幂等续跑口径（详 `docs/adr/ADR-006-startup-commands-and-error-codes.md`） |
```

**C.3 D7 命令面（第 435 行后追加）**

```diff
   - 命令面（P0 全集，ADR-004）：`runtimes_list`、`runtime_retry`、`runtime_enable`、`session_list`、`session_create`、`session_send`、`session_interrupt`、`session_dispose`、`messages_page`、`run_retry`、`permissions_pending`、`permission_resolve`、`settings_get/set`、`workspace_set`、`backup_create`、`backup_list`、`backup_restore`、`export_diagnostics`、`app_restart`；
+  - 命令面（P0 全集，ADR-006；启动迁移 4 命令）：
+    - `startup_get`：无参数（严格解析：任何成员拒绝）；返回启动状态快照（`phase`/`data_dir`/`data_dir_source`/`detection`/`message`/`migration`/`pending_migration`）；阻断态仍可达；
+    - `startup_migrate`：`{ target_dir: string }`；校验链见 ADR-006 附录 A.2（canonicalize + 存在目录 + 可写 + 空间护栏 + 同步盘拒绝 + 非源子目录 + 目标为空或含本应用迁移标记的续跑）；成功返回 `StartupSnapshot`；失败返回结构化错误；
+    - `startup_pick_target`：无参数（严格解析：任何成员拒绝）；Rust 侧系统目录选择器，返回 `{ target_dir: string | null }`（null=取消）；
+    - `app_exit`：`{ confirm: true }`；拒绝启动页退出应用（与 `app_restart` 的 confirm 约定一致）；
+    - 错误码（ADR-006）：`startup_blocked`（启动门阻断）、`migration_failed`（迁移失败）、`internal`（内部错误）；参数类失败沿用既有码。
```

## 附录 D：拟议《实施计划与验收标准》v1.12 合入文本

**D.1 M1-08 DoD3（第 188 行）**

```diff
-  3. IPC 校验框架：畸形参数样本集（超长/未知字段/非法枚举）全部返回结构化错误且不落库（单测矩阵）；**新增命令 `backup_list`/`backup_restore`/`app_restart`/`run_retry`/`runtime_retry`/`runtime_enable`/`workspace_set` 全部纳入校验矩阵**（含 `run_id` ULID、终态、白名单、`confirm:true`、路径 canonicalize 与同步盘拒绝）；
+  3. IPC 校验框架：畸形参数样本集（超长/未知字段/非法枚举）全部返回结构化错误且不落库（单测矩阵）；**ADR-004 七命令（`backup_list`/`backup_restore`/`app_restart`/`run_retry`/`runtime_retry`/`runtime_enable`/`workspace_set`）与 ADR-006 四命令（`startup_get`/`startup_migrate`/`startup_pick_target`/`app_exit`）全部纳入校验矩阵**（含 `run_id` ULID、终态、白名单、`confirm:true`、无参数命令的未知成员拒绝、`target_dir` 路径 canonicalize/存在目录/Windows 特殊形态、同步盘拒绝）；
```

**D.2 M3-01 描述（第 378 行）**

```diff
@@ M3-01（第 378 行） @@
-- 描述：Vite+React+Zustand+TQ、tauri-specta 生成、EventStore（seq 去重/补读/16ms 批处理）。
+- 描述：Vite+React+Zustand+TQ、tauri-specta 生成（**覆盖 D7 命令全集，含 ADR-006 的 4 个启动迁移命令**，T14 diff 校验）、EventStore（seq 去重/补读/16ms 批处理）。
```

**D.3 M4-04 新增 DoD4（在现有 DoD1–3 之后追加；不修改既有条目）**

现有文本（`实施计划与验收标准.md:483-485`，逐字）：

```text
  1. T1–T15 全通过（含 T1 P50<500ms/P95<2s；T2 P95<150ms；T3 10 个并发会话×每会话 1 run 控制事件 0 丢失；mock-only 路径下标注「Mock-only beta」）；
  2. 覆盖率：Rust 与 TS 行覆盖 >70%；
  3. 验收报告产出：每项附证据链接与失败重跑记录。
```

```diff
   3. 验收报告产出：每项附证据链接与失败重跑记录。
+  4. 命令面完整性：D7 全集（ADR-004 七命令 + ADR-006 四命令）逐条登记验收清单并验证可调用（含拒绝启动页「迁移/退出」路径与 `startup_blocked`/`migration_failed` 错误码分支）。
```

**D.4 修订记录追加一行（第 30 行 `v1.11` 行之后）**

```diff
 | v1.11 | 评审 C1/C2 修订（ADR-005）：M1-03 补 `messages.client_msg_id` + UNIQUE（DoD3/DoD5）；M2-01 DoD3 增「重启核心后重放仍不重复」断言；M1-11 DoD2 增会话恢复验证（`native_id` 恢复并续聊）、DoD3 增 Mode R/N 结论；Gate 1 结论补恢复能力；M2-02/M3-06 重放语义按 Mode R/N；§6 #15、§7 映射、§8 基线与设计文档 v1.5 / 需求文档 v0.5 同步 |
+| v1.12 | 同步设计文档 v1.6（ADR-006）：D7 命令面补 4 个启动迁移命令与 3 个错误码；M1-08 DoD3 校验矩阵扩为十一命令（含 `app_exit` 的 `confirm:true` 与两无参命令的严格解析样本）；M3-01 bindings 覆盖启动迁移命令；M4-04 新增 DoD4 命令面完整性；§8 基线与设计文档 v1.6 同步 |
```

**D.5 头部与 §8 基线引用**

```diff
-| 计划版本 | v1.11 |
-| 状态 | v1.11 已冻结（2026-09-16 评审批准）；任务进度可更新，结构与门禁变更走 ADR |
-| 基线 | 设计文档 v1.5（冻结，含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005） |
+| 计划版本 | v1.12 |
+| 状态 | v1.12 已冻结（2026-09-17 评审批准）；任务进度可更新，结构与门禁变更走 ADR |
+| 基线 | 设计文档 v1.6（冻结，含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005、ADR-006） |
```

```diff
@@ §8 计划外声明与衔接（第 598 行） @@
-- **变更控制**：设计文档已冻结 v1.5（含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005）；本计划任何任务若需偏离设计（新增机制/改阈值语义），必须先走设计文档 ADR 并升版（v1.x），再回流本计划；常量级调参（如阈值数值）不视为决策变更，但须记录在任务证据中。
+- **变更控制**：设计文档已冻结 v1.6（含 ADR-001、ADR-002、ADR-003、ADR-004、ADR-005、ADR-006）；本计划任何任务若需偏离设计（新增机制/改阈值语义），必须先走设计文档 ADR 并升版（v1.x），再回流本计划；常量级调参（如阈值数值）不视为决策变更，但须记录在任务证据中。
```

## 附录 E：实现与证据索引

| 内容 | 路径 |
|---|---|
| 命令实现与注册（debug/release 双 handler） | `crates/aether-tauri/src/ipc/commands.rs` |
| 错误码与线上形态 | `crates/aether-tauri/src/ipc/error.rs` |
| 启动门/迁移/续跑状态机 | `crates/aether-tauri/src/startup/{mod,migrate,pointer,state}.rs` |
| 目录选择器抽象 | `crates/aether-tauri/src/picker.rs` |
| 命令层测试（含 `startup_get` 快照、迁移、续跑、选择器、无参严格解析） | `crates/aether-tauri/tests/m1_06_{startup_ipc,picker,migration}.rs` |
| 启动阻断/迁移/续跑/单实例 E2E（真实 WebView） | `scripts/test/m1-06/e2e-startup-guard.mjs` |
| 验证入口（样本 + E2E + 静态断言） | `scripts/test/m1-06/verify-m1-06.mjs` |
| 真实系统选择器冒烟记录 | `docs/M1-06-目录选择器与迁移主路径验证.md` §5 |
| CI 运行（含 M1-06 双 job 全绿） | run [35203116016](https://github.com/Scott-PyMu/Aether/actions/runs/35203116016)、[35181557079](https://github.com/Scott-PyMu/Aether/actions/runs/35181557079) |
