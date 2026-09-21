#!/usr/bin/env node
/**
 * M2-02 确定性 Claude Code CLI 替身（fake-claude）。
 *
 * 目的：在无网络/无凭证环境下，以官方 stream-json 事件形状驱动真实适配器进程，
 * 覆盖：流式正文、工具调用（正常/失败/中断）、异常路径（result.is_error、无 result、
 * stdout 混入坏行）、Mode R 会话恢复（跨进程 `--resume`）。
 *
 * 仅用于测试：不发起任何网络请求；会话记忆写入 `FAKE_CLAUDE_HOME/sessions/<id>.json`。
 *
 * 用法（由适配器 spawn）：
 *   node cli.mjs [--settings ...] --session-id <uuid>|-r/--resume <uuid> -p \
 *     --output-format stream-json --include-partial-messages --verbose
 * 环境：
 *   FAKE_CLAUDE_HOME     会话存储目录（默认 os.tmpdir()/fake-claude-home）
 *   FAKE_CLAUDE_PID_FILE 启动时追加本进程 pid（测试断言进程树回收）
 *
 * 提示词指令（stdin 全文，trim 后精确匹配）：
 *   chat / <其它>        流式输出固定文本并成功结束
 *   tool:normal          工具调用正常完成（tool_use → tool_result ok）
 *   tool:fail            工具调用失败（tool_result is_error）
 *   tool:slow            工具调用悬挂（等待被中断）
 *   slow                 持续输出 delta（等待被中断/超时）
 *   fail:api-error       result.is_error=true（api_error_status=503）
 *   fail:no-result       退出码 1 且无 result 事件
 *   fail:bad-json        连续输出坏行（≥20）后悬挂
 *   remember:<token>     记忆口令并回复 STORED
 *   recall               回复已记忆口令（或 NONE）
 */

import { appendFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const FAKE_MODEL = "claude-fake-1";
const FAKE_TOOLS = ["Bash", "Read", "Write", "Edit", "Glob", "Grep"];
const DELTA_FRAGMENT = "Aether M2-02 fake stream ";
const MAX_TURNS = 1;

function fail(message) {
  process.stderr.write(`fake-claude: ${message}\n`);
  process.exit(2);
}

function parseArgs(argv) {
  const parsed = { sessionId: null, resume: false, promptFormat: null };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    switch (flag) {
      case "--session-id":
        parsed.sessionId = argv[++index] ?? null;
        parsed.resume = false;
        break;
      case "--resume":
      case "-r":
        parsed.sessionId = argv[++index] ?? null;
        parsed.resume = true;
        break;
      case "--output-format":
        parsed.promptFormat = argv[++index] ?? null;
        break;
      case "--include-partial-messages":
      case "--verbose":
      case "-p":
      case "--strict-mcp-config":
        break;
      default:
        // 跳过带值参数（settings/permission-mode/model/max-turns/tools 等）。
        if (!flag.startsWith("--")) break;
        if (["--setting-sources", "--settings", "--permission-mode", "--model", "--max-turns", "--tools"].includes(flag)) {
          index += 1;
        }
        break;
    }
  }
  if (!parsed.sessionId) fail("缺少 --session-id/--resume");
  if (parsed.promptFormat && parsed.promptFormat !== "stream-json") {
    fail(`不支持的 output-format: ${parsed.promptFormat}`);
  }
  return parsed;
}

function readPrompt() {
  return new Promise((resolve) => {
    let data = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (chunk) => {
      data += chunk;
    });
    process.stdin.on("end", () => resolve(data));
    process.stdin.on("error", () => resolve(data));
  });
}

function sessionDir() {
  const home = process.env.FAKE_CLAUDE_HOME;
  const dir = home && home.length > 0 ? home : join(tmpdir(), "fake-claude-home");
  mkdirSync(join(dir, "sessions"), { recursive: true });
  return dir;
}

function sessionPath(sessionId) {
  return join(sessionDir(), "sessions", `${sessionId}.json`);
}

function loadSession(sessionId) {
  const path = sessionPath(sessionId);
  if (!existsSync(path)) return null;
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return null;
  }
}

function saveSession(sessionId, state) {
  writeFileSync(sessionPath(sessionId), JSON.stringify(state), "utf8");
}

function writeLine(line) {
  return new Promise((resolve) => {
    process.stdout.write(`${line}\n`, resolve);
  });
}

