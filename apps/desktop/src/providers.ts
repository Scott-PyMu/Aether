/**
 * 模型与供应商配置 IPC 契约（M3-11；ADR-010 决策 3/4；UI-UX S-05/§2.4/§7.3）。
 *
 * - 命令与生成绑定一致（`providers_list` / `provider_create` / `provider_update` /
 *   `provider_delete` / `provider_toggle` / `provider_model_add` /
 *   `provider_model_toggle`；T14 生成物 `packages/protocol/src/bindings.ts`）；
 * - 密钥语义（D10）：`api_key` 明文仅经本地 IPC 传输，核心写 keyring（A3 降级走
 *   加密文件）后只返回/落库 `api_key_ref`；前端不缓存明文；
 * - 模型选择器派生（M3-11 前置登记，UI-UX §2.4/§9 Q2）：仅启用供应商的启用模型
 *   出现；与新建表单手动输入共存（同一草稿字段），运行期只读。
 */
import { invoke } from "@tauri-apps/api/core";

/** 供应商类型（ADR-010 决策 3：创建后不可改）。 */
export type ProviderType = "anthropic" | "openai" | "deepseek" | "google" | "custom";

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
  /** 明文仅传输（非空时核心写 keyring）；缺省/空串 = 不写入。 */
  api_key?: string;
  enabled: boolean;
}

export interface ProviderUpdateInput {
  id: string;
  name: string;
  /** 缺省 = 不变；空串 = 清除。 */
  base_url?: string;
  /** 缺省 = 不变；空串 = 清除；非空 = 覆盖明文写入。 */
  api_key?: string;
  enabled: boolean;
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
}

/** 生产实现（Tauri IPC；命令入参统一 `payload` 包装，与生成绑定一致）。 */
export const providersIpc: ProvidersIpc = {
  async list() {
    const result = await invoke<{ providers: ProviderEntry[] }>("providers_list");
    return result.providers;
  },
  async create(input) {
    return invoke<ProviderEntry>("provider_create", { payload: input });
  },
  async update(input) {
    return invoke<ProviderEntry>("provider_update", { payload: input });
  },
  async remove(id) {
    const result = await invoke<{ deleted: boolean }>("provider_delete", {
      payload: { id },
    });
    return result.deleted;
  },
  async toggle(id, enabled) {
    return invoke<{ id: string; enabled: boolean }>("provider_toggle", {
      payload: { id, enabled },
    });
  },
  async addModel(providerId, modelId, displayName) {
    return invoke<ProviderModel>("provider_model_add", {
      payload: { provider_id: providerId, model_id: modelId, display_name: displayName },
    });
  },
  async toggleModel(providerId, modelId, enabled) {
    return invoke<{ provider_id: string; model_id: string; enabled: boolean }>(
      "provider_model_toggle",
      { payload: { provider_id: providerId, model_id: modelId, enabled } },
    );
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
  return (
    providers.find((provider) =>
      provider.models.some((model) => model.model_id === modelId),
    ) ?? null
  );
}
