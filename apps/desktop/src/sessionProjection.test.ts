/**
 * 会话投影单测（M3-02）：消息基线与事件流的合并（无重复气泡）、run 状态、
 * 工具调用、错误收口。
 */
import type { AetherEvent } from "@aether/protocol";
import { describe, expect, it } from "vitest";

import type { MessageRow } from "./session";
import { projectSession } from "./sessionProjection";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const RUN = "01J8ZQ5R0N7W9Y8X6V4T2S0K1R";

let idCounter = 0;

function event(
  seq: number,
  type: string,
  payload: unknown,
  runId: string | null = RUN,
): AetherEvent {
  idCounter += 1;
  return {
    v: 1,
    id: `01J${String(idCounter).padStart(23, "0")}`,
    session_id: SESSION,
    run_id: runId,
    runtime_id: "mock",
    seq,
    ts: 1_700_000_000_000 + seq,
    type,
    payload,
  };
}

function messageRow(overrides: Partial<MessageRow>): MessageRow {
  return {
    id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
    session_id: SESSION,
    run_id: null,
    client_msg_id: null,
    role: "user",
    content: "你好",
    seq: 1,
    created_at: 1_700_000_000_001,
    ...overrides,
  };
}

describe("projectSession（M3-02）", () => {
  it("用户消息基线 + 助手流式事件 → 两条气泡，终稿覆盖流式", () => {
    const messages = [messageRow({})];
    const events = [
      event(1, "run.started", { run_id: RUN }),
      event(2, "message.delta", { message_id: "m-1", text: "你" }),
      event(3, "message.delta", { message_id: "m-1", text: "好" }),
      event(4, "message.completed", {
        message: {
          id: "m-1",
          session_id: SESSION,
          run_id: RUN,
          role: "assistant",
          content: "你好",
          created_at: 1_700_000_000_010,
        },
        usage: null,
      }),
      event(5, "run.completed", { run_id: RUN, usage: null }),
    ];
    const projection = projectSession(events, messages);
    expect(projection.bubbles).toHaveLength(2);
    expect(projection.bubbles[0]).toMatchObject({ role: "user", text: "你好" });
    expect(projection.bubbles[1]).toMatchObject({
      role: "assistant",
      text: "你好",
      streaming: false,
      runId: RUN,
    });
    expect(projection.activeRunId).toBeNull();
    expect(projection.runs).toEqual([{ runId: RUN, status: "completed" }]);
  });

  it("助手历史消息（messages 表）与实时事件合并为同一气泡（无重复）", () => {
    const messages = [
      messageRow({}),
      messageRow({
        id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1N",
        run_id: RUN,
        role: "assistant",
        content: "历史终稿",
        seq: 2,
        created_at: 1_700_000_000_020,
      }),
    ];
    const events = [
      event(1, "run.started", { run_id: RUN }),
      event(2, "message.delta", { message_id: "m-1", text: "增量" }),
    ];
    const projection = projectSession(events, messages);
    expect(projection.bubbles).toHaveLength(2);
    expect(projection.bubbles[1]?.key).toBe(`run:${RUN}`);
    expect(projection.bubbles[1]?.text).toBe("历史终稿增量");
  });

  it("run 失败/取消与工具调用状态收口", () => {
    const events = [
      event(1, "run.started", { run_id: RUN }),
      event(2, "tool.call_started", {
        tool_call_id: "t-1",
        tool_name: "mock.read_file",
        args: {},
      }),
      event(3, "tool.call_failed", {
        tool_call_id: "t-1",
        tool_name: "mock.read_file",
        duration_ms: 7,
        error: { code: "io_error", message: "读失败", recoverable: true },
      }),
      event(4, "run.failed", {
        run_id: RUN,
        error: { code: "api_error", message: "503", recoverable: true },
      }),
    ];
    const projection = projectSession(events, []);
    expect(projection.toolCalls).toEqual([
      {
        id: "t-1",
        name: "mock.read_file",
        status: "failed",
        errorCode: "io_error",
        durationMs: 7,
      },
    ]);
    expect(projection.runs[0]).toMatchObject({
      runId: RUN,
      status: "failed",
      errorCode: "api_error",
      recoverable: true,
    });
    expect(projection.lastError).toEqual({ code: "api_error", message: "503" });
    expect(projection.activeRunId).toBeNull();
  });

  it("运行中：activeRunId 指向最近 run.started，助手气泡保持流式", () => {
    const events = [
      event(1, "run.started", { run_id: RUN }),
      event(2, "message.delta", { message_id: "m-1", text: "生成" }),
    ];
    const projection = projectSession(events, []);
    expect(projection.activeRunId).toBe(RUN);
    expect(projection.bubbles[0]?.streaming).toBe(true);
    expect(projection.runs[0]?.status).toBe("running");
  });

  // M3-06：取消原因透传（存储降级中断 → UI 展示「已中断（存储降级）」）。
  it("run.cancelled 透传取消原因（persist_degraded）", () => {
    const events = [
      event(1, "run.started", { run_id: RUN }),
      event(2, "run.cancelled", { run_id: RUN, reason: "persist_degraded" }),
    ];
    const projection = projectSession(events, []);
    expect(projection.runs[0]).toMatchObject({
      runId: RUN,
      status: "cancelled",
      cancelReason: "persist_degraded",
    });
  });
});
