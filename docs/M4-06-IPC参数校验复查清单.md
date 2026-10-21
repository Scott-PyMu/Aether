# M4-06：IPC 参数校验全量复查清单（63 命令）

> 依据：设计 D7（每个 IPC 命令显式校验：serde 严格反序列化 + 路径 canonicalize +
> 枚举白名单 + 长度上限；失败返回结构化错误码，不落库、不透传下游）、M1-08 DoD3、
> 各任务 DTO 登记（ADR-004/006/007/010/018/019/024）。
> 机器核验：`node scripts/test/m4-04/verify-m4-04.mjs`（命令面 63 条三源一致 +
> `ipc_validation`/`health_command`/`m1_06_startup_ipc`/`m1_06_picker` 矩阵回归）；
> `node scripts/test/m4-06/verify-m4-06.mjs`（本清单 ↔ bindings 命令面逐条比对，
> 缺失即失败）；本清单为逐条人工复查记录（签核栏）。

| # | 命令 | 校验要点 | 证据（测试/入口） | 结论 |
|---|---|---|---|---|
| 1 | `runtimes_list` | 无参数；监督器未接线 → `core_not_ready` | `ipc_validation.rs`（畸形样本）；`health_command.rs` | ✅ |
| 2 | `session_list` | 请求形状（可选过滤）；序列化 `SessionSummary` | `ipc_validation.rs`；`m3_02_session_backend.rs` | ✅ |
| 3 | `session_create` | `runtime_id` 白名单；标题 ≤256；`thinking_depth` 0–4；`model` 透传 | `ipc_validation.rs`；`m3_10_thinking.rs` | ✅ |
| 4 | `session_send` | `session_id` 格式；文本 ≤1MiB；`client_msg_id` ULID 幂等；`thinking_depth` 覆盖 | `ipc_validation.rs`；`m2_01_lifecycle.rs` | ✅ |
| 5 | `session_interrupt` | `session_id` 存在性；无活动 run 的结构化结果 | `ipc_validation.rs`；`m2_01_lifecycle.rs` | ✅ |
| 6 | `session_dispose` | `session_id` 存在性；幂等 | `ipc_validation.rs`；`m3_02_adapter_executor.rs` | ✅ |
| 7 | `messages_page` | 分页 ≤500；`last_seq` 缺口 >10k → `readback_gap_too_large` | `ipc_validation.rs`；`m3_02_session_backend.rs` | ✅ |
| 8 | `permissions_pending` | 可选 `session_id` 过滤；`core_not_ready` 防伪造空队列 | `ipc_validation.rs`；`m3_03_permission_center.rs` | ✅ |
| 9 | `permission_resolve` | `request_id`/decision 枚举；审计与落盘 | `ipc_validation.rs`；`m2_10_permission_loop.rs` | ✅ |
| 10 | `settings_get` | 键白名单 | `ipc_validation.rs` | ✅ |
| 11 | `settings_set` | 键白名单 + 值类型/上限 | `ipc_validation.rs` | ✅ |
| 12 | `backup_create` | `target_dir` canonicalize + 可写 + 空间护栏；缺省内部目录；可选 `passphrase`（M5-03/ADR-018 A-5：非空/≤1024 字符/无控制字符；仅传输） | `ipc_validation.rs`；`m3_04_backup_ipc.rs`；`m5_03_encrypted_backup.rs` | ✅ |
| 13 | `backup_list` | 无参数（任何成员拒绝） | `ipc_validation.rs` | ✅ |
| 14 | `backup_restore` | 来源枚举或外部路径（存在 + `.db`/`.aebk`；M5-03/ADR-020）；内部 id 白名单；可选 `passphrase`（同 `backup_create` 校验） | `ipc_validation.rs`；`m3_04_backup_ipc.rs`；`m5_03_encrypted_backup.rs` | ✅ |
| 15 | `app_restart` | `confirm:true` 显式确认 | `ipc_validation.rs`；`m3-06/e2e-degraded-recovery.mjs` | ✅ |
| 16 | `run_retry` | `run_id` ULID + 仅终态 | `ipc_validation.rs`；`m3_06_run_retry.rs` | ✅ |
| 17 | `runtime_retry` | 白名单 + 仅 `disabled+start_failed` | `ipc_validation.rs`；`m1_10_supervisor.rs` | ✅ |
| 18 | `runtime_enable` | 白名单 + 仅 `disabled`；`untrusted/version_mismatch` 需先修复 | `ipc_validation.rs`；`m1_10_supervisor.rs` | ✅ |
| 19 | `workspace_set` | `workspace_id` 存在性或 `root_path` canonicalize + 同步盘拒绝 | `ipc_validation.rs`；`m3_08_memory.rs` | ✅ |
| 20 | `ref_pick` | `{ kind }` 严格解析；取消 `{ path: null }` | `m1_06_picker.rs`；`m3_09_ref_pick.rs` | ✅ |
| 21 | `artifacts_list` | `session_id` 格式；只读 | `ipc_validation.rs`；`m3_09_artifacts.rs` | ✅ |
| 22 | `artifact_add` | canonicalize + 可访问性；失败 `artifact_path_rejected`；幂等 | `ipc_validation.rs`；`m3_09_artifacts.rs` | ✅ |
| 23 | `artifact_remove` | `session_id`+路径；不存在幂等 `{removed:false}` | `ipc_validation.rs`；`m3_09_artifacts.rs` | ✅ |
| 24 | `providers_list` | 无参数；响应含 `api_key_ref` 不含明文 | `ipc_validation.rs`；`m3_11_providers.rs` | ✅ |
| 25 | `provider_create` | type 枚举；`base_url` 格式；custom 必填；`api_key` ≤8192 三态 | `ipc_validation.rs`；`m3_11_providers.rs` | ✅ |
| 26 | `provider_update` | id 存在性；type 不可改；密钥三态 | `ipc_validation.rs`；`m3_11_providers.rs` | ✅ |
| 27 | `provider_delete` | 内置 → `builtin_provider_undeletable` | `ipc_validation.rs`；`m3_11_providers.rs` | ✅ |
| 28 | `provider_toggle` | 内置可停用；id 存在性 | `ipc_validation.rs`；`m3_11_providers.rs` | ✅ |
| 29 | `provider_model_add` | `model_id` 唯一（`UNIQUE(provider_id,model_id)`）/重复结构化错误 | `ipc_validation.rs`；`m3_11_providers.rs` | ✅ |
| 30 | `provider_model_toggle` | 模型存在性 → `provider_model_not_found` | `ipc_validation.rs`；`m3_11_providers.rs` | ✅ |
| 31 | `export_diagnostics` | `target_dir` canonicalize + 可写；脱敏 0 命中 | `ipc_validation.rs`；`m3_05_diagnostics.rs` | ✅ |
| 32 | `health` | 无参数严格解析（`null`/缺省/空对象合法；未知成员拒绝）；两态 `normal/persist_degraded` | `health_command.rs`；`m2-07` 两态 E2E | ✅ |
| 33 | `startup_get` | 无参数（任何成员拒绝）；阻断态可达 | `m1_06_startup_ipc.rs`；`e2e-startup-guard.mjs` | ✅ |
| 34 | `startup_pick_target` | 无参数；取消 → `{ target_dir: null }` | `m1_06_picker.rs`；`e2e-startup-guard.mjs` | ✅ |
| 35 | `startup_migrate` | `target_dir` canonicalize + 存在目录 + 可写 + 空间护栏 + 同步盘拒绝 + 非源子目录 | `m1_06_migration.rs`；`e2e-startup-guard.mjs` | ✅ |
| 36 | `app_exit` | `{ confirm: true }`；拒绝启动页退出 | `ipc_validation.rs`；`e2e-startup-guard.mjs` | ✅ |
| 37 | `session_export` | `target_dir` canonicalize + 可写 + 空间护栏；JSONL 字段与设计附录 B/E 逐项映射（D13/ADR-021） | `ipc_validation.rs`；`m5_05_export.rs` | ✅ |
| 38 | `archive_export` | 同 `session_export` 护栏；全库归档产物可被 M5-04 链路同格式消费 | `ipc_validation.rs`；`m5_05_export.rs` | ✅ |
| 39 | `audit_query` | 过滤枚举白名单 + 分页上限；响应审计条目字段与表一致 | `ipc_validation.rs`；`m6_03_audit.rs` | ✅ |
| 40 | `audit_export` | `target_dir` canonicalize + 可写；导出内容脱敏扫描 0 命中 | `ipc_validation.rs`；`m6_03_audit.rs` | ✅ |
| 41 | `session_permission_mode_set` | 模式枚举白名单（read_only/workspace_write/danger_full_access）；read_only 档记忆写不变量（M6-01/ADR-014 附录 B） | `ipc_validation.rs`；`m6_01_permission_modes.rs` | ✅ |
| 42 | `rules_list` | 无参数（任何成员拒绝）；响应规则集按创建时间升序（M6-01/ADR-018） | `ipc_validation.rs`；`m6_01_rules.rs` | ✅ |
| 43 | `rule_add` | resource/target_kind 组合白名单；target 形态（ADR-019 兼容矩阵：canonical 幂等 / 不可规范化拒绝） | `ipc_validation.rs`；`m6_01_rules.rs`；`m6_02_rule_safety.rs` | ✅ |
| 44 | `rule_update` | `rule_id` 存在性（`rule_not_found`）；同 `rule_add` 形态校验 | `ipc_validation.rs`；`m6_01_rules.rs` | ✅ |
| 45 | `rule_delete` | `rule_id` 存在性（`rule_not_found`）；删除生效后清单回落 | `ipc_validation.rs`；`m6_01_rules.rs` | ✅ |
| 46 | `rule_toggle` | `rule_id` 存在性 + `enabled` 布尔严格解析 | `ipc_validation.rs`；`m6_01_rules.rs` | ✅ |
| 47 | `runtime_disable` | 运行时白名单；禁用态拒绝会话创建/运行（结构化错误，M7-01） | `ipc_validation.rs`；`m7_01_runtime_control.rs` | ✅ |
| 48 | `session_pause` | `session_id` 格式/存在性；能力门未声明 `pause_resume` → `pause_unsupported`；非可暂停态拒绝（M7-03/ADR-017） | `ipc_validation.rs`；`m7_03_pause.rs` | ✅ |
| 49 | `session_resume` | 能力门实时复核；仅 `paused` 可恢复；`persist_degraded` 拒恢复（M7-03/ADR-017） | `ipc_validation.rs`；`m7_03_pause.rs` | ✅ |
| 50 | `provider_test` | DTO 严格解析 + 畸形矩阵；无密钥不发请求（`invalid_value` field=api_key_ref）；五路径结构化结果含状态码（M7-06/ADR-024） | `ipc_validation.rs`；`m7_06_provider_connectivity.rs` | ✅ |
| 51 | `provider_models_fetch` | 同 `provider_test` 校验；模型归一化（trim/去重/空串/超长跳过）与幂等 upsert（M7-06/ADR-024） | `ipc_validation.rs`；`m7_06_provider_connectivity.rs` | ✅ |
| 52 | `workspaces_list` | 无参数；响应工作区清单 + 未分配合成组计数（M7-08/ADR-026） | `ipc_validation.rs`；`m7_08_workspaces.rs` | ✅ |
| 53 | `workspace_create` | `name` ≤64 + 非空；`root_path` canonicalize/存在目录/同步盘拒绝/同根唯一（M7-08） | `ipc_validation.rs`；`m7_08_workspaces.rs` | ✅ |
| 54 | `workspace_update` | `workspace_id` ULID + 名称校验；`root_path` 不可变（未知字段严格拒绝） | `ipc_validation.rs`；`m7_08_workspaces.rs` | ✅ |
| 55 | `workspace_delete` | `workspace_id` 存在性；存在会话引用 → `workspace_in_use`（不级联/不改派） | `ipc_validation.rs`；`m7_08_workspaces.rs` | ✅ |
| 56 | `session_workspace_assign` | 双 ULID；在途/等待审批 → `session_busy`；权限基准不重算（R15） | `ipc_validation.rs`；`m7_08_workspaces.rs` | ✅ |
| 57 | `session_search` | query 非空/≤256；limit ≤100；offset ≥0；FTS 短语转义（M7-07/ADR-025） | `ipc_validation.rs`；`m7_07_session_manage.rs` | ✅ |
| 58 | `session_rename` | `session_id` ULID；标题非空/≤256/控制字符拒绝；单写队列（M7-07） | `ipc_validation.rs`；`m7_07_session_manage.rs` | ✅ |
| 59 | `session_delete` | `session_id` ULID；在途/待审批 → `session_busy`；硬删级联（events 不级联，R7） | `ipc_validation.rs`；`m7_07_session_manage.rs` | ✅ |
| 60 | `presets_list` | 无参数；响应按创建时间升序（M7-09/ADR-026） | `ipc_validation.rs`；`m7_09_presets.rs` | ✅ |
| 61 | `preset_add` | 名称 ≤64；提示词 ≤32KiB；非空；审计（M7-09） | `ipc_validation.rs`；`m7_09_presets.rs` | ✅ |
| 62 | `preset_update` | `preset_id` 存在性；至少一项更新；内置可编辑（M7-09） | `ipc_validation.rs`；`m7_09_presets.rs` | ✅ |
| 63 | `preset_delete` | `preset_id` 存在性；内置 → `builtin_preset_undeletable`（M7-09） | `ipc_validation.rs`；`m7_09_presets.rs` | ✅ |

