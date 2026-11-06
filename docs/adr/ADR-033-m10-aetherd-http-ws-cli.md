# ADR-033：M10 `aetherd` HTTP/WS/CLI API、鉴权、端口发现与版本化

| 项 | 内容 |
|---|---|
| 编号 | ADR-033（M10-00-03） |
| 版本 | **v1.1（2026-11-05，M10 方案决策记录 v0.5 合入）**；v1.0 = M10-00 初始登记 |
| 状态 | **已接受 v1.1（2026-11-05，M10-00 ADR 批次 + 用户决策记录 v0.5；用户授权 AI 审批；人工复核建议见 `docs/M10-00-ADR批次评审.md`）** |
| 决策日期 | 2026-11-05 |
| 决策载体 | 《设计文档》**v1.32**（D7/D11、附录 B/C）；`docs/P2-架构迁移-实施计划与验收标准.md` **v0.10**；`docs/M10-00-登记表.json`；`docs/M10-方案决策记录与修订建议.md` v0.5 |
| 关联 | 设计 §5 P2、D11；ADR-029（stdio 为 UI 通道）、ADR-030（发布基线）、ADR-031/032/034/035/037（新命令增量接入） |
| 取代 / 被取代 | 无（新增）；**不替换** stdio UI 通道，HTTP/WS/CLI 为并行 API |
| 回退条件 | 见 §7 |
| 实现状态 | **未开始（登记先行；M10-03 执行）**；未登记前不得写业务代码 |

## 1. 背景与触发

M10-03 要求在 `aetherd` 上新增本地 HTTP/WS API 与 CLI，外部客户端可用同一业务核心；设计 D11 已把「aetherd 单承载 + 多客户端」列为 P2 决策变更。M9 已冻结 stdio JSON-RPC 2.0/JSONL 为 UI 通道（63 命令/32 枚举/事件 9 字段）；M10-03 必须在不改变该通道与业务语义的前提下，冻结监听、鉴权、端口/发现、版本化、错误映射、背压、CLI 与一致性验收。

## 2. 决策

### D-033-1 监听与绑定（v1.1：默认关闭）

- **`aetherd --stdio` 默认不开启 HTTP**（UI 主路径，减小本地攻击面）；需要外部 API 时显式 `--stdio --http` 或 `AETHERD_HTTP=1`。
- **`aetherd --http-only`**：开启 HTTP 且**必须成功**（headless/CLI/外部消费者）；listener 失败 → 致命退出码 **25**；仍走同一 `.aetherd.lock`/数据目录，禁止双 daemon。
- `AETHERD_HTTP=0` 强制关闭；与 `--http-only` 冲突 → 参数错误（退出码 10）。
- `AETHERD_HTTP_REQUIRED=1` 仅在 HTTP 已开启时有意义（测试/CI）：listener 失败致命；否则 `--stdio --http` listener 失败不致命，`health.api.state="error"`，stdio 继续。
- 默认仅绑定 **`127.0.0.1`**；IPv6 默认 **`::1`** 仅在显式 `AETHERD_HTTP_BIND=::1` 时启用；禁止默认 `0.0.0.0`/`::`。
- 默认端口 **`0`（随机空闲端口）**；可选固定端口 `AETHERD_HTTP_PORT=1…65535`。端口占用 → 明确错误 `EADDRINUSE`，不静默换端口（随机端口模式除外，重试 ≤3 次）。
- 远程绑定：仅当显式 `AETHERD_HTTP_BIND` 为非 loopback **且** `AETHERD_ALLOW_REMOTE=1`；**M10 不实现远程绑定**，需另立 ADR；默认拒绝。
- **仅 HTTP 开启时写 `.aetherd-api.json`**；关闭时删除；`--stdio` 默认不产生发现文件。
- 优雅关闭：先停止接受新连接（≤1s），等待在途请求（≤3s），关闭 WS（1012），删除发现文件，最后随 daemon 关闭序列退出；不阻塞 stdio 关闭预算（ADR-029 ≤8.2s）。
- 信号：`--http-only` 处理 SIGINT/SIGTERM；**Windows 使用 Tokio `tokio::signal::windows` 的 `ctrl_c`/`ctrl_break`/`ctrl_close`/`ctrl_logoff`/`ctrl_shutdown`**，覆盖 `CTRL_CLOSE_EVENT`/注销/关机；`ctrl_close` 触发优雅关闭（停止接受 ≤1s、WS 1012、在途 ≤3s、删 discovery/释放锁），Windows console close 约 5s 预算按 best-effort 记录；**不引入 `unsafe`/`windows-sys`**；父进程死亡由 stdin EOF/Job Object 回收。

