/**
 * M2-02 DoD4：真实 Claude Code 会话完成率预演（opt-in；需凭证与网络）。
 *
 * 口径（实施计划 M2-02 DoD4）：连续 50 次真实会话，完成率 ≥95%（≥48/50），
 * 每个 run 均有终态（completed / failed，无挂起）。失败不隐藏：逐 run 记录
 * terminal 与错误码，环境性失败（中转 5xx 等）计入分母但在证据中显式说明。
 *
 * 显式要求（沿用 AETHER_REQUIRE_* 纪律，禁止静默跳过）：
 *   AETHER_REQUIRE_REAL_CLAUDE=1  必须执行；缺凭证/适配器时判失败（非跳过）。
 *   未设置时打印 SKIP 行并退出 0（`pnpm verify:m2-02` 的 opt-in 步骤）。
 *
 * 环境变量：
 *   AETHER_CLAUDE_ADAPTER   适配器编译产物路径（必填）
 *   AETHER_CLAUDE_BIN       真实 claude 可执行文件（默认 `claude`）
 *   AETHER_CLAUDE_BASE_URL / AETHER_CLAUDE_TOKEN / AETHER_CLAUDE_MODEL
 *     （兼容 ANTHROPIC_BASE_URL / ANTHROPIC_AUTH_TOKEN / ANTHROPIC_MODEL）
 *   AETHER_REAL_CLAUDE_RUNS  运行次数（默认 50）
 *
 * 用法：node scripts/test/m2-02/real-claude-50.mjs
 */
import { spawn } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import process from "node:process";

const RUNS = Number(process.env.AETHER_REAL_CLAUDE_RUNS || 50);
const REQUIRED = process.env.AETHER_REQUIRE_REAL_CLAUDE === "1";
const adapter = process.env.AETHER_CLAUDE_ADAPTER || "";
const claudeBin = process.env.AETHER_CLAUDE_BIN || "claude";
const baseUrl = process.env.AETHER_CLAUDE_BASE_URL || process.env.ANTHROPIC_BASE_URL || "";
const token = process.env.AETHER_CLAUDE_TOKEN || process.env.ANTHROPIC_AUTH_TOKEN || "";
const model = process.env.AETHER_CLAUDE_MODEL || process.env.ANTHROPIC_MODEL || "deepseek-v4-pro";

const BASELINE_PROMPT = [
  "Output exactly the following three lines and nothing else. Do not add quotes, markdown or comments.",
  "Aether M2-02 real adapter baseline line 1",
  "Aether M2-02 real adapter baseline line 2",
  "Aether M2-02 real adapter baseline line 3",
].join("\n");

const TERMINAL_TYPES = new Set(["run.completed", "run.failed", "run.cancelled"]);

function skip(reason) {
  if (REQUIRED) {
    console.error(`FAIL（AETHER_REQUIRE_REAL_CLAUDE=1）：${reason}`);
    process.exit(1);
  }
  console.log(`SKIP（opt-in 未执行）：${reason}；设置 AETHER_REQUIRE_REAL_CLAUDE=1 与凭证后重跑`);
  process.exit(0);
}

if (!adapter) skip("AETHER_CLAUDE_ADAPTER 未设置（先运行 pnpm verify:m2-02 构建适配器）");
if (!baseUrl) skip("缺少 AETHER_CLAUDE_BASE_URL / ANTHROPIC_BASE_URL");
if (!token) skip("缺少 AETHER_CLAUDE_TOKEN / ANTHROPIC_AUTH_TOKEN");

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const outDir = join(process.cwd(), "scripts", "test", ".tmp", "m2-02", `real-claude-${stamp}`);
mkdirSync(outDir, { recursive: true });
const settingsFile = join(outDir, "settings.json");
writeFileSync(
  settingsFile,
  JSON.stringify({
    env: {
      ANTHROPIC_BASE_URL: baseUrl,
      ANTHROPIC_AUTH_TOKEN: token,
      ANTHROPIC_MODEL: model,
      ANTHROPIC_DEFAULT_HAIKU_MODEL: model,
      ANTHROPIC_DEFAULT_SONNET_MODEL: model,
      ANTHROPIC_DEFAULT_OPUS_MODEL: model,
    },
    includeCoAuthoredBy: false,
    permissions: { allow: [], deny: [] },
  }),
  "utf8",
);

const child = spawn(
  adapter,
  [
    "--claude-bin",
    claudeBin,
    "--settings-file",
    settingsFile,
    "--workspace",
    outDir,
    "--model",
    model,
    "--tools",
    "none",
    "--permission-mode",
    "default",
    "--run-timeout-ms",
    "240000",
  ],
  { stdio: ["pipe", "pipe", "pipe"], windowsHide: true },
);

