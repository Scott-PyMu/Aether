/**
 * M2-11 Codex 夹具（确定性，无网络）：复刻 `codex exec --json` 的事件形状与
 * `exec resume <thread_id>` 语义（对齐 M1-11 实测样本与接入笔记）。
 *
 * 触发词（prompt 包含即生效，优先级从上到下）：
 * - `fail:no-terminal` → 输出 thread.started 后以退出码 7 退出（无 turn 终态）；
 * - `fail:bad-json` → 输出 20 行坏 JSON 后退出；
 * - `fail:turn` → `turn.failed`（error.message 含夹具标记）；
 * - `tool:normal` → command_execution 正常完成；
 * - `tool:fail` → file_change status=failed；
 * - `stream:multi` → 两个 agent_message 条目（整段）；
 * - `remember:<TOKEN>` → 记忆 TOKEN 并回 `STORED`（Mode R 恢复用例）；
 * - `recall` → 回已记忆的 TOKEN；
 * - `slow` / `cancel` → 保持运行直到被外部终止（中断用例）；
 * - 其它 → 固定三行基线文本。
 *
 * 环境：
 * - `CODEX_HOME`：会话持久化目录（`fake-codex-sessions.json`）；
 * - `FAKE_CODEX_PID_FILE`：写入本进程 pid（进程树回收断言）；
 * - `FAKE_CODEX_STREAM_DELAY_MS`：事件间隔（默认 10ms；`0` 表示不等待）。
 */

import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const HOME = process.env.CODEX_HOME || process.env.FAKE_CODEX_HOME || process.cwd();
const PID_FILE = process.env.FAKE_CODEX_PID_FILE || "";
const DELAY_MS = Number(process.env.FAKE_CODEX_STREAM_DELAY_MS ?? 10);

const BASELINE = [
  "Aether M2-11 fake codex baseline line 1",
  "Aether M2-11 fake codex baseline line 2",
  "Aether M2-11 fake codex baseline line 3",
].join("\n");

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, Math.max(0, ms)));
}

function emit(event) {
  process.stdout.write(`${JSON.stringify(event)}\n`);
}

function sessionsPath() {
  return join(HOME, "fake-codex-sessions.json");
}

function loadSessions() {
  try {
    const parsed = JSON.parse(readFileSync(sessionsPath(), "utf8"));
    if (parsed && typeof parsed === "object" && parsed.threads && typeof parsed.threads === "object") {
      return parsed;
    }
  } catch {
    /* 首次运行 */
  }
  return { threads: {} };
}

function saveSessions(state) {
  mkdirSync(HOME, { recursive: true });
  writeFileSync(sessionsPath(), JSON.stringify(state), "utf8");
}

function parseInvocation(argv) {
  const resumeIndex = argv.indexOf("resume");
  const resume = resumeIndex >= 0;
  // `exec resume [OPTIONS] <SESSION_ID> -`：会话 id 是末位 `-` 之前的参数。
  const threadRef = resume ? (argv[argv.length - 2] ?? null) : null;
  const modelIndex = argv.indexOf("-m");
  const model = modelIndex >= 0 ? argv[modelIndex + 1] : undefined;
  return { resume, threadRef, model };
}

async function main() {
  const argv = process.argv.slice(2);
  if (PID_FILE) {
    mkdirSync(dirname(PID_FILE), { recursive: true });
    appendFileSync(PID_FILE, `${process.pid}\n`);
  }
  const { resume, threadRef } = parseInvocation(argv);
  const prompt = readFileSync(0, "utf8");
  const state = loadSessions();

  let threadId;
  if (resume) {
    if (!threadRef || !state.threads[threadRef]) {
      process.stderr.write(`codex: session not found: ${threadRef ?? "<missing>"}\n`);
      process.exit(3);
    }
    threadId = threadRef;
  } else {
    threadId = `thr_${Math.random().toString(36).slice(2, 12)}`;
    state.threads[threadId] = { memory: null, created_at: Date.now() };
    saveSessions(state);
  }
  emit({ type: "thread.started", thread_id: threadId });

  if (prompt.includes("fail:no-terminal")) {
    // 自然退出（不调 process.exit）：保证管道 stdout 完整 flush（Windows 截断风险）。
    process.exitCode = 7;
    return;
  }

  emit({ type: "turn.started" });
  await sleep(DELAY_MS);

  if (prompt.includes("fail:bad-json")) {
    for (let index = 0; index < 20; index += 1) {
      process.stdout.write(`not-json line ${index}\n`);
      await sleep(2);
    }
    process.exitCode = 9;
    return;
  }

  if (prompt.includes("fail:turn")) {
    emit({
      type: "turn.failed",
      error: { message: "fake-codex turn failed（夹具标记）", code: "fake_turn_error" },
    });
    process.exitCode = 1;
    return;
  }

  if (prompt.includes("slow") || prompt.includes("cancel")) {
    // 保持运行直到外部终止（在途 run 中断用例）。
    await sleep(60_000);
    return;
  }

  if (prompt.includes("tool:normal")) {
    emit({
      type: "item.started",
      item: { id: "tool-1", type: "command_execution", command: "echo hi", status: "in_progress" },
    });
    await sleep(DELAY_MS);
    emit({
      type: "item.completed",
      item: {
        id: "tool-1",
        type: "command_execution",
        command: "echo hi",
        aggregated_output: "hi\n",
        exit_code: 0,
        status: "completed",
      },
    });
  } else if (prompt.includes("tool:fail")) {
    emit({
      type: "item.started",
      item: { id: "tool-2", type: "file_change", path: "b.txt", status: "in_progress" },
    });
    await sleep(DELAY_MS);
    emit({
      type: "item.completed",
      item: { id: "tool-2", type: "file_change", path: "b.txt", status: "failed" },
    });
  }

  let text;
  if (prompt.includes("remember:")) {
    const token = prompt.slice(prompt.indexOf("remember:") + "remember:".length).trim();
    state.threads[threadId].memory = token;
    saveSessions(state);
    text = "STORED";
  } else if (prompt.includes("recall")) {
    text = state.threads[threadId].memory ?? "";
  } else if (prompt.includes("stream:multi")) {
    text = "part one";
    emit({ type: "item.completed", item: { id: "msg-1", type: "agent_message", text } });
    await sleep(DELAY_MS);
    text = "part onepart two";
    emit({ type: "item.updated", item: { id: "msg-1", type: "agent_message", text } });
    await sleep(DELAY_MS);
    emit({
      type: "turn.completed",
      usage: { input_tokens: 11, output_tokens: 22, total_tokens: 33 },
    });
    return;
  } else {
    text = BASELINE;
  }

  emit({ type: "item.completed", item: { id: "msg-final", type: "agent_message", text } });
  await sleep(DELAY_MS);
  emit({
    type: "turn.completed",
    usage: { input_tokens: 12, output_tokens: 34, total_tokens: 46 },
  });
}

main().catch((error) => {
  process.stderr.write(`fake-codex 崩溃: ${error?.stack ?? error}\n`);
  process.exit(1);
});
