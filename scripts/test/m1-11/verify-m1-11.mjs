/**
 * M1-11 A1 spike —— DoD 逐条验证编排器（可重复执行；证据落 scripts/test/.tmp/m1-11/，已 gitignore）。
 *
 * 覆盖 DoD：
 *  - DoD1（脚本可重复执行 + opt-in CI job）：检查样例脚本与 .github/workflows/spike-m1-11.yml 存在
 *  - DoD2①②③（≥1 候选连续 N=20 次 run）：终态无挂起 / 拼接流式文本对照基线一致 / 中断 5s 内生效
 *  - DoD2④（会话恢复）：重启进程 → 用 `sessions.config.native_id` 恢复原生会话 → 续聊；
 *    支持 = Mode R，不支持/仅能新建会话 = Mode N（ADR-005）——决定 M2-02 重放语义
 *  - DoD2 尾（首 token 与吞吐基线）：统计并写入基线文件
 *  - DoD3（接入笔记）：检查 docs/spike/M1-11-接入笔记.md 与必需小节（含「会话恢复能力（Mode R/N）」）
 *
 * 用法：node verify-m1-11.mjs [--runs 20] [--candidate all|both|claude|codex|dsh|dsh-acp] [--timeout-ms 240000] [--evidence-dir <p>]
 * 退出码：0 = 至少一个候选 ①②③ 全过且交付物检查通过；1 = 不满足（含全部候选不可用）。
 */
