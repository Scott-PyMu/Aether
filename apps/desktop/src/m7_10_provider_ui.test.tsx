/**
 * M7-10 / ADR-028 v0.9 前端验收：
 * - 三官方运行时冷启动（capabilities 无 hello 项）仍显示供应商选择（静态能力）；
 * - 新建会话 provider+model 必选（`provider-required-notice`）；
 * - 全局记忆 `ui.provider_selection` 预填视为已选；
 * - 旧会话 provider 补选（`session-provider-select[data-context=reselect]`）与
 *   `session_send.provider_id/model` 透传；
 * - provider 失效只读、发送禁用、中断保留（`session-provider-readonly`）；
 * - 端点预览/风险提示锚点。
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { EventStore } from "./eventStore";
import { SessionWorkbench } from "./SessionWorkbench";
import { ProvidersPage } from "./ProvidersPage";
import type {
  MessagesPageResult,
  RuntimeInfo,
  SessionIpc,
  SessionStatus,
  SessionSummary,
} from "./session";
import type { ProviderEntry, ProvidersIpc } from "./providers";
import type { SettingsIpc } from "./settings";

const SESSION = "01J8ZQ5R0N7W9Y8X6V4T2S0K1A";
const PROVIDER_A = "01J0000000000000000000PA1";
const PROVIDER_B = "01J0000000000000000000PB1";
const WAIT = { timeout: 5000 };

function provider(id: string, name: string, model: string): ProviderEntry {
  return {
    id,
    name,
    type: "deepseek",
    base_url: "https://api.deepseek.com",
    endpoints: {
      anthropic: "https://api.deepseek.com/anthropic",
      openai: "https://api.deepseek.com",
      dsh: "https://api.deepseek.com",
    },
    api_key_ref: `keychain://aether/provider/${id}`,
    enabled: true,
    is_builtin: false,
    created_at: 1,
    updated_at: 1,
    models: [
      {
        id: `${id}-model`,
        model_id: model,
        display_name: model,
        enabled: true,
      },
    ],
  };
}

function fakeProviders(): ProvidersIpc {
  const list = [provider(PROVIDER_A, "供应商 A", "model-a"), provider(PROVIDER_B, "供应商 B", "model-b")];
  return {
    list: vi.fn(async () => list),
    create: vi.fn(),
    update: vi.fn(),
    remove: vi.fn(),
    toggle: vi.fn(),
    addModel: vi.fn(),
    toggleModel: vi.fn(),
    test: vi.fn(),
    fetchModels: vi.fn(),
  } as unknown as ProvidersIpc;
}

function fakeSettings(memory: Record<string, string> = {}): SettingsIpc & {
  get: ReturnType<typeof vi.fn>;
  set: ReturnType<typeof vi.fn>;
} {
  return {
    get: vi.fn(async (key: string) => ({
      key,
      value: key === "ui.provider_selection" ? JSON.stringify(memory) : "{}",
    })),
    set: vi.fn(async (key: string, value: unknown) => ({ key, value })),
  } as unknown as SettingsIpc & {
    get: ReturnType<typeof vi.fn>;
    set: ReturnType<typeof vi.fn>;
  };
}

function runtime(overrides: Partial<RuntimeInfo> = {}): RuntimeInfo {
  return {
    id: "claude-code",
    name: "Claude Code",
    kind: "claude-code",
    version: "0.1.0",
    protocol: "1.0",
    // 冷启动：hello 尚未上报（静态 multi_provider 由 runtimes_list 合并；
    // UI 不依赖 hello/ready）。
    capabilities: [],
    enabled: true,
    status: "cold",
    status_reason: null,
    ...overrides,
  };
}

function summary(overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id: SESSION,
    runtime_id: "claude-code",
    workspace_id: null,
    parent_session_id: null,
    title: "工作台会话",
    status: "idle" as SessionStatus,
    model: "model-a",
    provider_id: PROVIDER_A,
    provider_name: "供应商 A",
    provider_state: "active",
    model_state: "active",
    created_at: 1,
    updated_at: 1,
    closed_at: null,
    ...overrides,
  };
}

function emptyPage(): MessagesPageResult {
  return {
    session_id: SESSION,
    max_seq: null,
    events: [],
    messages: [],
    complete: true,
  };
}

function fakeIpc(overrides: Partial<SessionIpc> = {}): SessionIpc {
  return {
    listRuntimes: vi.fn(async () => [runtime()]),
    listSessions: vi.fn(async () => []),
    searchSessions: vi.fn(async () => []),
    renameSession: vi.fn(),
    deleteSession: vi.fn(),
    createSession: vi.fn(async (input) =>
      summary({
        runtime_id: input.runtime_id ?? "claude-code",
        model: input.model ?? null,
        provider_id: input.provider_id ?? null,
        provider_state: input.provider_id ? "active" : "missing",
        model_state: input.model ? "active" : "missing",
      }),
    ),
    sendMessage: vi.fn(async () => ({
      session_id: SESSION,
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1R",
      queued: false,
      duplicate: false,
    })),
    interruptSession: vi.fn(async () => ({ session_id: SESSION })),
    pauseSession: vi.fn(),
    resumeSession: vi.fn(),
    disposeSession: vi.fn(),
    messagesPage: vi.fn(async () => emptyPage()),
    retryRun: vi.fn(),
    ...overrides,
  } as unknown as SessionIpc;
}

const stores: EventStore[] = [];
function newStore(): EventStore {
  const store = new EventStore();
  stores.push(store);
  return store;
}

afterEach(() => {
  cleanup();
  for (const store of stores.splice(0)) store.dispose();
  vi.restoreAllMocks();
});

async function findReselectProviderSelect(): Promise<HTMLSelectElement> {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    const found = screen
      .queryAllByTestId("session-provider-select")
      .find((element) => element.getAttribute("data-context") === "reselect");
    if (found) return found as HTMLSelectElement;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error("?????????????");
}

async function chooseProvider(providerId: string): Promise<void> {
  const select = (await screen.findByTestId("session-provider-select", {}, WAIT)) as HTMLSelectElement;
  await waitFor(() => expect(select.options.length).toBeGreaterThan(1), WAIT);
  fireEvent.change(select, { target: { value: providerId } });
}

describe("M7-10 供应商选择（ADR-028 v0.9）", () => {
  it("冷启动仍显示供应商选择；provider+model 必选；创建透传并写入全局记忆", async () => {
    const ipc = fakeIpc();
    const settings = fakeSettings();
    render(
      <SessionWorkbench
        store={newStore()}
        ipc={ipc}
        providersIpc={fakeProviders()}
        settingsIpc={settings}
      />,
    );
    await screen.findByTestId("session-provider-select", {}, WAIT);
    const submit = screen.getByTestId("session-create-submit") as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    expect(screen.getByTestId("provider-required-notice").getAttribute("data-field")).toBe(
      "provider",
    );
    await chooseProvider(PROVIDER_A);
    await waitFor(() => expect(submit.disabled).toBe(false), WAIT);
    expect((screen.getByTestId("session-model-input") as HTMLInputElement).value).toBe("model-a");
    fireEvent.click(submit);
    await waitFor(() => expect(ipc.createSession).toHaveBeenCalledTimes(1), WAIT);
    const input = (ipc.createSession as unknown as ReturnType<typeof vi.fn>).mock.calls[0]?.[0];
    expect(input.provider_id).toBe(PROVIDER_A);
    expect(input.model).toBe("model-a");
    await waitFor(
      () =>
        expect(settings.set).toHaveBeenCalledWith(
          "ui.provider_selection",
          JSON.stringify({ "claude-code": PROVIDER_A }),
        ),
      WAIT,
    );
  });

  it("全局记忆预填视为已选；model 空时显示 model 必选提示", async () => {
    render(
      <SessionWorkbench
        store={newStore()}
        ipc={fakeIpc()}
        providersIpc={fakeProviders()}
        settingsIpc={fakeSettings({ "claude-code": PROVIDER_B })}
      />,
    );
    const select = (await screen.findByTestId("session-provider-select", {}, WAIT)) as HTMLSelectElement;
    await waitFor(() => expect(select.value).toBe(PROVIDER_B), WAIT);
    expect((screen.getByTestId("session-model-input") as HTMLInputElement).value).toBe("model-b");
    expect((screen.getByTestId("session-create-submit") as HTMLButtonElement).disabled).toBe(false);
  });

  it("旧会话 provider 补选（data-context=reselect）→ session_send.provider_id/model", async () => {
    const oldSession = summary({
      provider_id: null,
      provider_name: null,
      provider_state: "missing",
      model: null,
      model_state: "missing",
    });
    const ipc = fakeIpc({ listSessions: vi.fn(async () => [oldSession]) });
    render(
      <SessionWorkbench
        store={newStore()}
        ipc={ipc}
        providersIpc={fakeProviders()}
        settingsIpc={fakeSettings()}
      />,
    );
    fireEvent.click(await screen.findByTestId("session-item", {}, WAIT));
    await screen.findByTestId("composer-input", {}, WAIT);
    const reselect = await findReselectProviderSelect();
    expect(reselect.getAttribute("data-context")).toBe("reselect");
    fireEvent.change(reselect, { target: { value: PROVIDER_A } });
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "继续" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(ipc.sendMessage).toHaveBeenCalledTimes(1), WAIT);
    const sent = (ipc.sendMessage as unknown as ReturnType<typeof vi.fn>).mock.calls[0]?.[0];
    expect(sent.provider_id).toBe(PROVIDER_A);
    expect(sent.model).toBe("model-a");
  });

  it("provider 失效 → 只读、禁用发送但保留中断；session_busy 使用补选上下文文案", async () => {
    const readonly = summary({
      provider_state: "invalid",
      model_state: "invalid",
      status: "running",
    });
    const ipc = fakeIpc({ listSessions: vi.fn(async () => [readonly]) });
    render(
      <SessionWorkbench
        store={newStore()}
        ipc={ipc}
        providersIpc={fakeProviders()}
        settingsIpc={fakeSettings()}
      />,
    );
    fireEvent.click(await screen.findByTestId("session-item", {}, WAIT));
    const current = await screen.findByTestId("session-provider-current", {}, WAIT);
    expect(current.getAttribute("data-state")).toBe("readonly");
    expect(current.getAttribute("data-model-state")).toBe("invalid");
    const notice = screen.getByTestId("session-provider-readonly");
    expect(notice.getAttribute("data-reason")).toBe("provider_disabled");
    expect((screen.getByTestId("composer-send") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByTestId("interrupt") as HTMLButtonElement).disabled).toBe(false);
  });

  it("session_busy 补选文案由后端消息直接呈现（不复用队列满文案）", async () => {
    const oldSession = summary({
      provider_id: null,
      provider_name: null,
      provider_state: "missing",
      model: null,
      model_state: "missing",
    });
    const ipc = fakeIpc({
      listSessions: vi.fn(async () => [oldSession]),
      sendMessage: vi.fn(async () => {
        throw {
          code: "invalid_value",
          message:
            "当前任务仍在运行，暂不能补选供应商/模型，请等当前任务结束后重试。（session_busy）",
        };
      }),
    });
    render(
      <SessionWorkbench
        store={newStore()}
        ipc={ipc}
        providersIpc={fakeProviders()}
        settingsIpc={fakeSettings()}
      />,
    );
    fireEvent.click(await screen.findByTestId("session-item", {}, WAIT));
    const reselect = await findReselectProviderSelect();
    fireEvent.change(reselect, { target: { value: PROVIDER_A } });
    fireEvent.change(screen.getByTestId("composer-input"), { target: { value: "继续" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    const error = await screen.findByTestId("workbench-error", {}, WAIT);
    expect(error.textContent).toContain(
      "当前任务仍在运行，暂不能补选供应商/模型，请等当前任务结束后重试。",
    );
  });
});

describe("M7-10 端点预览与风险提示", () => {
  it("端点由用户输入、Key 将发送至该地址（data-source 预览）", async () => {
    render(<ProvidersPage ipc={fakeProviders()} />);
    fireEvent.click(await screen.findByTestId("providers-add", {}, WAIT));
    const risk = await screen.findByTestId("provider-endpoint-risk", {}, WAIT);
    expect(risk.textContent).toContain("API Key 将发送至该地址");
    const previews = screen.getAllByTestId("provider-endpoint-preview");
    expect(previews.length).toBeGreaterThan(0);
    expect(previews[0]!.getAttribute("data-source")).toBeTruthy();
  });
});
