/**
 * M7-03 真实运行时暂停抽检（ADR-017 DoD2「真实运行时抽检」；opt-in）。
 *
 * 直接 spawn 编译后的适配器单文件，经 Aether 线协议（D6）驱动真实运行时：
 * 1) hello/initialize → 断言声明 `pause_resume`（ADR-017 附录 A-5）；
 * 2) session.create → 记住口令 → run.completed；
 * 3) `session.pause`（D6 净 +1）→ `{paused: true, cancelled_runs: []}`（无在途 run）；
 * 4) 同会话继续问 recall → 命中原生上下文（暂停不破坏 native 上下文；
 *    跨进程 Mode R 恢复由 M2-11 真实证据覆盖）；
 * 5) 输出 summary.json（PASS/FAIL 逐步骤）。
 *
 * 用法（opt-in；缺凭证/未安装运行时须显式跳过，禁止静默通过）：
 *   node scripts/test/m7-03/real-pause-smoke.mjs --runtime dsh \
 *     --adapter <编译产物> --dsh-bin <lib/bin.js> --dsh-home <dir> \
 *     [--dsh-profile acp] [--dsh-provider <p>] [--dsh-model <m>] [--dsh-version <v>]
 *   node scripts/test/m7-03/real-pause-smoke.mjs --runtime codex \
 *     --adapter <编译产物> [--codex-bin codex] [--codex-home <dir>] [--model <m>]
 */
import { spawn } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import readline from "node:readline";

import { repoRoot } from "../lib/exec.mjs";

const args = parseArgs(process.argv.slice(2));
const runtime = args.runtime;
if (runtime !== "dsh" && runtime !== "codex") {
  console.error("用法：--runtime dsh|codex");
  process.exit(2);
}
if (!args.adapter) {
  console.error("缺少 --adapter <编译产物>");
  process.exit(2);
}
if (runtime === "dsh" && (!args["dsh-bin"] || !args["dsh-home"])) {
  console.error("dsh 模式需要 --dsh-bin 与 --dsh-home");
  process.exit(2);
}
const timeoutMs = Number(args["timeout-ms"] ?? 180_000);
const passphrase = `AETHER-M7-03-REAL-${Date.now().toString(36).toUpperCase()}`;

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m7-03", `real-${runtime}-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });
const workspaceDir = path.join(evidenceDir, "workspace");
mkdirSync(workspaceDir, { recursive: true });

function parseArgs(argv) {
  const out = {};
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (!flag.startsWith("--")) continue;
    const key = flag.slice(2);
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) out[key] = true;
    else {
      out[key] = value;
      index += 1;
    }
  }
  return out;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, Math.max(0, ms)));
}

/** 极简线协议客户端（JSON-RPC over JSON-Lines；单连接单适配器进程）。 */
class WireClient {
  constructor(adapterArgs, env) {
    this.adapterArgs = adapterArgs;
    this.env = env;
    this.frames = [];
    this.pending = new Map();
    this.nextId = 1;
    this.child = null;
    this.closed = false;
    this.stderrTail = "";
    this.permissionRequests = [];
  }

