# M9 Gate G6 报告：打包

| 项 | 内容 |
|---|---|
| Gate | **G6 打包**（P2 计划 §2.15；ADR-029 v0.4 D-029-10；ADR-030 v0.2） |
| 任务 | M9-11（打包/签名/公证/安装冒烟） |
| 日期 | 2026-11-05（复跑；HEAD `994663713c8d4105486393f6f84fba5ec6a1790e`） |
| 计划/设计 | P2 v0.8；设计 v1.30；ADR-030 v0.2 |
| 证据 | `docs/M9-11-证据.md`、`docs/M9-11-macOS-暂停记录.md`、`docs/M9-14-macOS-签名授权方案.md`、`dist-out/m9-11-smoke-release-2026-11-03T13-33-32-572Z.json` |
| 验证 | `pnpm verify:m9-11`、`pnpm smoke:installer`（历史 release 冒烟）、CI run `41950594105`（Linux/macOS） |
| 结论 | **G6 条件通过（conditional_pass；`macos_release_pending`）** |

## 1. 通过条件逐项核对

| # | G6 条件（P2 §2.15） | 本次证据 | 判定 |
|---|---|---|---|
| 1 | MSI / per-arch DMG | electron-builder 配置：Windows `msi`；macOS `dmg`（per-arch，非 universal） | ✓ |
| 2 | sidecar / runtime-bundle | `extraResources`：`bin/aetherd` + `runtime-bundle`；共享适配器宿主 + `--adapter`（ADR-030） | ✓ |
| 3 | 安装冒烟 | release 冒烟 JSON：安装 → 首启 → 卸载；数据保留；UI ready 6224ms | ✓（Windows 本机） |
| 4 | Linux 冒烟 | CI run `41950594105` job「M9-12 Linux build + xvfb smoke」success；deb/AppImage | ✓（CI） |
| 5 | 无旧 MSI 升级链 | MSI UpgradeCode 与旧 Tauri 不同；Upgrade 表仅本产品（rows=0） | ✓ |
| 6 | 体积硬阈值 | MSI **160,219,136 B** ≤ 1.5×HEAD（191,717,376 B）且 ≤450 MiB | ✓ |
| 7 | idle RSS / 10 会话 | 安装产物 idle **514.57 MiB** ≤600 MiB；10 会话上界 724.39 MiB ≤1.2 GiB | ✓ |
| 8 | Windows 签名政策 | 未配置证书 → `unsigned_release=true` + SHA256SUMS + SmartScreen 指引；未声称已签名（ADR-030） | ✓ |
| 9 | macOS 签名/公证 | **未就绪**（`macos_release_pending`）；人工授权方案在案；发布工作流对 macOS 失败关闭 | ⚠️ 条件 |
| 10 | macOS DMG 冒烟 | CI run `41950594105` job「M9-12 DMG mount/copy/launch smoke (macOS)」success（unsigned 冒烟） | ✓ |

## 2. 本次复跑输出（2026-11-05）

```text
$ pnpm verify:m9-11
PASS  配置存在且 appId=dev.aether.desktop
PASS  Windows target=msi / macOS target=dmg（per-arch）
PASS  extraResources：bin/aetherd + runtime-bundle
PASS  MSI 无旧 MSI 升级链（Upgrade 表仅本产品）
PASS  release 冒烟通过（安装/首启/卸载/数据保留）
PASS  DoD-7 体积硬阈值（160219136 / 191717376 bytes）
PASS  DoD-7 首窗 ≤1.5s（visibleMs=864）
PASS  DoD-7 idle RSS ≤600MiB（514.57 MiB）
NOTE  UI ready 实测 6224ms（≤8000ms）
PASS  DoD-7 10 会话 ≤1.2GiB（724.39 MiB）
PASS  Windows 签名政策（unsigned 发布记录）
[verify:m9-11] PASS 检查=66 备注=1
```

CI（`https://github.com/Scott-PyMu/Aether/actions/runs/41950594105`）：

```text
✓ M9-12 Linux build + xvfb smoke
✓ M9-12 DMG mount/copy/launch smoke (macOS)
✓ M9-12 MSI silent install/uninstall smoke
✗ m9-11-package / Windows MSI + installer smoke
  — runner 失去通信（GitHub 基础设施错误；非安装器断言失败）
```

## 3. 条件与遗留

| 项 | 状态 | 影响/后续 |
|---|---|---|
| **macOS Developer ID 签名 + 公证** | **未就绪** | 仅阻塞 macOS **公开发布**；按 ADR-030 v0.2 不阻塞 M9/M10。Apple secrets 未申请（`M9_11_MAC_SIGNING_READY` 未设置）；发布工作流对 macOS 仍失败关闭 |
| Windows CI MSI job | runner 失联 | 本机 release 冒烟已通过；待 CI 复跑确认（不改变 G6 判定口径） |
| 旧内测包 | 文档说明 | 旧 Tauri MSI 不提供升级链；需手工卸载；不得双安装/双数据写 |

> G6 不因 macOS 签名未就绪而阻塞 M9/M10；但未取得密钥前不得对外发布 macOS DMG。

## 4. 签核

| 角色 | 日期 | 结论 |
|---|---|---|
| 执行（AI 编码助手） | 2026-11-05 | G6 条件复跑通过；Windows/Linux 可发布，macOS 公开发布待授权 |
| 项目负责人（用户裁定） | 2026-11-05 | **G6 条件通过（`conditional_pass` / `macos_release_pending`）**；不阻塞 M9/M10 |
