/**
 * Electron 壳本地命令（M9-06 DoD-3）单元测试。
 *
 * 覆盖：文件/目录选择、取消、非法参数、非法路径（不存在/相对路径/控制字符/
 * 超长）、对话框错误，以及 `startup_pick_target` 的通用目录选择器语义。
 */
import { mkdtempSync, mkdirSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  MAX_PATH_CHARS,
  REF_PICK_METHOD,
  SHELL_LOCAL_METHODS,
  STARTUP_PICK_TARGET_METHOD,
  handleShellLocalCommand,
  type DialogHost,
  type OpenDialogOptions,
} from "./localCommands";

const roots: string[] = [];

function tempDir(prefix = "aether-m9-06-"): string {
  const dir = mkdtempSync(path.join(os.tmpdir(), prefix));
  roots.push(dir);
  return dir;
}

/** Windows CI 的 `%TEMP%` 可能是 8.3 短路径（RUNNER~1）；断言与实现一样用 canonical 长路径。 */
function canonical(value: string): string {
  return realpathSync.native(value);
}

function dialogReturning(
  filePaths: readonly string[],
  options: { canceled?: boolean } = {},
): { dialog: DialogHost; calls: OpenDialogOptions[] } {
  const calls: OpenDialogOptions[] = [];
  return {
    calls,
    dialog: {
      async showOpenDialog(input) {
        calls.push(input);
        return { canceled: options.canceled ?? false, filePaths };
      },
    },
  };
}

afterEach(() => {
  vi.restoreAllMocks();
  while (roots.length > 0) {
    const dir = roots.pop();
    if (dir) {
      rmSync(dir, { recursive: true, force: true });
    }
  }
});