### D-033-2 鉴权与发现

- 启动时生成 **256-bit 随机 token**（`getrandom =0.3.4`，64 hex），仅驻留内存；不写日志、不进 URL、不出现在错误消息/响应/诊断包。
- **发现文件**：`<data_dir>/.aetherd-api.json`，内容：
  ```json
  { "v": 1, "pid": 1234, "port": 43123, "bind": "127.0.0.1", "token": "<64hex>", "started_at": 1762300000000, "protocol": "1.0" }
  ```
  文件权限：Unix `0600`（目录 `0700`）；Windows 使用当前用户 DACL（拒绝继承/其他用户）。发现文件在 daemon 关闭时删除；崩溃残留由下次启动覆盖（含 pid 校验）。
- 客户端优先级：`Authorization: Bearer <token>` 头（HTTP）> `AETHERD_API_TOKEN` 环境变量（CLI/CI）> 发现文件（同用户）。token 轮换：`POST /v1/admin/rotate-token`（需当前 token）原子替换内存 token 并返回新 token（仅响应体，不记日志）；旧 token 立即失效；发现文件同步更新。
- WS 鉴权：连接后 **5s 内**发送首帧 `{"type":"auth","token":"<token>"}`；超时/错误 → close code **4401**；token 不放在 URL/query/subprotocol。认证成功前不接受订阅/请求。
- CORS：默认不返回 `Access-Control-Allow-Origin`；预检 `OPTIONS` 返回 403（除非显式 `AETHERD_HTTP_CORS_ORIGIN` 单一来源，仅测试用；生产默认拒绝）。
- 速率/连接：每 token 100 req/s（令牌桶，突发 200）；最大并发连接 64；超过 → HTTP 429 / WS close 1013；连接空闲 60s 关闭。
- 请求日志脱敏：不记录 `Authorization`、token、请求体中的 `api_key`/`passphrase`；路径中的 `session_id` 可记；错误日志只记 method/status/耗时/request-id。

### D-033-3 API 契约（版本化 `/v1`）

- **HTTP RPC**：`POST /v1/rpc`，JSON-RPC 2.0 单请求（**不支持 batch**，避免放大）；body ≤ **2 MiB**；响应 ≤2 MiB；`Content-Type: application/json`；请求 id 由客户端提供，响应回显；`X-Aether-Request-Id` 可选（仅日志关联）。
- **方法映射**：HTTP `method` = 已登记 IPC 命令名（63 + M10 增量，最终 87）；`params` = 内层 DTO（与 stdio 完全一致，无 `payload` 包装）；未知方法 `-32601`；参数错误 `-32602` + `error.data.aether_code`；业务错误 `-32603` + `error.data.aether_code` + `business_code`。
- **超时**：每个命令沿用登记超时（见 `docs/M10-00-登记表.json`）；HTTP 层硬超时 = 命令超时 + 5s；超时返回 `-32603` + `aether_code=internal` + `business_code=request_timeout`（新增，仅传输层业务码；不进入 IPC 枚举）。
- **取消**：`POST /v1/rpc/cancel` `{request_id}` 或 JSON-RPC `$/cancelRequest` 通知；仅取消在途请求，不改变已落库状态；取消未知 id → 204。
- **发现/元数据**：`GET /v1/meta`（需 token）返回 `{protocol, app_version, daemon_version, commands: [{name, timeout_ms}], capabilities}`；**只列已实现命令**（M10-03 时为当时已发布命令，最终 G10 覆盖 87）；不含内部路径/密钥；`GET /v1/health` 返回 `HealthReport`（同 `health` 命令，需 token）。
- **OpenAPI/JSON Schema（v1.1）**：`docs/api/aetherd-v1.openapi.json` + `docs/api/schemas/*.json`；`scripts/ci/api-schema.mjs` 从 `command-manifest.json` 取已实现命令，**字段级校验**请求/响应 schema（类型/必填/枚举/长度/分页字段），并校验命令覆盖、无未登记命令、无缺失 DTO；CI diff=0；不手改生成物。
- **事件 WS**：`GET /v1/events` 升级 WebSocket；首帧 auth 后客户端发送 `{"type":"subscribe","session_id?":"<ULID>","last_seq?":123}`；服务端发送 `{"type":"event","envelope":{...9 字段...}}`；`seq`/`last_seq` 补读与 stdio 同口径；控制事件不丢，delta 可丢并发送 `{"type":"gap","session_id":"...","from_seq":...}`；慢消费者队列 >4096 或 >32MiB → close 1013，客户端用 `last_seq` 重连；不支持二进制帧。
- **错误结构**：与 stdio 相同 `error.data`；HTTP 状态码仅传输层：200（JSON-RPC 成功/业务错误均在 body）、400（畸形 JSON）、401（无/错 token）、403（CORS/远程绑定拒绝）、404（未知路径）、413（body 超限）、429（限流）、503（daemon 未就绪）。业务错误不映射为 4xx/5xx，保持与 stdio 一致。
- **背压**：HTTP 请求并发 ≤64；单连接串行处理；WS 事件队列有界；不回压核心 reader（复用 D8 journal 补读）。
- **不泄露**：响应/日志/WS 不含 `api_key`/token/内部绝对路径（允许数据目录逻辑标识，不返回绝对路径）；`/v1/meta` 不返回命令实现细节。

