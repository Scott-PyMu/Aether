/**
 * M1-11 A1 spike —— Claude Code 最小可编程接入样例（官方 CLI，无第三方依赖）。
 *
 * 覆盖：
 *  - createSession：`--session-id <uuid>` 显式建会话（首轮），后续 `--resume <id>` 续会话
 *  - sendMessage：`-p --output-format stream-json --include-partial-messages --verbose`
 *  - 终态：`result` 事件（is_error/subtype）或进程退出；超时视为挂起
 *  - 对照基线：同一 run 的 `result.result`（官方非流式聚合结果）；另见 control 模式
 *  - 中断：进程树回收（taskkill /T /F | SIGINT 进程组），实测「发起 → 退出」耗时
 *
 * 用法：node claude-code-spike.mjs [--runs 1] [--prompt "..."] [--mode fresh|continuation|interrupt|control]
 * 环境变量：M1_11_CLAUDE_BASE_URL / M1_11_CLAUDE_TOKEN / M1_11_CLAUDE_MODEL
 *          （兼容 ANTHROPIC_BASE_URL / ANTHROPIC_AUTH_TOKEN / ANTHROPIC_MODEL）
 */
import { randomUUID } from 'node:crypto';
import { unlinkSync, writeFileSync } from 'node:fs';
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

export function claudeConfig() {
  const baseUrl = envFirst(['M1_11_CLAUDE_BASE_URL', 'ANTHROPIC_BASE_URL'], '');
  const token = envFirst(['M1_11_CLAUDE_TOKEN', 'ANTHROPIC_AUTH_TOKEN', 'ANTHROPIC_API_KEY'], '');
  const model = envFirst(['M1_11_CLAUDE_MODEL', 'ANTHROPIC_MODEL'], 'deepseek-v4-pro');
  if (!baseUrl) throw new Error('缺少 M1_11_CLAUDE_BASE_URL（或 ANTHROPIC_BASE_URL）');
  if (!token) throw new Error('缺少 M1_11_CLAUDE_TOKEN（或 ANTHROPIC_AUTH_TOKEN / ANTHROPIC_API_KEY）');
  return { baseUrl, token, model };
}

