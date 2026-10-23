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
  CredentialClient,
  ERROR_CODES,
  RpcError,
  composePromptWithReferences,
  isUlid,
  parseReferences,
  ulid,
  type AdapterRuntimeInfo,
  type BrokerCredential,
  type EnvelopeContext,
  type JsonRpcPeer,
  type ProviderCatalog,
  type ProviderCatalogClient,
} from "@aether/adapter-sdk";
import { randomUUID } from "node:crypto";

import {
  ClaudeCliRun,
  cleanupSystemPromptFile,
  claudePermissionMode,
  materializeSystemPromptFile,
  thinkingBudgetForDepth,
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

/** 适配器能力清单（hello/initialize 上报；RA-04；M3-10 增 `thinking_depth`；
 * M7-03/ADR-017 增 `pause_resume`）。 */
export const RUNTIME_CAPABILITIES: readonly string[] = [
  "session.create",
  "session.send",
  "session.interrupt",
  "session.dispose",
  "tools.list",
  "permission.resolve",
  // M3-10/ADR-010：档位 0–4 → `MAX_THINKING_TOKENS` 预算（映射表见 M3-10 证据）。
  "thinking_depth",
  // M7-03/ADR-017：受控中断 + Mode R 原生恢复（`--session-id`/`--resume`，ADR-005）。
  "pause_resume",
  // ADR-028：按会话 provider 向核心凭据 broker 取数（同一适配器并发多供应商）。
  "multi_provider",
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
  /** M3-10/ADR-010：会话级思考深度（0–4；override 优先，仅本次 run）。 */
  thinkingDepth?: number;
  /**
   * M7-06/ADR-024 附录 A-4：档位投影后的 CLI 权限模式（仅新会话生效；
   * 在途会话保持创建时的投影参数——`session.create` 时一次判定）。
   */
  permissionMode?: string;
  /**
   * M7-09/ADR-026：`session.create.system_prompt`（合成提示词）物化文件路径；
   * 每次 CLI 调用以 `--append-system-prompt-file` 下发（`undefined` = 无系统提示词）。
   */
  systemPromptFile?: string;
  /** ADR-028：会话选中的供应商 id（broker 按需取凭据；缺省 = 旧单供应商 env）。 */
  providerId?: string;
  lastInit: ClaudeInitEvent | null;
  clientMsgIds: Map<string, string>;
  runs: Map<string, ClaudeRunState>;
  disposed: boolean;
}

/** M3-10：解析思考深度参数（整数 0–4；未知形态忽略，保持适配器宽容）。 */
function parseThinkingDepth(value: unknown): number | undefined {
  if (typeof value !== "number" || !Number.isInteger(value)) return undefined;
  if (value < 0 || value > 4) return undefined;
  return value;
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
  /** ADR-028：凭据 broker 客户端（缺省从 `AETHER_CREDENTIAL_BROKER` 构造；测试可注入）。 */
  credentialClient?: Pick<CredentialClient, "get"> | null;
  /** ADR-028 v0.9：`provider.catalog.get` 客户端工厂（生产 main 注入；测试缺省不查询）。 */
  catalogClientFactory?: (peer: JsonRpcPeer) => ProviderCatalogClient;
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
  private readonly credentialClient: Pick<CredentialClient, "get"> | null;
  private readonly catalogClient: ProviderCatalogClient | null;

  constructor(options: ClaudeAdapterOptions) {
    this.options = options;
    this.runTimeoutMs = options.runTimeoutMs ?? DEFAULT_RUN_TIMEOUT_MS;
    this.credentialClient =
      options.credentialClient === undefined
        ? CredentialClient.fromEnv()
        : options.credentialClient;
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
    this.catalogClient = options.catalogClientFactory?.(this.adapter.peer) ?? null;
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
      .handle("session.pause", (params) => this.onPause(params))
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
          // M7-09/ADR-026：适配器退出清理全部系统提示词临时文件。
          cleanupSystemPromptFile(session.systemPromptFile);
        }
        this.sessions.clear();
        this.adapter.stop();
        this.options.exit?.(0);
        return { ok: true };
      });
  }

  private async onCreateSession(params: unknown): Promise<Record<string, unknown>> {
    const input = (params ?? {}) as {
      title?: unknown;
      workspace?: unknown;
      model?: unknown;
      native_id?: unknown;
      thinking_depth?: unknown;
      permission_mode?: unknown;
      system_prompt?: unknown;
      provider_id?: unknown;
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
    const thinkingDepth = parseThinkingDepth(input.thinking_depth);
    // M7-06/ADR-024 A-4：档位 → `--permission-mode` 投影（仅新会话生效；未知档位忽略）。
    const permissionMode = claudePermissionMode(input.permission_mode);
    // M7-09/ADR-026：合成系统提示词（预设块 + 工作区约定块）→ CLI
    // `--append-system-prompt-file`（文件形态规避命令行长度上限；不改用户 settings）。
    const systemPrompt =
      typeof input.system_prompt === "string" && input.system_prompt.length > 0
        ? input.system_prompt
        : undefined;
    const systemPromptFile = systemPrompt !== undefined
      ? materializeSystemPromptFile(systemPrompt)
      : undefined;
    const providerId =
      typeof input.provider_id === "string" && input.provider_id.length > 0
        ? input.provider_id
        : undefined;
    if (providerId !== undefined) {
      await this.ensureCatalogSelection(providerId, model);
    }
    const session: ClaudeSession = {
      id: nativeId,
      nativeId,
      resumed: isUuid(requested),
      nativeCreated: isUuid(requested),
      ...(model !== undefined ? { model } : {}),
      ...(thinkingDepth !== undefined ? { thinkingDepth } : {}),
      ...(permissionMode !== undefined ? { permissionMode } : {}),
      ...(systemPromptFile !== undefined ? { systemPromptFile } : {}),
      ...(providerId !== undefined ? { providerId } : {}),
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
      // M7-09：回显是否携带系统提示词（不含正文；便于核心/测试观测，不泄露内容）。
      has_system_prompt: systemPromptFile !== undefined,
      created_at: Date.now(),
    };
  }

  private async onSend(params: unknown): Promise<Record<string, unknown>> {
    const input = (params ?? {}) as {
      session_id?: unknown;
      client_msg_id?: unknown;
      text?: unknown;
      run_id?: unknown;
      thinking_depth?: unknown;
      references?: unknown;
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
    // M3-10/ADR-010：run 启动时判定——`session.send` 覆盖优先（仅本次 run），
    // 否则会话级值；未声明档位时不注入预算（CLI 默认）。
    const thinkingDepth = parseThinkingDepth(input.thinking_depth) ?? session.thinkingDepth;
    const thinkingBudget = thinkingBudgetForDepth(thinkingDepth);
    // M7-06/ADR-024 A-3：统一映射 = 消息前缀引用块（核心不读内容；引用 ≠ 预授权）。
    const prompt = composePromptWithReferences(input.text, parseReferences(input.references));
    // ADR-028：会话选中 provider 时，从核心 broker 取当前 run 的凭据；
    // 只覆盖本次 CLI 子进程 env，不缓存。broker 未配置时显式失败，避免静默
    // 回退到可能属于其他供应商的单供应商 env。
    let providerEnv: NodeJS.ProcessEnv | undefined;
    if (session.providerId !== undefined) {
      await this.ensureCatalogSelection(session.providerId, session.model);
      if (this.credentialClient === null) {
        throw new RpcError(
          ERROR_CODES.INTERNAL_ERROR,
          "凭据 broker 未配置：无法为当前会话获取供应商密钥（请重启应用或检查运行时环境）",
        );
      }
      try {
        const credential: BrokerCredential = await this.credentialClient.get(
          session.providerId,
          "anthropic",
          "claude-code",
        );
        providerEnv = {
          ...(credential.base_url !== undefined
            ? { ANTHROPIC_BASE_URL: credential.base_url }
            : {}),
          [credential.auth_env]: credential.api_key,
        };
      } catch (error) {
        throw new RpcError(
          ERROR_CODES.INTERNAL_ERROR,
          `凭据 broker 获取失败：${error instanceof Error ? error.message : String(error)}`,
        );
      }
    }
    run.cli = new ClaudeCliRun({
      config: {
        ...this.options.cli,
        ...(session.model ? { model: session.model } : {}),
        ...(session.permissionMode !== undefined
          ? { permissionMode: session.permissionMode }
          : {}),
      },
      spec: {
        nativeId: session.nativeId,
        resume: session.resumed || session.nativeCreated,
        prompt,
        timeoutMs: this.runTimeoutMs,
        ...(thinkingBudget !== undefined ? { thinkingBudget } : {}),
        // M7-09/ADR-026：合成系统提示词经 CLI 原生 `--append-system-prompt-file` 生效
        // （首轮与 resume 同口径；会话销毁/适配器退出时清理临时文件）。
        ...(session.systemPromptFile !== undefined
          ? { appendSystemPromptFile: session.systemPromptFile }
          : {}),
      },
      onEvent: (event) => this.onCliEvent(session, run, event),
      onMalformedLine: (line) => this.onCliMalformedLine(run, line),
      onStderrLine: (line) => this.options.stderr?.(`[claude-cli] ${line}`),
      ...(this.options.spawnFn ? { spawnFn: this.options.spawnFn } : {}),
      ...(this.options.killTreeFn ? { killTreeFn: this.options.killTreeFn } : {}),
      ...(providerEnv !== undefined ? { env: providerEnv } : {}),
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

  /**
   * ADR-028 v0.9：目录查询校验（provider 启用 + model 归属/启用）。
   * 未注入目录客户端（单测）→ 跳过；生产 main 必须注入。
   */
  private async ensureCatalogSelection(
    providerId: string,
    model: string | undefined,
  ): Promise<void> {
    if (this.catalogClient === null) return;
    let catalog: ProviderCatalog;
    try {
      catalog = await this.catalogClient.fetch();
    } catch (error) {
      throw new RpcError(
        ERROR_CODES.INTERNAL_ERROR,
        `供应商目录查询失败：${error instanceof Error ? error.message : String(error)}`,
      );
    }
    const provider = catalog.providers.find(
      (entry) => entry.id === providerId && entry.enabled,
    );
    if (provider === undefined) {
      throw new RpcError(
        ERROR_CODES.INVALID_PARAMS,
        `供应商不存在或已停用：${providerId}`,
      );
    }
    if (model !== undefined) {
      const owned = provider.models.some(
        (entry) => entry.id === model && entry.enabled,
      );
      if (!owned) {
        throw new RpcError(
          ERROR_CODES.INVALID_PARAMS,
          `模型 ${model} 不属于供应商 ${providerId} 或未启用`,
        );
      }
    }
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
    // M7-09/ADR-026：会话销毁即清理系统提示词临时文件（不保留用户提示词副本）。
    cleanupSystemPromptFile(session.systemPromptFile);
    this.sessions.delete(sessionId);
    return { disposed: true };
  }

  /**
   * `session.pause`（M7-03/ADR-017）：核心侧受控中断收口后调用——取消残余工作
   * （在途 run 进程树回收）、释放会话运行时侧资源；会话保留（`nativeId` 不变，
   * Mode R 重放依赖 `--resume`）。
   */
  private onPause(params: unknown): Record<string, unknown> {
    const input = (params ?? {}) as { session_id?: unknown };
    const sessionId = typeof input.session_id === "string" ? input.session_id : "";
    const session = this.sessions.get(sessionId);
    if (!session || session.disposed) {
      throw new RpcError(ERROR_CODES.SESSION_NOT_FOUND, `会话不存在: ${sessionId || "<missing>"}`);
    }
    const cancelled: string[] = [];
    for (const run of session.runs.values()) {
      if (run.finished) continue;
      run.interrupted = true;
      void run.cli?.interrupt();
      cancelled.push(run.runId);
    }
    return { paused: true, session_id: session.id, cancelled_runs: cancelled };
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
