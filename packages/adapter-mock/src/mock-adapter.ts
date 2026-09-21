/**
 * Mock 适配器（M1-09）：D6 线协议骨架的参考实现与故障注入宿主。
 *
 * - 正常路径：initialize / session.create / session.send（流式）/ session.interrupt /
 *   session.dispose / tools.list / permission.resolve / health.ping / shutdown；
 * - 5 类工具调用注入清单（权威定义见 `scenarios.ts`，供 M2-02/M2-10 复用）；
 *   ④⑤ 为自包含**预置**（不经核心权限网关、不发 permission.request 通知；边界 B1，
 *   见 `docs/M1-09-证据.md`，M2-10 在真实回环中重放）；
 * - 附件外置（M2-09/D6）：`artifact:<文件名>[:<字节数>]` 触发——把大附件**数据体**
 *   写入 `AETHER_ARTIFACTS_DIR`（或 `--artifacts-dir`）目录，仅以 `artifact_ref`
 *   引用帧（路径 + 元数据）上报，数据体不进入线协议、不落库；
 * - 故障注入：half-line / bad-json / stdout-log / oversized-line（1–2MiB 非引用行）/
 *   line-over-2mib（>2MiB 断连）/ artifact-line（<1MiB 引用帧）/
 *   artifact-line-over-limit（1–2MiB 声称引用 → 契约违约）/
 *   artifact-ref-outside（引用帧路径逃逸 `..` → 核心校验拒绝）/
 *   line-over-2mib-burst / oversized-line-burst（M2-09 内存曲线：连续 N 次）/
 *   crash / capability-missing / hang（不响应模式）/ no-hello（见 `InjectionKind`）；
 * - 吞吐基准：`bench:<n>` 触发 n 条 `message.delta` 连发（DoD5 ≥1000 delta/s）。
 */

import { promises as fs } from "node:fs";
import path from "node:path";

import {
  Adapter,
  ERROR_CODES,
  PROTOCOL_VERSION,
  RpcError,
  ulid,
  type AdapterRuntimeInfo,
  type EnvelopeContext,
} from "@aether/adapter-sdk";

import {
  buildPermissionRequested,
  buildPermissionResolved,
  buildToolCallCompleted,
  buildToolCallFailed,
  buildToolCallStarted,
  PERMISSION_DENIED_ERROR,
  scenarioForText,
  TOOL_CALL_SCENARIOS,
  TOOL_FAILURE_ERROR,
  TOOL_TIMEOUT_ERROR,
  type ToolCallScenarioId,
} from "./scenarios";

export type InjectionKind =
  | "bad-json"
  | "stdout-log"
  | "half-line"
  | "oversized-line"
  | "line-over-2mib"
  | "oversized-line-burst"
  | "line-over-2mib-burst"
  | "artifact-line"
  | "artifact-line-over-limit"
  | "artifact-ref-outside"
  | "artifact-ref-malformed"
  | "crash"
  | "capability-missing"
  | "hang"
  | "no-hello";

export const INJECTION_KINDS: readonly InjectionKind[] = [
  "bad-json",
  "stdout-log",
  "half-line",
  "oversized-line",
  "line-over-2mib",
  "oversized-line-burst",
  "line-over-2mib-burst",
  "artifact-line",
  "artifact-line-over-limit",
  "artifact-ref-outside",
  "artifact-ref-malformed",
  "crash",
  "capability-missing",
  "hang",
  "no-hello",
];

export interface MockAdapterOptions {
  lines: AsyncIterable<string>;
  writeLine: (line: string) => Promise<void>;
  /** 绕过行编码的原始写入（半行/超大行注入）。 */
  rawWrite?: (chunk: string) => void;
  stderr?: (line: string) => void;
  /** 进程退出钩子（no-hello 之外的所有致命注入走此路径）。 */
  exit?: (code: number) => void;
  runtimeName?: string;
  runtimeVersion?: string;
  protocol?: string;
  sendHello?: boolean;
  injections?: InjectionKind[];
  injectCount?: number;
  streamDeltas?: number;
  streamIntervalMs?: number;
  longStreamIntervalMs?: number;
  /** 附件目录（M2-09/D6：`artifact:` 触发把附件数据体写这里；缺省禁止附件触发）。 */
  artifactsDir?: string;
}

