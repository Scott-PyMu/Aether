/**
 * `aetherd` stdio 客户端（M9-07；ADR-029 D-029-3 / §4.1 / §4.3）。
 *
 * 只做传输：JSON-RPC 2.0 over JSON-Lines、2MiB 帧上限、请求 id 匹配、
 * 通知分发、stderr 持续 drain（避免 pipe 满阻塞 daemon）。业务校验全部在
 * `aetherd`（main 的白名单只是第一道，ADR-029 §5.3）。
 *
 * 约束：
 * - 不依赖 Electron（便于单测）；生产由 main 注入 `child_process.spawn`；
 * - stdout 只接受 JSONL；超限/无法解析属于协议损坏 → 关闭连接并拒绝全部在途请求
 *   （ADR-029 §4.1：仅帧损坏/失序断连；不重启 daemon 的判定属 M9-09）；
 * - 首帧 `id:null` error = 预 `initialize` 致命失败（ADR-029 §4.2/§4.3）。
 */
import type { ChildProcessWithoutNullStreams, SpawnOptions } from "node:child_process";

/** 单帧上限（ADR-029 D6/§4.1：请求/响应/通知均 ≤2MiB）。 */
export const MAX_FRAME_BYTES = 2 * 1024 * 1024;
/** 默认请求超时（ADR-029 §4.8：无显式超时的默认 30s）。 */
export const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;
/** 默认 `initialize` 硬超时（ADR-029 §3.5：目标 ≤2s、硬超时 ≥10s）。 */
export const INITIALIZE_HARD_TIMEOUT_MS = 10_000;
/** stderr 环形缓冲（排障用；只保留尾部）。 */
export const STDERR_RING_LINES = 200;
/** stderr 环形缓冲字节上限。 */
export const STDERR_RING_BYTES = 64 * 1024;
/** JSON-RPC 传输/类别码（ADR-029 §4.3）。 */
export const RPC_CODE = {
  parseError: -32700,
  invalidRequest: -32600,
  methodNotFound: -32601,
  invalidParams: -32602,
  internalError: -32603,
  protocolMismatch: -32001,
  coreNotReady: -32002,
  tooLarge: -32003,
  shuttingDown: -32004,
  bootstrapFailure: -32005,
  capabilityMissing: -32006,
} as const;

/** daemon 错误 data（ADR-029 §4.3：`aether_code` + 可选 `business_code`）。 */
export interface RpcErrorData {
  aether_code?: string;
  business_code?: string;
  details?: unknown;
  field?: string;
  phase?: string;
  log_path?: string;
  reason?: string;
  [key: string]: unknown;
}

/** JSON-RPC error 对象。 */
export interface RpcErrorObject {
  code: number;
  message: string;
  data?: RpcErrorData;
}

/** daemon 返回的结构化错误（main → preload → renderer 原样透传）。 */
export class AetherRpcError extends Error {
  readonly code: number;
  readonly aetherCode?: string;
  readonly businessCode?: string;
  readonly data?: RpcErrorData;

  constructor(
    code: number,
    message: string,
    data?: RpcErrorData,
    options?: { cause?: unknown },
  ) {
    super(message);
    if (options?.cause !== undefined) {
      (this as { cause?: unknown }).cause = options.cause;
    }
    this.name = "AetherRpcError";
    this.code = code;
    this.data = data;
    this.aetherCode = typeof data?.aether_code === "string" ? data.aether_code : undefined;
    this.businessCode =
      typeof data?.business_code === "string" ? data.business_code : undefined;
  }

  /** 构造 `core_not_ready`（daemon 不可用/未握手完成）。 */
  static coreNotReady(message = "核心正在初始化，请稍候…"): AetherRpcError {
    return new AetherRpcError(RPC_CODE.coreNotReady, message, {
      aether_code: "core_not_ready",
    });
  }

  static fromObject(error: RpcErrorObject): AetherRpcError {
    const data =
      typeof error.data === "object" && error.data !== null ? error.data : undefined;
    return new AetherRpcError(error.code, error.message, data);
  }

  /** 序列化为 preload 可透传的形状（不依赖 Error 跨 IPC 的私有字段）。 */
  toWire(): RpcErrorObject {
    return {
      code: this.code,
      message: this.message,
      ...(this.data ? { data: this.data } : {}),
    };
  }
}

/** `initialize` 成功响应（ADR-029 §4.2）。 */
export interface InitializeResult {
  protocolVersion: string;
  daemonVersion: string;
  coreVersion: string;
  dataDir: string;
  phase: string;
  daemonCapabilities: string[];
}

/** `initialize` 请求参数（ADR-029 §4.2）。 */
export interface InitializeParams {
  protocolVersion: string;
  appVersion: string;
  client: string;
  clientCapabilities: string[];
  requiredCapabilities: string[];
}

