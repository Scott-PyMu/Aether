/**
 * 模型与供应商配置 IPC 契约（M3-11；ADR-010 决策 3/4；UI-UX S-05/§2.4/§7.3）。
 *
 * - 命令与 typed client 一致（`providers_list` / `provider_create` /
 *   `provider_update` / `provider_delete` / `provider_toggle` /
 *   `provider_model_add` / `provider_model_toggle`；`@aether/protocol` client）；
 * - 密钥语义（D10）：`api_key` 明文仅经本地 IPC 传输，核心写 keyring（A3 降级走
 *   加密文件）后只返回/落库 `api_key_ref`；前端不缓存明文；
 * - 模型选择器派生（M3-11 前置登记，UI-UX §2.4/§9 Q2）：仅启用供应商的启用模型
 *   出现；与新建表单手动输入共存（同一草稿字段），运行期只读。
 */
import { client } from "@aether/protocol";

/** 供应商类型（ADR-010 决策 3：创建后不可改）。 */
export type ProviderType = "anthropic" | "openai" | "deepseek" | "google" | "custom";

/**
 * ADR-028：主流供应商默认协议端点（用户只需填 Key；可覆盖）。
 * `openai` = OpenAI 兼容协议；`anthropic` = Anthropic 兼容协议；`google` = Gemini 原生。
 */
export const PROVIDER_ENDPOINT_PRESETS: Record<
  ProviderType,
  { endpoints: Record<string, string>; models: string[] }
> = {
  anthropic: {
    endpoints: { anthropic: "https://api.anthropic.com" },
    models: [],
  },
  openai: {
    endpoints: { openai: "https://api.openai.com/v1" },
    models: [],
  },
  deepseek: {
    endpoints: {
      openai: "https://api.deepseek.com",
      anthropic: "https://api.deepseek.com/anthropic",
    },
    models: ["deepseek-flash", "deepseek-v4-pro"],
  },
  google: {
    endpoints: {
      google: "https://generativelanguage.googleapis.com",
      openai: "https://generativelanguage.googleapis.com/v1beta/openai/",
    },
    models: [],
  },
  custom: {
    endpoints: {},
    models: [],
  },
};

/** 供应商模型（`provider_models` 表投影；启用态驱动选择器）。 */
export interface ProviderModel {
  id: string;
  model_id: string;
  display_name: string;
  enabled: boolean;
}

/** `providers_list` 元素（ADR-010 附录 B.1；`api_key_ref` 为引用，不含密钥本体）。 */
export interface ProviderEntry {
  id: string;
  name: string;
  type: ProviderType;
  base_url: string | null;
  /** ADR-028：协议端点（`anthropic`/`openai`/`google`/`dsh` → base URL）。 */
  endpoints?: Record<string, string> | null;
  api_key_ref: string | null;
  enabled: boolean;
  is_builtin: boolean;
  created_at: number;
  updated_at: number;
  models: ProviderModel[];
}

export interface ProviderCreateInput {
  name: string;
  type: ProviderType;
  base_url?: string;
  endpoints?: Record<string, string>;
  /** 明文仅传输（非空时核心写 keyring）；缺省/空串 = 不写入。 */
  api_key?: string;
  enabled: boolean;
}

export interface ProviderUpdateInput {
  id: string;
  name: string;
  /** 缺省 = 不变；空串 = 清除。 */
  base_url?: string;
  /** 缺省 = 不变；`{}` = 清除；非空 = 整体替换。 */
  endpoints?: Record<string, string>;
  /** 缺省 = 不变；空串 = 清除；非空 = 覆盖明文写入。 */
  api_key?: string;
  enabled: boolean;
}

/** 连通性测试回执（M7-06/ADR-024：成功回执含延迟与模型数）。 */
export interface ProviderTestResult {
  provider_id: string;
  ok: boolean;
  status: number;
  latency_ms: number;
  models_count: number;
}

/** 模型获取回执（M7-06/ADR-024：归一化 + 幂等 upsert）。 */
export interface ProviderModelsFetchResult {
  provider_id: string;
  ok: boolean;
  status: number;
  latency_ms: number;
  fetched: number;
  added: number;
  refreshed: number;
  models: string[];
}

