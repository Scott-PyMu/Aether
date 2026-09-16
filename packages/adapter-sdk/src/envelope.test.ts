import { describe, expect, it } from "vitest";

import { buildEnvelope, EVENT_ENVELOPE_VERSION, SessionSequencer } from "./envelope";

describe("事件信封 v1", () => {
  it("固定字段与 D4 信封一致", () => {
    const envelope = buildEnvelope(
      { runtimeId: "mock", sessionId: "s-1", runId: "r-1" },
      "log",
      { level: "info", message: "hi" },
      7,
      { id: "01J00000000000000000000001", ts: 42 },
    );
    expect(Object.keys(envelope)).toEqual([
      "v",
      "id",
      "session_id",
      "run_id",
      "runtime_id",
      "seq",
      "ts",
      "type",
      "payload",
    ]);
    expect(envelope.v).toBe(EVENT_ENVELOPE_VERSION);
    expect(envelope.id).toBe("01J00000000000000000000001");
    expect(envelope.session_id).toBe("s-1");
    expect(envelope.run_id).toBe("r-1");
    expect(envelope.runtime_id).toBe("mock");
    expect(envelope.seq).toBe(7);
    expect(envelope.ts).toBe(42);
    expect(envelope.type).toBe("log");
    expect(envelope.payload).toEqual({ level: "info", message: "hi" });
  });

  it("未提供 run_id 时为空（与 events.run_id NULL 对应）", () => {
    const envelope = buildEnvelope({ runtimeId: "mock", sessionId: "s" }, "log", {}, 1);
    expect(envelope.run_id).toBeNull();
  });

  it("会话 sequencer 单调唯一", () => {
    const sequencer = new SessionSequencer("s-1");
    expect(sequencer.current()).toBe(0);
    expect([sequencer.next(), sequencer.next(), sequencer.next()]).toEqual([1, 2, 3]);
    expect(sequencer.id).toBe("s-1");
  });
});
