/**
 * 日志脱敏（M9-07；ADR-029 §5.1/§5.4；AGENTS §2.9）。
 *
 * `shell.log` 由 main 写入，daemon 只读快照（ADR-029 §4.6）。密钥/令牌/口令
 * 不得出现在日志、诊断包、导出文件与错误 data 中；这里做**保守**替换：
 * 宁可多替换（把疑似值打码），也不把 `api_key`/Bearer/口令写盘。
 *
 * M9-10 会对全量日志/诊断/导出做扫描（0 命中）；本模块先保证壳层自身不泄漏。
 */

/** 脱敏后的占位符。 */
export const REDACTED = "[REDACTED]";

/** 环境变量名命中这些片段时不落盘（键本身也不写）。 */
const SECRET_ENV_HINTS = [
  "KEY",
  "TOKEN",
  "SECRET",
  "PASSWORD",
  "PASSWD",
  "PASSPHRASE",
  "CREDENTIAL",
  "AUTH",
];

/**
 * 文本脱敏：
 * - `api_key` / `apiKey` / `token` / `password` / `passphrase` / `secret` 的 JSON/键值形态；
 * - `Authorization: Bearer ...`；
 * - 常见供应商密钥前缀（`sk-...`）；
 * - `keychain://` 引用不是密钥（保留，便于诊断）。
 */
export function redactText(input: string): string {
  let output = input;
  output = output.replace(
    /(Authorization\s*:\s*Bearer\s+)[^\s"',;}]+/gi,
    (_match, prefix: string) => `${prefix}${REDACTED}`,
  );
  output = output.replace(
    /("?(?:api[_-]?key|token|password|passwd|passphrase|secret|authorization)"?\s*[:=]\s*)("([^"\\]|\\.)*"|[^\s,;}\]]+)/gi,
    (_match, prefix: string) => `${prefix}${REDACTED}`,
  );
  output = output.replace(/\bsk-[A-Za-z0-9_-]{8,}\b/g, REDACTED);
  return output;
}

/** 递归脱敏结构化值（保留键名，值替换；深度上限 8）。 */
export function redactValue(value: unknown, depth = 0): unknown {
  if (depth > 8) {
    return "[TRUNCATED]";
  }
  if (typeof value === "string") {
    return redactText(value);
  }
  if (Array.isArray(value)) {
    return value.map((item) => redactValue(item, depth + 1));
  }
  if (typeof value === "object" && value !== null) {
    const result: Record<string, unknown> = {};
    for (const [key, item] of Object.entries(value as Record<string, unknown>)) {
      if (isSecretKey(key)) {
        result[key] = REDACTED;
      } else {
        result[key] = redactValue(item, depth + 1);
      }
    }
    return result;
  }
  return value;
}

/** 键名是否属于密钥类（不区分大小写）。 */
export function isSecretKey(key: string): boolean {
  const normalized = key.toLowerCase();
  return (
    normalized.includes("api_key") ||
    normalized.includes("apikey") ||
    normalized.includes("token") ||
    normalized.includes("password") ||
    normalized.includes("passwd") ||
    normalized.includes("passphrase") ||
    normalized.includes("secret") ||
    normalized.includes("authorization") ||
    normalized === "credential" ||
    normalized === "credentials"
  );
}

/** 环境变量快照（只保留非密钥项；密钥键直接剔除）。 */
export function redactEnv(env: NodeJS.ProcessEnv): Record<string, string> {
  const result: Record<string, string> = {};
  for (const [key, value] of Object.entries(env)) {
    if (typeof value !== "string") {
      continue;
    }
    const upper = key.toUpperCase();
    if (SECRET_ENV_HINTS.some((hint) => upper.includes(hint))) {
      continue;
    }
    result[key] = redactText(value);
  }
  return result;
}
