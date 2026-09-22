import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import { parseArgs } from "./cli";
import { DEFAULT_DSH_VERSION_PIN } from "./dsh-plugin";

const baseArgs = ["--dsh-bin", "/opt/dsh/lib/bin.js", "--dsh-home", "/tmp/dsh-home"];

describe("parseArgs（DSH CLI）", () => {
  it("解析 providerConfig（内联 JSON）与默认值", () => {
    const options = parseArgs(
      [
        ...baseArgs,
        "--dsh-provider",
        "streamax",
        "--dsh-model",
        "deepseek-v4-pro",
        "--dsh-version",
        "0.1.5-rc.2",
        "--dsh-provider-config",
        '{"providers":{"streamax":{"api":"openai-completions","baseURL":"https://x/open/v1"}}}',
      ],
      () => {},
    );
    expect(options.cli.versionPin).toBe(DEFAULT_DSH_VERSION_PIN);
    expect(options.cli.provider).toBe("streamax");
    expect(options.cli.providerConfig).toMatchObject({
      providers: { streamax: { api: "openai-completions" } },
    });
  });

  it("解析 providerConfig（JSON 文件路径；容忍 UTF-8 BOM）", () => {
    const dir = mkdtempSync(join(tmpdir(), "dsh-cli-"));
    const file = join(dir, "provider.json");
    writeFileSync(
      file,
      JSON.stringify({ providers: { p: { api: "openai-completions" } } }),
      "utf8",
    );
    const options = parseArgs([...baseArgs, "--dsh-provider-config", file], () => {});
    expect(options.cli.providerConfig).toMatchObject({ providers: { p: {} } });

    const bomFile = join(dir, "provider-bom.json");
    writeFileSync(
      bomFile,
      `\uFEFF${JSON.stringify({ providers: { p: { api: "openai-completions" } } })}`,
      "utf8",
    );
    const bomOptions = parseArgs([...baseArgs, "--dsh-provider-config", bomFile], () => {});
    expect(bomOptions.cli.providerConfig).toMatchObject({ providers: { p: {} } });
  });

  it("拒绝畸形 providerConfig 与缺参", () => {
    expect(() => parseArgs([...baseArgs, "--dsh-provider-config", "not-json"], () => {})).toThrow(
      "不是合法 JSON",
    );
    expect(() =>
      parseArgs([...baseArgs, "--dsh-provider-config", '{"providers":{}}'], () => {}),
    ).toThrow("不得为空");
    expect(() => parseArgs(["--dsh-home", "/tmp/h"], () => {})).toThrow("--dsh-bin");
    expect(() => parseArgs(["--dsh-bin", "/x"], () => {})).toThrow("--dsh-home");
  });

  it("launch-token 非法值忽略且不阻塞", () => {
    const warnings: string[] = [];
    const options = parseArgs([...baseArgs, "--launch-token=bad"], (line) => warnings.push(line));
    expect(options.launchToken).toBeUndefined();
    expect(warnings.some((line) => line.includes("忽略非法"))).toBe(true);
  });
});
