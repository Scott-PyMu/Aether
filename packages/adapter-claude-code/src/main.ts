/**
 * Claude Code 适配器进程入口（M2-02）：仅做进程流装配（与 Mock 适配器同构）。
 *
 * 编译：`bun build src/main.ts --compile --outfile dist/aether-claude-adapter[.exe]`
 */

import { stdinLines, stderrLogger, stdoutWriter } from "@aether/adapter-sdk";

import { runCli } from "./cli";

async function main(): Promise<void> {
  const stderr = stderrLogger("[adapter-claude-code]");
  const writer = stdoutWriter();
  await runCli({
    argv: process.argv.slice(2),
    lines: stdinLines(),
    writeLine: writer.writeLine,
    stderr,
    exit: (code) => {
      setTimeout(() => process.exit(code), 20);
    },
  });
}

void main();
