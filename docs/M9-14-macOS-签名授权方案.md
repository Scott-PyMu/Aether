# M9-14 macOS 签名/公证人工授权方案

| 项 | 内容 |
|---|---|
| 依据 | ADR-030 v0.2（2026-11-05 用户裁定）；ADR-029 v0.4 §8.1 |
| 决定 | **M9 G8 条件通过、M10 可启动，不因 macOS 阻塞（不阻塞 M9/M10）**；macOS **公开发布**仍必须签名/公证 |
| 当前状态 | `M9_11_MAC_SIGNING_READY` 未设置；Apple Developer ID/公证 secrets 未申请；`macos_release_pending` |
| 发布门禁 | `release-desktop-installers.yml` 的 macOS publish 失败关闭；Windows/Linux 可发布 |
| 证据 | `docs/M9-Gate-G8-报告.md`、`docs/M9-13-macOS-密钥登记.md`、本文件 |

## 1. 范围与原则

- 本方案只处理 **macOS 公开发布**（DMG 对外分发）的签名/公证授权；不改变 M9/M10 的开发、CI、
  G8 条件通过或 M10-00 启动条件。
- **不跳过校验**：未签名/未公证的 macOS DMG 不得进入公开 Release；不得以“内测”名义对外分发。
- **无 Tauri 回退**：签名失败=停止 macOS 发布并修复；可保留上一已签名 Electron DMG。
- **人工授权**：由发布负责人 + 安全负责人双人确认；授权记录、证书/Team ID、产物 SHA256、
  公证 ticket、`stapler` 验证输出归档。
- 时间/次数类指标为发布后跟踪 KPI，不作为 M9/M10 阶段门禁（ADR-013）。

## 2. 前置材料（发布负责人）

| 材料 | 说明 | 存放 |
|---|---|---|
| Apple Developer Program 账号 | 有效付费账号 | 组织账户 |
| Developer ID Application 证书 | `.p12` + 密码；Team ID | GitHub Secrets / 受控密钥库 |
| 公证凭据 | Apple ID + app-specific password + Team ID（或 API key） | GitHub Secrets |
| macOS 构建机 | macOS 14/15（arm64/x64 runner 或受控 Mac） | CI/受控设备 |
| `notarytool`/`stapler` | Xcode Command Line Tools | 构建机 |

> 证书、密码、app-specific password **不得**出现在日志、证据、提交或聊天中（AGENTS §2.9）。

## 3. 方案 A：CI 自动签名/公证（推荐，取得 secrets 后）

1. 在仓库 Actions Variables 设置 `M9_11_MAC_SIGNING_READY=true`。
2. 配置 Secrets：`MAC_CERTS`（base64 p12）、`MAC_CERTS_PASSWORD`、`MAC_IDENTITY`、
   `APPLE_ID`、`APPLE_APP_SPECIFIC_PASSWORD`、`APPLE_TEAM_ID`。
3. 推送 tag 或手动触发 `release-desktop-installers.yml`：
   - `macos_arm64` / `macos_x64` 导入证书 → `pnpm package:desktop --arch ...` →
     `signatures.mjs` → `smoke:installer:macos`；
   - `publish` job 校验 macOS job 成功后才上传 DMG；否则失败关闭。
4. 证据：CI run id、`m9-11-smoke-*-macos.json`、`signatures.mjs` 输出、DMG SHA256。

## 4. 方案 B：受控 macOS 机器人工签名/公证（secrets 未进 CI 时）

在受控 macOS 机器（非公开 runner）执行：

```bash
export MAC_CERTS="$(base64 -i DeveloperID.p12)"
export MAC_CERTS_PASSWORD='<p12-password>'
export MAC_IDENTITY='Developer ID Application: <Team> (<TEAMID>)'
export APPLE_ID='<apple-id>'
export APPLE_APP_SPECIFIC_PASSWORD='<app-specific-password>'
export APPLE_TEAM_ID='<TEAMID>'
export AETHER_MAC_IDENTITY="$MAC_IDENTITY"
export AETHER_MAC_TEAM_ID="$APPLE_TEAM_ID"
export AETHER_NOTARIZE=1
export AETHER_REQUIRE_SIGNING=1

pnpm install --frozen-lockfile
pnpm package:desktop --arch arm64   # 或 --arch x64
node scripts/test/m9-11/signatures.mjs --artifact dist-out/Aether-*-arm64.dmg
pnpm smoke:installer:macos
```

人工复核（必须全部通过）：

```bash
codesign --verify --deep --strict --verbose=2 "/Applications/Aether.app"
spctl --assess --type execute --verbose=4 "/Applications/Aether.app"
xcrun stapler validate dist-out/Aether-*-arm64.dmg
shasum -a 256 dist-out/Aether-*-arm64.dmg
```

授权人将 DMG + `SHA256SUMS` + `signatures.mjs`/`smoke:installer:macos` 输出 + run/命令记录
交给发布负责人；发布负责人用 `gh release upload <tag> <dmg> <sha256>` 手工上传。
**不得**上传未签名/未公证产物。

## 5. 验收与证据

| 项 | 命令/证据 | 通过条件 |
|---|---|---|
| 签名/公证 | `node scripts/test/m9-11/signatures.mjs --artifact <dmg>` | exit=0；输出 Developer ID/notarization 通过 |
| 公证票据 | `xcrun stapler validate <dmg>` | 输出 `The validate action worked!` |
| Gatekeeper | `spctl --assess --type execute --verbose=4 <app>` | `accepted` |
| DMG 冒烟 | `pnpm smoke:installer:macos` | 挂载/复制/启动通过 |
| 哈希 | `shasum -a 256 <dmg>` | 与 Release/SHA256SUMS 一致 |
| 归档 | `docs/M9-14-证据.md` 追加行 + Release 附件 | 授权人/复核人/日期/产物哈希 |

## 6. 状态跟踪

| 项 | 状态 | Owner | 下一步 |
|---|---|---|---|
| Apple Developer ID/公证 secrets | **pending** | 发布负责人 | 申请/配置 secrets |
| `M9_11_MAC_SIGNING_READY` | **false/unset** | 发布负责人 | 取得材料后置 true |
| macOS 公开发布 | **blocked（仅发布）** | 发布负责人 | 方案 A 或 B |
| M9 G8 | **conditional_pass** | 项目负责人 | 已由用户裁定不阻塞（2026-11-05） |
| M10 | **unlocked（需 M10-00）** | 项目负责人 | 启动 M10-00 ADR 批次 |

## 7. 失败处理

- 证书过期/不可读：停止 macOS 发布；按 `docs/M9-13-macOS-密钥登记.md` 重新申请/导入。
- 公证失败：保留失败日志（脱敏），修复后重跑；不得改签名校验绕过。
- `stapler` 失败：不得上传；重新公证或重新打包。
- CI secrets 缺失：`publish` job 失败关闭；Windows/Linux 发布不受影响。
- 不得回装 Tauri 或生成回退包。
