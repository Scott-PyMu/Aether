# ADR-035：M10 多模态附件类型、限额、存储、保留与适配器能力

| 项 | 内容 |
|---|---|
| 编号 | ADR-035（M10-00-05） |
| 版本 | **v1.1（2026-11-05，M10 方案决策记录 v0.5 合入）**；v1.0 = M10-00 初始登记 |
| 状态 | **已接受 v1.1（2026-11-05，M10-00 ADR 批次 + 用户决策记录 v0.5；用户授权 AI 审批；人工复核建议见 `docs/M10-00-ADR批次评审.md`）** |
| 决策日期 | 2026-11-05 |
| 决策载体 | 《设计文档》**v1.32**（D6/D7/D10/D13、附录 C）；`docs/P2-架构迁移-实施计划与验收标准.md` **v0.10**；`docs/M10-00-登记表.json`；`docs/M10-方案决策记录与修订建议.md` v0.5 |
| 关联 | 设计 §5 P2、§4#19；ADR-002（DSH 带外通道）、ADR-010（artifacts）、ADR-021（保留）、ADR-024（引用注入）、ADR-029（frame 上限/安全） |
| 取代 / 被取代 | 无（新增）；不改变 M9 已冻结的 63 命令语义 |
| 回退条件 | 见 §7 |
| 实现状态 | **未开始（登记先行；M10-05 执行）**；未登记前不得写业务代码 |

## 1. 背景与触发

M10-05 要求支持图像等多模态输入，覆盖采集、存储、预览、发送、适配器能力与清理。设计 D12 已预留 `messages.content_parts`（多模态，P2）；M9 的 `artifacts` 只覆盖文件引用，不承载二进制内容。M10-00 必须先冻结附件格式/限额/存储/保留/能力门与命令面，避免 UI 与适配器各自实现不一致的二进制通道。

## 2. 决策

### D-035-1 范围与格式（M10 仅图像）

- **允许格式（按魔数嗅探，不信任扩展名）**：`image/png`、`image/jpeg`、`image/webp`、`image/gif`（GIF 仅静态首帧预览，原文件按原样发送）。
- **不允许**：SVG（可执行/脚本）、PDF、音视频、任意二进制、压缩包、Office 文档；M10 不做 OCR/图像生成；P4+ 另立 ADR 扩展。
- **限额（硬，v1.1/Q9）**：单文件 ≤ **10 MiB**；单条消息 ≤ **8 个**；单会话 ≤ **200 个**；单会话附件总量 ≤ **1 GiB**；**单图像素 ≤ 25,000,000（25MP）且最长边 ≤ 8192**；文件名 ≤255 字符；MIME 与内容不符 → 拒绝。
- **超限结构化拒绝**：`too_large`/`out_of_range`/`invalid_value` + `business_code`（`attachment_too_large`/`attachment_limit_exceeded`/`attachment_type_unsupported`）；不创建附件、不产生消息/事件。
- **像素检查前置**：`@napi-rs/image` 不提供 `limitInputPixels`；**必须在构造 `Transformer` 前**解析文件头宽高/像素并拒绝超限（GIF 同理），不得先整图解码再检查。

### D-035-2 存储：内容寻址外置，DB 只存元数据

- 内容路径：`<data_dir>/attachments/<sha256[0..2]>/<sha256>.<ext>`；同一内容全局去重；**不覆盖既有文件**；原子写（临时文件 + `sync_all` + rename）；跨盘不走 rename。
- DB 新表（0010+ 规划位）：
  ```sql
  CREATE TABLE attachments (
    id           TEXT PRIMARY KEY,
    session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    message_id   TEXT REFERENCES messages(id) ON DELETE SET NULL,
    sha256       TEXT NOT NULL,
    mime         TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    width        INTEGER,
    height       INTEGER,
    storage_path TEXT NOT NULL,     -- 相对 data_dir 的规范化路径（不存绝对路径）
    created_at   INTEGER NOT NULL,
    UNIQUE (session_id, sha256)
  );
  CREATE INDEX idx_attachments_session ON attachments(session_id, created_at);
  ```
- `messages.content_parts` 存附件引用（`[{kind:"image", attachment_id, mime, width, height}]`），**不存二进制/大字段**；消息正文仍为 `content`。
- 路径校验：canonicalize + 前缀必须在 `<data_dir>/attachments/`；拒绝 `..`/符号链接逃逸/Junction；Windows 大小写/保留名规则复用 D9 路径细则。
- 删除：`attachment_remove` 删 DB 行并异步删文件（无其他引用时）；会话删除 `ON DELETE CASCADE` 删行，后台清理孤儿文件；启动扫描 `attachments/` 中 >24h 未被 DB 引用的文件并清理；**不删除仍被引用的内容**。
- 保留：附件随会话删除；会话保留/归档不自动搬移二进制（导出默认只带引用，不带内容；如后续需要带内容，另立 ADR）。

