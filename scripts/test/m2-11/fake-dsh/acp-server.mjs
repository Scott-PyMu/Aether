/**
 * M2-11 DSH 夹具（确定性，无网络）：复刻 `dsh --profile acp` 的官方 ACP 协议面
 * （JSON-RPC 2.0 over stdio；`session/*`、`session/update`、`session/request_permission`），
 * 并模拟自研插件把原始 provider 增量写入带外通道（`AETHER_DSH_DELTA_FILE`）。
 *
 * 触发词（prompt 包含即生效）：
 * - `fail:prompt-error` → `session/prompt` 返回 JSON-RPC 错误；
 * - `cancel:long` / `slow` → prompt 保持 pending，直到 `session/cancel`（stopReason=cancelled）；
 * - `permission:allow` / `permission:deny` → 发起 `session/request_permission`，按客户端决议继续；
 * - `tool:normal` / `tool:fail` → `tool_call` + `tool_call_update` 生命周期；
 * - `dedupe` → 插件 delta（token 级）+ ACP committed 全文（前缀去重用例）；
 * - `dedupe:suffix` → 插件 delta 只覆盖前缀，committed 含 final-only 后缀；
 * - `no-end` → 同 dedupe 但不发 `end` 帧（丢弃 end 故障注入）；
 * - `drop-frames` → 插件 delta 中断（截断通道），committed 全量到达（兜底重建用例）；
 * - `remember:<TOKEN>` / `recall` → 会话记忆（Mode R 用例）；
 * - 其它 → 固定三行基线文本（3 个 committed chunk + 对齐的插件 delta）。
 *
 * 环境：`DSH_HOME`、`AETHER_DSH_DELTA_FILE`、`FAKE_DSH_PID_FILE`、
 * `FAKE_DSH_VERSION`（默认 0.1.5-rc.2）、`FAKE_DSH_NO_PLUGIN_HELLO=1`（门闩失败注入）、
 * `FAKE_DSH_TICK_MS`（默认 10）。
 */

import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const HOME = process.env.DSH_HOME || process.cwd();
const DELTA_FILE = process.env.AETHER_DSH_DELTA_FILE || "";
const PID_FILE = process.env.FAKE_DSH_PID_FILE || "";
const VERSION = process.env.FAKE_DSH_VERSION || "0.1.5-rc.2";
const TICK_MS = Number(process.env.FAKE_DSH_TICK_MS ?? 10);

const pendingRequests = new Map();
const sessions = loadSessions();
const activePrompts = new Map();
/** 冷启动竞态注入：前 N 次 session/new 返回 no adapter registered（默认 0）。 */
const NEW_FAILS = Number(process.env.FAKE_DSH_NEW_FAILS ?? 0);
let sessionNewAttempts = 0;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, Math.max(0, ms)));
}

function send(frame) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", ...frame })}\n`);
}

function reply(id, result) {
  send({ id, result });
}

function replyError(id, code, message) {
  send({ id, error: { code, message } });
}

function delta(frame) {
  if (!DELTA_FILE) return;
  try {
    appendFileSync(DELTA_FILE, `${JSON.stringify({ v: 1, at: Date.now(), ...frame })}\n`);
  } catch {
    /* 忽略 */
  }
}

function sessionsPath() {
  return join(HOME, "fake-dsh-sessions.json");
}

function loadSessions() {
  try {
    const parsed = JSON.parse(readFileSync(sessionsPath(), "utf8"));
    if (parsed && typeof parsed === "object" && parsed.sessions && typeof parsed.sessions === "object") {
      return parsed;
    }
  } catch {
    /* 首次运行 */
  }
  return { sessions: {} };
}

function saveSessions() {
  mkdirSync(HOME, { recursive: true });
  writeFileSync(sessionsPath(), JSON.stringify(sessions), "utf8");
}

function textFor(prompt) {
  if (prompt.includes("dedupe:suffix")) {
    return "Aether M2-11 fake dsh dedupe line 1\nline 2\nfinal-only suffix";
  }
  if (prompt.includes("dedupe") || prompt.includes("no-end") || prompt.includes("drop-frames")) {
    return "Aether M2-11 fake dsh dedupe line 1\nline 2\nline 3";
  }
  if (prompt.includes("remember:")) {
    return "STORED";
  }
  if (prompt.includes("recall")) {
    return "AETHER-DSH-TOKEN";
  }
  return [
    "Aether M2-11 fake dsh baseline line 1",
    "Aether M2-11 fake dsh baseline line 2",
    "Aether M2-11 fake dsh baseline line 3",
  ].join("\n");
}

