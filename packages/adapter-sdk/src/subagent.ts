/**
 * M10-02/ADR-032：适配器 → 核心子 Agent 请求客户端（D6 增量）。
 *
 * 四个方法只增不改（major 1.0）：
 * - `subagent.spawn`（30s）
 * - `subagent.cancel`（5s）
 * - `subagent.status`（10s）
 * - `subagent.result.get`（10s）
 *
 * 适配器 SDK 暴露 `subagent_spawn` 工具定义；工具调用时经 {@link SubagentClient}
 * 向核心发起请求。请求失败不得静默降级。
 */

import type { JsonRpcPeer } from "./rpc";

export const SUBAGENT_SPAWN_METHOD = "subagent.spawn";
export const SUBAGENT_CANCEL_METHOD = "subagent.cancel";
export const SUBAGENT_STATUS_METHOD = "subagent.status";
export const SUBAGENT_RESULT_GET_METHOD = "subagent.result.get";

export const SUBAGENT_METHOD_TIMEOUTS_MS = {
  [SUBAGENT_SPAWN_METHOD]: 30_000,
  [SUBAGENT_CANCEL_METHOD]: 5_000,
  [SUBAGENT_STATUS_METHOD]: 10_000,
  [SUBAGENT_RESULT_GET_METHOD]: 10_000,
} as const;

/** `subagent_spawn` 工具定义（`tools.list` 增量；schema 为通用对象）。 */
export const SUBAGENT_SPAWN_TOOL = {
  name: "aether_subagent_spawn",
  description:
    "派生一个隔离的子 Agent（explore/review/custom）。只传递显式上下文；结果以 compact envelope 回注。",
  input_schema: {
    type: "object",
    required: ["parent_session_id", "role", "purpose"],
    properties: {
      parent_session_id: { type: "string", description: "父会话 ULID" },
      role: { type: "string", enum: ["explore", "review", "custom"] },
      purpose: { type: "string", maxLength: 2048 },
      mode: { type: "string", enum: ["explore", "review", "custom"] },
      template: {
        type: "string",
        enum: ["parallel_explore", "adversarial_review", "custom"],
      },
      context_refs: { type: "array", maxItems: 32, items: { type: "object" } },
      runtime_id: { type: "string" },
      provider_id: { type: "string" },
      model: { type: "string" },
      permission_mode: {
        type: "string",
        enum: ["read_only", "workspace_write", "danger_full_access"],
      },
      timeout_ms: { type: "integer", minimum: 1, maximum: 7_200_000 },
    },
  },
} as const;

/** 子 Agent 请求的稳定形状。 */
export interface SubagentSpawnParams {
  parent_session_id: string;
  role: "explore" | "review" | "custom";
  purpose: string;
  mode?: "explore" | "review" | "custom";
  template?: "parallel_explore" | "adversarial_review" | "custom";
  context_refs?: Array<{ kind: "text" | "file" | "directory"; path?: string; text?: string }>;
  runtime_id?: string;
  provider_id?: string;
  model?: string;
  permission_mode?: "read_only" | "workspace_write" | "danger_full_access";
  timeout_ms?: number;
}

export interface SubagentSpawnResult {
  child_session_id: string;
  status: string;
  depth: number;
  mode: string;
  batch_id: string;
}

export interface SubagentListResult {
  items: Array<Record<string, unknown>>;
}

export interface SubagentResultChunk {
  child_session_id: string;
  result_chunk: string;
  offset: number;
  next_offset: number | null;
  total_bytes: number;
  truncated: boolean;
}

export type SubagentDeliveryMode = "message_append" | "origin_fork" | "system_replace";

/** `session.send.subagent_context`（D6；≤16KiB）。 */
export interface SubagentContextPayload {
  version: number;
  block: string;
  mode: SubagentDeliveryMode;
}

/** 解析/校验 `session.send.subagent_context`；非法时抛错（适配器映射 invalid_params）。 */
export function parseSubagentContext(value: unknown): SubagentContextPayload | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "object" || Array.isArray(value)) {
    throw new Error("subagent_context 必须是对象");
  }
  const record = value as Record<string, unknown>;
  const version = record.version;
  const block = record.block;
  const mode = record.mode ?? "message_append";
  if (typeof version !== "number" || !Number.isInteger(version) || version < 1) {
    throw new Error("subagent_context.version 必须是 >=1 的整数");
  }
  if (typeof block !== "string" || Buffer.byteLength(block, "utf8") > 16 * 1024) {
    throw new Error("subagent_context.block 必须是 <=16KiB 字符串");
  }
  if (
    mode !== "message_append" &&
    mode !== "origin_fork" &&
    mode !== "system_replace"
  ) {
    throw new Error("subagent_context.mode 非法");
  }
  return { version, block, mode };
}

/**
 * 子 Agent 客户端（生产 main 注入 peer；测试可传替身）。
 */
export class SubagentClient {
  constructor(private readonly peer: Pick<JsonRpcPeer, "request">) {}

  spawn(params: SubagentSpawnParams): Promise<SubagentSpawnResult> {
    return this.request(SUBAGENT_SPAWN_METHOD, params);
  }

  cancel(childSessionId: string, reason?: string): Promise<{ status: string }> {
    return this.request(SUBAGENT_CANCEL_METHOD, {
      child_session_id: childSessionId,
      ...(reason ? { reason } : {}),
    });
  }

  status(childSessionId: string): Promise<Record<string, unknown>> {
    return this.request(SUBAGENT_STATUS_METHOD, { child_session_id: childSessionId });
  }

  list(parentSessionId: string, includeCompleted = false): Promise<SubagentListResult> {
    return this.request(SUBAGENT_STATUS_METHOD, {
      parent_session_id: parentSessionId,
      include_completed: includeCompleted,
    });
  }

  resultGet(
    parentSessionId: string,
    childSessionId: string,
    offset = 0,
    limitBytes = 16_384,
  ): Promise<SubagentResultChunk> {
    return this.request(SUBAGENT_RESULT_GET_METHOD, {
      parent_session_id: parentSessionId,
      child_session_id: childSessionId,
      offset,
      limit_bytes: limitBytes,
    });
  }

  private request<T>(method: string, params: unknown): Promise<T> {
    const timeout = SUBAGENT_METHOD_TIMEOUTS_MS[
      method as keyof typeof SUBAGENT_METHOD_TIMEOUTS_MS
    ];
    return this.peer.request(method, params, timeout) as Promise<T>;
  }
}
