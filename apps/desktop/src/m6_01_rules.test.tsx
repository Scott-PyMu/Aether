/**
 * M6-01 权限规则管理页（UI 面；ADR-019 / ADR-018；UI-UX §7.3）。
 *
 * 覆盖（props 注入替身模式，同 m3_03）：
 * - 列表锚点 `rule-item` + `data-rule-id` / `data-enabled`；启停 `rule-toggle`；
 * - 删除二次确认（`rule-delete` → `rule-delete-confirm-button` → `deleteRule`）；
 * - 新增 `rule_add` payload 形状（`action` 由 `resource` 推导；`target_kind` 联动；
 *   `any` 时省略 `target`）；
 * - `rule_not_found` → 「规则不存在，已刷新列表」且自动重取 `rules_list`；
 * - SettingsPage 第三 tab `settings-tab-rules` 透传替身。
 */
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  rulesIpc as productionRulesIpc,
  type RuleAddInput,
  type RuleEntry,
  type RuleUpdateInput,
  type RulesIpc,
} from "./rules";
import { RulesPage } from "./RulesPage";
import { SettingsPage } from "./SettingsPage";

const RULE_A = "01J0000000000000000000RUL1";
const RULE_B = "01J0000000000000000000RUL2";
const WAIT = { timeout: 5000 };

afterEach(cleanup);

function ruleEntry(overrides: Partial<RuleEntry> = {}): RuleEntry {
  return {
    id: RULE_A,
    name: "规则",
    resource: "fs.read",
    action: "read",
    target_kind: "path",
    target: "C:\\ws",
    decision: "allow",
    priority: 0,
    enabled: true,
    created_at: 1,
    updated_at: 1,
    ...overrides,
  };
}

interface FakeRules {
  ipc: RulesIpc;
  store: RuleEntry[];
  listRules: ReturnType<typeof vi.fn>;
  addRule: ReturnType<typeof vi.fn>;
  updateRule: ReturnType<typeof vi.fn>;
  deleteRule: ReturnType<typeof vi.fn>;
  toggleRule: ReturnType<typeof vi.fn>;
}

/** 规则存储替身（列表可变；各命令为 spy 供断言；`rule_not_found` 可按需抛错）。 */
function fakeRulesIpc(seed: RuleEntry[] = []): FakeRules {
  const store: RuleEntry[] = seed.map((rule) => ({ ...rule }));
  let counter = 0;
  const nextId = () => `01J${String(++counter).padStart(23, "0")}`;
  const listRules = vi.fn(async () => store.map((rule) => ({ ...rule })));
  const addRule = vi.fn(async (input: RuleAddInput) => {
    const created: RuleEntry = {
      id: nextId(),
      name: input.name,
      resource: input.resource,
      action: input.action,
      target_kind: input.target_kind,
      target: input.target ?? null,
      decision: input.decision,
      priority: input.priority ?? 0,
      enabled: true,
      created_at: 2,
      updated_at: 2,
    };
    store.push(created);
    return { ...created };
  });
  const updateRule = vi.fn(async (input: RuleUpdateInput) => {
    const target = store.find((rule) => rule.id === input.rule_id);
    if (!target) {
      throw { code: "rule_not_found", message: "规则不存在" };
    }
    if (input.name !== undefined) target.name = input.name;
    if (input.resource !== undefined) target.resource = input.resource;
    if (input.action !== undefined) target.action = input.action;
    if (input.target_kind !== undefined) target.target_kind = input.target_kind;
    if (input.target !== undefined) target.target = input.target;
    if (input.decision !== undefined) target.decision = input.decision;
    if (input.priority !== undefined) target.priority = input.priority;
    target.updated_at = 3;
    return { ...target };
  });
  const deleteRule = vi.fn(async (ruleId: string) => {
    const index = store.findIndex((rule) => rule.id === ruleId);
    if (index < 0) {
      throw { code: "rule_not_found", message: "规则不存在" };
    }
    store.splice(index, 1);
    return { rule_id: ruleId, deleted: true };
  });
  const toggleRule = vi.fn(async (ruleId: string, enabled: boolean) => {
    const target = store.find((rule) => rule.id === ruleId);
    if (!target) {
      throw { code: "rule_not_found", message: "规则不存在" };
    }
    target.enabled = enabled;
    return { rule_id: ruleId, enabled };
  });
  return {
    ipc: { listRules, addRule, updateRule, deleteRule, toggleRule },
    store,
    listRules,
    addRule,
    updateRule,
    deleteRule,
    toggleRule,
  };
}