const DEFAULT_DELTAS = 24;
const DEFAULT_INTERVAL_MS = 5;
const DEFAULT_LONG_INTERVAL_MS = 50;
// D6：1–2MiB 非引用行正常解析（正向样例）。
const OVERSIZED_PAD_BYTES = 1536 * 1024;
// D6：>2MiB 任意行必须断连（负向样例）。
const LINE_OVER_2MIB_PAD_BYTES = 2560 * 1024;
// D6：artifact_ref 引用帧必须 <1MiB（正向样例）。
const ARTIFACT_PAD_BYTES = 512 * 1024;
// D6：1–2MiB 声称 artifact_ref → 契约违约（负向样例）。
const ARTIFACT_OVER_LIMIT_PAD_BYTES = 1200 * 1024;
// M2-09：附件默认字节数（>2MiB——证明数据体不可能经 2MiB 帧上限的线协议传输）。
const DEFAULT_ARTIFACT_BYTES = 3 * 1024 * 1024;
// M2-09：附件内容标记（「不落库/不进线协议」断言锚点）。
export const ARTIFACT_MARKER = "AETHER_MOCK_ARTIFACT_MARKER";
// M2-09：`artifact:` 触发前缀（session.send 文本）。
export const ARTIFACT_TRIGGER_PREFIX = "artifact:";
const DELTA_FRAGMENT = "0123456789abcdef";

interface ActiveRun {
  runId: string;
  cancelled: boolean;
  interruptWaiters: Array<() => void>;
  interrupt(): void;
}

function createActiveRun(runId: string): ActiveRun {
  const run: ActiveRun = {
    runId,
    cancelled: false,
    interruptWaiters: [],
    interrupt() {
      if (run.cancelled) return;
      run.cancelled = true;
      for (const waiter of run.interruptWaiters.splice(0)) waiter();
    },
  };
  return run;
}

interface MockSession {
  id: string;
  disposed: boolean;
  clientMsgIds: Map<string, string>;
  activeRun?: ActiveRun;
}

function emptyUsage(): Record<string, number> {
  return { input_tokens: 0, output_tokens: 0, total_tokens: 0 };
}

export class MockAdapter {
  readonly adapter: Adapter;
  readonly runtime: Required<AdapterRuntimeInfo>;

  private readonly options: Required<
    Pick<
      MockAdapterOptions,
      | "lines"
      | "writeLine"
      | "rawWrite"
      | "stderr"
      | "exit"
      | "injections"
      | "injectCount"
      | "streamDeltas"
      | "streamIntervalMs"
      | "longStreamIntervalMs"
    >
  > & { protocol?: string; sendHello: boolean; artifactsDir?: string };

  private readonly sessions = new Map<string, MockSession>();
  private readonly startedAt = Date.now();
  private sessionCounter = 0;

  private helloWritten = false;
  private injectionStarted = false;
  private disposedAll = false;

  constructor(options: MockAdapterOptions) {
    const protocol = options.protocol;
    const injections = options.injections ?? [];
    this.options = {
      lines: options.lines,
      writeLine: options.writeLine,
      rawWrite:
        options.rawWrite ??
        ((chunk: string) => {
          process.stdout.write(chunk);
        }),
      stderr: options.stderr ?? (() => {}),
      exit:
        options.exit ??
        ((code: number) => {
          setTimeout(() => process.exit(code), 20);
        }),
      injections,
      injectCount: options.injectCount ?? 20,
      streamDeltas: options.streamDeltas ?? DEFAULT_DELTAS,
      streamIntervalMs: options.streamIntervalMs ?? DEFAULT_INTERVAL_MS,
      longStreamIntervalMs: options.longStreamIntervalMs ?? DEFAULT_LONG_INTERVAL_MS,
      artifactsDir: options.artifactsDir,
      protocol,
      sendHello:
        (options.sendHello ?? true) &&
        !injections.includes("no-hello") &&
        (protocol === undefined || protocol === PROTOCOL_VERSION),
    };
    this.runtime = {
      name: options.runtimeName ?? "mock",
      version: options.runtimeVersion ?? "0.1.0",
      capabilities: [
        "session.create",
        "session.send",
        "session.interrupt",
        "session.dispose",
        "tools.list",
        "permission.resolve",
      ],
    };
    this.adapter = new Adapter({
      runtime: this.runtime,
      lines: this.options.lines,
      writeLine: this.onFrameWritten.bind(this),
      stderr: this.options.stderr,
      sendHello: this.options.sendHello,
    });
    this.registerHandlers();
  }

