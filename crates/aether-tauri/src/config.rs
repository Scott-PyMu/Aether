//! 桌面壳安全基线常量（M1-08 DoD 2 / DoD 4）。
//!
//! 可执行断言位于 `tests/security_baseline.rs`（从磁盘读取真实签入文件）；
//! 基线变更（CSP / capabilities / devtools）须走 ADR（AGENTS §8）。

/// CSP 基线（设计 D7 / 评审 #7，逐字符冻结）。
pub const EXPECTED_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src ipc: http://ipc.localhost; frame-src 'none'; object-src 'none'";

/// capabilities 最小 allowlist（按窗口裁剪；当前 UI 不消费任何插件/核心命令，
/// 因此为空）。新增权限必须同步修改本清单并更新本常量。
pub const CAPABILITY_PERMISSION_ALLOWLIST: &[&str] = &[];

/// capabilities 允许出现的字段（防新增未评审字段，如 `remote` / `local`）。
pub const CAPABILITY_ALLOWED_FIELDS: &[&str] = &[
    "$schema",
    "identifier",
    "description",
    "windows",
    "permissions",
];

/// 唯一允许签入的 capability 文件。
pub const CAPABILITY_FILE_ALLOWLIST: &[&str] = &["default.json"];

/// 必须禁用 Tauri 的资产 CSP 注入（`dangerousDisableAssetCspModification: true`）。
///
/// 默认行为会在构建期给前端 HTML 内的 `<script src="http://...">` 注入 nonce、给
/// 内联脚本注入 hash，等效于放宽 `script-src 'self'`（内联/远程脚本可执行）。
/// 设计 D7 要求「模型/网页内容不允许进入主 WebView 执行」，M1-08 DoD1 要求内联
/// 远程脚本测试页被阻断，因此固定为禁用；Tauri 自身 IPC 初始化脚本经
/// document-start 注入，不受 CSP 影响。
pub const ASSET_CSP_MODIFICATION_DISABLED: bool = true;
