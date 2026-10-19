/**
 * DSH 按会话选模型（M7-06/ADR-024 DoD4；实测口径见任务证据）。
 *
 * 实测（DSH 0.1.5-rc.2）：`session/new|resume` 的 `configOptions` 含
 * `{ id: "model", category: "model", type: "select" }`，其选项 `value` 为
 * **`["<provider>","<model>"]` 的 JSON 字符串**（按 provider 分组）；
 * 经 `session/set_config_option { configId: "model", value }` 生效（返回刷新后的
 * configOptions）。本模块只做解析与选择，不做网络/进程操作。
 */

/** ACP 模型配置项 id（实测值；0.1.5-rc.2）。 */
export const DSH_MODEL_CONFIG_ID = "model";
/** `session/set_config_option` 超时（同 reasoning：会话级有状态设置）。 */
export const MODEL_SET_TIMEOUT_MS = 5_000;

/** 模型选项条目（已解析 `["provider","model"]`）。 */
export interface DshModelOptionEntry {
  provider: string;
  model: string;
  /** 原始选项 value（回传给 `session/set_config_option`）。 */
  value: string;
}

interface ConfigOptionShape {
  id?: unknown;
  options?: unknown;
}

interface ConfigGroupShape {
  group?: unknown;
  options?: unknown;
}

interface ConfigValueShape {
  value?: unknown;
}

/** 从 `session/new|resume` 的 `configOptions` 解析全部模型选项（保序；非法项忽略）。 */
export function modelOptionEntriesFrom(configOptions: unknown): DshModelOptionEntry[] {
  if (!Array.isArray(configOptions)) return [];
  const entries: DshModelOptionEntry[] = [];
  for (const raw of configOptions as ConfigOptionShape[]) {
    if (typeof raw !== "object" || raw === null) continue;
    if (raw.id !== DSH_MODEL_CONFIG_ID) continue;
    const groups = Array.isArray(raw.options) ? (raw.options as ConfigGroupShape[]) : [];
    for (const group of groups) {
      if (typeof group !== "object" || group === null) continue;
      const options = Array.isArray(group.options) ? (group.options as ConfigValueShape[]) : [];
      for (const option of options) {
        if (typeof option !== "object" || option === null) continue;
        const value = option.value;
        if (typeof value !== "string") continue;
        const parsed = parseModelValue(value);
        if (parsed === null) continue;
        entries.push({ provider: parsed[0], model: parsed[1], value });
      }
    }
  }
  return entries;
}

/** 解析 `["provider","model"]` JSON 字符串；非法 → `null`。 */
export function parseModelValue(value: string): [string, string] | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(value);
  } catch {
    return null;
  }
  if (!Array.isArray(parsed) || parsed.length !== 2) return null;
  const [provider, model] = parsed;
  if (typeof provider !== "string" || provider.length === 0) return null;
  if (typeof model !== "string" || model.length === 0) return null;
  return [provider, model];
}

/**
 * 选择会话模型的选项：优先 `preferredProvider`（消费映射注入的 provider）命中项，
 * 否则按 configOptions 中的出现顺序取首个 `model` 匹配项；无匹配 → `undefined`。
 */
export function resolveModelOption(
  entries: readonly DshModelOptionEntry[],
  model: string,
  preferredProvider?: string,
): DshModelOptionEntry | undefined {
  const matched = entries.filter((entry) => entry.model === model);
  if (matched.length === 0) return undefined;
  if (preferredProvider !== undefined) {
    const preferred = matched.find((entry) => entry.provider === preferredProvider);
    if (preferred !== undefined) return preferred;
  }
  return matched[0];
}

// ===== 供应商消费映射（M7-06/ADR-024 §2.1-1：核心注入 env → DSH profile 分层配置）=====

/** DSH provider 键前缀（保证 YAML 标量以字母开头；ULID 理论上可能全数字）。 */
export const DSH_PROVIDER_PREFIX = "aether-";

/** DSH provider 键（`llm-pi-ai.providers` 与 acp 行 `provider` 共用）。 */
export function dshProviderKey(providerId: string): string {
  return `${DSH_PROVIDER_PREFIX}${providerId}`;
}

/** 供应商消费映射（已解析 env；非密钥）。 */
export interface DshProviderFromEnv {
  /** provider 键（`dshProviderKey`）。 */
  provider: string;
  /** `llm-pi-ai` composition base 配置（`{ providers: {...} }`）。 */
  providerConfig: Record<string, unknown>;
  /**
   * acp 行缺省模型（`AETHER_PROVIDER_MODEL`；实测缺省模型缺失时 DSH 不注册该
   * provider 的模型选项，M7-06 证据）。无 → `undefined`（不写 model 字段）。
   */
  defaultModel?: string;
}

/**
 * 从核心注入的消费映射 env 构造 DSH provider 配置（未注入 → `null`）。
 *
 * env（任务证据登记）：`AETHER_PROVIDER_BASE_URL`（触发条件）/ `AETHER_PROVIDER_ID` /
 * `AETHER_PROVIDER_ENV_KEY`（`apiKeyEnv`）/ `AETHER_PROVIDER_MODELS`（JSON 数组，
 * `[{id,name}]`；上限 100 条，保序截断）/ `AETHER_PROVIDER_MODEL`（acp 行缺省模型）。
 * 密钥值不在本配置内（DSH 经 `apiKeyEnv` 读取进程环境，D10 不落盘）。
 */
export function providerFromEnv(
  env: Record<string, string | undefined>,
): DshProviderFromEnv | null {
  const baseUrl = env["AETHER_PROVIDER_BASE_URL"];
  if (typeof baseUrl !== "string" || baseUrl.length === 0) return null;
  const providerId = env["AETHER_PROVIDER_ID"] ?? "aether";
  const envKey = env["AETHER_PROVIDER_ENV_KEY"] ?? "DEEPSEEK_API_KEY";
  const models = parseEnvModels(env["AETHER_PROVIDER_MODELS"]);
  const provider = dshProviderKey(providerId);
  const providerConfig = {
    providers: {
      [provider]: {
        displayName: providerId,
        apiKeyEnv: envKey,
        api: "openai-completions",
        baseURL: baseUrl,
        models,
      },
    },
  };
  const defaultModel =
    typeof env["AETHER_PROVIDER_MODEL"] === "string" && env["AETHER_PROVIDER_MODEL"].length > 0
      ? env["AETHER_PROVIDER_MODEL"]
      : undefined;
  return {
    provider,
    providerConfig,
    ...(defaultModel !== undefined ? { defaultModel } : {}),
  };
}

/** 解析 `AETHER_PROVIDER_MODELS`（JSON 数组；非法/空 → `[]`；上限 100 条）。 */
function parseEnvModels(raw: string | undefined): Array<{ id: string; name: string }> {
  if (raw === undefined || raw.length === 0) return [];
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return [];
  }
  if (!Array.isArray(parsed)) return [];
  const models: Array<{ id: string; name: string }> = [];
  for (const item of parsed) {
    if (models.length >= 100) break;
    if (typeof item !== "object" || item === null) continue;
    const record = item as Record<string, unknown>;
    const id = record["id"];
    if (typeof id !== "string" || id.length === 0) continue;
    const name = typeof record["name"] === "string" && record["name"].length > 0 ? record["name"] : id;
    models.push({ id, name });
  }
  return models;
}
