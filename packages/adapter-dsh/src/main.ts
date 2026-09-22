/**
 * DSH 适配器进程入口（M2-11）：仅做进程流装配。
 *
 * 编译：`bun build src/main.ts --compile --outfile dist/aether-dsh-adapter[.exe]`
 */

import { stdinLines, stderrLogger, stdoutWriter } from "@aether/adapter-sdk";

import { runCli } from "./cli";

async function main(): Promise<void> {
  const stderr = stderrLogger("[adapter-dsh]");
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
