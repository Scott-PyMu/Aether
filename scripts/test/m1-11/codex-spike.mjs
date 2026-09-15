/**
 * M1-11 A1 spike —— Codex 最小可编程接入样例（官方 CLI，无第三方依赖）。
 *
 * 覆盖：
 *  - createSession：`codex exec --json`，会话 id 来自 `thread.started.thread_id`；续会话 `codex exec resume <id>`
 *  - sendMessage：提示词走 stdin（末尾 `-`），stdout 为 JSONL 事件流
 *  - 终态：`turn.completed` / `turn.failed`；超时视为挂起
 *  - 对照基线：同一 run 的最终 `item.completed`(agent_message) 文本；control 模式用 `-o <file>` 非流式落盘
 *  - 中断：进程树回收（taskkill /T /F | SIGINT 进程组），实测「发起 → 退出」耗时
 *
 * 用法：node codex-spike.mjs [--runs 1] [--prompt "..."] [--mode fresh|continuation|interrupt|control]
 * 环境变量：M1_11_CODEX_MODEL（可选，覆盖 -m）、M1_11_CODEX_REASONING（默认 low）、
 *          M1_11_CODEX_EXTRA_ARGS（JSON 数组，追加 CLI 参数，如 ["-c","model_provider=..."])
 */
import { randomUUID } from 'node:crypto';
import { readFileSync, unlinkSync } from 'node:fs';
import { join } from 'node:path';
import {
  collectSecrets,
  envFirst,
  interruptTree,
  makeTmpDir,
  normalizeText,
  nowMs,
  parseArgs,
  readLines,
  redact,
  resolveCli,
  spawnCli,
  waitClose,
  writeJson,
} from './lib.mjs';

export const DEFAULT_PROMPT = [
  'Output exactly the following three lines and nothing else. Do not add quotes, markdown or comments.',
  'Aether M1-11 adapter spike baseline line 1',
  'Aether M1-11 adapter spike baseline line 2',
  'Aether M1-11 adapter spike baseline line 3',
].join('\n');

export const INTERRUPT_PROMPT =
  'Write an extremely long detailed essay about distributed systems. At least 3000 words. Do not stop early.';

export function codexConfig() {
  const model = envFirst(['M1_11_CODEX_MODEL'], '');
  const reasoning = envFirst(['M1_11_CODEX_REASONING'], 'low');
  const sandbox = envFirst(['M1_11_CODEX_SANDBOX'], 'read-only');
  let extraArgs = [];
  const rawExtra = process.env.M1_11_CODEX_EXTRA_ARGS;
  if (rawExtra) {
    const parsed = JSON.parse(rawExtra);
    if (!Array.isArray(parsed)) throw new Error('M1_11_CODEX_EXTRA_ARGS 必须是 JSON 数组');
    extraArgs = parsed.map(String);
  }
  return { model, reasoning, sandbox, extraArgs };
}

/** 使用隔离的 CODEX_HOME（CI 与本地 spike 都应避开用户级 MCP/技能配置）。 */
function spawnEnv() {
  const env = { ...process.env };
  if (process.env.M1_11_CODEX_HOME) env.CODEX_HOME = process.env.M1_11_CODEX_HOME;
  return env;
}

function commonArgs(config, cwd) {
  return [
    '--json',
    '--skip-git-repo-check',
    '--sandbox',
    config.sandbox,
    ...(config.model ? ['-m', config.model] : []),
    ...(config.reasoning ? ['-c', `model_reasoning_effort=${config.reasoning}`] : []),
    ...config.extraArgs,
    '--cd',
    cwd,
  ];
}

