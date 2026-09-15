# ADR-002：DSH 适配器接入采用 `--patch` 注入 + 带外 delta 通道

| 项 | 内容 |
|---|---|
| 状态 | 已通过 |
| 决策日期 | 2026-09-15 |
| 决策载体 | 《设计文档》v1.1 → v1.2（已落地，见设计文档修订记录）；实施计划 v1.5 → v1.6 同步新增 M2-11 |
| 关联 | 设计文档 D5/D6/D9、实施计划 M2-11（v1.6 新增）、M1-11 接入笔记 |
| 取代 | 无 |
| 被取代 | 无 |
| 回退条件 | 见 §5（4 条触发回退评审） |

## 1. 背景

M1-11 spike 期间对 DSH（DeepSeek Harness）0.1.5-rc.2 做了额外实验。

实测结论：

- 通过 Cordis 插件 + `--patch` 注入（不修改 DSH 源码、不拷贝 hermes 代码），
  可从 `ctx.on('agent/assistant-stream')` 获取与 hermes 消费契约一致的事件流：
  `start` / `chunk(text-delta / reasoning-delta / tool-call-delta)` / `end`。
- 探针实测：738 字的 ACP final 回答对应 77 个 text-delta 帧
  （探针输出共 82 行帧记录，另含 `block-end` / `usage` / `finish` / `end`），
  单帧 1–19 字符、累计 721 字符（与 final 差 17 字符 ≈2.3%，在 §3.6 的 5% 容差内）；
  帧携带 `attemptId` 可归并。
- 探针 fixture 已入库：`scripts/test/m1-11/fixtures/dsh-stream-probe/`；
  证据：`scripts/test/.tmp/m1-11/2026-09-15-dsh-plugin-probe/plugin-probe-run.json`
  （本地证据，`scripts/test/.tmp/` 不入库）。

与 hermes「保持一致」有两种理解：

- 路线 A（事件契约一致）：`--patch` 注入自研 Cordis 插件，
  delta 走带外通道；ACP 主通道仍负责 session / 工具 / 终态；适配器做前缀去重。
- 路线 B（实现细节也一致）：像 hermes 一样 patch `@deepseek-ai/dsh-acp`
  源码接缝，把 delta 塞进自定义 ACP 通知（单通道、顺序严格）。

## 2. 决策

1. **采用路线 A**：`--patch` 注入自研 Cordis 插件，
   delta 走带外通道（session 级 sidecar 文件 / 命名管道 / 本地 socket）；
   ACP 主通道仍负责 session / 工具 / 终态；适配器做前缀去重。
2. **不采用路线 B**：不 patch `@deepseek-ai/dsh-acp` 源码接缝。
   理由：我们本身是 ACP 客户端，没必要为了「单通道」去改上游源码；
   升级回归面小。
3. **权威终态仍取 ACP `session/prompt` settle**；delta 仅作增量呈现。
   与 hermes「原生流权威 + 去重」的原则一致。
4. **DSH 优先级不变**：仍低于 Codex / Claude Code；
   本 ADR 不改变 §2.2 适配器路线图的顺序。

## 3. 硬约束（缺一不可）

以下约束写入 M2-11 的 DoD，任何一条不满足即视为不满足 ADR。

### 3.1 版本门闩

- pin DSH 0.1.5-rc.2（本机实测版本）
- 启动探针校验帧形状；不匹配 → 拒绝加载
- 错误处理类比 D6 major 校验：
  `disabled` + `status_reason=version_mismatch` + 升级提示。
  D5 现有 `status_reason` 词典为 `handshake_timeout` / `start_failed` / `crash_loop` / `untrusted`，
  需随本 ADR 通过在 D5 与附录 C 注释中追加本取值；
  该列为 TEXT、无 CHECK 枚举约束，不涉及 DDL 迁移。

### 3.2 注入细节

- 插件包必须放 `profiles/<profile>/node_modules`（共享 fallback 无效）
- `package.json` 必须无 BOM
  （PS 5.1 的 `Set-Content -Encoding utf8` 加 BOM 会让 DSH 直接崩，已踩过）
- 建议用 Node 的 `fs.writeFileSync` 生成文件，不用 PowerShell

### 3.3 去重规则

- 官方 committed 消息与插件 delta 做前缀消费
- 保留 final-only 后缀
- 与 hermes 同款思路，避免 UI 重复

### 3.4 权限映射

- `session/request_permission` 按 D9 边界映射为预授权策略
- **不得假装回环、不得绕过 permission.request**
- 工具调用 100% 经权限门（复用 M2-10 探针验证）

### 3.5 合规（A11）

- 参考思路可以，**不能拷代码**
- 使用 DSH 内部事件契约必须写进任务证据 / 本 ADR
- 不修改 DSH 源码，不依赖未公开 API 的非注入方式

