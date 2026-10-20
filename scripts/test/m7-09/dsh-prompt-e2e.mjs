#!/usr/bin/env node
/**
 * M7-09/ADR-027：DSH 系统提示词插件注入真实 E2E（opt-in）。
 *
 * 链路（与生产同口径）：
 *   Aether 线协议（D6）→ 编译后的 DSH 适配器 → 真实 DSH 0.2.1-rc.2（`--profile acp --patch`）
 *   → `aether-dsh-prompt` 插件（agent 作用域 section）→ 模型请求 system 消息。
 *
 * 观测点：
 *   1. 适配器把 `session.create.system_prompt` 写入侧信道文件；
 *   2. 插件在真实 agent 回合组装时把该提示词注入 system 消息；
 *   3. 本地记录型 OpenAI 兼容上游收到请求，断言 marker 出现在 system 消息内，
 *      且位于 harness 身份之后（persona/plan/tools 之间）；
 *   4. 未携带 system_prompt 的会话不注入；dispose 清理侧信道文件。
 *
 * 不访问外网、不需要模型凭据：本地 HTTP 服务返回确定性 SSE。
 *
 * 用法（由 verify-m7-09 编排）：
 *   node scripts/test/m7-09/dsh-prompt-e2e.mjs \
 *     --adapter <aether-dsh-adapter.exe> \
 *     --dsh-bin <.../@deepseek-ai/dsh/lib/bin.js> \
 *     [--evidence-dir <dir>] [--timeout-ms 180000]
 */
import { execFileSync, spawn } from "node:child_process";
import { createServer } from "node:http";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import process from "node:process";
import readline from "node:readline";

import { repoRoot } from "../lib/exec.mjs";

const args = parseArgs(process.argv.slice(2));
if (!args.adapter || !args["dsh-bin"]) {
  console.error("用法：--adapter <bundle> --dsh-bin <lib/bin.js> [--evidence-dir <dir>]");
  process.exit(2);
}

const timeoutMs = Number(args["timeout-ms"] ?? 180_000);
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir =
  args["evidence-dir"] ??
  path.join(repoRoot, "scripts", "test", ".tmp", "m7-09", `dsh-prompt-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });
const home = mkdtempSync(path.join(tmpdir(), "aether-dsh-prompt-e2e-"));
const workspace = path.join(home, "workspace");
mkdirSync(workspace, { recursive: true });
const evidencePath = path.join(evidenceDir, "dsh-prompt-e2e.json");
const requestsPath = path.join(evidenceDir, "upstream-requests.jsonl");

const MARKER = "AETHER-M7-09-DSH-PROMPT-MARKER";
const SYSTEM_PROMPT = `<!-- aether:preset:01J00000000000000000000P02 -->\n你是资深软件工程师，专注于代码实现、重构与调试。\n\n# 工作区约定（记忆文件注入，优先级低于系统提示）\n来源：AGENTS.md\n\n工作区约定内容\n\n模板字面量：{{aether_literal}}`;
const EVIDENCE = {
  runtime: "deepseek-harness",
  dshVersion: null,
  adapter: args.adapter,
  dshBin: args["dsh-bin"],
  home,
  systemPrompt: SYSTEM_PROMPT,
  checks: {},
  upstreamRequests: 0,
  startedAt: new Date().toISOString(),
};
let activeClient = null;

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

function pass(name, detail = "") {
  EVIDENCE.checks[name] = { status: "PASS", detail };
  console.log(`PASS  ${name}${detail ? `（${detail}）` : ""}`);
}

function fail(name, detail) {
  EVIDENCE.checks[name] = { status: "FAIL", detail };
  console.error(`FAIL  ${name}：${detail}`);
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

/** 本地 OpenAI 兼容上游：记录请求体并返回确定性 SSE（不访问外网）。 */
function startUpstream() {
  const bodies = [];
  const server = createServer((request, response) => {
    if (request.method === "GET" && request.url?.endsWith("/models")) {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          object: "list",
          data: [{ id: "deepseek-flash", object: "model", owned_by: "aether-e2e" }],
        }),
      );
      return;
    }
    let raw = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => {
      raw += chunk;
    });
    request.on("end", () => {
      let body = null;
      try {
        body = raw.length > 0 ? JSON.parse(raw) : null;
      } catch {
        body = { __unparsed: raw.slice(0, 2000) };
      }
      bodies.push({
        url: request.url,
        method: request.method,
        headers: {
          authorization: request.headers.authorization ? "<present>" : null,
          "content-type": request.headers["content-type"] ?? null,
        },
        body,
        at: new Date().toISOString(),
      });
      EVIDENCE.upstreamRequests = bodies.length;
      writeFileSync(requestsPath, bodies.map((item) => JSON.stringify(item)).join("\n") + "\n", "utf8");
      const model = body?.model ?? "deepseek-flash";
      const chunk = (payload) => `data: ${JSON.stringify(payload)}\n\n`;
      response.writeHead(200, {
        "content-type": "text/event-stream",
        "cache-control": "no-cache",
        connection: "keep-alive",
      });
      response.write(
        chunk({
          id: "chatcmpl-aether-e2e",
          object: "chat.completion.chunk",
          created: Math.floor(Date.now() / 1000),
          model,
          choices: [{ index: 0, delta: { role: "assistant", content: MARKER }, finish_reason: null }],
        }),
      );
      response.write(
        chunk({
          id: "chatcmpl-aether-e2e",
          object: "chat.completion.chunk",
          created: Math.floor(Date.now() / 1000),
          model,
          choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
          usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
        }),
      );
      response.write("data: [DONE]\n\n");
      response.end();
    });
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      resolve({ server, port: address.port, bodies });
    });
  });
}