/** 单次 exec 调用（一次 createSession/sendMessage）。 */
export async function runCodexOnce(options) {
  const {
    cli,
    config,
    prompt,
    threadId = null,
    cwd = process.cwd(),
    timeoutMs = 240000,
    interruptAfterFirstDeltaMs = null,
    interruptFallbackMs = 8000,
    secrets = [],
    baselineText = null,
  } = options;

  const args = threadId
    ? ['exec', 'resume', '--json', '--skip-git-repo-check',
        ...(config.model ? ['-m', config.model] : []),
        ...(config.reasoning ? ['-c', `model_reasoning_effort=${config.reasoning}`] : []),
        '-c', `sandbox_mode=${config.sandbox}`,
        ...config.extraArgs, threadId, '-']
    : ['exec', ...commonArgs(config, cwd), '-'];

  const record = {
    candidate: 'codex',
    threadId: threadId || null,
    resumed: Boolean(threadId),
    model: config.model || '(codex config)',
    startedAt: new Date().toISOString(),
    terminal: 'none',
    timedOut: false,
    textItems: [],
    finalText: null,
    streamText: '',
    updatedTextSeen: false,
    usage: null,
    firstEventMs: null,
    firstTextMs: null,
    lastTextMs: null,
    eventCounts: {},
    transientErrors: [],
    errors: [],
    stderr: '',
    exitCode: null,
    interrupt: null,
    baselineMatch: null,
    wallClockMs: null,
  };

  const start = nowMs();
  const child = spawnCli(cli, args, {
    cwd,
    env: spawnEnv(),
    stdio: ['pipe', 'pipe', 'pipe'],
    windowsHide: true,
  });

  let interruptPromise = null;
  let closed = false;
  let fallbackTimer = null;
  let resolveInterruptSignal = null;
  const interruptSignal = new Promise((resolve) => {
    resolveInterruptSignal = resolve;
  });
  const triggerInterrupt = (delayMs) => {
    if (interruptPromise || closed) return;
    interruptPromise = new Promise((resolve) => {
      setTimeout(() => {
        if (closed) {
          resolve({ elapsedMs: 0, skipped: true });
          return;
        }
        interruptTree(child).then(resolve);
      }, Math.max(0, delayMs));
    });
    interruptPromise.then(resolveInterruptSignal);
  };
  child.once('close', () => {
    closed = true;
    if (fallbackTimer) clearTimeout(fallbackTimer);
  });
  if (interruptAfterFirstDeltaMs !== null) {
    fallbackTimer = setTimeout(() => triggerInterrupt(0), interruptFallbackMs);
  }
  const bump = (type) => {
    record.eventCounts[type] = (record.eventCounts[type] || 0) + 1;
  };

  const handleLine = (line, receivedAt) => {
    let event;
    try {
      event = JSON.parse(line);
    } catch {
      record.errors.push(`unparsed stdout line: ${redact(line, secrets).slice(0, 200)}`);
      return;
    }
    const elapsed = receivedAt - start;
    if (record.firstEventMs === null) record.firstEventMs = elapsed;
    switch (event.type) {
      case 'thread.started':
        bump('thread.started');
        if (event.thread_id) record.threadId = event.thread_id;
        break;
      case 'turn.started':
        bump('turn.started');
        break;
      case 'item.updated':
      case 'item.started': {
        const item = event.item || {};
        bump(`${event.type}:${item.type || 'unknown'}`);
        if (item.type === 'agent_message' && typeof item.text === 'string' && item.text.length > 0) {
          record.updatedTextSeen = true;
          if (record.firstTextMs === null) record.firstTextMs = elapsed;
        }
        break;
      }
      case 'item.completed': {
        const item = event.item || {};
        bump(`item.completed:${item.type || 'unknown'}`);
        if (item.type === 'agent_message' && typeof item.text === 'string' && item.text.length > 0) {
          if (record.firstTextMs === null) record.firstTextMs = elapsed;
          record.lastTextMs = elapsed;
          record.textItems.push(item.text);
          if (interruptAfterFirstDeltaMs !== null && record.textItems.length === 1) {
            triggerInterrupt(interruptAfterFirstDeltaMs);
          }
        }
        break;
      }
      case 'turn.completed':
        bump('turn.completed');
        record.terminal = 'completed';
        record.usage = event.usage ?? null;
        break;
      case 'turn.failed':
        bump('turn.failed');
        record.terminal = 'failed';
        record.errors.push(redact(JSON.stringify(event.error ?? event), secrets).slice(0, 400));
        break;
      case 'error':
        bump('error');
        record.transientErrors.push(redact(String(event.message || ''), secrets).slice(0, 300));
        break;
      default:
        bump(event.type || 'unknown');
        break;
    }
  };

  readLines(child.stdout, handleLine);
  readLines(child.stderr, (line) => {
    record.stderr = `${record.stderr}${redact(line, secrets)}\n`.slice(-4000);
  });

  child.stdin.write(`${prompt}\n`);
  child.stdin.end();

  const closePromise = waitClose(child, timeoutMs);
  const settled = await Promise.race([closePromise, interruptSignal]);
  if (settled && settled.elapsedMs !== undefined) {
    record.interrupt = settled;
    const finalClose = await waitClose(child, 10000);
    if (finalClose.kind === 'close') record.exitCode = finalClose.code;
    else if (finalClose.kind === 'spawn_error') record.errors.push(`spawn error: ${finalClose.error}`);
  } else if (settled && settled.kind === 'timeout') {
    record.timedOut = true;
    record.interrupt = await interruptTree(child);
  } else if (settled && settled.kind === 'spawn_error') {
    record.errors.push(`spawn error: ${settled.error}`);
  } else if (settled) {
    record.exitCode = settled.code;
  }
  if (interruptPromise && !record.interrupt) {
    record.interrupt = await interruptPromise;
  }

  if (record.terminal === 'none' && !record.timedOut && record.exitCode !== null) {
    record.terminal = 'failed';
    record.errors.push(`进程退出（code ${record.exitCode}）但未产出 turn.completed/turn.failed 终态事件`);
  }

  record.wallClockMs = nowMs() - start;
  record.finalText = record.textItems.length ? record.textItems[record.textItems.length - 1] : null;
  record.streamText = record.textItems.join('');
  record.baselineMatch =
    baselineText === null || record.finalText === null
      ? null
      : normalizeText(record.streamText) === normalizeText(baselineText);
  return record;
}

