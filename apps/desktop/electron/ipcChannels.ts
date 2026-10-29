/**
 * Electron IPC 通道名（ADR-029 §4.4/§16.4）。
 *
 * 独立成无 Node 依赖的模块：sandboxed preload 不能 `require('node:path')`
 * 等内置模块，preload 只能从这里取常量，不得引入 `security.ts`。
 */
/** 命令通道。 */
export const RPC_IPC_CHANNEL = "aether:rpc";
/** 版本通道。 */
export const VERSION_IPC_CHANNEL = "aether:version";
/** 事件通道（ADR-029 §4.4 三处命名映射之一）。 */
export const EVENT_IPC_CHANNEL = "aether:event";