/** 极简线协议客户端（同 m2-11 驱动口径，裁剪到本用例所需）。 */
class WireClient {
  constructor(command, commandArgs, env) {
    this.command = command;
    this.commandArgs = commandArgs;
    this.env = env;
    this.frames = [];
    this.pending = new Map();
    this.nextId = 1;
    this.child = null;
    this.closed = false;
    this.stderr = "";
  }

  start() {
    this.child = spawn(this.command, this.commandArgs, {
      env: { ...process.env, ...this.env },
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    const stdout = readline.createInterface({ input: this.child.stdout });
    stdout.on("line", (line) => this.onLine(line));
    this.child.stderr.on("data", (chunk) => {
      this.stderr = `${this.stderr}${chunk.toString("utf8")}`.slice(-12_000);
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

  async waitFrame(predicate, timeout = 20_000) {
    const deadline = Date.now() + timeout;
    for (;;) {
      const found = this.frames.find(predicate);
      if (found) return found;
      if (Date.now() > deadline) throw new Error("等待帧超时");
      await sleep(50);
    }
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

  async stop() {
    try {
      await this.request("shutdown", {}, 5000);
    } catch {
      /* 已退出 */
    }
    const deadline = Date.now() + 8000;
    while (!this.closed && Date.now() < deadline) await sleep(50);
    if (!this.closed) this.child.kill();
  }
}

function systemMessages(body) {
  const messages = Array.isArray(body?.messages) ? body.messages : [];
  return messages.filter((message) => message?.role === "system");
}

function systemText(body) {
  return systemMessages(body)
    .map((message) =>
      typeof message.content === "string"
        ? message.content
        : Array.isArray(message.content)
          ? message.content.map((part) => part?.text ?? "").join("")
          : "",
    )
    .join("\n");
}

/**
 * 预热 DSH ACP 启动链路：用空 patch 启动一次 ACP app，发送 `initialize` 并等待
 * 响应后立即回收。目的仅为测试环境减少首次模块加载/杀毒扫描对适配器固定 10s
 * `initialize` 预算的挤压；不改变生产 D6 超时口径，也不注入任何 Aether 插件。
 */
function prewarmDsh() {
  return new Promise((resolve) => {
    const patch = path.join(home, "aether-e2e-warmup.patch.yml");
    writeFileSync(patch, "[]\n", "utf8");
    const child = spawn(
      process.execPath,
      [args["dsh-bin"], "--profile", "acp", "--patch", patch],
      {
        cwd: workspace,
        env: { ...process.env, DSH_HOME: home },
        stdio: ["pipe", "pipe", "pipe"],
        windowsHide: true,
      },
    );
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      try {
        child.kill();
      } catch {
        /* 已退出 */
      }
      resolve();
    };
    const timer = setTimeout(finish, 45_000);
    child.on("error", finish);
    child.on("close", finish);
    const stdout = readline.createInterface({ input: child.stdout });
    stdout.on("line", (line) => {
      if (line.includes('"id":1')) finish();
    });
    child.stdin.write(
      `${JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: 1,
          clientCapabilities: {},
          clientInfo: { name: "aether-e2e-warmup", version: "0" },
        },
      })}\n`,
    );
  });
}

async function main() {
  // 预创建 profile（不启动 ACP app），降低全新 DSH_HOME 首次冷启动对 10s
  // `initialize` 预算的挤压（测试环境节流；生产 D6 超时口径不变）。
  try {
    execFileSync(process.execPath, [args["dsh-bin"], "--profile", "acp", "--dump-config"], {
      env: { ...process.env, DSH_HOME: home },
      stdio: "ignore",
      timeout: 30_000,
      windowsHide: true,
    });
  } catch {
    /* 预创建失败由后续真实启动给出诊断 */
  }
  // 预热 ACP app 模块链（空 patch；响应后立即回收），避免全量回归负载下的
  // 10s initialize 冷启动抖动（生产超时口径不变）。
  await prewarmDsh();
  const upstream = await startUpstream();
  const adapterArgs = [
    "--dsh-bin",
    args["dsh-bin"],
    "--dsh-node",
    process.execPath,
    "--dsh-home",
    home,
    "--dsh-profile",
    "acp",
    "--workspace",
    workspace,
    "--hello-timeout-ms",
    "30000",
  ];
  const env = {
    AETHER_PROVIDER_BASE_URL: `http://127.0.0.1:${upstream.port}/v1`,
    AETHER_PROVIDER_ID: "01J00000000000000000000M79",
    AETHER_PROVIDER_ENV_KEY: "AETHER_DSH_E2E_KEY",
    AETHER_PROVIDER_MODEL: "deepseek-flash",
    AETHER_PROVIDER_MODELS: JSON.stringify([{ id: "deepseek-flash", name: "deepseek-flash" }]),
    AETHER_DSH_E2E_KEY: "sk-aether-e2e",
  };
  const client = new WireClient(args.adapter, adapterArgs, env).start();
  activeClient = client;
  try {
    const hello = await client.waitFrame((frame) => frame.method === "hello", 30_000);
    assert(hello.params?.protocol === "1.0", `hello.protocol=${hello.params?.protocol}`);

    const initialized = await client.request("initialize", { config: {} });
    assert(initialized?.acknowledged === true, "initialize 未确认");
    EVIDENCE.dshVersion = initialized?.dsh_version ?? null;
    assert(
      initialized?.plugin_contract === "aether-dsh-stream@1",
      `插件契约帧不符：${initialized?.plugin_contract}`,
    );
    pass("initialize + 插件注入", `dsh=${initialized?.dsh_version ?? "?"}`);

    // 1) 携带 system_prompt：侧信道写入 + 真实回合注入 system 消息。
    const created = await client.request("session.create", {
      title: "m7-09-dsh-prompt",
      system_prompt: SYSTEM_PROMPT,
    });
    assert(created?.has_system_prompt === true, "session.create 未回显 has_system_prompt");
    assert(created?.system_prompt_effective === true, "写入成功但生效位不是 true");
    const sessionId = String(created.session_id);
    const promptFile = path.join(
      home,
      "aether-bridge",
      "prompts",
      `${encodeURIComponent(sessionId)}.md`,
    );
    assert(existsSync(promptFile), `侧信道文件不存在：${promptFile}`);
    assert(readFileSync(promptFile, "utf8") === SYSTEM_PROMPT, "侧信道内容与线协议载荷不一致");
    EVIDENCE.promptFile = promptFile;
    pass("侧信道写入", `session=${sessionId}`);

    const send = await client.request("session.send", {
      session_id: sessionId,
      client_msg_id: `m7-09-dsh-${Date.now()}`,
      text: "Reply with the requested marker only.",
    });
    const terminal = await client.waitTerminal(send.run_id);
    assert(terminal !== null, "run 未到达终态");
    assert(
      terminal.type === "run.completed",
      `run 终态=${terminal.type} ${JSON.stringify(terminal.payload ?? {}).slice(0, 500)}`,
    );
    await waitFor(() => upstream.bodies.length >= 1, 10_000, "上游请求未到达");
    const requestBody = upstream.bodies[upstream.bodies.length - 1]?.body;
    const system = systemText(requestBody);
    assert(system.includes("<!-- aether:preset:01J00000000000000000000P02 -->"), "system 消息缺少预设块");
    assert(system.includes("你是资深软件工程师"), "system 消息缺少预设正文");
    assert(system.includes("# 工作区约定"), "system 消息缺少记忆块");
    // interpolate: false：模板字面量必须原样保留（默认插值会因未知变量使组装失败）。
    assert(system.includes("{{aether_literal}}"), "system 消息未保留 {{...}} 字面量");
    const presetIndex = system.indexOf("<!-- aether:preset:");
    const memoryIndex = system.indexOf("# 工作区约定");
    assert(presetIndex >= 0 && memoryIndex > presetIndex, "预设块必须先于记忆块");
    const personaIndex = system.indexOf("You are an AI agent powered by DeepSeek Harness.");
    assert(personaIndex >= 0 && personaIndex < presetIndex, "section 必须位于 harness 身份之后");
    EVIDENCE.modelRequest = {
      url: upstream.bodies[upstream.bodies.length - 1]?.url,
      systemChars: system.length,
      presetIndex,
      memoryIndex,
      personaIndex,
    };
    pass("真实 DSH 回合注入 system 消息", `system=${system.length}B`);

    await client.request("session.dispose", { session_id: sessionId });
    assert(!existsSync(promptFile), "dispose 后侧信道文件未删除");
    pass("dispose 清理侧信道");

    // 2) 未携带 system_prompt：不写文件、不注入。
    const plain = await client.request("session.create", { title: "m7-09-dsh-plain" });
    assert(plain?.has_system_prompt === false, "未携带提示词却回显注入");
    assert(plain?.system_prompt_effective === null, "未携带提示词生效位不是 null");
    EVIDENCE.systemPromptEffective = {
      dispatched: created?.system_prompt_effective,
      plain: plain?.system_prompt_effective,
    };
    const plainFile = path.join(
      home,
      "aether-bridge",
      "prompts",
      `${encodeURIComponent(String(plain.session_id))}.md`,
    );
    assert(!existsSync(plainFile), "未携带提示词却写入侧信道文件");
    const plainSend = await client.request("session.send", {
      session_id: String(plain.session_id),
      client_msg_id: `m7-09-dsh-plain-${Date.now()}`,
      text: "Reply with the requested marker only.",
    });
    const plainTerminal = await client.waitTerminal(plainSend.run_id);
    assert(plainTerminal?.type === "run.completed", "无提示词会话 run 未完成");
    await waitFor(
      () => upstream.bodies.length >= 2,
      10_000,
      "无提示词会话的上游请求未到达",
    );
    const plainSystem = systemText(upstream.bodies[1]?.body);
    assert(!plainSystem.includes("<!-- aether:preset:"), "无提示词会话被注入了预设块");
    pass("未携带提示词不注入", `system=${plainSystem.length}B`);

    EVIDENCE.ok = true;
  } finally {
    await client.stop();
    await new Promise((resolve) => upstream.server.close(resolve));
    EVIDENCE.finishedAt = new Date().toISOString();
    writeFileSync(evidencePath, `${JSON.stringify(EVIDENCE, null, 2)}\n`, "utf8");
    console.log(`[m7-09] DSH prompt E2E 证据：${evidencePath}`);
  }
}

async function waitFor(predicate, timeout, message) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await sleep(50);
  }
  throw new Error(message);
}

main().catch((error) => {
  fail("DSH prompt E2E", error instanceof Error ? error.message : String(error));
  EVIDENCE.ok = false;
  EVIDENCE.error = error instanceof Error ? error.message : String(error);
  EVIDENCE.adapterStderrTail = activeClient?.stderr ?? "";
  EVIDENCE.finishedAt = new Date().toISOString();
  writeFileSync(evidencePath, `${JSON.stringify(EVIDENCE, null, 2)}\n`, "utf8");
  console.error(`[m7-09] 适配器 stderr 尾部：\n${EVIDENCE.adapterStderrTail}`);
  console.error(`[m7-09] 失败证据：${evidencePath}`);
  process.exitCode = 1;
});
