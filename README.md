# Aether

本地优先、可扩展的统一多 Agent 编排桌面平台（Electron 壳 + Rust `aetherd` 核心 + TypeScript 适配器 + React UI；ADR-029）。

- 基线：《设计文档》**v1.30**（ADR-001–015、017–030；ADR-029 **v0.4** = Electron + aetherd；ADR-030 **v0.2** = 打包基线/无证书发布 + macOS 人工授权不阻塞 M9/M10）、《需求文档》v0.18、《实施计划与验收标准》v1.24、`docs/P2-架构迁移-实施计划与验收标准.md` **v0.8**（含 M10 详细 DoD/G9–G13）、`docs/UI-UX-设计规格.md` v0.17（S-01 不可达/S-12/S-13）。
- 状态：**P2-M9 G8 条件通过（conditional_pass）**；Electron 为默认构建/发布壳，`crates/aether-tauri` 已退役，**无 Tauri 回退包**；**M10 可启动**（需 M10-00 ADR 批次）；macOS **公开发布**签名/公证走人工授权（`docs/M9-14-macOS-签名授权方案.md`），**不阻塞 M9/M10**。
- **旧内测包手工卸载**：曾安装 Tauri 内测构建的机器没有 MSI 升级链/回退包；请先手工卸载旧包，再安装 Electron 构建。卸载不会删除数据目录；但**不得双开**同一数据目录（daemon 的 `.aetherd.lock` OS 独占锁会以 `data_dir_locked` 拒绝第二个核心）。详见 `docs/M9-11-旧包手工卸载说明.md` 与 `docs/M9-14-发布说明.md`。
- 发布说明：`docs/M9-14-发布说明.md`（数据目录 / Keychain 提示、unsigned 记录、安装/卸载）；G8 签核：`docs/M9-Gate-G8-报告.md`。

## 架构总览

```
apps/desktop            React UI（Vite + Zustand；事件驱动）
apps/desktop/electron   Electron main / preload / builder（ADR-029）
packages/protocol       生成式 TS 类型/客户端 + 命令路由清单（禁止手改）
packages/adapter-sdk    TypeScript 适配器 SDK（线协议、信封、限流）
packages/adapter-*      官方适配器（claude-code / codex / deepseek-harness；mock 仅测试）
crates/aether-core      纯模型与事件信封（无 I/O）
crates/aether-store     SQLite 存储（单写队列 + WAL + 迁移）
crates/aether-adapters  适配器进程宿主与监督器（D5/D6）
crates/aether-control   生命周期、事件管线、权限服务、背压
crates/aether-security  权限策略引擎、路径守卫、密钥与脱敏
crates/aether-rpc       JSON-RPC 2.0 / JSONL 叶子基础设施（ADR-029）
crates/aether-app       transport-agnostic 命令层/DTO/校验/AppHost（ADR-029）
crates/aether-daemon    aetherd 二进制 / stdio 传输 / 生命周期（ADR-029）
```

依赖方向单向：`core → store / adapters / control / security → app → daemon`；Electron main 只经 preload → main → `aetherd` 的 JSON-RPC/JSONL 通信，不依赖核心 crate（AGENTS §2.1）。

## 本地开发

要求：Rust 1.98+、Node 20+、pnpm 10.34.5、Bun 1.4.2（适配器单文件编译）、Electron 45（精确 patch）。

```powershell
pnpm install --frozen-lockfile
pnpm -r build            # 前端与包构建
cargo build --workspace  # Rust 构建（不含已退役的 Tauri 壳）
```

### 调试模式（开发者模式）

`pnpm dev:desktop` 启动 Electron（Vite + main/preload），自动补齐 `cargo` / `pnpm` 的 PATH；
额外参数经 `--` 透传（如 `pnpm dev:desktop -- --no-watch`）。Electron main 在 Debug 下可用
DevTools；release 构建不注册测试钩子。

```powershell
pnpm dev:desktop
```

- 默认注册表为空（运行时选择器无条目）。需在调试中真实启动运行时，先构建运行时包并在
  **编译前**注入（`build.rs` 会内嵌清单/程序摘要；更换目录后需重新构建）：

