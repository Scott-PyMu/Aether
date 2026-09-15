/**
 * M1-11 A1 spike —— DeepSeek Harness（DSH）最小可编程接入样例（官方 CLI，无第三方依赖）。
 *
 * 覆盖：
 *  - createSession/sendMessage：`dsh --profile headless <task>`，headless 为官方一次性直连驱动，
 *    每次调用内部创建新会话，任务结束后打印最终 assistant 文本并退出（0=completed / 1=failed）。
 *  - 终态：进程退出码 + stderr `dsh: <code>: <message>`；超时视为挂起。
 *  - 对照基线：headless 输出即最终文本（无流式），与 control 运行文本比对。
 *  - 中断：进程树回收（taskkill /T /F | SIGINT 进程组），实测「发起 → 退出」耗时。
 *
 * 已知限制（实测记录，非本脚本缺陷）：
 *  - headless profile 无 token/增量输出（stdout 仅最终消息，见 @deepseek-ai/dsh-headless 源码 summarize()）；
 *  - headless CLI 无 `--resume`/`--session-id`（会话 id 由内部生成且不落 stdout）；
 *  - web profile 的 /api（HTTP + events.mux/events.host 下行 WS）是浏览器载体（browser-trust fence、
 *    无鉴权），不属于最小 spike 范围，若 DSH 进入 MVP 需单独立项评估（A1 降级路径 2 的 PTY 方案同理）。
 *
 * 用法：node deepseek-harness-spike.mjs [--runs 1] [--prompt "..."] [--mode fresh|interrupt]
 * 环境变量：M1_11_DSH_PROFILE（默认 headless）、M1_11_DSH_EXTRA_ARGS（JSON 数组追加 CLI 参数）
 */
import {
  collectSecrets,
  envFirst,
  interruptTree,
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

export function dshConfig() {
  const profile = envFirst(['M1_11_DSH_PROFILE'], 'headless');
  let extraArgs = [];
  const rawExtra = process.env.M1_11_DSH_EXTRA_ARGS;
  if (rawExtra) {
    const parsed = JSON.parse(rawExtra);
    if (!Array.isArray(parsed)) throw new Error('M1_11_DSH_EXTRA_ARGS 必须是 JSON 数组');
    extraArgs = parsed.map(String);
  }
  return { profile, extraArgs };
}

/** 单次 headless 调用（一次 createSession/sendMessage）。 */
export async function runDshOnce(options) {
  const {
    cli,
    config,
    prompt,
    cwd = process.cwd(),
    timeoutMs = 240000,
    interruptAfterMs = null,
    secrets = [],
    baselineText = null,
  } = options;

  const args = ['--profile', config.profile, ...config.extraArgs, prompt];
  const record = {
    candidate: 'deepseek-harness',
    profile: config.profile,
    startedAt: new Date().toISOString(),
    terminal: 'none',
    timedOut: false,
    text: '',
    firstByteMs: null,
    lastByteMs: null,
    eventCounts: { stdout_chunks: 0, stderr_lines: 0 },
    errors: [],
    stderr: '',
    exitCode: null,
    interrupt: null,
    baselineMatch: null,
    streaming: false,
    wallClockMs: null,
  };

  const start = nowMs();
  const child = spawnCli(cli, args, {
    cwd,
    env: { ...process.env },
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true,
  });

  let closed = false;
  let interruptPromise = null;
  let timer = null;
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
    if (timer) clearTimeout(timer);
  });
  if (interruptAfterMs !== null) {
    timer = setTimeout(() => triggerInterrupt(0), interruptAfterMs);
  }

  child.stdout.on('data', (chunk) => {
    const at = nowMs() - start;
    if (record.firstByteMs === null) record.firstByteMs = at;
    record.lastByteMs = at;
    record.eventCounts.stdout_chunks += 1;
    record.text += chunk.toString('utf8');
  });
  readLines(child.stderr, (line) => {
    record.eventCounts.stderr_lines += 1;
    record.stderr = `${record.stderr}${redact(line, secrets)}\n`.slice(-4000);
  });

  const closePromise = waitClose(child, timeoutMs);
  const settled = await Promise.race([closePromise, interruptSignal]);
  if (settled && settled.elapsedMs !== undefined) {
    record.interrupt = settled;
    const finalClose = await waitClose(child, 10000);
    if (finalClose.kind === 'close') record.exitCode = finalClose.code;
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

  record.text = normalizeText(record.text);
  if (record.timedOut) {
    record.terminal = 'none';
  } else if (record.exitCode === 0) {
    record.terminal = 'completed';
  } else if (record.exitCode !== null) {
    record.terminal = 'failed';
    const errorLine = record.stderr.trim().split(/\r?\n/).pop();
    if (errorLine) record.errors.push(redact(errorLine, secrets).slice(0, 300));
  }
  record.baselineMatch =
    baselineText === null || record.terminal !== 'completed'
      ? null
      : normalizeText(record.text) === normalizeText(baselineText);
  record.wallClockMs = nowMs() - start;
  return record;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const mode = typeof args.mode === 'string' ? args.mode : 'fresh';
  const runs = Number(args.runs || 1);
  const timeoutMs = Number(args['timeout-ms'] || 240000);
  const prompt = typeof args.prompt === 'string' ? args.prompt : DEFAULT_PROMPT;
  const cwd = typeof args.cwd === 'string' ? args.cwd : process.cwd();
  const outFile = typeof args.out === 'string' ? args.out : null;

  const cliOverride = envFirst(['M1_11_DSH_BIN'], '');
  const cli =
    cliOverride && /[\\/]/.test(cliOverride)
      ? { path: cliOverride, args: [], viaShell: false, resolvedPath: cliOverride }
      : resolveCli(cliOverride || 'dsh');
  const config = dshConfig();
  const secrets = collectSecrets('M1_11_DSH_API_KEY', 'STREAMAX_API_KEY', 'DEEPSEEK_API_KEY');
  const results = [];

  if (mode === 'interrupt') {
    results.push(
      await runDshOnce({
        cli,
        config,
        prompt:
          typeof args.prompt === 'string'
            ? args.prompt
            : 'Write an extremely long detailed essay about distributed systems. At least 3000 words. Do not stop early.',
        cwd,
        timeoutMs,
        interruptAfterMs: Number(args['interrupt-after-ms'] || 2500),
        secrets,
      }),
    );
  } else {
    for (let index = 0; index < runs; index += 1) {
      const record = await runDshOnce({ cli, config, prompt, cwd, timeoutMs, secrets });
      record.runIndex = index + 1;
      results.push(record);
    }
  }

  const summary = { candidate: 'deepseek-harness', mode, config, results };
  if (outFile) writeJson(outFile, summary);
  console.log(JSON.stringify(summary, null, 2));
}

const isDirectRun =
  process.argv[1] && process.argv[1].replace(/\\/g, '/').endsWith('/deepseek-harness-spike.mjs');
if (isDirectRun) {
  main().catch((error) => {
    console.error(`[deepseek-harness-spike] ${redact(error?.stack || error, collectSecrets('M1_11_DSH_API_KEY', 'STREAMAX_API_KEY'))}`);
    process.exitCode = 1;
  });
}