/** 非流式对照调用：`codex exec -o <file>`（官方 CLI 的 last-message 落盘）。 */
export async function runControl(cli, config, prompt, cwd, timeoutMs) {
  const dir = makeTmpDir('m1-11-codex-control');
  const outFile = join(dir, 'last-message.txt');
  const start = nowMs();
  const child = spawnCli(cli, ['exec', ...commonArgs(config, cwd), '-o', outFile, '-'], {
    cwd,
    env: spawnEnv(),
    stdio: ['pipe', 'pipe', 'pipe'],
    windowsHide: true,
  });
  let stderr = '';
  readLines(child.stdout, () => {});
  readLines(child.stderr, (line) => {
    stderr += `${line}\n`;
  });
  child.stdin.write(`${prompt}\n`);
  child.stdin.end();
  const closeOutcome = await waitClose(child, timeoutMs);
  let finalText = null;
  try {
    finalText = readFileSync(outFile, 'utf8');
  } catch {
    /* 文件可能未生成 */
  }
  try {
    unlinkSync(outFile);
  } catch {
    /* ignore */
  }
  return {
    candidate: 'codex',
    mode: 'control',
    terminal: finalText ? 'completed' : 'none',
    finalText,
    wallClockMs: nowMs() - start,
    exitCode: closeOutcome.code ?? null,
    timedOut: closeOutcome.kind === 'timeout',
    stderr: stderr.slice(0, 2000),
  };
}

