/**
 * ADR-028：本地凭据 broker 客户端（适配器侧，D10/M7-10）。
 *
 * - 适配器 spawn 时只拿到 `AETHER_CREDENTIAL_BROKER`（`{endpoint, token}`），
 *   不含任何供应商密钥；
 * - 每次 run 按 `provider_id + protocol` 请求当前供应商凭据；
 * - 响应只在内存中使用，写入当前 CLI 子进程 env 后即丢弃；不缓存、不落盘、不日志。
 */
import { connect, type Socket } from "node:net";

export interface BrokerCredential {
  /** 供应商协议端点（可能缺省，适配器使用自身缺省）。 */
  base_url?: string;
  /** 供应商 API Key（只在当前 run 使用）。 */
  api_key: string;
  /** 子进程认证 env 名（`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `DEEPSEEK_API_KEY`）。 */
  auth_env: string;
}

interface BrokerInfo {
  endpoint: string;
  token: string;
}

interface BrokerResponse {
  ok: boolean;
  base_url?: string;
  api_key?: string;
  auth_env?: string;
  error?: string;
}

const BROKER_TIMEOUT_MS = 5_000;
const BROKER_MAX_RESPONSE_BYTES = 64 * 1024;

export class CredentialClient {
  private constructor(
    private readonly endpoint: string,
    private readonly token: string,
  ) {}

  /** 从适配器进程 env 构造；未配置 broker（旧路径）返回 `null`。 */
  static fromEnv(env: NodeJS.ProcessEnv = process.env): CredentialClient | null {
    const raw = env.AETHER_CREDENTIAL_BROKER;
    if (typeof raw !== "string" || raw.trim().length === 0) {
      return null;
    }
    try {
      const parsed = JSON.parse(raw) as Partial<BrokerInfo>;
      if (
        typeof parsed.endpoint !== "string" ||
        parsed.endpoint.length === 0 ||
        typeof parsed.token !== "string" ||
        parsed.token.length === 0
      ) {
        return null;
      }
      return new CredentialClient(parsed.endpoint, parsed.token);
    } catch {
      return null;
    }
  }

  /** 按供商 + 协议获取单次凭据；失败抛错（调用方决定 run 失败/回退）。 */
  async get(
    providerId: string,
    protocol: string,
    runtimeId?: string,
  ): Promise<BrokerCredential> {
    return new Promise<BrokerCredential>((resolve, reject) => {
      const socket: Socket = connect({ path: this.endpoint });
      let buffer = "";
      let settled = false;
      const finish = (error: Error | null, credential?: BrokerCredential) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        socket.destroy();
        if (error) reject(error);
        else resolve(credential as BrokerCredential);
      };
      const timer = setTimeout(() => {
        finish(new Error("凭据 broker 超时"));
      }, BROKER_TIMEOUT_MS);

      socket.setEncoding("utf8");
      socket.on("connect", () => {
        socket.write(
          `${JSON.stringify({
            token: this.token,
            provider_id: providerId,
            protocol,
            ...(runtimeId !== undefined ? { runtime_id: runtimeId } : {}),
          })}\n`,
        );
      });
      socket.on("data", (chunk: string) => {
        buffer += chunk;
        if (buffer.length > BROKER_MAX_RESPONSE_BYTES) {
          finish(new Error("凭据 broker 响应过大"));
          return;
        }
        const newline = buffer.indexOf("\n");
        if (newline < 0) return;
        const line = buffer.slice(0, newline).trim();
        let response: BrokerResponse;
        try {
          response = JSON.parse(line) as BrokerResponse;
        } catch {
          finish(new Error("凭据 broker 响应格式无效"));
          return;
        }
        if (!response.ok || typeof response.api_key !== "string" || typeof response.auth_env !== "string") {
          finish(new Error(response.error ?? "凭据 broker 拒绝请求"));
          return;
        }
        finish(null, {
          ...(typeof response.base_url === "string" && response.base_url.length > 0
            ? { base_url: response.base_url }
            : {}),
          api_key: response.api_key,
          auth_env: response.auth_env,
        });
      });
      socket.on("error", (error) => {
        finish(new Error(`凭据 broker 连接失败：${error.message}`));
      });
      socket.on("close", () => {
        if (!settled) finish(new Error("凭据 broker 连接已关闭"));
      });
    });
  }
}
