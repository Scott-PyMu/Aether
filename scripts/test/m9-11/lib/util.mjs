/** M9-11 冒烟共享工具（仅 Node 标准库）。 */
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export async function waitFor(fn, timeoutMs, label, intervalMs = 100) {
  const started = Date.now();
  let lastError = null;
  while (Date.now() - started < timeoutMs) {
    try {
      const value = await fn();
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await sleep(intervalMs);
  }
  throw new Error(`等待超时（${timeoutMs}ms）：${label}${lastError ? `（${lastError.message}）` : ""}`);
}

export function readJson(file) {
  return JSON.parse(readFileSync(file, "utf8"));
}

/** 读取 JSONL 日志（忽略坏行）。 */
export function readJsonLines(file) {
  if (!existsSync(file)) return [];
  const lines = readFileSync(file, "utf8").split(/\r?\n/);
  const records = [];
  for (const line of lines) {
    if (line.trim().length === 0) continue;
    try {
      records.push(JSON.parse(line));
    } catch {
      // 日志轮转/半行：忽略。
    }
  }
  return records;
}

export function findShellEvent(records, event) {
  return records.find((record) => record?.event === event) ?? null;
}

export function isPidAlive(pid) {
  if (!pid || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

export function repoRelative(repoRoot, file) {
  return path.relative(repoRoot, file).split(path.sep).join("/");
}

/** 校验记录器（与 `scripts/test/lib/exec.mjs#summarize` 同口径输出）。 */
export function createChecks() {
  const checks = [];
  return {
    checks,
    record(name, ok, detail) {
      checks.push({
        name: detail ? `${name}（${detail}）` : name,
        exit: ok ? 0 : 1,
        expect: 0,
      });
      return ok;
    },
    skip(name, reason) {
      checks.push({ name, exit: 0, expect: 0, skip: true, reason });
    },
  };
}
