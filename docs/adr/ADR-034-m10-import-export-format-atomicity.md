# ADR-034：M10 导入/导出格式、版本、ID 重映射、冲突与原子性

| 项 | 内容 |
|---|---|
| 编号 | ADR-034（M10-00-04） |
| 版本 | **v1.0（2026-11-05）** |
| 状态 | **已接受（2026-11-05，M10-00 ADR 批次；用户授权 AI 审批；人工复核建议见 `docs/M10-00-ADR批次评审.md`）** |
| 决策日期 | 2026-11-05 |
| 决策载体 | 《设计文档》**v1.31**（D3/D4/D10/D13、附录 C）；`docs/P2-架构迁移-实施计划与验收标准.md` **v0.9**；`docs/M10-00-登记表.json` |
| 关联 | 设计 §5 P2、DS-04；ADR-005（`client_msg_id` 幂等）、ADR-009（`messages_page`）、ADR-020（备份/恢复）、ADR-021（归档格式）、ADR-023（审计）、ADR-025（会话管理）、ADR-029（daemon） |
| 取代 / 被取代 | 无（新增）；扩展 ADR-021 的归档格式使用面，不改变其 v1 行格式 |
| 回退条件 | 见 §7 |
| 实现状态 | **未开始（登记先行；M10-04 执行）**；未登记前不得写业务代码 |

## 1. 背景与触发

P1 已实现 `session_export`/`archive_export`（JSONL + manifest，`kind=session/message/event/run/audit`，格式版本 `v=1`），但没有导入；M10-04 要求补齐导入与完整往返，支持会话 JSONL/归档、不可信输入安全处理、原子性、ID 重映射/冲突、大库与脱敏。M9-13 的旧数据/备份/导出基线必须保持。本 ADR 冻结导入格式、版本、原子性、ID 策略与审计，作为 M10-04 的启动门。

## 2. 决策

### D-034-1 格式与版本（复用归档 v1，不新增容器格式）

- **不新增压缩容器/依赖**：导入源为现有 **JSONL**（UTF-8、LF 分隔）：
  - `session_import` 接受 `session_export`（`source=session-export`，单会话）；
  - `archive_import` 接受 `archive_export` / `auto-retention`（`source=archive-export|auto-retention`，可多会话）；
  - 首行必须是 manifest（`kind=manifest`，`v=1`），后续行 `kind` ∈ `session|message|event|run|audit`；`v>1` → `import_invalid_format`（提示升级应用）；`v<1` → 拒绝。
- **未知字段策略**：manifest/行内**已知 kind 的未知顶层字段忽略**（前向兼容，记录警告）；未知 `kind` → 整文件拒绝（不猜测语义）；未知 `event.type` → 跳过该事件并警告（不执行、不广播）。
- **校验**：manifest 的 `rows` 与实际 payload 行数一致；`sha256` 与 payload 行拼接（行尾 `\n` 归一）一致；不一致 → `import_checksum_mismatch`，不进入目标库。
- **行/文件上限**：单行 ≤ **2 MiB**；文件 ≤ **20 GiB**；manifest 行 ≤64 KiB；超过 → `import_too_large`；流式读取，内存 ≤1 MiB 缓冲 + 当前行。
- **不执行内容**：导入只做数据写入；**不触发工具调用、不访问网络、不执行事件 payload、不重建原生会话/不调用适配器**；导入后的会话按 provider/model 可用性决定是否只读。
- **导出格式不变**：`session_export`/`archive_export` 继续输出 v1 manifest + 行；M10-04 只允许新增可选字段（如 `exported_by`、`app_version`），不得改既有字段语义；M9-13 基线对照仍通过。

### D-034-2 原子性：staging DB + 维护窗口 + 原子切换

导入是维护操作，**不复用普通写队列长事务**（D3 单事务目标 <500ms）：

