/**
 * DeepSeek Harness 适配器（M2-11 / ADR-002 / ADR-008）：Aether 线协议（D6）方法实现。
 *
 * - 主通道：DSH 官方 ACP（`--profile acp`，JSON-RPC over stdio）负责 session/工具/终态，
 *   权威终态取 `session/prompt` settle（`stopReason`）；
 * - 带外通道：`--patch` 注入自研插件（版本门闩 + 插件落盘），插件经 sidecar JSONL 送
 *   token 级 delta；适配器按前缀去重消费（committed 为准，通道故障按 final 重建）；
 * - `session.create`：ACP `session/new` / `session/resume`（Mode R）；
 * - `session.interrupt`：协议级 `session/cancel`，5s 未 settle 则整树回收兜底；
 * - 权限：`session/request_permission` 100% 经 `permission.request` 回环（D9 边界：
 *   适配器不直通、不落审计），核心以 `permission.resolve` 决议。
 */

import {
  Adapter,
  ERROR_CODES,
  RpcError,
  ulid,
  type AdapterRuntimeInfo,
  type EnvelopeContext,
} from "@aether/adapter-sdk";
import { mkdirSync } from "node:fs";
import { join } from "node:path";

import { DshAcpClient, AcpError, type AcpUpdateParams } from "./dsh-acp-client";
import { DeltaChannel, DeltaReconciler, type DeltaFrame } from "./dsh-delta";
import {
  checkVersionLatch,
  materializePlugin,
  PLUGIN_CONTRACT,
  resolveDshVersion,
  writePatchFile,
} from "./dsh-plugin";

/** 运行时 id（DDL 注释/官方白名单）。 */
export const RUNTIME_NAME = "deepseek-harness";
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

/** 默认 run 硬超时（常量级；核心 120s 断流看门狗优先）。 */
export const DEFAULT_RUN_TIMEOUT_MS = 30 * 60 * 1000;
/** 权限决议等待上限（D9 300s 超时 deny；适配器侧同口径兜底）。 */
export const DEFAULT_PERMISSION_TIMEOUT_MS = 300_000;
/** 中断后等待 `session/prompt` settle 的上限（D6 `session.interrupt` 5s）。 */
export const INTERRUPT_SETTLE_TIMEOUT_MS = 5_000;
/**
 * 会话创建时等待 settings provider 路由注册的兜底预算（防御纵深）。
 *
 * 官方路径已由 `llm-pi-ai` composition base 同步注册消除竞态；本预算仅覆盖
 * 极端情况（如核心未传 `providerConfig`，纯依赖 settings 异步注入）。
 */
export const SESSION_PROVIDER_WAIT_MS = 5_000;
export const SESSION_PROVIDER_WAIT_INTERVAL_MS = 250;
/** 插件 hello 契约帧等待上限（版本门闩第二条件）。 */
export const DEFAULT_PLUGIN_HELLO_TIMEOUT_MS = 5_000;

/** 适配器侧稳定错误码。 */
export const DSH_ERROR_CODES = {
  /** DSH 进程/ACP 连接异常退出。 */
  SERVER_EXITED: "dsh_server_exited",
  /** `session/prompt` 返回错误。 */
  PROMPT_ERROR: "dsh_prompt_error",
  /** 适配器侧 run 超时（兜底；核心 120s 断流看门狗优先）。 */
  RUN_TIMEOUT: "run_timeout",
  /** 工具调用被中断（与 Mock ③/Claude 口径一致：message 含 abort）。 */
  TOOL_TIMEOUT: "timeout",
  /** ACP `tool_call_update.status=failed`。 */
  TOOL_EXECUTION_FAILED: "tool_execution_failed",
  /** 通道异常但 run 正常完成（仅诊断，不改变终态）。 */
  DELTA_FALLBACK: "delta_channel_fallback",
} as const;

/** DSH 工具目录（`tools.list` 静态部分；ACP 的 `tool_call.kind` 口径）。 */
export const DSH_TOOL_CATALOG: ReadonlyArray<{ name: string; description: string }> = [
  { name: "read", description: "读取文件" },
  { name: "edit", description: "编辑/写入文件" },
  { name: "execute", description: "执行命令" },
  { name: "search", description: "检索文件/内容" },
  { name: "think", description: "推理（内部）" },
];

export interface ToolDefinition {
  name: string;
  description: string;
  input_schema: Record<string, unknown>;
}

/**
 * ACP `tool_call`（kind/title）→ 权限回环 `resource`/`action`（M2-10 冻结形状：
 * `{request_id, session_id?, run_id?, resource, action, target?}`）。
 */
