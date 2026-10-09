# ADR-016：事件监听最小能力补齐（M4-05 E2E 缺陷修复登记；不改设计）

| 项 | 内容 |
|---|---|
| 编号 | ADR-016 |
| 版本 | **v0.5（已批准；评审链结案）** |
| 状态 | 已批准（评审链：首轮「有条件照准」→ 第二轮「附条件已闭环」→ 第三轮归档复核「通过（闭环）」→ 第四轮结案；修订随 M4-05 本批次落地；评审记录见 §6） |
| 决策日期 | 2026-10-08 |
| 关联 | 设计 D7（事件通道：单通道 `aether://event`；Tauri 安全基线 capabilities 最小 allowlist）、M1-08 DoD4、M3-01（事件桥）、M4-05（真实 WebView 内联回环 E2E）、`docs/ADR-016-评审意见.md`（首轮评审）、`docs/ADR-016-复审意见.md`（第二轮复审）、`docs/ADR-016-第三轮复审意见.md`（归档复核）、`docs/ADR-016-第四轮复审意见.md`（结案） |
| 取代 | 无 |
| 被取代 | 无 |
| 范围对价 | 无（缺陷修复：补齐实现设计 D7 所必需的最小权限；非新增能力） |

## 1. 背景与事实

M4-05「真实 WebView 内联权限回环 E2E」首次以真实 WebView2 驱动 UI 事件链路，
发现 **`aether://event` 通道在生产/调试 WebView 中不可用**：

- 前端 `useAetherEventBridge`（M3-01 交付）经生成绑定 `events.aetherEvent.listen`
  注册监听；真实环境下注册失败，`EventBridgeIndicator` 呈现「不可用」；
- 探针直接调用 `plugin:event|listen` 得到宿主返回：
  `event.listen not allowed. Permissions associated with this command:
  core:event:allow-listen, core:event:default`；
- 根因：M1-08 签入的 `capabilities/default.json` 权限集为 `[]`（当时 UI 不消费
  任何插件/核心命令），而 D7 明确要求 UI 经单通道 `aether://event` 接收事件；
  M3-01 的验证均为 Rust 侧 `RecordingSink`/Mock 应用冒烟，未覆盖真实 WebView，
  故该缺口在 M4-05 前未被发现。事件推送缺失导致审批卡不渲染、UI 不实时刷新
  （`permissions_pending` 数据正确，属纯展示链路缺陷）。

## 2. 决策

在**不改变任何设计语义**前提下，补齐事件监听所需的最小权限：

1. `crates/aether-tauri/capabilities/default.json` 的 `permissions` 增
   `core:event:allow-listen` 与 `core:event:allow-unlisten`（仅监听/解除监听；
   **不**授予 `allow-emit` / `allow-emit-to`，前端不向前端发事件）；
2. `crates/aether-tauri/src/config.rs` 与 `tests/security_baseline.rs` 的
   `CAPABILITY_PERMISSION_ALLOWLIST` 同步为上述两项（静态断言保持「清单 ↔
   磁盘文件」双向一致）；
3. CSP、`withGlobalTauri:false`、导航拦截、命令参数校验框架均不变；
4. 本 ADR 为**缺陷修复登记**（不改设计决策、不升设计文档版本）；M4-05 证据与
   Gate 4 报告登记该修正与验证结果。

## 3. 影响

- 安全面：新增权限为 Tauri 核心事件插件的监听对（最小可用集）；不引入远程来源、
  不改变窗口裁剪（仅 `main` 窗口）、不新增插件。（范围事实登记）capability 授权粒度为
  命令级：`core:event:allow-listen` 覆盖 `plugin:event|listen` 对任意事件名的调用，
  Tauri 不提供按事件名裁剪；在本地源内容 + 冻结 CSP + 单窗口约束下，残余信息面风险低，
  评审裁定接受为 P0 最小集。
- 行为面：`aether://event` 通道恢复为设计规定的实时推送；审批卡/会话分组/消息流
  等事件驱动 UI 恢复实时性（数据链路（命令轮询）本已正常）。
