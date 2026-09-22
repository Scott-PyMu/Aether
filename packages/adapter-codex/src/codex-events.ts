/**
 * Codex `exec --json` 事件 → Aether 附录 B 事件映射（ADR-008 / M2-11 Codex 路径）。
 *
 * 权威输入（M1-11 接入笔记「流式事件模型 / Codex」与 `codex exec --json` 实测）：
 * - `thread.started`：`thread_id`（服务端生成；适配器持久化为别名映射，Mode R 依据）；
 * - `turn.started` / `turn.completed(usage)` / `turn.failed(error)`：run 生命周期与终态；
 * - `item.started|updated|completed`：`agent_message`（正文整段；本环境无 token delta）、
 *   `command_execution` / `file_change` / `mcp_tool_call` 等工具条目；
 * - `error`：瞬态诊断（已知坑 5：不得见 error 即判失败，终态仲裁以 turn.completed/failed 为准）。
 */

/** 适配器侧稳定错误码（`run.failed` / `tool.call_failed` 使用）。 */
export const CODEX_ERROR_CODES = {
  /** CLI 进程无法启动（ENOENT 等）。 */
  SPAWN_FAILED: "spawn_failed",
  /** CLI 退出但未产出终态事件。 */
  CLI_EXIT: "cli_exit",
  /** `turn.failed`（模型/中转/执行错误）。 */
  TURN_FAILED: "turn_failed",
  /** 适配器侧 run 超时上限（兜底；核心 120s 断流看门狗优先）。 */
  RUN_TIMEOUT: "run_timeout",
  /** 工具条目以非零退出码/失败状态结束。 */
  TOOL_EXECUTION_FAILED: "tool_execution_failed",
  /** 工具调用被中断（与 Mock ③ 口径一致：message 含 abort）。 */
  TOOL_TIMEOUT: "timeout",
} as const;

export interface MappedEvent {
  type: string;
  payload: Record<string, unknown>;
}

export interface CodexRunContext {
  sessionId: string;
  runId: string;
  messageId: string;
}

export type RunEndReason =
  | "interrupted"
  | "timeout"
  | "spawn-error"
  | "child-unhealthy"
  | "process-exit";

export interface RunEndInfo {
  reason: RunEndReason;
  exitCode?: number | null;
  signal?: string | null;
  detail?: string;
}

/** Codex 工具条目类型 → 工具名（`tools.list` 目录与事件 `tool_name` 共用）。 */
export const CODEX_TOOL_TYPES: readonly string[] = [
  "command_execution",
  "file_change",
  "mcp_tool_call",
  "web_search",
];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function asNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function truncate(text: string, limit = 500): string {
  return text.length <= limit ? text : `${text.slice(0, limit)}…`;
}

/** `turn.completed.usage` → 附录 B `TokenUsage`（字段名兼容 input/output 变体）。 */
export function mapUsage(usage: unknown): Record<string, number> | null {
  if (!isRecord(usage)) return null;
  const input = asNumber(usage.input_tokens) ?? asNumber(usage.inputTokens) ?? 0;
  const output = asNumber(usage.output_tokens) ?? asNumber(usage.outputTokens) ?? 0;
  const total = asNumber(usage.total_tokens) ?? asNumber(usage.totalTokens) ?? input + output;
  return { input_tokens: input, output_tokens: output, total_tokens: total };
}

interface TrackedToolCall {
  name: string;
  startedAt: number;
  settled: boolean;
}

interface TrackedAgentMessage {
  /** 已上报的文本长度（item.updated 增量去重用）。 */
  emitted: number;
}

/**
 * 单 run 事件映射器（纯函数式：输入事件流 → 输出 Aether 事件序列，便于单测）。
 */
export class CodexRunMapper {
  private readonly textParts: string[] = [];
  private readonly toolCalls = new Map<string, TrackedToolCall>();
  private readonly agentMessages = new Map<string, TrackedAgentMessage>();
  private nativeThreadId: string | null = null;
  private terminal: "completed" | "failed" | null = null;
  private usage: Record<string, number> | null = null;
  private failureDetail: string | null = null;
  private finished = false;

  constructor(
    private readonly context: CodexRunContext,
    private readonly now: () => number = Date.now,
  ) {}

  get isFinished(): boolean {
    return this.finished;
  }