export function permissionResourceFor(
  kind: string | undefined,
  title: string | undefined,
): { resource: string; action: string } {
  const key = `${kind ?? ""} ${title ?? ""}`.toLowerCase();
  if (/read|view|glob|grep|search|list/.test(key)) {
    return { resource: "fs.read", action: "read" };
  }
  if (/edit|write|create|delete|move|rename|patch/.test(key)) {
    return { resource: "fs.write", action: "write" };
  }
  if (/execute|command|shell|bash|terminal|run/.test(key)) {
    return { resource: "exec", action: "execute" };
  }
  if (/net|fetch|http|web|download/.test(key)) {
    return { resource: "net", action: "fetch" };
  }
  return { resource: "tool", action: (kind ?? "call").toLowerCase() };
}

/** 依据观察到的工具名构造定义（未观察 → 全量目录）。 */
export function toolDefinitions(observed?: ReadonlySet<string>): ToolDefinition[] {
  const catalog = new Map(DSH_TOOL_CATALOG.map((tool) => [tool.name, tool]));
  const names =
    observed && observed.size > 0 ? [...observed] : DSH_TOOL_CATALOG.map((tool) => tool.name);
  return names.map((name) => ({
    name,
    description: catalog.get(name)?.description ?? "DSH 工具",
    input_schema: { type: "object" },
  }));
}

export interface DshCliConfig {
  /** `@deepseek-ai/dsh/lib/bin.js`（生产由核心物化路径）。 */
  bin: string;
  /** Node 可执行文件（默认当前进程）。 */
  nodeBin?: string;
  /** 隔离 `DSH_HOME`。 */
  home: string;
  /** profile（默认 `acp`）。 */
  profile: string;
  /** overlay 覆写 provider（可选）。 */
  provider?: string;
  /** overlay 覆写 model（可选）。 */
  model?: string;
  /** 追加参数（测试注入等）。 */
  extraArgs: string[];
  /** 工作目录（ACP `session/new` 的 cwd）。 */
  workspace: string;
  /** 版本门闩 pin（默认 `0.1.5-rc.2`）。 */
  versionPin: string;
  /** 显式 DSH 版本（测试/无包路径场景；缺省从 `bin` 向上解析 package.json）。 */
  dshVersion?: string;
  /**
   * `llm-pi-ai` composition base（`{providers:{...}}`；由核心从同一份 provider 物化
   * 结果传入，见 `dsh-plugin.ts` 的 `PatchOptions.providerConfig`）。
   * 声明后 provider 路由在插件 apply 时同步注册，消除 settings 异步注入竞态。
   */
  providerConfig?: Record<string, unknown>;
  /** overlay 文件路径（缺省生成到 DSH_HOME 下）。 */
  patchPath?: string;
  /** 带外通道目录（默认 `<DSH_HOME>/aether-bridge`）。 */
  deltaDir?: string;
  /** 插件 hello 等待上限。 */
  helloTimeoutMs?: number;
}

export interface DshAdapterOptions {
  lines: AsyncIterable<string>;
  writeLine: (line: string) => Promise<void>;
  stderr?: (line: string) => void;
  exit?: (code: number) => void;
  cli: DshCliConfig;
  runTimeoutMs?: number;
  permissionTimeoutMs?: number;
  runtimeVersion?: string;
}

interface DshRunState {
  runId: string;
  sessionId: string;
  messageId: string;
  reconciler: DeltaReconciler;
  attemptIds: Set<string>;
  emitChain: Promise<void>;
  interrupted: boolean;
  finished: boolean;
  stopReason: string | null;
  promptError: string | null;
  usage: Record<string, number> | null;
  openTools: Map<string, { name: string; args: unknown; startedAt: number; settled: boolean }>;
  settleWaiters: Array<() => void>;
}

interface DshSession {
  id: string;
  nativeId: string;
  resumed: boolean;
  model?: string;
  clientMsgIds: Map<string, string>;
  runs: Map<string, DshRunState>;
  observedTools: Set<string>;
  disposed: boolean;
}

interface PendingPermission {
  resolve: (decision: { decision: string; optionId?: string }) => void;
}

interface PluginHello {
  contract: string;
  dshVersion: string | null;
}

export class DshAdapter {
  readonly adapter: Adapter;
  readonly runtime: Required<AdapterRuntimeInfo>;
  readonly runTimeoutMs: number;
  readonly permissionTimeoutMs: number;

  private readonly options: DshAdapterOptions;
  private readonly sessions = new Map<string, DshSession>();
  private readonly attempts = new Map<string, string>();
  private readonly pendingPermissions = new Map<string, PendingPermission>();
  private readonly startedAt = Date.now();

  private server: DshAcpClient | null = null;
  private channel: DeltaChannel | null = null;
  private pluginHello: PluginHello | null = null;
  private initializeInfo: Record<string, unknown> | null = null;
  private initializePromise: Promise<Record<string, unknown>> | null = null;
  private shuttingDown = false;

