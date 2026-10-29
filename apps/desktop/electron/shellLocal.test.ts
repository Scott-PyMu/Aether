/**
 * 4 条壳本地命令单测（M9-07 DoD-4；ADR-029 §3.4/§4.6）。
 */
import { mkdtempSync, mkdirSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { afterAll, describe, expect, it, vi } from "vitest";

import {
  SHELL_COMMANDS,
  ShellLocalError,
  handleShellLocalCommand,
  isShellLocalCommand,
  validatePickedPath,
} from "./shellLocal";

const root = mkdtempSync(path.join(tmpdir(), "aether-m9-07-pick-"));
const file = path.join(root, "note.txt");
const directory = path.join(root, "workspace");
mkdirSync(directory);
writeFileSync(file, "hello", "utf8");

afterAll(() => {
  rmSync(root, { recursive: true, force: true });
});

function deps(picked: string | null) {
  return {
    pick: vi.fn(async () => picked),
    requestExit: vi.fn(),
    requestRestart: vi.fn(),
  };
}

describe("shell local commands", () => {
  it("路由集合为 4 条", () => {
    expect([...SHELL_COMMANDS]).toEqual([
      "app_exit",
      "app_restart",
      "ref_pick",
      "startup_pick_target",
    ]);
    expect(isShellLocalCommand("app_exit")).toBe(true);
    expect(isShellLocalCommand("health")).toBe(false);
  });

  it("app_exit：confirm:true → {exiting:true} + 退出动作在响应后执行", async () => {
    const dependencies = deps(null);
    const outcome = await handleShellLocalCommand(
      "app_exit",
      { confirm: true },
      dependencies,
    );
    expect(outcome.result).toEqual({ exiting: true });
    expect(dependencies.requestExit).not.toHaveBeenCalled();
    outcome.after?.();
    expect(dependencies.requestExit).toHaveBeenCalledWith(0);
  });

  it("app_exit/app_restart：confirm 缺失/非布尔/false/未知字段全部拒绝", async () => {
    const dependencies = deps(null);
    const cases: Array<[unknown, string, string | undefined]> = [
      [{}, "missing_field", "confirm"],
      [{ confirm: "true" }, "invalid_type", "confirm"],
      [{ confirm: false }, "invalid_value", "confirm"],
      [{ confirm: true, extra: 1 }, "unknown_field", "extra"],
      ["nope", "invalid_type", undefined],
    ];
    for (const [params, code, field] of cases) {
      await expect(
        handleShellLocalCommand("app_exit", params, dependencies),
      ).rejects.toMatchObject({
        aetherCode: code,
        ...(field ? { field } : {}),
      });
    }
    await expect(
      handleShellLocalCommand("app_restart", { confirm: false }, dependencies),
    ).rejects.toBeInstanceOf(ShellLocalError);
  });

  it("app_restart：confirm:true → {restarting:true} + 重启动作", async () => {
    const dependencies = deps(null);
    const outcome = await handleShellLocalCommand(
      "app_restart",
      { confirm: true },
      dependencies,
    );
    expect(outcome.result).toEqual({ restarting: true });
    outcome.after?.();
    expect(dependencies.requestRestart).toHaveBeenCalledTimes(1);
  });

  it("ref_pick：文件/目录返回 canonical 路径；取消返回 null", async () => {
    const fileOutcome = await handleShellLocalCommand(
      "ref_pick",
      { kind: "file" },
      deps(file),
    );
    expect(fileOutcome.result).toEqual({ path: realpathSync.native(file) });

    const directoryOutcome = await handleShellLocalCommand(
      "ref_pick",
      { kind: "directory" },
      deps(directory),
    );
    expect(directoryOutcome.result).toEqual({ path: realpathSync.native(directory) });

    const cancel = await handleShellLocalCommand(
      "ref_pick",
      { kind: "directory" },
      deps(null),
    );
    expect(cancel.result).toEqual({ path: null });
  });

  it("ref_pick：kind 缺失/非法/类型错误 + 未知字段", async () => {
    const dependencies = deps(file);
    for (const [params, code] of [
      [{}, "missing_field"],
      [{ kind: "bogus" }, "invalid_enum"],
      [{ kind: 1 }, "invalid_type"],
      [{ kind: "file", extra: true }, "unknown_field"],
    ] as const) {
      await expect(
        handleShellLocalCommand("ref_pick", params, dependencies),
      ).rejects.toMatchObject({ aetherCode: code });
    }
  });

  it("ref_pick：kind=file 选到目录/不存在路径 → path_rejected", async () => {
    await expect(
      handleShellLocalCommand("ref_pick", { kind: "file" }, deps(directory)),
    ).rejects.toMatchObject({ aetherCode: "path_rejected" });
    await expect(
      handleShellLocalCommand(
        "ref_pick",
        { kind: "file" },
        deps(path.join(root, "missing.txt")),
      ),
    ).rejects.toMatchObject({ aetherCode: "path_rejected" });
    expect(() => validatePickedPath("", "file")).toThrow(ShellLocalError);
  });

  it("startup_pick_target：省略/null/{} 等价；返回 canonical 目录", async () => {
    for (const params of [undefined, null, {}]) {
      const outcome = await handleShellLocalCommand(
        "startup_pick_target",
        params,
        deps(directory),
      );
      expect(outcome.result).toEqual({ target_dir: realpathSync.native(directory) });
    }
    await expect(
      handleShellLocalCommand("startup_pick_target", { extra: 1 }, deps(directory)),
    ).rejects.toMatchObject({ aetherCode: "unknown_field" });
    await expect(
      handleShellLocalCommand("startup_pick_target", "x", deps(directory)),
    ).rejects.toMatchObject({ aetherCode: "invalid_type" });
  });

  it("未知壳命令 → not_implemented", async () => {
    await expect(
      handleShellLocalCommand("nope", undefined, deps(null)),
    ).rejects.toMatchObject({ aetherCode: "not_implemented" });
  });
});