  /** `thread.started.thread_id`（Mode R 别名映射的来源）。 */
  get threadId(): string | null {
    return this.nativeThreadId;
  }

  /** 已流式拼接的正文（`message.delta` 之和）。 */
  get streamedText(): string {
    return this.textParts.join("");
  }

  /** 未收口的工具调用数（诊断用）。 */
  get openToolCalls(): number {
    let count = 0;
    for (const call of this.toolCalls.values()) {
      if (!call.settled) count += 1;
    }
    return count;
  }

  /** 喂入一条已解析的 CLI 事件（未知类型忽略，前向兼容）。 */
  push(event: unknown): MappedEvent[] {
    if (this.finished || !isRecord(event)) return [];
    switch (asString(event.type)) {
      case "thread.started":
        this.nativeThreadId = asString(event.thread_id) ?? this.nativeThreadId;
        return [];
      case "turn.started":
        return [];
      case "item.started":
      case "item.updated":
      case "item.completed":
        return this.onItem(asString(event.type) ?? "", event.item);
      case "turn.completed":
        this.terminal = "completed";
        this.usage = mapUsage(event.usage);
        return [];
      case "turn.failed":
        this.terminal = "failed";
        this.failureDetail = truncate(
          JSON.stringify(event.error ?? event.message ?? event),
        );
        return [];
      default:
        return [];
    }
  }

  /** 进程结束（或中断/超时/启动失败）→ 终态事件序列（幂等）。 */
  finish(end: RunEndInfo): MappedEvent[] {
    if (this.finished) return [];
    this.finished = true;
    const { runId } = this.context;

    if (end.reason === "interrupted") {
      return [
        ...this.failOpenToolCalls(CODEX_ERROR_CODES.TOOL_TIMEOUT, "工具调用被中断（abort）"),
        { type: "run.cancelled", payload: { run_id: runId, reason: "interrupted" } },
      ];
    }
    if (end.reason === "timeout") {
      return [
        ...this.failOpenToolCalls(
          CODEX_ERROR_CODES.TOOL_TIMEOUT,
          "工具调用超时并被中断（abort）",
        ),
        {
          type: "run.failed",
          payload: {
            run_id: runId,
            error: {
              code: CODEX_ERROR_CODES.RUN_TIMEOUT,
              message: end.detail ?? "适配器侧 run 超时（Codex CLI 已整树回收）",
              recoverable: true,
            },
          },
        },
      ];
    }
    if (end.reason === "spawn-error") {
      return [
        {
          type: "run.failed",
          payload: {
            run_id: runId,
            error: {
              code: CODEX_ERROR_CODES.SPAWN_FAILED,
              message: end.detail ?? "Codex CLI 启动失败",
              recoverable: true,
            },
          },
        },
      ];
    }
    if (end.reason === "child-unhealthy") {
      return [
        ...this.failOpenToolCalls(
          CODEX_ERROR_CODES.CLI_EXIT,
          "Codex CLI stdout 不健康（连续 20 次无法解析）",
        ),
        {
          type: "run.failed",
          payload: {
            run_id: runId,
            error: {
              code: CODEX_ERROR_CODES.CLI_EXIT,
              message: end.detail ?? "Codex CLI stdout 连续无效帧达到阈值（D6 口径 20）",
              recoverable: true,
            },
          },
        },
      ];
    }

    if (this.terminal === "failed") {
      return [
        ...this.failOpenToolCalls(CODEX_ERROR_CODES.TURN_FAILED, "run 失败前工具调用未收口"),
        {
          type: "run.failed",
          payload: {
            run_id: runId,
            error: {
              code: CODEX_ERROR_CODES.TURN_FAILED,
              message: truncate(this.failureDetail ?? "Codex turn.failed（无详情）"),
              recoverable: true,
            },
          },
        },
      ];
    }
    if (this.terminal !== "completed") {
      return [
        ...this.failOpenToolCalls(CODEX_ERROR_CODES.CLI_EXIT, "进程退出前工具调用未收口"),
        {
          type: "run.failed",
          payload: {
            run_id: runId,
            error: {
              code: CODEX_ERROR_CODES.CLI_EXIT,
              message: `Codex CLI 退出但未产出 turn.completed/turn.failed（exit=${end.exitCode ?? "null"} signal=${end.signal ?? "null"}）`,
              recoverable: true,
            },
          },
        },
      ];
    }

    const finalText = this.streamedText;
    const message = {
      id: this.context.messageId,
      session_id: this.context.sessionId,
      run_id: this.context.runId,
      role: "assistant",
      content: finalText,
      created_at: this.now(),
    };
    const completed: MappedEvent = {
      type: "message.completed",
      payload: this.usage ? { message, usage: this.usage } : { message },
    };
    const runCompleted: MappedEvent = {
      type: "run.completed",
      payload: this.usage ? { run_id: runId, usage: this.usage } : { run_id: runId },
    };
    return [completed, runCompleted];
  }

