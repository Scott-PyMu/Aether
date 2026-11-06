# ADR-036：M10 i18n / 主题范围、UI-UX 锚点与文案

| 项 | 内容 |
|---|---|
| 编号 | ADR-036（M10-00-06） |
| 版本 | **v1.0（2026-11-05）** |
| 状态 | **已接受（2026-11-05，M10-00 ADR 批次；用户授权 AI 审批；人工复核建议见 `docs/M10-00-ADR批次评审.md`）** |
| 决策日期 | 2026-11-05 |
| 决策载体 | 《设计文档》**v1.31**（D7、§4#25、§5 P2）；`docs/P2-架构迁移-实施计划与验收标准.md` **v0.9**；`docs/UI-UX-设计规格.md` **v0.18**；`docs/M10-00-登记表.json` |
| 关联 | 设计 §5 P2、§4#25；UI-UX v0.17/v0.18；ADR-007（错误码）、ADR-018（设置键）、ADR-029（Electron/UI 安全） |
| 取代 / 被取代 | 无（新增）；不改变 63 命令/事件/错误码语义 |
| 回退条件 | 见 §7 |
| 实现状态 | **未开始（登记先行；M10-06 执行）**；未登记前不得写业务代码 |

## 1. 背景与触发

M10-06 要求中/英双语与浅色/深色/跟随系统主题，不改后端契约。P0/P1 为中文单语、跟随系统配色；M10 需要冻结语言范围、key 组织、回退、日期/数字格式、错误码本地化、主题 token、锚点与测试口径，避免「半翻译」和硬编码色值蔓延。

## 2. 决策

### D-036-1 语言范围

- 支持 **`zh-CN`** 与 **`en-US`** 两种语言；**不做 RTL**（阿拉伯语/希伯来语等 P5+ 另立 ADR）。
- 设置键 `ui.locale`：`"system" | "zh-CN" | "en-US"`，缺省 `"system"`，≤32 字节；系统检测按 OS locale（`zh*` → `zh-CN`，其余 → `en-US`）；无法检测 → `zh-CN`（现有产品主语言）。
- 运行期切换立即生效，不重启；`document.documentElement.lang` 同步；不改变后端返回的中文错误 `message`（前端按 code 本地化，见 D-036-4）。
- 语言选择全局跨工作区；不自动改写会话内容/标题/用户输入；不翻译数据。

### D-036-2 key 组织与 100% 覆盖

- 文案文件：`apps/desktop/src/i18n/zh-CN.json`、`apps/desktop/src/i18n/en-US.json`；扁平 key，命名空间前缀：
  - `app.*`（通用/导航）、`session.*`、`workflow.*`、`subagent.*`、`attachment.*`、`settings.*`、`backup.*`、`audit.*`、`error.<aether_code>.*`、`error.business.<business_code>.*`、`a11y.*`。
- **所有用户可见文案必须走 key**：标题、按钮、占位符、空态、错误、toast、确认框、`aria-label`、`title`、无障碍描述；例外仅：品牌名（Aether）、代码/JSON/路径、日志/诊断原文、用户内容、第三方专有名词。
- `zh-CN` 与 `en-US` key 集合必须完全一致；缺失 key → CI 失败（`pnpm verify:i18n`）；运行期缺失 → 回退 `zh-CN` → key 本身，并显示可见占位（不崩溃、不空白）。
- key 值允许插值 `{name}`、复数（`Intl.PluralRules`）；不在 key 中拼接 HTML；不使用 `dangerouslySetInnerHTML`。
- 组件内禁止硬编码中文/英文文案（静态扫描 + 评审）；错误码/业务码映射必须覆盖 32 + 34（M10 完成后）业务码；未知 code 回退「操作失败（{code}）」。
- 日期/数字/相对时间：`Intl.DateTimeFormat`/`Intl.NumberFormat`/`Intl.RelativeTimeFormat`，按当前 locale；不手写 `YYYY-MM-DD` 拼接（数据展示除外）。
- 不把密钥/内部绝对路径写入文案；诊断/日志不翻译。

### D-036-3 主题范围与 token

