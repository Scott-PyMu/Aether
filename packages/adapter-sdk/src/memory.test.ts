import { mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  appendMemory,
  atomicWriteMemory,
  MEMORY_CONFLICT_CODE,
  MEMORY_FILE_PRIORITY,
  MEMORY_INJECTION_MAX_BYTES,
  MEMORY_READ_FAILED_CODE,
  MEMORY_WRITE_FAILED_CODE,
  MemoryToolError,
  memoryFileCandidates,
  readMemory,
  readMemoryFile,
  selectMemoryFile,
  writeMemory,
} from "./memory";

let scratch: string;

beforeEach(() => {
  scratch = mkdtempSync(path.join(os.tmpdir(), "aether-m3-08-sdk-"));
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

describe("工作区记忆（M3-08/D14）", () => {
  it("优先级候选顺序为 AGENTS.md > AETHER.md > CLAUDE.md", () => {
    expect(MEMORY_FILE_PRIORITY).toEqual(["AGENTS.md", "AETHER.md", "CLAUDE.md"]);
    expect(memoryFileCandidates("C:/ws")).toEqual([
      path.join("C:/ws", "AGENTS.md"),
      path.join("C:/ws", "AETHER.md"),
      path.join("C:/ws", "CLAUDE.md"),
    ]);
    expect(MEMORY_INJECTION_MAX_BYTES).toBe(32 * 1024);
  });

  it("selectMemoryFile 按优先级选择第一个存在的文件", async () => {
    expect(await selectMemoryFile(scratch)).toBeNull();
    writeFileSync(path.join(scratch, "CLAUDE.md"), "c");
    expect(await selectMemoryFile(scratch)).toBe(path.join(scratch, "CLAUDE.md"));
    writeFileSync(path.join(scratch, "AETHER.md"), "a");
    expect(await selectMemoryFile(scratch)).toBe(path.join(scratch, "AETHER.md"));
    writeFileSync(path.join(scratch, "AGENTS.md"), "g");
    expect(await selectMemoryFile(scratch)).toBe(path.join(scratch, "AGENTS.md"));
  });

  it("readMemoryFile 返回正文与快照；不存在 → memory_read_failed", async () => {
    const target = path.join(scratch, "AGENTS.md");
    writeFileSync(target, "约定");
    const result = await readMemoryFile(target);
    expect(result.content).toBe("约定");
    expect(result.snapshot.size).toBe(Buffer.byteLength("约定", "utf8"));

    const missing = path.join(scratch, "AETHER.md");
    await expect(readMemory(missing)).rejects.toMatchObject({
      code: MEMORY_READ_FAILED_CODE,
    });
  });

  it("writeMemory 原子写（临时文件 + rename；无半写目标、无残留临时文件）", async () => {
    const target = path.join(scratch, "AGENTS.md");
    const snapshot = await writeMemory(target, "第一版");
    expect(snapshot.size).toBe(Buffer.byteLength("第一版", "utf8"));
    expect(readFileSync(target, "utf8")).toBe("第一版");

    // 写入期间：目标未变、临时文件存在；完成后临时文件清理。
    const observed: string[] = [];
    await writeMemory(target, "第二版加长内容", {
      chunkBytes: 3,
      chunkDelayMs: 1,
      onChunk: () => {
        if (readFileSync(target, "utf8") !== "第二版加长内容") observed.push("old");
        observed.push(
          readdirSync(scratch).some((name) => name.includes(".aether-tmp-")) ? "temp" : "no-temp",
        );
      },
    });
    expect(observed).toContain("old");
    expect(observed).toContain("temp");
    expect(readFileSync(target, "utf8")).toBe("第二版加长内容");
    expect(readdirSync(scratch).some((name) => name.includes(".aether-tmp-"))).toBe(false);
  });

  it("appendMemory 读取现值后追加（跨会话写入读回口径）", async () => {
    const target = path.join(scratch, "AETHER.md");
    await appendMemory(target, "会话 A 写入");
    await appendMemory(target, "|会话 B 追加");
    expect(readFileSync(target, "utf8")).toBe("会话 A 写入|会话 B 追加");
  });

  it("外部修改后写入 → memory_conflict 且不覆盖（D14）", async () => {
    const target = path.join(scratch, "AGENTS.md");
    writeFileSync(target, "原始内容");
    const expected = await readMemoryFile(target);
    // 外部修改（模拟并发写者）。
    writeFileSync(target, "外部修改后的内容");

    await expect(
      atomicWriteMemory(target, "本次写入", expected.snapshot),
    ).rejects.toMatchObject({ code: MEMORY_CONFLICT_CODE });
    expect(readFileSync(target, "utf8")).toBe("外部修改后的内容");
    expect(readdirSync(scratch).some((name) => name.includes(".aether-tmp-"))).toBe(false);
  });

  it("新建目标的并发创建同样判冲突（预期不存在但不为空）", async () => {
    const target = path.join(scratch, "CLAUDE.md");
    writeFileSync(target, "他人先建");
    await expect(atomicWriteMemory(target, "x", null)).rejects.toMatchObject({
      code: MEMORY_CONFLICT_CODE,
    });
    expect(readFileSync(target, "utf8")).toBe("他人先建");
  });

  it("单次写入超过 1MB → memory_write_failed（D9 上限）", async () => {
    const target = path.join(scratch, "AGENTS.md");
    const giant = "x".repeat(1_048_577);
    await expect(writeMemory(target, giant)).rejects.toMatchObject({
      code: MEMORY_WRITE_FAILED_CODE,
    });
  });

  it("MemoryToolError 携带稳定错误码与可恢复标记", () => {
    const error = new MemoryToolError(MEMORY_CONFLICT_CODE, "冲突");
    expect(error.code).toBe("memory_conflict");
    expect(error.recoverable).toBe(true);
    expect(error).toBeInstanceOf(Error);
  });
});