/** 单次 print 调用（一次 createSession/sendMessage）。 */
export async function runClaudeOnce(options) {
  const {
    cli,
    config,
    settingsFile,
    prompt,
    sessionId,
    resume = false,
    timeoutMs = 180000,
    interruptAfterFirstDeltaMs = null,
    interruptFallbackMs = 8000,
    cwd = process.cwd(),
    extraArgs = [],
    secrets = [],
    baselineText = null,
  } = options;

  const args = [
    '--setting-sources',
    'local',
    '--settings',
    settingsFile,
    '--strict-mcp-config',
    '--tools',
    '',
    '--max-turns',
    '1',
    '--model',
    config.model,
    ...(sessionId ? (resume ? ['--resume', sessionId] : ['--session-id', sessionId]) : []),
    ...extraArgs,
    '-p',
    '--output-format',
    'stream-json',
    '--include-partial-messages',
    '--verbose',
  ];

  const record = {
    candidate: 'claude-code',
    sessionId: sessionId || null,
    resumed: Boolean(resume),
    model: config.model,
    startedAt: new Date().toISOString(),
    terminal: 'none',
    timedOut: false,
    streamText: '',
    finalText: null,
    assistantText: null,
    isError: null,
    subtype: null,
    apiErrorStatus: null,
    numTurns: null,
    usage: null,
    durationMs: null,
    durationApiMs: null,
    totalCostUsd: null,
    firstTextMs: null,
    lastTextMs: null,
    textDeltaCount: 0,
    thinkingDeltaCount: 0,
    eventCounts: {},
    errors: [],
    stderr: '',
    exitCode: null,
    interrupt: null,
    baselineMatch: null,
    controlMatch: null,
    wallClockMs: null,
  };

  const start = nowMs();
  const child = spawnCli(cli, args, {
    cwd,
    env: { ...process.env },
    stdio: ['pipe', 'pipe', 'pipe'],
    windowsHide: true,
  });
  // 进程身份：供「重启进程 → native_id 恢复」步骤断言（DoD2④）。
  record.pid = child.pid ?? null;

  let assistantMessages = [];
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
    switch (event.type) {
      case 'system':
        bump(`system:${event.subtype || ''}`);
        if (event.subtype === 'init' && event.session_id) record.sessionId = event.session_id;
        break;
      case 'stream_event': {
        const inner = event.event || {};
        bump(`stream_event:${inner.type || ''}`);
        if (inner.type === 'content_block_delta') {
          const deltaType = inner.delta?.type;
          if (deltaType === 'text_delta' && typeof inner.delta.text === 'string') {
            if (record.firstTextMs === null) record.firstTextMs = elapsed;
            record.lastTextMs = elapsed;
            record.textDeltaCount += 1;
            record.streamText += inner.delta.text;
            if (interruptAfterFirstDeltaMs !== null && record.textDeltaCount === 1) {
              triggerInterrupt(interruptAfterFirstDeltaMs);
            }
          } else if (deltaType === 'thinking_delta') {
            record.thinkingDeltaCount += 1;
          }
        }
        break;
      }
      case 'assistant':
        bump('assistant');
        if (event.session_id) record.sessionId = event.session_id;
        if (event.message?.content) {
          const text = event.message.content
            .filter((block) => block?.type === 'text')
            .map((block) => block.text)
            .join('');
          if (text) assistantMessages.push(text);
        }
        break;
      case 'user':
        bump('user');
        if (event.session_id) record.sessionId = event.session_id;
        break;
      case 'result':
        bump('result');
        record.terminal = event.is_error ? 'failed' : 'completed';
        record.isError = Boolean(event.is_error);
        record.subtype = event.subtype || null;
        record.apiErrorStatus = event.api_error_status ?? null;
        record.numTurns = event.num_turns ?? null;
        record.usage = event.usage ?? null;
        record.durationMs = event.duration_ms ?? null;
        record.durationApiMs = event.duration_api_ms ?? null;
        record.totalCostUsd = event.total_cost_usd ?? null;
        record.finalText = typeof event.result === 'string' ? event.result : null;
        break;
      default:
        bump(event.type || 'unknown');
        break;
    }
  };

  readLines(child.stdout, handleLine);
  readLines(child.stderr, (line) => {
    record.stderr = `${record.stderr}${line}\n`.slice(-4000);
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
    record.errors.push(`进程退出（code ${record.exitCode}）但未产出 result 终态事件`);
  }

  record.wallClockMs = nowMs() - start;
  record.assistantText = assistantMessages.length ? assistantMessages[assistantMessages.length - 1] : null;
  record.baselineMatch =
    record.finalText === null ? null : normalizeText(record.streamText) === normalizeText(record.finalText);
  record.controlMatch =
    baselineText === null || record.finalText === null
      ? null
      : normalizeText(record.finalText) === normalizeText(baselineText);
  return record;
}

