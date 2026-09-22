/**
 * DSH 适配器 CLI（M2-11）。
 *
 * CLI：
 * - `--launch-token=<ULID>`（D5：核心 spawn 注入）
 * - `--dsh-bin <path>`（必填；`@deepseek-ai/dsh/lib/bin.js`）
 * - `--dsh-node <path>`（Node 可执行文件；默认当前进程）
 * - `--dsh-home <dir>`（必填；隔离 DSH_HOME）
 * - `--dsh-profile <name>`（默认 acp）
 * - `--dsh-provider <name>` / `--dsh-model <name>`（overlay 覆写）
 * - `--dsh-version <ver>`（显式版本；缺省从 dsh-bin 向上解析 package.json）
 * - `--dsh-version-pin <ver>`（默认 0.1.5-rc.2）
 * - `--dsh-provider-config <inline-json|file>`（`llm-pi-ai` composition base；核心传入）
 * - `--dsh-arg <arg>`（可重复；追加启动参数）
 * - `--dsh-patch <path>`（overlay 路径；缺省生成到 DSH_HOME）
 * - `--delta-dir <dir>`（带外通道目录；默认 `<DSH_HOME>/aether-bridge`）
 * - `--hello-timeout-ms <n>`（插件契约帧等待上限）
 * - `--workspace <dir>`（ACP session cwd）
 * - `--run-timeout-ms <n>` / `--permission-timeout-ms <n>`
 */

import { readFileSync } from "node:fs";

import { isUlid, type LineWriter } from "@aether/adapter-sdk";

import { DEFAULT_DSH_VERSION_PIN, validateProviderConfig } from "./dsh-plugin";
import { DshAdapter, DEFAULT_RUN_TIMEOUT_MS, type DshCliConfig } from "./dsh-adapter";

export const LAUNCH_TOKEN_FLAG = "--launch-token=";

export interface CliOptions {
  cli: DshCliConfig;
  runTimeoutMs?: number;
  permissionTimeoutMs?: number;
  launchToken?: string;
}

function requireValue(argv: string[], index: number, flag: string): string {
  const value = argv[index];
  if (value === undefined) throw new Error(`参数 ${flag} 缺少值`);
  return value;
}

function requireNumber(argv: string[], index: number, flag: string): number {
  const value = Number(requireValue(argv, index, flag));
  if (!Number.isFinite(value) || value < 0) throw new Error(`参数 ${flag} 需要非负数字`);
  return value;
}

export function parseLaunchToken(flag: string, warn: (line: string) => void): string | undefined {
  const value = flag.slice(LAUNCH_TOKEN_FLAG.length);
  if (isUlid(value)) return value;
  warn("忽略非法 --launch-token（期望 26 位 Crockford ULID）");
  return undefined;
}

/** 解析 CLI 参数；未知参数/缺值抛错。 */
export function parseArgs(argv: string[], warn: (line: string) => void): CliOptions {
  const cli: Partial<DshCliConfig> & { extraArgs: string[]; profile: string; versionPin: string } = {
    extraArgs: [],
    profile: "acp",
    versionPin: DEFAULT_DSH_VERSION_PIN,
  } as Partial<DshCliConfig> & { extraArgs: string[]; profile: string; versionPin: string };
  const options: CliOptions = { cli: cli as DshCliConfig };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index] ?? "";
    if (flag.startsWith(LAUNCH_TOKEN_FLAG)) {
      const token = parseLaunchToken(flag, warn);
      if (token !== undefined) options.launchToken = token;
      continue;
    }
    if (flag === "--launch-token") {
      warn("忽略非法 --launch-token（缺少 =<ULID> 值）");
      continue;
    }
    switch (flag) {
      case "--dsh-bin":
        cli.bin = requireValue(argv, ++index, flag);
        break;
      case "--dsh-node":
        cli.nodeBin = requireValue(argv, ++index, flag);
        break;
      case "--dsh-home":
        cli.home = requireValue(argv, ++index, flag);
        break;
      case "--dsh-profile":
        cli.profile = requireValue(argv, ++index, flag);
        break;
      case "--dsh-provider":
        cli.provider = requireValue(argv, ++index, flag);
        break;
      case "--dsh-model":
        cli.model = requireValue(argv, ++index, flag);
        break;
      case "--dsh-version":
        cli.dshVersion = requireValue(argv, ++index, flag);
        break;
      case "--dsh-provider-config": {
        const raw = requireValue(argv, ++index, flag).trim();
        let parsed: unknown;
        try {
          // 容忍 Windows PowerShell 写入的 UTF-8 BOM（spike 已知坑 19② 同类）。
          const text = (raw.startsWith("{") ? raw : readFileSync(raw, "utf8")).replace(
            /^\uFEFF/,
            "",
          );
          parsed = JSON.parse(text);
        } catch (error) {
          throw new Error(
            `--dsh-provider-config 不是合法 JSON 或文件不可读：${error instanceof Error ? error.message : String(error)}`,
          );
        }
        cli.providerConfig = validateProviderConfig(parsed);
        break;
      }
      case "--dsh-version-pin":
        cli.versionPin = requireValue(argv, ++index, flag);
        break;
      case "--dsh-arg":
        cli.extraArgs.push(requireValue(argv, ++index, flag));
        break;
      case "--dsh-patch":
        cli.patchPath = requireValue(argv, ++index, flag);
        break;
      case "--delta-dir":
        cli.deltaDir = requireValue(argv, ++index, flag);
        break;
      case "--hello-timeout-ms":
        cli.helloTimeoutMs = requireNumber(argv, ++index, flag);
        break;
      case "--workspace":
        cli.workspace = requireValue(argv, ++index, flag);
        break;
      case "--run-timeout-ms":
        options.runTimeoutMs = requireNumber(argv, ++index, flag);
        break;
      case "--permission-timeout-ms":
        options.permissionTimeoutMs = requireNumber(argv, ++index, flag);
        break;
      default:
        throw new Error(`未知参数: ${flag || "<empty>"}`);
    }
  }
  if (!cli.bin) throw new Error("缺少 --dsh-bin（@deepseek-ai/dsh/lib/bin.js）");
  if (!cli.home) throw new Error("缺少 --dsh-home（隔离 DSH_HOME）");
  if (!cli.workspace) cli.workspace = process.cwd();
  return options;
}

export interface CliStreams {
  argv: string[];
  lines: AsyncIterable<string>;
  writeLine: LineWriter["writeLine"];
  stderr: (line: string) => void;
  exit: (code: number) => void;
}

export async function runCli(streams: CliStreams): Promise<void> {
  let options: CliOptions;
  try {
    options = parseArgs(streams.argv, streams.stderr);
  } catch (error) {
    streams.stderr(`参数错误: ${error instanceof Error ? error.message : String(error)}`);
    streams.exit(2);
    return;
  }
  if (options.launchToken !== undefined) {
    streams.stderr(`launch-token=${options.launchToken}`);
  }
  const adapter = new DshAdapter({
    lines: streams.lines,
    writeLine: streams.writeLine,
    stderr: streams.stderr,
    exit: (code) => streams.exit(code),
    cli: options.cli,
    ...(options.runTimeoutMs !== undefined ? { runTimeoutMs: options.runTimeoutMs } : {}),
    ...(options.permissionTimeoutMs !== undefined
      ? { permissionTimeoutMs: options.permissionTimeoutMs }
      : {}),
  });
  try {
    await adapter.run();
  } catch (error) {
    streams.stderr(`运行失败: ${error instanceof Error ? error.message : String(error)}`);
    streams.exit(1);
  }
}

export { DEFAULT_RUN_TIMEOUT_MS };
