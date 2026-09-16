#!/usr/bin/env bun
/**
 * Mock 适配器入口（M1-09）：用 Bun 编译为单文件可执行。
 *
 * 编译：`bun build src/main.ts --compile --outfile dist/aether-mock-adapter[.exe]`
 *
 * CLI：
 * - `--inject <kind>`（可重复）bad-json | stdout-log | half-line | oversized-line |
 *   line-over-2mib | artifact-line | artifact-line-over-limit | crash |
 *   capability-missing | hang | no-hello
 * - `--inject-count <n>`（默认 20）
 * - `--protocol <ver>` 覆盖 hello.protocol（版本不匹配注入）
 * - `--name <runtime 名>` / `--runtime-version <ver>`
 * - `--stream-deltas <n>` / `--stream-interval-ms <n>` / `--long-stream-interval-ms <n>`
 */

import { stdinLines, stderrLogger, stdoutWriter } from "@aether/adapter-sdk";

import { INJECTION_KINDS, MockAdapter, type InjectionKind, type MockAdapterOptions } from "./mock-adapter";

interface CliOptions {
  injections: InjectionKind[];
  injectCount?: number;
  protocol?: string;
  runtimeName?: string;
  runtimeVersion?: string;
  streamDeltas?: number;
  streamIntervalMs?: number;
  longStreamIntervalMs?: number;
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

export function parseArgs(argv: string[]): CliOptions {
  const options: CliOptions = { injections: [] };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
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
      case "--no-hello":
        options.injections.push("no-hello");
        break;
      default:
        throw new Error(`未知参数: ${flag ?? "<empty>"}`);
    }
  }
  return options;
}

async function main(): Promise<void> {
  const stderr = stderrLogger("[aether-mock]");
  let options: CliOptions;
  try {
    options = parseArgs(process.argv.slice(2));
  } catch (error) {
    stderr(`参数错误: ${error instanceof Error ? error.message : String(error)}`);
    process.exit(2);
    return;
  }

  const writer = stdoutWriter();
  const mockOptions: MockAdapterOptions = {
    lines: stdinLines(),
    writeLine: writer.writeLine,
    rawWrite: (chunk) => {
      process.stdout.write(chunk);
    },
    stderr,
    exit: (code) => {
      setTimeout(() => process.exit(code), 20);
    },
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

  const mock = new MockAdapter(mockOptions);
  try {
    await mock.run();
  } catch (error) {
    stderr(`运行失败: ${error instanceof Error ? error.message : String(error)}`);
    process.exit(1);
  }
}

void main();
