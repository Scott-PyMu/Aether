/**
 * Mock 适配器 CLI（M1-09；M1-10 增量修订）。
 *
 * 与进程无关的纯逻辑集中在本文件（可单测）；`main.ts` 仅负责进程流装配。
 *
 * CLI：
 * - `--inject <kind>`（可重复）bad-json | stdout-log | half-line | oversized-line |
 *   line-over-2mib | oversized-line-burst | line-over-2mib-burst | artifact-line |
 *   artifact-line-over-limit | artifact-ref-outside | crash |
 *   capability-missing | hang | no-hello
 * - `--inject-count <n>`（默认 20）
 * - `--protocol <ver>` 覆盖 hello.protocol（版本不匹配注入）
 * - `--name <runtime 名>` / `--runtime-version <ver>`
 * - `--stream-deltas <n>` / `--stream-interval-ms <n>` / `--long-stream-interval-ms <n>`
 * - `--artifacts-dir <path>` 附件目录（M2-09/D6；缺省回退环境变量 `AETHER_ARTIFACTS_DIR`）
 * - `--launch-token=<ULID>`（D5：核心 spawn 注入；合法值仅写 stderr 启动日志，
 *   非法值 warn 后忽略，不阻塞启动；stdout 线协议不受影响）
 */

import { isUlid, type LineWriter } from "@aether/adapter-sdk";

import { INJECTION_KINDS, MockAdapter, type InjectionKind, type MockAdapterOptions } from "./mock-adapter";

/** D5 spawn 注入形式（单参数 `--launch-token=<ULID>`）。 */
export const LAUNCH_TOKEN_FLAG = "--launch-token=";

export interface CliOptions {
  injections: InjectionKind[];
  injectCount?: number;
  protocol?: string;
  runtimeName?: string;
  runtimeVersion?: string;
  streamDeltas?: number;
  streamIntervalMs?: number;
  longStreamIntervalMs?: number;
  /** M2-09：附件目录（缺省回退 `AETHER_ARTIFACTS_DIR`）。 */
  artifactsDir?: string;
  /** D5 启动令牌（仅合法 ULID；非法/缺失为 undefined）。 */
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

/**
 * 解析 `--launch-token=<ULID>`（D5）。
 *
 * - 合法 ULID（26 位 Crockford Base32）→ 返回 token；
 * - 非法值（非 ULID / 空值 / 缺少 `=<值>`）→ `warn` 后返回 undefined，**不抛错**。
 *
 * 注意：`warn` 是裸行 sink；生产由 `main.ts` 的 `stderrLogger("[adapter-mock]")`
 * 统一加前缀，最终 stderr 形如 `[adapter-mock] launch-token=<ULID>`。
 */
export function parseLaunchToken(flag: string, warn: (line: string) => void): string | undefined {
  const value = flag.slice(LAUNCH_TOKEN_FLAG.length);
  if (isUlid(value)) return value;
  warn("忽略非法 --launch-token（期望 26 位 Crockford ULID）");
  return undefined;
}

/** 解析 CLI 参数；未知参数/缺值仍按既有行为抛错。 */
export function parseArgs(argv: string[], warn: (line: string) => void): CliOptions {
  const options: CliOptions = { injections: [] };
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
      case "--inject": {
        const value = requireValue(argv, ++index, flag);
        if (!INJECTION_KINDS.includes(value as InjectionKind)) {
          throw new Error(`未知注入类型: ${value}（可用: ${INJECTION_KINDS.join(", ")}）`);
        }
        options.injections.push(value as InjectionKind);
        break;
      }
      case "--inject-count":
        options.injectCount = requireNumber(argv, ++index, flag);
        break;
      case "--protocol":
        options.protocol = requireValue(argv, ++index, flag);
        break;
      case "--name":
        options.runtimeName = requireValue(argv, ++index, flag);
        break;
      case "--runtime-version":
        options.runtimeVersion = requireValue(argv, ++index, flag);
        break;
      case "--stream-deltas":
        options.streamDeltas = requireNumber(argv, ++index, flag);
        break;
      case "--stream-interval-ms":
        options.streamIntervalMs = requireNumber(argv, ++index, flag);
        break;
      case "--long-stream-interval-ms":
        options.longStreamIntervalMs = requireNumber(argv, ++index, flag);
        break;
      case "--artifacts-dir":
        options.artifactsDir = requireValue(argv, ++index, flag);
        break;
      case "--no-hello":
        options.injections.push("no-hello");
        break;
      default:
        throw new Error(`未知参数: ${flag || "<empty>"}`);
    }
  }
  return options;
}

/**
 * 把 `--launch-token` 写入 stderr 启动日志（供 D5 PID 台账第 ③ 条件核对）。
 *
 * 只走 stderr：stdout 保持 JSON-RPC over JSON-Lines 纯净；生产输出形如
 * `[adapter-mock] launch-token=<ULID>`（前缀由 `main.ts` 的 stderr logger 添加）。
 */
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
  rawWrite: (chunk: string) => void;
  stderr: (line: string) => void;
  exit: (code: number) => void;
}

/** 执行 CLI（解析 → 启动日志 → Mock 事件循环）。 */
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

  const mockOptions: MockAdapterOptions = {
    lines: streams.lines,
    writeLine: streams.writeLine,
    rawWrite: streams.rawWrite,
    stderr: streams.stderr,
    exit: streams.exit,
    injections: options.injections,
  };
  if (options.injectCount !== undefined) mockOptions.injectCount = options.injectCount;
  if (options.protocol !== undefined) mockOptions.protocol = options.protocol;
  if (options.runtimeName !== undefined) mockOptions.runtimeName = options.runtimeName;
  if (options.runtimeVersion !== undefined) mockOptions.runtimeVersion = options.runtimeVersion;
  if (options.streamDeltas !== undefined) mockOptions.streamDeltas = options.streamDeltas;
  if (options.streamIntervalMs !== undefined) mockOptions.streamIntervalMs = options.streamIntervalMs;
  if (options.longStreamIntervalMs !== undefined) {
    mockOptions.longStreamIntervalMs = options.longStreamIntervalMs;
  }
  // M2-09：`--artifacts-dir` 优先，缺省回退 `AETHER_ARTIFACTS_DIR`（监督器 spawn 注入）。
  mockOptions.artifactsDir = options.artifactsDir ?? process.env.AETHER_ARTIFACTS_DIR;

  const mock = new MockAdapter(mockOptions);
  try {
    await mock.run();
  } catch (error) {
    streams.stderr(`运行失败: ${error instanceof Error ? error.message : String(error)}`);
    streams.exit(1);
  }
}
