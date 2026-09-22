import { describe, expect, it } from "vitest";

import { buildCodexArgs, normalizeCommand } from "./codex-cli";
import { parseArgs } from "./cli";

const config = {
  bin: "codex",
  extraArgs: [],
  workspace: "C:/ws",
  sandbox: "read-only",
  reasoning: "low",
  model: "gpt-5.5",
};

describe("buildCodexArgs", () => {
  it("新会话：exec --json + 沙箱 + --cd + stdin 占位", () => {
    const args = buildCodexArgs(config, { prompt: "hi", timeoutMs: 1000 });
    expect(args).toEqual([
      "exec",
      "--json",
      "--skip-git-repo-check",
      "-m",
      "gpt-5.5",
      "-c",
      "model_reasoning_effort=low",
      "--sandbox",
      "read-only",
      "--cd",
      "C:/ws",
      "-",
    ]);
  });

  it("恢复：exec resume + -c sandbox_mode（不带 --sandbox/--cd，已知坑 7）", () => {
    const args = buildCodexArgs(config, { threadId: "thr_1", prompt: "hi", timeoutMs: 1000 });
    expect(args).toContain("resume");
    expect(args).not.toContain("--sandbox");
    expect(args).not.toContain("--cd");
    expect(args[args.length - 1]).toBe("-");
    expect(args[args.length - 2]).toBe("thr_1");
    expect(args).toContain("sandbox_mode=read-only");
  });

  it("extraArgs 前置（node 脚本注入顺序）", () => {
    const args = buildCodexArgs({ ...config, extraArgs: ["cli.mjs"] }, { prompt: "hi", timeoutMs: 1 });
    expect(args[0]).toBe("cli.mjs");
  });
});

describe("normalizeCommand", () => {
  it("Windows .cmd 垫片经 cmd.exe；直连路径不动", () => {
    expect(normalizeCommand("codex", ["exec"], "win32", "cmd.exe").command).toBe("cmd.exe");
    expect(normalizeCommand("codex.cmd", ["exec"], "win32", "cmd.exe").command).toBe("cmd.exe");
    expect(normalizeCommand("node", ["x.mjs"], "win32", "cmd.exe")).toEqual({
      command: "node",
      args: ["x.mjs"],
    });
    expect(normalizeCommand("codex", ["exec"], "linux").command).toBe("codex");
  });
});

describe("parseArgs", () => {
  it("解析全部参数并忽略非法 launch-token", () => {
    const warnings: string[] = [];
    const options = parseArgs(
      [
        "--launch-token=01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "--codex-bin",
        "node",
        "--codex-arg",
        "cli.mjs",
        "--codex-home",
        "C:/home",
        "--state-dir",
        "C:/state",
        "--workspace",
        "C:/ws",
        "--model",
        "m",
        "--sandbox",
        "workspace-write",
        "--reasoning",
        "high",
        "--config",
        "a=b",
        "--run-timeout-ms",
        "1234",
      ],
      (line) => warnings.push(line),
    );
    expect(options.launchToken).toBe("01ARZ3NDEKTSV4RRFFQ69G5FAV");
    expect(options.cli.bin).toBe("node");
    expect(options.cli.extraArgs).toEqual(["cli.mjs"]);
    expect(options.cli.home).toBe("C:/home");
    expect(options.stateDir).toBe("C:/state");
    expect(options.cli.sandbox).toBe("workspace-write");
    expect(options.cli.configOverrides).toEqual(["a=b"]);
    expect(options.runTimeoutMs).toBe(1234);

    parseArgs(["--launch-token=bad"], (line) => warnings.push(line));
    expect(warnings.some((line) => line.includes("忽略非法"))).toBe(true);
  });

  it("未知参数抛错", () => {
    expect(() => parseArgs(["--nope"], () => {})).toThrow("未知参数");
  });
});