describe("壳本地命令（M9-06 DoD-3）", () => {
  it("ref_pick 文件返回 canonical 路径（解析 . / .. 与符号链接）", async () => {
    const root = tempDir();
    mkdirSync(path.join(root, "sub"));
    const file = path.join(root, "sub", "notes.md");
    writeFileSync(file, "x");
    const messy = `${root}${path.sep}.${path.sep}sub${path.sep}..${path.sep}sub${path.sep}notes.md`;
    const { dialog, calls } = dialogReturning([messy]);

    const outcome = await handleShellLocalCommand(REF_PICK_METHOD, { kind: "file" }, dialog);

    expect(outcome).toEqual({ ok: true, result: { path: canonical(file) } });
    expect(calls).toEqual([{ properties: ["openFile"] }]);
  });

  it("ref_pick 目录与取消语义（取消返回 null，不伪造路径）", async () => {
    const root = tempDir();
    const picked = dialogReturning([root]);
    const directory = await handleShellLocalCommand(
      REF_PICK_METHOD,
      { kind: "directory" },
      picked.dialog,
    );
    expect(directory).toEqual({ ok: true, result: { path: canonical(root) } });
    expect(picked.calls).toEqual([{ properties: ["openDirectory"] }]);

    const canceled = dialogReturning([root], { canceled: true });
    const outcome = await handleShellLocalCommand(
      REF_PICK_METHOD,
      { kind: "file" },
      canceled.dialog,
    );
    expect(outcome).toEqual({ ok: true, result: { path: null } });
  });

  it("startup_pick_target 仍是通用目录选择器（无参数 + canonical target_dir）", async () => {
    const root = tempDir();
    const messy = `${root}${path.sep}.${path.sep}..${path.sep}${path.basename(root)}`;
    const { dialog, calls } = dialogReturning([messy]);

    for (const params of [null, undefined, {}]) {
      const outcome = await handleShellLocalCommand(STARTUP_PICK_TARGET_METHOD, params, dialog);
      expect(outcome).toEqual({ ok: true, result: { target_dir: canonical(root) } });
    }
    expect(calls).toEqual([
      { properties: ["openDirectory"] },
      { properties: ["openDirectory"] },
      { properties: ["openDirectory"] },
    ]);

    const canceled = dialogReturning([root], { canceled: true });
    const outcome = await handleShellLocalCommand(
      STARTUP_PICK_TARGET_METHOD,
      null,
      canceled.dialog,
    );
    expect(outcome).toEqual({ ok: true, result: { target_dir: null } });
  });

  it("参数严格解析：未知字段/缺省/类型/枚举/非对象", async () => {
    const { dialog } = dialogReturning(["C:\\unused"]);

    expect(await handleShellLocalCommand(REF_PICK_METHOD, {}, dialog)).toEqual({
      ok: false,
      error: { code: "missing_field", message: "缺少必填字段 kind", field: "kind" },
    });
    expect(
      await handleShellLocalCommand(REF_PICK_METHOD, { kind: "file", extra: true }, dialog),
    ).toMatchObject({ ok: false, error: { code: "unknown_field", field: "extra" } });
    expect(await handleShellLocalCommand(REF_PICK_METHOD, { kind: 3 }, dialog)).toMatchObject({
      ok: false,
      error: { code: "invalid_type", field: "kind" },
    });
    expect(
      await handleShellLocalCommand(REF_PICK_METHOD, { kind: "bogus" }, dialog),
    ).toMatchObject({ ok: false, error: { code: "invalid_enum", field: "kind" } });
    expect(await handleShellLocalCommand(REF_PICK_METHOD, null, dialog)).toMatchObject({
      ok: false,
      error: { code: "invalid_json" },
    });
    expect(
      await handleShellLocalCommand(STARTUP_PICK_TARGET_METHOD, { extra: 1 }, dialog),
    ).toMatchObject({ ok: false, error: { code: "unknown_field", field: "extra" } });
    expect(await handleShellLocalCommand(STARTUP_PICK_TARGET_METHOD, [], dialog)).toMatchObject({
      ok: false,
      error: { code: "invalid_json" },
    });
    expect(await handleShellLocalCommand("session_create", {}, dialog)).toMatchObject({
      ok: false,
      error: { code: "not_implemented" },
    });
  });

  it("非法路径：不存在/相对路径/控制字符/超长均拒绝（不返回不可信路径）", async () => {
    const root = tempDir();
    const missing = path.join(root, "missing.txt");
    const cases: Array<{ picked: string; field: "path" | "target_dir"; code: string }> = [
      { picked: missing, field: "path", code: "path_rejected" },
      { picked: "relative/notes.md", field: "path", code: "path_rejected" },
      { picked: `${root}\u0000evil`, field: "path", code: "invalid_format" },
      { picked: `${root}${"x".repeat(MAX_PATH_CHARS)}`, field: "target_dir", code: "too_large" },
    ];
    for (const item of cases) {
      const { dialog } = dialogReturning([item.picked]);
      const method = item.field === "path" ? REF_PICK_METHOD : STARTUP_PICK_TARGET_METHOD;
      const params = item.field === "path" ? { kind: "file" } : null;
      const outcome = await handleShellLocalCommand(method, params, dialog);
      expect(outcome).toMatchObject({ ok: false, error: { code: item.code, field: item.field } });
    }
  });

  it("对话框错误映射 internal（不吞错、不伪造取消）", async () => {
    const dialog: DialogHost = {
      async showOpenDialog() {
        throw new Error("dialog unavailable");
      },
    };
    expect(await handleShellLocalCommand(REF_PICK_METHOD, { kind: "file" }, dialog)).toMatchObject(
      { ok: false, error: { code: "internal" } },
    );
  });

  it("磁盘上的符号链接在选择后解析为真实目标（canonical）", async () => {
    const root = tempDir();
    const target = path.join(root, "target.txt");
    writeFileSync(target, "x");
    const link = path.join(root, "link.txt");
    try {
      symlinkSync(target, link);
    } catch {
      // Windows 未启用开发者模式/无符号链接权限：跳过该断言（canonicalize 语义由
      // `.`/`..` 用例覆盖；不视为失败）。
      return;
    }
    const { dialog } = dialogReturning([link]);
    expect(await handleShellLocalCommand(REF_PICK_METHOD, { kind: "file" }, dialog)).toEqual({
      ok: true,
      result: { path: canonical(target) },
    });
  });

  it("壳本地命令清单与 63 条路由口径一致（仅 ref_pick/startup_pick_target）", () => {
    expect(SHELL_LOCAL_METHODS).toEqual([REF_PICK_METHOD, STARTUP_PICK_TARGET_METHOD]);
  });
});