function update(sessionId, body) {
  send({ method: "session/update", params: { sessionId, update: body } });
}

/** 把 committed 文本按 12 字符切片，模拟 token 级 delta 通道。 */
function chunksOf(text) {
  const chunks = [];
  let index = 0;
  while (index < text.length) {
    chunks.push(text.slice(index, index + 8));
    index += 8;
  }
  return chunks;
}

async function streamDelta(attemptId, chunks, { dropAfter = Number.POSITIVE_INFINITY, end = true } = {}) {
  delta({ type: "start", attemptId });
  for (let index = 0; index < chunks.length; index += 1) {
    if (index >= dropAfter) break;
    delta({ type: "chunk", attemptId, chunkType: "text-delta", text: chunks[index] });
    await sleep(TICK_MS);
  }
  if (end && dropAfter === Number.POSITIVE_INFINITY) delta({ type: "end", attemptId });
}

async function runPrompt(id, params) {
  const sessionId = params?.sessionId;
  const session = sessions.sessions[sessionId];
  if (!session) {
    replyError(id, -32002, `session not found: ${sessionId}`);
    return;
  }
  const prompt = (params?.prompt ?? []).map((part) => part?.text ?? "").join("\n");
  const attemptId = `attempt-${Math.random().toString(36).slice(2, 10)}`;

  if (prompt.includes("fail:prompt-error")) {
    replyError(id, -32000, "fake-dsh prompt failure（夹具标记）");
    return;
  }

  const settle = (result) => {
    activePrompts.delete(sessionId);
    reply(id, result);
  };
  activePrompts.set(sessionId, { settle });

  if (prompt.includes("cancel:long") || prompt.includes("slow")) {
    // 先给一段增量再挂起，等待 session/cancel（协议级取消）。
    await streamDelta(attemptId, chunksOf("partial before cancel "), { end: false });
    return; // pending：由 session/cancel 触发 settle
  }

  if (prompt.includes("permission:allow") || prompt.includes("permission:deny")) {
    const requestId = `perm-${Math.random().toString(36).slice(2, 8)}`;
    const options = [
      { optionId: "allow-once", kind: "allow_once", name: "允许一次" },
      { optionId: "reject-once", kind: "reject_once", name: "拒绝" },
    ];
    // 真实 DSH：先有 tool_call 生命周期，ask 只带 toolCallId（title/kind 缺失）——
    // 夹具对齐该形状以验证适配器的跟踪补全。
    update(sessionId, {
      sessionUpdate: "tool_call",
      toolCallId: "tc-perm",
      title: "edit",
      kind: "edit",
      status: "in_progress",
      rawInput: { path: "a.txt" },
    });
    const outcome = await new Promise((resolve) => {
      pendingRequests.set(requestId, resolve);
      send({
        id: requestId,
        method: "session/request_permission",
        params: {
          sessionId,
          toolCall: { toolCallId: "tc-perm" },
          options,
        },
      });
    });
    const selected = outcome?.outcome?.outcome === "selected";
    const optionId = outcome?.outcome?.optionId ?? null;
    const allowed = selected && String(optionId).startsWith("allow");
    update(sessionId, {
      sessionUpdate: "tool_call_update",
      toolCallId: "tc-perm",
      status: allowed ? "completed" : "failed",
    });
    const text = allowed ? "permission allowed" : "permission denied";
    update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
    settle({ stopReason: "end_turn" });
    return;
  }

  if (prompt.includes("tool:normal") || prompt.includes("tool:fail")) {
    const failed = prompt.includes("tool:fail");
    update(sessionId, {
      sessionUpdate: "tool_call",
      toolCallId: "tc-1",
      title: failed ? "edit" : "read",
      kind: failed ? "edit" : "read",
      status: "in_progress",
      rawInput: { path: "a.txt" },
    });
    await sleep(TICK_MS);
    update(sessionId, {
      sessionUpdate: "tool_call_update",
      toolCallId: "tc-1",
      status: failed ? "failed" : "completed",
    });
    const text = failed ? "tool failed run" : "tool ok run";
    await streamDelta(attemptId, chunksOf(text));
    update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
    settle({ stopReason: "end_turn" });
    return;
  }

  const text = textFor(prompt);
  if (prompt.includes("remember:")) {
    session.memory = prompt.slice(prompt.indexOf("remember:") + "remember:".length).trim();
    saveSessions();
  } else if (prompt.includes("recall")) {
    // 记忆优先（Mode R 断言）。
    const recalled = session.memory ?? text;
    await streamDelta(attemptId, chunksOf(recalled));
    update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: recalled } });
    settle({ stopReason: "end_turn" });
    return;
  }

  const chunks = chunksOf(text);
  if (prompt.includes("drop-frames")) {
    // 截断通道：跳过中间块（前缀不连续）且不发 end；ACP committed 全量到达 → 适配器兜底重建。
    await streamDelta(attemptId, chunks, { dropAfter: 1, end: false });
    await streamDelta(attemptId, chunks.slice(3), { end: false });
    update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
    settle({ stopReason: "end_turn" });
    return;
  }
  if (prompt.includes("dedupe:suffix")) {
    const streamed = text.slice(0, Math.max(0, text.length - "final-only suffix".length));
    await streamDelta(attemptId, chunksOf(streamed));
    update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
    settle({ stopReason: "end_turn" });
    return;
  }
  if (prompt.includes("no-end")) {
    await streamDelta(attemptId, chunks, { end: false });
    update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
    settle({ stopReason: "end_turn" });
    return;
  }

  // 默认路径：插件 delta（token 级）+ 单个 committed 全文（M1-11 实测：一次消息 1 个 chunk）。
  await streamDelta(attemptId, chunks);
  update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text } });
  update(sessionId, {
    sessionUpdate: "usage_update",
    used: 1234,
    size: 128000,
  });
  settle({ stopReason: "end_turn" });
}