  private onItem(eventType: string, rawItem: unknown): MappedEvent[] {
    if (!isRecord(rawItem)) return [];
    const itemType = asString(rawItem.type) ?? "";
    if (itemType === "agent_message") {
      return this.onAgentMessage(eventType, rawItem);
    }
    if (!CODEX_TOOL_TYPES.includes(itemType)) {
      return [];
    }
    const itemId = asString(rawItem.id);
    if (itemId === undefined) return [];
    if (eventType === "item.started") {
      if (this.toolCalls.has(itemId)) return [];
      this.toolCalls.set(itemId, {
        name: itemType,
        startedAt: this.now(),
        settled: false,
      });
      return [
        {
          type: "tool.call_started",
          payload: {
            tool_call_id: itemId,
            tool_name: itemType,
            args: this.toolArgs(rawItem),
          },
        },
      ];
    }
    if (eventType !== "item.completed") return [];
    const tracked = this.toolCalls.get(itemId);
    if (!tracked || tracked.settled) return [];
    tracked.settled = true;
    const durationMs = Math.max(0, this.now() - tracked.startedAt);
    const failure = this.toolFailure(rawItem);
    if (failure !== null) {
      return [
        {
          type: "tool.call_failed",
          payload: {
            tool_call_id: itemId,
            tool_name: tracked.name,
            duration_ms: durationMs,
            error: {
              code: CODEX_ERROR_CODES.TOOL_EXECUTION_FAILED,
              message: truncate(failure),
              recoverable: true,
            },
          },
        },
      ];
    }
    return [
      {
        type: "tool.call_completed",
        payload: {
          tool_call_id: itemId,
          tool_name: tracked.name,
          duration_ms: durationMs,
        },
      },
    ];
  }

  private onAgentMessage(eventType: string, item: Record<string, unknown>): MappedEvent[] {
    const itemId = asString(item.id) ?? "agent_message";
    const text = asString(item.text) ?? "";
    const tracked = this.agentMessages.get(itemId) ?? { emitted: 0 };
    this.agentMessages.set(itemId, tracked);
    if (text.length <= tracked.emitted) return [];
    const suffix = text.slice(tracked.emitted);
    tracked.emitted = text.length;
    if (eventType === "item.started") return [];
    this.textParts.push(suffix);
    return [
      {
        type: "message.delta",
        payload: { message_id: this.context.messageId, text: suffix },
      },
    ];
  }

  private toolArgs(item: Record<string, unknown>): Record<string, unknown> {
    const args: Record<string, unknown> = {};
    for (const key of ["command", "changes", "server", "tool", "query", "path"]) {
      const value = item[key];
      if (value !== undefined) args[key] = value;
    }
    return args;
  }

  private toolFailure(item: Record<string, unknown>): string | null {
    const exitCode = asNumber(item.exit_code);
    if (exitCode !== undefined && exitCode !== 0) {
      return `退出码 ${exitCode}（${truncate(asString(item.aggregated_output) ?? "无输出")}）`;
    }
    const status = asString(item.status);
    if (status !== undefined && ["failed", "error", "declined", "cancelled"].includes(status)) {
      return `状态 ${status}`;
    }
    if (isRecord(item.error)) {
      return asString(item.error.message) ?? JSON.stringify(item.error);
    }
    return null;
  }

  private failOpenToolCalls(code: string, message: string): MappedEvent[] {
    const emissions: MappedEvent[] = [];
    for (const [toolCallId, tracked] of this.toolCalls) {
      if (tracked.settled) continue;
      tracked.settled = true;
      emissions.push({
        type: "tool.call_failed",
        payload: {
          tool_call_id: toolCallId,
          tool_name: tracked.name,
          duration_ms: Math.max(0, this.now() - tracked.startedAt),
          error: { code, message, recoverable: true },
        },
      });
    }
    return emissions;
  }
}
