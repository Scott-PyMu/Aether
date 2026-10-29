/**
 * Aether 协议包（设计 D6 / D7；ADR-029 §7）。
 *
 * - 线协议版本常量：适配器握手校验 major，不匹配拒绝加载（D6）；
 * - `types.ts` / `client.ts` 由 `aether-app` 清单生成（**禁止手改**，AGENTS §2.8）；
 *   生成/校验入口：`pnpm --filter @aether/protocol generate|check`（M9-08）；
 * - 事件为 9 字段 `AetherEvent`；错误为 `AetherError { aether_code, business_code?, details? }`。
 */
export { APP_VERSION } from "./version";
export * from "./types";
export * from "./client";

/** 线协议版本（设计 D6：JSON-RPC 2.0 over stdio，协议版本 1.0）。 */
export const PROTOCOL_VERSION = {
  major: 1,
  minor: 0,
} as const;

export type ProtocolVersion = typeof PROTOCOL_VERSION;
