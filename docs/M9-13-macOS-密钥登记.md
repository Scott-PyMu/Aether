# M9-13 macOS 签名/公证密钥登记

| 项 | 内容 |
|---|---|
| 任务 | M9-13（G7 平台矩阵）/ M9-11（G6 打包签名） |
| 登记日期 | 2026-11-03 |
| 状态 | **密钥未申请**（暂缓 macOS 签名/公证复验） |
| 影响 | M9-13 平台矩阵 macOS 标记 `ci-unverified`；G6 macOS 签名/公证门禁不通过 |
| 处理 | 先登记，不伪造通过；密钥申请完成后按本文 §3 重新验证并回填证据 |

## 1. 缺失材料

| 材料 | 用途 | 当前状态 |
|---|---|---|
| Apple Developer Program 账号 / Team ID | Developer ID Application 证书 | 未申请 |
| `Developer ID Application` 证书（.p12） | `codesign` 嵌套签名（`aetherd`/适配器 → `.app` → DMG） | 未申请 |
| 证书密码 | CI 导入钥匙串 | 未申请 |
| App Store Connect API Key（Issuer ID / Key ID / `.p8`） | `notarytool` 公证 | 未申请 |
| 公证 Apple ID 专用密码（备选路径） | `notarytool` 备选认证 | 未申请 |

## 2. 当前证据口径

- `docs/M9-13-证据.md` §6：macOS 标 `ci-unverified`，原因明确写「密钥未申请」；
- `scripts/test/m9-13/platform-matrix.json`：macOS `status=ci-unverified`，附 `reason`；
- `verify:m9-13` 对未跑项要求 `reason`，缺失即失败；不将未签名冒烟冒充签名/公证通过；
- M9-12 CI run 41185306916 的 `m9-12-e2e-macos` / `m9-12-installer-macos` 仅作为**未签名冒烟**历史证据，不构成 G6 通过。

## 3. 密钥申请后的重新验证步骤

1. 在仓库 Secrets 配置：
   - `CSC_LINK`（Developer ID `.p12` base64 或路径）
   - `CSC_KEY_PASSWORD`
   - `APPLE_ID` / `APPLE_APP_SPECIFIC_PASSWORD` / `APPLE_TEAM_ID`
   - 或 `APPLE_API_KEY` / `APPLE_API_KEY_ID` / `APPLE_API_ISSUER`
2. 在 macOS runner 执行 `pnpm package:desktop`（per-arch DMG）；
3. 执行嵌套签名顺序校验：`aetherd`/适配器 → `.app` → DMG，同一 Team ID；
4. 执行 `codesign --verify --deep --strict`、`spctl --assess`、`xcrun stapler validate`；
5. 执行 DMG 挂载/复制/启动冒烟（`pnpm smoke:installer:macos`）；
6. 将 run id、哈希、签名/公证输出回填 `docs/M9-13-证据.md` §6 与 `docs/M9-11-证据.md`；
7. 重新运行 `pnpm verify:m9-13`（平台矩阵 macOS 状态改为 `passed` 后不再有该备注）。

## 4. 约束

- 密钥/证书/口令不得进入仓库、日志、诊断包、测试夹具（AGENTS §2.9）；
- 未取得密钥前不得将 macOS 状态改为 `passed`，不得以 unsigned 冒烟替代 G6；
- 申请完成后本文件保留为登记记录，不删除。
