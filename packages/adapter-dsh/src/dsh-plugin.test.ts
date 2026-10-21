import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { describe, expect, it } from "vitest";

import {
  buildPatchText,
  checkVersionLatch,
  clearPromptDir,
  DEFAULT_DSH_VERSION_PIN,
  materializePlugin,
  materializePromptPlugin,
  PLUGIN_CONTRACT,
  PLUGIN_INDEX_JS,
  promptFilePath,
  PROMPT_DIR_ENV,
  PROMPT_PLUGIN_INDEX_JS,
  PROMPT_PLUGIN_PACKAGE_NAME,
  PROMPT_SECTION_NAME,
  PROMPT_SECTION_ORDER,
  resolveDshVersion,
  validateProviderConfig,
  writePatchFile,
} from "./dsh-plugin";

describe("版本门闩（DoD1）", () => {
  it("pin 精确匹配通过；不匹配/未知拒绝并给升级提示", () => {
    expect(checkVersionLatch(DEFAULT_DSH_VERSION_PIN, DEFAULT_DSH_VERSION_PIN).ok).toBe(true);
    const mismatch = checkVersionLatch("0.1.1-rc.2", DEFAULT_DSH_VERSION_PIN);
    expect(mismatch.ok).toBe(false);
    expect(mismatch.detail).toContain(DEFAULT_DSH_VERSION_PIN);
    expect(mismatch.detail).toContain("0.1.1-rc.2");
    const unknown = checkVersionLatch(null, DEFAULT_DSH_VERSION_PIN);
    expect(unknown.ok).toBe(false);
    expect(unknown.detail).toContain("未知");
  });

  it("从 dsh-bin 向上解析 @deepseek-ai/dsh 包版本", () => {
    const root = mkdtempSync(join(tmpdir(), "dsh-version-"));
    const pkgDir = join(root, "node_modules", "@deepseek-ai", "dsh");
    const bin = join(pkgDir, "lib", "bin.js");
    mkdirSync(join(pkgDir, "lib"), { recursive: true });
    writeFileSync(
      join(pkgDir, "package.json"),
      JSON.stringify({ name: "@deepseek-ai/dsh", version: DEFAULT_DSH_VERSION_PIN }),
      "utf8",
    );
    writeFileSync(bin, "// fake\n", "utf8");
    expect(resolveDshVersion(bin)).toBe(DEFAULT_DSH_VERSION_PIN);

    const other = mkdtempSync(join(tmpdir(), "dsh-version-other-"));
    writeFileSync(join(other, "x.js"), "// x\n", "utf8");
    expect(resolveDshVersion(join(other, "x.js"))).toBeNull();
  });
});

