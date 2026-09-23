/**
 * 会话事件 → 工作台视图投影（M3-02；UI-01/02/05）。
 *
 * 数据来源：
 * - **消息基线**（`messages_page` 最近一页）：用户/助手历史气泡（`messages` 表）；
 * - **事件流**（EventStore，`aether://event`）：`message.delta` 流式增量、
 *   `message.completed` 终稿、`run.*` 状态、`tool.call_*` 工具调用。
 *
 * 归属键：
 * - 助手气泡以 `run:<run_id>` 为键——消息表落库的助手消息与实时事件投影到同一气泡，
 *   避免「刷新后重复气泡」；
 * - 用户气泡以 `msg:<message_id>` 为键（发送即乐观展示，历史从消息表回填）。
 */
import type { AetherEvent } from "@aether/protocol";

import type { MessageRow } from "./session";

export interface ChatBubble {
  key: string;
  role: "user" | "assistant";
  text: string;
  /** 流式进行中（`message.delta` 已到、终稿未到）。 */
  streaming: boolean;
  runId: string | null;
  ts: number;
  /** 排序稳定项（消息 seq / 事件 seq）。 */
  order: number;
}

export type ToolCallStatus = "running" | "completed" | "failed";

export interface ToolCallView {
  id: string;
  name: string;
  status: ToolCallStatus;
  errorCode?: string;
  durationMs?: number;
}

export type RunStatusView = "running" | "completed" | "failed" | "cancelled";

export interface RunView {
  runId: string;
  status: RunStatusView;
  errorCode?: string;
  errorMessage?: string;
  recoverable?: boolean;
}

export interface SessionProjection {
  bubbles: ChatBubble[];
  toolCalls: ToolCallView[];
  runs: RunView[];
  activeRunId: string | null;
  lastError: { code: string; message: string } | null;
}

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null
    ? (value as Record<string, unknown>)
    : {};
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function assistantKey(runId: string | null, messageId: string | undefined): string {
  return runId ? `run:${runId}` : `msg:${messageId ?? "unknown"}`;
}

