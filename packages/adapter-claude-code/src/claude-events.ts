/**
 * Claude Code stream-json 事件 → Aether 附录 B 事件映射（M2-02 DoD2）。
 *
 * 权威输入（M1-11 接入笔记「流式事件模型」）：
 * - `system`（subtype `init`）：会话 id、工具名清单、模型、cwd；
 * - `stream_event` → `content_block_delta/text_delta`：逐 token 正文（拼接即流式文本）；
 *   `thinking_delta` 为推理，不入正文（不发射事件——附录 B 预留类型 P0 不启用）；
 * - `assistant`：完整 message（含 `tool_use` 块）→ `tool.call_started`；
 * - `user`：工具结果（`tool_result` 块）→ `tool.call_completed` / `tool.call_failed`；
 * - `result`：run 终态（`is_error`/`subtype`/`usage`/`api_error_status`）→
 *   `message.completed` + `run.completed` 或 `run.failed`。
 *
 * 边界（D9，设计 §2.1 第 4 条）：Claude print 模式无交互审批通道，适配器只**观察**
 * CLI 进程内的工具调用并按上述事件上报；工具的实际执行策略由 CLI 启动配置（信任级）
 * 决定，不经核心权限门（差异记录见 `docs/M2-02-证据.md` §边界与 `docs/spike/M1-11-接入笔记.md`）。
 */

/** 适配器侧稳定错误码（`run.failed` / `tool.call_failed` 使用）。 */
export const CLAUDE_ERROR_CODES = {
  /** CLI 进程无法启动（ENOENT 等）。 */
  SPAWN_FAILED: "spawn_failed",
  /** CLI 退出但未产出 `result` 终态。 */
  CLI_EXIT: "cli_exit",
  /** `result.is_error=true`（含中转/模型错误，`api_error_status` 进 message）。 */
  API_ERROR: "api_error",
  /** 适配器侧 run 超时上限（兜底；核心 120s 断流看门狗优先）。 */
  RUN_TIMEOUT: "run_timeout",
  /** 工具结果 `is_error=true`。 */
  TOOL_EXECUTION_FAILED: "tool_execution_failed",
  /** 工具调用被中断（与 Mock ③ 口径一致：message 含 abort）。 */
  TOOL_TIMEOUT: "timeout",
} as const;

export interface MappedEvent {
  type: string;
  payload: Record<string, unknown>;
}

export interface ClaudeRunContext {
  sessionId: string;
  runId: string;
  messageId: string;
}

/** `result` 事件（官方 CLI `-p --output-format stream-json`）。 */
export interface ClaudeResultEvent {
  is_error?: boolean;
  subtype?: string;
  result?: string;
  usage?: {
    input_tokens?: number;
    output_tokens?: number;
    total_tokens?: number;
  } | null;
  api_error_status?: number | null;
  duration_ms?: number | null;
  num_turns?: number | null;
  total_cost_usd?: number | null;
  session_id?: string;
}

/** `system/init` 事件（工具清单来源；`tools.list` 合并展示）。 */
export interface ClaudeInitEvent {
  session_id?: string;
  tools?: string[];
  model?: string;
  cwd?: string;
}

export type RunEndReason = "interrupted" | "timeout" | "spawn-error" | "child-unhealthy" | "process-exit";

export interface RunEndInfo {
  reason: RunEndReason;
  /** 进程退出码（reason=process-exit 时）。 */
  exitCode?: number | null;
  /** 进程信号（reason=process-exit 时）。 */
  signal?: string | null;
  /** 诊断信息（spawn 错误等）。 */
  detail?: string;
}

interface TrackedToolCall {
  name: string;
  startedAt: number;
  settled: boolean;
}

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

/** 工具结果文本 → 失败原因（`tool.call_failed.error.message`）。 */
export function toolResultText(block: Record<string, unknown>): string {
  const content = block.content;
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    const parts: string[] = [];
    for (const item of content) {
      if (isRecord(item) && typeof item.text === "string") parts.push(item.text);
    }
    if (parts.length > 0) return parts.join("\n");
  }
  return "工具执行失败（CLI 未提供详情）";
}

