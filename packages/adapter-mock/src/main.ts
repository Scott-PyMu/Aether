#!/usr/bin/env bun
/**
 * Mock 适配器入口（M1-09）：Bun 编译入口 + 进程流装配。
 *
 * CLI 解析与运行逻辑在 `./cli`（可单测）；本文件保证 stdout 只承载线协议帧
 * （JSON-RPC over JSON-Lines），日志一律走 stderr。
 *
 * 编译：`bun build src/main.ts --compile --outfile dist/aether-mock-adapter[.exe]`
 *
 * CLI（详见 `cli.ts`）：
 * `--inject` / `--inject-count` / `--protocol` / `--name` / `--runtime-version` /
 * `--stream-deltas` / `--stream-interval-ms` / `--long-stream-interval-ms` /
 * `--no-hello` / `--launch-token=<ULID>`（D5）。
 */

import { stdinLines, stderrLogger, stdoutWriter } from "@aether/adapter-sdk";

import { runCli } from "./cli";

async function main(): Promise<void> {
  // stderr 前缀与 D5 启动令牌日志格式一致（`[adapter-mock] launch-token=<ULID>`）。
  const stderr = stderrLogger("[adapter-mock]");
  const writer = stdoutWriter();
  await runCli({
    argv: process.argv.slice(2),
    lines: stdinLines(),
    writeLine: writer.writeLine,
    rawWrite: (chunk) => {
      process.stdout.write(chunk);
    },
    stderr,
    exit: (code) => {
      setTimeout(() => process.exit(code), 20);
    },
  });
}

void main();