/** 投影会话视图（纯函数；输入为消息基线与已发布事件）。 */
export function projectSession(
  events: AetherEvent[],
  messages: MessageRow[] = [],
): SessionProjection {
  const bubbles = new Map<string, ChatBubble>();
  const toolCalls = new Map<string, ToolCallView>();
  const runs = new Map<string, RunView>();
  let activeRunId: string | null = null;
  let lastError: SessionProjection["lastError"] = null;

  // 1) 消息基线（历史气泡；助手按 run 归属与实时事件合并）。
  for (const message of messages) {
    if (message.role === "user") {
      bubbles.set(`msg:${message.id}`, {
        key: `msg:${message.id}`,
        role: "user",
        text: message.content,
        streaming: false,
        runId: message.run_id ?? null,
        ts: message.created_at,
        order: message.seq,
      });
    } else if (message.role === "assistant") {
      const key = assistantKey(message.run_id ?? null, message.id);
      bubbles.set(key, {
        key,
        role: "assistant",
        text: message.content,
        streaming: false,
        runId: message.run_id ?? null,
        ts: message.created_at,
        order: message.seq,
      });
    }
  }

  // 2) 事件流（按到达顺序；EventStore 已按 seq 排序）。
  for (const event of events) {
    const payload = asRecord(event.payload);
    const runId = event.run_id ?? null;
    switch (event.type) {
      case "run.started": {
        if (runId) {
          runs.set(runId, { runId, status: "running" });
          activeRunId = runId;
          // 立即建立流式助手气泡（首 token 前显示生成中）。
          const key = assistantKey(runId, undefined);
          if (!bubbles.has(key)) {
            bubbles.set(key, {
              key,
              role: "assistant",
              text: "",
              streaming: true,
              runId,
              ts: event.ts,
              order: event.seq,
            });
          }
        }
        break;
      }
      case "message.delta": {
        const messageId = asString(payload.message_id);
        const text = asString(payload.text) ?? "";
        const key = assistantKey(runId, messageId);
        const existing = bubbles.get(key);
        if (existing) {
          existing.text += text;
          existing.streaming = true;
          existing.ts = event.ts;
        } else {
          bubbles.set(key, {
            key,
            role: "assistant",
            text,
            streaming: true,
            runId,
            ts: event.ts,
            order: event.seq,
          });
        }
        break;
      }
      case "message.completed": {
        const message = asRecord(payload.message);
        const content = asString(message.content) ?? "";
        const messageId = asString(message.id);
        const key = assistantKey(runId, messageId);
        const existing = bubbles.get(key);
        if (existing) {
          existing.text = content;
          existing.streaming = false;
          existing.ts = event.ts;
        } else {
          bubbles.set(key, {
            key,
            role: "assistant",
            text: content,
            streaming: false,
            runId,
            ts: event.ts,
            order: event.seq,
          });
        }
        break;
      }
      case "run.completed": {
        if (runId) {
          runs.set(runId, { runId, status: "completed" });
          if (activeRunId === runId) {
            activeRunId = null;
          }
          const bubble = bubbles.get(assistantKey(runId, undefined));
          if (bubble) {
            bubble.streaming = false;
          }
        }
        break;
      }
      case "run.failed": {
        const error = asRecord(payload.error);
        const code = asString(error.code) ?? "run_failed";
        const message = asString(error.message) ?? "run 失败";
        if (runId) {
          runs.set(runId, {
            runId,
            status: "failed",
            errorCode: code,
            errorMessage: message,
            recoverable: error.recoverable === true,
          });
          if (activeRunId === runId) {
            activeRunId = null;
          }
          const bubble = bubbles.get(assistantKey(runId, undefined));
          if (bubble) {
            bubble.streaming = false;
          }
        }
        lastError = { code, message };
        break;
      }
      case "run.cancelled": {
        if (runId) {
          runs.set(runId, { runId, status: "cancelled" });
          if (activeRunId === runId) {
            activeRunId = null;
          }
          const bubble = bubbles.get(assistantKey(runId, undefined));
          if (bubble) {
            bubble.streaming = false;
          }
        }
        break;
      }
      case "tool.call_started": {
        const id = asString(payload.tool_call_id) ?? `tool-${event.seq}`;
        toolCalls.set(id, {
          id,
          name: asString(payload.tool_name) ?? "tool",
          status: "running",
        });
        break;
      }
      case "tool.call_completed": {
        const id = asString(payload.tool_call_id) ?? `tool-${event.seq}`;
        const existing = toolCalls.get(id);
        toolCalls.set(id, {
          id,
          name: asString(payload.tool_name) ?? existing?.name ?? "tool",
          status: "completed",
          durationMs:
            typeof payload.duration_ms === "number" ? payload.duration_ms : undefined,
        });
        break;
      }
      case "tool.call_failed": {
        const id = asString(payload.tool_call_id) ?? `tool-${event.seq}`;
        const existing = toolCalls.get(id);
        const error = asRecord(payload.error);
        toolCalls.set(id, {
          id,
          name: asString(payload.tool_name) ?? existing?.name ?? "tool",
          status: "failed",
          errorCode: asString(error.code),
          durationMs:
            typeof payload.duration_ms === "number" ? payload.duration_ms : undefined,
        });
        break;
      }
      case "error": {
        lastError = {
          code: asString(payload.code) ?? "error",
          message: asString(payload.message) ?? "核心错误",
        };
        break;
      }
      default:
        break;
    }
  }

  const ordered = [...bubbles.values()].sort((left, right) => {
    if (left.ts !== right.ts) {
      return left.ts - right.ts;
    }
    return left.order - right.order;
  });
  return {
    bubbles: ordered,
    toolCalls: [...toolCalls.values()],
    runs: [...runs.values()],
    activeRunId,
    lastError,
  };
}