/** CLI usage → 附录 B `TokenUsage`（input/output/total，`total` 缺省为两项之和）。 */
export function mapUsage(
  usage: ClaudeResultEvent["usage"],
): Record<string, number> | null {
  if (!usage || typeof usage !== "object") return null;
  const input = asNumber(usage.input_tokens) ?? 0;
  const output = asNumber(usage.output_tokens) ?? 0;
  const total = asNumber(usage.total_tokens) ?? input + output;
  return { input_tokens: input, output_tokens: output, total_tokens: total };
}

/**
 * 单 run 事件映射器（纯函数式：输入事件流 → 输出 Aether 事件序列，便于单测）。
 *
 * 使用序：`push()` 逐条喂入 CLI stdout 行 →（中断/退出/超时）→ `finish()`。
 * `finish` 幂等（终态只产出一次）。
 */
export class ClaudeRunMapper {
  private readonly textParts: string[] = [];
  private readonly toolCalls = new Map<string, TrackedToolCall>();
  private readonly startedAt: number;
  private result: ClaudeResultEvent | null = null;
  private init: ClaudeInitEvent | null = null;
  private finished = false;

  constructor(
    private readonly context: ClaudeRunContext,
    private readonly now: () => number = Date.now,
  ) {
    this.startedAt = this.now();
  }

  /** 是否已产出终态。 */
  get isFinished(): boolean {
    return this.finished;
  }

  /** 最近一次 `system/init`（工具清单/会话 id/模型）。 */
  get initEvent(): ClaudeInitEvent | null {
    return this.init;
  }

  /** 已流式拼接的正文（`message.delta` 之和）。 */
  get streamedText(): string {
    return this.textParts.join("");
  }

  /** 最终正文：优先 `result.result`（官方聚合），回退流式拼接。 */
  get finalText(): string {
    const resultText = this.result?.result;
    if (typeof resultText === "string" && resultText.length > 0) return resultText;
    return this.streamedText;
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
    const type = asString(event.type);
    switch (type) {
      case "system":
        return this.onSystem(event);
      case "stream_event":
        return this.onStreamEvent(event);
      case "assistant":
        return this.onAssistant(event);
      case "user":
        return this.onUser(event);
      case "result":
        this.result = event as ClaudeResultEvent;
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
        ...this.failOpenToolCalls(
          CLAUDE_ERROR_CODES.TOOL_TIMEOUT,
          "工具调用被中断（abort）",
        ),
        { type: "run.cancelled", payload: { run_id: runId, reason: "interrupted" } },
      ];
    }
    if (end.reason === "timeout") {
      return [
        ...this.failOpenToolCalls(
          CLAUDE_ERROR_CODES.TOOL_TIMEOUT,
          `工具调用超时并被中断（abort；run 已运行 ${this.now() - this.startedAt}ms 超出适配器上限）`,
        ),
        {
          type: "run.failed",
          payload: {
            run_id: runId,
            error: {
              code: CLAUDE_ERROR_CODES.RUN_TIMEOUT,
              message: end.detail ?? "适配器侧 run 超时（CLI 进程已回收）",
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
              code: CLAUDE_ERROR_CODES.SPAWN_FAILED,
              message: end.detail ?? "Claude CLI 启动失败",
              recoverable: true,
            },
          },
        },
      ];
    }
    if (end.reason === "child-unhealthy") {
      return [
        ...this.failOpenToolCalls(
          CLAUDE_ERROR_CODES.CLI_EXIT,
          "Claude CLI stdout 不健康（连续 20 次无法解析）",
        ),
        {
          type: "run.failed",
          payload: {
            run_id: runId,
            error: {
              code: CLAUDE_ERROR_CODES.CLI_EXIT,
              message: end.detail ?? "Claude CLI stdout 连续无效帧达到阈值（D6 口径 20）",
              recoverable: true,
            },
          },
        },
      ];
    }