describe("RulesPage（M6-01）", () => {
  it("列表 data-enabled；启停调用 rule_toggle payload", async () => {
    const rules = fakeRulesIpc([
      ruleEntry({ id: RULE_A, name: "允许读取" }),
      ruleEntry({
        id: RULE_B,
        name: "拒绝执行",
        resource: "exec",
        action: "exec",
        target_kind: "command",
        target: "rm",
        decision: "deny",
        enabled: false,
      }),
    ]);
    render(<RulesPage ipc={rules.ipc} />);
    const items = await screen.findAllByTestId("rule-item", {}, WAIT);
    expect(items.map((item) => item.getAttribute("data-rule-id"))).toEqual([RULE_A, RULE_B]);
    expect(items.map((item) => item.getAttribute("data-enabled"))).toEqual(["true", "false"]);
    expect(
      within(items[0] as HTMLElement).getByTestId("rule-toggle").getAttribute("title"),
    ).toContain("停用");
    expect(
      within(items[1] as HTMLElement).getByTestId("rule-toggle").getAttribute("title"),
    ).toContain("启用");

    fireEvent.click(within(items[0] as HTMLElement).getByTestId("rule-toggle"));
    await waitFor(() => expect(rules.toggleRule).toHaveBeenCalledWith(RULE_A, false), WAIT);
    await waitFor(() => {
      const updated = screen
        .getAllByTestId("rule-item")
        .find((item) => item.getAttribute("data-rule-id") === RULE_A);
      expect(updated?.getAttribute("data-enabled")).toBe("false");
    }, WAIT);
  });

  it("删除：二次确认（可取消）后调用 rule_delete", async () => {
    const rules = fakeRulesIpc([ruleEntry({ id: RULE_A }), ruleEntry({ id: RULE_B, name: "第二条" })]);
    render(<RulesPage ipc={rules.ipc} />);
    await screen.findAllByTestId("rule-item", {}, WAIT);

    fireEvent.click(
      within(screen.getAllByTestId("rule-item")[1] as HTMLElement).getByTestId("rule-delete"),
    );
    expect(screen.getByTestId("rule-delete-confirm")).toBeTruthy();
    fireEvent.click(screen.getByTestId("rule-delete-cancel"));
    expect(screen.queryByTestId("rule-delete-confirm")).toBeNull();
    expect(rules.deleteRule).not.toHaveBeenCalled();

    fireEvent.click(
      within(screen.getAllByTestId("rule-item")[1] as HTMLElement).getByTestId("rule-delete"),
    );
    fireEvent.click(screen.getByTestId("rule-delete-confirm-button"));
    await waitFor(() => expect(rules.deleteRule).toHaveBeenCalledWith(RULE_B), WAIT);
    await waitFor(() => expect(screen.getAllByTestId("rule-item")).toHaveLength(1), WAIT);
  });

  it("新增：action 推导 + target_kind 联动 + any 省略 target", async () => {
    const rules = fakeRulesIpc([]);
    render(<RulesPage ipc={rules.ipc} />);
    await screen.findByTestId("rules-empty", {}, WAIT);

    fireEvent.change(screen.getByTestId("rule-name-input"), {
      target: { value: "允许写区外" },
    });
    fireEvent.change(screen.getByTestId("rule-resource-select"), {
      target: { value: "fs.write" },
    });
    fireEvent.change(screen.getByTestId("rule-target-input"), {
      target: { value: "C:\\ws\\out" },
    });
    fireEvent.change(screen.getByTestId("rule-priority-input"), { target: { value: "5" } });
    fireEvent.click(screen.getByTestId("rule-add-submit"));
    await waitFor(() => expect(rules.addRule).toHaveBeenCalledTimes(1), WAIT);
    expect(rules.addRule.mock.calls[0]?.[0]).toEqual({
      name: "允许写区外",
      resource: "fs.write",
      action: "write",
      target_kind: "path",
      target: "C:\\ws\\out",
      decision: "allow",
      priority: 5,
    });

    // 联动：fs.write(path) → exec 不兼容 → 回退 command；exec → net 回退 host。
    fireEvent.change(screen.getByTestId("rule-resource-select"), { target: { value: "exec" } });
    await waitFor(
      () =>
        expect(
          (screen.getByTestId("rule-target-kind-select") as HTMLSelectElement).value,
        ).toBe("command"),
      WAIT,
    );
    fireEvent.change(screen.getByTestId("rule-resource-select"), { target: { value: "net" } });
    await waitFor(
      () =>
        expect(
          (screen.getByTestId("rule-target-kind-select") as HTMLSelectElement).value,
        ).toBe("host"),
      WAIT,
    );
    const kindSelect = screen.getByTestId("rule-target-kind-select") as HTMLSelectElement;
    expect([...kindSelect.options].map((option) => option.value)).toEqual(["host", "any"]);

    // any → target 省略（字段禁用）。
    fireEvent.change(kindSelect, { target: { value: "any" } });
    expect((screen.getByTestId("rule-target-input") as HTMLInputElement).disabled).toBe(true);
    fireEvent.change(screen.getByTestId("rule-name-input"), {
      target: { value: "全网络放行" },
    });
    fireEvent.click(screen.getByTestId("rule-add-submit"));
    await waitFor(() => expect(rules.addRule).toHaveBeenCalledTimes(2), WAIT);
    const payload = rules.addRule.mock.calls[1]?.[0] as RuleAddInput;
    expect(payload).toEqual({
      name: "全网络放行",
      resource: "net",
      action: "connect",
      target_kind: "any",
      decision: "allow",
      priority: 0,
    });
    expect(payload).not.toHaveProperty("target");
  });

  it("rule_not_found：「规则不存在，已刷新列表」+ 自动重取 listRules", async () => {
    const rules = fakeRulesIpc([ruleEntry({ id: RULE_A })]);
    rules.toggleRule.mockRejectedValueOnce({ code: "rule_not_found", message: "规则不存在" });
    render(<RulesPage ipc={rules.ipc} />);
    await screen.findAllByTestId("rule-item", {}, WAIT);
    const listCallsBefore = rules.listRules.mock.calls.length;

    fireEvent.click(screen.getByTestId("rule-toggle"));
    const error = await screen.findByTestId("rules-error", {}, WAIT);
    expect(error.getAttribute("data-code")).toBe("rule_not_found");
    expect(error.textContent).toContain("规则不存在，已刷新列表");
    await waitFor(
      () => expect(rules.listRules.mock.calls.length).toBeGreaterThan(listCallsBefore),
      WAIT,
    );
  });

  it("SettingsPage：settings-tab-rules 可达并透传 rulesIpc 替身", async () => {
    const rules = fakeRulesIpc([ruleEntry({ id: RULE_A, name: "允许读取" })]);
    render(<SettingsPage dataDir="C:\\data" rulesIpc={rules.ipc} onBack={() => {}} />);
    fireEvent.click(screen.getByTestId("settings-tab-rules"));
    expect(screen.getByTestId("settings-tab-rules").getAttribute("data-active")).toBe("true");
    await screen.findByTestId("rules-page", {}, WAIT);
    const items = await screen.findAllByTestId("rule-item", {}, WAIT);
    expect(items[0]?.getAttribute("data-rule-id")).toBe(RULE_A);
  });

  it("生产契约存在且命令名与生成绑定一致", () => {
    expect(typeof productionRulesIpc.listRules).toBe("function");
    expect(typeof productionRulesIpc.addRule).toBe("function");
    expect(typeof productionRulesIpc.updateRule).toBe("function");
    expect(typeof productionRulesIpc.deleteRule).toBe("function");
    expect(typeof productionRulesIpc.toggleRule).toBe("function");
  });
});