function handleMessage(message) {
  if (message.id !== undefined && pendingRequests.has(message.id)) {
    const resolve = pendingRequests.get(message.id);
    pendingRequests.delete(message.id);
    resolve(message.result);
    return;
  }
  if (message.method === "session/cancel") {
    const sessionId = message.params?.sessionId;
    const active = activePrompts.get(sessionId);
    if (active) active.settle({ stopReason: "cancelled" });
    return;
  }
  if (message.method === undefined || message.id === undefined) return;
  switch (message.method) {
    case "initialize":
      reply(message.id, {
        protocolVersion: 1,
        agentCapabilities: { loadSession: true, promptCapabilities: {} },
        agentInfo: { name: "fake-dsh", version: VERSION },
      });
      return;
    case "session/new": {
      sessionNewAttempts += 1;
      if (sessionNewAttempts <= NEW_FAILS) {
        send({
          id: message.id,
          error: {
            code: -32603,
            message: "Internal error",
            data: { details: 'no adapter registered for provider "fake-provider"' },
          },
        });
        return;
      }
      const sessionId = `acp-${Math.random().toString(36).slice(2, 10)}`;
      sessions.sessions[sessionId] = { memory: null, created_at: Date.now() };
      saveSessions();
      reply(message.id, { sessionId, configOptions: [] });
      return;
    }
    case "session/resume": {
      const sessionId = message.params?.sessionId;
      if (!sessions.sessions[sessionId]) {
        replyError(message.id, -32002, `session not found: ${sessionId}`);
        return;
      }
      reply(message.id, { sessionId, configOptions: [] });
      return;
    }
    case "session/close": {
      delete sessions.sessions[message.params?.sessionId];
      saveSessions();
      reply(message.id, { ok: true });
      return;
    }
    case "session/prompt": {
      void runPrompt(message.id, message.params).catch((error) => {
        replyError(message.id, -32603, `fake-dsh 崩溃：${error?.message ?? error}`);
      });
      return;
    }
    default:
      replyError(message.id, -32601, `method not found: ${message.method}`);
  }
}

function main() {
  if (PID_FILE) {
    mkdirSync(dirname(PID_FILE), { recursive: true });
    appendFileSync(PID_FILE, `${process.pid}\n`);
  }
  if (process.env.FAKE_DSH_NO_PLUGIN_HELLO !== "1") {
    delta({ type: "hello", contract: "aether-dsh-stream@1", dshVersion: VERSION });
  }
  process.stderr.write(`fake-dsh acp server ready（${VERSION}）\n`);
  let buffer = "";
  process.stdin.setEncoding("utf8");
  process.stdin.on("data", (chunk) => {
    buffer += chunk;
    let index = buffer.indexOf("\n");
    while (index >= 0) {
      const line = buffer.slice(0, index).replace(/\r$/, "");
      buffer = buffer.slice(index + 1);
      if (line.trim().length > 0) {
        try {
          handleMessage(JSON.parse(line));
        } catch (error) {
          process.stderr.write(`fake-dsh 无效帧：${error?.message ?? error}\n`);
        }
      }
      index = buffer.indexOf("\n");
    }
  });
  process.stdin.on("end", () => process.exit(0));
}

main();
