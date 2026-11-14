# ADR-020：自动备份与加密备份方案（D13/DS-05 P1 完整；P1-A05）

| 项 | 内容 |
|---|---|
| 编号 | ADR-020（P1-A05） |
| 版本 | **v1.1（2026-11-14，ADR-039 v1.0 整套备份适配）**：v0.1 创建；v0.2 批次评审闭环；v1.0 作为 P1 基线；v1.1 按 ADR-039 v1.0 扩展为整套 generation 备份 |
| 状态 | **已批准（v1.1，2026-11-14；v1.0 于 2026-10-09 批准；人工授权 AI 审批见 `docs/M5-01-ADR批次评审意见.md` §7）**；已合入《设计文档》v1.35 |
| 决策日期 | 2026-11-14（v1.1）；2026-10-09（v1.0） |
| 决策载体 | 《设计文档》v1.35（D3/D13、附录 A DS-05）；`docs/P2-架构迁移-实施计划与验收标准.md` v0.14；**ADR-039 v1.0** |
| 关联 | 设计 D3/D10/D13、A3、A10、§5 P1/P2 退出标准；需求 DS-05；ADR-003（外部路径/空间检查）、ADR-004（存储降级边界）、ADR-007（设置键框架）；ADR-014 附录 C-1、ADR-018 附录 A-4；M1-07（A3 降级密钥设施）；P1 计划 M5-02/M5-03；**ADR-039（generation/`CURRENT`/整套备份）** |
| 取代 / 被取代 | 本 ADR **v1.1 取代 v1.0**；`backups` 表字段保持，备份产物从单文件扩展为整套 generation + manifest |
| 回退条件 | 见 §6 |
| 实现状态 | M5-02/M5-03 已执行（单文件）；**v1.1 整套备份随 M10-09 落地** |

## 1. 背景

- P0：手动 `VACUUM INTO`（保留 10 份）、无自动排期、无加密；D13 失效条件：库 >5GB 或事件增长 >500MB/月 → P1 保留策略（已由 ADR-021 承接）。
- 需求侧（P1 计划 §10 待确认 4）：**自动备份频率与保留份数；加密密钥来源（Keychain / 口令 / 两者）与用户提示口径**——本 ADR 闭合。
- 既有设施：`backup_create`/`backup_list`/`backup_restore`（D13 恢复七步）、`backups` 表、A3 安全级别降级（`secrets.enc` + 口令）、M1-07 加密文件设施、7 天未备份提醒（`backup.reminder`，M3-05）。
- P1 退出标准 2：加密备份恢复演练脚本通过（M5-03）。

## 2. 决策

### 2.1 核心决策（五段式）

- **决策**：
  1. **自动备份**：进程内调度器按 **`backup.auto.frequency`（`daily` 缺省 / `weekly`）**触发；总开关 **`backup.auto.enabled` 缺省 `true`**（ADR-013 §4 风险与缓解表口径「自动备份默认开启」）；保留 **`backup.auto.retention`（缺省 10）份自动产物**（`kind='auto'`），**手动产物（`kind='manual'`）不自动删除**。
  2. **错过窗口策略**：应用未运行时错过排期 → 启动后 **2 分钟（抖动窗口）内补备一次**（记录 `missed_window_catchup` 诊断标记）；连续错过 ≥7 天仍只补一次（不追补多次）。
  3. **加密**：默认加密（**`backup.encryption.enabled` 缺省 `true`**）；算法 **XChaCha20-Poly1305（AEAD）**；**默认密钥 = OS Keychain 主密钥**（`keychain://aether/backup/master`，32B 随机，首次备份生成）；**口令模式为显式可选**（`backup_create`/`backup_restore` 可选 `passphrase`，Argon2id `m=64MiB, t=3, p=1` + 随机 salt 派生；`passphrase` 仅传输、不落盘、不入日志/诊断/导出）。
  4. **恢复**：`backup_restore` 自动用 Keychain 主密钥解密（整套 generation）；Keychain 不可用（换机/降级未解锁）→ 提示口令（仅口令模式产物可恢复）；**口令/密钥错误 → 明确拒绝（结构化错误）且当前 generation 不被破坏**；恢复 staging 空间预检 ≥ generation ×1.1，`CURRENT` 原子切换，失败整套回滚（ADR-039 D-039-8）。
  5. **失败隔离**：备份失败（目标不可写/空间不足/磁盘满/加密失败）**不产生半成品、不删除既有备份、不误入 `persist_degraded`**（D4 边界：备份域失败 ≠ 存储写失败）；告警记录 + 下次窗口重试；连续失败写入 `backup_list.auto.consecutive_failures` 与诊断包。
  6. **整套备份集（ADR-039 v1.0）**：备份产物为 `<backup_id>/manifest.json` + `manifest.sha256` + `generation/`（主库 + 8 分片 + `shards.json`）；逐片 `VACUUM INTO` + 主库 `VACUUM INTO`；停写/drain/checkpoint 后构成同一一致点；`backup_list` 按套列出；旧单文件 `.db`/`.aebk` 仅展示、不可恢复；维护态独占（除 `health`/`app_exit` 外拒绝）；旧 generation 保留 1 份（`.pre-*`/`.failed-*`），附件不随库恢复。