/** 请求超时（区别于 daemon 返回的 `internal`；启动编排据此重试一次）。 */
export class AetherRpcTimeoutError extends AetherRpcError {
  constructor(method: string, timeoutMs: number) {
    super(RPC_CODE.internalError, `核心请求超时（${method}，${timeoutMs}ms）`, {
      aether_code: "internal",
    });
    this.name = "AetherRpcTimeoutError";
  }
}

/** daemon 通知。 */
export interface DaemonNotification {
  method: string;
  params?: unknown;
}

/** 进程退出信息。 */
export interface DaemonExit {
  code: number | null;
  signal: NodeJS.Signals | null;
}

export interface DaemonSpawn {
  command: string;
  args: string[];
  env: NodeJS.ProcessEnv;
  cwd?: string;
}

/** 注入点：默认 `child_process.spawn`；测试可替换。 */
export type SpawnFunction = (
  command: string,
  args: readonly string[],
  options: SpawnOptions,
) => ChildProcessWithoutNullStreams;

export interface DaemonClientOptions {
  spawn: SpawnFunction;
  spawnSpec: DaemonSpawn;
  /** 通知出口（`aether.event` / `daemon.status` / `permission.pending`）。 */
  onNotification?: (notification: DaemonNotification) => void;
  /** 首帧 `id:null` error（预 `initialize` 致命失败）。 */
  onBootstrapError?: (error: AetherRpcError) => void;
  /** stderr 行（已按行切分；同时写入环形缓冲）。 */
  onStderrLine?: (line: string) => void;
  /** 进程退出（只触发一次）。 */
  onExit?: (exit: DaemonExit) => void;
  /** 启动失败（ENOENT/EACCES 等）。 */
  onSpawnError?: (error: Error) => void;
  /** 默认请求超时（毫秒）。 */
  requestTimeoutMs?: number;
  /** 帧上限（默认 2MiB）。 */
  maxFrameBytes?: number;
}

interface PendingRequest {
  method: string;
  resolve: (value: unknown) => void;
  reject: (error: AetherRpcError) => void;
  timer: ReturnType<typeof setTimeout> | null;
}

/**
 * 单连接 stdio 客户端。
 *
 * 生命周期：`start()` → `initialize()` → 业务请求/通知 → `shutdown()`/`kill()`。
 * 连接关闭后所有在途请求以 `core_not_ready` 拒绝（不静默挂起）。
 */
export class DaemonClient {
  private readonly options: DaemonClientOptions;
  private readonly requestTimeoutMs: number;
  private readonly maxFrameBytes: number;
  private child: ChildProcessWithoutNullStreams | null = null;
  private buffer = Buffer.alloc(0);
  private stderrBuffer = "";
  private readonly stderrLines: string[] = [];
  private readonly pending = new Map<number, PendingRequest>();
  private nextId = 1;
  private closed = false;
  private exit: DaemonExit | null = null;
  private bootstrapError: AetherRpcError | null = null;
  private initialized = false;

  constructor(options: DaemonClientOptions) {
    this.options = options;
    this.requestTimeoutMs = options.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;
    this.maxFrameBytes = options.maxFrameBytes ?? MAX_FRAME_BYTES;
    if (this.requestTimeoutMs <= 0) {
      throw new Error("requestTimeoutMs 必须 >0");
    }
    if (this.maxFrameBytes <= 0) {
      throw new Error("maxFrameBytes 必须 >0");
    }
  }

