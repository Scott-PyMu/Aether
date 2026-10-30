# M9-11 macOS 打包暂停记录

| 项 | 内容 |
|---|---|
| 日期 | 2026-10-30 |
| 状态 | **macOS 暂未通过（暂停）**；Windows + Linux 子集通过 |
| 任务 | M9-11（DoD-1/2/3 的 macOS 部分） |
| 原因 | 仓库 Actions secrets 为空（GitHub API `total_count=0`）。没有 Apple Developer ID Application 证书、没有公证凭据，macOS arm64/x64 job 在签名前置检查失败，未产出 DMG。 |
| 影响 | `docs/M9-11-证据.md` 的 DoD-1（per-arch DMG）、DoD-2（签名/公证/stapler）、DoD-3（Keychain 签名身份）在 macOS 上未验证；M9-11 整体不能签字，G6 不能通过。Windows（unsigned，ADR-030）与 Linux 不受影响。 |
| 当前处置 | `.github/workflows/m9-11-package.yml` 的 `macos-arm64` / `macos-x64` 增加 `if: vars.M9_11_MAC_SIGNING_READY == 'true'`。变量未设置时 job 为 **skipped**，不再让 workflow 因缺密钥变红；Windows/Linux job 继续运行。 |
| 恢复条件 | 申请 Apple Developer Program（个人 $99/年；组织需 D-U-N-S）→ 创建 Developer ID Application 证书并导出 `.p12` → 配置下列 secrets → 设置 variable `M9_11_MAC_SIGNING_READY=true` → 重跑 `m9-11-package`。 |
| 所需 secrets | `MAC_CERTS`（.p12 Base64）、`MAC_CERTS_PASSWORD`（p12 导出密码）、`MAC_IDENTITY`（如 `Developer ID Application: Name (TEAMID)`）、`APPLE_ID`、`APPLE_APP_SPECIFIC_PASSWORD`（appleid.apple.com 生成的 App 专用密码）、`APPLE_TEAM_ID`（10 位 Team ID） |
| 恢复后验收 | arm64/x64 per-arch DMG → Developer ID 签名 + hardened runtime → 公证 + `stapler validate` → `codesign --verify --deep --strict` / `spctl` / Team ID 一致 → DMG 挂载/复制/启动冒烟 → `verify:m9-11`；全部绿后回填 `docs/M9-11-证据.md` 并签 G6。 |
| 不采用 | unsigned/ad-hoc macOS DMG 不满足 DoD-2（Gatekeeper 拦截）；不得以「右键打开」宣称通过。ADR-030 仅 Windows 允许 unsigned。 |
