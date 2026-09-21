/**
 * Claude Code 适配器（M2-02）：Aether 线协议（D6）方法实现。
 *
 * - `initialize`：能力上报；
 * - `session.create`：生成/接受原生会话 id（`native_id` = Claude `--session-id` UUID；
 *   传入 `native_id` 即 **Mode R 恢复**，见 ADR-005）；
 * - `session.send`：ack 快路径（不等模型）+ 逐 token `message.delta` 流式；
 * - `session.interrupt`：进程树回收（D5），适配器合成 `run.cancelled`；
 * - `session.dispose`：中断在途 run 并注销会话；
 * - `tools.list`：工具定义（静态目录 ∪ 最近一次 CLI `system/init` 观察到的工具名）；
 * - `permission.resolve`：D9 边界声明（Claude print 模式无交互审批通道，见模块头注释）；
 * - `health.ping` / `shutdown`。
 *
 * 事件序列（附录 B）：
 * `run.started` →（`message.delta`… / `tool.call_started` → `tool.call_completed|failed`）→
 * `message.completed` + `run.completed`（或 `run.failed` / `run.cancelled`）。
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
import { randomUUID } from "node:crypto";

import {
  ClaudeCliRun,
  type ClaudeCliConfig,
  type KillTreeFn,
  type SpawnFn,
} from "./claude-cli";
import {
  ClaudeRunMapper,
  type ClaudeInitEvent,
  type MappedEvent,
  type RunEndInfo,
} from "./claude-events";

/** 运行时 id（与 DDL 注释/官方白名单一致）。 */
export const RUNTIME_NAME = "claude-code";
/** 适配器版本（semver，hello 上报）。 */
export const ADAPTER_VERSION = "0.1.0";

/** 适配器能力清单（hello/initialize 上报；RA-04）。 */
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

/** 内置工具目录（`tools.list` 的静态部分；schema 为通用对象，描述为能力级说明）。 */
export const CLAUDE_TOOL_CATALOG: ReadonlyArray<{
  name: string;
  description: string;
}> = [
  { name: "Task", description: "启动子代理执行多步任务" },
  { name: "Bash", description: "在工作区执行 shell 命令" },
  { name: "Glob", description: "按 glob 模式查找文件" },
  { name: "Grep", description: "正则检索文件内容" },
  { name: "Read", description: "读取工作区文件" },
  { name: "Edit", description: "对工作区文件做精确替换编辑" },
  { name: "Write", description: "写入工作区文件" },
  { name: "NotebookEdit", description: "编辑 Jupyter Notebook 单元" },
  { name: "WebFetch", description: "抓取网页并转文本" },
  { name: "WebSearch", description: "联网检索" },
  { name: "TodoWrite", description: "维护任务清单" },
  { name: "BashOutput", description: "读取后台 shell 输出" },
  { name: "KillShell", description: "终止后台 shell" },
];

/** `tools.list` 响应中的工具定义（D6：工具发现）。 */
export interface ToolDefinition {
  name: string;
  description: string;
  input_schema: Record<string, unknown>;
}

/** 依据观察到的工具名构造定义（未观察 → 全量目录）。 */
export function toolDefinitions(observed?: readonly string[]): ToolDefinition[] {
  const catalog = new Map(CLAUDE_TOOL_CATALOG.map((tool) => [tool.name, tool]));
  const names =
    observed && observed.length > 0 ? [...observed] : CLAUDE_TOOL_CATALOG.map((tool) => tool.name);
  return names.map((name) => ({
    name,
    description: catalog.get(name)?.description ?? "Claude Code 工具",
    input_schema: { type: "object" },
  }));
}

/** UUID v4 形状校验（Claude `--session-id` 要求）。 */
export function isUuid(value: unknown): value is string {
  return (
    typeof value === "string" &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value)
  );
}

interface ClaudeRunState {
  runId: string;
  sessionId: string;
  messageId: string;
  mapper: ClaudeRunMapper;
  cli: ClaudeCliRun | null;
  emitChain: Promise<void>;
  interrupted: boolean;
  finished: boolean;
  invalidStdoutStreak: number;
  forcedFailure: string | null;
}

interface ClaudeSession {
  id: string;
  nativeId: string;
  /** 由 `native_id` 恢复（Mode R）创建。 */
  resumed: boolean;
  /** 原生会话已存在（init 已观察，或由 native_id 恢复）。 */
  nativeCreated: boolean;
  model?: string;
  lastInit: ClaudeInitEvent | null;
  clientMsgIds: Map<string, string>;
  runs: Map<string, ClaudeRunState>;
  disposed: boolean;
}

export interface ClaudeAdapterOptions {
  lines: AsyncIterable<string>;
  writeLine: (line: string) => Promise<void>;
  stderr?: (line: string) => void;
  exit?: (code: number) => void;
  /** CLI 基础配置（bin/extraArgs/settingsFile/workspace/model/tools/permissionMode/maxTurns）。 */
  cli: ClaudeCliConfig;
  /** 适配器侧 run 硬超时（默认 30min；核心 120s 断流看门狗优先）。 */
  runTimeoutMs?: number;
  /** 测试注入。 */
  spawnFn?: SpawnFn;
  killTreeFn?: KillTreeFn;
  runtimeVersion?: string;
}

/** 默认 run 硬超时（常量级；核心断流看门狗 120s 在其之前生效）。 */
export const DEFAULT_RUN_TIMEOUT_MS = 30 * 60 * 1000;

