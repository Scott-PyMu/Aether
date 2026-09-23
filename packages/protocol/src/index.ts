/**
 * Aether 协议包（设计 D6 / D7）。
 *
 * - 线协议版本常量：适配器握手校验 major，不匹配拒绝加载（D6）；
 * - `bindings.ts` 为 tauri-specta 生成物（**禁止手改**，AGENTS §2.8）；
 *   生成/校验入口：`pnpm --filter @aether/protocol generate|check`（T14）。
 */
export { APP_VERSION } from "./version";
export * from "./bindings";

/** 线协议版本（设计 D6：JSON-RPC 2.0 over stdio，协议版本 1.0）。 */
export const PROTOCOL_VERSION = {
  major: 1,
  minor: 0,
} as const;

export type ProtocolVersion = typeof PROTOCOL_VERSION;
