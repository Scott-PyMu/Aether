import { describe, expect, it } from "vitest";

import { CodexRunMapper, mapUsage } from "./codex-events";

const context = { sessionId: "s-1", runId: "r-1", messageId: "m-1" };

describe("CodexRunMapper", () => {
  it("thread.started 记录原生 id；turn.completed 产出终稿与 usage", () => {
    const mapper = new CodexRunMapper(context, () => 1000);
    expect(mapper.push({ type: "thread.started", thread_id: "thr_1" })).toEqual([]);
    expect(mapper.threadId).toBe("thr_1");
    expect(
      mapper.push({ type: "item.completed", item: { id: "m", type: "agent_message", text: "hello" } }),
    ).toEqual([
      { type: "message.delta", payload: { message_id: "m-1", text: "hello" } },
    ]);
    mapper.push({ type: "turn.completed", usage: { input_tokens: 1, output_tokens: 2 } });
    const mapped = mapper.finish({ reason: "process-exit", exitCode: 0 });
    expect(mapped.map((item) => item.type)).toEqual(["message.completed", "run.completed"]);
    expect(mapped[0]?.payload.usage).toEqual({ input_tokens: 1, output_tokens: 2, total_tokens: 3 });
  });

  it("item.updated 只上报增量后缀", () => {
    const mapper = new CodexRunMapper(context);
    mapper.push({ type: "item.completed", item: { id: "m", type: "agent_message", text: "abc" } });
    const delta = mapper.push({
      type: "item.updated",
      item: { id: "m", type: "agent_message", text: "abcdef" },
    });
    expect(delta).toEqual([{ type: "message.delta", payload: { message_id: "m-1", text: "def" } }]);
    expect(mapper.streamedText).toBe("abcdef");
  });

  it("工具条目 exit_code 非零 → tool.call_failed", () => {
    const mapper = new CodexRunMapper(context, () => 100);
    mapper.push({
      type: "item.started",
      item: { id: "t", type: "command_execution", command: "false" },
    });
    const mapped = mapper.push({
      type: "item.completed",
      item: { id: "t", type: "command_execution", command: "false", exit_code: 1, aggregated_output: "err" },
    });
    expect(mapped[0]?.type).toBe("tool.call_failed");
    expect((mapped[0]?.payload.error as { code: string }).code).toBe("tool_execution_failed");
  });

  it("中断时在途工具收口为 timeout/abort + run.cancelled", () => {
    const mapper = new CodexRunMapper(context);
    mapper.push({ type: "item.started", item: { id: "t", type: "file_change" } });
    const mapped = mapper.finish({ reason: "interrupted" });
    expect(mapped.map((item) => item.type)).toEqual(["tool.call_failed", "run.cancelled"]);
    expect((mapped[0]?.payload.error as { code: string }).code).toBe("timeout");
    expect(String((mapped[0]?.payload.error as { message: string }).message)).toContain("abort");
  });

  it("终态幂等：finish 二次调用无事件", () => {
    const mapper = new CodexRunMapper(context);
    mapper.push({ type: "turn.completed", usage: null });
    mapper.finish({ reason: "process-exit", exitCode: 0 });
    expect(mapper.finish({ reason: "process-exit", exitCode: 0 })).toEqual([]);
  });

  it("mapUsage 兼容 camelCase 与缺省 total", () => {
    expect(mapUsage({ inputTokens: 3, outputTokens: 4 })).toEqual({
      input_tokens: 3,
      output_tokens: 4,
      total_tokens: 7,
    });
    expect(mapUsage(null)).toBeNull();
    expect(mapUsage({ totalTokens: 9 })).toEqual({
      input_tokens: 0,
      output_tokens: 0,
      total_tokens: 9,
    });
  });
});