**错误码全集（32）**：`invalid_json`/`unknown_field`/`missing_field`/`invalid_type`/
`invalid_value`/`invalid_enum`/`too_large`/`out_of_range`/`invalid_format`/`path_rejected`/
`startup_blocked`/`migration_failed`/`internal`/`core_not_ready`/`not_implemented`/
`readback_gap_too_large`/`artifact_path_rejected`/`builtin_provider_undeletable`/
`provider_not_found`/`provider_model_not_found`/`rule_not_found`（M6-01）/`runtime_busy`、
`no_candidate_runtime`（M7-01）/`pause_unsupported`（M7-03）/`provider_unreachable`、
`provider_auth_failed`、`provider_error`（M7-06）/`session_not_found`（M7-07）/
`workspace_not_found`、`workspace_in_use`（M7-08）/`preset_not_found`、
`builtin_preset_undeletable`（M7-09）——逐条测试引用覆盖由
`m4-04 DoD4-错误码` 机械核验（0 未覆盖）。

**警告码子表**：`thinking_depth_unsupported`（同步判定路径响应警告 + 延迟判定路径
回显生效值，`m3_10_thinking.rs`）。

**签核**

| 角色 | 结论 | 日期 |
|---|---|---|
| 自动化核验（m4-04 DoD4 命令面矩阵） | 通过（36/36 命令可调用、20/20 错误码有测试分支） | 2026-10-08 |
| 自动化核验（M7-06 后全量复核） | 通过（51/51 命令可调用、27/27 错误码有测试分支；新增 15 条登记见 #37–#51） | 2026-10-19 |
| 人工复查 | 待用户签核（本清单供评审逐条核对） | — |
