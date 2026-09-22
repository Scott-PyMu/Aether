/**
 * Codex 适配器 CLI（ADR-008 / M2-11 Codex 路径）。
 *
 * CLI：
 * - `--launch-token=<ULID>`（D5：核心 spawn 注入；合法值写 stderr 启动日志）
 * - `--codex-bin <path>`（默认 `codex`；测试指向 node）
 * - `--codex-arg <arg>`（可重复；追加在子命令之前）
 * - `--codex-home <dir>`（隔离 CODEX_HOME，spike 已知坑 9）
 * - `--state-dir <dir>`（别名映射目录；默认 `<CODEX_HOME>/aether-bridge`）
 * - `--workspace <dir>`（CLI 工作根 `--cd`）
 * - `--model <name>`（会话级默认模型；`session.create` 的 model 覆盖优先）
 * - `--sandbox <mode>`（read-only 默认）
 * - `--reasoning <level>`（`-c model_reasoning_effort=<level>`）
 * - `--config <key=value>`（可重复；附加 `-c` 覆写）
 * - `--run-timeout-ms <n>`（适配器硬超时；默认 30min）
 */

import { isUlid, type LineWriter } from "@aether/adapter-sdk";

import { CodexAdapter, DEFAULT_RUN_TIMEOUT_MS } from "./codex-adapter";
import type { CodexCliConfig } from "./codex-cli";

/** D5 spawn 注入形式（单参数 `--launch-token=<ULID>`）。 */
export const LAUNCH_TOKEN_FLAG = "--launch-token=";

export interface CliOptions {
  cli: CodexCliConfig;
  stateDir?: string;
  runTimeoutMs?: number;
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

/** 解析 `--launch-token=<ULID>`（非法值 warn 后忽略，不阻塞启动）。 */
export function parseLaunchToken(flag: string, warn: (line: string) => void): string | undefined {
  const value = flag.slice(LAUNCH_TOKEN_FLAG.length);
  if (isUlid(value)) return value;
  warn("忽略非法 --launch-token（期望 26 位 Crockford ULID）");
  return undefined;
}

/** 解析 CLI 参数；未知参数/缺值抛错。 */
export function parseArgs(argv: string[], warn: (line: string) => void): CliOptions {
  const configOverrides: string[] = [];
  const cli: CodexCliConfig = {
    bin: "codex",
    extraArgs: [],
    workspace: process.cwd(),
    sandbox: "read-only",
    reasoning: "low",
    configOverrides,
  };
  const options: CliOptions = { cli };
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
      case "--codex-bin":
        cli.bin = requireValue(argv, ++index, flag);
        break;
      case "--codex-arg":
        cli.extraArgs.push(requireValue(argv, ++index, flag));
        break;
      case "--codex-home":
        cli.home = requireValue(argv, ++index, flag);
        break;
      case "--state-dir":
        options.stateDir = requireValue(argv, ++index, flag);
        break;
      case "--workspace":
        cli.workspace = requireValue(argv, ++index, flag);
        break;
      case "--model":
        cli.model = requireValue(argv, ++index, flag);
        break;
      case "--sandbox":
        cli.sandbox = requireValue(argv, ++index, flag);
        break;
      case "--reasoning":
        cli.reasoning = requireValue(argv, ++index, flag);
        break;
      case "--config":
        configOverrides.push(requireValue(argv, ++index, flag));
        break;
      case "--run-timeout-ms":
        options.runTimeoutMs = requireNumber(argv, ++index, flag);
        break;
      default:
        throw new Error(`未知参数: ${flag || "<empty>"}`);
    }
  }
  return options;
}

/** 把 `--launch-token` 写入 stderr 启动日志（D5 PID 台账第 ③ 条件核对）。 */
export function announceLaunchToken(
  launchToken: string | undefined,
  stderr: (line: string) => void,
): void {
  if (launchToken !== undefined) {
    stderr(`launch-token=${launchToken}`);
  }
}

/** CLI 所需的进程流装配（生产：`main.ts`；单测：注入桩）。 */
export interface CliStreams {
  argv: string[];
  lines: AsyncIterable<string>;
  writeLine: LineWriter["writeLine"];
  stderr: (line: string) => void;
  exit: (code: number) => void;
}

/** 执行 CLI（解析 → 启动日志 → 适配器事件循环）。 */
export async function runCli(streams: CliStreams): Promise<void> {
  let options: CliOptions;
  try {
    options = parseArgs(streams.argv, streams.stderr);
  } catch (error) {
    streams.stderr(`参数错误: ${error instanceof Error ? error.message : String(error)}`);
    streams.exit(2);
    return;
  }

  announceLaunchToken(options.launchToken, streams.stderr);

  const adapter = new CodexAdapter({
    lines: streams.lines,
    writeLine: streams.writeLine,
    stderr: streams.stderr,
    exit: (code) => streams.exit(code),
    cli: options.cli,
    ...(options.stateDir !== undefined ? { stateDir: options.stateDir } : {}),
    ...(options.runTimeoutMs !== undefined ? { runTimeoutMs: options.runTimeoutMs } : {}),
  });
  try {
    await adapter.run();
  } catch (error) {
    streams.stderr(`运行失败: ${error instanceof Error ? error.message : String(error)}`);
    streams.exit(1);
  }
}

export { DEFAULT_RUN_TIMEOUT_MS };
