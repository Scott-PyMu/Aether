import { describe, expect, it } from "vitest";

import {
  dshProviderKey,
  modelOptionEntriesFrom,
  parseModelValue,
  providerFromEnv,
  resolveModelOption,
} from "./dsh-model";

/** 与真实 DSH 0.2.1-rc.2 实测形态一致（`session/new` configOptions 摘录；0.1.5-rc.2 形态相同）。 */
const REAL_SHAPED_CONFIG_OPTIONS = [
  {
    id: "model",
    name: "Model",
    category: "model",
    type: "select",
    currentValue: '["streamax","deepseek-v4-pro"]',
    options: [
      {
        group: "deepseek-official",
        name: "DeepSeek",
        options: [
          { value: '["deepseek-official","deepseek-v4-flash"]', name: "DeepSeek-V4-Flash" },
          { value: '["deepseek-official","deepseek-v4-pro"]', name: "DeepSeek-V4-Pro" },
        ],
      },
      {
        group: "streamax",
        name: "streamax",
        options: [
          { value: '["streamax","deepseek-v4-pro"]', name: "deepseek-v4-pro" },
          { value: '["streamax","deepseek-v4-flash"]', name: "deepseek-v4-flash" },
        ],
      },
    ],
  },
  { id: "reasoning_effort", category: "thought_level", options: [] },
];

describe("M7-06 DSH 按会话选模型（ADR-024 DoD4）", () => {
  it("解析真实形态的 model 选项（保序；非 model 配置项忽略）", () => {
    const entries = modelOptionEntriesFrom(REAL_SHAPED_CONFIG_OPTIONS);
    expect(entries).toEqual([
      { provider: "deepseek-official", model: "deepseek-v4-flash", value: '["deepseek-official","deepseek-v4-flash"]' },
      { provider: "deepseek-official", model: "deepseek-v4-pro", value: '["deepseek-official","deepseek-v4-pro"]' },
      { provider: "streamax", model: "deepseek-v4-pro", value: '["streamax","deepseek-v4-pro"]' },
      { provider: "streamax", model: "deepseek-v4-flash", value: '["streamax","deepseek-v4-flash"]' },
    ]);
    expect(modelOptionEntriesFrom(undefined)).toEqual([]);
    expect(modelOptionEntriesFrom("x")).toEqual([]);
  });

  it("parseModelValue：仅接受 [provider, model] 两元素字符串数组", () => {
    expect(parseModelValue('["a","b"]')).toEqual(["a", "b"]);
    expect(parseModelValue("not-json")).toBeNull();
    expect(parseModelValue('["a"]')).toBeNull();
    expect(parseModelValue('["a","b","c"]')).toBeNull();
    expect(parseModelValue('["","b"]')).toBeNull();
    expect(parseModelValue('[1,"b"]')).toBeNull();
  });

  it("选择规则：优先消费映射 provider，否则首个匹配；无匹配 → undefined", () => {
    const entries = modelOptionEntriesFrom(REAL_SHAPED_CONFIG_OPTIONS);
    // 优先 preferredProvider。
    expect(resolveModelOption(entries, "deepseek-v4-pro", "streamax")?.provider).toBe("streamax");
    // preferred 不命中该模型 → 回退首个匹配。
    expect(resolveModelOption(entries, "deepseek-v4-flash", "streamax")?.provider).toBe("streamax");
    expect(resolveModelOption(entries, "deepseek-v4-flash", "not-exist")?.provider).toBe(
      "deepseek-official",
    );
    // 无匹配。
    expect(resolveModelOption(entries, "unknown-model", "streamax")).toBeUndefined();
  });

  it("providerFromEnv：注入 env → DSH provider 配置（密钥值不入配置；缺省模型透出）", () => {
    expect(providerFromEnv({})).toBeNull();
    const provider = providerFromEnv({
      AETHER_PROVIDER_BASE_URL: "https://gateway.example.com/open/v1",
      AETHER_PROVIDER_ID: "01J00000000000000000000P02",
      AETHER_PROVIDER_ENV_KEY: "DEEPSEEK_API_KEY",
      AETHER_PROVIDER_MODEL: "m-1",
      AETHER_PROVIDER_MODELS: JSON.stringify([
        { id: "m-1", name: "m-1" },
        { id: "m-2", name: "m-2" },
        { id: "", name: "skip" },
      ]),
    });
    expect(provider).not.toBeNull();
    expect(provider?.provider).toBe(dshProviderKey("01J00000000000000000000P02"));
    expect(provider?.defaultModel).toBe("m-1");
    const config = provider?.providerConfig as { providers: Record<string, unknown> };
    const entry = config.providers[provider!.provider] as Record<string, unknown>;
    expect(entry.apiKeyEnv).toBe("DEEPSEEK_API_KEY");
    expect(entry.api).toBe("openai-completions");
    expect(entry.baseURL).toBe("https://gateway.example.com/open/v1");
    expect(entry.models).toEqual([
      { id: "m-1", name: "m-1" },
      { id: "m-2", name: "m-2" },
    ]);
    expect(JSON.stringify(config)).not.toContain("sk-");
    // 无 AETHER_PROVIDER_MODEL → defaultModel 省略。
    const withoutModel = providerFromEnv({
      AETHER_PROVIDER_BASE_URL: "https://x",
    });
    expect(withoutModel?.defaultModel).toBeUndefined();
  });

  it("providerFromEnv：非法 models JSON → 空清单（不抛错）", () => {
    const provider = providerFromEnv({
      AETHER_PROVIDER_BASE_URL: "https://x",
      AETHER_PROVIDER_MODELS: "{not json",
    });
    const config = provider?.providerConfig as { providers: Record<string, { models: unknown[] }> };
    expect(config.providers[provider!.provider]!.models).toEqual([]);
  });
});
