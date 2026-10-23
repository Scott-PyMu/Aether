/**
 * JSON-RPC 2.0 over JSON-Lines 对等端（D6）。
 *
 * 适配器侧职责：
 * - 接收核心请求（9 方法表）并回 result/error；
 * - 未知方法回 `-32601` 且**不断连**（D6 前向兼容）；
 * - 发送通知（`hello` / `event` / `permission.request` / `log`）；
 * - 无效帧由调用方计数（D6：连续 20 次判不健康）。
 */

import { ERROR_CODES } from "./protocol";

export class RpcError extends Error {
  constructor(
    readonly code: number,
    message: string,
    readonly data?: unknown,
  ) {
    super(message);
    this.name = "RpcError";
  }
}

export interface JsonRpcRequest {
  jsonrpc: "2.0";
  id: number | string;
  method: string;
  params?: unknown;
}

export interface JsonRpcNotification {
  jsonrpc: "2.0";
  method: string;
  params?: unknown;
}

export interface JsonRpcResponse {
  jsonrpc: "2.0";
  id: number | string;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}

export type RequestHandler = (method: string, params: unknown) => unknown | Promise<unknown>;
export type NotificationHandler = (method: string, params: unknown) => void | Promise<void>;

export interface JsonRpcPeerOptions {
  onRequest: RequestHandler;
  onNotification: NotificationHandler;
  onResponse?: (id: number | string, result: unknown) => void;
}

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  timer: NodeJS.Timeout | null;
}

export class JsonRpcPeer {
  private readonly pending = new Map<string, PendingRequest>();
  private nextRequestId = 1;

  constructor(
    private readonly writeLine: (line: string) => Promise<void>,
    private readonly options: JsonRpcPeerOptions,
  ) {}

  /**
   * 适配器 → 核心请求（ADR-028 v0.9：仅登记 `provider.catalog.get`）。
   *
   * 请求 id 使用独立命名空间（`a:<n>` 字符串），与核心 → 适配器数字 id 按方向 +
   * id 匹配，互不冲突；超时/断连清除待决表。
   */
  request(method: string, params: unknown, timeoutMs = 5_000): Promise<unknown> {
    const id = `a:${this.nextRequestId}`;
    this.nextRequestId += 1;
    return new Promise<unknown>((resolve, reject) => {
      const timer =
        timeoutMs > 0
          ? setTimeout(() => {
              this.pending.delete(id);
              reject(new RpcError(-32603, `${method} 超时（${timeoutMs}ms）`, {
                aether_code: "core_not_ready",
              }));
            }, timeoutMs)
          : null;
      this.pending.set(id, { resolve, reject, timer });
      void this.writeLine(
        JSON.stringify({ jsonrpc: "2.0", id, method, params } as JsonRpcRequest),
      ).catch((error: unknown) => {
        const pending = this.pending.get(id);
        if (!pending) return;
        this.pending.delete(id);
        if (pending.timer) clearTimeout(pending.timer);
        pending.reject(error instanceof Error ? error : new Error(String(error)));
      });
    });
  }

  /**
   * 处理一行帧；返回 `false` 表示帧非法（调用方自行计数/断连）。
   * 请求处理器异常映射为 JSON-RPC error（`RpcError` 透传 code/message）。
   */
  async handleLine(line: string): Promise<boolean> {
    let message: unknown;
    try {
      message = JSON.parse(line);
    } catch {
      return false;
    }
    if (typeof message !== "object" || message === null || Array.isArray(message)) {
      return false;
    }
    const frame = message as Record<string, unknown>;
    if (typeof frame.jsonrpc !== "string") {
      return false;
    }

    if (typeof frame.method === "string") {
      const method = frame.method;
      const params = frame.params ?? null;
      const hasId = "id" in frame && frame.id !== null && frame.id !== undefined;
      if (hasId) {
        const id = frame.id as number | string;
        try {
          const result = await this.options.onRequest(method, params);
          await this.reply(id, result ?? null);
        } catch (error) {
          if (error instanceof RpcError) {
            await this.replyError(id, error.code, error.message, error.data);
          } else {
            const detail = error instanceof Error ? error.message : String(error);
            await this.replyError(id, ERROR_CODES.INTERNAL_ERROR, detail);
          }
        }
      } else {
        await this.options.onNotification(method, params);
      }
      return true;
    }

    if ("id" in frame && ("result" in frame || "error" in frame)) {
      const id = frame.id as number | string;
      const pending = this.pending.get(String(id));
      if (pending) {
        this.pending.delete(String(id));
        if (pending.timer) clearTimeout(pending.timer);
        const error = frame.error as
          | { code?: number; message?: string; data?: unknown }
          | undefined;
        if (error) {
          pending.reject(
            new RpcError(error.code ?? ERROR_CODES.INTERNAL_ERROR, error.message ?? "RPC 错误", error.data),
          );
        } else {
          pending.resolve(frame.result);
        }
      }
      this.options.onResponse?.(id, frame.result);
      return true;
    }

    return false;
  }

  async notify(method: string, params?: unknown): Promise<void> {
    const frame: JsonRpcNotification = { jsonrpc: "2.0", method };
    if (params !== undefined) frame.params = params;
    await this.writeLine(JSON.stringify(frame));
  }

  /**
   * 发送带附加顶层字段的通知（M2-09 `artifact_ref` 需要顶层 `"type"` 判别键；
   * 行顶层 `type` 是帧层探测的判别依据，`notify` 的标准形状无法表达）。
   */
  async notifyFrame(
    method: string,
    extra: Record<string, unknown>,
    params?: unknown,
  ): Promise<void> {
    const frame: JsonRpcNotification & Record<string, unknown> = {
      jsonrpc: "2.0",
      method,
      ...extra,
    };
    if (params !== undefined) frame.params = params;
    await this.writeLine(JSON.stringify(frame));
  }

  async reply(id: number | string, result: unknown): Promise<void> {
    const frame: JsonRpcResponse = { jsonrpc: "2.0", id, result };
    await this.writeLine(JSON.stringify(frame));
  }

  async replyError(
    id: number | string,
    code: number,
    message: string,
    data?: unknown,
  ): Promise<void> {
    const frame: JsonRpcResponse = { jsonrpc: "2.0", id, error: { code, message } };
    if (data !== undefined) frame.error!.data = data;
    await this.writeLine(JSON.stringify(frame));
  }
}