  /** 启动：必要时先发协议版本覆盖的 hello，再进入读循环。 */
  async run(): Promise<void> {
    if (!this.options.sendHello) {
      const protocol = this.options.protocol;
      if (protocol !== undefined) {
        await this.adapter.peer.notify("hello", {
          protocol,
          runtime: { ...this.runtime },
        });
      }
    }
    if (this.options.injections.includes("hang")) {
      // DoD7 不响应模式：完成 hello 后停止响应 health/请求，但不退出（T5b/M2-08/M4-01 复用）。
      if (this.options.sendHello) {
        await this.adapter.peer.notify("hello", {
          protocol: this.options.protocol ?? PROTOCOL_VERSION,
          runtime: { ...this.runtime },
        });
      }
      await new Promise(() => {});
      return;
    }
    await this.adapter.run();
  }

  private async onFrameWritten(line: string): Promise<void> {
    await this.options.writeLine(line);
    if (this.helloWritten || this.injectionStarted) return;
    try {
      const frame = JSON.parse(line) as { method?: string };
      if (frame.method !== "hello") return;
    } catch {
      return;
    }
    this.helloWritten = true;
    this.injectionStarted = true;
    void this.runInjections();
  }

  private async runInjections(): Promise<void> {
    const { injections, injectCount } = this.options;
    for (const injection of injections) {
      switch (injection) {
        case "bad-json":
          for (let index = 0; index < injectCount; index += 1) {
            this.options.rawWrite("{ 这不是 JSON\n");
          }
          break;
        case "stdout-log":
          for (let index = 0; index < injectCount; index += 1) {
            this.options.rawWrite(`[mock] 日志误入 stdout #${index}\n`);
          }
          break;
        case "half-line":
          this.options.rawWrite('{"jsonrpc":"2.0","method":"log","params":');
          this.options.exit(0);
          break;
        case "oversized-line":
          this.options.rawWrite(
            `${JSON.stringify({
              jsonrpc: "2.0",
              method: "log",
              params: { pad: "x".repeat(OVERSIZED_PAD_BYTES) },
            })}\n`,
          );
          break;
        case "line-over-2mib":
          this.options.rawWrite(
            `${JSON.stringify({
              jsonrpc: "2.0",
              method: "log",
              params: { pad: "x".repeat(LINE_OVER_2MIB_PAD_BYTES) },
            })}\n`,
          );
          break;
        case "oversized-line-burst":
          // M2-09 内存曲线：连续 `injectCount` 次 1–2MiB 非引用行（解析后逐行释放）。
          for (let index = 0; index < this.options.injectCount; index += 1) {
            this.options.rawWrite(
              `${JSON.stringify({
                jsonrpc: "2.0",
                method: "log",
                params: { pad: "x".repeat(OVERSIZED_PAD_BYTES) },
              })}\n`,
            );
          }
          break;
        case "line-over-2mib-burst":
          // M2-09 内存曲线：连续 `injectCount` 次 >2MiB 行（首行即触发核心断连，
          // 其余行在断连后写 EPIPE 属预期——断言对象是核心侧内存上界）。
          for (let index = 0; index < this.options.injectCount; index += 1) {
            this.options.rawWrite(
              `${JSON.stringify({
                jsonrpc: "2.0",
                method: "log",
                params: { pad: "x".repeat(LINE_OVER_2MIB_PAD_BYTES) },
              })}\n`,
            );
          }
          break;
        case "artifact-line":
          this.options.rawWrite(
            `${JSON.stringify({
              jsonrpc: "2.0",
              method: "artifact_ref",
              type: "artifact_ref",
              params: { refs: [], pad: "a".repeat(ARTIFACT_PAD_BYTES) },
            })}\n`,
          );
          break;
        case "artifact-line-over-limit":
          this.options.rawWrite(
            `${JSON.stringify({
              jsonrpc: "2.0",
              method: "artifact_ref",
              type: "artifact_ref",
              params: { refs: [], pad: "a".repeat(ARTIFACT_OVER_LIMIT_PAD_BYTES) },
            })}\n`,
          );
          break;
        case "artifact-ref-outside":
          // M2-09：引用帧路径逃逸（`..` 段）——核心 ArtifactValidator 必须拒绝。
          this.options.rawWrite(
            `${JSON.stringify({
              jsonrpc: "2.0",
              method: "artifact_ref",
              type: "artifact_ref",
              params: { refs: [{ path: "../outside.bin", size: 1, kind: "application/octet-stream" }] },
            })}\n`,
          );
          break;
        case "artifact-ref-malformed":
          // M2-09：引用帧形状非法（refs 非数组）——线协议层按「无效帧」计数（D6）。
          this.options.rawWrite(
            `${JSON.stringify({
              jsonrpc: "2.0",
              method: "artifact_ref",
              type: "artifact_ref",
              params: { refs: "not-an-array" },
            })}\n`,
          );
          break;
        case "crash":
          this.options.exit(41);
          break;
        case "capability-missing":
        case "hang":
        case "no-hello":
          break;
      }
    }
  }

