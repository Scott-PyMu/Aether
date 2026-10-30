/**
 * 适配器宿主公共逻辑（M9-11 包体优化）。
 *
 * 背景：三个官方适配器各自 `bun build --compile` 会各自内嵌一份 Bun 运行时
 * （每个约 92MB），安装包因此膨胀约 180MB。宿主二进制只内嵌一份运行时，
 * 通过 `--adapter <id>` 分发到对应适配器 CLI；线协议/参数/日志前缀不变。
 *
 * 该优化不改变：
 * - 适配器 id/kind/协议版本/能力（仍由各 `cli.ts` 决定）；
 * - D5 `--launch-token` 注入（宿主只剥离 `--adapter`，其余参数原样透传）；
 * - stderr 前缀（`[adapter-claude-code]` 等）。
 */
import { stderrLogger, stdinLines, stdoutWriter } from "@aether/adapter-sdk";

export interface HostStreams {
  argv: string[];
  lines: AsyncIterable<string>;
  writeLine: (line: string) => Promise<void>;
  /** Mock 适配器用于超限 artifact 原始输出；官方适配器可忽略。 */
  rawWrite: (chunk: string) => void;
  stderr: (line: string) => void;
  exit: (code: number) => void;
}

export type HostRunner = (streams: HostStreams) => Promise<void> | void;

export interface HostEntry {
  /** stderr 前缀（与各适配器 main.ts 保持一致）。 */
  prefix: string;
  run: HostRunner;
}

export type HostEntries = Record<string, HostEntry>;

const ADAPTER_FLAG = "--adapter";

/** 解析并剥离 `--adapter <id>` / `--adapter=<id>`；缺失返回错误。 */
export function extractAdapter(
  argv: string[],
): { adapter: string | null; rest: string[]; error?: string } {
  const rest = [...argv];
  for (let index = 0; index < rest.length; index += 1) {
    const token = rest[index];
    if (token === ADAPTER_FLAG) {
      const value = rest[index + 1];
      if (value === undefined) {
        return { adapter: null, rest, error: "缺少 --adapter <id>" };
      }
      rest.splice(index, 2);
      return { adapter: value, rest };
    }
    if (token?.startsWith(`${ADAPTER_FLAG}=`)) {
      const value = token.slice(ADAPTER_FLAG.length + 1);
      rest.splice(index, 1);
      return { adapter: value, rest };
    }
  }
  return { adapter: null, rest, error: "缺少 --adapter <id>" };
}

/** 启动宿主：分发到 `entries[adapter]`，其余参数原样交给适配器 CLI。 */
export function startHost(entries: HostEntries, argv: string[]): void {
  const parsed = extractAdapter(argv);
  const usage = Object.keys(entries).sort().join(" / ");
  if (parsed.adapter === null) {
    process.stderr.write(`[adapter-host] ${parsed.error ?? "未知错误"}；可用：${usage}\n`);
    process.exit(2);
    return;
  }
  const entry = entries[parsed.adapter];
  if (entry === undefined) {
    process.stderr.write(
      `[adapter-host] 未知适配器：${parsed.adapter}；可用：${usage}\n`,
    );
    process.exit(2);
    return;
  }
  const stderr = stderrLogger(entry.prefix);
  const writer = stdoutWriter();
  void entry.run({
    argv: parsed.rest,
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