import { readFileSync, existsSync, appendFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { collectSecrets, ensureDir, parseArgs, redact, resolveCli, sleep, stats, writeJson } from './lib.mjs';
import {
  DEFAULT_PROMPT as CLAUDE_PROMPT,
  prepareClaudeSettings,
  runClaudeOnce,
  runControl as runClaudeControl,
} from './claude-code-spike.mjs';
import {
  DEFAULT_PROMPT as CODEX_PROMPT,
  codexConfig,
  runCodexOnce,
  runControl as runCodexControl,
} from './codex-spike.mjs';
import {
  DEFAULT_PROMPT as DSH_PROMPT,
  dshConfig,
  runDshOnce,
} from './deepseek-harness-spike.mjs';
import {
  DEFAULT_PROMPT as DSH_ACP_PROMPT,
  dshAcpConfig,
  ensureAcpPatch,
  runAcpOnce,
} from './deepseek-harness-acp-spike.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, '..', '..', '..');
const EVIDENCE_ROOT = join(REPO_ROOT, 'scripts', 'test', '.tmp', 'm1-11');
const NOTES_FILE = join(REPO_ROOT, 'docs', 'spike', 'M1-11-接入笔记.md');
const CI_JOB_FILE = join(REPO_ROOT, '.github', 'workflows', 'spike-m1-11.yml');

const CONTINUE_PROMPT =
  'What passphrase did I ask you to remember? Reply with just the passphrase.';
const CONTINUE_MARKER = 'AETHER-SPIKE-41';

function cliVersion(cli) {
  if (!cli) return null;
  const result = cli.viaShell
    ? spawnSync(process.env.ComSpec || 'cmd.exe', ['/d', '/s', '/c', `"${cli.path}" --version`], {
        encoding: 'utf8',
        windowsVerbatimArguments: true,
      })
    : spawnSync(cli.path, [...(cli.args || []), '--version'], { encoding: 'utf8' });
  const text = `${result.stdout || ''}${result.stderr || ''}`.trim().split(/\r?\n/)[0] || '';
  return text || null;
}

function resolveCliFallback(name) {
  try {
    return resolveCli(name);
  } catch {
    return null;
  }
}

function checkArtifacts() {
  const samples = [
    'lib.mjs',
    'claude-code-spike.mjs',
    'codex-spike.mjs',
    'deepseek-harness-spike.mjs',
    'deepseek-harness-acp-spike.mjs',
    'verify-m1-11.mjs',
  ].map((file) => ({
    file: `scripts/test/m1-11/${file}`,
    exists: existsSync(join(HERE, file)),
  }));
  let ciJob = { file: '.github/workflows/spike-m1-11.yml', exists: false, opt_in: false };
  if (existsSync(CI_JOB_FILE)) {
    const content = readFileSync(CI_JOB_FILE, 'utf8');
    // 防回归：PowerShell here-string 内容/终止符与 JSON 载荷必须缩进在 run:| 块内；
    // 列 0 会截断 YAML 块标量导致 workflow 无法解析（GitHub 报 0-job 失败 run）。
    const columnZero = content
      .split(/\r?\n/)
      .map((line, index) => ({ line, index: index + 1 }))
      .filter(({ line }) => /^(@'|"@|'@|\{ ?")/.test(line));
    if (columnZero.length > 0) {
      console.error(
        `[m1-11] spike-m1-11.yml 存在列 0 的块内容行：${columnZero
          .map((item) => `L${item.index}`)
          .join(', ')}`,
      );
    }
    ciJob = {
      file: ciJob.file,
      exists: true,
      opt_in: /workflow_dispatch/.test(content),
      yaml_indentation_ok: columnZero.length === 0,
    };
  }
  const requiredSections = [
    '## 结论',
    '## 鉴权',
    '## 会话模型映射',
    '## 会话恢复能力（Mode R/N）',
    '## 权限映射',
    '## 对照基线',
    '## 已知坑',
  ];
  let notes = { file: 'docs/spike/M1-11-接入笔记.md', exists: false, sections: {} };
  if (existsSync(NOTES_FILE)) {
    const content = readFileSync(NOTES_FILE, 'utf8');
    notes = {
      file: notes.file,
      exists: true,
      sections: Object.fromEntries(requiredSections.map((section) => [section, content.includes(section)])),
    };
  }
  return { samples, ciJob, notes };
}

function appendJsonl(file, record, secrets) {
  ensureDir(dirname(file));
  appendFileSync(file, `${redact(JSON.stringify(record), secrets)}\n`, 'utf8');
}

/**
 * DoD2④ 会话恢复结论（ADR-005 Mode R/N）。
 *
 * 恢复用例 = 两次独立进程：首进程创建原生会话并记住口令（native_id 等价于
 * `sessions.config.native_id`）；进程退出后，第二个进程用 native_id 恢复原生会话并续聊。
 * 两个 pid 必须不同（证明「重启进程」），且续聊命中口令标记。
 * 支持恢复 = Mode R；不支持/仅能新建会话 = Mode N（决定 M2-02 重放语义）。
 */
function sessionRecovery({ nativeId, first, second, ok, note = null }) {
  const processRestart = Boolean(first?.pid && second?.pid && first.pid !== second.pid);
  const supported = Boolean(ok) && processRestart;
  return {
    native_id: nativeId ?? null,
    native_id_field: 'sessions.config.native_id',
    steps: ['创建原生会话（进程 1）', '进程 1 退出', 'native_id 恢复（进程 2）', '续聊命中'],
    process_restart: processRestart,
    first_pid: first?.pid ?? null,
    second_pid: second?.pid ?? null,
    resumed_requested: second?.resumed === true,
    second_terminal: second?.terminal ?? null,
    ok: supported,
    mode: supported ? 'R' : 'N',
    note,
  };
}

function summarizeRuns(records) {
  const firstTexts = records.map((r) => r.firstTextMs).filter((v) => Number.isFinite(v));
  const throughputs = records
    .map((r) => throughput(r))
    .filter((v) => Number.isFinite(v) && v > 0);
  return {
    first_token_ms: stats(firstTexts),
    output_tokens_per_s: stats(throughputs),
    wall_clock_ms: stats(records.map((r) => r.wallClockMs)),
  };
}

function throughput(record) {
  const tokens = record?.usage?.output_tokens;
  if (!Number.isFinite(tokens) || record.firstTextMs === null || record.lastTextMs === null) return null;
  const span = (record.lastTextMs - record.firstTextMs) / 1000;
  if (span <= 0) return null;
  return Math.round((tokens / span) * 100) / 100;
}

async function verifyClaude({ runs, timeoutMs, evidenceDir, secrets, log, cwd }) {
  const candidate = { candidate: 'claude-code', status: 'running' };
  let prepared;
  let claudeCli = null;
  try {
    claudeCli = resolveCli('claude');
    candidate.cliVersion = cliVersion(claudeCli);
    candidate.cliPath = claudeCli.resolvedPath || claudeCli.path;
    prepared = prepareClaudeSettings();
  } catch (error) {
    candidate.status = 'blocked';
    candidate.reason = redact(error.message, secrets);
    log(`[claude] 不可用：${candidate.reason}`);
    return candidate;
  }
  try {
    candidate.config = { baseUrl: prepared.config.baseUrl, model: prepared.config.model };
    log('[claude] 对照基线（非流式 --output-format json）…');
    const control = await runClaudeControl(prepared, claudeCli, CLAUDE_PROMPT, timeoutMs, cwd);
    candidate.control = {
      terminal: control.terminal,
      wallClockMs: control.wallClockMs,
      exitCode: control.exitCode,
      timedOut: control.timedOut,
      stderrPreview: redact((control.stderr || '').slice(0, 500), secrets),
      textPreview: redact((control.finalText || '').slice(0, 200), secrets),
    };
    const baselineText = control.finalText;

    const records = [];
    const runsFile = join(evidenceDir, 'claude-runs.jsonl');
    for (let index = 0; index < runs; index += 1) {
      const record = await runClaudeOnce({
        cli: claudeCli,
        ...prepared,
        prompt: CLAUDE_PROMPT,
        sessionId: randomUUID(),
        timeoutMs,
        cwd,
        baselineText,
      });
      record.runIndex = index + 1;
      records.push(record);
      appendJsonl(runsFile, record, secrets);
      log(`[claude] run ${index + 1}/${runs} → ${record.terminal} baseline=${record.baselineMatch} control=${record.controlMatch} first=${record.firstTextMs}ms`);
    }

    log('[claude] 会话恢复（DoD2④：重启进程 → native_id 恢复 → 续聊）…');
    const sessionId = randomUUID();
    const continuationFirst = await runClaudeOnce({
      cli: claudeCli,
      ...prepared,
      prompt: `Remember this passphrase: ${CONTINUE_MARKER}. Reply with exactly: STORED`,
      sessionId,
      timeoutMs,
      cwd,
    });
    const continuationSecond = await runClaudeOnce({
      cli: claudeCli,
      ...prepared,
      prompt: CONTINUE_PROMPT,
      sessionId,
      resume: true,
      timeoutMs,
      cwd,
    });
    appendJsonl(join(evidenceDir, 'claude-continuation.jsonl'), continuationFirst, secrets);
    appendJsonl(join(evidenceDir, 'claude-continuation.jsonl'), continuationSecond, secrets);

    log('[claude] 中断请求（首 token 后 500ms / 兜底 8s）…');
    const interruptRecord = await runClaudeOnce({
      cli: claudeCli,
      ...prepared,
      prompt: 'Write an extremely long detailed essay about distributed systems. At least 3000 words. Do not stop early.',
      sessionId: randomUUID(),
      timeoutMs,
      cwd,
      interruptAfterFirstDeltaMs: 500,
      interruptFallbackMs: 8000,
    });
    appendJsonl(join(evidenceDir, 'claude-interrupt.jsonl'), interruptRecord, secrets);

    const terminalCount = records.filter((r) => r.terminal !== 'none' && !r.timedOut).length;
    const completedCount = records.filter((r) => r.terminal === 'completed').length;
    const inRunMatches = records.filter((r) => r.baselineMatch === true).length;
    const controlMatches = records.filter((r) => r.controlMatch === true).length;
    const interruptOk = Number.isFinite(interruptRecord.interrupt?.elapsedMs) && interruptRecord.interrupt.elapsedMs < 5000;
    const continuationOk =
      continuationSecond.terminal === 'completed' &&
      (continuationSecond.streamText || '').includes(CONTINUE_MARKER);

    candidate.status = 'completed';
    candidate.runs = {
      requested: runs,
      terminal: terminalCount,
      completed: completedCount,
      failed: records.length - completedCount,
      hangs: records.filter((r) => r.timedOut).length,
      in_run_baseline_matches: inRunMatches,
      control_baseline_matches: controlMatches,
      first_text_delta_present: records.filter((r) => r.textDeltaCount > 0).length,
    };
    candidate.timing = summarizeRuns(records);
    candidate.interrupt = {
      requestedAfterFirstDeltaMs: 500,
      elapsedMs: interruptRecord.interrupt?.elapsedMs ?? null,
      within5s: Boolean(interruptOk),
      partialTextCharsAtInterrupt: interruptRecord.streamText.length,
      textDeltaCount: interruptRecord.textDeltaCount,
      exitCode: interruptRecord.exitCode,
    };
    const recovery = sessionRecovery({
      nativeId: sessionId,
      first: continuationFirst,
      second: continuationSecond,
      ok: continuationOk,
    });
    console.log(
      `[claude] 恢复模式 Mode ${recovery.mode}（process_restart=${recovery.process_restart}，` +
        `pid ${recovery.first_pid} → ${recovery.second_pid}）`,
    );
    candidate.session_continuation = {
      sessionId,
      firstTerminal: continuationFirst.terminal,
      secondTerminal: continuationSecond.terminal,
      secondTextPreview: redact((continuationSecond.streamText || '').slice(0, 120), secrets),
      ok: continuationOk,
      recovery_mode: recovery.mode,
    };
    candidate.session_recovery = recovery;
    candidate.checks = {
      terminal_no_hang: terminalCount === runs,
      in_run_baseline_consistent: inRunMatches === runs,
      control_baseline_consistent: controlMatches === runs,
      interrupt_within_5s: Boolean(interruptOk),
      session_continuation_ok: continuationOk,
      session_recovery_ok: recovery.ok,
      session_recovery_mode: recovery.mode,
      streaming_available: candidate.runs.first_text_delta_present === runs,
    };
    candidate.verdict =
      candidate.checks.terminal_no_hang &&
      candidate.checks.in_run_baseline_consistent &&
      candidate.checks.control_baseline_consistent &&
      candidate.checks.interrupt_within_5s &&
      candidate.checks.streaming_available
        ? 'pass'
        : candidate.checks.terminal_no_hang && candidate.checks.interrupt_within_5s
          ? 'partial'
          : 'fail';
    return candidate;
  } finally {
    prepared.cleanup();
  }
}

async function verifyCodex({ runs, timeoutMs, evidenceDir, secrets, log, cwd }) {
  const candidate = { candidate: 'codex', status: 'running' };
  let cli;
  let config;
  try {
    const binOverride = process.env.M1_11_CODEX_BIN || '';
    cli = binOverride && /[\\/]/.test(binOverride)
      ? { path: binOverride, args: [], viaShell: false, resolvedPath: binOverride }
      : resolveCli(binOverride || 'codex');
    candidate.cliVersion = cliVersion(cli);
    candidate.cliPath = cli.resolvedPath || cli.path;
    config = codexConfig();
  } catch (error) {
    candidate.status = 'blocked';
    candidate.reason = redact(error.message, secrets);
    log(`[codex] 不可用：${candidate.reason}`);
    return candidate;
  }
  candidate.config = { model: config.model || '(codex config)', reasoning: config.reasoning, sandbox: config.sandbox };

  log('[codex] 对照基线（非流式 -o last-message）…');
  const control = await runCodexControl(cli, config, CODEX_PROMPT, cwd, timeoutMs);
  candidate.control = {
    terminal: control.terminal,
    wallClockMs: control.wallClockMs,
    exitCode: control.exitCode,
    timedOut: control.timedOut,
    stderrPreview: redact((control.stderr || '').slice(0, 500), secrets),
    textPreview: redact((control.finalText || '').slice(0, 200), secrets),
  };
  const baselineText = control.finalText;

  const records = [];
  const runsFile = join(evidenceDir, 'codex-runs.jsonl');
  for (let index = 0; index < runs; index += 1) {
    const record = await runCodexOnce({
      cli,
      config,
      prompt: CODEX_PROMPT,
      cwd,
      timeoutMs,
      secrets,
      baselineText,
    });
    record.runIndex = index + 1;
    records.push(record);
    appendJsonl(runsFile, record, secrets);
    log(`[codex] run ${index + 1}/${runs} → ${record.terminal} baseline=${record.baselineMatch} first=${record.firstTextMs}ms`);
  }

  log('[codex] 会话恢复（DoD2④：重启进程 → native_id（thread_id）恢复 → 续聊）…');
  const continuationFirst = await runCodexOnce({
    cli,
    config,
    prompt: `Remember this passphrase: ${CONTINUE_MARKER}. Reply with exactly: STORED`,
    cwd,
    timeoutMs,
    secrets,
  });
  const continuationSecond = await runCodexOnce({
    cli,
    config,
    prompt: CONTINUE_PROMPT,
    threadId: continuationFirst.threadId,
    cwd,
    timeoutMs,
    secrets,
  });
  appendJsonl(join(evidenceDir, 'codex-continuation.jsonl'), continuationFirst, secrets);
  appendJsonl(join(evidenceDir, 'codex-continuation.jsonl'), continuationSecond, secrets);

  log('[codex] 中断请求（首文本后 500ms / 兜底 8s）…');
  const interruptRecord = await runCodexOnce({
    cli,
    config,
    prompt: 'Write an extremely long detailed essay about distributed systems. At least 3000 words. Do not stop early.',
    cwd,
    timeoutMs,
    interruptAfterFirstDeltaMs: 500,
    interruptFallbackMs: 8000,
    secrets,
  });
  appendJsonl(join(evidenceDir, 'codex-interrupt.jsonl'), interruptRecord, secrets);

  const terminalCount = records.filter((r) => r.terminal !== 'none' && !r.timedOut).length;
  const completedCount = records.filter((r) => r.terminal === 'completed').length;
  const controlMatches = records.filter((r) => r.baselineMatch === true).length;
  const interruptOk = Number.isFinite(interruptRecord.interrupt?.elapsedMs) && interruptRecord.interrupt.elapsedMs < 5000;
  const continuationOk =
    continuationSecond.terminal === 'completed' &&
    (continuationSecond.streamText || '').includes(CONTINUE_MARKER);

  candidate.status = 'completed';
  candidate.runs = {
    requested: runs,
    terminal: terminalCount,
    completed: completedCount,
    failed: records.length - completedCount,
    hangs: records.filter((r) => r.timedOut).length,
    control_baseline_matches: controlMatches,
    agent_message_items_per_run: records.map((r) => r.textItems.length),
    text_delta_events_available: records.some((r) => r.updatedTextSeen),
  };
  candidate.timing = summarizeRuns(records);
  candidate.interrupt = {
    requestedAfterFirstDeltaMs: 500,
    elapsedMs: interruptRecord.interrupt?.elapsedMs ?? null,
    within5s: Boolean(interruptOk),
    textItemsAtInterrupt: interruptRecord.textItems.length,
    eventCountsAtInterrupt: interruptRecord.eventCounts,
    exitCode: interruptRecord.exitCode,
  };
  const recovery = sessionRecovery({
    nativeId: continuationFirst.threadId,
    first: continuationFirst,
    second: continuationSecond,
    ok: continuationOk,
  });
  console.log(
    `[codex] 恢复模式 Mode ${recovery.mode}（process_restart=${recovery.process_restart}，` +
      `pid ${recovery.first_pid} → ${recovery.second_pid}）`,
  );
  candidate.session_continuation = {
    threadId: continuationFirst.threadId,
    firstTerminal: continuationFirst.terminal,
    secondTerminal: continuationSecond.terminal,
    secondTextPreview: redact((continuationSecond.streamText || '').slice(0, 120), secrets),
    ok: continuationOk,
    recovery_mode: recovery.mode,
  };
  candidate.session_recovery = recovery;
  candidate.checks = {
    terminal_no_hang: terminalCount === runs,
    control_baseline_consistent: controlMatches === runs,
    interrupt_within_5s: Boolean(interruptOk),
    session_continuation_ok: continuationOk,
    session_recovery_ok: recovery.ok,
    session_recovery_mode: recovery.mode,
    streaming_available: candidate.runs.text_delta_events_available,
  };
  candidate.verdict =
    candidate.checks.terminal_no_hang &&
    candidate.checks.control_baseline_consistent &&
    candidate.checks.interrupt_within_5s &&
    candidate.checks.streaming_available
      ? 'pass'
      : candidate.checks.terminal_no_hang && candidate.checks.interrupt_within_5s
        ? 'partial'
        : 'fail';
  return candidate;
}

async function verifyDsh({ runs, timeoutMs, evidenceDir, secrets, log, cwd }) {
  const candidate = { candidate: 'deepseek-harness', status: 'running' };
  let cli;
  let config;
  try {
    const binOverride = process.env.M1_11_DSH_BIN || '';
    cli = binOverride && /[\\/]/.test(binOverride)
      ? { path: binOverride, args: [], viaShell: false, resolvedPath: binOverride }
      : resolveCli(binOverride || 'dsh');
    candidate.cliVersion = cliVersion(cli);
    candidate.cliPath = cli.resolvedPath || cli.path;
    config = dshConfig();
  } catch (error) {
    candidate.status = 'blocked';
    candidate.reason = redact(error.message, secrets);
    log(`[dsh] 不可用：${candidate.reason}`);
    return candidate;
  }
  candidate.config = { profile: config.profile, extraArgs: config.extraArgs };

  log('[dsh] 对照基线（headless 一次性输出，作为非流式基线）…');
  const control = await runDshOnce({ cli, config, prompt: DSH_PROMPT, cwd, timeoutMs, secrets });
  candidate.control = {
    terminal: control.terminal,
    wallClockMs: control.wallClockMs,
    exitCode: control.exitCode,
    timedOut: control.timedOut,
    stderrPreview: redact((control.stderr || '').slice(0, 500), secrets),
    textPreview: redact((control.text || '').slice(0, 200), secrets),
  };
  const baselineText = control.terminal === 'completed' ? control.text : null;

  const records = [];
  const runsFile = join(evidenceDir, 'dsh-runs.jsonl');
  for (let index = 0; index < runs; index += 1) {
    const record = await runDshOnce({ cli, config, prompt: DSH_PROMPT, cwd, timeoutMs, secrets, baselineText });
    record.runIndex = index + 1;
    records.push(record);
    appendJsonl(runsFile, record, secrets);
    log(`[dsh] run ${index + 1}/${runs} → ${record.terminal} baseline=${record.baselineMatch} wall=${record.wallClockMs}ms`);
  }

  log('[dsh] 中断请求（固定延迟 2500ms，长任务提示词）…');
  const interruptRecord = await runDshOnce({
    cli,
    config,
    prompt: 'Write an extremely long detailed essay about distributed systems. At least 3000 words. Do not stop early.',
    cwd,
    timeoutMs,
    interruptAfterMs: 2500,
    secrets,
  });
  appendJsonl(join(evidenceDir, 'dsh-interrupt.jsonl'), interruptRecord, secrets);

  const terminalCount = records.filter((r) => r.terminal !== 'none' && !r.timedOut).length;
  const completedCount = records.filter((r) => r.terminal === 'completed').length;
  const baselineMatches = records.filter((r) => r.baselineMatch === true).length;
  const interruptOk = Number.isFinite(interruptRecord.interrupt?.elapsedMs) && interruptRecord.interrupt.elapsedMs < 5000;

  candidate.status = 'completed';
  candidate.runs = {
    requested: runs,
    terminal: terminalCount,
    completed: completedCount,
    failed: records.length - completedCount,
    hangs: records.filter((r) => r.timedOut).length,
    control_baseline_matches: baselineMatches,
    streaming_deltas_observed: records.filter((r) => r.eventCounts.stdout_chunks > 1).length,
  };
  candidate.timing = summarizeRuns(
    records.map((r) => ({
      ...r,
      firstTextMs: r.firstByteMs,
      lastTextMs: r.lastByteMs,
      usage: null,
    })),
  );
  candidate.interrupt = {
    requestedAfterMs: 2500,
    elapsedMs: interruptRecord.interrupt?.elapsedMs ?? null,
    within5s: Boolean(interruptOk),
    partialTextCharsAtInterrupt: interruptRecord.text.length,
    exitCode: interruptRecord.exitCode,
  };
  candidate.limitations = {
    streaming: 'headless profile 仅打印最终 assistant 文本（源码 summarize() 无增量事件）',
    session_resume: 'headless CLI 无 --resume/--session-id；web profile 的 /api 为浏览器载体（无鉴权、browser-trust fence）',
  };
  candidate.session_continuation = { supported: false, note: candidate.limitations.session_resume };
  candidate.session_recovery = {
    native_id: null,
    native_id_field: 'sessions.config.native_id',
    steps: ['创建原生会话（进程 1）', '不支持恢复'],
    process_restart: false,
    ok: false,
    mode: 'N',
    note: candidate.limitations.session_resume,
  };
  candidate.checks = {
    terminal_no_hang: terminalCount === runs,
    control_baseline_consistent: baselineMatches === runs,
    interrupt_within_5s: Boolean(interruptOk),
    session_continuation_ok: false,
    session_recovery_ok: false,
    session_recovery_mode: 'N',
    streaming_available: false,
  };
  candidate.verdict =
    candidate.checks.terminal_no_hang &&
    candidate.checks.control_baseline_consistent &&
    candidate.checks.interrupt_within_5s &&
    candidate.checks.streaming_available
      ? 'pass'
      : candidate.checks.terminal_no_hang && candidate.checks.interrupt_within_5s
        ? 'partial'
        : 'fail';
  return candidate;
}

async function verifyDshAcp({ runs, timeoutMs, evidenceDir, secrets, log, cwd }) {
  const candidate = { candidate: 'deepseek-harness-acp', status: 'running' };
  let config;
  let patch;
  try {
    config = dshAcpConfig();
    if (!config.bin) throw new Error('缺少 M1_11_DSH_ACP_BIN（@deepseek-ai/dsh@0.1.5-rc.2 的 lib/bin.js）');
    if (!config.home) throw new Error('缺少 M1_11_DSH_ACP_HOME（隔离 DSH_HOME）');
    patch = ensureAcpPatch(config);
    try {
      const anchor = join(dirname(config.bin), '..', 'package.json');
      candidate.cliVersion = JSON.parse(readFileSync(anchor, 'utf8')).version || null;
    } catch {
      candidate.cliVersion = null;
    }
  } catch (error) {
    candidate.status = 'blocked';
    candidate.reason = redact(error.message, secrets);
    log(`[dsh-acp] 不可用：${candidate.reason}`);
    return candidate;
  }
  candidate.config = {
    profile: config.profile,
    provider: config.provider,
    model: config.model,
    home: config.home,
    patch: patch,
  };

  log('[dsh-acp] 对照基线（ACP 首次运行输出）…');
  const control = await runAcpOnce({ config, patch, prompt: DSH_ACP_PROMPT, cwd, timeoutMs, secrets });
  candidate.control = {
    terminal: control.terminal,
    stopReason: control.stopReason,
    wallClockMs: control.wallClockMs,
    textPreview: redact((control.streamText || '').slice(0, 200), secrets),
  };
  const baselineText = control.terminal === 'completed' ? control.streamText : null;

  const records = [];
  const runsFile = join(evidenceDir, 'dsh-acp-runs.jsonl');
  for (let index = 0; index < runs; index += 1) {
    const record = await runAcpOnce({ config, patch, prompt: DSH_ACP_PROMPT, cwd, timeoutMs, secrets, baselineText });
    record.runIndex = index + 1;
    records.push(record);
    appendJsonl(runsFile, record, secrets);
    log(`[dsh-acp] run ${index + 1}/${runs} → ${record.terminal}/${record.stopReason} baseline=${record.baselineMatch} first=${record.firstTextMs}ms`);
  }

  log('[dsh-acp] 会话恢复（DoD2④：重启进程 → session/resume 跨进程恢复 → 续聊）…');
  const continuationFirst = await runAcpOnce({
    config,
    patch,
    prompt: `Remember this passphrase: ${CONTINUE_MARKER}. Reply with exactly: STORED`,
    cwd,
    timeoutMs,
    secrets,
  });
  const continuationSecond = await runAcpOnce({
    config,
    patch,
    prompt: CONTINUE_PROMPT,
    sessionId: continuationFirst.sessionId,
    resume: true,
    cwd,
    timeoutMs,
    secrets,
  });
  appendJsonl(join(evidenceDir, 'dsh-acp-continuation.jsonl'), continuationFirst, secrets);
  appendJsonl(join(evidenceDir, 'dsh-acp-continuation.jsonl'), continuationSecond, secrets);

  log('[dsh-acp] 中断请求（协议级 session/cancel，首文本后 500ms / 兜底 8s）…');
  const interruptRecord = await runAcpOnce({
    config,
    patch,
    prompt: 'Write an extremely long detailed essay about distributed systems. At least 3000 words. Do not stop early.',
    cwd,
    timeoutMs,
    cancelAfterFirstTextMs: 500,
    cancelFallbackMs: 8000,
    secrets,
  });
  appendJsonl(join(evidenceDir, 'dsh-acp-interrupt.jsonl'), interruptRecord, secrets);

  const terminalCount = records.filter((r) => r.terminal !== 'none' && !r.timedOut).length;
  const completedCount = records.filter((r) => r.terminal === 'completed').length;
  const baselineMatches = records.filter((r) => r.baselineMatch === true).length;
  const interruptOk = Number.isFinite(interruptRecord.interrupt?.elapsedMs) && interruptRecord.interrupt.elapsedMs < 5000;
  const continuationOk =
    continuationSecond.terminal === 'completed' &&
    (continuationSecond.streamText || '').includes(CONTINUE_MARKER);
  const chunkCounts = records.map((r) => r.updates.agent_message_chunk);

  candidate.status = 'completed';
  candidate.runs = {
    requested: runs,
    terminal: terminalCount,
    completed: completedCount,
    failed: records.length - completedCount,
    hangs: records.filter((r) => r.timedOut).length,
    control_baseline_matches: baselineMatches,
    agent_message_chunks_per_run: chunkCounts,
    thought_chunks_present: records.filter((r) => r.updates.agent_thought_chunk > 0).length,
    usage_updates_present: records.filter((r) => r.updates.usage_update > 0).length,
  };
  candidate.timing = summarizeRuns(records);
  candidate.interrupt = {
    transport: 'session/cancel（协议级，无进程强杀）',
    elapsedMs: interruptRecord.interrupt?.elapsedMs ?? null,
    within5s: Boolean(interruptOk),
    stopReason: interruptRecord.stopReason,
    partialTextCharsAtInterrupt: interruptRecord.streamText.length,
  };
  candidate.limitations = {
    streaming: 'ACP 只发 committed 语义更新：一次 assistant 消息 = 一个 agent_message_chunk（无 token 级 delta）',
    reference: 'hermes-studio 为获得 token 级 delta 注入私有 Cordis 插件（_ekko/assistant_stream），属适配器实现，不在最小 spike 范围',
  };
  const recovery = sessionRecovery({
    nativeId: continuationFirst.sessionId,
    first: continuationFirst,
    second: continuationSecond,
    ok: continuationOk,
  });
  console.log(
    `[dsh-acp] 恢复模式 Mode ${recovery.mode}（process_restart=${recovery.process_restart}，` +
      `pid ${recovery.first_pid} → ${recovery.second_pid}）`,
  );
  candidate.session_continuation = {
    sessionId: continuationFirst.sessionId,
    firstTerminal: continuationFirst.terminal,
    secondTerminal: continuationSecond.terminal,
    secondTextPreview: redact((continuationSecond.streamText || '').slice(0, 120), secrets),
    ok: continuationOk,
    recovery_mode: recovery.mode,
  };
  candidate.session_recovery = recovery;
  candidate.checks = {
    terminal_no_hang: terminalCount === runs,
    control_baseline_consistent: baselineMatches === runs,
    interrupt_within_5s: Boolean(interruptOk),
    session_continuation_ok: continuationOk,
    session_recovery_ok: recovery.ok,
    session_recovery_mode: recovery.mode,
    streaming_available: chunkCounts.every((count) => count > 1),
  };
  candidate.verdict =
    candidate.checks.terminal_no_hang &&
    candidate.checks.control_baseline_consistent &&
    candidate.checks.interrupt_within_5s &&
    candidate.checks.streaming_available
      ? 'pass'
      : candidate.checks.terminal_no_hang && candidate.checks.interrupt_within_5s
        ? 'partial'
        : 'fail';
  return candidate;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const runs = Number(args.runs || 20);
  const timeoutMs = Number(args['timeout-ms'] || 240000);
  const candidateFilter = typeof args.candidate === 'string' ? args.candidate : 'all';
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const evidenceDir = ensureDir(
    typeof args['evidence-dir'] === 'string' ? args['evidence-dir'] : join(EVIDENCE_ROOT, stamp),
  );
  const cwd = ensureDir(typeof args.cwd === 'string' ? args.cwd : join(EVIDENCE_ROOT, 'workspace'));
  const secrets = collectSecrets(
    'M1_11_CLAUDE_TOKEN',
    'ANTHROPIC_AUTH_TOKEN',
    'ANTHROPIC_API_KEY',
    'M1_11_CODEX_TOKEN',
    'OPENAI_API_KEY',
    'M1_11_DSH_API_KEY',
    'STREAMAX_API_KEY',
    'DEEPSEEK_API_KEY',
  );
  const log = (message) => console.log(`[m1-11] ${message}`);

  const report = {
    task: 'M1-11 A1 spike',
    generatedAt: new Date().toISOString(),
    runsRequested: runs,
    evidenceDir,
    artifacts: checkArtifacts(),
    candidates: {},
  };

  log(`证据目录：${evidenceDir}`);
  const selected = new Set(
    candidateFilter === 'all'
      ? ['claude', 'codex', 'dsh', 'dsh-acp']
      : candidateFilter === 'both'
        ? ['claude', 'codex']
        : [candidateFilter],
  );
  if (selected.has('claude')) {
    report.candidates.claude = await verifyClaude({ runs, timeoutMs, evidenceDir, secrets, log, cwd });
    await sleep(500);
  }
  if (selected.has('codex')) {
    report.candidates.codex = await verifyCodex({ runs, timeoutMs, evidenceDir, secrets, log, cwd });
  }
  if (selected.has('dsh')) {
    report.candidates.dsh = await verifyDsh({ runs, timeoutMs, evidenceDir, secrets, log, cwd });
  }
  if (selected.has('dsh-acp')) {
    report.candidates['dsh-acp'] = await verifyDshAcp({ runs, timeoutMs, evidenceDir, secrets, log, cwd });
  }

  const artifactOk =
    report.artifacts.samples.every((sample) => sample.exists) &&
    report.artifacts.ciJob.exists &&
    report.artifacts.ciJob.opt_in &&
    report.artifacts.ciJob.yaml_indentation_ok !== false &&
    report.artifacts.notes.exists &&
    Object.values(report.artifacts.notes.sections).every(Boolean);
  report.artifactsOk = artifactOk;

  const verdicts = Object.values(report.candidates).map((candidate) => candidate.verdict || candidate.status);
  report.summary = {
    candidates: verdicts,
    anyPass: verdicts.includes('pass'),
    artifactOk,
  };

  // Gate 1 结论（ADR-005）：选定适配器的会话恢复能力（Mode R/N）决定 M2-02 重放语义。
  const preferred = ['claude-code', 'codex', 'deepseek-harness-acp', 'deepseek-harness'];
  const passing = preferred
    .map((name) =>
      Object.values(report.candidates).find(
        (candidate) => candidate.candidate === name && candidate.verdict === 'pass',
      ),
    )
    .find(Boolean);
  const recoveryMode = passing?.session_recovery?.mode ?? null;
  report.gate1Recovery = passing
    ? {
        selected: passing.candidate,
        mode: recoveryMode,
        statement:
          `M2-02 重放语义按 Mode ${recoveryMode} 执行` +
          (recoveryMode === 'R'
            ? '（用 sessions.config.native_id 恢复原生会话续聊，上下文保留）'
            : '（新建原生会话并重放输入，UI 明示原生上下文可能丢失）'),
      }
    : {
        selected: null,
        mode: null,
        statement: '无通过候选；按 A1 降级路径 3 评估 M2-02M（Mock-only beta）',
      };
  report.summary.recoveryMode = recoveryMode;
  console.log(
    `[m1-11] Gate 1 恢复能力结论：selected=${report.gate1Recovery.selected ?? 'none'} ` +
      `mode=${recoveryMode ?? 'n/a'}`,
  );

  writeJson(join(evidenceDir, 'summary.json'), report);
  const baselineFile = join(EVIDENCE_ROOT, 'baseline.json');
  const previous = existsSync(baselineFile) ? JSON.parse(readFileSync(baselineFile, 'utf8')) : { task: 'M1-11 A1 spike', revisions: [] };
  previous.revisions.push({
    at: report.generatedAt,
    runs,
    candidates: Object.fromEntries(
      Object.entries(report.candidates).map(([name, value]) => [
        name,
        {
          verdict: value.verdict || value.status,
          first_token_ms: value.timing?.first_token_ms ?? null,
          output_tokens_per_s: value.timing?.output_tokens_per_s ?? null,
        },
      ]),
    ),
  });
  writeJson(baselineFile, previous);

  console.log(JSON.stringify(report, null, 2));
  process.exitCode = report.summary.anyPass && artifactOk ? 0 : 1;
}

main().catch((error) => {
  console.error(`[m1-11] 验证器失败：${redact(error?.stack || error)}`);
  process.exitCode = 1;
});
