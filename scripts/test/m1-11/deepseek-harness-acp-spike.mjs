/**
 * M1-11 A1 spike —— DeepSeek Harness（DSH）官方 ACP 接入样例
 * （@deepseek-ai/dsh@0.1.5-rc.2 的 acp profile；JSON-RPC 2.0 over stdio）。
 *
 * 参考 hermes-studio 的接入结论（只参考架构，不拷贝代码）：
 *  - DSH 的 headless profile 只有终稿；官方可编程面是 acp profile 的 Agent Client Protocol；
 *  - ACP 只携带「语义更新」（committed assistant/thought chunks、工具生命周期、usage），
 *    原始 provider delta 被协议有意排除（hermes 为拿到 token 级 delta 自注入私有插件，
 *    属于适配器实现，不在本最小 spike 范围）；
 *  - 会话由 server 持久化，支持 session/resume 跨进程恢复；session/cancel 为协议级取消。
 *
 * 实测协议面（0.1.5-rc.2）：
 *  - initialize {protocolVersion:1,...} → {protocolVersion:1, agentCapabilities}
 *  - session/new {cwd, mcpServers:[]} → {sessionId, configOptions}
 *  - session/prompt {sessionId, prompt:[{type:'text',text}]} → {stopReason}
 *    过程通知 session/update（agent_message_chunk / agent_thought_chunk / tool_call* / usage_update）
 *  - session/cancel（通知）/ session/close
 *
 * 用法：
 *   node deepseek-harness-acp-spike.mjs [--runs 1] [--mode fresh|continuation|interrupt]
 * 环境变量：
 *   M1_11_DSH_ACP_BIN        dsh 启动器 bin.js 路径（0.1.5-rc.2 安装；必填，否则回退全局 dsh）
 *   M1_11_DSH_ACP_HOME       隔离 DSH_HOME（必填）
 *   M1_11_DSH_ACP_PATCH      可选 overlay patch 路径（默认在 DSH_HOME 下生成 provider/model）
 *   M1_11_DSH_ACP_PROVIDER   默认 streamax；M1_11_DSH_ACP_MODEL 默认 deepseek-v4-pro
 *   M1_11_DSH_ACP_PROFILE    默认 acp
 */
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import {
  collectSecrets,
  envFirst,
  interruptTree,
  normalizeText,
  nowMs,
  parseArgs,
  readLines,
  redact,
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

export function dshAcpConfig() {
  return {
    bin: envFirst(['M1_11_DSH_ACP_BIN'], ''),
    home: envFirst(['M1_11_DSH_ACP_HOME'], ''),
    patch: envFirst(['M1_11_DSH_ACP_PATCH'], ''),
    provider: envFirst(['M1_11_DSH_ACP_PROVIDER'], 'streamax'),
    model: envFirst(['M1_11_DSH_ACP_MODEL'], 'deepseek-v4-pro'),
    profile: envFirst(['M1_11_DSH_ACP_PROFILE'], 'acp'),
  };
}

/** 在隔离 DSH_HOME 写入 provider/model overlay（不触碰用户级配置）。 */
export function ensureAcpPatch(config) {
  if (config.patch) return config.patch;
  if (!config.home) throw new Error('缺少 M1_11_DSH_ACP_HOME（隔离 DSH_HOME）');
  mkdirSync(config.home, { recursive: true });
  const patchPath = join(config.home, 'spike-acp.patch.yml');
  writeFileSync(
    patchPath,
    [
      '- id: acp',
      '  config:',
      `    provider: ${config.provider}`,
      `    model: ${config.model}`,
      '',
    ].join('\n'),
    'utf8',
  );
  return patchPath;
}

/** 极简 ACP 客户端：JSON-RPC over stdio，单连接驱动一到多个 session。 */
class AcpClient {
  constructor({ bin, home, profile, patch, cwd, extraArgs = [], secrets = [], onEvent = () => {} }) {
    if (!bin) throw new Error('缺少 M1_11_DSH_ACP_BIN（dsh 0.1.5-rc.2 的 lib/bin.js）');
    if (!home) throw new Error('缺少 M1_11_DSH_ACP_HOME（隔离 DSH_HOME）');
    this.home = home;
    this.cwd = cwd;
    this.onEvent = onEvent;
    this.secrets = secrets;
    this.buffer = '';
    this.pending = new Map();
    this.nextId = 1;
    this.closed = false;
    this.stderr = '';
    this.argv = [bin, '--profile', profile, '--patch', patch, ...extraArgs];
    this.spawnedAt = null;
    this.child = null;
  }

  start() {
    const cli = { path: process.execPath, args: this.argv, viaShell: false };
    this.child = spawnCli(cli, [], {
      cwd: this.cwd,
      env: { ...process.env, DSH_HOME: this.home },
      stdio: ['pipe', 'pipe', 'pipe'],
      windowsHide: true,
    });
    this.spawnedAt = nowMs();
    readLines(this.child.stdout, (line, at) => this.#receive(line, at));
    readLines(this.child.stderr, (line) => {
      this.stderr = `${this.stderr}${redact(line, this.secrets)}\n`.slice(-8000);
    });
    this.child.once('close', () => {
      this.closed = true;
      for (const { reject } of this.pending.values()) reject(new Error('ACP connection closed'));
      this.pending.clear();
    });
    return this.child;
  }

  #send(message) {
    if (this.closed || !this.child?.stdin?.writable) throw new Error('ACP connection is closed');
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', ...message })}\n`);
  }

  request(method, params, timeoutMs = 60000) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = timeoutMs > 0 ? setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${method} 超时（${timeoutMs}ms）`));
      }, timeoutMs) : null;
      this.pending.set(id, {
        resolve: (value) => {
          if (timer) clearTimeout(timer);
          resolve(value);
        },
        reject: (error) => {
          if (timer) clearTimeout(timer);
          reject(error);
        },
      });
      this.#send({ id, method, params });
    });
  }

  notify(method, params) {
    this.#send({ method, params });
  }

  async respond(id, result) {
    this.#send({ id, result });
  }

  #receive(line, at) {
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      this.onEvent({ kind: 'unparsed', line: redact(line, this.secrets).slice(0, 200), at });
      return;
    }
    if (message.id !== undefined && this.pending.has(message.id)) {
      const { resolve, reject } = this.pending.get(message.id);
      this.pending.delete(message.id);
      if (message.error) reject(new Error(redact(JSON.stringify(message.error), this.secrets)));
      else resolve(message.result);
      return;
    }
    if (message.method === 'session/update') {
      this.onEvent({ kind: 'update', at, update: message.params?.update, sessionId: message.params?.sessionId });
      return;
    }
    if (message.method === 'session/request_permission') {
      const options = message.params?.options || [];
      const allow = options.find((o) => /allow/.test(o.optionId || o.kind || '')) || options[0];
      this.onEvent({ kind: 'permission', at, params: message.params });
      if (message.id !== undefined && allow) {
        this.respond(message.id, { outcome: { outcome: 'selected', optionId: allow.optionId } });
      } else if (message.id !== undefined) {
        this.respond(message.id, { outcome: { outcome: 'cancelled' } });
      }
      return;
    }
    this.onEvent({ kind: 'other', at, message });
  }

  endInput() {
    try {
      this.child?.stdin?.end();
    } catch {
      /* ignore */
    }
  }
}

/**
 * 单次 ACP 会话：create/resume → prompt → 终态（可选协议级取消）。
 */
export async function runAcpOnce(options) {
  const {
    config,
    patch,
    prompt,
    sessionId = null,
    resume = false,
    cwd = process.cwd(),
    timeoutMs = 300000,
    cancelAfterFirstTextMs = null,
    cancelFallbackMs = 8000,
    secrets = [],
    baselineText = null,
    scratchDir = null,
  } = options;

  const record = {
    candidate: 'deepseek-harness-acp',
    sessionId,
    resumed: Boolean(resume),
    model: config.model,
    provider: config.provider,
    startedAt: new Date().toISOString(),
    terminal: 'none',
    stopReason: null,
    timedOut: false,
    streamText: '',
    reasoningText: '',
    updates: { agent_message_chunk: 0, agent_thought_chunk: 0, tool_call: 0, tool_call_update: 0, usage_update: 0, other: 0 },
    toolCalls: [],
    usage: null,
    permissionRequests: 0,
    firstUpdateMs: null,
    firstTextMs: null,
    lastTextMs: null,
    errors: [],
    stderr: '',
    exitCode: null,
    interrupt: null,
    baselineMatch: null,
    wallClockMs: null,
  };

  const workspace = scratchDir || cwd;
  const start = nowMs();
  let client;
  let closed = false;
  let cancelState = null;
  let cancelFallbackTimer = null;

  const handleUpdate = (event) => {
    if (record.firstUpdateMs === null) record.firstUpdateMs = event.at - start;
    const update = event.update || {};
    const kind = update.sessionUpdate;
    if (kind === 'agent_message_chunk') {
      const text = update.content?.text || '';
      if (!text) return;
      record.updates.agent_message_chunk += 1;
      record.streamText += text;
      if (record.firstTextMs === null) record.firstTextMs = event.at - start;
      record.lastTextMs = event.at - start;
      if (cancelAfterFirstTextMs !== null && record.updates.agent_message_chunk === 1) {
        scheduleCancel(cancelAfterFirstTextMs);
      }
    } else if (kind === 'agent_thought_chunk') {
      record.updates.agent_thought_chunk += 1;
      record.reasoningText += update.content?.text || '';
    } else if (kind === 'tool_call') {
      record.updates.tool_call += 1;
      record.toolCalls.push({ id: update.toolCallId, title: update.title, kind: update.kind, status: update.status });
    } else if (kind === 'tool_call_update') {
      record.updates.tool_call_update += 1;
      const existing = record.toolCalls.find((t) => t.id === update.toolCallId);
      if (existing) existing.status = update.status;
    } else if (kind === 'usage_update') {
      record.updates.usage_update += 1;
      record.usage = update;
    } else {
      record.updates.other += 1;
    }
  };

  client = new AcpClient({
    bin: config.bin,
    home: config.home,
    profile: config.profile,
    patch,
    cwd: workspace,
    secrets,
    onEvent: (event) => {
      if (event.kind === 'update') handleUpdate(event);
      else if (event.kind === 'permission') record.permissionRequests += 1;
      else if (event.kind === 'unparsed') record.errors.push(event.line);
    },
  });
  client.start();
  client.child.once('close', () => {
    closed = true;
    if (cancelFallbackTimer) clearTimeout(cancelFallbackTimer);
  });

  const scheduleCancel = (delayMs) => {
    if (cancelState) return;
    cancelState = { requested: false, requestedAtMs: null, settledAtMs: null };
    setTimeout(() => {
      if (closed) return;
      cancelState.requested = true;
      cancelState.requestedAtMs = nowMs() - start;
      client.notify('session/cancel', { sessionId: record.sessionId });
    }, Math.max(0, delayMs));
  };

  try {
    const init = await client.request('initialize', {
      protocolVersion: 1,
      clientCapabilities: {},
      clientInfo: { name: 'aether-m1-11-spike', version: '0.1.0' },
    }, 60000);
    record.protocolVersion = init?.protocolVersion ?? null;
    record.agentInfo = init?.agentInfo ?? null;

    const session = resume
      ? await client.request('session/resume', { sessionId, cwd: workspace, mcpServers: [] }, 60000)
      : await client.request('session/new', { cwd: workspace, mcpServers: [] }, 60000);
    record.sessionId = session?.sessionId || sessionId;
    record.configOptions = session?.configOptions?.map((o) => ({ id: o.id, currentValue: o.currentValue })) ?? null;

    if (cancelAfterFirstTextMs !== null) {
      cancelFallbackTimer = setTimeout(() => scheduleCancel(0), cancelFallbackMs);
    }

    const promptResult = await client.request(
      'session/prompt',
      { sessionId: record.sessionId, prompt: [{ type: 'text', text: prompt }] },
      timeoutMs,
    );
    record.stopReason = promptResult?.stopReason ?? null;
    record.terminal = record.stopReason ? 'completed' : 'failed';
    if (cancelState) cancelState.settledAtMs = nowMs() - start;

    await client.request('session/close', { sessionId: record.sessionId }, 15000).catch((error) => {
      record.errors.push(`session/close: ${redact(error.message, secrets)}`);
    });
    client.endInput();
    const closeOutcome = await waitClose(client.child, 10000);
    record.exitCode = closeOutcome.code ?? null;
  } catch (error) {
    const message = redact(error.message, secrets);
    record.errors.push(message);
    record.timedOut = /超时/.test(message);
    record.terminal = record.timedOut ? 'none' : 'failed';
    if (!closed) {
      record.interrupt = await interruptTree(client.child);
    }
  } finally {
    if (cancelFallbackTimer) clearTimeout(cancelFallbackTimer);
    if (cancelState?.requested) {
      const settledAtMs = cancelState.settledAtMs ?? nowMs() - start;
      record.interrupt = {
        kind: 'session/cancel',
        requestedAtMs: cancelState.requestedAtMs,
        settledAtMs,
        elapsedMs: cancelState.requestedAtMs === null ? null : settledAtMs - cancelState.requestedAtMs,
        stopReason: record.stopReason,
      };
    }
    if (!closed && client.child) {
      const finalClose = await waitClose(client.child, 10000);
      record.exitCode = record.exitCode ?? finalClose.code ?? null;
      if (finalClose.kind === 'timeout') {
        await interruptTree(client.child);
      }
    }
  }

  record.wallClockMs = nowMs() - start;
  record.stderr = (client?.stderr || '').slice(-8000);
  record.baselineMatch =
    baselineText === null || record.terminal !== 'completed'
      ? null
      : normalizeText(record.streamText) === normalizeText(baselineText);
  record.streaming = record.updates.agent_message_chunk > 1;
  record.limitations = {
    token_level_delta: 'ACP 有意不携带原始 provider delta（官方只发 committed 语义更新）',
  };
  return record;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const mode = typeof args.mode === 'string' ? args.mode : 'fresh';
  const runs = Number(args.runs || 1);
  const timeoutMs = Number(args['timeout-ms'] || 300000);
  const prompt = typeof args.prompt === 'string' ? args.prompt : DEFAULT_PROMPT;
  const cwd = typeof args.cwd === 'string' ? args.cwd : process.cwd();
  const outFile = typeof args.out === 'string' ? args.out : null;

  const config = dshAcpConfig();
  const patch = ensureAcpPatch(config);
  const secrets = collectSecrets('M1_11_DSH_API_KEY', 'STREAMAX_API_KEY', 'DEEPSEEK_API_KEY');
  const results = [];

  if (mode === 'continuation') {
    const first = await runAcpOnce({
      config,
      patch,
      prompt: 'Remember this passphrase: AETHER-SPIKE-41. Reply with exactly: STORED',
      cwd,
      timeoutMs,
      secrets,
    });
    const second = await runAcpOnce({
      config,
      patch,
      prompt: 'What passphrase did I ask you to remember? Reply with just the passphrase.',
      sessionId: first.sessionId,
      resume: true,
      cwd,
      timeoutMs,
      secrets,
    });
    results.push(first, second);
  } else if (mode === 'interrupt') {
    results.push(
      await runAcpOnce({
        config,
        patch,
        prompt: typeof args.prompt === 'string' ? args.prompt : INTERRUPT_PROMPT,
        cwd,
        timeoutMs,
        cancelAfterFirstTextMs: Number(args['interrupt-after-ms'] || 500),
        secrets,
      }),
    );
  } else {
    for (let index = 0; index < runs; index += 1) {
      const record = await runAcpOnce({ config, patch, prompt, cwd, timeoutMs, secrets });
      record.runIndex = index + 1;
      results.push(record);
    }
  }

  const summary = { candidate: 'deepseek-harness-acp', mode, config: { ...config, patch }, results };
  if (outFile) writeJson(outFile, summary);
  console.log(JSON.stringify(summary, null, 2));
}

const isDirectRun =
  process.argv[1] && process.argv[1].replace(/\\/g, '/').endsWith('/deepseek-harness-acp-spike.mjs');
if (isDirectRun) {
  main().catch((error) => {
    console.error(
      `[deepseek-harness-acp-spike] ${redact(error?.stack || error, collectSecrets('M1_11_DSH_API_KEY', 'STREAMAX_API_KEY'))}`,
    );
    process.exitCode = 1;
  });
}