const pending = new Map();
const runWaiters = new Map();
const events = [];
let buffer = "";
let nextId = 1;

function sendRequest(method, params) {
  const id = nextId++;
  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject, method, timer: setTimeout(() => reject(new Error(`${method} 超时`)), 60_000) });
  });
}

child.stdout.setEncoding("utf8");
child.stdout.on("data", (chunk) => {
  buffer += chunk;
  let index = buffer.indexOf("\n");
  while (index >= 0) {
    const line = buffer.slice(0, index).replace(/\r$/, "");
    buffer = buffer.slice(index + 1);
    index = buffer.indexOf("\n");
    if (!line.trim()) continue;
    let frame;
    try {
      frame = JSON.parse(line);
    } catch {
      continue;
    }
    if (frame.id !== undefined && pending.has(frame.id)) {
      const waiter = pending.get(frame.id);
      pending.delete(frame.id);
      clearTimeout(waiter.timer);
      if (frame.error) waiter.reject(new Error(`${waiter.method} 错误 ${frame.error.code}: ${frame.error.message}`));
      else waiter.resolve(frame.result ?? {});
      continue;
    }
    if (frame.method === "event" && frame.params) {
      const envelope = frame.params;
      events.push(envelope);
      if (TERMINAL_TYPES.has(envelope.type) && envelope.run_id) {
        const waiter = runWaiters.get(envelope.run_id);
        if (waiter) {
          runWaiters.delete(envelope.run_id);
          waiter(envelope.type);
        }
      }
    }
  }
});
let stderr = "";
child.stderr.setEncoding("utf8");
child.stderr.on("data", (chunk) => {
  stderr = `${stderr}${chunk}`.slice(-4000);
});

function waitRun(runId, timeoutMs) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      runWaiters.delete(runId);
      reject(new Error(`run ${runId} 超时（无终态）`));
    }, timeoutMs);
    runWaiters.set(runId, (terminal) => {
      clearTimeout(timer);
      resolve(terminal);
    });
  });
}

const results = [];
let completed = 0;
let failedTerminal = 0;
let hangCount = 0;

try {
  await sendRequest("initialize", { config: {} });
  const created = await sendRequest("session.create", { title: "m2-02-real-rate" });
  const sessionId = created.session_id;
  console.log(`[m2-02 real] session=${sessionId} model=${model} runs=${RUNS}`);

  for (let index = 0; index < RUNS; index += 1) {
    const clientMsgId = `m2-02-real-${String(index).padStart(3, "0")}`;
    const startedAt = Date.now();
    try {
      const ack = await sendRequest("session.send", {
        session_id: sessionId,
        client_msg_id: clientMsgId,
        text: BASELINE_PROMPT,
      });
      const terminal = await waitRun(ack.run_id, 240_000);
      const runEvents = events.filter((event) => event.run_id === ack.run_id);
      const failedEvent = runEvents.find((event) => event.type === "run.failed");
      if (terminal === "run.completed") completed += 1;
      else failedTerminal += 1;
      results.push({
        index,
        runId: ack.run_id,
        terminal,
        wallMs: Date.now() - startedAt,
        error: failedEvent?.payload?.error ?? null,
      });
      const label = terminal === "run.completed" ? "completed" : `failed(${failedEvent?.payload?.error?.code ?? "?"})`;
      console.log(`[m2-02 real] run ${index + 1}/${RUNS}: ${label} (${Date.now() - startedAt}ms)`);
    } catch (error) {
      hangCount += 1;
      results.push({ index, terminal: "none", error: { code: "hang_or_timeout", message: String(error) } });
      console.error(`[m2-02 real] run ${index + 1}/${RUNS}: 无终态（${error}）`);
    }
  }
} finally {
  try {
    child.stdin.end();
    await new Promise((resolve) => child.once("close", resolve));
  } catch {
    child.kill();
  }
}

const rate = completed / RUNS;
const summary = {
  task: "M2-02 DoD4",
  model,
  runs: RUNS,
  completed,
  failedTerminal,
  hang: hangCount,
  completionRate: rate,
  threshold: 0.95,
  passed: rate >= 0.95 && hangCount === 0,
  results,
  stderrTail: stderr.split(/\r?\n/).slice(-20),
};
writeFileSync(join(outDir, "summary.json"), JSON.stringify(summary, null, 2), "utf8");
console.log(`\n[m2-02 real] 完成率 = ${completed}/${RUNS}（${(rate * 100).toFixed(1)}%）；失败终态 ${failedTerminal}；无终态 ${hangCount}`);
console.log(`[m2-02 real] 证据：${join(outDir, "summary.json")}`);
console.log(summary.passed ? "REAL-CLAUDE-50: PASS" : "REAL-CLAUDE-50: FAIL");
process.exit(summary.passed ? 0 : 1);