  private registerHandlers(): void {
    this.adapter
      .handle("initialize", () => ({
        capabilities: this.runtime.capabilities,
        protocol: PROTOCOL_VERSION,
        acknowledged: true,
      }))
      .handle("session.create", () => {
        this.sessionCounter += 1;
        const session: MockSession = {
          id: `mock-sess-${this.sessionCounter}`,
          disposed: false,
          clientMsgIds: new Map(),
        };
        this.sessions.set(session.id, session);
        return { session_id: session.id, created_at: Date.now() };
      })
      .handle("session.send", (params) => {
        const input = params as {
          session_id?: string;
          client_msg_id?: string;
          text?: string;
        };
        const session = this.session(input.session_id);
        if (!session) {
          throw new RpcError(
            ERROR_CODES.SESSION_NOT_FOUND,
            `会话不存在: ${input.session_id ?? "<missing>"}`,
          );
        }
        const clientMsgId = input.client_msg_id ?? ulid();
        const existing = session.clientMsgIds.get(clientMsgId);
        if (existing) {
          return { accepted: true, run_id: existing, duplicate: true };
        }
        const runId = ulid();
        session.clientMsgIds.set(clientMsgId, runId);
        void this.streamRun(session, runId, input.text ?? "");
        return { accepted: true, run_id: runId };
      })
      .handle("session.interrupt", (params) => {
        const input = params as { session_id?: string };
        const session = this.session(input.session_id);
        if (!session) {
          throw new RpcError(
            ERROR_CODES.SESSION_NOT_FOUND,
            `会话不存在: ${input.session_id ?? "<missing>"}`,
          );
        }
        const run = session.activeRun;
        if (!run) return { interrupted: false };
        run.interrupt();
        return { interrupted: true, run_id: run.runId };
      })
      .handle("session.dispose", (params) => {
        const input = params as { session_id?: string };
        const session = this.session(input.session_id);
        if (!session) {
          throw new RpcError(
            ERROR_CODES.SESSION_NOT_FOUND,
            `会话不存在: ${input.session_id ?? "<missing>"}`,
          );
        }
        session.disposed = true;
        session.activeRun?.interrupt();
        this.sessions.delete(session.id);
        return { disposed: true };
      })
      .handle("tools.list", () => {
        if (this.options.injections.includes("capability-missing")) {
          throw new RpcError(ERROR_CODES.CAPABILITY_MISSING, "能力缺失: tools.list（注入）");
        }
        return {
          tools: [
            { name: "mock.echo", description: "回显", input_schema: { type: "object" } },
            { name: "mock.read_file", description: "读文件", input_schema: { type: "object" } },
            { name: "mock.write_file", description: "写文件", input_schema: { type: "object" } },
          ],
        };
      })
      .handle("permission.resolve", () => {
        // 边界 B1（M1-09 证据）：M1 预置路径不产生待决权限请求——Mock 自包含产出
        // ④⑤ 事件序列，由场景固定决策，不发送 permission.request 通知，也不依赖本方法。
        // M2-10 真实回环（mock-only 路径）在本方法内登记/结算 pending 状态。
        return { resolved: false, reason: "m1-preset（不走核心权限网关，见 B1）" };
      })
      .handle("health.ping", () => {
        if (this.options.injections.includes("hang")) {
          return new Promise(() => {});
        }
        return { status: "ok", uptime_ms: Date.now() - this.startedAt, pid: process.pid };
      })
      .handle("shutdown", () => {
        this.disposeAll();
        this.adapter.stop();
        this.options.exit(0);
        return { ok: true };
      });
  }

  private session(sessionId: string | undefined): MockSession | undefined {
    if (!sessionId) return undefined;
    return this.sessions.get(sessionId);
  }

  private disposeAll(): void {
    this.disposedAll = true;
    for (const session of this.sessions.values()) {
      session.disposed = true;
      session.activeRun?.interrupt();
    }
    this.sessions.clear();
  }

  private async emit(
    context: EnvelopeContext,
    type: string,
    payload: Record<string, unknown>,
  ): Promise<void> {
    await this.adapter.emitEvent(context, type, payload);
  }