    const result = this.result;
    if (!result) {
      return [
        ...this.failOpenToolCalls(
          CLAUDE_ERROR_CODES.CLI_EXIT,
          "进程退出前工具调用未收口",
        ),
        {
          type: "run.failed",
          payload: {
            run_id: runId,
            error: {
              code: CLAUDE_ERROR_CODES.CLI_EXIT,
              message: `Claude CLI 退出但未产出 result 终态事件（exit=${end.exitCode ?? "null"} signal=${end.signal ?? "null"}）`,
              recoverable: true,
            },
          },
        },
      ];
    }
    if (result.is_error) {
      const status = asNumber(result.api_error_status);
      return [
        ...this.failOpenToolCalls(CLAUDE_ERROR_CODES.API_ERROR, "run 失败前工具调用未收口"),
        {
          type: "run.failed",
          payload: {
            run_id: runId,
            error: {
              code: CLAUDE_ERROR_CODES.API_ERROR,
              message: truncate(
                `Claude 返回错误（subtype=${result.subtype ?? "unknown"}${status !== undefined ? `, api_error_status=${status}` : ""}）：${result.result ?? "无详情"}`,
              ),
              recoverable: true,
            },
          },
        },
      ];
    }

    const usage = mapUsage(result.usage);
    const message = {
      id: this.context.messageId,
      session_id: this.context.sessionId,
      run_id: this.context.runId,
      role: "assistant",
      content: this.finalText,
      created_at: this.now(),
    };
    const completed: MappedEvent = {
      type: "message.completed",
      payload: usage ? { message, usage } : { message },
    };
    const runCompleted: MappedEvent = {
      type: "run.completed",
      payload: usage ? { run_id: runId, usage } : { run_id: runId },
    };
    return [completed, runCompleted];
  }

  private onSystem(event: Record<string, unknown>): MappedEvent[] {
    if (event.subtype === "init") {
      this.init = {
        session_id: asString(event.session_id),
        tools: Array.isArray(event.tools)
          ? event.tools.filter((tool): tool is string => typeof tool === "string")
          : undefined,
        model: asString(event.model),
        cwd: asString(event.cwd),
      };
    }
    return [];
  }

  private onStreamEvent(event: Record<string, unknown>): MappedEvent[] {
    const inner = event.event;
    if (!isRecord(inner)) return [];
    if (inner.type !== "content_block_delta") return [];
    const delta = inner.delta;
    if (!isRecord(delta)) return [];
    if (delta.type !== "text_delta") return [];
    const text = asString(delta.text);
    if (text === undefined || text.length === 0) return [];
    this.textParts.push(text);
    return [
      {
        type: "message.delta",
        payload: { message_id: this.context.messageId, text },
      },
    ];
  }

  private onAssistant(event: Record<string, unknown>): MappedEvent[] {
    const message = event.message;
    if (!isRecord(message)) return [];
    const content = message.content;
    if (!Array.isArray(content)) return [];
    const emissions: MappedEvent[] = [];
    for (const block of content) {
      if (!isRecord(block) || block.type !== "tool_use") continue;
      const toolCallId = asString(block.id);
      const name = asString(block.name);
      if (toolCallId === undefined || name === undefined) continue;
      if (this.toolCalls.has(toolCallId)) continue;
      this.toolCalls.set(toolCallId, {
        name,
        startedAt: this.now(),
        settled: false,
      });
      emissions.push({
        type: "tool.call_started",
        payload: {
          tool_call_id: toolCallId,
          tool_name: name,
          args: isRecord(block.input) ? block.input : (block.input ?? {}),
        },
      });
    }
    return emissions;
  }

  private onUser(event: Record<string, unknown>): MappedEvent[] {
    const message = event.message;
    if (!isRecord(message)) return [];
    const content = message.content;
    if (!Array.isArray(content)) return [];
    const emissions: MappedEvent[] = [];
    for (const block of content) {
      if (!isRecord(block) || block.type !== "tool_result") continue;
      const toolCallId = asString(block.tool_use_id);
      if (toolCallId === undefined) continue;
      const tracked = this.toolCalls.get(toolCallId);
      if (!tracked || tracked.settled) continue;
      tracked.settled = true;
      const durationMs = Math.max(0, this.now() - tracked.startedAt);
      if (block.is_error === true) {
        emissions.push({
          type: "tool.call_failed",
          payload: {
            tool_call_id: toolCallId,
            tool_name: tracked.name,
            duration_ms: durationMs,
            error: {
              code: CLAUDE_ERROR_CODES.TOOL_EXECUTION_FAILED,
              message: truncate(toolResultText(block)),
              recoverable: true,
            },
          },
        });
      } else {
        emissions.push({
          type: "tool.call_completed",
          payload: {
            tool_call_id: toolCallId,
            tool_name: tracked.name,
            duration_ms: durationMs,
          },
        });
      }
    }
    return emissions;
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
