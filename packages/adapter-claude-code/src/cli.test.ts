import { existsSync, readFileSync } from "node:fs";

import { describe, expect, it } from "vitest";

import {
  buildClaudeArgs,
  cleanupSystemPromptFile,
  claudePermissionMode,
  ensureIsolatedSettings,
  materializeSystemPromptFile,
  normalizeCommand,
  type ClaudeCliConfig,
} from "./claude-cli";
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

describe("M7-09 系统提示词下发（ADR-026）", () => {
  it("--append-system-prompt-file 透传（首轮/resume 同口径）；缺省不注入", () => {
    const file = "/tmp/aether-system-prompt.md";
    for (const resume of [false, true]) {
      const args = buildClaudeArgs(baseConfig(), {
        nativeId: "11111111-2222-3333-4444-555555555555",
        resume,
        prompt: "hi",
        timeoutMs: 1,
        appendSystemPromptFile: file,
      });
      const index = args.indexOf("--append-system-prompt-file");
      expect(index).toBeGreaterThanOrEqual(0);
      expect(args[index + 1]).toBe(file);
      // 与内联变体互斥：不得同时出现（避免提示词重复注入）。
      expect(args).not.toContain("--append-system-prompt");
    }
    const absent = buildClaudeArgs(baseConfig(), {
      nativeId: "11111111-2222-3333-4444-555555555555",
      resume: false,
      prompt: "hi",
      timeoutMs: 1,
    });
    expect(absent).not.toContain("--append-system-prompt-file");
  });

  it("物化文件原样写入（含合成块标记与换行）；清理后文件/目录移除", () => {
    const content =
      "<!-- aether:preset:01J00000000000000000000P02 -->\n你是资深软件工程师\n\n# 工作区约定（记忆文件注入，优先级低于系统提示）\n来源：AGENTS.md\n\n约定";
    const file = materializeSystemPromptFile(content);
    expect(readFileSync(file, "utf8")).toBe(content);
    cleanupSystemPromptFile(file);
    expect(existsSync(file)).toBe(false);
    // 幂等：重复清理不抛错（不存在时忽略）。
    expect(() => cleanupSystemPromptFile(file)).not.toThrow();
    expect(() => cleanupSystemPromptFile(undefined)).not.toThrow();
  });
});

describe("M7-06 档位投影（ADR-024 附录 A-4）", () => {
  it("消费映射生效时隔离用户级 settings（空文件）；未生效/已显式配置不触发", () => {
    // 触发：注入 ANTHROPIC_BASE_URL（消费映射）。
    const file = ensureIsolatedSettings({}, { ANTHROPIC_BASE_URL: "http://127.0.0.1:1/v1" });
    expect(file).toBeTruthy();
    expect(readFileSync(String(file), "utf8")).toBe("{}\n");
    const aether = ensureIsolatedSettings({}, { AETHER_PROVIDER_BASE_URL: "https://x" });
    expect(aether).toBeTruthy();
    // 未触发。
    expect(ensureIsolatedSettings({}, {})).toBeUndefined();
    // 已显式配置 settings-file → 不覆盖。
    expect(
      ensureIsolatedSettings({ settingsFile: "/tmp/custom.json" }, { ANTHROPIC_BASE_URL: "x" }),
    ).toBeUndefined();
  });
  it("三档映射：read_only→plan / workspace_write→acceptEdits / danger_full_access→bypassPermissions", () => {
    expect(claudePermissionMode("read_only")).toBe("plan");
    expect(claudePermissionMode("workspace_write")).toBe("acceptEdits");
    expect(claudePermissionMode("danger_full_access")).toBe("bypassPermissions");
    expect(claudePermissionMode("unknown")).toBeUndefined();
    expect(claudePermissionMode(undefined)).toBeUndefined();
    expect(claudePermissionMode(3)).toBeUndefined();
  });

  it("投影值进入 --permission-mode（逐档断言）", () => {
    for (const [mode, expected] of [
      ["read_only", "plan"],
      ["workspace_write", "acceptEdits"],
      ["danger_full_access", "bypassPermissions"],
    ] as const) {
      const args = buildClaudeArgs(
        baseConfig({ permissionMode: claudePermissionMode(mode) }),
        {
          nativeId: "11111111-2222-3333-4444-555555555555",
          resume: false,
          prompt: "hi",
          timeoutMs: 1,
        },
      );
      const index = args.indexOf("--permission-mode");
      expect(index).toBeGreaterThanOrEqual(0);
      expect(args[index + 1]).toBe(expected);
    }
  });
});

describe("normalizeCommand（Windows shim 归一化）", () => {  it("win32 下 claude/.cmd 经 cmd.exe；其余平台/路径原样", () => {
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