export interface ProvidersIpc {
  list(): Promise<ProviderEntry[]>;
  create(input: ProviderCreateInput): Promise<ProviderEntry>;
  update(input: ProviderUpdateInput): Promise<ProviderEntry>;
  remove(id: string): Promise<boolean>;
  toggle(id: string, enabled: boolean): Promise<{ id: string; enabled: boolean }>;
  addModel(providerId: string, modelId: string, displayName: string): Promise<ProviderModel>;
  toggleModel(
    providerId: string,
    modelId: string,
    enabled: boolean,
  ): Promise<{ provider_id: string; model_id: string; enabled: boolean }>;
  /** M7-06/ADR-024：真实连通性测试（网络请求；五路径错误码）。 */
  test(providerId: string): Promise<ProviderTestResult>;
  /** M7-06/ADR-024：从供应商获取模型（归一化 + 幂等 upsert）。 */
  fetchModels(providerId: string): Promise<ProviderModelsFetchResult>;
}

/** 生产实现（typed client；命令入参为内层 DTO）。 */
export const providersIpc: ProvidersIpc = {
  async list() {
    const result = await client.providersList<{ providers: ProviderEntry[] }>();
    return result.providers;
  },
  async create(input) {
    return client.providerCreate<ProviderEntry>(input);
  },
  async update(input) {
    return client.providerUpdate<ProviderEntry>(input);
  },
  async remove(id) {
    const result = await client.providerDelete<{ deleted: boolean }>({ id });
    return result.deleted;
  },
  async toggle(id, enabled) {
    return client.providerToggle<{ id: string; enabled: boolean }>({ id, enabled });
  },
  async addModel(providerId, modelId, displayName) {
    return client.providerModelAdd<ProviderModel>({
      provider_id: providerId,
      model_id: modelId,
      display_name: displayName,
    });
  },
  async toggleModel(providerId, modelId, enabled) {
    return client.providerModelToggle<{
      provider_id: string;
      model_id: string;
      enabled: boolean;
    }>({ provider_id: providerId, model_id: modelId, enabled });
  },
  async test(providerId) {
    return client.providerTest<ProviderTestResult>({ provider_id: providerId });
  },
  async fetchModels(providerId) {
    return client.providerModelsFetch<ProviderModelsFetchResult>({
      provider_id: providerId,
    });
  },
};

/** 官方预设（UI 侧常量，原型 `officialProviders`；不落库，ADR-010 决策 3）。 */
export interface OfficialProviderPreset {
  name: string;
  baseUrl: string;
  preview: string;
  color: string;
  abbr: string;
}

export const OFFICIAL_PROVIDER_PRESETS: Record<
  Exclude<ProviderType, "custom">,
  OfficialProviderPreset
> = {
  anthropic: {
    name: "Anthropic",
    baseUrl: "https://api.anthropic.com",
    preview: "https://api.anthropic.com/v1/messages",
    color: "#D97757",
    abbr: "A",
  },
  openai: {
    name: "OpenAI",
    baseUrl: "https://api.openai.com",
    preview: "https://api.openai.com/v1/chat/completions",
    color: "#10A37F",
    abbr: "O",
  },
  deepseek: {
    name: "DeepSeek",
    baseUrl: "https://api.deepseek.com",
    preview: "https://api.deepseek.com/v1/chat/completions",
    color: "#4D6BFE",
    abbr: "D",
  },
  google: {
    name: "Google Gemini",
    baseUrl: "https://generativelanguage.googleapis.com",
    preview: "https://generativelanguage.googleapis.com/v1beta/models",
    color: "#4285F4",
    abbr: "G",
  },
};

/** 官方预设（自定义/未知类型为 `null`）。 */
export function presetFor(type: ProviderType): OfficialProviderPreset | null {
  if (type === "custom") {
    return null;
  }
  return OFFICIAL_PROVIDER_PRESETS[type] ?? null;
}

/** 卡片缩写（原型语义：官方用预设缩写，自定义取名称首字符）。 */
export function providerAbbr(provider: Pick<ProviderEntry, "name" | "type">): string {
  const preset = presetFor(provider.type);
  if (preset) {
    return preset.abbr;
  }
  const first = provider.name.trim().charAt(0);
  return first ? first.toUpperCase() : "?";
}

/** 卡片颜色（UI 侧常量；自定义为中性灰）。 */
export function providerColor(type: ProviderType): string {
  return presetFor(type)?.color ?? "#666666";
}

