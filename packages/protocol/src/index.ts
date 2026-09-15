/**
 * Aether 协议包（设计 D6 / D7）。
 *
 * - 线协议版本常量：适配器握手校验 major，不匹配拒绝加载（D6）；
 * - tauri-specta 生成的 `bindings.ts` 将放置于本包（禁止手改，AGENTS §2.8），
 *   生成流程自 M1-08 起落地。
 */
export { APP_VERSION } from "./version";

/** 线协议版本（设计 D6：JSON-RPC 2.0 over stdio，协议版本 1.0）。 */
export const PROTOCOL_VERSION = {
  major: 1,
  minor: 0,
} as const;

export type ProtocolVersion = typeof PROTOCOL_VERSION;
