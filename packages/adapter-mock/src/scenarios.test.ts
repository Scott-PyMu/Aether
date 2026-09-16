import { readFileSync } from "node:fs";

import { describe, expect, it } from "vitest";

import { scenarioForText, TOOL_CALL_SCENARIOS } from "./scenarios";

interface Fixture {
  scenarios: Array<{ id: string; label: string; trigger: string; events: string[] }>;
}

const fixtureUrl = new URL(
  "../../../scripts/test/m1-09/fixtures/tool-call-scenarios.json",
  import.meta.url,
);

describe("工具调用注入清单（DoD6 权威定义）", () => {
  it("代码常量与权威夹具逐项一致（供 M2-02/M2-10 复用）", () => {
    const fixture = JSON.parse(readFileSync(fixtureUrl, "utf8")) as Fixture;
    expect(fixture.scenarios).toHaveLength(5);
    for (const scenario of fixture.scenarios) {
      const actual = TOOL_CALL_SCENARIOS[scenario.id as keyof typeof TOOL_CALL_SCENARIOS];
      expect(actual, `缺少场景 ${scenario.id}`).toBeDefined();
      expect(actual.label).toBe(scenario.label);
      expect(actual.trigger).toBe(scenario.trigger);
      expect(actual.events, scenario.id).toEqual(scenario.events);
    }
  });

  it("触发文本唯一且可解析", () => {
    const triggers = Object.values(TOOL_CALL_SCENARIOS).map((scenario) => scenario.trigger);
    expect(new Set(triggers).size).toBe(triggers.length);
    for (const scenario of Object.values(TOOL_CALL_SCENARIOS)) {
      expect(scenarioForText(scenario.trigger)).toBe(scenario.id);
    }
    expect(scenarioForText("tool:unknown")).toBeUndefined();
    expect(scenarioForText("普通消息")).toBeUndefined();
  });
});