/** 供应商描述：`{名称} · N 个模型已启用`（原型 providerDesc 口径）。 */
export function providerDesc(provider: ProviderEntry): string {
  const enabledCount = provider.models.filter((model) => model.enabled).length;
  return enabledCount > 0
    ? `${provider.name} · ${enabledCount} 个模型已启用`
    : `${provider.name} · 暂无已启用模型`;
}

/** 模型选择器分组（仅启用供应商的启用模型；ADR-010 决策 3 派生口径）。 */
export interface ModelGroup {
  id: string;
  name: string;
  models: ProviderModel[];
}

export function deriveModelGroups(providers: ProviderEntry[]): ModelGroup[] {
  return providers
    .filter((provider) => provider.enabled)
    .map((provider) => ({
      id: provider.id,
      name: provider.name,
      models: provider.models.filter((model) => model.enabled),
    }))
    .filter((group) => group.models.length > 0);
}

/**
 * 选中失效回退（UI-UX §2.4 M3-11 前置登记）：
 * - 此前未选择（`selection === null`）→ 保持空选择（不自动为新建会话预选模型）；
 * - 已选择但已失效 → 回退到首个可用模型；无可用模型 → 清空。
 */
export function ensureSelectedModelValid(
  selection: { providerId: string; modelId: string } | null,
  groups: ModelGroup[],
): { providerId: string; modelId: string } | null {
  if (selection === null) {
    return null;
  }
  const stillValid = groups.some(
    (group) =>
      group.id === selection.providerId &&
      group.models.some((model) => model.model_id === selection.modelId),
  );
  if (stillValid) {
    return selection;
  }
  const first = groups[0]?.models[0];
  if (!first || !groups[0]) {
    return null;
  }
  return { providerId: groups[0].id, modelId: first.model_id };
}

/** 按模型 ID 反查供应商（选择器回显用）。 */
export function findModelProvider(
  providers: ProviderEntry[],
  modelId: string,
): ProviderEntry | null {
  // ADR-028：同一模型可能出现在多个供应商；展示取启用供应商中
  // `(updated_at, id)` 最大者（确定性）。实际新建会话由用户点击的 provider 显式透传。
  const candidates = providers
    .filter(
      (provider) =>
        provider.enabled &&
        provider.models.some((model) => model.model_id === modelId && model.enabled),
    )
    .sort((left, right) => {
      if (left.updated_at !== right.updated_at) {
        return right.updated_at - left.updated_at;
      }
      return right.id.localeCompare(left.id);
    });
  return candidates[0] ?? null;
}

// ===== ADR-028 v0.9：三官方运行时供应商选择 / 端点 / 记忆 =====

/** 核心静态 `multi_provider` 三官方运行时（与后端 `MULTI_PROVIDER_RUNTIMES` 对齐）。 */
export const MULTI_PROVIDER_RUNTIMES = ["claude-code", "codex", "deepseek-harness"] as const;

/** 运行时 → 协议（非官方运行时 → `null`）。 */
export function runtimeProtocol(runtimeId: string | null | undefined): string | null {
  switch (runtimeId) {
    case "claude-code":
      return "anthropic";
    case "codex":
      return "openai";
    case "deepseek-harness":
      return "dsh";
    default:
      return null;
  }
}

/** 运行时是否受 M7-10 必选供应商/模型约束（静态清单，不依赖 hello/ready）。 */
export function requiresProviderSelection(runtimeId: string | null | undefined): boolean {
  return runtimeProtocol(runtimeId) !== null;
}

/**
 * 供应商是否支持该运行时协议（显式端点优先；类型回退与核心解析一致：
 * Anthropic/DeepSeek 支持 Anthropic；OpenAI/DeepSeek 支持 OpenAI；DeepSeek 支持 DSH；
 * custom 可用显式端点/base_url）。
 */
export function providerSupportsRuntime(
  provider: ProviderEntry,
  runtimeId: string | null | undefined,
): boolean {
  const protocol = runtimeProtocol(runtimeId);
  if (protocol === null) {
    return false;
  }
  if (provider.endpoints?.[protocol]) {
    return true;
  }
  if (provider.type === "custom") {
    return Boolean(provider.base_url);
  }
  switch (protocol) {
    case "anthropic":
      return provider.type === "anthropic" || provider.type === "deepseek";
    case "openai":
      return provider.type === "openai" || provider.type === "deepseek";
    case "dsh":
      return provider.type === "deepseek";
    default:
      return false;
  }
}

