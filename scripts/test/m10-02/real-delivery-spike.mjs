#!/usr/bin/env node
/**
 * M10-02 真实 CLI 交付模式验证（ADR-032 v1.2 §D-032-4）。
 *
 * 使用本机真实 CLI 二进制 + 本地 mock 上游（不消耗真实凭据、不落密钥）：
 * - Claude Code：`claude -p --resume`，验证 message_append 追加历史、system 不变；
 * - Codex CLI：`codex exec resume`，验证 message_append 追加，developer 指令不变；
 * - DSH CLI：`dsh headless --session-id`，验证 message_append 追加历史；
 * - 三者均验证 origin_fork 的新 ephemeral 会话不包含父 live intervening turns。
 *
 * 运行：node scripts/test/m10-02/real-delivery-spike.mjs
 * 输出：scripts/test/m10-02/.tmp/q5-real-delivery/results.json
 */
import { createHash, randomUUID } from "node:crypto";
import { createServer } from "node:http";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { spawn, spawnSync } from "node:child_process";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const tmpRoot = path.join(repoRoot, "scripts", "test", "m10-02", ".tmp", "q5-real-delivery");
rmSync(tmpRoot, { recursive: true, force: true });
mkdirSync(tmpRoot, { recursive: true });

const results = {
  schema: "aether.m10-02.q5-real-delivery-spike.v1",
  started_at: new Date().toISOString(),
  claude: null,
  codex: null,
  dsh: null,
  checks: [],
};
const checks = [];
function check(name, ok, detail = "") {
  checks.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? ` :: ${detail}` : ""}`);
}

function sha(text) {
  return createHash("sha256").update(String(text)).digest("hex");
}
function textsOf(content) {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((part) => (typeof part === "string" ? part : part?.text ?? ""))
      .join("\n");
  }
  return "";
}
function countMatches(text, needle) {
  return [...String(text).matchAll(new RegExp(needle.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "g"))]
    .length;
}
function quoteCmdArg(value) {
  const text = String(value);
  if (!/[\s"&|<>^]/.test(text)) return text;
  return `"${text.replace(/"/g, '\\"')}"`;
}
function findCodexJs() {
  const where = spawnSync(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", "where codex"], {
    encoding: "utf8",
  });
  const cmdPath = String(where.stdout ?? "")
    .split(/\r?\n/)
    .find((line) => line.toLowerCase().endsWith(".cmd"));
  if (!cmdPath) return null;
  const text = readFileSync(cmdPath, "utf8");
  const match = text.match(
    /["']([^"']*node_modules[\\/]@openai[\\/]codex[\\/]bin[\\/]codex\.js)["']/i,
  );
  if (!match) return null;
  return match[1].replace(/%dp0%/gi, path.dirname(cmdPath));
}
function parseJsonLines(text) {
  const items = [];
  for (const line of String(text).split(/\r?\n/)) {
    if (!line.trim()) continue;
    try {
      items.push(JSON.parse(line));
    } catch {
      /* ignore */
    }
  }
  return items;
}

function runClaude(args, env, stdinPrompt) {
  const command = ["claude", ...args].map(quoteCmdArg).join(" ");
  return new Promise((resolve) => {
    const child = spawn(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", command], {
      cwd: tmpRoot,
      env: { ...process.env, ...env },
      windowsHide: true,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    if (stdinPrompt !== undefined) {
      child.stdin.write(stdinPrompt);
    }
    child.stdin.end();
    const timer = setTimeout(() => {
      spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], {
        windowsHide: true,
        stdio: "ignore",
      });
    }, 90_000);
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr });
    });
  });
}

function runCodex(codexJs, args, env, prompt) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [codexJs, ...args], {
      cwd: tmpRoot,
      env: { ...process.env, ...env },
      windowsHide: true,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.stdin.write(`${prompt}\n`);
    child.stdin.end();
    const timer = setTimeout(() => {
      spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], {
        windowsHide: true,
        stdio: "ignore",
      });
    }, 90_000);
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr });
    });
  });
}

