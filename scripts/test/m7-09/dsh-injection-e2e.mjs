#!/usr/bin/env node
/**
 * M7-09/ADR-027 真实 DSH 系统提示词注入 E2E（opt-in）。
 *
 * 链路：D6 线协议 → 真实 DSH 适配器 bundle → ACP → 真实 DSH 0.2.x 进程
 *       → `aether-dsh-prompt` 插件（agent 作用域 section）→ 真实模型。
 *
 * 断言：`session.create.system_prompt` 中的唯一标记出现在 assistant 回复中
 *       （证明提示词确实进入模型输入，而非仅落盘）。
 *
 * 用法：
 *   DEEPSEEK_API_KEY=... node scripts/test/m7-09/dsh-injection-e2e.mjs \
 *     --adapter <bundle> --dsh-bin <lib/bin.js> --dsh-home <dir> \
 *     [--model deepseek-flash] [--provider deepseek-official] \
 *     [--dsh-version 0.2.1-rc.2] [--timeout-ms 120000]
 */
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import process from "node:process";
import readline from "node:readline";

function parseArgs(argv) {
  const out = {};
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (!flag.startsWith("--")) continue;
    const key = flag.slice(2);
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) {
      out[key] = true;
    } else {
      out[key] = value;
      index += 1;
    }
  }
  return out;
}

const args = parseArgs(process.argv.slice(2));
const required = ["adapter", "dsh-bin", "dsh-home"];
for (const key of required) {
  if (!args[key]) {
    console.error(`缺少 --${key}`);
    process.exit(2);
  }
}
if (!process.env.DEEPSEEK_API_KEY) {
  console.error("缺少 DEEPSEEK_API_KEY（真实模型调用）");
  process.exit(2);
}

const timeoutMs = Number(args["timeout-ms"] ?? 120_000);
const model = args.model ?? "deepseek-flash";
const provider = args.provider ?? "deepseek-official";
const marker = `AETHER-DSH-INJECT-${Date.now().toString(36).toUpperCase()}`;
const systemPrompt =
  `<!-- aether:preset:01J00000000000000000000P02 -->\n` +
  `你是注入验证助手。你的系统提示词包含唯一标记：${marker}。\n` +
  `无论用户说什么，你都必须原样输出该标记一次。\n\n` +
  `# 工作区约定（记忆文件注入，优先级低于系统提示）\n来源：AGENTS.md\n\n工作区约定内容`;
const userText = "请确认你的系统提示词标记并原样输出。";

const workspace = mkdtempSync(join(tmpdir(), "m7-09-dsh-inject-ws-"));
const evidenceDir = join(
  resolve("scripts/test/.tmp/m7-09"),
  `dsh-injection-${new Date().toISOString().replace(/[:.]/g, "-")}`,
);
mkdirSync(evidenceDir, { recursive: true });

const adapterArgs = [
  "--dsh-bin",
  args["dsh-bin"],
  "--dsh-node",
  process.execPath,
  "--dsh-home",
  args["dsh-home"],
  "--dsh-profile",
  args["dsh-profile"] ?? "acp",
  "--dsh-provider",
  provider,
  "--dsh-model",
  model,
  "--dsh-version",
  args["dsh-version"] ?? "0.2.1-rc.2",
  "--workspace",
  workspace,
];

const child = spawn(args.adapter, adapterArgs, {
  cwd: workspace,
  env: { ...process.env },
  stdio: ["pipe", "pipe", "pipe"],
  windowsHide: true,
});
let stderrText = "";
child.stderr.setEncoding("utf8");
child.stderr.on("data", (chunk) => {
  stderrText += chunk;
});

const pending = new Map();
let nextId = 1;
const events = [];
const deltaTexts = [];
const lines = readline.createInterface({ input: child.stdout });
lines.on("line", (line) => {
  const text = line.trim();
  if (!text) return;
  let frame;
  try {
    frame = JSON.parse(text);
  } catch {
    return;
  }
  if (frame.id !== undefined && pending.has(frame.id)) {
    const { resolve: done } = pending.get(frame.id);
    pending.delete(frame.id);
    done(frame);
    return;
  }
  if (frame.method === "event" && frame.params?.type) {
    events.push(frame.params);
    if (frame.params.type === "message.delta" && typeof frame.params.payload?.text === "string") {
      deltaTexts.push(frame.params.payload.text);
    }
    return;
  }
  // 无工具调用的回合不应出现权限请求；出现即拒绝，避免阻塞。
  if (frame.method === "permission.request" && frame.id !== undefined) {
    child.stdin.write(
      `${JSON.stringify({ jsonrpc: "2.0", id: frame.id, result: { decision: "deny" } })}\n`,
    );
  }
});