### D-033-4 CLI（`aether` / `aetherd --cli`）

- 独立二进制 `aether`（或 `aetherd --cli` 子命令），**headless 不依赖 Electron/Node**；通过 stdio（默认，若由 daemon spawn）或 HTTP（`--url`/发现文件）调用同一命令。
- 最小命令集：
  - `aether session list|create|send|messages|interrupt`
  - `aether workflow run|status|cancel|retry-node`
  - `aether subagent tree <parent> | result --parent <parent> --child <child> | resolve`
  - `aether import <file>|export <session>`
  - `aether health`
  - `aether events --session <id> [--last-seq N]`（**原生 WS**，JSONL 流）
  - `aether api rotate-token`
- `aether events` 使用 `tokio-tungstenite =0.29.0` 原生 WS：连接 `ws://127.0.0.1:<port>/v1/events`，首帧 auth，subscribe，JSONL 输出；处理 gap/1013/1012/4401，断线用 `last_seq` 重连；**不使用 `messages_page` 轮询**。
- 输出：默认紧凑 JSON（一行一结果）；`--pretty` 美化；`events` 输出 JSONL；错误输出 `{error:{code,message,data}}` 到 stderr。
- 退出码：`0` 成功；`1` 内部错误；`2` 用法/参数；`3` 未找到；`4` 鉴权失败；`5` 服务不可用/未就绪；`6` 超时；`7` 取消。
- `aether --help`/子命令 `--help` 必须列出参数、示例、退出码；`--version` 输出 app/protocol/daemon 版本；`--http`/`--http-only`/`--no-http` 帮助与 daemon 一致。
- CLI 不读取/打印 token；从 `AETHERD_API_TOKEN` 或发现文件读取；`--token-file` 可选（0600 校验）。

### D-033-5 多客户端一致性与单写者

- 所有传输（stdio/HTTP/WS/CLI）调用同一 `aether-app::dispatch`，不复制业务逻辑；不持有独立会话/运行/权限状态。
- 一致性验收：对代表性命令（`health`、`session_list`、`session_create`、`session_send`、`messages_page`、`workflow_run_get`、`queue_list`）在 stdio/HTTP/WS/CLI 四通道做结构化对照，**0 差异**（排除请求 id/token/时间戳/耗时）；事件流 `seq`/`last_seq` 对照 0 差异。
- 并发客户端：10 个客户端同时请求，互不串流；seq 单调；单写者/写队列不变；`session_busy`/队列语义与 stdio 相同。
- 命令覆盖：M10-03 完成时覆盖当时已发布的全部命令；G10 最终覆盖 **87**（63 M9 + 24 M10）；未实现的新命令不得在 `/v1/meta` 中虚报。