- 设置键 `ui.theme`：`"system" | "light" | "dark"`，缺省 `"system"`，≤32 字节；系统模式监听 `prefers-color-scheme`，运行期切换不重启。
- 主题通过 CSS 变量/token 实现：`--color-bg`、`--color-surface`、`--color-text`、`--color-text-muted`、`--color-border`、`--color-accent`、`--color-danger`、`--color-warning`、`--color-success`、`--space-*`、`--radius-*`、`--shadow-*`、`--font-*`；组件不得硬编码十六进制/RGB 色值（静态扫描）。
- `data-theme="light|dark"` 挂在根元素；system 解析后仍写具体 `data-theme` + `data-theme-source="system"`，便于断言。
- 对比度：正文 ≥4.5:1、大字/图形 ≥3:1（WCAG AA）；焦点环可见；不依赖颜色 alone 传达状态（配合文本/图标）。
- 主题不影响布局/锚点；切换后滚动位置/输入草稿保持；不重新加载数据。
- 不新增 UI 依赖/图标库；沿用现有 CSS/组件。

### D-036-4 错误码本地化

- 前端错误展示优先级：`business_code` → `aether_code` → 通用；映射 key `error.business.<business_code>.title/message`、`error.<aether_code>.title/message`。
- 后端 `message` 仅作诊断/未知兜底，不直接展示（可能为中文/英文混合）；未知 code 显示通用文案 + code。
- 新增业务码（M10-00 登记 25 个）必须同时补 zh-CN/en-US 文案；M10-06 验收覆盖全部已登记 code。
- 错误详情/`data` 中的路径/ID 可展示，但不得展示密钥/token（D10）。

### D-036-5 UI-UX v0.18 锚点与文案

- 新增外观设置（S-05 分区/S-15）：`locale-select`（`data-locale="system|zh-CN|en-US"`）、`theme-select`（`data-theme="system|light|dark"`）。
- 新增 P2 文案表（UI-UX §5.4）：25 个业务码 + 2 个警告码 + 工作流/子 Agent/附件/导入关键状态；中英双语 key 一一对应。
- 锚点计数：v0.17 的 22 组 → **29 组**（+7 组：`workflow-list`/`workflow-editor`/`workflow-run-view`/`subagent-tree`/`subagent-result`/`attachment`/`appearance`；共 18 个新 testid）。
- 不重命名既有锚点；新增锚点自 v0.18 冻结，实施不得改名。
- 不改变布局基线（1280×800、min 960×640）；长文案（en-US 通常更长）不得溢出/遮挡；伪语言/长文案测试只用于验证，不进入产品。

### D-036-6 测试与验收

- `pnpm verify:i18n`：
  1. `zh-CN`/`en-US` key 集合一致、无空值、无重复；
  2. 组件/JSX/TS 中用户可见硬编码中文/英文扫描 0 命中（允许白名单：品牌/代码/测试夹具）；
  3. 所有已登记 `aether_code` + `business_code` 有文案；
  4. 主题 token 扫描：组件内无硬编码色值（允许 token 定义文件/测试）。
- 主题矩阵：light/dark/system × zh-CN/en-US 截图/视觉回归；锚点不回归；切换不重启。
- 长文案/伪语言：en-US 200% 长文案不溢出；不要求 RTL。
- E2E：语言切换 → 标题/按钮/错误文案变化；主题切换 → `data-theme`/颜色 token 变化；设置持久化（`settings_get/set`）。
- 性能：语言/主题切换 <100ms（renderer 本地），不触发后端命令。

## 3. 影响

- 《设计文档》v1.31：D7 设置键 9 → 11（`ui.locale`/`ui.theme`）；§4#25 从「P2 不做」转 P2 实现；D7 错误码本地化口径补注。
- P2 计划 v0.9：M10-06 解锁；G12 以中/英 key 100%、主题矩阵、视觉/锚点无回归为验收口径。
- UI-UX v0.18：S-05/S-15 外观设置；§5.4 文案；§7.4 锚点 +1 组；§8 更新 i18n/主题行。
- 不新增命令/事件/迁移；不改变 63 命令语义；不新增依赖。

## 4. 主要风险与缓解

