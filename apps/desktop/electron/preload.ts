/**
 * Electron preload（M9-07 DoD-2；ADR-029 §5.1/P1-7）。
 *
 * 只通过 `contextBridge.exposeInMainWorld("aether", ...)` 暴露**五个**成员：
 * `rpc` / `onEvent` / `offEvent` / `pathForFile` / `version`。
 * 不暴露 `ipcRenderer`、`require`、`process`、`fs`、`child_process` 或任意 channel。
 *
 * 注意：renderer 在 `sandbox:true` 下运行；本文件由 Vite 打包为 CommonJS
 * （`preload.cjs`），sandboxed preload 不支持 ESM。
 */
import { contextBridge, ipcRenderer, webUtils } from "electron";

import { EVENT_IPC_CHANNEL, RPC_IPC_CHANNEL, VERSION_IPC_CHANNEL } from "./ipcChannels";

/** 单次 RPC 参数上限（与 ADR-029 D6 帧上限一致；main 仍会复核）。 */
const MAX_PAYLOAD_BYTES = 2 * 1024 * 1024;

export interface VersionInfo {
  app: string;
  daemon?: string;
  core?: string;
  protocol: string;
  electron: string;
  chrome: string;
  node: string;
}

export interface AetherIpcError {
  code: number;
  message: string;
  data?: {
    aether_code?: string;
    business_code?: string;
    details?: unknown;
    [key: string]: unknown;
  };
}

/**
 * `rpc` 的返回信封。
 *
 * 为什么不是 reject：`contextBridge` 跨隔离世界只保留标准 `Error` 的 message，
 * 自定义 `aether_code`/`business_code`/`details` 会丢失。返回可克隆对象是
 * Electron 官方推荐的跨桥错误传递方式；M9-08 的 typed client 负责把
 * `{ok:false,error}` 转成 `AetherError { aether_code, business_code?, details? }`。
 */
export interface RpcEnvelope {
  ok: boolean;
  result?: unknown;
  error?: AetherIpcError;
}

type EventHandler = (event: unknown) => void;

const handlerByCallback = new WeakMap<EventHandler, (_event: unknown, payload: unknown) => void>();

function payloadBytes(params: unknown): number {
  if (params === undefined) {
    return 0;
  }
  try {
    return new TextEncoder().encode(JSON.stringify(params)).byteLength;
  } catch {
    return Number.POSITIVE_INFINITY;
  }
}

function makeError(error: NonNullable<RpcEnvelope["error"]>): AetherIpcError {
  return {
    code: typeof error.code === "number" ? error.code : -32603,
    message: typeof error.message === "string" ? error.message : "核心响应缺少错误信息",
    ...(error.data ? { data: error.data } : {}),
  };
}

const api = {
  /** 调用 63 条命令之一（main 先查路由清单；daemon 为最终权威）。 */
  async rpc(method: string, params?: unknown): Promise<RpcEnvelope> {
    if (typeof method !== "string" || method.length === 0) {
      throw new TypeError("rpc(method) 必须是非空字符串");
    }
    if (payloadBytes(params) > MAX_PAYLOAD_BYTES) {
      return {
        ok: false,
        error: {
          code: -32003,
          message: "rpc 参数超过 2MiB 上限",
          data: { aether_code: "too_large" },
        },
      };
    }
    const envelope = (await ipcRenderer.invoke(RPC_IPC_CHANNEL, {
      method,
      params,
    })) as RpcEnvelope;
    if (envelope === null || typeof envelope !== "object") {
      return {
        ok: false,
        error: {
          code: -32603,
          message: "核心响应形状非法",
          data: { aether_code: "internal" },
        },
      };
    }
    if (envelope.ok) {
      return { ok: true, result: envelope.result };
    }
    return { ok: false, error: makeError(envelope.error ?? { code: -32603, message: "核心响应缺少错误信息" }) };
  },

  /** 订阅 `aether:event`（取消订阅用 `offEvent`）。 */
  onEvent(callback: EventHandler): void {
    if (typeof callback !== "function") {
      throw new TypeError("onEvent(callback) 必须传入函数");
    }
    const handler = (_event: unknown, payload: unknown): void => {
      callback(payload);
    };
    handlerByCallback.set(callback, handler);
    ipcRenderer.on(EVENT_IPC_CHANNEL, handler);
  },

  /** 取消订阅（必须与 `onEvent` 使用同一函数引用）。 */
  offEvent(callback: EventHandler): void {
    const handler = handlerByCallback.get(callback);
    if (handler === undefined) {
      return;
    }
    ipcRenderer.removeListener(EVENT_IPC_CHANNEL, handler);
    handlerByCallback.delete(callback);
  },

  /**
   * 拖拽文件路径（ADR-029 P1-7）：preload 内 `webUtils.getPathForFile`，
   * **只返回路径字符串**；不暴露 `webUtils`/`File` 其他能力。
   */
  pathForFile(file: File): string {
    if (file === null || typeof file !== "object") {
      throw new TypeError("pathForFile(file) 必须传入 File");
    }
    return webUtils.getPathForFile(file);
  },

  /** 应用/daemon/core/协议/运行时版本（诊断与关于页）。 */
  async version(): Promise<VersionInfo> {
    return (await ipcRenderer.invoke(VERSION_IPC_CHANNEL)) as VersionInfo;
  },
};

export type AetherPreloadApi = typeof api;

contextBridge.exposeInMainWorld("aether", Object.freeze(api));
