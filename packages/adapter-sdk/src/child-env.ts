/**
 * ADR-028 v0.9：CLI/工具子进程 env 共享剥离工具。
 *
 * 所有官方适配器的 CLI/工具子进程必须移除 `AETHER_CREDENTIAL_BROKER`：该 token
 * 只允许适配器进程自身持有（§2.3 已知信任边界），不得继承给 CLI，避免 CLI 绕过
 * 适配器直接向 broker 请求任意 provider 凭据。
 */

export const BROKER_ENV_KEY = "AETHER_CREDENTIAL_BROKER";

/** 返回移除 broker token 后的新 env（不修改入参）。 */
export function stripBrokerEnv(
  env: NodeJS.ProcessEnv = process.env,
): NodeJS.ProcessEnv {
  const next: NodeJS.ProcessEnv = { ...env };
  delete next[BROKER_ENV_KEY];
  return next;
}

/** 就地剥离（用于已构造的 env 对象；返回同一对象）。 */
export function stripBrokerEnvInPlace(env: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  delete env[BROKER_ENV_KEY];
  return env;
}
