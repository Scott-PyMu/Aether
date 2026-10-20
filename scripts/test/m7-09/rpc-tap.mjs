#!/usr/bin/env node
/**
 * M7-09 真实运行时线上观测 tap（opt-in 证据；DoD4「≥1 真实运行时」）。
 *
 * 用途：核心 → 适配器之间是 JSON-RPC 2.0 over JSON-Lines（D6）。本脚本作为
 * **透明代理**（tap）包住真实适配器进程：转发 stdin/stdout，同时把请求/响应行里
 * 带 `method` 的 JSON-RPC 帧（脱敏后仅 method/params/id）追加写入 `--log`。
 *
 * 用法（由 `m7_09_presets.rs` 的 opt-in 用例构造 RuntimeManifest）：
 *   node rpc-tap.mjs --log <path.jsonl> -- <adapter-bundle> [adapter args...]
 *
 * 边界：tap 只做观测，不改写任何帧；真实运行时进程仍由 tap 以子进程方式启动
 * （测试专用，不进入生产装配）。
 */
import { spawn } from "node:child_process";
import { createWriteStream } from "node:fs";
import process from "node:process";
import { createInterface } from "node:readline";

const argv = process.argv.slice(2);
let logPath = null;
let index = 0;
for (; index < argv.length; index += 1) {
  const flag = argv[index];
  if (flag === "--log") {
    index += 1;
    logPath = argv[index] ?? null;
    continue;
  }
  if (flag === "--") {
    index += 1;
    break;
  }
  console.error(`[rpc-tap] 未知参数：${flag}`);
  process.exit(2);
}
const rest = argv.slice(index);
const command = rest[0];
const args = rest.slice(1);
if (!command) {
  console.error("[rpc-tap] 缺少目标适配器命令");
  process.exit(2);
}

const log = logPath ? createWriteStream(logPath, { flags: "a" }) : null;

function record(direction, line) {
  if (!log) return;
  let parsed;
  try {
    parsed = JSON.parse(line);
  } catch {
    return;
  }
  if (parsed === null || typeof parsed !== "object" || parsed.method === undefined) return;
  log.write(
    `${JSON.stringify({
      direction,
      id: parsed.id ?? null,
      method: parsed.method,
      params: parsed.params ?? null,
    })}\n`,
  );
}

const child = spawn(command, args, {
  stdio: ["pipe", "pipe", "inherit"],
  windowsHide: true,
});

// 核心 → 适配器（记录请求帧后原样转发）。
const input = createInterface({ input: process.stdin });
input.on("line", (line) => {
  record("request", line);
  child.stdin.write(`${line}\n`);
});
input.on("close", () => {
  child.stdin.end();
});

// 适配器 → 核心（记录响应帧后原样转发）。
const output = createInterface({ input: child.stdout });
output.on("line", (line) => {
  record("response", line);
  process.stdout.write(`${line}\n`);
});

child.on("exit", (code) => {
  log?.end();
  process.exit(code ?? 0);
});
child.on("error", (error) => {
  console.error(`[rpc-tap] 启动失败：${error.message}`);
  process.exit(1);
});
