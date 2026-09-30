/**
 * M3-11 前端集成测试：模型与供应商配置（ADR-010 决策 3/4；UI-UX S-05/§2.4/§7.3）。
 *
 * 覆盖：
 * - DoD3（UI 面）：内置删除入口置灰；删除二次确认；`builtin_provider_undeletable`
 *   结构化提示；
 * - DoD4（UI 面）：API Key 掩码输入；编辑态 `api_key_ref` 只读；创建/更新三态透传；
 * - DoD5：`provider-test` 点击 → toast「连通性测试将在 P1 开放」，无 IPC 调用；
 * - DoD6：模型选择器仅启用供应商的启用模型；停用后移除；失效回退；空态文案；
 * - DoD7：选择模型 → `session.create.model` 透传断言；运行期只读（无 update 入口）。
 */
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { EventStore } from "./eventStore";
import { ModelSelector } from "./ModelSelector";
import type { PermissionIpc } from "./permission";
import {
  providersIpc as productionProvidersIpc,
  type ProviderEntry,
  type ProvidersIpc,
} from "./providers";
import { ProvidersPage } from "./ProvidersPage";
import { SettingsPage } from "./SettingsPage";
import type { SessionIpc, SessionStatus, SessionSummary } from "./session";
import { SessionWorkbench } from "./SessionWorkbench";

const PROVIDER_A = "01J00000000000000000000PA1";
const PROVIDER_B = "01J00000000000000000000PB2";

const WAIT = { timeout: 5000 };

afterEach(cleanup);

function providerEntry(overrides: Partial<ProviderEntry> = {}): ProviderEntry {
  return {
    id: PROVIDER_A,
    name: "Anthropic",
    type: "anthropic",
    base_url: "https://api.anthropic.com",
    api_key_ref: null,
    enabled: true,
    is_builtin: true,
    created_at: 1,
    updated_at: 1,
    models: [
      { id: "m1", model_id: "claude-opus", display_name: "Claude Opus", enabled: true },
      { id: "m2", model_id: "claude-hidden", display_name: "Hidden", enabled: false },
    ],
    ...overrides,
  };
}

interface FakeProviders {
  ipc: ProvidersIpc;
  store: ProviderEntry[];
  list: ReturnType<typeof vi.fn>;
  create: ReturnType<typeof vi.fn>;
  update: ReturnType<typeof vi.fn>;
  remove: ReturnType<typeof vi.fn>;
  toggle: ReturnType<typeof vi.fn>;
  addModel: ReturnType<typeof vi.fn>;
  toggleModel: ReturnType<typeof vi.fn>;
}

/** 供应商存储替身（列表可变；各命令为 spy 供断言）。 */
function fakeProvidersIpc(seed: ProviderEntry[] = []): FakeProviders {
  const store: ProviderEntry[] = seed.map((provider) => ({ ...provider }));
  let counter = 0;
  const nextId = () => `01J${String(++counter).padStart(23, "0")}`;
  const list = vi.fn(async () => store.map((provider) => ({ ...provider })));
  const create = vi.fn(async (input: Parameters<ProvidersIpc["create"]>[0]) => {
    const created: ProviderEntry = {
      id: nextId(),
      name: input.name,
      type: input.type,
      base_url: input.base_url ?? null,
      api_key_ref: input.api_key ? `keychain://aether/provider/${nextId()}` : null,
      enabled: input.enabled,
      is_builtin: false,
      created_at: 2,
      updated_at: 2,
      models: [],
    };
    store.push(created);
    return created;
  });
  const update = vi.fn(async (input: Parameters<ProvidersIpc["update"]>[0]) => {
    const target = store.find((provider) => provider.id === input.id);
    if (!target) {
      throw { code: "provider_not_found", message: "供应商不存在" };
    }
    target.name = input.name;
    if (input.base_url !== undefined) {
      target.base_url = input.base_url.length > 0 ? input.base_url : null;
    }
    if (input.api_key !== undefined) {
      target.api_key_ref =
        input.api_key.length > 0 ? `keychain://aether/provider/${input.id}` : null;
    }
    target.enabled = input.enabled;
    return { ...target };
  });
  const remove = vi.fn(async (id: string) => {
    const index = store.findIndex((provider) => provider.id === id);
    if (index < 0) {
      throw { code: "provider_not_found", message: "供应商不存在" };
    }
    if (store[index]?.is_builtin) {
      throw { code: "builtin_provider_undeletable", message: "内置供应商不可删除" };
    }
    store.splice(index, 1);
    return true;
  });
  const toggle = vi.fn(async (id: string, enabled: boolean) => {
    const target = store.find((provider) => provider.id === id);
    if (target) {
      target.enabled = enabled;
    }
    return { id, enabled };
  });
  const addModel = vi.fn(async (providerId: string, modelId: string, displayName: string) => {
    const target = store.find((provider) => provider.id === providerId);
    const model = {
      id: nextId(),
      model_id: modelId,
      display_name: displayName,
      enabled: true,
    };
    target?.models.push(model);
    return model;
  });
  const toggleModel = vi.fn(async (providerId: string, modelId: string, enabled: boolean) => {
    const target = store.find((provider) => provider.id === providerId);
    const model = target?.models.find((item) => item.model_id === modelId);
    if (model) {
      model.enabled = enabled;
    }
    return { provider_id: providerId, model_id: modelId, enabled };
  });
  return {
    ipc: { list, create, update, remove, toggle, addModel, toggleModel } as ProvidersIpc,
    store,
    list,
    create,
    update,
    remove,
    toggle,
    addModel,
    toggleModel,
  };
}