/** 运行时可用供应商（启用 + 协议兼容）；空 = 无有效选择。 */
export function providersForRuntime(
  providers: ProviderEntry[],
  runtimeId: string | null | undefined,
): ProviderEntry[] {
  const enabled = providers.filter((provider) => provider.enabled);
  if (!runtimeId) {
    // 自动路由：运行时未定；展示全部启用供应商（最终由核心按候选运行时协议校验）。
    return enabled;
  }
  return enabled.filter((provider) => providerSupportsRuntime(provider, runtimeId));
}

/** 全局记忆映射（`ui.provider_selection`：runtime → provider_id）。 */
export type ProviderSelectionMemory = Record<string, string>;

/** 解析设置值（字符串 JSON 对象；损坏/非对象 → `{}`）。 */
export function parseProviderSelectionMemory(raw: unknown): ProviderSelectionMemory {
  if (typeof raw !== "string" || raw.trim().length === 0) {
    return {};
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
      return {};
    }
    const memory: ProviderSelectionMemory = {};
    for (const [runtime, provider] of Object.entries(parsed)) {
      if (typeof provider === "string" && provider.length > 0) {
        memory[runtime] = provider;
      }
    }
    return memory;
  } catch {
    return {};
  }
}

/** 记忆值 + 目录 → 预填选择（失效记忆不自动回退；返回 `null` 表示需用户重选）。 */
export function rememberedProvider(
  providers: ProviderEntry[],
  runtimeId: string | null | undefined,
  memory: ProviderSelectionMemory,
): ProviderEntry | null {
  if (!runtimeId) return null;
  const remembered = memory[runtimeId];
  if (!remembered) return null;
  return (
    providersForRuntime(providers, runtimeId).find((provider) => provider.id === remembered) ??
    null
  );
}

/** 首个可用模型（供应商启用模型按列表顺序）。 */
export function firstEnabledModel(provider: ProviderEntry | null): ProviderModel | null {
  return provider?.models.find((model) => model.enabled) ?? null;
}

/** 协议端点预览（`data-source` = explicit|base_url|default）。 */
export function endpointPreview(
  provider: Pick<ProviderEntry, "type" | "base_url" | "endpoints">,
  protocol: string,
): { url: string; source: "explicit" | "base_url" | "default" } | null {
  const explicit = provider.endpoints?.[protocol];
  if (typeof explicit === "string" && explicit.trim().length > 0) {
    return { url: explicit.trim(), source: "explicit" };
  }
  const base = provider.base_url?.trim().replace(/\/+$/, "");
  if (base) {
    if (protocol === "anthropic" && provider.type === "deepseek" && !base.endsWith("/anthropic")) {
      return { url: `${base}/anthropic`, source: "base_url" };
    }
    if (protocol === "openai") {
      if (provider.type === "openai" && base === "https://api.openai.com") {
        return { url: `${base}/v1`, source: "base_url" };
      }
      if (provider.type === "custom" || provider.type === "openai" || provider.type === "deepseek") {
        return { url: base, source: "base_url" };
      }
    }
    if (protocol === "dsh" && provider.type === "deepseek") {
      return { url: base, source: "base_url" };
    }
  }
  switch (`${protocol}:${provider.type}`) {
    case "anthropic:anthropic":
      return { url: "https://api.anthropic.com", source: "default" };
    case "anthropic:deepseek":
      return { url: "https://api.deepseek.com/anthropic", source: "default" };
    case "openai:openai":
      return { url: "https://api.openai.com/v1", source: "default" };
    case "openai:deepseek":
    case "dsh:deepseek":
      return { url: "https://api.deepseek.com", source: "default" };
    default:
      return null;
  }
}

/** 端点风险提示（ADR-028 v0.9：不做 HTTPS/主机/可信保证，UI 必须提示）。 */
export const PROVIDER_ENDPOINT_RISK_NOTICE =
  "端点由用户填写，API Key 将发送至该地址；应用不做 HTTPS 强制、主机白名单或可信保证，请自行确认端点可信性。";
