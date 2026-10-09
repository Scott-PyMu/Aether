# M4-06：IPC 参数校验全量复查清单（36 命令）

> 依据：设计 D7（每个 IPC 命令显式校验：serde 严格反序列化 + 路径 canonicalize +
> 枚举白名单 + 长度上限；失败返回结构化错误码，不落库、不透传下游）、M1-08 DoD3、
> 各任务 DTO 登记（ADR-004/006/007/010）。
> 机器核验：`node scripts/test/m4-04/verify-m4-04.mjs`（命令面 36 条三源一致 +
> `ipc_validation`/`health_command`/`m1_06_startup_ipc`/`m1_06_picker` 矩阵回归）；
> 本清单为逐条人工复查记录（签核栏）。

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
| 12 | `backup_create` | `target_dir` canonicalize + 可写 + 空间护栏；缺省内部目录 | `ipc_validation.rs`；`m3_04_backup_ipc.rs` | ✅ |
| 13 | `backup_list` | 无参数（任何成员拒绝） | `ipc_validation.rs` | ✅ |
| 14 | `backup_restore` | 来源枚举或外部路径（存在 + `.db`）；内部 id 白名单 | `ipc_validation.rs`；`m3_04_backup_ipc.rs` | ✅ |
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

**错误码全集（20）**：`invalid_json`/`unknown_field`/`missing_field`/`invalid_type`/
`invalid_value`/`invalid_enum`/`too_large`/`out_of_range`/`invalid_format`/`path_rejected`/
`startup_blocked`/`migration_failed`/`internal`/`core_not_ready`/`not_implemented`/
`readback_gap_too_large`/`artifact_path_rejected`/`builtin_provider_undeletable`/
`provider_not_found`/`provider_model_not_found`——逐条测试引用覆盖由
`m4-04 DoD4-错误码` 机械核验（0 未覆盖）。

**警告码子表**：`thinking_depth_unsupported`（同步判定路径响应警告 + 延迟判定路径
回显生效值，`m3_10_thinking.rs`）。

**签核**

| 角色 | 结论 | 日期 |
|---|---|---|
| 自动化核验（m4-04 DoD4 命令面矩阵） | 通过（36/36 命令可调用、20/20 错误码有测试分支） | 2026-10-08 |
| 人工复查 | 待用户签核（本清单供评审逐条核对） | — |