  /** 启动子进程（`stdio: ['pipe','pipe','pipe']`，`windowsHide: true`）。 */
  start(): void {
    if (this.child !== null) {
      throw new Error("DaemonClient 已启动");
    }
    const { command, args, env, cwd } = this.options.spawnSpec;
    const child = this.options.spawn(command, args, {
      cwd,
      env,
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    this.child = child;

    child.stdout.on("data", (chunk: Buffer | string) => {
      this.handleStdout(typeof chunk === "string" ? Buffer.from(chunk) : chunk);
    });
    child.stderr.on("data", (chunk: Buffer | string) => {
      this.handleStderr(typeof chunk === "string" ? chunk : chunk.toString("utf8"));
    });
    child.on("error", (error: Error) => {
      this.closed = true;
      this.options.onSpawnError?.(error);
      this.rejectAll(
        new AetherRpcError(RPC_CODE.internalError, `无法启动核心进程：${error.message}`, {
          aether_code: "internal",
        }),
      );
    });
    child.on("exit", (code, signal) => {
      this.exit = { code, signal };
      this.closed = true;
      const error =
        this.bootstrapError ??
        new AetherRpcError(RPC_CODE.coreNotReady, "核心进程已退出", {
          aether_code: "core_not_ready",
        });
      this.rejectAll(error);
      this.options.onExit?.({ code, signal });
    });
  }

  /** 当前子进程 pid（未启动为 `null`）。 */
  get pid(): number | null {
    return this.child?.pid ?? null;
  }

  /** 是否已关闭（进程退出/致命协议错误）。 */
  get isClosed(): boolean {
    return this.closed;
  }

  /** 首帧致命错误（若有）。 */
  get bootstrapFailure(): AetherRpcError | null {
    return this.bootstrapError;
  }

  /** 进程退出信息（若有）。 */
  get exitInfo(): DaemonExit | null {
    return this.exit;
  }

  /** stderr 尾部（最多 {@link STDERR_RING_LINES} 行）。 */
  get stderrTail(): string {
    return this.stderrLines.join("\n");
  }

  /**
   * `initialize` 握手（必须是连接上的第一个请求）。
   *
   * 超时只拒绝本次请求；连接重建/重试一次由启动编排负责（ADR-029 §3.5）。
   */
  async initialize(
    params: InitializeParams,
    timeoutMs = INITIALIZE_HARD_TIMEOUT_MS,
  ): Promise<InitializeResult> {
    if (this.initialized) {
      throw new AetherRpcError(RPC_CODE.invalidRequest, "initialize 只能调用一次", {
        aether_code: "invalid_json",
      });
    }
    const result = await this.request("initialize", params, timeoutMs);
    this.initialized = true;
    return result as InitializeResult;
  }

  /** 业务请求（`params` 省略时按无参方法发送）。 */
  request(
    method: string,
    params?: unknown,
    timeoutMs = this.requestTimeoutMs,
  ): Promise<unknown> {
    if (this.closed || this.child === null) {
      return Promise.reject(
        this.bootstrapError ?? AetherRpcError.coreNotReady("核心未连接"),
      );
    }
    const id = this.nextId;
    this.nextId += 1;
    const frame: Record<string, unknown> = { jsonrpc: "2.0", id, method };
    if (params !== undefined) {
      frame.params = params;
    }
    const encoded = JSON.stringify(frame);
    if (Buffer.byteLength(encoded, "utf8") > this.maxFrameBytes) {
      return Promise.reject(
        new AetherRpcError(RPC_CODE.tooLarge, "请求帧超过 2MiB 上限", {
          aether_code: "too_large",
        }),
      );
    }

    return new Promise<unknown>((resolve, reject) => {
      const timer =
        timeoutMs > 0
          ? setTimeout(() => {
              this.pending.delete(id);
              reject(new AetherRpcTimeoutError(method, timeoutMs));
            }, timeoutMs)
          : null;
      this.pending.set(id, { method, resolve, reject, timer });
      this.child?.stdin.write(`${encoded}\n`, (error) => {
        if (error) {
          this.settleReject(
            id,
            new AetherRpcError(RPC_CODE.coreNotReady, `写入核心失败：${error.message}`, {
              aether_code: "core_not_ready",
            }),
          );
        }
      });
    });
  }

  /** 发送 `shutdown` 并等待进程退出（回执 ≠ 关闭完成；ADR-029 §3.6）。 */
  async shutdown(timeoutMs = 5_000): Promise<DaemonExit | null> {
    if (this.closed || this.child === null) {
      return this.exit;
    }
    const started = Date.now();
    try {
      // 回执应立即返回；只给回执一小段预算，剩余预算留给进程退出。
      await this.request("shutdown", undefined, Math.min(2_000, timeoutMs));
    } catch {
      // 回执失败不阻塞退出等待（daemon 可能已在关闭序列）。
    }
    const remaining = Math.max(0, timeoutMs - (Date.now() - started));
    return this.waitForExit(remaining);
  }

  /** 等待进程退出；超时返回 `null`（强杀由调用方/M9-09 负责）。 */
  waitForExit(timeoutMs: number): Promise<DaemonExit | null> {
    if (this.exit !== null) {
      return Promise.resolve(this.exit);
    }
    if (this.child === null) {
      return Promise.resolve(null);
    }
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        resolve(null);
      }, timeoutMs);
      this.child?.once("exit", (code, signal) => {
        clearTimeout(timer);
        resolve({ code, signal });
      });
    });
  }

  /** 强制结束（整树回收由 M9-09；此处只发信号，不裸杀进程树）。 */
  kill(signal: NodeJS.Signals = "SIGTERM"): void {
    if (this.child === null) {
      return;
    }
    try {
      this.child.kill(signal);
    } catch {
      // 进程可能已退出。
    }
  }

  /** 关闭 stdin（daemon 侧 stdin EOF 优雅关闭；ADR-029 §3.6-5）。 */
  closeInput(): void {
    try {
      this.child?.stdin.end();
    } catch {
      // 已关闭。
    }
  }

  private handleStdout(chunk: Buffer): void {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    while (true) {
      const newline = this.buffer.indexOf(0x0a);
      if (newline < 0) {
        break;
      }
      const line = this.buffer.subarray(0, newline);
      this.buffer = this.buffer.subarray(newline + 1);
      if (line.length > this.maxFrameBytes) {
        this.protocolFailure("核心输出帧超过 2MiB 上限");
        return;
      }
      if (line.length === 0) {
        continue;
      }
      this.handleLine(line.toString("utf8"));
      if (this.closed) {
        return;
      }
    }
    if (this.buffer.length > this.maxFrameBytes) {
      this.protocolFailure("核心输出帧超过 2MiB 上限（未终结）");
    }
  }

  private handleLine(line: string): void {
    let frame: unknown;
    try {
      frame = JSON.parse(line);
    } catch {
      this.protocolFailure("核心输出不是合法 JSONL（帧损坏）");
      return;
    }
    if (typeof frame !== "object" || frame === null) {
      this.protocolFailure("核心输出不是 JSON-RPC 对象");
      return;
    }
    const record = frame as Record<string, unknown>;
    if (record.jsonrpc !== "2.0") {
      this.protocolFailure("核心输出缺少 jsonrpc=2.0");
      return;
    }
    const id = record.id;
    const hasError = typeof record.error === "object" && record.error !== null;
    const hasResult = Object.prototype.hasOwnProperty.call(record, "result");

    if (id === null || id === undefined) {
      if (hasError) {
        const error = AetherRpcError.fromObject(record.error as RpcErrorObject);
        this.bootstrapError = error;
        this.options.onBootstrapError?.(error);
        this.rejectAll(error);
      } else if (typeof record.method === "string") {
        this.options.onNotification?.({
          method: record.method,
          params: record.params,
        });
      }
      return;
    }

    if (typeof id !== "number") {
      this.protocolFailure("核心响应 id 类型非法");
      return;
    }
    const pending = this.pending.get(id);
    if (pending === undefined) {
      // 迟到的响应按 id 丢弃（ADR-029 §4.8）。
      return;
    }
    this.pending.delete(id);
    if (pending.timer !== null) {
      clearTimeout(pending.timer);
    }
    if (hasError) {
      pending.reject(AetherRpcError.fromObject(record.error as RpcErrorObject));
      return;
    }
    if (!hasResult) {
      pending.reject(
        new AetherRpcError(RPC_CODE.parseError, "核心响应缺少 result/error", {
          aether_code: "invalid_json",
        }),
      );
      return;
    }
    pending.resolve(record.result);
  }

  private handleStderr(text: string): void {
    this.stderrBuffer += text;
    let newline = this.stderrBuffer.indexOf("\n");
    while (newline >= 0) {
      const line = this.stderrBuffer.slice(0, newline).replace(/\r$/, "");
      this.stderrBuffer = this.stderrBuffer.slice(newline + 1);
      this.pushStderrLine(line);
      newline = this.stderrBuffer.indexOf("\n");
    }
    if (this.stderrBuffer.length > STDERR_RING_BYTES) {
      const overflow = this.stderrBuffer.length - STDERR_RING_BYTES;
      this.stderrBuffer = this.stderrBuffer.slice(overflow);
    }
  }

  private pushStderrLine(line: string): void {
    if (line.length === 0) {
      return;
    }
    this.stderrLines.push(line);
    while (this.stderrLines.length > STDERR_RING_LINES) {
      this.stderrLines.shift();
    }
    let total = 0;
    for (const item of this.stderrLines) {
      total += Buffer.byteLength(item, "utf8") + 1;
    }
    while (total > STDERR_RING_BYTES && this.stderrLines.length > 1) {
      const removed = this.stderrLines.shift();
      total -= Buffer.byteLength(removed ?? "", "utf8") + 1;
    }
    this.options.onStderrLine?.(line);
  }

  private protocolFailure(message: string): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.rejectAll(
      new AetherRpcError(RPC_CODE.parseError, message, { aether_code: "invalid_json" }),
    );
    this.kill("SIGTERM");
  }

  private settleReject(id: number, error: AetherRpcError): void {
    const pending = this.pending.get(id);
    if (pending === undefined) {
      return;
    }
    this.pending.delete(id);
    if (pending.timer !== null) {
      clearTimeout(pending.timer);
    }
    pending.reject(error);
  }

  private rejectAll(error: AetherRpcError): void {
    const entries = [...this.pending.entries()];
    this.pending.clear();
    for (const [, pending] of entries) {
      if (pending.timer !== null) {
        clearTimeout(pending.timer);
      }
      pending.reject(error);
    }
  }
}