describe("插件落盘与 overlay（DoD2）", () => {
  it("插件包写入 profile node_modules 且 package.json 无 BOM", () => {
    const home = mkdtempSync(join(tmpdir(), "dsh-plugin-"));
    const materialized = materializePlugin(join(home, "profiles", "acp"));
    expect(materialized.dir).toContain(join("profiles", "acp", "node_modules", "aether-dsh-stream"));
    const bytes = readFileSync(materialized.packageJsonPath);
    expect([bytes[0], bytes[1], bytes[2]]).not.toEqual([0xef, 0xbb, 0xbf]);
    const pkg = JSON.parse(bytes.toString("utf8")) as { name: string; type: string };
    expect(pkg.name).toBe("aether-dsh-stream");
    expect(pkg.type).toBe("module");
    expect(readFileSync(materialized.indexPath, "utf8")).toBe(PLUGIN_INDEX_JS);
    expect(PLUGIN_INDEX_JS).toContain("agent/assistant-stream");
    expect(PLUGIN_INDEX_JS).toContain(PLUGIN_CONTRACT);
  });

  it("overlay 覆写 acp 行并 insert 插件行", () => {
    const text = buildPatchText({ provider: "streamax", model: "deepseek-v4-pro" });
    expect(text).toContain("- id: acp");
    expect(text).toContain("provider: streamax");
    expect(text).toContain("model: deepseek-v4-pro");
    expect(text).toContain("- insert:");
    expect(text).toContain("name: aether-dsh-stream");
    const noProvider = buildPatchText({});
    expect(noProvider).not.toContain("- id: acp");
    expect(noProvider).toContain("insert");
  });

  it("providerConfig 以 composition base 写入 llm-pi-ai 行（官方分层，消竞态）", () => {
    const text = buildPatchText({
      provider: "streamax",
      model: "deepseek-v4-pro",
      providerConfig: {
        providers: {
          streamax: {
            apiKeyEnv: "STREAMAX_API_KEY",
            api: "openai-completions",
            baseURL: "https://example.invalid/open/v1",
            models: [{ id: "deepseek-v4-pro", name: "deepseek-v4-pro" }],
          },
        },
      },
    });
    expect(text).toContain("- id: llm-pi-ai");
    expect(text).toContain('"apiKeyEnv":"STREAMAX_API_KEY"');
    expect(text).toContain("https://example.invalid/open/v1");
    // JSON 内联为合法 YAML flow（不出现手写 YAML 缩进错误）。
    const llmLine = text.split("\n").find((line) => line.startsWith("  config: {")) ?? "";
    expect(llmLine).not.toBe("");
    expect(JSON.parse(llmLine.slice("  config: ".length))).toMatchObject({
      providers: { streamax: { api: "openai-completions" } },
    });
  });

  it("validateProviderConfig 拒绝空/畸形形状", () => {
    expect(() => validateProviderConfig(null)).toThrow("JSON 对象");
    expect(() => validateProviderConfig({})).toThrow('"providers"');
    expect(() => validateProviderConfig({ providers: {} })).toThrow("不得为空");
    expect(() => validateProviderConfig({ providers: { p: {} } })).not.toThrow();
  });

  it("writePatchFile 写出到 DSH_HOME 并返回路径", () => {
    const home = mkdtempSync(join(tmpdir(), "dsh-patch-"));
    const path = writePatchFile(home, "acp", { model: "m" });
    expect(path).toBe(join(home, "aether-dsh-acp.patch.yml"));
    expect(readFileSync(path, "utf8")).toContain("name: aether-dsh-stream");
  });
});

