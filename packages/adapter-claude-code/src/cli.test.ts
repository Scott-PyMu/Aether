import { describe, expect, it } from "vitest";

import { buildClaudeArgs, normalizeCommand, type ClaudeCliConfig } from "./claude-cli";
import { parseArgs, parseLaunchToken } from "./cli";

const VALID_TOKEN = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

function baseConfig(overrides: Partial<ClaudeCliConfig> = {}): ClaudeCliConfig {
  return { bin: "claude", extraArgs: [], workspace: ".", ...overrides };
}

describe("buildClaudeArgs（接入笔记固定参数形状）", () => {
  it("首轮 --session-id；续聊 --resume；流式参数齐全", () => {
    const fresh = buildClaudeArgs(baseConfig(), {
      nativeId: "11111111-2222-3333-4444-555555555555",
      resume: false,
      prompt: "hi",
      timeoutMs: 1000,
    });
    expect(fresh).toContain("--session-id");
    expect(fresh).not.toContain("--resume");
    expect(fresh).toEqual(
      expect.arrayContaining(["-p", "--output-format", "stream-json", "--include-partial-messages", "--verbose"]),
    );

    const resume = buildClaudeArgs(baseConfig(), {
      nativeId: "11111111-2222-3333-4444-555555555555",
      resume: true,
      prompt: "hi",
      timeoutMs: 1000,
    });
    expect(resume).toContain("--resume");
    expect(resume).not.toContain("--session-id");
  });

  it("settings/模型/工具/权限/max-turns 透传；tools=none → 空串", () => {
    const args = buildClaudeArgs(
      baseConfig({
        settingsFile: "/tmp/settings.json",
        model: "claude-x",
        tools: "none",
        permissionMode: "plan",
        maxTurns: 3,
        extraArgs: ["/tmp/fake-cli.mjs"],
      }),
      { nativeId: "11111111-2222-3333-4444-555555555555", resume: false, prompt: "hi", timeoutMs: 1 },
    );
    expect(args).toEqual(
      expect.arrayContaining([
        "--setting-sources",
        "local",
        "--settings",
        "/tmp/settings.json",
        "--strict-mcp-config",
        "--permission-mode",
        "plan",
        "--tools",
        "",
        "--max-turns",
        "3",
        "--model",
        "claude-x",
        "/tmp/fake-cli.mjs",
      ]),
    );
    const defaultMode = buildClaudeArgs(baseConfig(), {
      nativeId: "11111111-2222-3333-4444-555555555555",
      resume: false,
      prompt: "hi",
      timeoutMs: 1,
    });
    expect(defaultMode).toEqual(expect.arrayContaining(["--permission-mode", "default"]));
    expect(defaultMode).not.toContain("--settings");
    expect(defaultMode).not.toContain("--tools");
  });
});

describe("normalizeCommand（Windows shim 归一化）", () => {
  it("win32 下 claude/.cmd 经 cmd.exe；其余平台/路径原样", () => {
    expect(normalizeCommand("claude", ["-p"], "win32", "cmd.exe")).toEqual({
      command: "cmd.exe",
      args: ["/d", "/s", "/c", "claude", "-p"],
    });
    expect(normalizeCommand("C:\\bin\\claude.cmd", [], "win32", "cmd.exe").command).toBe("cmd.exe");
    expect(normalizeCommand("C:\\bin\\claude.exe", [], "win32", "cmd.exe")).toEqual({
      command: "C:\\bin\\claude.exe",
      args: [],
    });
    expect(normalizeCommand("claude", ["-p"], "linux", undefined)).toEqual({
      command: "claude",
      args: ["-p"],
    });
  });
});

describe("CLI 参数解析", () => {
  it("默认值与全部参数；--launch-token 合法解析", () => {
    const options = parseArgs(
      [
        "--claude-bin",
        "node",
        "--claude-arg",
        "/tmp/fake.mjs",
        "--settings-file",
        "/tmp/s.json",
        "--workspace",
        "/work",
        "--model",
        "m",
        "--tools",
        "none",
        "--permission-mode",
        "plan",
        "--max-turns",
        "2",
        "--run-timeout-ms",
        "5000",
        `--launch-token=${VALID_TOKEN}`,
      ],
      () => {},
    );
    expect(options.cli).toMatchObject({
      bin: "node",
      extraArgs: ["/tmp/fake.mjs"],
      settingsFile: "/tmp/s.json",
      workspace: "/work",
      model: "m",
      tools: "none",
      permissionMode: "plan",
      maxTurns: 2,
    });
    expect(options.runTimeoutMs).toBe(5000);
    expect(options.launchToken).toBe(VALID_TOKEN);
  });

  it("非法 launch-token / 未知参数 / 缺值", () => {
    const warnings: string[] = [];
    expect(parseLaunchToken("--launch-token=nope", (line) => warnings.push(line))).toBeUndefined();
    expect(warnings).toHaveLength(1);
    expect(() => parseArgs(["--unknown"], () => {})).toThrow("未知参数");
    expect(() => parseArgs(["--claude-bin"], () => {})).toThrow("缺少值");
    expect(() => parseArgs(["--max-turns", "-1"], () => {})).toThrow("非负数字");
    const ignored: string[] = [];
    const options = parseArgs(["--launch-token"], (line) => ignored.push(line));
    expect(options.launchToken).toBeUndefined();
    expect(ignored[0]).toContain("缺少");
  });
});