  private delay(ms: number): Promise<void> {
    return new Promise((resolve) => {
      setTimeout(resolve, ms);
    });
  }

  private waitForInterrupt(run: ActiveRun): Promise<void> {
    if (run.cancelled) return Promise.resolve();
    return new Promise((resolve) => {
      run.interruptWaiters.push(resolve);
    });
  }

  private async streamRun(session: MockSession, runId: string, text: string): Promise<void> {
    const context: EnvelopeContext = {
      runtimeId: this.runtime.name,
      sessionId: session.id,
      runId,
    };
    const run: ActiveRun = createActiveRun(runId);
    session.activeRun = run;
    try {
      await this.emit(context, "run.started", { run_id: runId });
      const scenario = scenarioForText(text);
      if (scenario) {
        await this.runToolScenario(scenario, context, run);
      } else if (text.trim().startsWith("bench:")) {
        const count = Number.parseInt(text.trim().slice("bench:".length), 10);
        await this.runStream(context, run, Number.isFinite(count) ? count : 0, 0);
      } else if (text.trim().startsWith(ARTIFACT_TRIGGER_PREFIX)) {
        await this.runArtifactScenario(
          context,
          run,
          text.trim().slice(ARTIFACT_TRIGGER_PREFIX.length),
        );
      } else if (text.trim() === "long") {
        await this.runStream(context, run, Number.MAX_SAFE_INTEGER, this.options.longStreamIntervalMs);
      } else {
        await this.runStream(
          context,
          run,
          this.options.streamDeltas,
          this.options.streamIntervalMs,
        );
      }
    } catch (error) {
      const detail = error instanceof Error ? error.message : String(error);
      await this.emit(context, "run.failed", {
        run_id: runId,
        error: { code: "mock_internal", message: detail, recoverable: true },
      });
    } finally {
      if (session.activeRun === run) {
        session.activeRun = undefined;
      }
    }
  }

  private async runStream(
    context: EnvelopeContext,
    run: ActiveRun,
    deltas: number,
    intervalMs: number,
  ): Promise<void> {
    const messageId = ulid();
    let content = "";
    let emitted = 0;
    while (emitted < deltas && !run.cancelled && !this.disposedAll) {
      content += DELTA_FRAGMENT;
      emitted += 1;
      await this.emit(context, "message.delta", { message_id: messageId, text: DELTA_FRAGMENT });
      if (run.cancelled) break;
      if (intervalMs > 0) {
        await Promise.race([this.delay(intervalMs), this.waitForInterrupt(run)]);
      }
    }
    if (run.cancelled || this.disposedAll) {
      await this.emit(context, "run.cancelled", { run_id: run.runId, reason: "interrupted" });
      return;
    }
    const usage = {
      input_tokens: 0,
      output_tokens: content.length,
      total_tokens: content.length,
    };
    await this.emit(context, "message.completed", {
      message: {
        id: messageId,
        session_id: context.sessionId,
        run_id: context.runId ?? null,
        role: "assistant",
        content,
        created_at: Date.now(),
      },
      usage,
    });
    await this.emit(context, "run.completed", { run_id: run.runId, usage });
  }

  private async runArtifactScenario(
    context: EnvelopeContext,
    run: ActiveRun,
    spec: string,
  ): Promise<void> {
    // spec = `<文件名>` 或 `<文件名>:<字节数>`（默认 3MiB，>2MiB 证明数据体无法走线协议）。
    const [rawName, rawBytes] = spec.split(":");
    const name = (rawName ?? "").trim();
    if (!name) {
      await this.emit(context, "run.failed", {
        run_id: run.runId,
        error: { code: "mock_artifacts_missing", message: "artifact: 缺少文件名", recoverable: true },
      });
      return;
    }
    const artifactsDir = this.options.artifactsDir;
    if (!artifactsDir) {
      await this.emit(context, "run.failed", {
        run_id: run.runId,
        error: {
          code: "mock_artifacts_missing",
          message: "附件目录未配置（AETHER_ARTIFACTS_DIR）",
          recoverable: true,
        },
      });
      return;
    }
    const bytes = rawBytes !== undefined && rawBytes !== "" && Number.isFinite(Number(rawBytes))
      ? Math.max(1, Math.trunc(Number(rawBytes)))
      : DEFAULT_ARTIFACT_BYTES;
    const kind = name.endsWith(".png") ? "image/png" : "application/octet-stream";
    const target = path.join(artifactsDir, name);
    try {
      // 数据体只落 artifacts 文件：标记 + 填充；内容绝不进入线协议。
      const padding = "x".repeat(Math.max(0, bytes - ARTIFACT_MARKER.length));
      await fs.writeFile(target, `${ARTIFACT_MARKER}${padding}`);
    } catch (error) {
      const detail = error instanceof Error ? error.message : String(error);
      await this.emit(context, "run.failed", {
        run_id: run.runId,
        error: { code: "mock_artifacts_missing", message: `附件写入失败: ${detail}`, recoverable: true },
      });
      return;
    }
    await this.adapter.emitArtifactRef({
      session_id: context.sessionId,
      run_id: context.runId ?? undefined,
      refs: [{ path: name, size: bytes, kind }],
    });
    const messageId = ulid();
    await this.emit(context, "message.completed", {
      message: {
        id: messageId,
        session_id: context.sessionId,
        run_id: context.runId ?? null,
        role: "assistant",
        content: `附件已存 artifacts 目录：${name}（${bytes} 字节）`,
        created_at: Date.now(),
      },
      usage: emptyUsage(),
    });
    await this.emit(context, "run.completed", { run_id: run.runId, usage: emptyUsage() });
  }