- **实现要点**：
  1. 调度器：后台任务（不阻塞主流程）；触发点 = 上次成功时间 + 频率；执行走既有 `backup_create` 代码路径（逐片 `VACUUM INTO` + 主库 `VACUUM INTO` 到临时 generation + manifest + 原子 rename），停写/drain/checkpoint 后快照；写队列 P95 不劣化（M5-02 DoD2 基准断言）。
  2. 保留滚动：仅 `kind='auto'`；清理动作**先验证新产物可用（主库+全部分片 `quick_check` + manifest）再删最旧**；`backups` 表与磁盘清单一致（M5-02 DoD3 脚本断言）。
  3. 设置键（M5-01 DoD5② 增行，见附录 A-1）：`backup.auto.enabled` / `backup.auto.frequency` / `backup.auto.retention` / `backup.encryption.enabled`。
  4. `backup_list` 响应增 `auto` 段（ADR-018 附录 A-5）；7 天未备份提醒与自动备份联动：自动备份成功后提醒计时刷新，**不重复告警**（M5-02 DoD5）。
  5. 加密产物格式（附录 A-2）：magic `AEBK1` + version + flags + (口令模式: salt) + nonce(24B) + ciphertext + tag；`backups.encrypted` 标记；来源 id 记入产物头（恢复校验）。
  6. 演练：`scripts/test/m5-03/` 演练脚本（可重复执行、CI/夜跑接线；**演练本身不依赖外部凭证，禁止静默跳过**）：加密整套备份 → 清 generation → 恢复 → 主库+全部分片 `quick_check` + 行数/哈希/manifest 抽查一致；附件缺失提示可见。
- **主要风险**：主密钥丢失 = 加密备份不可恢复（用户未备份 Keychain）；口令遗忘；自动备份与手动备份并发争用写队列；目标目录磁盘满导致连续失败；「加密失败静默降级为不加密」错误实现。
- **缓解措施**：恢复手册明示密钥来源与丢失后果（M5-03 DoD5）；**禁止静默降级**——加密开关为 true 且加密不可用（Keychain 失败且用户未提供口令）→ 自动备份**暂停并告警**（不产出不加密产物）；用户显式关闭 `backup.encryption.enabled` 才允许不加密；备份互斥锁（同一时刻最多 1 个备份任务，自动/手动串行）；空间检查复用 M3-04 口径（db+wal ×1.2）。
- **失效条件**：加密备份恢复演练**两轮不通过** → 冻结发布路径，评审加密方案（改 ADR 换方案）；出现「密钥泄露/产物被静默降级为明文」→ 立即停用自动备份并另立 ADR；自动备份引发主流程写延迟劣化（D3 P95 阈值）→ 暂停调度器并复审。

### 2.2 明确不做（P1 边界）