### D-035-3 采集与发送

- 采集入口：粘贴/拖拽/文件选择（`ref_pick` 选择文件后 `attachment_add`）；UI 先校验大小/数量再上传；上传中不阻塞输入；失败可重试/删除。
- `attachment_add`：`{session_id, source_path, kind:"image"?}`；源路径经 shell 选择器/用户拖拽提供，核心 canonicalize + 读取 + 魔数校验 + 哈希 + 原子存储；返回 `AttachmentSummary`（**Q10 A：不返回 `storage_path`**）`{attachment_id, sha256, mime, size_bytes, width?, height?, created_at}`；renderer 用 `sha256` 构造 `aether-attachment://thumb/<sha256>`。
- **缩略图（v1.1/Q3/Q10）**：解码/缩放/编码在 Electron **UtilityProcess**（受限进程，非沙箱）；`@napi-rs/image =1.16.0`（MIT）处理 PNG/JPEG/WebP，`omggif =1.0.10`（MIT）解 GIF 首帧；512px 最长边、WebP q80、EXIF rotate、剥离元数据；renderer 只展示已生成的缩略图，不解码原图；main 通过 `aether-attachment://` 协议按 `sha256` 取缓存/生成，路径白名单仅 `<data_dir>/attachments/`。缩略图缓存 `<data_dir>/attachments/thumbs/<sha256>.webp`，不写 DB。
- `attachment_list`：`{session_id}`；按 `created_at` 升序；只返回元数据。
- `attachment_remove`：`{attachment_id}`；幂等（不存在 → `{removed:false}`）；删除 DB 行 + 异步文件清理；正在发送的消息引用 → 拒绝或标记 `detached`（实现任务定义并测试）。
- 发送：`session_send` 增可选 `attachments: [attachment_id]`（≤8）；核心校验归属会话/存在/未删除/能力；写消息 `content_parts` 后向适配器 `session.send` 传**引用**：
  ```json
  { "attachments": [{ "kind": "image", "path": "<canonical>", "mime": "image/png", "size_bytes": 123, "sha256": "...", "artifact_ref": true }] }
  ```
  引用帧 <1MiB；**数据体不进线协议**（D6）；适配器按路径读取（信任级边界同 artifacts）。不支持时**不得静默丢弃**：返回 `attachment_type_unsupported`，不创建 run。
- `session_create` 不接收附件；附件只属于消息。

### D-035-4 适配器/模型能力门

- 适配器能力项新增 **`multimodal`**（hello/initialize 字符串项）；三官方运行时按静态能力表声明（与 `multi_provider` 同口径；P0/P1 官方均支持图像输入，具体模型能力另判）。
- 模型能力：`provider_models.capabilities`（0010+ 规划位，JSON 数组，如 `["vision"]`）；`providers_list` 增可选 `capabilities`（不改变既有字段语义）。
- 判定：
  - 运行时未声明 `multimodal` → UI 禁用附件入口 + 原因；`attachment_add` 仍可存（草稿），`session_send` 带附件 → `attachment_type_unsupported`。
  - 模型未声明 `vision`/能力未知 → UI 警告 `multimodal_model_unverified`（非阻断，用户可继续）；发送前显示警告；不静默丢弃。
  - 模型明确 `vision=false` → 拒绝发送（`attachment_type_unsupported`）。
- 不支持的多模态模型/运行时置灰或警告；错误文案见 UI-UX v0.19 §5.4。
- **解码库（v1.1/Q3）**：`@napi-rs/image =1.16.0`（MIT，PNG/JPEG/WebP/EXIF/缩放/WebP 编码）+ `omggif =1.0.10`（MIT，GIF 首帧 RGBA）；**不使用 `sharp`/`@img/*`**。`@napi-rs/image` 未提供 `limitInputPixels`，必须在构造 `Transformer` 前解析文件头宽高/像素并拒绝 >25MP；GIF 同理。
- **UtilityProcess（v1.1/Q3）**：解码/缩放/编码在 Electron UtilityProcess（受限进程，**非沙箱**）；无网络/无 shell 仅代码约束，不作隔离保证；崩溃重启 1 次后禁用附件并提示。

### D-035-5 UI 与无障碍