function findDshJs() {
  const where = spawnSync(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", "where dsh"], {
    encoding: "utf8",
  });
  const cmdPath = String(where.stdout ?? "")
    .split(/\r?\n/)
    .find((line) => line.toLowerCase().endsWith(".cmd"));
  if (cmdPath) {
    const text = readFileSync(cmdPath, "utf8");
    const match = text.match(/["']([^"']*node_modules[\\/]@deepseek-ai[\\/]dsh[\\/]lib[\\/]bin\.js)["']/i);
    if (match) return match[1].replace(/%dp0%/gi, path.dirname(cmdPath));
  }
  const fallback = path.join(
    process.env.APPDATA ?? "",
    "npm",
    "node_modules",
    "@deepseek-ai",
    "dsh",
    "lib",
    "bin.js",
  );
  return fallback;
}

function runDsh(args, env) {
  const dshJs = findDshJs();
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [dshJs, ...args], {
      cwd: tmpRoot,
      env: { ...process.env, ...env },
      windowsHide: true,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    const timer = setTimeout(() => {
      spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], {
        windowsHide: true,
        stdio: "ignore",
      });
    }, 180_000);
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr });
    });
  });
}

function envelope(version, marker) {
  return `<aether:subagent-results version="${version}">{"source":"subagent","verdict":"approve","summary":"${marker}","result_refs":["subagent:${marker}"]}</aether:subagent-results>`;
}

// ===== 共用 mock 上游 =====
const requests = [];
let currentRun = 0;
const server = createServer((request, response) => {
  let raw = "";
  request.setEncoding("utf8");
  request.on("data", (chunk) => (raw += chunk));
  request.on("end", () => {
    let body = null;
    try {
      body = JSON.parse(raw);
    } catch {
      body = null;
    }
    requests.push({ runIndex: currentRun, url: request.url, body });
    if (request.url?.includes("count_tokens")) {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ input_tokens: 10 }));
      return;
    }
    if (request.url?.endsWith("/responses")) {
      response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
      const send = (type, data) =>
        response.write(
          `event: ${type}\ndata: ${JSON.stringify({ type, sequence_number: 1, ...data })}\n\n`,
        );
      const resp = {
        id: "resp_delivery_spike",
        object: "response",
        created_at: Math.floor(Date.now() / 1000),
        status: "in_progress",
        model: "mock-model",
        output: [],
        parallel_tool_calls: false,
        tool_choice: "auto",
        tools: [],
      };
      send("response.created", { response: resp });
      send("response.in_progress", { response: resp });
      send("response.output_item.added", {
        output_index: 0,
        item: { id: "item_1", type: "message", role: "assistant", status: "in_progress", content: [] },
      });
      send("response.content_part.added", {
        item_id: "item_1",
        output_index: 0,
        content_index: 0,
        part: { type: "output_text", text: "", annotations: [] },
      });
      send("response.output_text.delta", {
        item_id: "item_1",
        output_index: 0,
        content_index: 0,
        delta: "OK",
      });
      send("response.output_text.done", {
        item_id: "item_1",
        output_index: 0,
        content_index: 0,
        text: "OK",
      });
      send("response.content_part.done", {
        item_id: "item_1",
        output_index: 0,
        content_index: 0,
        part: { type: "output_text", text: "OK", annotations: [] },
      });
      send("response.output_item.done", {
        output_index: 0,
        item: {
          id: "item_1",
          type: "message",
          role: "assistant",
          status: "completed",
          content: [{ type: "output_text", text: "OK", annotations: [] }],
        },
      });
      send("response.completed", {
        response: {
          ...resp,
          status: "completed",
          output: [
            {
              id: "item_1",
              type: "message",
              role: "assistant",
              status: "completed",
              content: [{ type: "output_text", text: "OK", annotations: [] }],
            },
          ],
          usage: {
            input_tokens: 1,
            input_tokens_details: { cached_tokens: 0 },
            output_tokens: 1,
            output_tokens_details: { reasoning_tokens: 0 },
            total_tokens: 2,
          },
        },
      });
      response.end();
      return;
    }
    if (request.url?.endsWith("/chat/completions")) {
      const id = "chatcmpl_delivery_spike";
      const created = Math.floor(Date.now() / 1000);
      if (body?.stream === true) {
        response.writeHead(200, {
          "content-type": "text/event-stream",
          "cache-control": "no-cache",
          connection: "keep-alive",
        });
        const send = (data) => response.write(`data: ${JSON.stringify(data)}\n\n`);
        send({
          id,
          object: "chat.completion.chunk",
          created,
          model: body?.model ?? "mock-model",
          choices: [{ index: 0, delta: { role: "assistant", content: "OK" }, finish_reason: null }],
        });
        send({
          id,
          object: "chat.completion.chunk",
          created,
          model: body?.model ?? "mock-model",
          choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
        });
        response.write("data: [DONE]\n\n");
        response.end();
        return;
      }
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          id,
          object: "chat.completion",
          created,
          model: body?.model ?? "mock-model",
          choices: [
            { index: 0, message: { role: "assistant", content: "OK" }, finish_reason: "stop" },
          ],
          usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
        }),
      );
      return;
    }
    // Anthropic Messages
    const model = body?.model ?? "claude-mock";
    if (body?.stream) {
      response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
      const send = (event, data) =>
        response.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
      const message = {
        id: "msg_delivery_spike",
        type: "message",
        role: "assistant",
        model,
        content: [{ type: "text", text: "OK" }],
        stop_reason: "end_turn",
        stop_sequence: null,
        usage: { input_tokens: 10, output_tokens: 1 },
      };
      send("message_start", { type: "message_start", message: { ...message, content: [] } });
      send("content_block_start", {
        type: "content_block_start",
        index: 0,
        content_block: { type: "text", text: "" },
      });
      send("content_block_delta", {
        type: "content_block_delta",
        index: 0,
        delta: { type: "text_delta", text: "OK" },
      });
      send("content_block_stop", { type: "content_block_stop", index: 0 });
      send("message_delta", {
        type: "message_delta",
        delta: { stop_reason: "end_turn", stop_sequence: null },
        usage: { output_tokens: 1 },
      });
      send("message_stop", { type: "message_stop" });
      response.end();
      return;
    }
    response.writeHead(200, { "content-type": "application/json" });
    response.end(
      JSON.stringify({
        id: "msg_delivery_spike",
        type: "message",
        role: "assistant",
        model,
        content: [{ type: "text", text: "OK" }],
        stop_reason: "end_turn",
        stop_sequence: null,
        usage: { input_tokens: 10, output_tokens: 1 },
      }),
    );
  });
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