- 不做增量备份 / 差量备份（全量 `VACUUM INTO` 口径不变）；
- 不做云/网络目标自动上传（外部路径仅本地目录，A4 同步盘拒绝口径不变）；
- 不做密钥托管/硬件密钥（P3+，D10 失效条件）；
- 不做备份产物的跨版本自动迁移（恢复七步的版本检查口径不变）。

## 3. 影响

### 3.1 《设计文档》v1.34 → v1.35（v1.1 增量）

- D13：自动/加密备份保持 ADR-020；**备份/恢复改为整套 generation + manifest**，逐片 `VACUUM INTO`、staging 空间预检、`CURRENT` 原子切换、旧 generation 保留 1 份（`.pre-*`/`.failed-*`）；维护态除 `health`/`app_exit` 外拒绝；附件不随库恢复。
- D3 备份段：分片库逐片 checkpoint/`VACUUM INTO`；主库 + 分片一致点。
- D7：`backup_create`/`backup_list`/`backup_restore` 字段增量与外部路径语义；无新增命令。
- **受影响的既有语义**：恢复从单 `.db` 改为整套 generation；旧单文件 `.db`/`.aebk` 仅展示、不可恢复；`backup_list.capacity` 统计当前 generation 主库+全部分片；M9-01 基线重录。
- **保留策略**：手动产物不自动删除；自动产物按 `backup.auto.retention` 滚动；旧 generation（`.pre-*`/`.failed-*`）按 ADR-039 D-039-8 保留 1 份/清理。

### 3.2 `docs/P2-架构迁移-实施计划与验收标准.md` v0.13 → v0.14

- M10-09 承接整套备份/恢复/导入；G13 条件包含 M10-09/10/11 证据与 M9-01 重录；UI-UX v0.20 的 S-06 适配旧 `single_file` 禁用/附件提示。

### 3.3 不影响的

- 不改 A3 降级机制本身（复用 M1-07 密钥设施）；不改 D10 密钥存储面（主密钥仍在 Keychain/`secrets.enc`）；不新增事件类型；不新增依赖（加密实现用既有 `aether-security` 设施提供的算法；若需引入新 crate 必须先走 `deny.toml` 白名单评审）；不改迁移 0001–0013（0014 由 ADR-039 v1.0 登记）。

## 4. 版本（拟合入）

| 文档 | 修订前 | 修订后 |
|---|---|---|
| 《设计文档》 | v1.34 | **v1.35** |
| 《P2-架构迁移-实施计划与验收标准》 | v0.13 | **v0.14** |
| UI-UX | v0.19 | **v0.20** |

## 5. 后续（未决项）

1. 密钥轮换（重加密历史备份）——P2 评估。
2. 备份目标多目录轮换——P2 评估。
3. Linux Secret Service 可用性下的密钥行为——随 Linux 条件项（ADR-014 决策 5）。

## 6. 回退条件

1. **本 ADR 被否决** → 自动备份与加密不落地；M5-02/M5-03 撤回；D13 维持 P0 手动口径；设置键增行撤回。
2. **加密方案被评审否决** → 换方案另立 ADR；已产出的加密产物须保证通过恢复手册可恢复或明确标记不可用。
3. **密钥丢失/泄露事件** → 停用自动备份 + 复盘；涉及设计缺陷另立 ADR。
4. **备份失败误入 `persist_degraded`** → 按 bug 处理并回归 D4 边界断言（M5-02 DoD4）。
5. **新错误码/警告码需求** → 先按 ADR-006 附录 B 登记，不得自造。