- 输入区附件条：**只展示已生成的缩略图**（≤512px 最长边），文件名/大小、删除、重试、上传进度；`data-state="uploading|ready|failed|sending"`；`data-error-code` 可断言。
- 预览：点击缩略图打开轻量预览（不进入 P4 文件浏览器）；键盘可操作；`alt` 使用清洗后的文件名（不含路径/控制字符），不把图片内容当作文本；**renderer 不接收原图 Buffer**。
- 大图/多图布局不回归：≤8 个横向滚动/换行；不挤压消息流；无障碍焦点顺序：附件条 → 输入框 → 发送。
- 粘贴/拖拽失败显示结构化错误；不阻塞 UI；不执行附件内容（不自动打开/不运行）。
- 缩略图协议：`aether-attachment://thumb/<sha256>`；CSP `img-src` 增 `aether-attachment:`；协议只允许 GET/64hex/路径白名单，不返回原图。

### D-035-6 安全、性能与诊断

- **不执行附件内容**：不做 SVG/HTML/脚本渲染；图片解码在 Electron UtilityProcess（受限进程，非沙箱），25MP/8192 上限；核心只做魔数/尺寸元数据读取（不整图解码）。
- 文件名/元数据清洗：去路径分隔符、控制字符、双向文本控制符；长度 ≤255；EXIF 不作为可信信息；不写入日志；缩略图剥离元数据。
- 密钥/路径扫描：附件二进制不进入日志/诊断/导出；`api_key`/token/PEM 扫描 0 命中（诊断只含数量/字节数/哈希前缀，不记绝对路径）。
- 性能：10 MiB 单图、10 并发发送；25MP 解码内存按 2 并发估算约 380MiB（25MP×4B×2×2 开销）；worker 并发 2、队列 16、单图 10s；DB 只增元数据；诊断指标 `attachments.count/bytes/orphans_cleaned/thumb_generated/thumb_failed`。
- 容量：单会话 1 GiB 上限；数据目录空间不足 → `attachment_storage_failed`；不静默降级为 base64 入库。
- 保留：孤儿清理 24h；会话删除级联；缩略图缓存随附件孤儿清理；不自动删除仍被消息引用的文件。
- **macOS（Q12）**：签名/公证不纳入本任务；`macos_release_pending`；仍做未签名 DMG 打包烟测（UtilityProcess 加载 `@napi-rs/image`、生成缩略图）。

## 3. 影响

- 《设计文档》v1.32：D6 `session.send.attachments` + `multimodal` 能力；D7 命令面 +3（累计 86）；D10 附件不落密钥/日志；D13 附件保留；附录 C 增 0010+ `attachments` 表/索引 + `provider_models.capabilities`。
- P2 计划 v0.10：M10-05 解锁；G11 以格式/限额/存储/清理/能力门/密钥 0 泄漏/25MP 为验收口径。
- UI-UX v0.19：消息输入区附件条/缩略图预览；锚点 `attachment` 组（1 组，3 个 testid）；文案见 §5.4；**不新增 testid**。
- 不新增运行时；新增 Node 依赖 `@napi-rs/image`/`omggif`（MIT，需 license/打包检查）；不改变事件类型（`message.completed` payload 可增可选 `attachments` 摘要）；不扩展 `IpcErrorCode` 32。

## 4. 主要风险与缓解

| 风险 | 缓解 | 失效条件 |
|---|---|---|
| 附件增长 | 10MiB/8/200/1GiB 限额；外置内容寻址；清理/诊断 | 超限或清理失败 → G11 不通过 |
| 解码炸弹/恶意文件 | 魔数白名单；SVG 拒绝；像素/尺寸上限；不执行内容 | 出现解码崩溃/OOM → 停止发布 |
| 路径逃逸 | canonicalize + 前缀白名单；不覆盖；相对路径入库 | 任一逃逸 → 停止发布 |
| 静默丢附件 | 能力门返回 `attachment_type_unsupported`；UI 置灰/警告 | 静默丢弃 → G11 不通过 |
| 密钥/路径泄漏 | 二进制不入日志/诊断/导出；扫描 0 命中 | 任一泄漏 → 停止发布 |
| DB 大字段 | 只存元数据/引用；`content_parts` 不含二进制 | DB 出现大字段 → 修复后重跑 |

## 5. 备选方案（未采纳）

| 方案 | 原因 |
|---|---|
| base64 存 DB | 违反 D3/容量/备份；帧上限 |
| 支持 SVG/PDF/视频 | 安全面/解码复杂；P4+ 另立 ADR |
| 自动把附件内容放进导出 | 体积/密钥/隐私风险；默认只带引用 |
| 适配器直接读任意路径 | 需 core 校验/审计；保持 artifact_ref 语义 |
| 上传到云端 | 违反本地优先；不做 |
| `sharp`/`@img/*` | **v1.1 不采纳**：用户决策改用 `@napi-rs/image` + `omggif`（MIT），避免 LGPL/体积/供应链风险 |
| renderer/main 直接解码 | 用户裁定放 UtilityProcess；renderer 只展示缩略图；main 不解码 |

