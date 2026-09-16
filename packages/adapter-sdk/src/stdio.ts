/**
 * stdio 传输（D6：JSON-RPC over stdio）。
 *
 * 约定：
 * - stdout **只**承载线协议帧（SDK 出站编码）；
 * - 日志/诊断一律走 stderr（SDK 拦截 `console.log` 重定向 stderr 的职责由适配器自行遵守，
 *   Mock 通过 `stderrLogger` 实现）；
 * - 写侧带背压（drain 等待），防止洪水写入打爆内存。
 */

import { once } from "node:events";
import { createInterface } from "node:readline";

import { encodeLine } from "./framing";

/** 读取 stdin 的 LF 分隔行（容忍 CRLF；行内容不含换行符）。 */
export function stdinLines(): AsyncIterable<string> {
  return createInterface({ input: process.stdin, crlfDelay: Infinity });
}

export interface LineWriter {
  writeLine(line: string): Promise<void>;
}

/** stdout 行写入器（编码 + LF + 背压）。 */
export function stdoutWriter(): LineWriter {
  return {
    async writeLine(line: string): Promise<void> {
      const chunk = encodeLine(line);
      if (!process.stdout.write(chunk)) {
        await once(process.stdout, "drain");
      }
    },
  };
}

/** stderr 日志器（带前缀，保证日志不污染协议流）。 */
export function stderrLogger(prefix = "[aether-adapter]"): (line: string) => void {
  return (line: string) => {
    process.stderr.write(`${prefix} ${line}\n`);
  };
}
