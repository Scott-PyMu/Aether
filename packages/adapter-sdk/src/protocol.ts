/**
 * D6 线协议常量（TS 侧镜像，与 `crates/aether-adapters/src/protocol.rs` 对齐）。
 *
 * 线协议为 JSON-RPC 2.0 over stdio / JSON-Lines v1.0；协议版本字符串 `"1.0"`。
 */

export const PROTOCOL_MAJOR = 1;
export const PROTOCOL_MINOR = 0;
export const PROTOCOL_VERSION = "1.0";

/** 握手超时（D6：进程启动 10s 内必须发 `hello`）。 */
export const HANDSHAKE_TIMEOUT_MS = 10_000;
/** D6 失败场景表：连续 20 次无效帧（坏 JSON / 帧校验失败）判不健康。 */
export const INVALID_FRAME_UNHEALTHY_THRESHOLD = 20;

/** D6 单帧硬上限 2MiB（ADR-004）。 */
export const MAX_FRAME_BYTES = 2 * 1024 * 1024;
/** D6 `artifact_ref` 引用帧上限：引用帧本身必须 <1MiB（≥1MiB 视为契约违约）。 */
export const ARTIFACT_REF_LIMIT_BYTES = 1024 * 1024;
/** 附件引用行判别值（行顶层 `"type"` 字段，M2-09 落全帧形状）。 */
export const ARTIFACT_REF_TYPE = "artifact_ref";
/** `artifact_ref` 引用帧方法名（与判别值同值；M2-09 全帧形状）。 */
export const ARTIFACT_REF_METHOD = "artifact_ref";

/**
 * `artifact_ref` 引用帧全帧形状（M2-09/D6）：
 *
 * ```json
 * {
 *   "jsonrpc": "2.0",
 *   "method": "artifact_ref",
 *   "type": "artifact_ref",
 *   "params": {
 *     "session_id": "01J...",
 *     "run_id": "01J...",
 *     "refs": [
 *       { "path": "shot.png", "size": 3145728, "kind": "image/png" }
 *     ]
 *   }
 * }
 * ```
 *
 * 数据体不进入线协议：附件内容存 artifacts 文件（`path` 相对 artifacts 根目录），
 * 引用帧只携带路径 + 元数据；引用帧本身必须 <1MiB。
 */
export interface ArtifactRefEntry {
  /** 相对 artifacts 根目录的路径（禁止绝对路径 / `..` / 盘符 / UNC）。 */
  path: string;
  /** 附件字节数（核心侧与实际文件大小核对）。 */
  size: number;
  /** 附件类型等元数据（可选）。 */
  kind?: string;
}

export interface ArtifactRefParams {
  /** 所属会话（可选）。 */
  session_id?: string;
  /** 所属 run（可选）。 */
  run_id?: string;
  /** 附件引用列表。 */
  refs: ArtifactRefEntry[];
}

/** 核心 → 适配器方法表（D6）。 */
export const METHOD_TIMEOUTS_MS = {
  initialize: 10_000,
  "session.create": 30_000,
  "session.send": 30_000,
  "session.interrupt": 5_000,
  "session.dispose": 15_000,
  "tools.list": 10_000,
  "permission.resolve": 5_000,
  "health.ping": 5_000,
  shutdown: 5_000,
} as const;

export type RpcMethod = keyof typeof METHOD_TIMEOUTS_MS;

export const RPC_METHODS = Object.keys(METHOD_TIMEOUTS_MS) as RpcMethod[];

export function isRpcMethod(value: string): value is RpcMethod {
  return Object.prototype.hasOwnProperty.call(METHOD_TIMEOUTS_MS, value);
}

/** 适配器 → 核心通知方法（D6）。 */
export const NOTIFICATION_HELLO = "hello";
export const NOTIFICATION_EVENT = "event";
export const NOTIFICATION_PERMISSION_REQUEST = "permission.request";
export const NOTIFICATION_LOG = "log";

/** 错误码（D6：JSON-RPC 标准码 + 应用码 1001–1005）。 */
export const ERROR_CODES = {
  PARSE_ERROR: -32700,
  INVALID_REQUEST: -32600,
  METHOD_NOT_FOUND: -32601,
  INVALID_PARAMS: -32602,
  INTERNAL_ERROR: -32603,
  ADAPTER_CRASHED: 1001,
  REQUEST_TIMEOUT: 1002,
  VERSION_MISMATCH: 1003,
  CAPABILITY_MISSING: 1004,
  SESSION_NOT_FOUND: 1005,
} as const;

export type ErrorCodeName = keyof typeof ERROR_CODES;
export type ErrorCodeValue = (typeof ERROR_CODES)[ErrorCodeName];

export interface RpcErrorShape {
  code: number;
  message: string;
  data?: unknown;
}

export interface HelloPayload {
  protocol: string;
  runtime: {
    name: string;
    version: string;
    capabilities?: string[];
  };
}

/** 解析协议 major 段（`"1.0"` → `1`）。 */
export function protocolMajor(protocol: string): number | undefined {
  const head = protocol.split(".")[0];
  if (head === undefined || !/^\d+$/.test(head)) return undefined;
  return Number(head);
}

export function validateProtocol(protocol: string): boolean {
  return protocolMajor(protocol) === PROTOCOL_MAJOR;
}
