import { describe, expect, it } from "vitest";

import {
  CLAUDE_ERROR_CODES,
  ClaudeRunMapper,
  mapUsage,
  toolResultText,
  type MappedEvent,
} from "./claude-events";

const CONTEXT = { sessionId: "sess-1", runId: "run-1", messageId: "msg-1" };

function types(events: MappedEvent[]): string[] {
  return events.map((event) => event.type);
}

function payloadOf(events: MappedEvent[], type: string): Record<string, unknown> {
  const found = events.find((event) => event.type === type);
  expect(found, `缺少事件 ${type}`).toBeDefined();
  return found?.payload ?? {};
}

function pushAndCollect(mapper: ClaudeRunMapper, events: unknown[]): MappedEvent[] {
  const out: MappedEvent[] = [];
  for (const event of events) out.push(...mapper.push(event));
  return out;
}

describe("ClaudeRunMapper：流式与终态（附录 B）", () => {
  it("text_delta → message.delta；result → message.completed + run.completed（usage 映射）", () => {
    const mapper = new ClaudeRunMapper(CONTEXT, () => 1000);
    const streamed = pushAndCollect(mapper, [
      { type: "system", subtype: "init", session_id: "native-1", tools: ["Bash", "Read"] },
      { type: "stream_event", event: { type: "content_block_delta", delta: { type: "text_delta", text: "你好" } } },
      { type: "stream_event", event: { type: "content_block_delta", delta: { type: "text_delta", text: "，世界" } } },
      { type: "stream_event", event: { type: "content_block_delta", delta: { type: "thinking_delta", thinking: "x" } } },
      { type: "stream_event", event: { type: "content_block_delta", delta: { type: "text_delta", text: "" } } },
      {
        type: "result",
        subtype: "success",
        is_error: false,
        result: "你好，世界",
        usage: { input_tokens: 3, output_tokens: 5 },
      },
    ]);
    expect(types(streamed)).toEqual(["message.delta", "message.delta"]);
    expect(payloadOf(streamed, "message.delta")).toEqual({ message_id: "msg-1", text: "你好" });
    expect(mapper.streamedText).toBe("你好，世界");
    expect(mapper.initEvent?.session_id).toBe("native-1");

    const final = mapper.finish({ reason: "process-exit", exitCode: 0, signal: null });
    expect(types(final)).toEqual(["message.completed", "run.completed"]);
    const message = payloadOf(final, "message.completed")["message"] as Record<string, unknown>;
    expect(message).toMatchObject({
      id: "msg-1",
      session_id: "sess-1",
      run_id: "run-1",
      role: "assistant",
      content: "你好，世界",
    });
    expect(payloadOf(final, "message.completed")["usage"]).toEqual({
      input_tokens: 3,
      output_tokens: 5,
      total_tokens: 8,
    });
    expect(payloadOf(final, "run.completed")["run_id"]).toBe("run-1");
    // finish 幂等。
    expect(mapper.finish({ reason: "process-exit" })).toEqual([]);
  });

  it("无 delta 时 finalText 回退 result.result；无 result 时判 cli_exit 失败", () => {
    const mapper = new ClaudeRunMapper(CONTEXT);
    pushAndCollect(mapper, [{ type: "result", result: "整段到达", is_error: false }]);
    expect(mapper.streamedText).toBe("");
    expect(mapper.finalText).toBe("整段到达");
    const final = mapper.finish({ reason: "process-exit", exitCode: 0 });
    const message = payloadOf(final, "message.completed")["message"] as Record<string, unknown>;
    expect(message["content"]).toBe("整段到达");

    const empty = new ClaudeRunMapper(CONTEXT);
    expect(empty.finalText).toBe("");
    const emptyFinal = empty.finish({ reason: "process-exit", exitCode: 0 });
    expect(types(emptyFinal)).toEqual(["run.failed"]);
    expect(
      (payloadOf(emptyFinal, "run.failed")["error"] as Record<string, unknown>)["code"],
    ).toBe(CLAUDE_ERROR_CODES.CLI_EXIT);
  });

  it("assistant tool_use → tool.call_started（幂等）；user tool_result → completed/failed", () => {
    let clock = 10_000;
    const mapper = new ClaudeRunMapper(CONTEXT, () => clock);
    const started = pushAndCollect(mapper, [
      {
        type: "assistant",
        message: {
          content: [
            { type: "text", text: "忽略" },
            { type: "tool_use", id: "toolu_1", name: "Write", input: { path: "a.md" } },
          ],
        },
      },
      {
        type: "assistant",
        message: { content: [{ type: "tool_use", id: "toolu_1", name: "Write", input: { path: "a.md" } }] },
      },
    ]);
    expect(types(started)).toEqual(["tool.call_started"]);
    expect(payloadOf(started, "tool.call_started")).toEqual({
      tool_call_id: "toolu_1",
      tool_name: "Write",
      args: { path: "a.md" },
    });
    expect(mapper.openToolCalls).toBe(1);

    clock += 42;
    const finishers = pushAndCollect(mapper, [
      {
        type: "user",
        message: {
          content: [{ type: "tool_result", tool_use_id: "toolu_1", is_error: false, content: "ok" }],
        },
      },
    ]);
    expect(types(finishers)).toEqual(["tool.call_completed"]);
    expect(payloadOf(finishers, "tool.call_completed")).toEqual({
      tool_call_id: "toolu_1",
      tool_name: "Write",
      duration_ms: 42,
    });
    expect(mapper.openToolCalls).toBe(0);

    const second = new ClaudeRunMapper(CONTEXT, () => clock);
    pushAndCollect(second, [
      { type: "assistant", message: { content: [{ type: "tool_use", id: "toolu_2", name: "Read", input: {} }] } },
    ]);
    const failed = pushAndCollect(second, [
      {
        type: "user",
        message: {
          content: [
            { type: "tool_result", tool_use_id: "toolu_2", is_error: true, content: [{ type: "text", text: "ENOENT" }] },
          ],
        },
      },
    ]);
    expect(types(failed)).toEqual(["tool.call_failed"]);
    const error = payloadOf(failed, "tool.call_failed")["error"] as Record<string, unknown>;
    expect(error["code"]).toBe(CLAUDE_ERROR_CODES.TOOL_EXECUTION_FAILED);
    expect(error["message"]).toBe("ENOENT");
  });

  it("interrupt → 未收口工具 call_failed(timeout/abort) + run.cancelled", () => {
    const mapper = new ClaudeRunMapper(CONTEXT);
    pushAndCollect(mapper, [
      { type: "assistant", message: { content: [{ type: "tool_use", id: "toolu_9", name: "Bash", input: {} }] } },
    ]);
    const final = mapper.finish({ reason: "interrupted" });
    expect(types(final)).toEqual(["tool.call_failed", "run.cancelled"]);
    const error = payloadOf(final, "tool.call_failed")["error"] as Record<string, unknown>;
    expect(error["code"]).toBe(CLAUDE_ERROR_CODES.TOOL_TIMEOUT);
    expect(String(error["message"])).toContain("abort");
    expect(payloadOf(final, "run.cancelled")).toEqual({ run_id: "run-1", reason: "interrupted" });
  });

  it("timeout → 工具收口 + run.failed(run_timeout, recoverable)", () => {
    const mapper = new ClaudeRunMapper(CONTEXT);
    const final = mapper.finish({ reason: "timeout", detail: "超出 100ms" });
    expect(types(final)).toEqual(["run.failed"]);
    const error = payloadOf(final, "run.failed")["error"] as Record<string, unknown>;
    expect(error["code"]).toBe(CLAUDE_ERROR_CODES.RUN_TIMEOUT);
    expect(error["recoverable"]).toBe(true);
  });

  it("spawn-error / child-unhealthy / 无 result → run.failed（对应错误码）", () => {
    const spawn = new ClaudeRunMapper(CONTEXT).finish({ reason: "spawn-error", detail: "ENOENT" });
    expect(types(spawn)).toEqual(["run.failed"]);
    expect((payloadOf(spawn, "run.failed")["error"] as Record<string, unknown>)["code"]).toBe(
      CLAUDE_ERROR_CODES.SPAWN_FAILED,
    );

    const unhealthy = new ClaudeRunMapper(CONTEXT).finish({
      reason: "child-unhealthy",
      detail: "连续 20 次",
    });
    expect((payloadOf(unhealthy, "run.failed")["error"] as Record<string, unknown>)["code"]).toBe(
      CLAUDE_ERROR_CODES.CLI_EXIT,
    );

    const orphan = new ClaudeRunMapper(CONTEXT).finish({ reason: "process-exit", exitCode: 7, signal: null });
    const error = payloadOf(orphan, "run.failed")["error"] as Record<string, unknown>;
    expect(error["code"]).toBe(CLAUDE_ERROR_CODES.CLI_EXIT);
    expect(String(error["message"])).toContain("exit=7");

    const unhealthyWithTool = new ClaudeRunMapper(CONTEXT);
    pushAndCollect(unhealthyWithTool, [
      { type: "assistant", message: { content: [{ type: "tool_use", id: "toolu_x", name: "Bash", input: {} }] } },
    ]);
    expect(types(unhealthyWithTool.finish({ reason: "child-unhealthy" }))).toEqual([
      "tool.call_failed",
      "run.failed",
    ]);
  });

  it("result.is_error → run.failed(api_error)（含 api_error_status）；成功终态不产出", () => {
    const mapper = new ClaudeRunMapper(CONTEXT);
    pushAndCollect(mapper, [
      { type: "result", is_error: true, subtype: "error_during_execution", api_error_status: 503, result: "503" },
    ]);
    const final = mapper.finish({ reason: "process-exit", exitCode: 1 });
    expect(types(final)).toEqual(["run.failed"]);
    const error = payloadOf(final, "run.failed")["error"] as Record<string, unknown>;
    expect(error["code"]).toBe(CLAUDE_ERROR_CODES.API_ERROR);
    expect(String(error["message"])).toContain("503");
  });

  it("未知/畸形事件忽略且不抛错；工具结果缺 started 时忽略", () => {
    const mapper = new ClaudeRunMapper(CONTEXT);
    expect(
      pushAndCollect(mapper, [
        null,
        42,
        "text",
        [],
        { type: "unknown.future" },
        { type: "system", subtype: "other" },
        { type: "stream_event" },
        { type: "stream_event", event: { type: "content_block_delta", delta: { type: "text_delta" } } },
        { type: "assistant" },
        { type: "assistant", message: { content: [{ type: "tool_use" }] } },
        { type: "user", message: { content: [{ type: "tool_result", tool_use_id: "nope" }] } },
      ]),
    ).toEqual([]);
  });
});

describe("辅助函数", () => {
  it("mapUsage：缺省 total 为 input+output；缺失返回 null", () => {
    expect(mapUsage({ input_tokens: 1, output_tokens: 2 })).toEqual({
      input_tokens: 1,
      output_tokens: 2,
      total_tokens: 3,
    });
    expect(mapUsage({ input_tokens: 1, output_tokens: 2, total_tokens: 9 })).toEqual({
      input_tokens: 1,
      output_tokens: 2,
      total_tokens: 9,
    });
    expect(mapUsage(null)).toBeNull();
    expect(mapUsage(undefined)).toBeNull();
  });

  it("toolResultText：字符串/块数组/缺省", () => {
    expect(toolResultText({ content: "plain" })).toBe("plain");
    expect(toolResultText({ content: [{ type: "text", text: "a" }, { type: "text", text: "b" }] })).toBe("a\nb");
    expect(toolResultText({})).toContain("未提供详情");
  });
});
