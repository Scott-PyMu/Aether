import { appendFileSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import { DeltaChannel, DeltaReconciler, parseDeltaLine } from "./dsh-delta";

describe("parseDeltaLine", () => {
  it("接受已知帧并忽略非法行", () => {
    expect(parseDeltaLine('{"type":"chunk","text":"a","chunkType":"text-delta"}')).toMatchObject({
      type: "chunk",
      text: "a",
      chunkType: "text-delta",
    });
    expect(parseDeltaLine("not-json")).toBeNull();
    expect(parseDeltaLine("[1,2]")).toBeNull();
    expect(parseDeltaLine('{"no":"type"}')).toBeNull();
  });
});

describe("DeltaChannel", () => {
  it("按偏移增量读取 JSONL；截断后从头重读", () => {
    const dir = mkdtempSync(join(tmpdir(), "dsh-channel-"));
    const path = join(dir, "delta.jsonl");
    writeFileSync(path, "", "utf8");
    const frames: string[] = [];
    const channel = new DeltaChannel(path, {
      onFrame: (frame) => frames.push(`${frame.type}:${frame.text ?? ""}`),
    });
    appendFileSync(path, '{"type":"hello","contract":"aether-dsh-stream@1"}\n');
    appendFileSync(path, '{"type":"chunk","text":"a"}\n');
    channel.poll();
    expect(frames).toEqual(["hello:", "chunk:a"]);

    appendFileSync(path, '{"type":"chunk","text":"b"}\n');
    channel.poll();
    expect(frames).toEqual(["hello:", "chunk:a", "chunk:b"]);

    // 截断（故障注入）→ 从 0 重读。
    writeFileSync(path, '{"type":"chunk","text":"c"}\n', "utf8");
    channel.poll();
    expect(frames[frames.length - 1]).toBe("chunk:c");
    channel.stop();
  });
});

describe("DeltaReconciler（前缀去重 / 兜底）", () => {
  it("插件 delta 全量覆盖 committed → 无重复增量", () => {
    const reconciler = new DeltaReconciler();
    expect(reconciler.onChunk("line 1 ")).toBe("line 1 ");
    expect(reconciler.onChunk("line 2")).toBe("line 2");
    const result = reconciler.onCommitted("line 1 line 2");
    expect(result.suffix).toBe("");
    expect(result.fallback).toBe(false);
    reconciler.markEnd();
    expect(reconciler.finalText()).toBe("line 1 line 2");
  });

  it("committed 含 final-only 后缀 → 只补后缀", () => {
    const reconciler = new DeltaReconciler();
    reconciler.onChunk("line 1 ");
    reconciler.onChunk("line 2");
    const result = reconciler.onCommitted("line 1 line 2\nfinal-only suffix");
    expect(result.suffix).toBe("\nfinal-only suffix");
    expect(result.fallback).toBe(false);
  });

  it("前缀不一致（截断/丢帧）→ 丢弃增量并按 committed 重建", () => {
    const reconciler = new DeltaReconciler();
    reconciler.onChunk("partial");
    const result = reconciler.onCommitted("complete text");
    expect(result.fallback).toBe(true);
    expect(result.suffix).toBe("");
    expect(result.droppedChars).toBe("partial".length);
    expect(reconciler.finalText()).toBe("complete text");
    expect(reconciler.fallbackTriggered).toBe(true);
  });

  it("committed 后到达的迟到增量被丢弃", () => {
    const reconciler = new DeltaReconciler();
    reconciler.onCommitted("done");
    expect(reconciler.onChunk("late")).toBe("");
    expect(reconciler.finalText()).toBe("done");
  });

  it("无 committed 的通道-only 场景回退流式拼接", () => {
    const reconciler = new DeltaReconciler();
    reconciler.onChunk("a");
    reconciler.onChunk("b");
    expect(reconciler.finalText()).toBe("ab");
  });
});
