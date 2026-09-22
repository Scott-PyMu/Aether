/**
 * Codex 适配器（ADR-008 / M2-11 Codex 路径）：Aether 线协议（D6）方法实现。
 *
 * - `initialize`：能力上报；
 * - `session.create`：返回稳定别名作为 `native_id`（ADR-008 §3.3）；传入已知 `native_id`
 *   即 Mode R 恢复（`exec resume <thread_id>`）；
 * - `session.send`：ack 快路径 + `message.delta`（Codex 正文整段到达，按条目增量上移）；
 * - `session.interrupt`：进程树回收（D5），适配器合成在途工具收口与 `run.cancelled`；
 * - `session.dispose`：中断在途 run 并注销会话；
 * - `tools.list`：工具条目类型目录（静态 ∪ 观察子集）；
 * - `permission.resolve`：D9 边界声明（exec 模式无交互审批通道）；
 * - `health.ping` / `shutdown`。
 */

import {
  Adapter,
  ERROR_CODES,
  RpcError,
  isUlid,
  ulid,
  type AdapterRuntimeInfo,
  type EnvelopeContext,
} from "@aether/adapter-sdk";
import { mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import {
  CodexCliRun,
  type CodexCliConfig,
  type KillTreeFn,
  type SpawnFn,
} from "./codex-cli";
import {
  CodexRunMapper,
  CODEX_TOOL_TYPES,
  type MappedEvent,
  type RunEndInfo,
} from "./codex-events";
import { CodexSessionStore } from "./codex-sessions";

/** 运行时 id（与 DDL 注释/官方白名单一致）。 */
export const RUNTIME_NAME = "codex";
/** 适配器版本（semver，hello 上报）。 */
export const ADAPTER_VERSION = "0.1.0";

/** 适配器能力清单（hello/initialize 上报）。 */
export const RUNTIME_CAPABILITIES: readonly string[] = [
  "session.create",
  "session.send",
  "session.interrupt",
  "session.dispose",
  "tools.list",
  "permission.resolve",
];

/** CLI stdout 无法解析的行：连续达到该阈值 → 该 run 判不健康（口径同 D6 连续无效帧 20 次）。 */
export const CLI_INVALID_LINE_THRESHOLD = 20;

/** 默认 run 硬超时（常量级；核心断流看门狗 120s 在其之前生效）。 */
export const DEFAULT_RUN_TIMEOUT_MS = 30 * 60 * 1000;

/** Codex 工具条目目录（`tools.list` 静态部分）。 */
export const CODEX_TOOL_CATALOG: ReadonlyArray<{ name: string; description: string }> = [
  { name: "command_execution", description: "在工作区执行 shell 命令" },
  { name: "file_change", description: "应用/写入工作区文件变更" },
  { name: "mcp_tool_call", description: "调用 MCP 工具" },
  { name: "web_search", description: "联网检索" },
];

/** `tools.list` 响应中的工具定义（D6：工具发现）。 */
export interface ToolDefinition {
  name: string;
  description: string;
  input_schema: Record<string, unknown>;
}

/** 依据观察到的工具名构造定义（未观察 → 全量目录）。 */
export function toolDefinitions(observed?: ReadonlySet<string>): ToolDefinition[] {
  const catalog = new Map(CODEX_TOOL_CATALOG.map((tool) => [tool.name, tool]));
  const names =
    observed && observed.size > 0 ? [...observed] : CODEX_TOOL_CATALOG.map((tool) => tool.name);
  return names.map((name) => ({
    name,
    description: catalog.get(name)?.description ?? "Codex 工具",
    input_schema: { type: "object" },
  }));
}

interface CodexRunState {
  runId: string;
  sessionId: string;
  messageId: string;
  mapper: CodexRunMapper;
  cli: CodexCliRun | null;
  emitChain: Promise<void>;
  interrupted: boolean;
  finished: boolean;
  invalidStdoutStreak: number;
  forcedFailure: string | null;
}

interface CodexSession {
  id: string;
  alias: string;
  threadId: string | null;
  resumed: boolean;
  model?: string;
  clientMsgIds: Map<string, string>;
  runs: Map<string, CodexRunState>;
  observedTools: Set<string>;
  disposed: boolean;
}

export interface CodexAdapterOptions {
  lines: AsyncIterable<string>;
  writeLine: (line: string) => Promise<void>;
  stderr?: (line: string) => void;
  exit?: (code: number) => void;
  cli: CodexCliConfig;
  /** 状态目录（别名映射；默认 `<CODEX_HOME|~/.codex>/aether-bridge`）。 */
  stateDir?: string;
  runTimeoutMs?: number;
  /** 测试注入。 */
  spawnFn?: SpawnFn;
  killTreeFn?: KillTreeFn;
  runtimeVersion?: string;
}

export class CodexAdapter {
  readonly adapter: Adapter;
  readonly runtime: Required<AdapterRuntimeInfo>;
  readonly runTimeoutMs: number;

  private readonly options: CodexAdapterOptions;
  private readonly sessions = new Map<string, CodexSession>();
  private readonly store: CodexSessionStore;
  private storeWarning: string | null = null;
  private readonly startedAt = Date.now();

  constructor(options: CodexAdapterOptions) {
    this.options = options;
    this.runTimeoutMs = options.runTimeoutMs ?? DEFAULT_RUN_TIMEOUT_MS;
    this.runtime = {
      name: RUNTIME_NAME,
      version: options.runtimeVersion ?? ADAPTER_VERSION,
      capabilities: [...RUNTIME_CAPABILITIES],
    };
    this.store = new CodexSessionStore(
      options.stateDir !== undefined
        ? join(options.stateDir, "sessions.json")
        : join(defaultStateDir(options.cli.home), "sessions.json"),
    );
    this.storeWarning = this.store.load();
    if (this.storeWarning !== null) {
      options.stderr?.(`[codex] 别名映射加载失败（按空映射继续）：${this.storeWarning}`);
    }
    this.adapter = new Adapter({
      runtime: this.runtime,
      lines: options.lines,
      writeLine: options.writeLine,
      stderr: options.stderr,
    });
    this.registerHandlers();
  }

  /** 进入读循环（hello 由 SDK 发送）。 */
  run(): Promise<void> {
    return this.adapter.run();
  }

  get sessionCount(): number {
    return this.sessions.size;
  }

  private registerHandlers(): void {
    this.adapter
      .handle("initialize", () => ({
        capabilities: this.runtime.capabilities,
        protocol: "1.0",
        acknowledged: true,
      }))
      .handle("session.create", (params) => this.onCreateSession(params))
      .handle("session.send", (params) => this.onSend(params))
      .handle("session.interrupt", (params) => this.onInterrupt(params))
      .handle("session.dispose", (params) => this.onDispose(params))
      .handle("tools.list", (params) => {
        const input = params as { session_id?: string } | null;
        const session = input?.session_id ? this.sessions.get(input.session_id) : undefined;
        if (input?.session_id && !session) {
          throw new RpcError(ERROR_CODES.SESSION_NOT_FOUND, `会话不存在: ${input.session_id}`);
        }
        return { tools: toolDefinitions(session?.observedTools) };
      })
      .handle("permission.resolve", () => ({
        // D9 边界（设计 §2.1 第 4 条）：Codex exec 模式无交互审批通道，CLI 进程内
        // 行为（含工具执行）不经核心权限门；适配器只观察并上报工具事件。
        resolved: false,
        reason: "codex-exec-mode（D9 边界：CLI 进程内行为不经权限门）",
      }))
      .handle("health.ping", () => ({
        status: "ok",
        uptime_ms: Date.now() - this.startedAt,
        pid: process.pid,
        alias_bindings: this.store.size,
      }))
      .handle("shutdown", () => {
        for (const session of this.sessions.values()) {
          for (const run of session.runs.values()) {
            run.interrupted = true;
            void run.cli?.interrupt();
          }
        }
        this.sessions.clear();
        this.adapter.stop();
        this.options.exit?.(0);
        return { ok: true };
      });
  }

  private onCreateSession(params: unknown): Record<string, unknown> {
    const input = (params ?? {}) as {
      title?: unknown;
      workspace?: unknown;
      model?: unknown;
      native_id?: unknown;
    };
    const requested = input.native_id;
    if (requested !== undefined && !isAliasId(requested)) {
      throw new RpcError(
        ERROR_CODES.INVALID_PARAMS,
        "native_id 必须是 26 位 ULID 别名（Codex thread_id 由服务端生成，见 ADR-008 §3.3）",
      );
    }
    const model = typeof input.model === "string" && input.model.length > 0 ? input.model : undefined;
    const alias = isAliasId(requested) ? requested : ulid();
    const threadId = isAliasId(requested) ? this.store.lookup(requested) : null;
    const session: CodexSession = {
      id: alias,
      alias,
      threadId,
      resumed: isAliasId(requested) && threadId !== null,
      ...(model !== undefined ? { model } : {}),
      clientMsgIds: new Map(),
      runs: new Map(),
      observedTools: new Set(),
      disposed: false,
    };
    this.sessions.set(alias, session);
    return {
      session_id: session.id,
      native_id: session.alias,
      resumed: session.resumed,
      model: model ?? this.options.cli.model ?? null,
      created_at: Date.now(),
    };
  }

  private onSend(params: unknown): Record<string, unknown> {
    const input = (params ?? {}) as {
      session_id?: unknown;
      client_msg_id?: unknown;
      text?: unknown;
      run_id?: unknown;
    };
    const sessionId = typeof input.session_id === "string" ? input.session_id : "";
    const session = this.sessions.get(sessionId);
    if (!session || session.disposed) {
      throw new RpcError(ERROR_CODES.SESSION_NOT_FOUND, `会话不存在: ${sessionId || "<missing>"}`);
    }
    if (typeof input.text !== "string") {
      throw new RpcError(ERROR_CODES.INVALID_PARAMS, "session.send 缺少 text");
    }
    const clientMsgId =
      typeof input.client_msg_id === "string" && input.client_msg_id.length > 0
        ? input.client_msg_id
        : ulid();
    const existing = session.clientMsgIds.get(clientMsgId);
    if (existing !== undefined) {
      return { accepted: true, run_id: existing, duplicate: true };
    }
    const runId =
      typeof input.run_id === "string" && isUlid(input.run_id) ? input.run_id : ulid();
    const messageId = ulid();
    const run: CodexRunState = {
      runId,
      sessionId: session.id,
      messageId,
      mapper: new CodexRunMapper({ sessionId: session.id, runId, messageId }),
      cli: null,
      emitChain: Promise.resolve(),
      interrupted: false,
      finished: false,
      invalidStdoutStreak: 0,
      forcedFailure: null,
    };
    run.cli = new CodexCliRun({
      config: { ...this.options.cli, ...(session.model ? { model: session.model } : {}) },
      spec: {
        threadId: session.threadId,
        prompt: input.text,
        timeoutMs: this.runTimeoutMs,
      },
      env: this.cliEnv(),
      onEvent: (event) => this.onCliEvent(session, run, event),
      onMalformedLine: (line) => this.onCliMalformedLine(run, line),
      onStderrLine: (line) => this.options.stderr?.(`[codex-cli] ${line}`),
      ...(this.options.spawnFn ? { spawnFn: this.options.spawnFn } : {}),
      ...(this.options.killTreeFn ? { killTreeFn: this.options.killTreeFn } : {}),
    });
    session.clientMsgIds.set(clientMsgId, runId);
    session.runs.set(runId, run);

    void this.enqueue(run, async () => {
      await this.emit(run, "run.started", { run_id: runId });
    });
    run.cli.start();
    void this.awaitRun(session, run);
    return { accepted: true, run_id: runId };
  }

  private onInterrupt(params: unknown): Record<string, unknown> {
    const input = (params ?? {}) as { session_id?: unknown };
    const sessionId = typeof input.session_id === "string" ? input.session_id : "";
    const session = this.sessions.get(sessionId);
    if (!session || session.disposed) {
      throw new RpcError(ERROR_CODES.SESSION_NOT_FOUND, `会话不存在: ${sessionId || "<missing>"}`);
    }
    const active = [...session.runs.values()].filter((run) => !run.finished);
    if (active.length === 0) return { interrupted: false };
    for (const run of active) {
      run.interrupted = true;
      void run.cli?.interrupt();
    }
    return { interrupted: true, run_id: active[0]?.runId ?? null };
  }

  private onDispose(params: unknown): Record<string, unknown> {
    const input = (params ?? {}) as { session_id?: unknown };
    const sessionId = typeof input.session_id === "string" ? input.session_id : "";
    const session = this.sessions.get(sessionId);
    if (!session) {
      throw new RpcError(ERROR_CODES.SESSION_NOT_FOUND, `会话不存在: ${sessionId || "<missing>"}`);
    }
    session.disposed = true;
    for (const run of session.runs.values()) {
      run.interrupted = true;
      void run.cli?.interrupt();
    }
    this.sessions.delete(sessionId);
    return { disposed: true };
  }

  private cliEnv(): NodeJS.ProcessEnv {
    const env: NodeJS.ProcessEnv = { ...process.env };
    if (this.options.cli.home) {
      env.CODEX_HOME = this.options.cli.home;
    }
    return env;
  }

  private onCliEvent(session: CodexSession, run: CodexRunState, event: unknown): void {
    void this.enqueue(run, async () => {
      const mapped = run.mapper.push(event);
      if (run.mapper.threadId !== null && session.threadId === null) {
        session.threadId = run.mapper.threadId;
        if (!session.resumed) {
          this.store.bind(session.alias, run.mapper.threadId);
        }
      }
      run.invalidStdoutStreak = 0;
      await this.emitMapped(run, mapped);
    });
  }

  private onCliMalformedLine(run: CodexRunState, line: string): void {
    this.options.stderr?.(
      `[codex-cli] 无法解析的 stdout 行（连续 ${run.invalidStdoutStreak + 1}/${CLI_INVALID_LINE_THRESHOLD}）：${line.slice(0, 200)}`,
    );
    void this.enqueue(run, async () => {
      run.invalidStdoutStreak += 1;
      if (run.invalidStdoutStreak >= CLI_INVALID_LINE_THRESHOLD && !run.finished) {
        run.forcedFailure = `连续 ${CLI_INVALID_LINE_THRESHOLD} 次无法解析的 Codex stdout 行 → 判不健康`;
        await run.cli?.interrupt();
      }
    });
  }

  private async awaitRun(session: CodexSession, run: CodexRunState): Promise<void> {
    const cli = run.cli;
    if (!cli) return;
    await cli.waitExit(this.runTimeoutMs + 10_000);
    const end = this.endInfo(run);
    await this.enqueue(run, async () => {
      if (run.finished) return;
      const mapped = run.mapper.finish(end);
      await this.emitMapped(run, mapped);
      run.finished = true;
      session.runs.delete(run.runId);
    });
  }

  private endInfo(run: CodexRunState): RunEndInfo {
    const cli = run.cli;
    if (!cli) {
      return { reason: "spawn-error", detail: "Codex CLI 未启动" };
    }
    const spawnFailure = cli.spawnFailure;
    if (spawnFailure !== null) {
      return { reason: "spawn-error", detail: spawnFailure };
    }
    if (run.forcedFailure !== null) {
      return { reason: "child-unhealthy", detail: run.forcedFailure };
    }
    if (cli.wasTimedOut) {
      return {
        reason: "timeout",
        detail: `run 超出适配器上限 ${this.runTimeoutMs}ms（CLI 进程已整树回收）`,
      };
    }
    if (run.interrupted) {
      return { reason: "interrupted" };
    }
    const exit = cli.exitInfo;
    return {
      reason: "process-exit",
      exitCode: exit?.code ?? null,
      signal: exit?.signal ?? null,
    };
  }

  private enqueue(run: CodexRunState, task: () => Promise<void>): Promise<void> {
    const next = run.emitChain.then(task, task);
    run.emitChain = next.then(
      () => undefined,
      () => undefined,
    );
    return next;
  }

  private emitMapped(run: CodexRunState, mapped: MappedEvent[]): Promise<void> {
    let chain = Promise.resolve();
    for (const item of mapped) {
      chain = chain.then(() => this.emit(run, item.type, item.payload));
    }
    return chain;
  }

  private emit(
    run: CodexRunState,
    type: string,
    payload: Record<string, unknown>,
  ): Promise<void> {
    const context: EnvelopeContext = {
      runtimeId: this.runtime.name,
      sessionId: run.sessionId,
      runId: run.runId,
    };
    return this.adapter.emitEvent(context, type, payload);
  }
}

/** 别名形状校验（ULID；`native_id` 的唯一合法形式）。 */
export function isAliasId(value: unknown): value is string {
  return typeof value === "string" && isUlid(value);
}

/** 默认状态目录：`<CODEX_HOME|~/.codex>/aether-bridge`（无环境变量时回退用户目录）。 */
export function defaultStateDir(codexHome?: string): string {
  const base = codexHome ?? process.env.CODEX_HOME ?? join(homedir(), ".codex");
  const dir = join(base, "aether-bridge");
  mkdirSync(dir, { recursive: true });
  return dir;
}

export { CODEX_TOOL_TYPES };
