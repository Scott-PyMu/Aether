# Aether

本地优先、可扩展的统一多 Agent 编排桌面平台（Tauri 2 + Rust 核心 + TypeScript 适配器 + React UI）。

- 基线：《设计文档》当前冻结版（含 ADR-001–016）、《需求文档》v0.12、《实施计划与验收标准》v1.24。
- 状态：P0（MVP）技术验收进行中；失败场景演练与验收清单见 `docs/`。

## 架构总览

```
apps/desktop            React UI（Vite + Zustand；事件驱动）
packages/protocol       Tauri IPC 类型契约（tauri-specta 生成物，禁止手改）
packages/adapter-sdk    TypeScript 适配器 SDK（线协议、信封、限流）
packages/adapter-*      官方适配器（claude-code / codex / deepseek-harness；mock 仅测试）
crates/aether-core      纯模型与事件信封（无 I/O）
crates/aether-store     SQLite 存储（单写队列 + WAL + 迁移）
crates/aether-adapters  适配器进程宿主与监督器（D5/D6）
crates/aether-control   生命周期、事件管线、权限服务、背压
crates/aether-security  权限策略引擎、路径守卫、密钥与脱敏
crates/aether-tauri     桌面壳与 IPC 命令层（薄）
```

依赖方向单向：`core → store / adapters / control / security → tauri`（AGENTS §2.1）。

## 本地开发

要求：Rust 1.98+、Node 20+、pnpm 10.34.5、Bun 1.4.2（适配器单文件编译）、WebView2（Windows）。

```powershell
pnpm install --frozen-lockfile
pnpm -r build            # 前端与包构建
cargo build --workspace  # Rust 构建
pnpm --filter @aether/desktop dev   # 前端热更（配合 cargo tauri dev）
```

## 验证与门禁

| 目的 | 命令 |
|---|---|
| 本地全量门禁（与 CI 等价） | `pnpm ci:local` |
| Rust 测试 | `cargo test --workspace --exclude aether-tauri` |
| 前端测试 | `pnpm -r test` |
| 覆盖率门禁（Rust/TS ≥70%） | `pnpm verify:coverage` |
| 类型契约（T14） | `pnpm --filter @aether/protocol check` |
| 依赖合规 | `cargo deny check`（配 `deny.toml` 白名单） |
| 供应链（相似度/许可证/SBOM） | `pnpm verify:supply-chain` |

任务级验证入口：`pnpm verify:m1-02` … `pnpm verify:m4-05`（逐任务 DoD 证据）。

失败场景演练（30 场景，夜跑）：

```powershell
node scripts/test/m4-01/verify-m4-01.mjs   # 进程类 11 场景
node scripts/test/m4-02/verify-m4-02.mjs   # 存储类 8 场景
node scripts/test/m4-03/verify-m4-03.mjs   # 协议/并发类 11 场景
```

## 发布

- 安装器：Windows MSI / macOS DMG（`node scripts/ci/build-desktop-installer.mjs`），
  安装产物内置三官方运行时注册包（ADR-015 schema，构建期由
  `scripts/ci/build-runtime-bundles.mjs` 生成）；Mock 适配器仅测试构建。
- 哈希：`dist-out/SHA256SUMS.txt`；签名/公证步骤挂接见
  `docs/打包与发布说明.md` 与 `.github/workflows/release-desktop-installers.yml`。

## 文档

| 文档 | 说明 |
|---|---|
| `docs/适配器接入指南.md` | 适配器 manifest、线协议、SDK、会话/权限回环、测试 |
| `docs/失败场景处置手册.md` | 30 个失败场景 → 用户/维护操作步骤 |
| `docs/隐私说明.md` | 数据流向、密钥、诊断包与脱敏边界 |
| `docs/打包与发布说明.md` | 安装器、WebView2 引导、签名/空签、SmartScreen/Gatekeeper |
| `docs/M4-04-验收报告.md` | T1–T15 验收与性能基线 |
| `docs/M4-Gate4-验收报告.md` | P0 门禁（Gate 4）验收报告 |

## 许可

Apache-2.0（见 `LICENSE`）。实现遵循净室合规：不拷贝上游项目源码（AGENTS §2.10）。
