/**
 * 壳层日志（M9-07；ADR-029 §4.6/§5.4/P2-9）。
 *
 * - 写入方：Electron main；**先脱敏后落盘**（`redact.ts`）；
 * - 路径：`<data_dir>/logs/shell.log`；数据目录不可写时退化为内存环形缓冲
 *   （S-12 仍可展示已有日志与路径，不因日志失败阻断启动）；
 * - 轮转：单文件超过 {@link DEFAULT_SHELL_LOG_MAX_BYTES} 后 rename 为
 *   `shell.log.1`（只保留一份历史；M9-10/M9-11 若需归档再扩展）；
 * - 只追加、不改写；daemon 侧只读快照（不写同一文件）。
 */
import { appendFileSync, existsSync, mkdirSync, readFileSync, renameSync, rmSync, statSync } from "node:fs";

import { logDirFor, shellLogPathFor } from "./dataDir";
import { redactValue } from "./redact";

/** 默认单文件上限（2MiB；与帧上限同量级，避免日志无限增长）。 */
export const DEFAULT_SHELL_LOG_MAX_BYTES = 2 * 1024 * 1024;
/** 内存环形缓冲行数上限（日志不可写时兜底）。 */
export const MEMORY_LOG_LINES = 500;

export type ShellLogLevel = "debug" | "info" | "warn" | "error";

export interface ShellLogFields {
  [key: string]: unknown;
}

export interface ShellLoggerOptions {
  dataDir: string;
  /** 测试注入。 */
  now?: () => number;
  pid?: number;
  maxBytes?: number;
  /** 文件写入实现（测试注入；缺省 appendFileSync）。 */
  append?: (file: string, line: string) => void;
}

export interface ShellLogger {
  readonly path: string;
  readonly writable: boolean;
  log(level: ShellLogLevel, event: string, fields?: ShellLogFields): void;
  /** 内存缓冲 + 文件尾部（S-12 离线诊断用）。 */
  tail(maxLines?: number): string;
  /** 最近一次写入错误（若有）。 */
  readonly lastError?: string;
}

interface LogLine {
  ts: number;
  level: ShellLogLevel;
  event: string;
  pid: number;
  [key: string]: unknown;
}

/** 创建壳层日志器；永不抛错。 */
export function createShellLogger(options: ShellLoggerOptions): ShellLogger {
  const now = options.now ?? Date.now;
  const pid = options.pid ?? process.pid;
  const maxBytes = options.maxBytes ?? DEFAULT_SHELL_LOG_MAX_BYTES;
  const file = shellLogPathFor(options.dataDir);
  const memory: string[] = [];
  let writable = true;
  let lastError: string | undefined;

  try {
    mkdirSync(logDirFor(options.dataDir), { recursive: true });
  } catch (error) {
    writable = false;
    lastError = `创建日志目录失败：${errorText(error)}`;
  }

  const rotate = (): void => {
    try {
      if (!existsSync(file)) {
        return;
      }
      const size = statSync(file).size;
      if (size < maxBytes) {
        return;
      }
      const rotated = `${file}.1`;
      rmSync(rotated, { force: true });
      renameSync(file, rotated);
    } catch (error) {
      lastError = `轮转 shell.log 失败：${errorText(error)}`;
    }
  };

  const append = (line: string): void => {
    if (!writable) {
      return;
    }
    try {
      if (options.append) {
        options.append(file, line);
      } else {
        rotate();
        appendFileSync(file, line, "utf8");
      }
    } catch (error) {
      writable = false;
      lastError = `写入 shell.log 失败：${errorText(error)}`;
    }
  };

  return {
    path: file,
    get writable(): boolean {
      return writable;
    },
    get lastError(): string | undefined {
      return lastError;
    },
    log(level, event, fields = {}) {
      const record: LogLine = {
        ts: now(),
        level,
        event,
        pid,
        ...(redactValue(fields) as Record<string, unknown>),
      };
      const line = `${JSON.stringify(record)}\n`;
      append(line);
      memory.push(line.trimEnd());
      while (memory.length > MEMORY_LOG_LINES) {
        memory.shift();
      }
    },
    tail(maxLines = 200) {
      const limit = Math.max(1, maxLines);
      const memoryTail = memory.slice(-limit);
      if (!writable) {
        return memoryTail.join("\n");
      }
      try {
        if (!existsSync(file)) {
          return memoryTail.join("\n");
        }
        const text = readFileSync(file, "utf8");
        return text.split(/\r?\n/).filter((line) => line.length > 0).slice(-limit).join("\n");
      } catch {
        return memoryTail.join("\n");
      }
    },
  };
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** 读取任意日志文件尾部（离线诊断；不抛错）。 */
export function readFileTail(file: string, maxLines: number): string {
  try {
    // 日志文件受轮转上限约束（2MiB），整读后截取尾部足够安全。
    const text = readFileSync(file, "utf8");
    return text
      .split(/\r?\n/)
      .filter((line) => line.length > 0)
      .slice(-Math.max(1, maxLines))
      .join("\n");
  } catch {
    return "";
  }
}
