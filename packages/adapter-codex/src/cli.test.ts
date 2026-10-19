import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import {
  buildCodexArgs,
  CODEX_CONFIG_MARKER,
  codexSandbox,
  materializeCodexProviderConfig,
  normalizeCommand,
  renderCodexProviderConfig,
} from "./codex-cli";
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

describe("M7-06 档位投影与供应商消费映射（ADR-024）", () => {
  it("三档映射：read_only→read-only / workspace_write→workspace-write / danger_full_access→danger-full-access", () => {
    expect(codexSandbox("read_only")).toBe("read-only");
    expect(codexSandbox("workspace_write")).toBe("workspace-write");
    expect(codexSandbox("danger_full_access")).toBe("danger-full-access");
    expect(codexSandbox("unknown")).toBeUndefined();
  });

  it("投影值进入 --sandbox / resume -c sandbox_mode（逐档断言）", () => {
    for (const [mode, expected] of [
      ["read_only", "read-only"],
      ["workspace_write", "workspace-write"],
      ["danger_full_access", "danger-full-access"],
    ] as const) {
      const sandbox = codexSandbox(mode);
      const fresh = buildCodexArgs({ ...config, sandbox: sandbox ?? config.sandbox }, { prompt: "hi", timeoutMs: 1 });
      const flag = fresh.indexOf("--sandbox");
      expect(flag).toBeGreaterThanOrEqual(0);
      expect(fresh[flag + 1]).toBe(expected);
      const resume = buildCodexArgs(
        { ...config, sandbox: sandbox ?? config.sandbox },
        { threadId: "thr_1", prompt: "hi", timeoutMs: 1 },
      );
      expect(resume).toContain(`sandbox_mode=${expected}`);
    }
  });

  it("config.toml 物化：只含 base_url 与 env 键名（不写密钥值）；非本适配器文件拒绝覆盖", () => {
    const home = mkdtempSync(join(tmpdir(), "aether-codex-home-"));
    const path = materializeCodexProviderConfig(
      home,
      "provider-1",
      "https://gateway.example.com/v1",
      "OPENAI_API_KEY",
    );
    expect(path).toBe(join(home, "config.toml"));
    const text = readFileSync(path, "utf8");
    expect(text.startsWith(CODEX_CONFIG_MARKER)).toBe(true);
    expect(text).toContain('model_provider = "provider-1"');
    expect(text).toContain("[model_providers.provider-1]");
    expect(text).toContain('base_url = "https://gateway.example.com/v1"');
    expect(text).toContain('env_key = "OPENAI_API_KEY"');
    expect(text).not.toContain("sk-");
    expect(text).not.toContain("api_key");
    // 幂等重复物化（同一标记）成功。
    expect(() =>
      materializeCodexProviderConfig(home, "provider-1", "https://gateway.example.com/v1", "OPENAI_API_KEY"),
    ).not.toThrow();
    // 用户已有非 Aether 配置 → 拒绝覆盖。
    writeFileSync(path, "model = \"user-config\"\n", "utf8");
    expect(() =>
      materializeCodexProviderConfig(home, "provider-1", "https://gateway.example.com/v1", "OPENAI_API_KEY"),
    ).toThrow(/拒绝覆盖/);
  });

  it("renderCodexProviderConfig 拒绝非法 provider id / env 键名", () => {
    expect(() => renderCodexProviderConfig("bad id", "https://x", "OPENAI_API_KEY")).toThrow();
    expect(() => renderCodexProviderConfig("p", "https://x", "bad-key")).toThrow();
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