- 测试面：M1-08 安全基线单测更新为新的最小 allowlist；M4-05 E2E 新增真实
  WebView 事件通道断言（监听注册成功 + **解除监听成功（运行时覆盖，评审意见 §6-B1 首选已实施）**
  + 内联权限回环完成）。
- 回退：若评审否决本修正，则 M4-05 无法通过（D7 通道不可用）；按 AGENTS §7
  停止并回报，不得以「UI 降级为轮询」静默替代冻结的单通道设计。

## 4. 验证

- `cargo test -p aether-tauri --test security_baseline`（capabilities 最小
  allowlist 双向一致）；
- `node scripts/test/m4-05/e2e-inline-permission-loop.mjs`（真实 WebView：
  `event-listen-probe ok=true` + `event-unlisten-probe ok=true`（评审意见 §6-B1 首选）→
  审批卡渲染 → 允许 → 适配器回执 → run 终态）；
- M1-08 E2E（CSP/导航）回归不受影响。
- T2 口径登记：需求「事件端到端回显 P95<150ms」的 P0 验收测点为管线级广播（UI 事件桥同源；`crates/aether-tauri/tests/m4_04_t1_t3.rs:243-254`；M4-04 报告 T2 实测 34ms）；真实 WebView 投递段由本任务 E2E 功能性覆盖（渲染完成即通过），不另设 P0 时延门槛；证据落点：`docs/M4-05-证据.md` §2、`docs/M4-Gate4-验收报告.md` §6。

## 5. 变更记录

| 版本 | 日期 | 变更 |
|---|---|---|
| v0.1 | 2026-10-08 | 创建：M4-05 E2E 缺陷（事件监听权限缺失）登记与最小权限修复 |
| v0.2 | 2026-10-09 | 首轮评审修订：关联行 D1→D7；§3 补权限范围事实；§4 补 T2 口径与证据落点；新增 §6 评审记录；状态转已批准 |
| v0.3 | 2026-10-09 | 第二轮复审归档（「附条件已闭环」）落文：状态行补第二轮复审结论；§6 增第二轮复审行；关联行补复审意见文件引用；§3/§4 引用措辞按评审意见编号修正（修订依据：`docs/ADR-016-第三轮复审意见.md` 归档复核 §2-M1） |
| v0.4 | 2026-10-09 | 第三轮归档复核「通过（闭环）」记录：状态行与 §6 评审记录同步、版本升号；评审链归档（不改决策） |
| v0.5 | 2026-10-09 | 第四轮结案复核记录：M1/M2 全部关闭、评审链结案；关联行补第四轮引用；§6 增结案行（不改决策） |

## 6. 评审记录

| 日期 | 评审 | 结论 | 备注 |
|---|---|---|---|
| 2026-10-09 | 首轮评审 | **有条件照准**（1 P1 + 5 P2；详见 `docs/ADR-016-评审意见.md`） | 裁定：T2 管线级口径接受（不补测）；`allow-listen` 全事件名范围接受；「登记→修复→评审」存量时序经评审追认（缺陷修复属恢复 D7 规定状态，E2E 证据依赖修复方可执行）；条件 = 本意见 §6-A 修订落文 |
| 2026-10-09 | 第二轮复审 | **通过（附条件已闭环）**——首轮 1 P1 + 5 P2 全部关闭；新增 3 微项（非阻塞；见 `docs/ADR-016-复审意见.md` §3） | 独立复跑：security_baseline 6/6、E2E 10/10（含 listen/unlisten 探针） |
| 2026-10-09 | 第三轮归档复核 | **通过（闭环）**——上轮 3 微项 + 4 项归档动作全部落实；新增 2 微项 M1/M2（非阻塞，已随批处理） | 代码未动确认（探针/E2E/能力清单/基线单测 mtime 未变），上轮独立复跑结论维持有效；评审链归档（`docs/ADR-016-第三轮复审意见.md`） |
| 2026-10-09 | 第四轮结案复核 | **M1/M2 全部关闭；评审链结案**（可选备注 O1 已由 v0.4 §6 第三轮行满足） | 无未决事项；`docs/ADR-016-第四轮复审意见.md`；不再开复审轮次 |