  constructor(options: DshAdapterOptions) {
    this.options = options;
    this.runTimeoutMs = options.runTimeoutMs ?? DEFAULT_RUN_TIMEOUT_MS;
    this.permissionTimeoutMs = options.permissionTimeoutMs ?? DEFAULT_PERMISSION_TIMEOUT_MS;
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

  run(): Promise<void> {
    return this.adapter.run();
  }

  /** 残留通道数（DoD6 探针：`session.dispose` 后必须为 0）。 */
  residualChannels(): number {
    return this.attempts.size;
  }

  get sessionCount(): number {
    return this.sessions.size;
  }

  get pluginContract(): string | null {
    return this.pluginHello?.contract ?? null;
  }

  private registerHandlers(): void {
    this.adapter
      .handle("initialize", () => this.onInitialize())
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
      .handle("permission.resolve", (params) => {
        const input = (params ?? {}) as {
          request_id?: unknown;
          decision?: unknown;
          option_id?: unknown;
        };
        const requestId = typeof input.request_id === "string" ? input.request_id : "";
        const pending = this.pendingPermissions.get(requestId);
        if (!pending) return { resolved: false, reason: "未知/已超时的 request_id" };
        const decision = input.decision === "allow" ? "allow" : "deny";
        const optionId = typeof input.option_id === "string" ? input.option_id : undefined;
        this.pendingPermissions.delete(requestId);
        pending.resolve(optionId !== undefined ? { decision, optionId } : { decision });
        return { resolved: true, decision };
      })
      .handle("health.ping", () => ({
        status: this.server && !this.server.isClosed ? "ok" : "degraded",
        uptime_ms: Date.now() - this.startedAt,
        pid: process.pid,
        dsh_pid: this.server?.pid ?? null,
        plugin_contract: this.pluginContract,
        channels: this.attempts.size,
        sessions: this.sessions.size,
      }))
      .handle("shutdown", () => {
        void this.onShutdown();
        return { ok: true };
      });
  }

  // ===== 初始化 / 版本门闩 / 插件注入 =====

  private onInitialize(): Promise<Record<string, unknown>> | Record<string, unknown> {
    if (this.initializeInfo !== null) return this.initializeInfo;
    if (this.initializePromise === null) {
      this.initializePromise = this.performInitialize();
    }
    return this.initializePromise;
  }

  private async performInitialize(): Promise<Record<string, unknown>> {
    const cli = this.options.cli;
    const profileDir = join(cli.home, "profiles", cli.profile);
    const deltaDir = cli.deltaDir ?? join(cli.home, "aether-bridge");
    mkdirSync(deltaDir, { recursive: true });
    const deltaFile = join(deltaDir, `delta-${process.pid}-${Date.now()}.jsonl`);

    // DoD2：插件落盘（独立包名 + 无 BOM 字节断言）与 overlay 生成。
    try {
      materializePlugin(profileDir);
    } catch (error) {
      throw new RpcError(
        ERROR_CODES.INTERNAL_ERROR,
        `插件注入失败：${error instanceof Error ? error.message : String(error)}`,
      );
    }
    const patchPath =
      cli.patchPath ??
      writePatchFile(cli.home, cli.profile, {
        ...(cli.provider !== undefined ? { provider: cli.provider } : {}),
        ...(cli.model !== undefined ? { model: cli.model } : {}),
        ...(cli.providerConfig !== undefined ? { providerConfig: cli.providerConfig } : {}),
      });

    // DoD1：版本门闩（pin 精确匹配 + 插件契约帧）。
    const version = cli.dshVersion ?? resolveDshVersion(cli.bin);
    const latch = checkVersionLatch(version, cli.versionPin);
    if (!latch.ok) {
      throw new RpcError(ERROR_CODES.VERSION_MISMATCH, latch.detail);
    }

    this.channel = new DeltaChannel(deltaFile, {
      onFrame: (frame) => this.onDeltaFrame(frame),
      onMalformedLine: (line) =>
        this.options.stderr?.(`[dsh-delta] 无法解析的帧：${line.slice(0, 200)}`),
    });
    this.channel.start();

    this.server = new DshAcpClient({
      bin: cli.bin,
      ...(cli.nodeBin !== undefined ? { nodeBin: cli.nodeBin } : {}),
      profile: cli.profile,
      patchPath,
      home: cli.home,
      workspace: cli.workspace,
      deltaFile,
      extraArgs: cli.extraArgs,
      onUpdate: (params) => this.onAcpUpdate(params),
      onPermissionRequest: (params, requestId) => this.onPermissionRequest(params, requestId),
      onStderr: (line) => this.options.stderr?.(line),
      onClose: (detail) => this.onServerClosed(detail),
    });

    try {
      const initResult = await this.server.request(
        "initialize",
        {
          protocolVersion: 1,
          clientCapabilities: {},
          clientInfo: { name: "aether-adapter-dsh", version: ADAPTER_VERSION },
        },
        10_000,
      );
      const hello = await this.waitPluginHello(
        cli.helloTimeoutMs ?? DEFAULT_PLUGIN_HELLO_TIMEOUT_MS,
      );
      this.pluginHello = hello;
      this.initializeInfo = {
        capabilities: this.runtime.capabilities,
        protocol: "1.0",
        acknowledged: true,
        dsh_version: version,
        plugin_contract: hello.contract,
        acp: initResult ?? {},
      };
      return this.initializeInfo;
    } catch (error) {
      await this.cleanupServer();
      if (error instanceof RpcError) throw error;
      if (error instanceof AcpError) {
        throw new RpcError(
          ERROR_CODES.INTERNAL_ERROR,
          `DSH ACP initialize 失败：${error.message}`,
        );
      }
      throw new RpcError(
        ERROR_CODES.INTERNAL_ERROR,
        `DSH 启动失败：${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }

  private waitPluginHello(timeoutMs: number): Promise<PluginHello> {
    if (this.pluginHello !== null) return Promise.resolve(this.pluginHello);
    return new Promise<PluginHello>((resolve, reject) => {
      const started = Date.now();
      const timer = setInterval(() => {
        if (this.pluginHello !== null) {
          clearInterval(timer);
          resolve(this.pluginHello);
          return;
        }
        if (Date.now() - started >= timeoutMs) {
          clearInterval(timer);
          reject(
            new RpcError(
              ERROR_CODES.VERSION_MISMATCH,
              `插件契约帧未在 ${timeoutMs}ms 内到达（期望 ${PLUGIN_CONTRACT}）→ DSH 内部事件契约不匹配，` +
                `请确认 DSH 版本 pin 与插件注入结果（ADR-002 §3.1）`,
            ),
          );
        }
      }, 25);
      if (typeof timer.unref === "function") timer.unref();
    });
  }

  private async cleanupServer(): Promise<void> {
    this.channel?.stop();
    this.channel = null;
    if (this.server) {
      const server = this.server;
      this.server = null;
      await server.close(2_000);
    }
  }

  // ===== D6 会话方法 =====

  private onCreateSession(params: unknown): Promise<Record<string, unknown>> {
    return this.performCreateSession(params);
  }

  private async performCreateSession(params: unknown): Promise<Record<string, unknown>> {
    await this.ensureInitialized();
    const input = (params ?? {}) as {
      title?: unknown;
      workspace?: unknown;
      model?: unknown;
      native_id?: unknown;
    };
    const requested = input.native_id;
    if (requested !== undefined && (typeof requested !== "string" || requested.length === 0)) {
      throw new RpcError(ERROR_CODES.INVALID_PARAMS, "native_id 必须是非空字符串（ACP sessionId）");
    }
    const model = typeof input.model === "string" && input.model.length > 0 ? input.model : undefined;
    const server = this.server;
    if (!server) throw new RpcError(ERROR_CODES.INTERNAL_ERROR, "DSH ACP 未连接");
    let sessionId: string;
    let resumed = false;
    try {
      if (typeof requested === "string") {
        const result = (await this.requestWithProviderWait(
          "session/resume",
          { sessionId: requested, cwd: this.options.cli.workspace, mcpServers: [] },
          15_000,
        )) as { sessionId?: string } | null;
        sessionId = result?.sessionId ?? requested;
        resumed = true;
      } else {
        const result = (await this.requestWithProviderWait(
          "session/new",
          { cwd: this.options.cli.workspace, mcpServers: [] },
          30_000,
        )) as { sessionId?: string } | null;
        if (!result?.sessionId) {
          throw new RpcError(ERROR_CODES.INTERNAL_ERROR, "session/new 未返回 sessionId");
        }
        sessionId = result.sessionId;
      }
    } catch (error) {
      if (error instanceof AcpError) {
        const details = error.data === undefined ? "" : `（${JSON.stringify(error.data)}）`;
        throw new RpcError(
          ERROR_CODES.SESSION_NOT_FOUND,
          `DSH 会话不可用：${error.message}${details}`,
        );
      }
      throw error;
    }
    const session: DshSession = {
      id: sessionId,
      nativeId: sessionId,
      resumed,
      ...(model !== undefined ? { model } : {}),
      clientMsgIds: new Map(),
      runs: new Map(),
      observedTools: new Set(),
      disposed: false,
    };
    this.sessions.set(sessionId, session);
    return {
      session_id: session.id,
      native_id: session.nativeId,
      resumed: session.resumed,
      model: model ?? this.options.cli.model ?? null,
      created_at: Date.now(),
    };
  }

  /**
   * `session/new|resume` 路由就绪等待：仅对 `no adapter registered for provider`
   * 做有界重试（≤ [`SESSION_PROVIDER_WAIT_MS`]），其余错误立即上抛。
   */
  private async requestWithProviderWait(
    method: string,
    params: unknown,
    timeoutMs: number,
  ): Promise<unknown> {
    const server = this.server;
    if (!server) throw new RpcError(ERROR_CODES.INTERNAL_ERROR, "DSH ACP 未连接");
    const deadline = Date.now() + SESSION_PROVIDER_WAIT_MS;
    for (;;) {
      try {
        return await server.request(method, params, timeoutMs);
      } catch (error) {
        const detail =
          error instanceof AcpError
            ? `${error.message} ${JSON.stringify(error.data ?? {})}`
            : error instanceof Error
              ? error.message
              : String(error);
        const retryable =
          error instanceof AcpError && /no adapter registered for provider/i.test(detail);
        if (!retryable || Date.now() >= deadline) throw error;
        this.options.stderr?.(
          `[dsh-acp] ${method}: provider 路由尚未注册，等待重试（${detail.trim()}）`,
        );
        await new Promise((resolve) =>
          setTimeout(resolve, SESSION_PROVIDER_WAIT_INTERVAL_MS),
        );
      }
    }
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
    if (!this.server || this.server.isClosed) {
      throw new RpcError(ERROR_CODES.INTERNAL_ERROR, "DSH ACP 未连接");
    }
    const clientMsgId =
      typeof input.client_msg_id === "string" && input.client_msg_id.length > 0
        ? input.client_msg_id
        : ulid();
    const existing = session.clientMsgIds.get(clientMsgId);
    if (existing !== undefined) {
      return { accepted: true, run_id: existing, duplicate: true };
    }
    const runId = typeof input.run_id === "string" && input.run_id.length > 0 ? input.run_id : ulid();
    const messageId = ulid();
    const run: DshRunState = {
      runId,
      sessionId: session.id,
      messageId,
      reconciler: new DeltaReconciler(),
      attemptIds: new Set(),
      emitChain: Promise.resolve(),
      interrupted: false,
      finished: false,
      stopReason: null,
      promptError: null,
      usage: null,
      openTools: new Map(),
      settleWaiters: [],
    };
    session.clientMsgIds.set(clientMsgId, runId);
    session.runs.set(runId, run);

    void this.enqueue(run, async () => {
      await this.emit(run, "run.started", { run_id: runId });
    });
    void this.drivePrompt(session, run, input.text);
    return { accepted: true, run_id: runId };
  }

  private async drivePrompt(session: DshSession, run: DshRunState, text: string): Promise<void> {
    const server = this.server;
    if (!server) return;
    try {
      const result = (await server.request(
        "session/prompt",
        { sessionId: session.id, prompt: [{ type: "text", text }] },
        this.runTimeoutMs,
      )) as { stopReason?: string } | null;
      run.stopReason = result?.stopReason ?? null;
    } catch (error) {
      run.promptError = error instanceof Error ? error.message : String(error);
    }
    this.channel?.poll();
    await this.finalizeRun(run);
  }

  private async finalizeRun(run: DshRunState): Promise<void> {
    await this.enqueue(run, async () => {
      if (run.finished) return;
      run.finished = true;
      const session = this.sessions.get(run.sessionId);
      if (run.interrupted || run.stopReason === "cancelled") {
        await this.failOpenTools(run, DSH_ERROR_CODES.TOOL_TIMEOUT, "工具调用被中断（abort）");
        await this.emit(run, "run.cancelled", { run_id: run.runId, reason: "interrupted" });
      } else if (run.promptError !== null) {
        await this.failOpenTools(run, DSH_ERROR_CODES.PROMPT_ERROR, "run 失败前工具调用未收口");
        await this.emit(run, "run.failed", {
          run_id: run.runId,
          error: {
            code: DSH_ERROR_CODES.PROMPT_ERROR,
            message: run.promptError,
            recoverable: true,
          },
        });
      } else {
        const content = run.reconciler.finalText();
        const message = {
          id: run.messageId,
          session_id: run.sessionId,
          run_id: run.runId,
          role: "assistant",
          content,
          created_at: Date.now(),
        };
        const completedPayload: Record<string, unknown> = { message };
        if (run.usage) completedPayload["usage"] = run.usage;
        await this.emit(run, "message.completed", completedPayload);
        const runPayload: Record<string, unknown> = { run_id: run.runId };
        if (run.usage) runPayload["usage"] = run.usage;
        await this.emit(run, "run.completed", runPayload);
      }
      this.releaseRun(run, session);
    });
  }

  private onInterrupt(params: unknown): Promise<Record<string, unknown>> {
    const input = (params ?? {}) as { session_id?: unknown };
    const sessionId = typeof input.session_id === "string" ? input.session_id : "";
    const session = this.sessions.get(sessionId);
    if (!session || session.disposed) {
      throw new RpcError(ERROR_CODES.SESSION_NOT_FOUND, `会话不存在: ${sessionId || "<missing>"}`);
    }
    const active = [...session.runs.values()].filter((run) => !run.finished);
    if (active.length === 0) return Promise.resolve({ interrupted: false });
    const run = active[0] as DshRunState;
    for (const item of active) {
      item.interrupted = true;
    }
    this.server?.notify("session/cancel", { sessionId: session.id });
    const settle = this.waitSettle(run, INTERRUPT_SETTLE_TIMEOUT_MS);
    return settle.then(async (settled) => {
      if (!settled) {
        // 协议级取消未生效（≤5s）→ 整树回收兜底（D5）。
        await this.server?.kill();
      }
      return { interrupted: true, run_id: run.runId };
    });
  }

  private onDispose(params: unknown): Promise<Record<string, unknown>> {
    const input = (params ?? {}) as { session_id?: unknown };
    const sessionId = typeof input.session_id === "string" ? input.session_id : "";
    const session = this.sessions.get(sessionId);
    if (!session) {
      throw new RpcError(ERROR_CODES.SESSION_NOT_FOUND, `会话不存在: ${sessionId || "<missing>"}`);
    }
    session.disposed = true;
    for (const run of session.runs.values()) {
      run.interrupted = true;
    }
    this.server?.request("session/close", { sessionId: session.id }, 5_000).catch(() => undefined);
    this.sessions.delete(sessionId);
    this.releaseRuns(session);
    return Promise.resolve({ disposed: true, residual_channels: this.residualChannels() });
  }

  private async onShutdown(): Promise<void> {
    this.shuttingDown = true;
    for (const session of this.sessions.values()) {
      session.disposed = true;
      for (const run of session.runs.values()) {
        run.interrupted = true;
      }
      // 不调用 `session/close`：DSH 的会话持久化正是 Mode R（session/resume）的依据，
      // 关闭会话会丢弃原生上下文；进程退出本身经 stdin EOF 收口（spike 已知坑 18）。
      this.releaseRuns(session);
    }
    this.sessions.clear();
    this.pendingPermissions.clear();
    await this.cleanupServer();
    this.adapter.stop();
    this.options.exit?.(0);
  }

  private async ensureInitialized(): Promise<void> {
    const info = await this.onInitialize();
    if (info === null) {
      throw new RpcError(ERROR_CODES.INTERNAL_ERROR, "DSH 未初始化");
    }
  }

  // ===== 带外通道路由 =====

  private onDeltaFrame(frame: DeltaFrame): void {
    if (frame.type === "hello") {
      this.pluginHello = { contract: frame.contract ?? "", dshVersion: frame.dshVersion ?? null };
      return;
    }
    if (frame.type === "start") {
      const run = this.resolveRunForFrame(frame);
      if (run && frame.attemptId) {
        run.attemptIds.add(frame.attemptId);
        this.attempts.set(frame.attemptId, run.runId);
      }
      return;
    }
    if (frame.type === "chunk") {
      const run = frame.attemptId ? this.runByAttempt(frame.attemptId) : this.activeRunForFrame(frame);
      if (!run) return;
      if (frame.attemptId && !this.attempts.has(frame.attemptId)) {
        run.attemptIds.add(frame.attemptId);
        this.attempts.set(frame.attemptId, run.runId);
      }
      if (frame.chunkType !== undefined && frame.chunkType !== "text-delta") {
        return; // 推理/工具增量 P0 不上报（附录 B 预留类型不启用）。
      }
      const text = frame.text ?? "";
      if (text.length === 0) return;
      const suffix = run.reconciler.onChunk(text);
      if (suffix.length > 0) {
        void this.enqueue(run, async () => {
          await this.emit(run, "message.delta", { message_id: run.messageId, text: suffix });
        });
      }
      return;
    }
    if (frame.type === "end") {
      if (frame.attemptId) {
        const run = this.runByAttempt(frame.attemptId);
        run?.reconciler.markEnd();
        this.attempts.delete(frame.attemptId);
      }
    }
  }

  private resolveRunForFrame(frame: DeltaFrame): DshRunState | null {
    if (frame.attemptId) {
      const known = this.runByAttempt(frame.attemptId);
      if (known) return known;
    }
    return this.activeRunForFrame(frame);
  }

  private runByAttempt(attemptId: string): DshRunState | null {
    const runId = this.attempts.get(attemptId);
    if (runId === undefined) return null;
    for (const session of this.sessions.values()) {
      const run = session.runs.get(runId);
      if (run) return run;
    }
    return null;
  }

  /** 未携带 attemptId 的帧：按 sessionId 或唯一活跃 run 绑定（多活跃且无 sessionId → 丢弃）。 */
  private activeRunForFrame(frame: DeltaFrame): DshRunState | null {
    if (frame.sessionId) {
      const session = this.sessions.get(frame.sessionId);
      if (session) {
        const active = [...session.runs.values()].filter((run) => !run.finished);
        if (active.length === 1) return active[0] as DshRunState;
      }
      return null;
    }
    const active: DshRunState[] = [];
    for (const session of this.sessions.values()) {
      for (const run of session.runs.values()) {
        if (!run.finished) active.push(run);
      }
    }
    return active.length === 1 ? (active[0] as DshRunState) : null;
  }

  // ===== ACP 通知路由 =====

  private onAcpUpdate(params: AcpUpdateParams): void {
    const sessionId = params.sessionId;
    const session = sessionId ? this.sessions.get(sessionId) : undefined;
    const update = params.update;
    if (!session || !update) return;
    const active = [...session.runs.values()].filter((run) => !run.finished);
    const run = active[0];
    if (!run) return;
    const kind = update.sessionUpdate;
    if (kind === "agent_message_chunk") {
      const text = update.content?.text ?? "";
      if (text.length === 0) return;
      // 先同步排空带外通道（写入顺序先于 ACP update），保证前缀去重判定稳定。
      this.channel?.poll();
      const result = run.reconciler.onCommitted(text);
      if (result.suffix.length > 0) {
        void this.enqueue(run, async () => {
          await this.emit(run, "message.delta", { message_id: run.messageId, text: result.suffix });
        });
      }
      return;
    }
    if (kind === "tool_call") {
      const toolCallId = update.toolCallId;
      if (!toolCallId || run.openTools.has(toolCallId)) return;
      const name = update.title ?? update.kind ?? "tool";
      run.openTools.set(toolCallId, {
        name,
        args: update.rawInput ?? {},
        startedAt: Date.now(),
        settled: false,
      });
      session.observedTools.add(update.kind ?? name);
      void this.enqueue(run, async () => {
        await this.emit(run, "tool.call_started", {
          tool_call_id: toolCallId,
          tool_name: name,
          args: update.rawInput ?? {},
        });
      });
      return;
    }
    if (kind === "tool_call_update") {
      const toolCallId = update.toolCallId;
      if (!toolCallId) return;
      const tracked = run.openTools.get(toolCallId);
      if (!tracked || tracked.settled) return;
      if (update.status !== "completed" && update.status !== "failed") return;
      tracked.settled = true;
      const durationMs = Math.max(0, Date.now() - tracked.startedAt);
      if (update.status === "failed") {
        void this.enqueue(run, async () => {
          await this.emit(run, "tool.call_failed", {
            tool_call_id: toolCallId,
            tool_name: tracked.name,
            duration_ms: durationMs,
            error: {
              code: DSH_ERROR_CODES.TOOL_EXECUTION_FAILED,
              message: `ACP tool_call_update.status=failed（${tracked.name}）`,
              recoverable: true,
            },
          });
        });
      } else {
        void this.enqueue(run, async () => {
          await this.emit(run, "tool.call_completed", {
            tool_call_id: toolCallId,
            tool_name: tracked.name,
            duration_ms: durationMs,
          });
        });
      }
      return;
    }
    if (kind === "usage_update") {
      // ACP 只给上下文占用（非输出 token）→ 仅诊断，不伪造 usage 字段（spike 已知坑 15）。
      this.options.stderr?.(
        `[dsh-acp] usage_update used=${String(update["used"] ?? "?")} size=${String(update["size"] ?? "?")}`,
      );
    }
  }

  // ===== 权限回环（D9） =====

  private async onPermissionRequest(
    params: unknown,
    _requestId: number | string,
  ): Promise<unknown> {
    const input = (params ?? {}) as {
      sessionId?: string;
      toolCall?: { toolCallId?: string; title?: string; kind?: string; rawInput?: unknown };
      options?: Array<{ optionId?: string; kind?: string; name?: string }>;
    };
    const sessionId = input.sessionId ?? "";
    const session = this.sessions.get(sessionId);
    const run = session ? [...session.runs.values()].find((item) => !item.finished) : undefined;
    // 真实 ACP ask 可能只带 toolCallId（title/kind/rawInput 缺失）→ 用前置 tool_call 跟踪补全。
    const tracked =
      input.toolCall?.toolCallId !== undefined
        ? run?.openTools.get(input.toolCall.toolCallId)
        : undefined;
    const toolName = input.toolCall?.title ?? input.toolCall?.kind ?? tracked?.name ?? "unknown";
    const kind = input.toolCall?.kind ?? input.toolCall?.title ?? tracked?.name;
    const { resource, action } = permissionResourceFor(input.toolCall?.kind, kind);
    const rawInput = (input.toolCall?.rawInput ??
      tracked?.args ??
      {}) as Record<string, unknown>;
    const target =
      typeof rawInput["path"] === "string"
        ? (rawInput["path"] as string)
        : typeof rawInput["command"] === "string"
          ? (rawInput["command"] as string)
          : null;
    const requestId = ulid();
    const decisionPromise = new Promise<{ decision: string; optionId?: string }>((resolve) => {
      this.pendingPermissions.set(requestId, { resolve });
    });
    await this.adapter.emitPermissionRequest({
      request_id: requestId,
      session_id: sessionId,
      run_id: run?.runId ?? null,
      resource,
      action,
      target,
      tool_call_id: input.toolCall?.toolCallId ?? null,
      tool_name: toolName,
      args: rawInput,
      options: input.options ?? [],
      requested_at: Date.now(),
    });
    // 超时默认拒绝（D9 300s 超时 deny 的适配器侧兜底）。
    const timeoutPromise = new Promise<{ decision: string; optionId?: string }>((resolve) => {
      const timer = setTimeout(() => {
        this.pendingPermissions.delete(requestId);
        resolve({ decision: "deny" });
      }, this.permissionTimeoutMs);
      if (typeof timer.unref === "function") timer.unref();
    });
    const resolved = await Promise.race([decisionPromise, timeoutPromise]);
    if (resolved.decision === "allow") {
      const options = input.options ?? [];
      const optionId =
        resolved.optionId ??
        options.find((option) => /always|session/i.test(option.optionId ?? option.kind ?? ""))
          ?.optionId ??
        options.find((option) => /allow/i.test(option.optionId ?? option.kind ?? ""))?.optionId ??
        options[0]?.optionId;
      if (optionId !== undefined) {
        return { outcome: { outcome: "selected", optionId } };
      }
      return { outcome: { outcome: "cancelled" } };
    }
    return { outcome: { outcome: "cancelled" } };
  }

  // ===== 生命周期收口 =====

  private onServerClosed(detail: string): void {
    if (this.shuttingDown) return;
    this.options.stderr?.(`[dsh-acp] ${detail}`);
    const runs: DshRunState[] = [];
    for (const session of this.sessions.values()) {
      for (const run of session.runs.values()) {
        if (!run.finished) runs.push(run);
      }
    }
    for (const run of runs) {
      run.promptError = detail;
      void this.finalizeRun(run);
    }
  }

  private async failOpenTools(run: DshRunState, code: string, message: string): Promise<void> {
    for (const [toolCallId, tracked] of run.openTools) {
      if (tracked.settled) continue;
      tracked.settled = true;
      await this.emit(run, "tool.call_failed", {
        tool_call_id: toolCallId,
        tool_name: tracked.name,
        duration_ms: Math.max(0, Date.now() - tracked.startedAt),
        error: { code, message, recoverable: true },
      });
    }
  }

  private waitSettle(run: DshRunState, timeoutMs: number): Promise<boolean> {
    if (run.finished) return Promise.resolve(true);
    return new Promise<boolean>((resolve) => {
      const timer = setTimeout(() => resolve(false), timeoutMs);
      run.settleWaiters.push(() => {
        clearTimeout(timer);
        resolve(true);
      });
    });
  }

  private releaseRun(run: DshRunState, session: DshSession | undefined): void {
    for (const attemptId of run.attemptIds) {
      this.attempts.delete(attemptId);
    }
    run.attemptIds.clear();
    for (const waiter of run.settleWaiters.splice(0)) waiter();
    session?.runs.delete(run.runId);
  }

  private releaseRuns(session: DshSession): void {
    for (const run of session.runs.values()) {
      for (const attemptId of run.attemptIds) {
        this.attempts.delete(attemptId);
      }
      run.attemptIds.clear();
      run.interrupted = true;
      run.finished = true;
      for (const waiter of run.settleWaiters.splice(0)) waiter();
    }
  }

  private enqueue(run: DshRunState, task: () => Promise<void>): Promise<void> {
    const next = run.emitChain.then(task, task);
    run.emitChain = next.then(
      () => undefined,
      () => undefined,
    );
    return next;
  }

  private emit(
    run: DshRunState,
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