## 6. 契约登记汇总

- 命令：3（`attachment_add`/`attachment_list`/`attachment_remove`；累计登记 82 → 85）。
- 现有命令增量：`session_send.attachments`（≤8）、`session_send.timeout_ms`（与 ADR-037 共用）、`providers_list` 可选 `capabilities`。
- 事件：0 新增；`message.completed` 可选 `attachments` 摘要。
- 设置键：0；迁移：0010+ `attachments` 表 + 索引 + `provider_models.capabilities`。
- 业务码：4（`attachment_type_unsupported`/`attachment_too_large`/`attachment_limit_exceeded`/`attachment_storage_failed`）；`attachment_remove` 幂等，不登记 `attachment_not_found`。
- 警告码：1（`multimodal_model_unverified`）。
- UI-UX 锚点：1 组（`attachment`，3 个 testid）；不新增 testid。
- 解码依赖：`@napi-rs/image =1.16.0` + `omggif =1.0.10`（均 MIT）；UtilityProcess（受限进程，非沙箱）。

## 7. 回退条件

1. 本 ADR 被否决 → M10-05 保持「未解锁」；不实现附件；文本路径不变。
2. 解码/路径/限额/密钥任一失败 → 不发布多模态；保留文本；修复后重跑 G11。
3. 适配器能力/线协议不兼容 → 回退 `session.send.attachments` 增量，另立 ADR；不得静默丢附件。
4. 需要新依赖（图像处理） → 先查 `deny.toml` 并另立依赖 ADR；不得静默引入。

## 8. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-11-05 | 文档维护（AI） | v1.0 创建 | 按 M10-00 DoD-1/2/5/7/8；登记先行，代码未改 |
| 2026-11-05 | 独立评审（AI，M10-00 批次） | 通过 | 检查格式白名单/限额/外置存储/能力门/密钥扫描；闭环见 `docs/M10-00-ADR批次评审.md` |
| 2026-11-05 | 用户决策记录 v0.5 | **v1.1 通过** | Q3 `@napi-rs/image`+`omggif`；Q9 25MP/8192；Q10 不返回 storage_path；Q12 macOS pending；见 `docs/M10-方案决策记录与修订建议.md` |

## 9. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v1.0 | 2026-11-05 | 创建：图像格式/限额/内容寻址/能力门/3 命令/0010+ 表/清理 | 文档维护 |
| v1.1 | 2026-11-05 | 用户决策记录 v0.5：`@napi-rs/image`+`omggif`（MIT，替代 sharp）；25MP/8192；UtilityProcess 受限进程；`attachment_add` 不返回 storage_path；缩略图协议/缓存；macOS signing pending | 文档维护 |
| v1.1 修订 | 2026-11-05 | 决策记录 v0.7：Q3 三平台打包 spike 必做（含未签名 DMG）；不引入 sharp/@img | 文档维护 |

---

## 附录 A：格式矩阵

| MIME | 魔数 | 允许 | 备注 |
|---|---|---|---|
| `image/png` | `89 50 4E 47 0D 0A 1A 0A` | 是 | 支持透明 |
| `image/jpeg` | `FF D8 FF` | 是 | — |
| `image/webp` | `52 49 46 46 ... 57 45 42 50` | 是 | — |
| `image/gif` | `47 49 46 38` | 是 | 仅首帧预览；原文件发送 |
| `image/svg+xml` | XML | 否 | 可执行内容 |
| `application/pdf` | `%PDF` | 否 | P4+ 另立 ADR |
| 其他 | — | 否 | `attachment_type_unsupported` |

## 附录 B：附件生命周期

```
添加 → 已存储（ready） → 发送引用（sending） → 消息完成（attached）
  ├→ 删除（remove）→ DB 行删除 → 文件无引用时异步清理
  ├→ 会话删除 → CASCADE 行删除 → 文件无引用时清理
  └→ 孤儿扫描（>24h 无 DB 引用）→ 清理
```

## 附录 C：能力判定

| 运行时 `multimodal` | 模型 `vision` | 行为 |
|---|---|---|
| 否 | 任意 | UI 禁用附件；`session_send` 带附件 → `attachment_type_unsupported` |
| 是 | 未知/未声明 | 允许 + 警告 `multimodal_model_unverified` |
| 是 | `vision=true` | 允许 |
| 是 | `vision=false` | 拒绝发送 |