/** 用临时 settings.json 承载鉴权（密钥不落仓库；run 结束后删除）。 */
export function prepareClaudeSettings() {
  const config = claudeConfig();
  const dir = makeTmpDir('m1-11-claude');
  const settingsFile = join(dir, 'settings.json');
  const env = {
    ANTHROPIC_BASE_URL: config.baseUrl,
    ANTHROPIC_AUTH_TOKEN: config.token,
    ANTHROPIC_MODEL: config.model,
    ANTHROPIC_DEFAULT_HAIKU_MODEL: config.model,
    ANTHROPIC_DEFAULT_SONNET_MODEL: config.model,
    ANTHROPIC_DEFAULT_OPUS_MODEL: config.model,
  };
  writeFileSync(
    settingsFile,
    JSON.stringify({ env, includeCoAuthoredBy: false, permissions: { allow: [], deny: [] } }),
    'utf8',
  );
  return {
    config,
    settingsFile,
    secrets: collectSecrets('M1_11_CLAUDE_TOKEN', 'ANTHROPIC_AUTH_TOKEN', 'ANTHROPIC_API_KEY'),
    cleanup: () => {
      try {
        unlinkSync(settingsFile);
      } catch {
        /* ignore */
      }
    },
  };
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const mode = typeof args.mode === 'string' ? args.mode : 'fresh';
  const runs = Number(args.runs || 1);
  const timeoutMs = Number(args['timeout-ms'] || 180000);
  const prompt = typeof args.prompt === 'string' ? args.prompt : DEFAULT_PROMPT;
  const outFile = typeof args.out === 'string' ? args.out : null;

  const cli = resolveCli('claude');
  const prepared = prepareClaudeSettings();
  const results = [];
  try {
    if (mode === 'control') {
      const record = await runControl(prepared, cli, prompt, timeoutMs);
      results.push(record);
    } else if (mode === 'continuation') {
      const sessionId = randomUUID();
      const first = await runClaudeOnce({
        cli,
        ...prepared,
        prompt: 'Remember this passphrase: AETHER-SPIKE-41. Reply with exactly: STORED',
        sessionId,
        timeoutMs,
      });
      const second = await runClaudeOnce({
        cli,
        ...prepared,
        prompt: 'What passphrase did I ask you to remember? Reply with just the passphrase.',
        sessionId,
        resume: true,
        timeoutMs,
      });
      results.push(first, second);
    } else if (mode === 'interrupt') {
      const record = await runClaudeOnce({
        cli,
        ...prepared,
        prompt: typeof args.prompt === 'string' ? args.prompt : INTERRUPT_PROMPT,
        sessionId: randomUUID(),
        timeoutMs,
        interruptAfterFirstDeltaMs: Number(args['interrupt-after-ms'] || 1500),
      });
      results.push(record);
    } else {
      for (let index = 0; index < runs; index += 1) {
        const record = await runClaudeOnce({
          cli,
          ...prepared,
          prompt,
          sessionId: randomUUID(),
          timeoutMs,
        });
        record.runIndex = index + 1;
        results.push(record);
      }
    }
  } finally {
    prepared.cleanup();
  }

  const summary = { candidate: 'claude-code', mode, results };
  if (outFile) writeJson(outFile, summary);
  console.log(JSON.stringify(summary, null, 2));
}

/** 非流式对照调用：`-p --output-format json`（官方非流式结果）。 */
export async function runControl(prepared, cli, prompt, timeoutMs, cwd = process.cwd()) {
  const start = nowMs();
  const child = spawnCli(
    cli,
    [
      '--setting-sources',
      'local',
      '--settings',
      prepared.settingsFile,
      '--strict-mcp-config',
      '--tools',
      '',
      '--max-turns',
      '1',
      '--model',
      prepared.config.model,
      '-p',
      '--output-format',
      'json',
    ],
    { cwd, env: { ...process.env }, stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true },
  );
  let stdout = '';
  let stderr = '';
  readLines(child.stdout, (line) => {
    stdout += `${line}\n`;
  });
  readLines(child.stderr, (line) => {
    stderr += `${line}\n`;
  });
  child.stdin.write(`${prompt}\n`);
  child.stdin.end();
  const closeOutcome = await waitClose(child, timeoutMs);
  let result = null;
  for (const line of stdout.split(/\r?\n/)) {
    if (!line.trim()) continue;
    try {
      const event = JSON.parse(line);
      if (event.type === 'result') result = event;
    } catch {
      /* ignore */
    }
  }
  return {
    candidate: 'claude-code',
    mode: 'control',
    terminal: result ? (result.is_error ? 'failed' : 'completed') : 'none',
    isError: result?.is_error ?? null,
    apiErrorStatus: result?.api_error_status ?? null,
    finalText: typeof result?.result === 'string' ? result.result : null,
    usage: result?.usage ?? null,
    durationMs: result?.duration_ms ?? null,
    wallClockMs: nowMs() - start,
    exitCode: closeOutcome.code ?? null,
    timedOut: closeOutcome.kind === 'timeout',
    stderr: stderr.slice(0, 2000),
  };
}

const isDirectRun =
  process.argv[1] && process.argv[1].replace(/\\/g, '/').endsWith('/claude-code-spike.mjs');
if (isDirectRun) {
  main().catch((error) => {
    console.error(`[claude-code-spike] ${redact(error?.stack || error, collectSecrets('M1_11_CLAUDE_TOKEN', 'ANTHROPIC_AUTH_TOKEN'))}`);
    process.exitCode = 1;
  });
}