describe("M7-09/ADR-027 系统提示词注入插件", () => {
  it("侧信道文件名经 URL 编码（防御 native_id 路径分隔符）", () => {
    const dir = join(tmpdir(), "dsh-prompt-path");
    expect(promptFilePath(dir, "a/b\\c")).toBe(join(dir, "a%2Fb%5Cc.md"));
    expect(promptFilePath(dir, "..")).toBe(join(dir, "...md"));
  });

  it("启动清理仅删除 .md 侧信道文件（保留其他文件）", () => {
    const dir = mkdtempSync(join(tmpdir(), "dsh-prompt-clear-"));
    writeFileSync(join(dir, "a.md"), "x", "utf8");
    writeFileSync(join(dir, "b.md"), "y", "utf8");
    writeFileSync(join(dir, "keep.json"), "{}", "utf8");
    expect(clearPromptDir(dir)).toBe(2);
    expect(existsSync(join(dir, "a.md"))).toBe(false);
    expect(existsSync(join(dir, "b.md"))).toBe(false);
    expect(existsSync(join(dir, "keep.json"))).toBe(true);
  });

  it("插件包落盘（无 BOM）且 overlay 同时 insert 两个插件", () => {
    const home = mkdtempSync(join(tmpdir(), "dsh-prompt-plugin-"));
    const materialized = materializePromptPlugin(join(home, "profiles", "acp"));
    expect(materialized.dir).toContain(
      join("profiles", "acp", "node_modules", PROMPT_PLUGIN_PACKAGE_NAME),
    );
    const bytes = readFileSync(materialized.packageJsonPath);
    expect([bytes[0], bytes[1], bytes[2]]).not.toEqual([0xef, 0xbb, 0xbf]);
    const pkg = JSON.parse(bytes.toString("utf8")) as { name: string; type: string };
    expect(pkg.name).toBe(PROMPT_PLUGIN_PACKAGE_NAME);
    expect(pkg.type).toBe("module");
    expect(readFileSync(materialized.indexPath, "utf8")).toBe(PROMPT_PLUGIN_INDEX_JS);
    expect(PROMPT_PLUGIN_INDEX_JS).toContain('inject = ["systemPrompt"]');
    expect(PROMPT_PLUGIN_INDEX_JS).toContain('ctx.on("agent/created"');
    expect(PROMPT_PLUGIN_INDEX_JS).toContain("agent.ctx.systemPrompt.section");
    expect(PROMPT_PLUGIN_INDEX_JS).toContain(PROMPT_SECTION_NAME);
    expect(PROMPT_PLUGIN_INDEX_JS).toContain(String(PROMPT_SECTION_ORDER));
    expect(PROMPT_PLUGIN_INDEX_JS).toContain(PROMPT_DIR_ENV);

    const patch = buildPatchText({});
    expect(patch).toContain("name: aether-dsh-stream");
    expect(patch).toContain(`name: ${PROMPT_PLUGIN_PACKAGE_NAME}`);
    expect(patch.match(/- id: /g)).toHaveLength(2);
  });

  it("运行时行为：agent/created 注册 agent 作用域 section；按会话 id 读文件；缺失返回空串", async () => {
    // CI Windows 临时目录可能为 8.3 短名（RUNNER~1）；动态 import 需真实长路径。
    const home = realpathSync(mkdtempSync(join(tmpdir(), "dsh-prompt-run-")));
    const promptDir = join(home, "prompts");
    mkdirSync(promptDir, { recursive: true });
    const materialized = materializePromptPlugin(join(home, "profiles", "acp"));
    const sessionId = "acp-test-session";
    const prompt = "<!-- aether:preset:01J00000000000000000000P02 -->\n你是资深软件工程师\n\n# 工作区约定\n来源：AGENTS.md\n\n约定";
    writeFileSync(promptFilePath(promptDir, sessionId), prompt, "utf8");

    const previous = process.env[PROMPT_DIR_ENV];
    process.env[PROMPT_DIR_ENV] = promptDir;
    try {
      // 环境变量在模块加载时读取：以查询串绕过模块缓存。
      const moduleUrl = `${pathToFileURL(materialized.indexPath).href}?v=${Date.now()}`;
      const plugin = (await import(moduleUrl)) as {
        name: string;
        inject: string[];
        apply: (ctx: unknown) => void;
      };
      expect(plugin.name).toBe(PROMPT_PLUGIN_PACKAGE_NAME);
      expect(plugin.inject).toEqual(["systemPrompt"]);

      const handlers: Array<(payload: { agent: unknown }) => void> = [];
      const ctx = {
        on: (event: string, listener: (payload: { agent: unknown }) => void) => {
          if (event === "agent/created") handlers.push(listener);
          return () => undefined;
        },
      };
      plugin.apply(ctx);
      expect(handlers).toHaveLength(1);

      const sections: Array<{
        name: string;
        order: number;
        text: () => string;
        interpolate?: boolean;
      }> = [];
      const agent = {
        id: sessionId,
        ctx: {
          systemPrompt: {
            section: (section: { name: string; order: number; text: () => string }) => {
              sections.push(section);
              return () => undefined;
            },
          },
        },
      };
      handlers[0]?.({ agent });
      expect(sections).toHaveLength(1);
      expect(sections[0]?.name).toBe(PROMPT_SECTION_NAME);
      expect(sections[0]?.order).toBe(PROMPT_SECTION_ORDER);
      expect(sections[0]?.text()).toBe(prompt);
      // 用户提示词含 `{{...}}` 时必须按原文注入，不能被 DSH 变量插值破坏。
      expect(sections[0]?.interpolate).toBe(false);

      // 另一会话/缺失文件 → 空串（不注入）。
      const other = {
        id: "acp-missing",
        ctx: {
          systemPrompt: {
            section: (section: { name: string; order: number; text: () => string }) => {
              sections.push(section);
              return () => undefined;
            },
          },
        },
      };
      handlers[0]?.({ agent: other });
      expect(sections[1]?.text()).toBe("");
    } finally {
      if (previous === undefined) delete process.env[PROMPT_DIR_ENV];
      else process.env[PROMPT_DIR_ENV] = previous;
    }
  });
});
