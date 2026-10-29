/**
 * 壳级页面 IPC 通道（S-12/S-13；ADR-029 §3.5-8/§5.4）。
 *
 * 独立成无 Electron 依赖的模块：main 与 preload 共享常量，但**不能**从
 * preload 入口导入常量（入口顶层会执行 `contextBridge.exposeInMainWorld`，
 * 在 main 进程会抛 “Cannot read properties of undefined”）。
 */
/** S-12 状态。 */
export const FATAL_STATE_CHANNEL = "aether:fatal-state";
/** S-12 重试。 */
export const FATAL_RETRY_CHANNEL = "aether:fatal-retry";
/** S-12 退出。 */
export const FATAL_EXIT_CHANNEL = "aether:fatal-exit";
/** S-12 离线导出诊断。 */
export const FATAL_EXPORT_CHANNEL = "aether:fatal-export";
/** S-13 状态。 */
export const KEYCHAIN_STATE_CHANNEL = "aether:keychain-state";
/** S-13 重试授权。 */
export const KEYCHAIN_RETRY_CHANNEL = "aether:keychain-retry";
/** S-13 口令提交。 */
export const KEYCHAIN_PASSPHRASE_CHANNEL = "aether:keychain-passphrase";
/** S-13 跳过（只读/降级）。 */
export const KEYCHAIN_SKIP_CHANNEL = "aether:keychain-skip";

export interface KeychainActionResult {
  ok: boolean;
  error?: string;
}

export interface KeychainViewState {
  scope: "provider" | "backup";
  mode: "reauth" | "passphrase" | "degraded";
  reason: string;
  notice?: string;
}