### D-033-6 安全与运维

- 默认 loopback + token + CORS 拒绝 + body/连接/速率上限；远程绑定需显式配置 + 单独 ADR；M10 不实现 TLS（本地 loopback 明文，token 防误用；TLS/远程在 P3+ 另立 ADR）。
- 端口冲突：固定端口模式返回明确错误（`api.port_in_use` + 端口号），不静默换端口；随机端口模式重试 ≤3 次。
- 发现文件残留：启动时若发现文件存在且 pid 不存活/端口不匹配 → 覆盖并记日志；不删除他人文件。
- 诊断：`health.api` = `{state:"disabled|listening|error", port?, since?, last_error?}`；不返回 token。
- 关闭：停止监听、关闭连接、删除发现文件；在 daemon 关闭预算内完成（≤1s 停止接受 + ≤3s 在途）。
- **依赖（v1.1）**：`axum =0.8.9`（MIT，MSRV 1.80）、`tokio-tungstenite =0.29.0`（MIT，CLI WS）、`ureq =3.4.2`（CLI RPC，`default-features=false`）、`getrandom =0.3.4`（MIT/Apache-2.0）、`futures-util`、`hex`、`serde`；**workspace `rust-version` 由 1.80 升 1.85**（`ureq 3.4.2` MSRV 1.85）。`cargo deny`/`pnpm check:licenses` 必须通过；精确版本锁定；新增依赖先查 `deny.toml`。

## 3. 影响

- 《设计文档》v1.32：D11 补 HTTP/WS/CLI 监听/鉴权/发现/版本化与默认关闭策略；D7 明确 stdio 仍为 UI 专用通道，API 并行新增；D8 背压复用；附录 B 无新增事件类型（WS 传输同一事件流）。
- P2 计划 v0.10：M10-03 解锁；G10 以鉴权/loopback/87 命令一致/四通道 0 差异/外部消费者为验收口径。
- UI-UX v0.19：不新增主 UI 锚点；`health.api` 仅诊断/关于页可选展示；无画布。
- 命令面：**不新增业务命令**（0）；API 端点/CLI 不作为 IPC 命令计数；`IpcErrorCode` 32 不变；新增传输层业务码 `request_timeout`（不进入 IPC 枚举）。
- 迁移/设置键：0。

## 4. 主要风险与缓解

| 风险 | 缓解 | 失效条件 |
|---|---|---|
| token 泄漏 | 内存 token；发现文件 0600/DACL；不记日志/URL；轮换 | 任一泄漏路径 → 停止 API 发布 |
| 本地攻击面 | loopback + token + CORS 拒绝 + body/连接/速率上限；远程需 ADR | 无 token 请求成功 → G10 不通过 |
| 多客户端状态不一致 | 全部走同一 dispatch；单写者/seq；四通道对照 | 出现串流/seq 跳变 → 冻结 M10-03 |
| 端口/发现文件冲突 | 固定端口明确报错；残留 pid 校验；随机端口重试 | 静默换端口/覆盖他人文件 → 修复后重跑 |
| 背压拖垮核心 | WS 有界队列 + gap/close；journal 补读；不回压 reader | 慢客户端导致控制事件丢失 → G10 不通过 |
| 新依赖引入 | 先查 deny.toml；需要则另立 ADR | 未批准依赖 → 不合入 |

## 5. 备选方案（未采纳）

| 方案 | 原因 |
|---|---|
| 用 HTTP/WS 替换 stdio | 违反 ADR-029/M9 冻结；UI 通道不回退 |
| 默认绑定 `0.0.0.0` | 攻击面不可接受；远程需 ADR |
| token 放 URL/query | 易进日志/历史/代理；明确禁止 |
| 默认 CORS `*` | 浏览器侧跨站风险；默认拒绝 |
| 引入 GraphQL/gRPC | 超范围，增加依赖与契约面；JSON-RPC 已统一 |
| CLI 直接访问 SQLite | 破坏单写者/权限/审计；禁止 |

## 6. 契约登记汇总