/** 诊断：直接探测 Responses 兼容端点的流式行为（解释 codex exec 成败根因）。 */
export async function probeResponsesStream({ baseUrl, apiKey, model, timeoutMs = 60000, prompt }) {
  const startedAt = nowMs();
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  const result = {
    mode: 'stream-probe',
    baseUrl,
    model,
    status: null,
    contentType: null,
    ttfbMs: null,
    eventCount: 0,
    bytes: 0,
    completed: false,
    elapsedMs: null,
    error: null,
  };
  try {
    const response = await fetch(`${baseUrl.replace(/\/+$/, '')}/responses`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: `Bearer ${apiKey}` },
      body: JSON.stringify({ model, input: prompt || 'Reply with exactly: PONG', stream: true }),
      signal: controller.signal,
    });
    result.status = response.status;
    result.contentType = response.headers.get('content-type');
    result.ttfbMs = nowMs() - startedAt;
    if (!response.body) {
      result.error = '响应无 body';
      return result;
    }
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      result.bytes += value.length;
      buffer += decoder.decode(value, { stream: true });
      let index;
      while ((index = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, index).trim();
        buffer = buffer.slice(index + 1);
        if (line.startsWith('event:') || line.startsWith('data:')) result.eventCount += 1;
        if (line.includes('response.completed')) result.completed = true;
      }
      if (result.completed) break;
    }
  } catch (error) {
    result.error = error.name === 'AbortError' ? `超时（${timeoutMs}ms）` : error.message;
  } finally {
    clearTimeout(timer);
    result.elapsedMs = nowMs() - startedAt;
  }
  return result;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const mode = typeof args.mode === 'string' ? args.mode : 'fresh';
  const runs = Number(args.runs || 1);
  const timeoutMs = Number(args['timeout-ms'] || 240000);
  const prompt = typeof args.prompt === 'string' ? args.prompt : DEFAULT_PROMPT;
  const cwd = typeof args.cwd === 'string' ? args.cwd : process.cwd();
  const outFile = typeof args.out === 'string' ? args.out : null;

  const cliOverride = envFirst(['M1_11_CODEX_BIN'], '');
  const cli =
    cliOverride && /[\\/]/.test(cliOverride)
      ? { path: cliOverride, args: [], viaShell: false, resolvedPath: cliOverride }
      : resolveCli(cliOverride || 'codex');
  const config = codexConfig();
  const secrets = collectSecrets('M1_11_CODEX_TOKEN', 'OPENAI_API_KEY', 'M1_11_CODEX_API_KEY');
  const results = [];

  if (mode === 'stream-probe') {
    const baseUrl = envFirst(['M1_11_CODEX_BASE_URL'], '');
    const apiKey = envFirst(['M1_11_CODEX_API_KEY', 'OPENAI_API_KEY'], '');
    if (!baseUrl || !apiKey) {
      throw new Error('stream-probe 需要 M1_11_CODEX_BASE_URL 与 M1_11_CODEX_API_KEY（或 OPENAI_API_KEY）');
    }
    const probe = await probeResponsesStream({
      baseUrl,
      apiKey,
      model: config.model || 'gpt-5.5',
      timeoutMs: Number(args['timeout-ms'] || 60000),
      prompt: typeof args.prompt === 'string' ? args.prompt : undefined,
    });
    results.push(probe);
  } else if (mode === 'control') {
    results.push(await runControl(cli, config, prompt, cwd, timeoutMs));
  } else if (mode === 'continuation') {
    const first = await runCodexOnce({
      cli,
      config,
      prompt: 'Remember this passphrase: AETHER-SPIKE-41. Reply with exactly: STORED',
      cwd,
      timeoutMs,
      secrets,
    });
    const second = await runCodexOnce({
      cli,
      config,
      prompt: 'What passphrase did I ask you to remember? Reply with just the passphrase.',
      threadId: first.threadId,
      cwd,
      timeoutMs,
      secrets,
    });
    results.push(first, second);
  } else if (mode === 'interrupt') {
    const record = await runCodexOnce({
      cli,
      config,
      prompt: typeof args.prompt === 'string' ? args.prompt : INTERRUPT_PROMPT,
      cwd,
      timeoutMs,
      interruptAfterFirstDeltaMs: Number(args['interrupt-after-ms'] || 1500),
      secrets,
    });
    results.push(record);
  } else {
    for (let index = 0; index < runs; index += 1) {
      const record = await runCodexOnce({ cli, config, prompt, cwd, timeoutMs, secrets });
      record.runIndex = index + 1;
      results.push(record);
    }
  }

  const summary = { candidate: 'codex', mode, config: { ...config, model: config.model || '(codex config)' }, results };
  if (outFile) writeJson(outFile, summary);
  console.log(JSON.stringify(summary, null, 2));
}

const isDirectRun = process.argv[1] && process.argv[1].replace(/\\/g, '/').endsWith('/codex-spike.mjs');
if (isDirectRun) {
  main().catch((error) => {
    console.error(`[codex-spike] ${redact(error?.stack || error, collectSecrets('M1_11_CODEX_TOKEN', 'OPENAI_API_KEY'))}`);
    process.exitCode = 1;
  });
}

export { randomUUID };
