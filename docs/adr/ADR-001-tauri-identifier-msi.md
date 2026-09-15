# ADR-001：修正 Tauri identifier 并明确 MSI/DMG 生成职责

| 项 | 内容 |
|---|---|
| 状态 | 已接受 |
| 日期 | 2026-09-15 |
| 决策载体 | 《设计文档》v1.0 → v1.1 |
| 回退条件 | 无 |

## 背景

1. `tauri.conf.json` 的 identifier 为 `dev.aether.app`，以 `.app` 结尾，与 macOS 应用包后缀冲突（Tauri 校验会拒绝/告警），且不符合反向域名惯例。
2. 交付职责重叠：`cargo-dist` 与 Tauri bundler 均会生成 MSI（DMG 同理），存在双来源产物，CI 重复构建、签名与发布目标不明确。

## 决策

1. identifier 改为 `dev.aether.desktop`。
2. MSI/DMG 由 Tauri 生成（MSI 含 WebView2 `embedBootstrapper` 内嵌引导）；`cargo-dist` 仅负责编排与发布（生成三平台 CI 工作流、上传产物、挂接签名/公证步骤），不生成安装器。

## 影响

- 《设计文档》D1 实现要点：identifier 单行修改。
- 《设计文档》§2.1 平台交付段：重写为 Tauri 产出、cargo-dist 编排。
- 《设计文档》§2.2 技术栈表「打包/发布」行：cargo-dist 具体组件改为「编排 CI/发布与产物上传」（评审补充，ADR 起草时遗漏）。
- 《设计文档》§5 P0「新增能力」措辞：`Tauri 产出 MSI/DMG、cargo-dist 编排发布`（评审补充）。
- 《设计文档》升版 v1.0 → v1.1，并记入修订记录。
- 《实施计划与验收标准》M1-01 DoD 第 4 条：改为「Tauri 产出 MSI/DMG，cargo-dist 编排上传」。
- 《实施计划与验收标准》M4-05 DoD 第 1 条：同步改为 Tauri 产出、cargo-dist 编排（评审补充，ADR 起草时遗漏）。
- 不影响任何其他设计决策。

## 回退条件

无。