- 新增业务命令：0；API 端点：`POST /v1/rpc`、`POST /v1/rpc/cancel`、`GET /v1/meta`、`GET /v1/health`、`GET /v1/events`（WS）、`POST /v1/admin/rotate-token`。
- CLI 命令组：session/workflow/subagent/import/export/health/events/api；`events` 原生 WS。
- 事件：0 新增（WS 传输同一 `aether.event` 流）。
- 设置键：0；迁移：0。
- 传输层业务码：`request_timeout`（仅 HTTP/CLI，不进入 IPC 枚举）。
- 退出码：25（`--http-only` 或 `AETHERD_HTTP_REQUIRED=1` 监听失败）；CLI 0–7。
- 依赖：axum 0.8.9 / tokio-tungstenite 0.29.0 / ureq 3.4.2 / getrandom 0.3.4 / futures-util / hex / serde；workspace rust-version 1.85。

## 7. 回退条件

1. 本 ADR 被否决 → M10-03 保持「未解锁」；不实现 listener/CLI；stdio 仍为唯一正式通道。
2. 鉴权/loopback/一致性任一失败 → 不发布 HTTP/WS/CLI；stdio 不受影响；修复后重跑 G10。
3. 需要新依赖（HTTP/WS crate）而未获批准 → 停止实现，另立依赖 ADR；不得静默引入。
4. 远程绑定需求出现 → 停止扩展，另立 ADR；M10 不实现。

## 8. 评审记录

| 日期 | 评审人 | 结论 | 备注 |
|---|---|---|---|
| 2026-11-05 | 文档维护（AI） | v1.0 创建 | 按 M10-00 DoD-1/2/3/7/8；登记先行，代码未改 |
| 2026-11-05 | 独立评审（AI，M10-00 批次） | 通过 | 检查 loopback/token/发现文件/CORS/body 上限/四通道一致性；闭环见 `docs/M10-00-ADR批次评审.md` |
| 2026-11-05 | 用户决策记录 v0.5 | **v1.1 通过** | Q4 HTTP 默认关闭、Q8 原生 WS、Q2 87、axum 0.8.9/tokio-tungstenite 0.29.0、OpenAPI 字段级；见 `docs/M10-方案决策记录与修订建议.md` |

## 9. 变更记录

| 版本 | 日期 | 变更 | 作者 |
|---|---|---|---|
| v1.0 | 2026-11-05 | 创建：监听/鉴权/发现/API/WS/CLI/一致性/安全/回退 | 文档维护 |
| v1.1 | 2026-11-05 | 用户决策记录 v0.5：HTTP 默认关闭、`--http-only`/`--stdio --http`/`--no-http`；87；原生 WS（tokio-tungstenite 0.29.0）；axum 0.8.9；OpenAPI 字段级校验；rust-version 1.85 | 文档维护 |
| v1.1 修订 | 2026-11-05 | 决策记录 v0.7：最终计数 87 文本统一；discovery token 示例 64hex；Windows 使用 Tokio signal API，不引入 unsafe/windows-sys | 文档维护 |

---

## 附录 A：HTTP/WS 错误映射

| 场景 | HTTP | JSON-RPC/WS | data |
|---|---|---|---|
| 畸形 JSON | 400 | `-32700` | — |
| 无/错 token | 401 | — | — |
| CORS/远程拒绝 | 403 | — | — |
| body >2MiB | 413 | `-32600` | `aether_code=too_large` |
| 限流 | 429 | — | `Retry-After` |
| 未知方法 | 200 | `-32601` | `aether_code=not_implemented` |
| 参数错误 | 200 | `-32602` | `aether_code=invalid_*` |
| 业务错误 | 200 | `-32603` | `aether_code` + `business_code` |
| 请求超时 | 200 | `-32603` | `business_code=request_timeout` |
| daemon 未就绪 | 503 | `-32603` | `aether_code=core_not_ready` |

## 附录 B：发现文件与 CLI 示例

```json
{ "v": 1, "pid": 1234, "port": 43123, "bind": "127.0.0.1", "token": "***", "started_at": 1762300000000, "protocol": "1.0" }
```

```bash
aether health --url http://127.0.0.1:43123 --token-file ~/.aether/api-token
aether session create --runtime codex --provider <id> --model <model> --title "review"
aether workflow run <workflow_id> --session <session_id> --json
aether events --session <session_id> --last-seq 120
```