  start() {
    this.child = spawn(this.adapterArgs[0], this.adapterArgs.slice(1), {
      env: { ...process.env, ...this.env },
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    const stdout = readline.createInterface({ input: this.child.stdout });
    stdout.on("line", (line) => this.onLine(line));
    this.child.stderr.on("data", (chunk) => {
      this.stderrTail = `${this.stderrTail}${chunk.toString("utf8")}`.slice(-8000);
    });
    this.child.once("close", () => {
      this.closed = true;
      for (const pending of this.pending.values()) pending.reject(new Error("适配器进程退出"));
      this.pending.clear();
    });
    return this;
  }

  onLine(line) {
    let frame;
    try {
      frame = JSON.parse(line);
    } catch {
      return;
    }
    this.frames.push(frame);
    if (frame.id !== undefined && this.pending.has(frame.id)) {
      const { resolve, reject, timer } = this.pending.get(frame.id);
      this.pending.delete(frame.id);
      clearTimeout(timer);
      if (frame.error) reject(new Error(`RPC ${frame.error.code}: ${frame.error.message}`));
      else resolve(frame.result);
      return;
    }
    if (frame.method === "permission.request") {
      this.permissionRequests.push(frame.params);
      // 自动放行（真实运行时可能触发工具审批；本抽检不做权限断言）。
      void this.request("permission.resolve", {
        request_id: frame.params?.request_id,
        decision: "allow",
      }).catch(() => undefined);
    }
  }

  request(method, params, timeout = timeoutMs) {
    if (this.closed) return Promise.reject(new Error("连接已关闭"));
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${method} 超时（${timeout}ms）`));
      }, timeout);
      this.pending.set(id, { resolve, reject, timer });
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    });
  }

  events(runId) {
    return this.frames
      .filter((frame) => frame.method === "event")
      .map((frame) => frame.params)
      .filter((envelope) => runId === undefined || envelope.run_id === runId);
  }

  async waitTerminal(runId, timeout = timeoutMs) {
    const deadline = Date.now() + timeout;
    for (;;) {
      const terminal = this.events(runId).find((envelope) =>
        ["run.completed", "run.failed", "run.cancelled"].includes(envelope.type),
      );
      if (terminal) return terminal;
      if (Date.now() > deadline) return null;
      await sleep(100);
    }
  }

  completedText(runId) {
    const completed = this.events(runId).find((envelope) => envelope.type === "message.completed");
    return completed?.payload?.message?.content ?? "";
  }

  async stop(timeout = 8000) {
    try {
      await this.request("shutdown", {}, 5000);
    } catch {
      /* 已退出 */
    }
    const deadline = Date.now() + timeout;
    while (!this.closed && Date.now() < deadline) await sleep(50);
    if (!this.closed) this.child.kill();
  }
}

function adapterArgsFor() {
  if (runtime === "dsh") {
    return [
      "--dsh-bin",
      args["dsh-bin"],
      "--dsh-node",
      process.execPath,
      "--dsh-home",
      args["dsh-home"],
      "--dsh-profile",
      args["dsh-profile"] ?? "acp",
      ...(args["dsh-provider"] ? ["--dsh-provider", args["dsh-provider"]] : []),
      ...(args["dsh-model"] ? ["--dsh-model", args["dsh-model"]] : []),
      ...(args["dsh-version"] ? ["--dsh-version", args["dsh-version"]] : []),
      ...(args["dsh-provider-config"] ? ["--dsh-provider-config", args["dsh-provider-config"]] : []),
      "--workspace",
      workspaceDir,
    ];
  }
  return [
    "--codex-bin",
    args["codex-bin"] ?? "codex",
    ...(args["codex-home"] ? ["--codex-home", args["codex-home"]] : []),
    ...(args.model ? ["--model", args.model] : []),
    "--sandbox",
    args.sandbox ?? "read-only",
    "--reasoning",
    args.reasoning ?? "low",
    "--workspace",
    workspaceDir,
  ];
}

const summary = {
  task: "M7-03",
  runtime,
  adapter: args.adapter,
  startedAt: new Date().toISOString(),
  environment: {
    dshHome: args["dsh-home"] ?? null,
    dshProvider: args["dsh-provider"] ?? null,
    model: args.model ?? args["dsh-model"] ?? "(runtime default)",
  },
  steps: {},
};

const ok = (name, detail = "") => {
  summary.steps[name] = { status: "PASS", detail };
  console.log(`PASS  ${name}${detail ? `（${detail}）` : ""}`);
};
const fail = (name, detail) => {
  summary.steps[name] = { status: "FAIL", detail };
  console.error(`FAIL  ${name}：${detail}`);
};

async function main() {
  const client = new WireClient(
    [args.adapter, ...adapterArgsFor()],
    runtime === "codex" && args["codex-home"] ? { CODEX_HOME: args["codex-home"] } : {},
  ).start();
  try {
    // 1) 握手 + 能力声明。
    const deadline = Date.now() + 15_000;
    let hello = null;
    while (Date.now() < deadline && !hello) {
      hello = client.frames.find((frame) => frame.method === "hello") ?? null;
      if (!hello) await sleep(50);
    }
    if (!hello) throw new Error("hello 未在 15s 内到达");
    const capabilities = hello.params?.runtime?.capabilities ?? [];
    if (!capabilities.includes("pause_resume")) {
      throw new Error(`hello 未声明 pause_resume：${JSON.stringify(capabilities)}`);
    }
    const initialized = await client.request("initialize", { config: {} });
    if (initialized?.acknowledged !== true) throw new Error("initialize 未确认");
    ok("能力声明 pause_resume", `runtime=${hello.params?.runtime?.name}`);

    // 2) 记住口令（建立原生会话上下文）。
    const session = await client.request("session.create", { title: "m7-03-real-pause" });
    const remember = await client.request("session.send", {
      session_id: session.session_id,
      client_msg_id: `m7-03-${runtime}-remember`,
      text: `Remember this passphrase: ${passphrase}. Reply with exactly: STORED`,
    });
    const rememberTerminal = await client.waitTerminal(remember.run_id);
    if (rememberTerminal?.type !== "run.completed") {
      throw new Error(`remember run 终态=${rememberTerminal?.type}`);
    }
    ok("记忆建立", "run.completed");

    // 3) D6 `session.pause`（无在途 run；best-effort 收尾调用）。
    const paused = await client.request("session.pause", { session_id: session.session_id });
    if (paused?.paused !== true) {
      throw new Error(`session.pause 响应异常：${JSON.stringify(paused)}`);
    }
    ok("session.pause", `cancelled_runs=${JSON.stringify(paused.cancelled_runs ?? [])}`);

    // 4) 同会话 recall：暂停不得破坏原生上下文（核心恢复重放依赖 native 会话）。
    const recall = await client.request("session.send", {
      session_id: session.session_id,
      client_msg_id: `m7-03-${runtime}-recall`,
      text: "What passphrase did I ask you to remember? Reply with just the passphrase.",
    });
    const recallTerminal = await client.waitTerminal(recall.run_id);
    const recallText = client.completedText(recall.run_id);
    if (recallTerminal?.type !== "run.completed" || !recallText.includes(passphrase)) {
      throw new Error(
        `暂停后 recall 未命中原生上下文：terminal=${recallTerminal?.type} text=${recallText.slice(0, 160)}`,
      );
    }
    ok("暂停后 recall 命中原生上下文", passphrase);
  } catch (error) {
    fail("real-pause-smoke", error instanceof Error ? error.message : String(error));
    console.error(`[real ${runtime}] 适配器 stderr 尾部：\n${client.stderrTail.slice(-2000)}`);
    summary.adapterStderrTail = client.stderrTail.slice(-4000);
    summary.finishedAt = new Date().toISOString();
    writeFileSync(path.join(evidenceDir, "summary.json"), JSON.stringify(summary, null, 2), "utf8");
    await client.stop(3000).catch(() => undefined);
    process.exit(1);
  }
  summary.finishedAt = new Date().toISOString();
  writeFileSync(path.join(evidenceDir, "summary.json"), JSON.stringify(summary, null, 2), "utf8");
  console.log(`[real ${runtime}] 证据：${path.relative(repoRoot, path.join(evidenceDir, "summary.json"))}`);
  await client.stop(3000).catch(() => undefined);
  process.exitCode = 0;
}

void main();