| 风险 | 缓解 | 失效条件 |
|---|---|---|
| 半翻译/硬编码 | key 100% CI + 静态扫描 + 评审 | key 覆盖 <100% → G12 不通过 |
| 长文案布局回归 | en-US 长文案/200% 测试；锚点断言 | 溢出/遮挡 → 修复后重跑 |
| 主题色值蔓延 | token 扫描；禁止组件硬编码 | 出现硬编码色值 → 修复后重跑 |
| 错误码文案缺失 | 32+34 全覆盖脚本；未知 code 兜底 | 缺失 code → G12 不通过 |
| 运行期切换状态丢失 | 切换不重载数据；草稿/滚动保持 | 切换重启/丢草稿 → 修复后重跑 |
| 把密钥/路径写进文案 | 扫描 + 脱敏；文案只含 code/ID | 任一泄漏 → 停止发布 |

## 5. 备选方案（未采纳）

| 方案 | 原因 |
|---|---|
| 引入 i18n 框架/依赖 | 现有 React 上下文足够；避免新依赖 |
| 支持 RTL/更多语言 | P5+；M10 范围冻结为中/英 |
| 用后端 message 直接展示 | 中英混合/不可控；按 code 本地化 |
| 每个组件内联文案对象 | 无法保证 100% 覆盖/扫描 |
| 主题用 CSS-in-JS 运行时 | 增加依赖/性能；token 足够 |

## 6. 契约登记汇总

- 设置键：2（`ui.locale`/`ui.theme`；9 → 11）。
- 命令/事件/迁移：0。
- UI-UX：v0.18；锚点 22 → 29（+`appearance` 组）；§5.4 文案；S-15 外观设置。
- 业务码文案：25 + 既有 9 + IPC 32；警告码 2。
- 不改变 `IpcErrorCode` 32 枚举/EventEnvelope 9 字段。

## 7. 回退条件

1. 本 ADR 被否决 → M10-06 保持「未解锁」；保留中文单语/跟随系统。
2. key 覆盖/主题矩阵/视觉回归任一失败 → 不发布 i18n/主题；修复后重跑 G12；不阻塞后端。
3. 需要新 i18n/主题依赖 → 先查 `deny.toml` 并另立 ADR；不得静默引入。
4. RTL/更多语言需求 → P5+ 另立 ADR；M10 不扩范围。

## 8. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-11-05 | 文档维护（AI） | v1.0 创建 | 按 M10-00 DoD-1/5/7/8；登记先行，代码未改 |
| 2026-11-05 | 独立评审（AI，M10-00 批次） | 通过 | 检查语言/key/回退/主题 token/锚点/错误码覆盖；闭环见 `docs/M10-00-ADR批次评审.md` |

## 9. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v1.0 | 2026-11-05 | 创建：zh-CN/en-US、key/回退/错误码、light/dark/system token、锚点/测试 | 文档维护 |

---

## 附录 A：key 示例

```json
{
  "app.title": "Aether",
  "workflow.list.title": "工作流",
  "workflow.run.cancel": "取消运行",
  "subagent.tree.title": "子 Agent",
  "attachment.add": "添加图片",
  "settings.appearance.locale": "语言",
  "settings.appearance.theme": "主题",
  "error.business.workflow_invalid_definition.title": "工作流定义无效",
  "error.business.workflow_invalid_definition.message": "工作流定义无效：{reason}",
  "a11y.attachment.remove": "移除附件"
}
```

## 附录 B：主题 token（最小集）

| token | light | dark |
|---|---|---|
| `--color-bg` | `#f7f8fa` | `#111318` |
| `--color-surface` | `#ffffff` | `#1a1d24` |
| `--color-text` | `#1b1f27` | `#e8eaed` |
| `--color-text-muted` | `#5c6470` | `#9aa3af` |
| `--color-border` | `#d9dde3` | `#2c313a` |
| `--color-accent` | `#2f6fed` | `#6b9bff` |
| `--color-danger` | `#c62828` | `#ff6b6b` |

> 具体值可在 M10-06 证据中按对比度实测微调（常量级调参，记录证据）；token 名称冻结。