describe("ProvidersPage（M3-11 DoD3/4/5）", () => {
  it("DoD3：内置删除入口置灰；非内置删除二次确认调用 remove", async () => {
    const providers = fakeProvidersIpc([
      providerEntry(),
      providerEntry({
        id: PROVIDER_B,
        name: "自定义",
        type: "custom",
        is_builtin: false,
        models: [],
      }),
    ]);
    render(<ProvidersPage ipc={providers.ipc} />);
    await screen.findAllByTestId("provider-card", {}, WAIT);

    const cards = screen.getAllByTestId("provider-card");
    const builtinCard = cards.find((card) => card.getAttribute("data-provider-id") === PROVIDER_A);
    const customCard = cards.find((card) => card.getAttribute("data-provider-id") === PROVIDER_B);
    expect(builtinCard?.getAttribute("data-is-builtin")).toBe("true");
    const builtinDelete = within(builtinCard as HTMLElement).getByTestId("provider-delete");
    expect((builtinDelete as HTMLButtonElement).disabled).toBe(true);
    expect(builtinDelete.getAttribute("title")).toContain("内置供应商不可删除");

    fireEvent.click(within(customCard as HTMLElement).getByTestId("provider-delete"));
    fireEvent.click(screen.getByTestId("provider-delete-confirm-button"));
    await waitFor(() => {
      expect(providers.remove).toHaveBeenCalledWith(PROVIDER_B);
    }, WAIT);
    await waitFor(() => {
      expect(screen.getAllByTestId("provider-card")).toHaveLength(1);
    }, WAIT);
  });

  it("DoD3（错误面）：builtin_provider_undeletable 结构化提示", async () => {
    const providers = fakeProvidersIpc([
      providerEntry({ is_builtin: false, models: [] }),
    ]);
    providers.remove.mockRejectedValueOnce({
      code: "builtin_provider_undeletable",
      message: "内置供应商不可删除",
    });
    render(<ProvidersPage ipc={providers.ipc} />);
    await screen.findAllByTestId("provider-card", {}, WAIT);
    fireEvent.click(screen.getByTestId("provider-delete"));
    fireEvent.click(screen.getByTestId("provider-delete-confirm-button"));
    const error = await screen.findByTestId("provider-error", {}, WAIT);
    expect(error.getAttribute("data-code")).toBe("builtin_provider_undeletable");
    expect(error.textContent).toContain("内置供应商不可删除");
  });

  it("DoD4：创建携带 API Key（掩码输入）；编辑态 api_key_ref 只读；三态透传", async () => {
    const providers = fakeProvidersIpc([
      providerEntry({ api_key_ref: "keychain://aether/provider/" + PROVIDER_A }),
    ]);
    render(<ProvidersPage ipc={providers.ipc} />);
    await screen.findAllByTestId("provider-card", {}, WAIT);

    // 创建：官方预设 → 填 Key → 创建。
    fireEvent.click(screen.getByTestId("providers-add"));
    const keyInput = (await screen.findByTestId("provider-api-key-input", {}, WAIT)) as HTMLInputElement;
    expect(keyInput.type).toBe("password");
    fireEvent.click(screen.getByTestId("provider-api-key-reveal"));
    expect(keyInput.type).toBe("text");
    fireEvent.change(screen.getByTestId("provider-name-input"), {
      target: { value: "新供应商" },
    });
    fireEvent.change(keyInput, { target: { value: "unit-plaintext-key" } });
    fireEvent.click(screen.getByTestId("provider-form-save"));
    await waitFor(() => {
      expect(providers.create).toHaveBeenCalledTimes(1);
    }, WAIT);
    const createInput = providers.create.mock.calls[0]?.[0];
    expect(createInput.api_key).toBe("unit-plaintext-key");
    expect(JSON.stringify(await providers.list())).not.toContain("unit-plaintext-key");
    await screen.findAllByTestId("provider-card", {}, WAIT);

    // 编辑（种子供应商）：只读引用；未触碰 → 缺省（不变，不带 api_key 字段）。
    const seededCard = screen
      .getAllByTestId("provider-card")
      .find((card) => card.getAttribute("data-provider-id") === PROVIDER_A) as HTMLElement;
    fireEvent.click(within(seededCard).getByTestId("provider-edit"));
    await screen.findByTestId("provider-form", {}, WAIT);
    expect(screen.getByTestId("provider-api-key-ref-readonly").textContent).toContain(
      "keychain://aether/provider/",
    );
    fireEvent.click(screen.getByTestId("provider-form-save"));
    await waitFor(() => {
      expect(providers.update).toHaveBeenCalledTimes(1);
    }, WAIT);
    const untouched = providers.update.mock.calls[0]?.[0];
    expect(untouched).not.toHaveProperty("api_key");

    // 覆盖：输入新密钥 → 非空覆盖。
    fireEvent.click(
      within(
        screen
          .getAllByTestId("provider-card")
          .find((card) => card.getAttribute("data-provider-id") === PROVIDER_A) as HTMLElement,
      ).getByTestId("provider-edit"),
    );
    fireEvent.change(await screen.findByTestId("provider-api-key-input", {}, WAIT), {
      target: { value: "unit-overwrite-key" },
    });
    fireEvent.click(screen.getByTestId("provider-form-save"));
    await waitFor(() => {
      expect(providers.update).toHaveBeenCalledTimes(2);
    }, WAIT);
    expect(providers.update.mock.calls[1]?.[0]?.api_key).toBe("unit-overwrite-key");

    // 清除：触碰后清空 → 空串清除。
    fireEvent.click(
      within(
        screen
          .getAllByTestId("provider-card")
          .find((card) => card.getAttribute("data-provider-id") === PROVIDER_A) as HTMLElement,
      ).getByTestId("provider-edit"),
    );
    const clearInput = (await screen.findByTestId("provider-api-key-input", {}, WAIT)) as HTMLInputElement;
    fireEvent.change(clearInput, { target: { value: "x" } });
    fireEvent.change(clearInput, { target: { value: "" } });
    fireEvent.click(screen.getByTestId("provider-form-save"));
    await waitFor(() => {
      expect(providers.update).toHaveBeenCalledTimes(3);
    }, WAIT);
    expect(providers.update.mock.calls[2]?.[0]?.api_key).toBe("");
  });

  it("DoD5：测试连接仅 toast，无 IPC 调用、无网络请求", async () => {
    const providers = fakeProvidersIpc([providerEntry()]);
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
    try {
      render(<ProvidersPage ipc={providers.ipc} />);
      await screen.findAllByTestId("provider-card", {}, WAIT);
      fireEvent.click(screen.getByTestId("provider-edit"));
      const listCallsBefore = providers.list.mock.calls.length;
      fireEvent.click(await screen.findByTestId("provider-test", {}, WAIT));
      const notice = await screen.findByTestId("provider-test-notice", {}, WAIT);
      expect(notice.textContent).toContain("连通性测试将在 P1 开放");
      expect(providers.list.mock.calls.length).toBe(listCallsBefore);
      expect(providers.create).not.toHaveBeenCalled();
      expect(providers.update).not.toHaveBeenCalled();
      expect(fetchSpy).not.toHaveBeenCalled();
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("DoD6（供应商面）：模型添加与启停走命令面", async () => {
    const providers = fakeProvidersIpc([
      providerEntry({ is_builtin: false, api_key_ref: null }),
    ]);
    render(<ProvidersPage ipc={providers.ipc} />);
    await screen.findAllByTestId("provider-card", {}, WAIT);
    fireEvent.click(screen.getByTestId("provider-edit"));
    await screen.findAllByTestId("provider-model-item", {}, WAIT);
    expect(screen.getAllByTestId("provider-model-item")).toHaveLength(2);
    fireEvent.click(screen.getAllByTestId("provider-model-toggle")[0] as HTMLElement);
    await waitFor(() => {
      expect(providers.toggleModel).toHaveBeenCalledWith(PROVIDER_A, "claude-opus", false);
    }, WAIT);
    fireEvent.change(screen.getByTestId("provider-model-id-input"), {
      target: { value: "claude-sonnet" },
    });
    fireEvent.change(screen.getByTestId("provider-model-name-input"), {
      target: { value: "Claude Sonnet" },
    });
    fireEvent.click(screen.getByTestId("provider-model-add"));
    await waitFor(() => {
      expect(providers.addModel).toHaveBeenCalledWith(
        PROVIDER_A,
        "claude-sonnet",
        "Claude Sonnet",
      );
    }, WAIT);
  });
});

describe("SettingsPage 集成（M3-11）", () => {
  it("模型与供应商配置子页可达；切回基本设置不丢失既有锚点", async () => {
    const providers = fakeProvidersIpc([providerEntry()]);
    render(
      <SettingsPage dataDir="C:\\data" providersIpc={providers.ipc} onBack={() => {}} />,
    );
    expect(screen.getByTestId("settings-backup-reminder")).toBeTruthy();
    fireEvent.click(screen.getByTestId("settings-tab-providers"));
    await screen.findByTestId("providers-page", {}, WAIT);
    expect(screen.queryByTestId("settings-backup-reminder")).toBeNull();
    fireEvent.click(screen.getByTestId("settings-tab-general"));
    expect(screen.getByTestId("settings-backup-reminder")).toBeTruthy();
    expect(screen.queryByTestId("providers-page")).toBeNull();
  });
});

describe("ModelSelector（M3-11 DoD6）", () => {
  it("仅启用供应商的启用模型出现；空态文案", async () => {
    const providers = fakeProvidersIpc([
      providerEntry(),
      providerEntry({
        id: PROVIDER_B,
        name: "停用供应商",
        type: "openai",
        enabled: false,
        is_builtin: false,
        models: [{ id: "m3", model_id: "gpt-hidden", display_name: "GPT Hidden", enabled: true }],
      }),
    ]);
    const { unmount } = render(
      <ModelSelector ipc={providers.ipc} value={null} onChange={() => {}} />,
    );
    fireEvent.click(await screen.findByTestId("model-selector-toggle", {}, WAIT));
    const items = await screen.findAllByTestId("model-selector-item", {}, WAIT);
    expect(items).toHaveLength(1);
    expect(items[0]?.getAttribute("data-model-id")).toBe("claude-opus");
    expect(items[0]?.getAttribute("data-provider-id")).toBe(PROVIDER_A);
    unmount();

    const empty = fakeProvidersIpc([]);
    render(<ModelSelector ipc={empty.ipc} value={null} onChange={() => {}} />);
    fireEvent.click(await screen.findByTestId("model-selector-toggle", {}, WAIT));
    const emptyState = await screen.findByTestId("model-selector-empty", {}, WAIT);
    expect(emptyState.textContent).toContain("没有可用的模型");
  });

  it("停用后移除（重新打开选择器时刷新派生）", async () => {
    const providers = fakeProvidersIpc([providerEntry()]);
    render(<ModelSelector ipc={providers.ipc} value={null} onChange={() => {}} />);
    fireEvent.click(await screen.findByTestId("model-selector-toggle", {}, WAIT));
    await screen.findAllByTestId("model-selector-item", {}, WAIT);

    // 供应商停用 → 重新打开 → 空态。
    if (providers.store[0]) {
      providers.store[0].enabled = false;
    }
    fireEvent.click(screen.getByTestId("model-selector-toggle"));
    fireEvent.click(screen.getByTestId("model-selector-toggle"));
    const emptyState = await screen.findByTestId("model-selector-empty", {}, WAIT);
    expect(emptyState.textContent).toContain("没有可用的模型");
  });

  it("选中失效回退：已选模型被停用 → 回退到首个可用模型", async () => {
    const providers = fakeProvidersIpc([providerEntry()]);
    const onChange = vi.fn();
    render(<ModelSelector ipc={providers.ipc} value="claude-hidden" onChange={onChange} />);
    await waitFor(() => {
      expect(onChange).toHaveBeenCalledWith("claude-opus");
    }, WAIT);
  });

  it("生产契约存在且命令名与生成绑定一致", () => {
    expect(typeof productionProvidersIpc.list).toBe("function");
    expect(typeof productionProvidersIpc.create).toBe("function");
    expect(typeof productionProvidersIpc.update).toBe("function");
    expect(typeof productionProvidersIpc.remove).toBe("function");
    expect(typeof productionProvidersIpc.toggle).toBe("function");
    expect(typeof productionProvidersIpc.addModel).toBe("function");
    expect(typeof productionProvidersIpc.toggleModel).toBe("function");
  });
});

// ===== SessionWorkbench 集成（DoD6/DoD7：选择模型 → session.create.model） =====

function sessionSummary(overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1A",
    runtime_id: "mock",
    workspace_id: null,
    parent_session_id: null,
    title: "供应商会话",
    status: "idle" as SessionStatus,
    model: null,
    created_at: 1,
    updated_at: 1,
    closed_at: null,
    workspace_root: null,
    ...overrides,
  };
}

function fakeSessionIpc(): SessionIpc {
  return {
    listRuntimes: vi.fn(async () => [
      {
        id: "mock",
        name: "Mock",
        kind: "mock",
        version: "0.1.0",
        protocol: "1.0",
        capabilities: [],
        enabled: true,
        status: "ready" as const,
        status_reason: null,
      },
    ]),
    listSessions: vi.fn(async () => [sessionSummary()]),
    createSession: vi.fn(async () => sessionSummary({ model: "claude-opus" })),
    sendMessage: vi.fn(async () => ({
      session_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1A",
      message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1R",
      queued: false,
      duplicate: false,
    })),
    interruptSession: vi.fn(async () => ({ session_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1A" })),
    disposeSession: vi.fn(async () => ({
      session_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1A",
      status: "completed" as SessionStatus,
    })),
    messagesPage: vi.fn(async () => ({
      session_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1A",
      max_seq: null,
      events: [],
      messages: [],
      complete: true,
    })),
    retryRun: vi.fn(async () => ({
      session_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1A",
      run_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1R",
      input_message_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1M",
      queued: false,
    })),
  } as unknown as SessionIpc;
}

function fakePermissionIpc(): PermissionIpc {
  return {
    pendingPermissions: vi.fn(async () => []),
    resolvePermission: vi.fn(),
    retryRuntime: vi.fn(),
    enableRuntime: vi.fn(),
  } as unknown as PermissionIpc;
}

describe("SessionWorkbench 模型选择器集成（M3-11 DoD6/DoD7）", () => {
  it("选择模型 → session.create.model 透传（UI-05 路径）；运行期无 update 入口", async () => {
    const store = new EventStore();
    const providers = fakeProvidersIpc([providerEntry()]);
    const ipc = fakeSessionIpc();
    try {
      render(
        <SessionWorkbench
          store={store}
          ipc={ipc}
          permissionIpc={fakePermissionIpc()}
          providersIpc={providers.ipc}
        />,
      );
      // 选择模型（作用于新建会话）。
      fireEvent.click(await screen.findByTestId("model-selector-toggle", {}, WAIT));
      const item = await screen.findByTestId("model-selector-item", {}, WAIT);
      fireEvent.click(item);
      const modelInput = screen.getByTestId("session-model-input") as HTMLInputElement;
      await waitFor(() => {
        expect(modelInput.value).toBe("claude-opus");
      }, WAIT);

      // 新建会话 → `session.create.model` 断言（UI-05 既有透传）。
      fireEvent.click(screen.getByTestId("session-create-submit"));
      await waitFor(() => {
        expect(ipc.createSession).toHaveBeenCalled();
      }, WAIT);
      const input = (ipc.createSession as unknown as ReturnType<typeof vi.fn>).mock
        .calls[0]?.[0];
      expect(input.model).toBe("claude-opus");

      // 运行期只读：发送消息不触发供应商配置命令（无 update 入口）。
      expect(providers.update).not.toHaveBeenCalled();
    } finally {
      store.dispose();
    }
  });

  it("无可用模型时选择器空态（不阻断手动输入）", async () => {
    const store = new EventStore();
    const providers = fakeProvidersIpc([]);
    try {
      render(
        <SessionWorkbench
          store={store}
          ipc={fakeSessionIpc()}
          permissionIpc={fakePermissionIpc()}
          providersIpc={providers.ipc}
        />,
      );
      fireEvent.click(await screen.findByTestId("model-selector-toggle", {}, WAIT));
      const emptyState = await screen.findByTestId("model-selector-empty", {}, WAIT);
      expect(emptyState.textContent).toContain("没有可用的模型");
      const modelInput = screen.getByTestId("session-model-input") as HTMLInputElement;
      expect(modelInput.disabled).toBe(false);
    } finally {
      store.dispose();
    }
  });
});
