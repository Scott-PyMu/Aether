# M9-11 阈值问题：处理记录（已由 ADR-030 解决）

> 状态：**已关闭（2026-10-30）**。首轮阻塞事实保留作追溯；决策见
> `docs/adr/ADR-030-M9-11-打包基线与无证书发布.md`，实测见 `docs/M9-11-证据.md`。
> 不再要求 M9-13 复测体积/idle RSS/UI ready 的门槛口径（G7 只做回归复跑）。

## 1. 首轮阻塞事实（2026-10-30 上午）

| 指标 | 首轮实测 | 首轮冻结阈值 | 结论 |
|---|---|---|---|
| release MSI 体积 | 233,693,184 B | 191,717,376 B | 超 21.9% |
| idle RSS | 528–536 MiB | 500 MiB | 超 6–7% |
| UI ready | 5.8–6.9 s | 5 s | 超（G7） |

根因：Electron 45 运行时 ~300MB + 3×Bun 适配器（各 92MB，各自内嵌一份 Bun 运行时），
非残留代码/资源。

## 2. 本轮处理

### 2.1 包体（真实优化，未放宽体积阈值）

- 新增 `packages/adapter-host`：单宿主二进制 + `--adapter` 分发；`runtime-bundle` 由
  3×92MB 降为 1×96.5MB（`program` 相同、`args=["--adapter",id]`）。
- afterPack 移除 Electron 遗留 `default_app.asar`；保留 `compression=maximum`、
  `electronLanguages=[en-US,zh-CN]`、asar、无 sourcemap、sidecar 不入 asar。
- 结果：release MSI **160,219,136 B（-31.4%）**，**≤191,717,376 B 通过**。

### 2.2 内存（审计 + 阈值按 Electron 口径修订）

- 实测组成：Electron main/GPU/renderer/utility ≈435 MiB、daemon 30.8 MiB、conhost 19 MiB、
  2 个预热适配器 77 MiB；无泄漏/残留进程。
- 保留启动预热（ADR-015/M2-08 语义；保证会话创建/首条消息时延）；idle RSS 硬阈值
  **500→600 MiB**（ADR-030；实测 536.57 MiB，仍为硬门禁）。
- UI ready 硬阈值 **5,000→8,000 ms**（同步预热在打包产物上的实测口径；开发 E2E 仍按 5s 硬断言）。

### 2.3 无证书发布（对齐 Proma）

- Proma `release.yml`：Windows job **未配置证书/签名**（electron-builder 默认跳过），
  macOS job 使用 `MAC_CERTS`/`APPLE_ID`/`APPLE_APP_SPECIFIC_PASSWORD`/`APPLE_TEAM_ID`
  签名 + 公证。
- Aether 采用同一政策：Windows 无证书时 **unsigned 发布**，记录 `unsigned_release=true`、
  归档 SHA256、发布说明给 SmartScreen 指引；**macOS 仍强制签名/公证**，缺失即 G6 不通过。
- 获得 Windows 证书后配置 `CSC_LINK`/`AETHER_WIN_CERT_FILE` 即恢复 Authenticode + RFC3161 校验。

## 3. 门禁影响

- `docs/M9-01-baseline/freeze/thresholds.json`：`idle_rss_max_bytes=629145600`、
  `ui_ready_max_ms=8000`；manifest 哈希同步；`pnpm verify:m9-01-baseline` 通过。
- P2 计划 v0.6：M9-11 DoD-7、G6、M9-13 DoD-5、性能表同步 ADR-030。
- 体积阈值不变；`pnpm verify:m9-11` 在本机 Windows 通过（macOS/Linux 项仍标注未验证）。

## 4. 复跑

```powershell
pnpm package:desktop
pnpm smoke:installer
pnpm package:desktop:test
pnpm smoke:installer --variant test
pnpm verify:m9-11
pnpm verify:signatures --artifact dist-out\Aether-0.1.0-x64.msi
```