async function emit(event) {
  await writeLine(JSON.stringify(event));
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** 无限悬挂（等待被适配器/测试中断）。
 *
 * 必须挂一个活动 handle：仅有未决议 Promise 时 Node 事件循环耗尽会自行退出，
 * 使「悬挂」场景变成退出（M2-02 中断用例曾因该竞态偶发失败）。
 */
function hangForever() {
  return new Promise(() => {
    setInterval(() => {}, 1 << 30);
  });
}

function initEvent(sessionId) {
  return {
    type: "system",
    subtype: "init",
    session_id: sessionId,
    tools: FAKE_TOOLS,
    model: FAKE_MODEL,
    cwd: process.cwd(),
    permissionMode: "default",
    apiKeySource: "none",
  };
}

function deltaEvent(sessionId, text) {
  return {
    type: "stream_event",
    session_id: sessionId,
    event: {
      type: "content_block_delta",
      index: 0,
      delta: { type: "text_delta", text },
    },
  };
}

function resultEvent(sessionId, { text, isError = false, subtype = "success", apiErrorStatus = null }) {
  return {
    type: "result",
    subtype: isError ? subtype : "success",
    is_error: isError,
    result: text,
    session_id: sessionId,
    usage: {
      input_tokens: 12,
      output_tokens: text.length,
      cache_creation_input_tokens: 0,
      cache_read_input_tokens: 0,
    },
    duration_ms: 25,
    duration_api_ms: 20,
    num_turns: MAX_TURNS,
    total_cost_usd: 0,
    api_error_status: apiErrorStatus,
  };
}

async function streamText(sessionId, text, { chunkSize = 6, intervalMs = 2 } = {}) {
  let emitted = "";
  for (let offset = 0; offset < text.length; offset += chunkSize) {
    const chunk = text.slice(offset, offset + chunkSize);
    emitted += chunk;
    await emit(deltaEvent(sessionId, chunk));
    if (intervalMs > 0) await sleep(intervalMs);
  }
  return emitted;
}

async function finish(sessionId, { text, isError = false, subtype = "success", apiErrorStatus = null }) {
  await emit(resultEvent(sessionId, { text, isError, subtype, apiErrorStatus }));
  await sleep(5);
  process.exit(isError ? 1 : 0);
}

function toolUseEvent(sessionId, toolCallId, toolName, input) {
  return {
    type: "assistant",
    session_id: sessionId,
    message: {
      id: `msg_${toolCallId}`,
      type: "message",
      role: "assistant",
      model: FAKE_MODEL,
      content: [
        { type: "text", text: "调用工具。" },
        { type: "tool_use", id: toolCallId, name: toolName, input },
      ],
      stop_reason: "tool_use",
      usage: { input_tokens: 10, output_tokens: 4 },
    },
  };
}

function toolResultEvent(sessionId, toolCallId, { isError = false, content = "done" }) {
  return {
    type: "user",
    session_id: sessionId,
    message: {
      role: "user",
      content: [
        {
          type: "tool_result",
          tool_use_id: toolCallId,
          is_error: isError,
          content,
        },
      ],
    },
  };
}

async function main() {
  const parsed = parseArgs(process.argv.slice(2));
  const pidFile = process.env.FAKE_CLAUDE_PID_FILE;
  if (pidFile && pidFile.length > 0) {
    try {
      appendFileSync(pidFile, `${process.pid}\n`, "utf8");
    } catch {
      /* 诊断文件可选 */
    }
  }

  const prompt = (await readPrompt()).trim();
  const sessionId = parsed.sessionId;
  const existing = loadSession(sessionId);

  if (parsed.resume && !existing) {
    await emit(initEvent(sessionId));
    await emit(
      resultEvent(sessionId, {
        text: `resume 失败：会话 ${sessionId} 不存在`,
        isError: true,
        subtype: "error_resume_missing",
      }),
    );
    process.exit(1);
  }
  const state = existing ?? { sessionId, memory: [] };
  // 原生会话在启动时即落盘（与 Claude Code 的会话持久化语义一致）：
  // 后续 run 的 `--resume` 才能命中；否则第二次运行会被误判为 resume 缺失。
  if (!existing) saveSession(sessionId, state);

  await emit(initEvent(sessionId));

  if (prompt.startsWith("remember:")) {
    const token = prompt.slice("remember:".length).trim();
    state.memory.push(token);
    saveSession(sessionId, state);
    await streamText(sessionId, "STORED");
    await finish(sessionId, { text: "STORED" });
    return;
  }
  if (prompt === "recall") {
    const token = state.memory.length > 0 ? state.memory[state.memory.length - 1] : "NONE";
    await streamText(sessionId, token);
    await finish(sessionId, { text: token });
    return;
  }

  switch (prompt) {
    case "tool:normal": {
      const toolCallId = `toolu_${sessionId.slice(0, 8)}_1`;
      const text = await streamText(sessionId, "工具调用完成。");
      await emit(toolUseEvent(sessionId, toolCallId, "Write", { file_path: "notes.md", content: "hi" }));
      await sleep(10);
      await emit(toolResultEvent(sessionId, toolCallId, { isError: false, content: "written" }));
      await finish(sessionId, { text });
      return;
    }
    case "tool:fail": {
      const toolCallId = `toolu_${sessionId.slice(0, 8)}_2`;
      await streamText(sessionId, "开始工具调用。");
      await emit(toolUseEvent(sessionId, toolCallId, "Read", { file_path: "missing.md" }));
      await sleep(10);
      await emit(
        toolResultEvent(sessionId, toolCallId, {
          isError: true,
          content: "ENOENT: no such file or directory, open 'missing.md'",
        }),
      );
      await finish(sessionId, { text: "工具执行失败。" });
      return;
    }
    case "tool:slow": {
      const toolCallId = `toolu_${sessionId.slice(0, 8)}_3`;
      await emit(toolUseEvent(sessionId, toolCallId, "Bash", { command: "sleep 3600" }));
      await hangForever();
      return;
    }
    case "slow": {
      let counter = 0;
      for (;;) {
        counter += 1;
        await emit(deltaEvent(sessionId, `${DELTA_FRAGMENT}${counter} `));
        await sleep(200);
      }
    }
    case "fail:api-error": {
      await finish(sessionId, {
        text: "upstream connect error or disconnect/reset before headers (503)",
        isError: true,
        subtype: "error_during_execution",
        apiErrorStatus: 503,
      });
      return;
    }
    case "fail:no-result": {
      process.stderr.write("fake-claude: 模拟启动后崩溃（stderr 诊断）\n");
      await sleep(10);
      process.exit(7);
      return;
    }
    case "fail:bad-json": {
      for (let index = 0; index < 25; index += 1) {
        await writeLine(`{ not json #${index}`);
      }
      await hangForever();
      return;
    }
    default: {
      const text = await streamText(sessionId, `${DELTA_FRAGMENT}完成`);
      await finish(sessionId, { text });
      return;
    }
  }
}

main().catch((error) => {
  process.stderr.write(`fake-claude: ${error?.stack ?? error}\n`);
  process.exit(3);
});