1. **获取维护锁**：`import`/`restore` 互斥；进入 `maintenance=import`，停止接纳新 run（已有 run 等待/取消由用户选择），UI 显示只读/进度。
2. **快照当前库**：`VACUUM INTO <data_dir>/imports/<job_id>/work.db`（或等价只读复制 + WAL checkpoint）；失败 → `import_job_not_active`/`internal`，不动现场。
3. **staging 应用**：在 `work.db` 上按行批量写入（每批 ≤1000 行，单事务目标 <500ms），维护 `import_id_map` 临时表（`old_id,new_id,kind`，仅 staging 库）；每批校验行数/哈希；进度每 1s 或每 10k 行更新。
4. **候选验收**：`PRAGMA quick_check`、schema 版本 ≤ 当前程序、行数/关键字段计数、manifest 哈希复验；任一失败 → 丢弃 work.db，现场不变。
5. **原子切换**：写恢复日志（复用 D13 七步的 `.pre-import` 现场保护 + `-wal`/`-shm` 同批改名），关闭连接池/写队列，`rename(work.db → aether.db)`；失败 → 回滚现场。
6. **重开/恢复**：重新打开 DB、`quick_check`、迁移到当前版本；成功 → `maintenance` 解除、广播 `import.completed` 审计；失败 → 回滚 `.pre-import` 并重启 daemon。
7. **清理**：成功后删除 `imports/<job_id>` staging；崩溃残留由启动清理（>24h 或 job 非活动）；`dry_run` 只执行 1–4 并返回预览，不切换。

- **取消**：`import_cancel` 在每个批次边界检查；取消 → 停止写入、删除 staging、保留现场；返回 `import_cancelled`。kill -9：启动时根据 journal/staging 标记清理，不自动续跑（可重新导入）。
- **大文件**：≥1GB 样本流式处理；进度/取消/空间检查（目标数据目录可用空间 ≥ 预期增量×1.2 + 当前 db+wal 的 1.2 倍）；不足 → `import_too_large`/`invalid_value`。
- **不产生半成品**：任何失败都不改变既有 `aether.db`；`.pre-import` 保留至下一次成功启动后按保留策略清理。

### D-034-3 ID 重映射与冲突

| 模式 | 行为 |
|---|---|
| `preserve`（非默认） | 保留原 ID；与目标库冲突 → `import_conflict`（列出冲突 id/kind），不覆盖、不合并 |
| `remap`（默认） | 为 session/message/run/event/permission/artifact/audit 生成新 ULID；维护映射；保留时间戳/标题/内容/seq 顺序 |

- **默认 `remap`**：避免与现有数据冲突；`preserve` 仅用于空库/明确合并场景；UI 默认 remap 并提示。
- 映射规则：
  - `sessions.id` → 新 ULID；`sessions.parent_session_id`、`workflow_runs.session_id`、`node_runs.session_id`、`tasks.session_id`、`artifacts.session_id`、`messages.session_id`、`events.session_id`、`permissions.session_id`、`audit_log.session_id` 同步映射。
  - `messages.id`/`runs.id`/`events.id`/`permissions.id`/`audit_log.id` → 新 ULID；`runs.input_message_id`、`messages.run_id`、`events.run_id`、`node_runs.id` 等引用同步映射。
  - `events.seq`/`messages.seq` 按会话**保持相对顺序**重新分配（从目标会话当前 max+1 起），保证 `UNIQUE(session_id,seq)`；事件按 `seq` 升序写入；不改变事件类型/ts/payload。
  - `client_msg_id`：同会话内已存在 → 视为重复，跳过该消息（计 `duplicates`）并警告；不报错、不覆盖；保留 `UNIQUE(session_id,client_msg_id)`。
  - `sessions.title` 缺失 → `Imported session <id 前 8 位>`；`workspace_id` 缺失/不存在 → `NULL` + 警告 + 会话只读（不自动创建工作区）；`provider_id`/`model` 缺失/停用/不属于 → 保留原值，`provider_state=invalid`/`model_state=invalid`，会话只读 + 警告；不自动选新 provider。
  - `artifacts.path`：只作为引用文本导入；若路径不存在或不在工作区，跳过并警告（不读取、不复制、不执行）；不导入 `api_key`/token 字段。
  - 未知/高版本 `v` → 拒绝；未知可选字段忽略。
- **往返一致**：导出 → 导入（remap）→ 再导出；比较行数、`kind` 计数、会话标题/消息角色与内容、事件 `type`/payload（排除 id/seq/ts 等重映射/易变字段）、审计/权限/引用条数；哈希按 canonical 归一后比较；差异 → 证据记录，不通过 G11。

### D-034-4 命令面与作业模型