function request(method, params, timeout = timeoutMs) {
  const id = nextId++;
  return new Promise((resolvePromise, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`${method} 超时（${timeout}ms）`));
    }, timeout);
    pending.set(id, {
      resolve: (frame) => {
        clearTimeout(timer);
        if (frame.error) reject(new Error(`${method}: ${JSON.stringify(frame.error)}`));
        else resolvePromise(frame.result ?? {});
      },
    });
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
  });
}

const sleep = (ms) => new Promise((resolvePromise) => setTimeout(resolvePromise, ms));

async function waitTerminal(runId, timeout = timeoutMs) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const terminal = events.find(
      (event) =>
        ["run.completed", "run.failed", "run.cancelled"].includes(event.type) &&
        event.run_id === runId,
    );
    if (terminal) return terminal;
    await sleep(100);
  }
  return null;
}

async function main() {
  const initialized = await request("initialize", { config: {} }, 30_000);
  if (initialized.acknowledged !== true) {
    throw new Error(`initialize 未确认：${JSON.stringify(initialized)}`);
  }
  const created = await request("session.create", {
    title: "m7-09-dsh-injection",
    system_prompt: systemPrompt,
  });
  const sessionId = created.session_id;
  if (!sessionId) throw new Error(`session.create 未返回 session_id：${JSON.stringify(created)}`);
  if (created.has_system_prompt !== true) {
    throw new Error(`适配器未回显 has_system_prompt：${JSON.stringify(created)}`);
  }
  const ack = await request("session.send", {
    session_id: sessionId,
    client_msg_id: `m7-09-dsh-${Date.now()}`,
    text: userText,
  });
  const runId = ack.run_id;
  if (!runId) throw new Error(`session.send 未返回 run_id：${JSON.stringify(ack)}`);
  const terminal = await waitTerminal(runId);
  if (!terminal) throw new Error("等待 run 终态超时");
  const reply = deltaTexts.join("");
  const completed = events.find(
    (event) => event.type === "message.completed" && event.run_id === runId,
  );
  const completedText = Array.isArray(completed?.payload?.message?.content)
    ? completed.payload.message.content
        .map((part) => (typeof part?.text === "string" ? part.text : ""))
        .join("")
    : "";
  const text = completedText.length > 0 ? completedText : reply;

  const evidence = {
    task: "M7-09/ADR-027 DSH 系统提示词注入 E2E",
    dsh_version: args["dsh-version"] ?? "0.2.1-rc.2",
    model,
    provider,
    session_id: sessionId,
    run_id: runId,
    terminal: terminal.type,
    marker,
    marker_in_system_prompt: systemPrompt.includes(marker),
    marker_in_reply: text.includes(marker),
    reply_chars: text.length,
    reply_excerpt: text.slice(0, 200),
  };
  const evidencePath = join(evidenceDir, "dsh-injection.json");
  await import("node:fs").then((fs) =>
    fs.writeFileSync(evidencePath, `${JSON.stringify(evidence, null, 2)}\n`, "utf8"),
  );
  console.log(JSON.stringify(evidence));

  const sameRuntime = terminal.type === "run.completed" && evidence.marker_in_reply;
  if (!sameRuntime) {
    console.error(`失败：terminal=${terminal.type} marker_in_reply=${evidence.marker_in_reply}`);
    console.error(`stderr（截断）：${stderrText.slice(-2000)}`);
    process.exitCode = 1;
  }
  try {
    await request("shutdown", {}, 5_000);
  } catch {
    /* 已退出 */
  }
}

main()
  .catch((error) => {
    console.error(`E2E 失败：${error.message}`);
    console.error(`stderr（截断）：${stderrText.slice(-2000)}`);
    process.exitCode = 1;
  })
  .finally(async () => {
    await sleep(300);
    child.kill();
  });