  private async runToolScenario(
    scenario: ToolCallScenarioId,
    context: EnvelopeContext,
    run: ActiveRun,
  ): Promise<void> {
    const definition = TOOL_CALL_SCENARIOS[scenario];
    const toolCallId = ulid();
    const toolName = definition.toolName;

    switch (scenario) {
      case "normal": {
        await this.emit(
          context,
          "tool.call_started",
          buildToolCallStarted(toolCallId, toolName, { path: "README.md" }),
        );
        await this.emit(
          context,
          "tool.call_completed",
          buildToolCallCompleted(toolCallId, toolName, 12),
        );
        await this.emit(context, "run.completed", { run_id: run.runId, usage: emptyUsage() });
        return;
      }
      case "fail": {
        await this.emit(
          context,
          "tool.call_started",
          buildToolCallStarted(toolCallId, toolName, { path: "missing.md" }),
        );
        await this.emit(
          context,
          "tool.call_failed",
          buildToolCallFailed(toolCallId, toolName, 7, TOOL_FAILURE_ERROR),
        );
        await this.emit(context, "run.completed", { run_id: run.runId, usage: emptyUsage() });
        return;
      }
      case "timeout": {
        const startedAt = Date.now();
        await this.emit(
          context,
          "tool.call_started",
          buildToolCallStarted(toolCallId, toolName, { path: "slow.log" }),
        );
        await this.waitForInterrupt(run);
        await this.emit(
          context,
          "tool.call_failed",
          buildToolCallFailed(toolCallId, toolName, Date.now() - startedAt, TOOL_TIMEOUT_ERROR),
        );
        await this.emit(context, "run.cancelled", { run_id: run.runId, reason: "interrupted" });
        return;
      }
      case "permission_allow":
      case "permission_deny": {
        // 边界 B1（M1-09 证据 / DoD6）：M1 阶段为事件序列**预置**——Mock 自包含产出
        // permission.requested → permission.resolved → 工具终态：
        // - 不发送 `permission.request` 通知（B3 断言其为 0）；
        // - 不等待 `permission.resolve`（决策固定在场景定义中）；
        // - 不经核心权限网关、不写任何存储（M1-09 链路无 DB 依赖，B3 静态断言）。
        // M2-10 在真实回环中重放：适配器发 permission.request → 核心网关 → permission.resolve。
        const requestId = ulid();
        const requestPayload = {
          request_id: requestId,
          resource: "fs.write",
          action: "write",
          target: "notes.md",
        };
        const presetDecision = TOOL_CALL_SCENARIOS[scenario].decision;
        await this.emit(context, "permission.requested", buildPermissionRequested(requestPayload));
        await this.emit(
          context,
          "permission.resolved",
          buildPermissionResolved(requestId, presetDecision, "once"),
        );
        if (presetDecision === "allow") {
          await this.emit(
            context,
            "tool.call_completed",
            buildToolCallCompleted(toolCallId, toolName, 9),
          );
        } else {
          await this.emit(
            context,
            "tool.call_failed",
            buildToolCallFailed(toolCallId, toolName, 5, PERMISSION_DENIED_ERROR),
          );
        }
        await this.emit(context, "run.completed", { run_id: run.runId, usage: emptyUsage() });
        return;
      }
    }
  }
}
