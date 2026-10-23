/**
 * ADR-028 v0.9：`provider.catalog.get` 客户端（适配器侧，D6 只读方法）。
 *
 * - 仅经 `JsonRpcPeer` 发送 adapter → core 请求（5s 超时；不缓存、不落盘）；
 * - 响应只含非机密字段（id/name/type/enabled/endpoints/models）；
 * - 错误映射：`-32601` / `-32603` + `error.data.aether_code=core_not_ready`，
 *   `-32602` + `invalid_value`；传输超时/断连按 `core_not_ready` 处理。
 * - 目录查询失败 → 本次操作失败，不得静默使用 spawn 时快照。
 */
import { RpcError, type JsonRpcPeer } from "./rpc";

/** 目录模型条目（非机密）。 */
export interface ProviderCatalogModel {
  id: string;
  display_name: string;
  enabled: boolean;
}

/** 目录供应商条目（非机密；不含 key/api_key_ref/token）。 */
export interface ProviderCatalogEntry {
  id: string;
  name: string;
  type: string;
  enabled: boolean;
  endpoints: Record<string, string>;
  models: ProviderCatalogModel[];
}

export interface ProviderCatalog {
  providers: ProviderCatalogEntry[];
}

/** 适配器侧目录客户端（生产 = `fetchProviderCatalog(peer)`；测试可注入替身）。 */
export interface ProviderCatalogClient {
  fetch(): Promise<ProviderCatalog>;
}

/** 目录查询地图错误：统一为既定应用语义（不新增 Aether 错误码）。 */
export class CatalogError extends Error {
  constructor(
    readonly aetherCode: "core_not_ready" | "invalid_value",
    message: string,
  ) {
    super(message);
    this.name = "CatalogError";
  }
}

export const PROVIDER_CATALOG_METHOD = "provider.catalog.get";
export const PROVIDER_CATALOG_TIMEOUT_MS = 5_000;

function readAetherCode(data: unknown): "core_not_ready" | "invalid_value" | null {
  if (typeof data !== "object" || data === null) return null;
  const code = (data as Record<string, unknown>)["aether_code"];
  return code === "core_not_ready" || code === "invalid_value" ? code : null;
}

/** 查询目录（严格 `{}` 参数；失败抛 `CatalogError`）。 */
export async function fetchProviderCatalog(peer: JsonRpcPeer): Promise<ProviderCatalog> {
  let result: unknown;
  try {
    result = await peer.request(PROVIDER_CATALOG_METHOD, {}, PROVIDER_CATALOG_TIMEOUT_MS);
  } catch (error) {
    if (error instanceof RpcError) {
      const mapped = readAetherCode(error.data);
      if (error.code === -32602) {
        throw new CatalogError("invalid_value", `目录查询参数错误：${error.message}`);
      }
      throw new CatalogError(
        mapped ?? "core_not_ready",
        `目录查询失败（${error.code}）：${error.message}`,
      );
    }
    throw new CatalogError(
      "core_not_ready",
      `目录查询失败：${error instanceof Error ? error.message : String(error)}`,
    );
  }
  if (typeof result !== "object" || result === null) {
    throw new CatalogError("core_not_ready", "目录查询响应不是对象");
  }
  const providers = (result as Record<string, unknown>)["providers"];
  if (!Array.isArray(providers)) {
    throw new CatalogError("core_not_ready", "目录查询响应缺少 providers 数组");
  }
  return {
    providers: providers.map((raw): ProviderCatalogEntry => {
      const item = (raw ?? {}) as Record<string, unknown>;
      const models = Array.isArray(item["models"]) ? item["models"] : [];
      return {
        id: typeof item["id"] === "string" ? item["id"] : "",
        name: typeof item["name"] === "string" ? item["name"] : "",
        type: typeof item["type"] === "string" ? item["type"] : "",
        enabled: item["enabled"] === true,
        endpoints:
          typeof item["endpoints"] === "object" && item["endpoints"] !== null
            ? (item["endpoints"] as Record<string, string>)
            : {},
        models: models.map((model): ProviderCatalogModel => {
          const entry = (model ?? {}) as Record<string, unknown>;
          return {
            id: typeof entry["id"] === "string" ? entry["id"] : "",
            display_name:
              typeof entry["display_name"] === "string" ? entry["display_name"] : "",
            enabled: entry["enabled"] === true,
          };
        }),
      };
    }),
  };
}

/** 目录中查找启用 provider（未找到 → `undefined`）。 */
export function catalogProvider(
  catalog: ProviderCatalog,
  providerId: string,
): ProviderCatalogEntry | undefined {
  return catalog.providers.find((provider) => provider.id === providerId && provider.enabled);
}
