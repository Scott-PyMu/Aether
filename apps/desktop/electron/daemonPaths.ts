/**
 * `aetherd` 解析与 spawn 参数（M9-07；ADR-029 §3.5-3/§8.1）。
 *
 * 解析顺序：
 * 1. `AETHERD_BIN`（测试/E2E 与受控部署）；
 * 2. 打包产物 `process.resourcesPath/bin/aetherd`（electron-builder `extraResources`；
 *    M9-11 落地）；
 * 3. 开发期仓库产物 `target/debug/aetherd`（`pnpm dev:desktop` 自动构建）。
 *
 * 参数固定 `--stdio`；`AETHERD_EXTRA_ARGS` 仅用于 E2E 注入 Node 假 daemon
 * （JSON 数组，生产不设置）。数据目录/运行时目录等仍由环境变量透传。
 */
import { existsSync } from "node:fs";
import path from "node:path";

import type { InitializeParams } from "./daemonClient";

/** daemon 可执行文件名（ADR-029 §5）。 */
export const DAEMON_NAME = "aetherd";
/** 固定运行模式参数。 */
export const DAEMON_STDIO_ARG = "--stdio";
/** 运行时注册清单目录环境变量（daemon 读取）。 */
export const RUNTIME_BUNDLE_ENV = "AETHER_RUNTIME_BUNDLE_DIR";

export interface DaemonBinaryOptions {
  env?: NodeJS.ProcessEnv;
  platform?: NodeJS.Platform;
  /** `app.getAppPath()`（开发期 = 仓库根）。 */
  appPath: string;
  /** `process.resourcesPath`（打包后）。 */
  resourcesPath?: string;
  isPackaged?: boolean;
  exists?: (file: string) => boolean;
}

/** 解析 daemon 可执行文件路径；找不到时返回候选（spawn 会以 ENOENT 报错）。 */
export function resolveDaemonBinary(options: DaemonBinaryOptions): string {
  const env = options.env ?? process.env;
  const platform = options.platform ?? process.platform;
  const exists = options.exists ?? existsSync;
  const exe = platform === "win32" ? `${DAEMON_NAME}.exe` : DAEMON_NAME;

  const override = env.AETHERD_BIN;
  if (typeof override === "string" && override.trim().length > 0) {
    return path.resolve(override);
  }

  const candidates: string[] = [];
  if (options.resourcesPath) {
    candidates.push(path.join(options.resourcesPath, "bin", exe));
  }
  candidates.push(path.join(options.appPath, "target", "release", exe));
  candidates.push(path.join(options.appPath, "target", "debug", exe));

  for (const candidate of candidates) {
    if (exists(candidate)) {
      return candidate;
    }
  }
  // 打包/开发均未找到：返回开发候选，让 spawn 报 ENOENT（S-12 daemon_start_failed）。
  return candidates[candidates.length - 1] ?? path.join(options.appPath, "target", "debug", exe);
}

/** daemon 参数（`--stdio` 必须存在；E2E 额外参数在前）。 */
export function daemonArguments(env: NodeJS.ProcessEnv = {}): string[] {
  const extra: string[] = [];
  const raw = env.AETHERD_EXTRA_ARGS;
  if (typeof raw === "string" && raw.trim().length > 0) {
    try {
      const parsed = JSON.parse(raw) as unknown;
      if (Array.isArray(parsed)) {
        for (const item of parsed) {
          if (typeof item === "string") {
            extra.push(item);
          }
        }
      }
    } catch {
      // 非法注入参数直接忽略（不把测试钩子带入生产参数）。
    }
  }
  return [...extra, DAEMON_STDIO_ARG];
}

/** `initialize` 参数（ADR-029 §4.2；client=`electron`）。 */
export function initializeParams(appVersion: string): InitializeParams {
  return {
    protocolVersion: "1.0",
    appVersion,
    client: "electron",
    clientCapabilities: ["events", "permissions", "dialogs", "browser-view"],
    requiredCapabilities: ["events", "permissions"],
  };
}
