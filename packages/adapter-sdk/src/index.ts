/**
 * @aether/adapter-sdk —— Aether TS 适配器 SDK（设计 D6）。
 *
 * 适配器与核心只经 JSON-RPC 2.0 over stdio 通信；本包提供帧、握手、信封与 RPC 对等端。
 */

export {
  ARTIFACT_REF_LIMIT_BYTES,
  ARTIFACT_REF_METHOD,
  ARTIFACT_REF_TYPE,
  ERROR_CODES,
  HANDSHAKE_TIMEOUT_MS,
  INVALID_FRAME_UNHEALTHY_THRESHOLD,
  isRpcMethod,
  MAX_FRAME_BYTES,
  METHOD_TIMEOUTS_MS,
  NOTIFICATION_EVENT,
  NOTIFICATION_HELLO,
  NOTIFICATION_LOG,
  NOTIFICATION_PERMISSION_REQUEST,
  PROTOCOL_MAJOR,
  PROTOCOL_MINOR,
  PROTOCOL_VERSION,
  protocolMajor,
  RPC_METHODS,
  validateProtocol,
  type ArtifactRefEntry,
  type ArtifactRefParams,
  type ErrorCodeName,
  type ErrorCodeValue,
  type HelloPayload,
  type RpcErrorShape,
  type RpcMethod,
} from "./protocol";
export {
  FramingError,
  encodeLine,
  LineReader,
} from "./framing";
export {
  buildEnvelope,
  EVENT_ENVELOPE_VERSION,
  SessionSequencer,
  type EnvelopeContext,
  type EventEnvelope,
} from "./envelope";
export {
  isUlid,
  resetUlidState,
  ulid,
} from "./ulid";
export {
  JsonRpcPeer,
  RpcError,
  type JsonRpcNotification,
  type JsonRpcRequest,
  type JsonRpcResponse,
  type JsonRpcPeerOptions,
  type NotificationHandler,
  type RequestHandler,
} from "./rpc";
export {
  Adapter,
  type AdapterOptions,
  type AdapterRuntimeInfo,
  type MethodHandler,
} from "./adapter";
export { stdinLines, stdoutWriter, stderrLogger, type LineWriter } from "./stdio";