## 7. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-10-09 | 文档维护（AI） | v0.1 创建 | 按 P1 计划 M5-02/M5-03 DoD 与 §10-4 定界；登记先行，代码未改 |
| 2026-10-09 | 独立评审（M5-01 批次，`docs/M5-01-ADR批次评审意见.md`） | 通过（含修订闭环） | 批次复核 17/17 闭环；本 ADR 相关条目见该意见 §6。未另行修改决策 |
| 2026-11-14 | 文档维护（AI） | v1.1 整套备份适配 | 按 ADR-039 v1.0：generation/manifest、逐片 `VACUUM INTO`、staging/CURRENT、旧 `single_file` 禁用 |

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v1.1 | 2026-11-14 | 整套 generation 备份/恢复 + manifest/`manifest.sha256`；逐片 `VACUUM INTO`；staging 空间预检/`CURRENT` 原子切换；旧 generation 保留 1 份；旧 `.db`/`.aebk` 仅展示；附件不随库提示 | 文档维护 |
| v1.0 | 2026-10-09 | P1 基线：自动备份排期/保留/补备、XChaCha20-Poly1305 + Keychain 主密钥（口令可选）、失败隔离、设置键与响应增量 | 文档维护 |
| v0.2 | 2026-10-09 | 批次评审闭环（不改决策） | 文档维护 |
| v0.1 | 2026-10-09 | 创建 | 文档维护 |

---

## 附录 A：加密 / 设置键 / 数据登记

**A-1 设置键增行（M5-01 DoD5②；键名/类型/缺省值/语义/迁移影响）**

| 键名 | 类型 | 缺省值 | 语义 | 迁移影响 |
|---|---|---|---|---|
| `backup.auto.enabled` | bool | `true` | 自动备份总开关（ADR-020） | 无（settings KV） |
| `backup.auto.frequency` | string（枚举 `daily`/`weekly`） | `"daily"` | 自动备份频率 | 无 |
| `backup.auto.retention` | int（1–100） | `10` | 自动产物保留份数（仅 `kind='auto'`） | 无 |
| `backup.encryption.enabled` | bool | `true` | 加密备份开关；`true` 且密钥不可用时备份暂停并告警（禁止静默降级） | 无 |

**A-2 加密产物格式（v1）**

```
magic   : "AEBK1" (5B)
version : u8 = 1
flags   : u8  bit0 = passphrase-mode, bit1 = keychain-mode
salt    : 16B（仅 passphrase-mode）
nonce   : 24B（随机）
body    : XChaCha20-Poly1305 ciphertext（末 16B 为 AEAD tag；AAD = magic..nonce 的完整头字节）
source  : 产物头附 `source_backup_id` + 生成时间（恢复校验与来源 id 正确性断言）
```

- **完整性**：单一 AEAD tag（16B）同时认证头与密文（头作为 AAD 输入）——**不另设独立 header_auth 字段**；任何头字节篡改都会在解密时失败并明确拒绝。
- **flags 合法组合（v1）**：`0x01`（口令模式，含 salt）与 `0x02`（Keychain 模式）**恰置其一**；两者同置、均不置或出现未知位 → 产物无效，拒绝恢复。
- 未加密（`backup.encryption.enabled=false` 或历史 P0 产物）：保持 P0 `.db` 形态，`encrypted=0`，可恢复。
- **整套备份（v1.1）**：每个 DB 文件按 A-2 加密，manifest 记录统一 `encrypted`/`artifact_mode` 与全部密文 sha256；同一备份集必须使用同一密钥/模式；不得逐片独立选择不同密钥导致 manifest 不一致。恢复先解密到 staging generation，再按 ADR-039 D-039-8 切换。

**A-3 审计与诊断**

| 动作/指标 | 说明 |
|---|---|
| `backup.auto.run`（audit，actor=system） | result = success/failed（detail 含产物 id/大小/耗时/失败原因） |
| 诊断包 | 自动备份最近结果、连续失败数、下次排期、补备标记、备份耗时/体积指标；备份集 manifest/`manifest.sha256`；各 generation（current/pre/failed/incomplete）大小与状态；附件未随库提示 |

**A-4 演练脚本契约（M5-03 落点）**

- `scripts/test/m5-03/restore-drill.mjs`：加密**整套 generation**备份 → 清 generation → 恢复 → 主库+全部分片 `quick_check` + 行数/哈希/manifest 抽查；口令错误路径、Keychain 不可用路径（模拟）、staging 空间不足、旧 `.db` 拒绝、附件提示；无外部凭证依赖，**缺依赖即失败而非跳过**。