| 命令 | 参数（严格解析） | 返回 | 超时 | 主要业务码 |
|---|---|---|---|---|
| `session_import` | `{source_path, mode?, dry_run?}` | `{job_id,status}` | 30s | `import_invalid_format`/`import_too_large` |
| `archive_import` | `{source_path, mode?, dry_run?}` | `{job_id,status}` | 30s | 同上 |
| `import_status` | `{job_id, include_warnings?}` | `ImportJobStatus` | 10s | `import_job_not_found` |
| `import_cancel` | `{job_id}` | `{status}` | 15s | `import_job_not_active` |

- `mode` ∈ `preserve|remap`（缺省 `remap`）；`dry_run` 缺省 `false`；`source_path` canonicalize + 存在文件 + `.jsonl` 后缀 + 可读 + 大小上限；不接受目录（归档目录由 UI 先选 manifest 文件）。
- `ImportJobStatus`：`{job_id, kind:"session|archive", status:"staging|validating|applying|awaiting_restart|completed|failed|cancelled", mode, dry_run, progress:{rows,bytes,total_bytes?}, summary?:{sessions,messages,events,runs,audits,duplicates,skipped}, warnings?:[{code,path?,detail}], error?:{code,message}}`。
- 作业在 daemon 内异步执行；`session_import`/`archive_import` 只做路径/首行/大小预检并返回 job；后续进度靠 `import_status` 轮询（UI 1s；CLI 1s/`--watch`）。
- 同一时间只允许 1 个导入作业（`import_job_not_active`/`import_conflict`）；与 `backup_restore` 互斥；`maintenance` 期间 `session_send`/`workflow_run` 拒绝（`core_not_ready`/`storage_backpressure`），只读命令可用。
- 取消：仅在 `staging|validating|applying` 有效；`awaiting_restart|completed|failed|cancelled` → `import_job_not_active`。

### D-034-5 导出增量与审计

- 现有 `session_export`/`archive_export` 保持 v1；M10-04 可选增 `exported_by`/`app_version`（不改变必填字段）；导出仍脱敏（`api_key`/token/PEM 0 命中）、原子写、空间检查。
- 审计动作（ADR-023 台账增行）：`import.started`、`import.completed`、`import.failed`、`import.cancelled`、`export.started`、`export.completed`、`export.failed`；`actor=user`；`detail` 含 job_id、source、mode、counts、warnings 数、duration；**不记录文件内容/密钥**。
- 导入完成后写审计；`dry_run` 只写 `import.started` + `import.completed(dry_run=true)`？为避免污染，`dry_run` 不写审计，只在 `import_status` 返回预览。最终实现以 M10-04 证据为准。

### D-034-6 迁移规划位

- **无新增表/列/索引**：staging 使用临时 DB + 临时 `import_id_map`；审计/会话/消息/事件复用现有表。
- 不修改 0001–0009；若实现中确需列/索引，先修订本 ADR 并登记 0010+，与附录 C/`verify:m1-03` 同批。

## 3. 影响

- 《设计文档》v1.31：D7 命令面 +4（累计 82）；D13 增导入维护例外；附录 C 无新迁移（仅说明 staging 临时库）。
- P2 计划 v0.9：M10-04 解锁；G11 以往返/原子/大库/脱敏为验收口径。
- UI-UX v0.18：S-06 增导入入口/结果/警告/失败恢复；不新增锚点组（复用 `archive`/`backup-*`；如需新增在 M10-04 证据登记）。
- 不新增依赖/设置键；不改变 EventEnvelope/事件类型；不改 `IpcErrorCode` 32。

## 4. 主要风险与缓解

| 风险 | 缓解 | 失效条件 |
|---|---|---|
| 导入不可信数据 | manifest/schema/version/checksum；未知 kind 拒绝；不执行内容；行/文件上限 | 损坏样本改变既有数据 → G11 不通过 |
| 半成品/数据丢失 | staging DB + 原子切换 + `.pre-import` 回滚；失败不动现场 | 出现半成品或既有数据丢失 → 停止发布 |
| 长事务/写放大 | 批量事务 <500ms；维护窗口停写；staging 独立库 | 单事务 >500ms 且无法拆分 → 记录并优化 |
| ID 冲突/引用断裂 | remap 映射全引用；`preserve` 冲突显式拒绝 | 出现悬挂引用/重复 seq → G11 不通过 |
| 大文件内存 | 1MiB 流式；进度/取消；空间检查 | OOM 或不可取消 → 修复后重跑 |
| 密钥/路径泄漏 | 脱敏扫描 0 命中；不导入 key/token；诊断不含内容 | 任一泄漏 → 停止发布 |