### 3.6 delta 丢失兜底

- 带外通道不做有序性保证，靠 ACP final 兜底重建
- 若 T 秒内未收到 end 事件，或 delta 总长度与 ACP final 偏差 >5%
  → 从 ACP final 回退重建

### 3.7 多 profile

- MVP 仅支持默认 profile；多 profile 场景在 P1 评估

### 3.8 通道清理

- 会话 dispose 后，带外通道 5s 内清理完毕
- 残留通道数 = 0（探针断言）

## 4. 影响

### 4.1 设计文档

- D5 与 D6 各追加「DSH 适配器增强机制」小节并互相交叉引用（D5 → D6/D9、D6 → D5/D9；已随 v1.1 → v1.2 落地）
- §2.2 适配器路线图：DSH 从「备选候选」标注为「已验证候选（ADR-002）」
- 修订记录追加一行（v1.1 → v1.2）

### 4.2 实施计划

- 新增任务 M2-11「DSH 适配器增强」
- §1.1 依赖全景：M2-11 与 M2-02/M2-03 并行
- §1.2 关键路径：M2-11 不在关键路径上
- §1.3 可并行分组：G7 追加 M2-11
- §1.4 门禁一览：**M2-11 不纳入 Gate 2**
- §1 任务总量汇总：M2 10 项 → 11 项、总计 34 项 → 35 项
- §7 映射表：追加 M2-11
- 修订记录追加一行（v1.5 → v1.6）

### 4.3 不影响的

- M1-11 的 DoD 不变（仍只验证 Codex / Claude Code）
- Gate 1 的通过条件不变
- 设计文档 §2.2 的适配器优先级顺序不变

## 5. 回退条件

满足任一条件即触发本 ADR 的回退评审：

1. **DSH 主版本升级导致帧形状不匹配**，且探针无法适配
   → DSH 适配器降级为「不接入」或改用 PTY + 屏幕解析（A1 降级路径 2 原文）
2. **DSH 官方废弃 `agent/assistant-stream` 事件或 Cordis 插件机制**
   → 同 1
3. **带外通道的帧级丢失率 >1% 或平均长度偏差 >5%**（20 次回归中的实测值）
   → 评估双通道或二进制帧方案；若仍不可行则回退到 ACP 单通道
   （牺牲增量呈现，等 final 到达后一次性渲染）
4. **合规审查发现代码相似度过高**
   → 对应模块重写；重写后仍不通过则放弃本机制

## 6. 证据与参考

- 探针 fixture（入库）：`scripts/test/m1-11/fixtures/dsh-stream-probe/`（package.json / index.js / patch.yml）
- 证据（本地，不入库）：`scripts/test/.tmp/m1-11/2026-09-15-dsh-plugin-probe/plugin-probe-run.json`
- 接入笔记：`docs/spike/M1-11-接入笔记.md`
  - 「DSH 适配器增强可行性」（对应本 ADR §2）
  - 坑 16：DSH ACP 只在 0.1.5+（对应 §3.1 版本门闩）
  - 坑 18：ACP 生命周期——`session/prompt` settle 才代表终态；
    `session/request_permission` 必须应答（对应 §2.3/§3.4）
  - 坑 19：插件注入两个硬要求——① 插件包必须放 profile 的
    `node_modules`（共享 fallback 无效）；② `package.json` 必须无 BOM
    （对应 §3.2）

## 7. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-09-15 | pymu | 通过 | 采纳路线 A 与硬约束 §3.1–§3.8；D5/D6 小节（设计文档 v1.2）与 M2-11（实施计划 v1.6）已落地；按 §5 回退条件复核 |

## 8. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v0.1 | 2026-09-15 | 草案 | （待填） |
| v0.2 | 2026-09-15 | 校对修正：状态/载体行补齐；探针数据与 artifact 对齐（77 text-delta 帧 / 累计 721 字符）；证据路径补全并标注本地未入库；坑引用编号修正；升版号修正（设计文档 v1.2、计划 v1.6）；补任务总量汇总影响 | （待填） |
| v0.3 | 2026-09-15 | 落地 §4.1：设计文档 D5/D6 各追加「DSH 适配器增强机制」小节（v1.1 → v1.2），`status_reason=version_mismatch` 同步 D5 词典与附录 C 注释；状态仍为草案（待评审） | （待填） |
| v0.4 | 2026-09-15 | 评审通过：状态改「已通过」；补评审记录；实施计划 v1.5 → v1.6 新增 M2-11（§1.1–§1.4/汇总/§7 同步，不纳入 Gate 2） | pymu |
| v0.5 | 2026-09-15 | 评审修订 §5.3：回退指标改为「帧级丢失率 >1% 或平均长度偏差 >5%」（§3.6 单次 5% 长度偏差阈值保留）；实施计划 v1.7 同步 M2-11 DoD7 | pymu |
