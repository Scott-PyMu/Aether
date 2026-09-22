import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import {
  buildPatchText,
  checkVersionLatch,
  DEFAULT_DSH_VERSION_PIN,
  materializePlugin,
  PLUGIN_CONTRACT,
  PLUGIN_INDEX_JS,
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