function requestsFor(runIndex) {
  return requests.filter((entry) => entry.runIndex === runIndex);
}
function mainRequest(runIndex, marker) {
  const candidates = requestsFor(runIndex).filter((entry) => entry.body !== null);
  return (
    candidates.find(
      (entry) =>
        JSON.stringify(entry.body).includes(marker) &&
        !JSON.stringify(entry.body).includes("Create a concise title"),
    ) ?? candidates[0]
  );
}

// ===== Claude Code =====
async function runClaudeDelivery() {
  const settings = path.join(tmpRoot, "claude-settings.json");
  writeFileSync(settings, "{}\n", "utf8");
  const base = [
    "--strict-mcp-config",
    "--setting-sources",
    "local",
    "--settings",
    settings,
    "--permission-mode",
    "plan",
    "--tools",
    "",
    "-p",
    "--output-format",
    "json",
  ];
  const env = {
    ANTHROPIC_BASE_URL: `http://127.0.0.1:${port}`,
    ANTHROPIC_AUTH_TOKEN: "q5-delivery-dummy",
    ANTHROPIC_API_KEY: "q5-delivery-dummy",
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
  };
  const session = randomUUID();
  const runs = [];
  async function run(name, args, prompt, marker = prompt) {
    currentRun += 1;
    const index = currentRun;
    const result = await runClaude(args, env, prompt);
    const request = mainRequest(index, marker);
    const systemText = (request?.body?.system ?? [])
      .map((block) => (typeof block === "string" ? block : block?.text ?? ""))
      .join("\n");
    const messages = (request?.body?.messages ?? []).map((message) => ({
      role: message.role,
      text: textsOf(message.content),
    }));
    const joined = messages.map((message) => message.text).join("\n");
    const record = {
      name,
      code: result.code,
      system_sha256: sha(systemText),
      messages,
      has_parent: joined.includes("PARENT_ONLY"),
      has_intervening: joined.includes("INTERVENING_ONLY"),
      v1: countMatches(joined, 'version="1"'),
      v2: countMatches(joined, 'version="2"'),
      stderr: result.stderr.slice(0, 200),
    };
    runs.push(record);
    return record;
  }
  const first = await run(
    "message_append v1",
    [...base, "--session-id", session],
    "PARENT_ONLY say ok",
    "PARENT_ONLY",
  );
  const second = await run(
    "message_append v2",
    [...base, "--resume", session],
    `RESULT_ONE\n\n${envelope(1, "RESULT_ONE")}`,
    "RESULT_ONE",
  );
  const third = await run(
    "message_append v3",
    [...base, "--resume", session],
    `RESULT_TWO\n\n${envelope(2, "RESULT_TWO")}`,
    "RESULT_TWO",
  );
  const parentSystem = first.system_sha256;
  check(
    "Claude message_append v1：父消息前缀保留 + envelope 恰好一次",
    first.code === 0 && second.has_parent && second.v1 === 1,
    JSON.stringify({ parent: second.has_parent, v1: second.v1 }),
  );
  check(
    "Claude message_append：父 system prompt 不变",
    second.system_sha256 === parentSystem && third.system_sha256 === parentSystem,
    JSON.stringify({ first: parentSystem, second: second.system_sha256, third: third.system_sha256 }),
  );
  check(
    "Claude message_append：旧 v1 保留为历史，v2 恰好一次",
    third.v1 === 1 && third.v2 === 1,
    JSON.stringify({ v1: third.v1, v2: third.v2 }),
  );

  const forkSession = randomUUID();
  const fork = await run(
    "origin_fork",
    [...base, "--session-id", forkSession],
    `[origin_snapshot]\nORIGIN_ONLY\n${envelope(1, "FORK_RESULT")}`,
    "ORIGIN_ONLY",
  );
  check(
    "Claude origin_fork：独立会话不含父 live intervening turns",
    fork.has_parent === false && fork.has_intervening === false && fork.v1 === 1,
    JSON.stringify({ parent: fork.has_parent, intervening: fork.has_intervening, v1: fork.v1 }),
  );
  return {
    version: spawnSync(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", "claude --version"], {
      encoding: "utf8",
    }).stdout?.trim(),
    session,
    runs,
    pass: checks.every((item) => item.ok),
  };
}

