# M7-10 共享凭据 Broker token 正式签署

| 项 | 内容 |
|---|---|
| 任务 | M7-10（ADR-028 v0.9） |
| 日期 | 2026-10-23 |
| 签署人 | 项目负责人（用户；M7-10 验收指令） |
| 范围 | 应用级共享 token（本地 Credential Broker，`AETHER_CREDENTIAL_BROKER`） |
| 结论 | **正式签署接受**（P1 正式发布可用；不转 per-adapter token ADR） |
| 载体 | `docs/adr/ADR-028-provider-catalog-credential-broker.md` §2.3、§9；`docs/M7-10-证据.md` |

## 1. 被签署对象

ADR-028 v0.9 的 Credential Broker 采用**应用级共享 token**：

- 应用启动时随机生成一次 token（同一次启动内连接复用），不落盘、不进日志/诊断包/导出/测试夹具；
- broker 仅本机、仅当前用户（Windows 当前用户 DACL / Unix `0700` 目录 + socket 权限）；
- 仅三个官方 runtime label 可请求；broker 校验 `runtime_id` 必填、runtime → protocol、
  provider → protocol 双重兼容；
- 每次 run 按 `provider_id + protocol` 现取密钥，broker 与适配器均不缓存；
- 所有官方适配器的 CLI/工具子进程统一剥离 `AETHER_CREDENTIAL_BROKER`（共享 `stripBrokerEnv`）。

## 2. 已接受残余风险（明示）

- 同一用户下的恶意进程、或被篡改的官方适配器进程，可读取 token 并向 broker 请求**任意**
  provider 的凭据；
- `runtime_id` 由客户端自报，仅作标签与范围收窄，**不构成强认证**；
- 上述风险在 P1 内以“本机单用户 + 官方适配器可信 + 子进程剥离 + 最小权限读取”作为补偿，
  不宣称“密钥不泄露”为无条件保证。

## 3. 触发重新评审的条件

出现以下任一情形，必须另立 ADR（不得在 ADR-028 内静默扩展）：

1. 接入第三方适配器（P3）；
2. 安全评审或产品负责人要求 per-adapter token；
3. 出现 token 越权/密钥泄露事件；
4. 需要 broker 持久审计（当前仅 tracing，不落库、不作为合规审计）。

## 4. 签署声明

本人已知悉上述残余风险与补偿措施，代表产品/安全职责**正式接受** ADR-028 v0.9 的应用级
共享 token 方案用于 P1 正式发布；若后续触发 §3 条件，按变更控制另立 ADR。

- 签署：项目负责人（用户）
- 日期：2026-10-23
- 关联验收指令：M7-10 后续验收（“共享 token 正式签署或转 per-adapter ADR”）