export class ClaudeCodeAdapter {
  readonly adapter: Adapter;
  readonly runtime: Required<AdapterRuntimeInfo>;
  readonly runTimeoutMs: number;

  private readonly options: ClaudeAdapterOptions;
  private readonly sessions = new Map<string, ClaudeSession>();
  private readonly startedAt = Date.now();

  constructor(options: ClaudeAdapterOptions) {
    this.options = options;
    this.runTimeoutMs = options.runTimeoutMs ?? DEFAULT_RUN_TIMEOUT_MS;
    this.runtime = {
      name: RUNTIME_NAME,
      version: options.runtimeVersion ?? ADAPTER_VERSION,
      capabilities: [...RUNTIME_CAPABILITIES],
    };
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

  /** 会话数（诊断/测试）。 */
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
          throw new RpcError(
            ERROR_CODES.SESSION_NOT_FOUND,
            `会话不存在: ${input.session_id}`,
          );
        }
        return { tools: toolDefinitions(session?.lastInit?.tools) };
      })
      .handle("permission.resolve", () => ({
        // D9 边界（设计 §2.1 第 4 条）：Claude print 模式无交互审批通道，CLI 进程内
        // 行为（含工具执行）不经核心权限门；适配器只观察并上报工具事件。
        // 本方法保留给未来可拦截路径，当前不产生决议。
        resolved: false,
        reason: "claude-print-mode（D9 边界：CLI 进程内行为不经权限门）",
      }))
      .handle("health.ping", () => ({
        status: "ok",
        uptime_ms: Date.now() - this.startedAt,
        pid: process.pid,
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
    if (requested !== undefined && !isUuid(requested)) {
      throw new RpcError(
        ERROR_CODES.INVALID_PARAMS,
        "native_id 必须是 UUID（Claude --session-id/--resume 要求）",
      );
    }
    const nativeId = isUuid(requested) ? requested : randomUUID();
    const model = typeof input.model === "string" && input.model.length > 0 ? input.model : undefined;
    const session: ClaudeSession = {
      id: nativeId,
      nativeId,
      resumed: isUuid(requested),
      nativeCreated: isUuid(requested),
      ...(model !== undefined ? { model } : {}),
      lastInit: null,
      clientMsgIds: new Map(),
      runs: new Map(),
      disposed: false,
    };
    this.sessions.set(nativeId, session);
    return {
      session_id: session.id,
      native_id: session.nativeId,
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
    const run: ClaudeRunState = {
      runId,
      sessionId: session.id,
      messageId,
      mapper: new ClaudeRunMapper({ sessionId: session.id, runId, messageId }),
      cli: null,
      emitChain: Promise.resolve(),
      interrupted: false,
      finished: false,
      invalidStdoutStreak: 0,
      forcedFailure: null,
    };
    run.cli = new ClaudeCliRun({
      config: { ...this.options.cli, ...(session.model ? { model: session.model } : {}) },
      spec: {
        nativeId: session.nativeId,
        resume: session.resumed || session.nativeCreated,
        prompt: input.text,
        timeoutMs: this.runTimeoutMs,
      },
      onEvent: (event) => this.onCliEvent(session, run, event),
      onMalformedLine: (line) => this.onCliMalformedLine(run, line),
      onStderrLine: (line) => this.options.stderr?.(`[claude-cli] ${line}`),
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

  private onCliEvent(session: ClaudeSession, run: ClaudeRunState, event: unknown): void {
    void this.enqueue(run, async () => {
      const mapped = run.mapper.push(event);
      if (run.mapper.initEvent) {
        session.lastInit = run.mapper.initEvent;
        if (!session.nativeCreated) session.nativeCreated = true;
      }
      run.invalidStdoutStreak = 0;
      await this.emitMapped(run, mapped);
    });
  }

  private onCliMalformedLine(run: ClaudeRunState, line: string): void {
    this.options.stderr?.(
      `[claude-cli] 无法解析的 stdout 行（连续 ${run.invalidStdoutStreak + 1}/${CLI_INVALID_LINE_THRESHOLD}）：${line.slice(0, 200)}`,
    );
    void this.enqueue(run, async () => {
      run.invalidStdoutStreak += 1;
      if (run.invalidStdoutStreak >= CLI_INVALID_LINE_THRESHOLD && !run.finished) {
        run.forcedFailure = `连续 ${CLI_INVALID_LINE_THRESHOLD} 次无法解析的 Claude stdout 行 → 判不健康`;
        await run.cli?.interrupt();
      }
    });
  }

  private async awaitRun(session: ClaudeSession, run: ClaudeRunState): Promise<void> {
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

  private endInfo(run: ClaudeRunState): RunEndInfo {
    const cli = run.cli;
    if (!cli) {
      return { reason: "spawn-error", detail: "Claude CLI 未启动" };
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

  private enqueue(run: ClaudeRunState, task: () => Promise<void>): Promise<void> {
    const next = run.emitChain.then(task, task);
    run.emitChain = next.then(
      () => undefined,
      () => undefined,
    );
    return next;
  }

  private emitMapped(run: ClaudeRunState, mapped: MappedEvent[]): Promise<void> {
    let chain = Promise.resolve();
    for (const item of mapped) {
      chain = chain.then(() => this.emit(run, item.type, item.payload));
    }
    return chain;
  }

  private emit(
    run: ClaudeRunState,
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
