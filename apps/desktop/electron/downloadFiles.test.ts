/**
 * 下载落盘单测（M9-10 DoD-4）：确认后才把隔离区文件移动到最终路径。
 */
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";

import { afterEach, describe, expect, it } from "vitest";

import { discardDownloadedFile, promoteDownloadedFile } from "./downloadFiles";

const roots: string[] = [];

function tempRoot(): string {
  const root = mkdtempSync(path.join(os.tmpdir(), "aether-m9-10-dl-"));
  roots.push(root);
  return root;
}

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
});

describe("downloadFiles", () => {
  it("确认后移动到最终路径并保留内容；隔离区不再存在", () => {
    const root = tempRoot();
    const quarantine = path.join(root, ".quarantine", "dl-1.part");
    const target = path.join(root, "Downloads", "evil.txt");
    mkdirSync(path.dirname(quarantine), { recursive: true });
    writeFileSync(quarantine, "downloaded-content");

    expect(promoteDownloadedFile(quarantine, target)).toEqual({ ok: true });
    expect(readFileSync(target, "utf8")).toBe("downloaded-content");
    expect(existsSync(quarantine)).toBe(false);
  });

  it("隔离区文件缺失时拒绝移动", () => {
    const root = tempRoot();
    const result = promoteDownloadedFile(
      path.join(root, ".quarantine", "missing.part"),
      path.join(root, "Downloads", "missing.txt"),
    );
    expect(result.ok).toBe(false);
    expect(result.error).toContain("隔离区文件不存在");
  });

  it("取消时清理隔离区且不触碰最终路径", () => {
    const root = tempRoot();
    const quarantine = path.join(root, ".quarantine", "dl-2.part");
    const target = path.join(root, "Downloads", "second.txt");
    mkdirSync(path.dirname(quarantine), { recursive: true });
    writeFileSync(quarantine, "partial");
    discardDownloadedFile(quarantine);
    expect(existsSync(quarantine)).toBe(false);
    expect(existsSync(target)).toBe(false);
  });
});