// ===== Codex =====
async function runCodexDelivery() {
  const codexJs = findCodexJs();
  if (!codexJs) throw new Error("找不到全局 codex.cmd 内的 codex.js");
  const home = path.join(tmpRoot, "codex-home");
  mkdirSync(home, { recursive: true });
  const provider = `model_providers.mock={name="mock",base_url=${JSON.stringify(
    `http://127.0.0.1:${port}/v1`,
  )},env_key="MOCK_API_KEY",wire_api="responses"}`;
  const common = [
    "exec",
    "--json",
    "--skip-git-repo-check",
    "-m",
    "mock-model",
    "-c",
    'model_provider="mock"',
    "-c",
    provider,
  ];
  const env = { CODEX_HOME: home, MOCK_API_KEY: "q5-delivery-dummy" };
  const runs = [];
  async function run(name, args, prompt, marker = prompt) {
    currentRun += 1;
    const index = currentRun;
    const result = await runCodex(codexJs, args, env, prompt);
    const request = mainRequest(index, marker);
    const input = request?.body?.input ?? [];
    const joined = input
      .map((item) => textsOf(item?.content ?? item?.text ?? ""))
      .join("\n");
    const developer = JSON.stringify(request?.body?.instructions ?? request?.body?.developer ?? "");
    const record = {
      name,
      code: result.code,
      stdout: result.stdout,
      developer_sha256: sha(developer),
      has_parent: joined.includes("PARENT_ONLY"),
      has_intervening: joined.includes("INTERVENING_ONLY"),
      v1: countMatches(joined, 'version="1"'),
      v2: countMatches(joined, 'version="2"'),
      stderr: result.stderr.slice(0, 200),
    };
    runs.push(record);
    return record;
  }
  const first = await run("message_append v1", [...common, "-"], "PARENT_ONLY say ok", "PARENT_ONLY");
  const threadId =
    parseJsonLines(first.stdout).find((item) => item?.type === "thread.started")?.thread_id ??
    parseJsonLines(first.stdout).find((item) => item?.thread_id)?.thread_id;
  if (!threadId) throw new Error("Codex 未返回 thread_id");
  const second = await run(
    "message_append v2",
    ["exec", "resume", ...common.slice(1), threadId, "-"],
    `RESULT_ONE ${envelope(1, "RESULT_ONE")}`,
    "RESULT_ONE",
  );
  const third = await run(
    "message_append v3",
    ["exec", "resume", ...common.slice(1), threadId, "-"],
    `RESULT_TWO ${envelope(2, "RESULT_TWO")}`,
    "RESULT_TWO",
  );
  check(
    "Codex message_append v1：父消息前缀保留 + envelope 恰好一次",
    first.code === 0 && second.has_parent && second.v1 === 1,
    JSON.stringify({ parent: second.has_parent, v1: second.v1 }),
  );
  check(
    "Codex message_append：developer 指令不变",
    second.developer_sha256 === first.developer_sha256 &&
      third.developer_sha256 === first.developer_sha256,
    JSON.stringify({
      first: first.developer_sha256,
      second: second.developer_sha256,
      third: third.developer_sha256,
    }),
  );
  check(
    "Codex message_append：旧 v1 保留为历史，v2 恰好一次",
    third.v1 === 1 && third.v2 === 1,
    JSON.stringify({ v1: third.v1, v2: third.v2 }),
  );

  const fork = await run(
    "origin_fork",
    [...common, "-"],
    `[origin_snapshot] ORIGIN_ONLY ${envelope(1, "FORK_RESULT")}`,
    "ORIGIN_ONLY",
  );
  check(
    "Codex origin_fork：独立 thread 不含父 live intervening turns",
    fork.has_parent === false && fork.has_intervening === false && fork.v1 === 1,
    JSON.stringify({ parent: fork.has_parent, intervening: fork.has_intervening, v1: fork.v1 }),
  );
  return {
    version: spawnSync(process.execPath, [codexJs, "--version"], { encoding: "utf8" }).stdout?.trim(),
    thread_id: threadId,
    runs,
    pass: checks.every((item) => item.ok),
  };
}

// ===== DSH =====
async function runDshDelivery() {
  const home = path.join(tmpRoot, "dsh-home");
  mkdirSync(home, { recursive: true });
  const profile = "aether-delivery";
  const env = { DSH_HOME: home, MOCK_API_KEY: "q5-delivery-dummy" };
  const create = spawnSync(
    process.env.ComSpec ?? "cmd.exe",
    ["/d", "/s", "/c", `dsh ${profile} --from-default-profile headless --dump-config`],
    { encoding: "utf8", env: { ...process.env, ...env }, timeout: 120_000 },
  );
  if (create.status !== 0) throw new Error(`DSH profile 创建失败：${create.stderr}`);
  const patch = path.join(tmpRoot, "dsh-patch.yml");
  writeFileSync(
    patch,
    [
      "- id: llm-pi-ai",
      "  config:",
      "    providers:",
      "      mock:",
      "        displayName: mock",
      "        apiKeyEnv: MOCK_API_KEY",
      "        api: openai-completions",
      `        baseURL: http://127.0.0.1:${port}/v1`,
      "        models:",
      "          - id: mock-model",
      "            name: mock-model",
      "- id: agent-default-model",
      "  config:",
      "    provider: mock",
      "    model: mock-model",
      "",
    ].join("\n"),
    "utf8",
  );
  const base = ["--profile", profile, "--patch", patch, "headless"];
  const runs = [];
  async function run(name, args, prompt, marker = prompt) {
    currentRun += 1;
    const index = currentRun;
    const result = await runDsh(args, env, prompt);
    const request = mainRequest(index, marker);
    const messages = (request?.body?.messages ?? []).map((message) => ({
      role: message.role,
      text: textsOf(message.content),
    }));
    const systemText = messages
      .filter((message) => message.role === "system")
      .map((message) => message.text)
      .join("\n");
    const joined = messages.map((message) => message.text).join("\n");
    const record = {
      name,
      code: result.code,
      stdout: result.stdout,
      system_sha256: sha(systemText),
      messages,
      has_parent: joined.includes("PARENT_ONLY"),
      has_intervening: joined.includes("INTERVENING_ONLY"),
      v1: countMatches(joined, 'version="1"'),
      v2: countMatches(joined, 'version="2"'),
      stderr: result.stderr.slice(0, 300),
    };
    runs.push(record);
    return record;
  }
  const first = await run(
    "message_append v1",
    [...base, "--json", "PARENT_ONLY say ok"],
    "PARENT_ONLY",
  );
  const sessionId = parseJsonLines(first.stdout).find((item) => item?.type === "session")?.sessionId;
  if (!sessionId) throw new Error("DSH 未返回 sessionId");
  const second = await run(
    "message_append v2",
    [...base, "--json", "--session-id", sessionId, `RESULT_ONE ${envelope(1, "RESULT_ONE")}`],
    "RESULT_ONE",
  );
  const third = await run(
    "message_append v3",
    [...base, "--json", "--session-id", sessionId, `RESULT_TWO ${envelope(2, "RESULT_TWO")}`],
    "RESULT_TWO",
  );
  check(
    "DSH message_append v1：父消息前缀保留 + envelope 恰好一次",
    first.code === 0 && second.has_parent && second.v1 === 1,
    JSON.stringify({ parent: second.has_parent, v1: second.v1 }),
  );
  check(
    "DSH message_append：父 system prompt 不变",
    second.system_sha256 === first.system_sha256 &&
      third.system_sha256 === first.system_sha256,
    JSON.stringify({
      first: first.system_sha256,
      second: second.system_sha256,
      third: third.system_sha256,
    }),
  );
  check(
    "DSH message_append：旧 v1 保留为历史，v2 恰好一次",
    third.v1 === 1 && third.v2 === 1,
    JSON.stringify({ v1: third.v1, v2: third.v2 }),
  );
  const fork = await run(
    "origin_fork",
    [...base, "--json", `[origin_snapshot] ORIGIN_ONLY ${envelope(1, "FORK_RESULT")}`],
    "ORIGIN_ONLY",
  );
  check(
    "DSH origin_fork：独立会话不含父 live intervening turns",
    fork.has_parent === false && fork.has_intervening === false && fork.v1 === 1,
    JSON.stringify({ parent: fork.has_parent, intervening: fork.has_intervening, v1: fork.v1 }),
  );
  return {
    version: spawnSync(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", "dsh --version"], {
      encoding: "utf8",
    }).stdout?.trim(),
    session_id: sessionId,
    runs,
    pass: checks.every((item) => item.ok),
  };
}

// ===== main =====
try {
  results.claude = await runClaudeDelivery();
  results.codex = await runCodexDelivery();
  results.dsh = await runDshDelivery();
} finally {
  server.close();
}
results.checks = checks;
results.passed = checks.filter((item) => item.ok).length;
results.failed = checks.filter((item) => !item.ok).length;
writeFileSync(
  path.join(tmpRoot, "results.json"),
  `${JSON.stringify(results, null, 2)}\n`,
  "utf8",
);
console.log(`\nQ5 real delivery spike: ${results.passed}/${checks.length} checks passed`);
process.exit(results.failed === 0 ? 0 : 1);