## 5. 备选方案（未采纳）

| 方案 | 原因 |
|---|---|
| 引入 ZIP/tar.zst 容器 | 需要新依赖/解压炸弹面；JSONL 已满足 |
| 单事务直接写 live DB | 大库长事务违反 D3；失败难回滚 |
| 导入时自动重建原生会话/调用适配器 | 不可信输入执行风险；明确禁止 |
| 自动合并同 ID 会话 | 语义不明、易覆盖；`preserve` 冲突显式拒绝 |
| 导入时自动选 provider/model | 可能改变用户意图/产生费用；只读警告 |

## 6. 契约登记汇总

- 命令：4（累计登记 78 → 82）。
- 事件：0（进度走 `import_status`；不新增事件类型）。
- 设置键：0；迁移：0（staging 临时库）。
- 业务码：7（`import_invalid_format`/`import_checksum_mismatch`/`import_conflict`/`import_too_large`/`import_cancelled`/`import_job_not_found`/`import_job_not_active`）。
- 审计动作：7（`import.*`/`export.*`）。
- UI-UX 锚点：0 新增组（复用 S-06 `archive`/`backup-*`；M10-04 如需新增在证据登记）。

## 7. 回退条件

1. 本 ADR 被否决 → M10-04 保持「未解锁」；不实现导入；导出仍可用。
2. 往返/原子/大库/脱敏任一失败 → 不发布导入；保留导出；修复后重跑 G11。
3. staging/切换导致数据丢失 → 立即停止发布，按恢复手册回滚 `.pre-import`；不得手工改库。
4. 需要新依赖/压缩容器 → 另立 ADR；不得静默引入。

## 8. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-11-05 | 文档维护（AI） | v1.0 创建 | 按 M10-00 DoD-1/2/4/7/8；登记先行，代码未改 |
| 2026-11-05 | 独立评审（AI，M10-00 批次） | 通过 | 检查格式复用/原子切换/ID 映射/不可信输入/审计；闭环见 `docs/M10-00-ADR批次评审.md` |

## 9. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v1.0 | 2026-11-05 | 创建：格式 v1/staging 原子切换/ID 重映射/4 命令/审计/回退 | 文档维护 |

---

## 附录 A：导入行样例（节选）

```jsonl
{"v":1,"kind":"manifest","created_at":1762300000000,"source":"session-export","session_id":"01J...","rows":3,"sha256":"<hex>"}
{"v":1,"kind":"session","id":"01J...","runtime_id":"codex","workspace_id":null,"parent_session_id":null,"title":"demo","status":"completed","model":"gpt-5","created_at":1762290000000,"updated_at":1762299999999,"closed_at":null}
{"v":1,"kind":"message","session_id":"01J...","id":"01J...","role":"user","content":"hello","seq":1,"ts":1762290000000}
{"v":1,"kind":"event","session_id":"01J...","id":"01J...","run_id":null,"runtime_id":"codex","seq":1,"type":"message.completed","payload":{},"ts":1762290000001,"v":1}
```

## 附录 B：ID 映射表（staging 临时）

| 列 | 说明 |
|---|---|
| `old_id` | 源文件 id（ULID/字符串） |
| `new_id` | 目标库新 ULID |
| `kind` | `session/message/run/event/permission/artifact/audit/node_run/workflow_run` |
| `session_old`/`session_new` | 便于按会话校验 seq |

临时表只存在于 staging `work.db`，切换后丢弃；不进入正式 schema。

## 附录 C：导入/恢复互斥

| 操作 | 导入中 | 恢复中 |
|---|---|---|
| `session_import`/`archive_import` | 拒绝（`import_job_not_active`） | 拒绝 |
| `backup_restore` | 拒绝 | 拒绝 |
| `session_send`/`workflow_run` | 拒绝（维护） | 拒绝 |
| `health`/`session_list`/`messages_page` | 只读可用 | 只读可用 |
