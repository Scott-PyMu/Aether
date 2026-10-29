/**
 * main 侧 RPC 第一道防线（M9-10 DoD-2；ADR-029 §5.1/D-029-4）。
 *
 * 口径（与 daemon 双端校验一致，daemon 为最终权威）：
 * - 未知方法 → `not_implemented`（JSON-RPC `-32601`）；
 * - 参数超过 2MiB → `too_large`（`-32003`）；
 * - 并发在途 ≥64 → `internal`（`-32603`，第 65 个请求起拒绝）。
 *
 * 本模块刻意保持**纯函数**：Electron 接线在 `app.ts`，单测覆盖边界矩阵，
 * 避免把安全判定散落在不可单测的 main 生命周期代码里。
 */

/** 单次 RPC 参数上限（ADR-029 D6：2MiB）。 */
export const MAX_RPC_PAYLOAD_BYTES = 2 * 1024 * 1024;
/** main 并发请求上限（ADR-029 §5.1：64）。 */
export const MAX_CONCURRENT_RPC = 64;

/** 拒绝原因（可直接映射 JSON-RPC error/data）。 */
export interface RpcGuardRejection {
  /** JSON-RPC 数值码。 */
  code: number;
  /** 设计文档错误码（`error_codes.json` 的 ipc 枚举）。 */
  aetherCode: string;
  message: string;
  /** 可选字段名（仅结构类错误使用）。 */
  field?: string;
}

/** 放行或拒绝。 */
export type RpcGuardDecision =
  | { ok: true }
  | { ok: false; rejection: RpcGuardRejection };

export interface RpcGuardInput {
  /** 命令名（已由 IPC 信封解析为非空字符串）。 */
  method: string;
  /** 命令参数（未解析；仅做大小测量）。 */
  params?: unknown;
  /** 当前在途请求数（调用方在放行后立即自增）。 */
  inFlight: number;
  /** 命令清单（63 条；由 `command-manifest.json` 构建）。 */
  commands: ReadonlySet<string>;
}

/** 参数序列化后的 UTF-8 字节数；不可序列化视为无限大（拒绝）。 */
export function payloadSizeBytes(params: unknown): number {
  if (params === undefined) {
    return 0;
  }
  try {
    const text = JSON.stringify(params);
    if (typeof text !== "string") {
      return Number.POSITIVE_INFINITY;
    }
    return Buffer.byteLength(text, "utf8");
  } catch {
    return Number.POSITIVE_INFINITY;
  }
}

/** 按固定顺序检查：方法白名单 → 大小 → 并发（ADR-029 §5.1）。 */
export function guardRpcRequest(input: RpcGuardInput): RpcGuardDecision {
  if (typeof input.method !== "string" || input.method.length === 0) {
    return reject(-32600, "invalid_json", "method 必须是非空字符串");
  }
  if (!input.commands.has(input.method)) {
    return reject(-32601, "not_implemented", `未知命令：${input.method}`);
  }
  const size = payloadSizeBytes(input.params);
  if (!Number.isFinite(size) || size > MAX_RPC_PAYLOAD_BYTES) {
    return reject(-32003, "too_large", "请求参数超过 2MiB 上限");
  }
  if (input.inFlight >= MAX_CONCURRENT_RPC) {
    return reject(
      -32603,
      "internal",
      `并发请求超过 ${MAX_CONCURRENT_RPC}，已拒绝`,
    );
  }
  return { ok: true };
}

function reject(
  code: number,
  aetherCode: string,
  message: string,
): RpcGuardDecision {
  return { ok: false, rejection: { code, aetherCode, message } };
}