```powershell
node scripts/ci/build-runtime-bundles.mjs        # 默认 dist-out/runtime-bundle；AETHER_BUNDLE_MOCK=1 另含 Mock
$env:AETHER_RUNTIME_BUNDLE_DIR="$PWD\dist-out\runtime-bundle"
$env:AETHER_RUNTIME_REGISTRY_DIR="$PWD\dist-out\runtime-bundle"
pnpm dev:desktop
```

- 建议隔离调试数据目录（A4：必须本地磁盘，勿放 OneDrive / 网络盘）：

```powershell
$env:AETHER_DATA_DIR="$env:TEMP\aether-dev-data"
```

- OS 凭据库不可用时会走 A3 降级加密文件，需提供口令（调试/演练专用）：

```powershell
$env:AETHER_SECRETS_PASSPHRASE="dev-pass"
```

- 仅 UI 调试（浏览器打开 `http://localhost:5173`，无 Rust/IPC）：`pnpm --filter @aether/desktop dev`。

## 验证与门禁

| 目的 | 命令 |
|---|---|
| 本地全量门禁（与 CI 等价） | `pnpm ci:local` |
| Rust 测试（全 workspace） | `cargo test --workspace` |
| 前端测试 | `pnpm -r test` |
| 覆盖率门禁（Rust/TS ≥70%） | `pnpm verify:coverage` |
| 协议/类型契约（T14） | `pnpm --filter @aether/protocol check` |
| 命令/错误码基线 | `pnpm verify:m9-01-baseline && pnpm verify:error-codes` |
| 依赖合规 | `cargo deny check`（配 `deny.toml` 白名单） |
| 供应链（相似度/许可证/SBOM） | `pnpm verify:supply-chain` |
| G8 切换/退役/失败处理 | `pnpm verify:m9-14 && pnpm m9-14:failure-drill` |

M9 任务级验证入口：`pnpm verify:m9-02` … `pnpm verify:m9-14`。历史 M1–M8 任务验证脚本仍保留在
`scripts/test/` 供证据追溯；其中依赖已退役 Tauri 宿主的场景已由 M9-07/09/10/11/12/13 的
Electron/daemon 等价验证承接。

## 发布

- 安装器：`pnpm package:desktop`（Windows MSI / macOS per-arch DMG / Linux deb+AppImage）。
  安装产物内置三官方运行时注册包（ADR-015 schema，构建期由
  `scripts/ci/build-runtime-bundles.mjs` 生成）；Mock 适配器仅测试构建。
- 安装冒烟：`pnpm smoke:installer`（Windows）、`pnpm smoke:installer:macos`、`pnpm smoke:installer:linux`。
- 哈希：`dist-out/SHA256SUMS.txt`；签名/公证策略见 `docs/打包与发布说明.md` 与
  `.github/workflows/release-desktop-installers.yml`（Windows 可按 ADR-030 unsigned 发布并记录；
  macOS **公开发布**必须 Developer ID 签名 + 公证；未取得授权时仅阻塞 macOS 发布，不阻塞 M9/M10）。
- **无 Tauri 回退包**：旧内测包需手工卸载；发布失败=停止发布并修复（`docs/M9-14-失败处理演练报告.md`）。

## 文档

| 文档 | 说明 |
|---|---|
| `docs/适配器接入指南.md` | 适配器 manifest、线协议、SDK、会话/权限回环、测试 |
| `docs/失败场景处置手册.md` | 30 个失败场景 → 用户/维护操作步骤 |
| `docs/隐私说明.md` | 数据流向、密钥、诊断包与脱敏边界 |
| `docs/打包与发布说明.md` | 安装器、WebView2 引导、签名/空签、SmartScreen/Gatekeeper |
| `docs/M9-14-证据.md` | 切换、失败处理、依赖树、文档 diff、发布说明 |
| `docs/M9-14-发布说明.md` | 旧内测包手工卸载、数据目录、Keychain |
| `docs/M9-Gate-G8-报告.md` | G8 条件通过/发布状态 |
| `docs/M9-14-macOS-签名授权方案.md` | macOS 公开发布的人工授权、签名/公证步骤与证据 |
| `docs/M9-13-证据.md` | G7 全量回归/性能/旧数据/覆盖率 |

## 许可

Apache-2.0（见 `LICENSE`）。实现遵循净室合规：不拷贝上游项目源码（AGENTS §2.10）。
