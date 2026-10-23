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
  CredentialClient,
  ERROR_CODES,
  RpcError,
  composePromptWithReferences,
  isUlid,
  parseReferences,
  stripBrokerEnv,
  ulid,
  type AdapterRuntimeInfo,
  type BrokerCredential,
  type EnvelopeContext,
} from "@aether/adapter-sdk";
import { cpSync, existsSync, mkdirSync, rmSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import {
  CodexCliRun,
  codexSandbox,
  developerInstructionsLimit,
  materializeCodexProviderConfig,
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

/**
 * 超长系统提示词的回退前缀块标记（M7-09/ADR-026）。
 *
 * 仅当 `developer_instructions` 编码后超出平台命令行长度上限时使用：内容完整保留，
 * 以显式块标记置于用户消息前（通道变更可见、可审计；非静默丢弃）。
 */
export const SYSTEM_PROMPT_FALLBACK_HEADER = "<aether-system-prompt>";
export const SYSTEM_PROMPT_FALLBACK_FOOTER = "</aether-system-prompt>";

/** 适配器能力清单（hello/initialize 上报；M3-10 增 `thinking_depth`；
 * M7-03/ADR-017 增 `pause_resume`）。 */
export const RUNTIME_CAPABILITIES: readonly string[] = [
  "session.create",
  "session.send",
  "session.interrupt",
  "session.dispose",
  "tools.list",
  "permission.resolve",
  // M3-10/ADR-010：档位 0–4 → `model_reasoning_effort`（映射表见 M3-10 证据）。
  "thinking_depth",
  // M7-03/ADR-017：受控中断 + Mode R 原生恢复（`codex exec resume`，ADR-008 §3.3）。
  "pause_resume",
  // ADR-028 v0.9：会话级多供应商（per-session CODEX_HOME + 按 run 取凭据）。
  "multi_provider",
];

/**
 * 思考深度档位 → Codex `model_reasoning_effort`（ADR-010 决策 2）。
 *
 * Codex 原生刻度为 minimal/low/medium/high（四档）：本表把 0–4 映射到该刻度，
 * 档位 4（最大）落于原生上限 high（无 xhigh 时 3/4 同值；映射口径见证据文档）。
 */
export const CODEX_REASONING_BY_DEPTH: readonly string[] = [
  "minimal",
  "low",
  "medium",
  "high",
  "high",
];

/** 档位 → reasoning effort（越界/非整数 → `undefined`，不覆盖 CLI 默认）。 */
export function reasoningForThinkingDepth(depth: unknown): string | undefined {
  if (typeof depth !== "number" || !Number.isInteger(depth)) return undefined;
  if (depth < 0 || depth >= CODEX_REASONING_BY_DEPTH.length) return undefined;
  return CODEX_REASONING_BY_DEPTH[depth];
}

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
  /** M3-10/ADR-010：会话级思考深度（0–4；override 优先，仅本次 run）。 */
  thinkingDepth?: number;
  /**
   * M7-06/ADR-024 A-4：档位投影后的 Codex 沙箱模式（仅新会话生效；
   * `session.create` 时一次判定）。
   */
  sandbox?: string;
  /**
   * M7-09/ADR-026：`session.create.system_prompt`（预设块 + 工作区约定块合成结果）。
   * 每次 run 以 `-c developer_instructions=…` 下发；超长时回退为提示词前缀块。
   */
  systemPrompt?: string;
  /** ADR-028 v0.9：会话级供应商 id（broker 按 run 取凭据）。 */
  providerId?: string;
  /** ADR-028 v0.9：per-session `CODEX_HOME`（首次补选时创建并复制/采用原生状态）。 */
  home?: string;
  clientMsgIds: Map<string, string>;
  runs: Map<string, CodexRunState>;
  observedTools: Set<string>;
  disposed: boolean;
}

/** M3-10：解析思考深度参数（整数 0–4；未知形态忽略，保持适配器宽容）。 */
function parseThinkingDepth(value: unknown): number | undefined {
  if (typeof value !== "number" || !Number.isInteger(value)) return undefined;
  if (value < 0 || value > 4) return undefined;
  return value;
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
  /** ADR-028 v0.9：凭据客户端（缺省 = 从 `AETHER_CREDENTIAL_BROKER` 构造）。 */
  credentialClient?: Pick<CredentialClient, "get"> | null;
  /** ADR-028 v0.9：per-session CODEX_HOME 根目录（缺省 = legacy home 的同级目录）。 */
  sessionHomesDir?: string;
}

export class CodexAdapter {
  readonly adapter: Adapter;
  readonly runtime: Required<AdapterRuntimeInfo>;
  readonly runTimeoutMs: number;

  private readonly options: CodexAdapterOptions;
  private readonly sessions = new Map<string, CodexSession>();
  private readonly store: CodexSessionStore;
  private readonly credentialClient: Pick<CredentialClient, "get"> | null;
  private readonly sessionLocks = new Map<string, Promise<void>>();
  private storeWarning: string | null = null;
  private readonly startedAt = Date.now();

  constructor(options: CodexAdapterOptions) {
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
      .handle("initialize", () => {
        // M7-06/ADR-024 §2.1-1：供应商消费映射物化——核心注入
        // `AETHER_PROVIDER_BASE_URL`（+ `AETHER_PROVIDER_ID`/`AETHER_PROVIDER_ENV_KEY`）
        // 时，向隔离 `CODEX_HOME` 写 `config.toml`（只含 base_url/env 键名，**不含密钥值**）。
        // 物化失败 → 拒绝 initialize（不静默按官方默认运行）；未注入 → 官方默认路径。
        this.materializeProviderConfigFromEnv();
        return {
          capabilities: this.runtime.capabilities,
          protocol: "1.0",
          acknowledged: true,
        };
      })
      .handle("session.create", (params) => this.onCreateSession(params))
      .handle("session.send", (params) => this.onSend(params))
      .handle("session.interrupt", (params) => this.onInterrupt(params))
      .handle("session.pause", (params) => this.onPause(params))
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
    if (requested !== undefined && !isAliasId(requested)) {
      throw new RpcError(
        ERROR_CODES.INVALID_PARAMS,
        "native_id 必须是 26 位 ULID 别名（Codex thread_id 由服务端生成，见 ADR-008 §3.3）",
      );
    }
    const model = typeof input.model === "string" && input.model.length > 0 ? input.model : undefined;
    const thinkingDepth = parseThinkingDepth(input.thinking_depth);
    // M7-06/ADR-024 A-4：档位 → `sandbox_mode` 投影（仅新会话生效；未知档位忽略）。
    const sandbox = codexSandbox(input.permission_mode);
    // M7-09/ADR-026：合成系统提示词（预设块 + 工作区约定块）随会话保存；每次 run
    // 以 `-c developer_instructions=…` 下发（超长回退见 onSend）。
    const systemPrompt =
      typeof input.system_prompt === "string" && input.system_prompt.length > 0
        ? input.system_prompt
        : undefined;
    const alias = isAliasId(requested) ? requested : ulid();
    const threadId = isAliasId(requested) ? this.store.lookup(requested) : null;
    const providerId =
      typeof input.provider_id === "string" && input.provider_id.length > 0
        ? input.provider_id
        : undefined;
    const existing = this.sessions.get(alias);
    if (
      providerId !== undefined &&
      existing !== undefined &&
      [...existing.runs.values()].some((run) => !run.finished)
    ) {
      throw new RpcError(
        ERROR_CODES.SESSION_NOT_FOUND,
        "会话有在途 run：禁止切换供应商/复制状态（session_busy）",
      );
    }
    let home: string | undefined;
    if (providerId !== undefined) {
      // ADR-028 v0.9：per-session CODEX_HOME；首次补选复制/采用 legacy 原生状态，
      // 仅空闲/独占锁；失败清理新目录、本次 send 失败（核心不持久化）。
      home = await this.withSessionLock(alias, async () => {
        const target = existing?.home ?? this.sessionHome(alias);
        // 新会话：仅创建空 per-session home；旧会话首次补选：复制/采用 legacy 原生状态。
        this.prepareSessionHome(
          target,
          isAliasId(requested) && existing?.home === undefined,
        );
        const credential = await this.requireCredential(providerId);
        if (credential.base_url === undefined) {
          throw new RpcError(
            ERROR_CODES.INTERNAL_ERROR,
            `供应商 ${providerId} 缺少 openai 协议端点（无法物化 config.toml）`,
          );
        }
        materializeCodexProviderConfig(
          target,
          providerId,
          credential.base_url,
          credential.auth_env,
        );
        return target;
      });
    }
    const session: CodexSession = {
      id: alias,
      alias,
      threadId,
      resumed: isAliasId(requested) && threadId !== null,
      ...(model !== undefined ? { model } : {}),
      ...(thinkingDepth !== undefined ? { thinkingDepth } : {}),
      ...(sandbox !== undefined ? { sandbox } : {}),
      ...(systemPrompt !== undefined ? { systemPrompt } : {}),
      ...(providerId !== undefined ? { providerId } : {}),
      ...(home !== undefined ? { home } : {}),
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
      // M7-09：回显是否携带系统提示词（不含正文）。
      has_system_prompt: systemPrompt !== undefined,
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
    // M3-10/ADR-010：run 启动时判定——`session.send` 覆盖优先（仅本次 run），
    // 否则会话级值；映射为 `-c model_reasoning_effort=<level>`（未声明档位则沿用
    // 适配器 CLI 配置的默认强度）。
    const thinkingDepth = parseThinkingDepth(input.thinking_depth) ?? session.thinkingDepth;
    const reasoning = reasoningForThinkingDepth(thinkingDepth);
    // M7-06/ADR-024 A-3：统一映射 = 消息前缀引用块（核心不读内容；引用 ≠ 预授权）。
    let prompt = composePromptWithReferences(input.text, parseReferences(input.references));
    // M7-09/ADR-026：合成系统提示词经 `-c developer_instructions=…` 下发；
    // 编码后超出平台命令行长度上限时回退为显式前缀块（内容完整、可见、可审计）。
    let developerInstructions: string | undefined;
    if (session.systemPrompt !== undefined) {
      const encodedLength = JSON.stringify(session.systemPrompt).length;
      if (encodedLength + 40 <= developerInstructionsLimit()) {
        developerInstructions = session.systemPrompt;
      } else {
        prompt = `${SYSTEM_PROMPT_FALLBACK_HEADER}\n${session.systemPrompt}\n${SYSTEM_PROMPT_FALLBACK_FOOTER}\n\n${prompt}`;
        this.options.stderr?.(
          `[codex] 系统提示词超出命令行长度上限（编码后 ${encodedLength} 字符 > ${developerInstructionsLimit()}）：` +
            `已回退为提示词前缀块 ${SYSTEM_PROMPT_FALLBACK_HEADER}（内容完整，仅通道变更）`,
        );
      }
    }
    // ADR-028 v0.9：每次 run 按 `provider_id + protocol` 取凭据（不缓存）；仅写入
    // 本次 CLI 子进程 env。Codex 的 per-session CODEX_HOME 与 config.toml 已在
    // `session.create` 准备（不含密钥值），此处只刷新端点/auth env。
    let providerEnv: NodeJS.ProcessEnv = {};
    let sessionHome: string | undefined = session.home;
    if (session.providerId !== undefined) {
      const credential = await this.requireCredential(session.providerId);
      if (credential.base_url === undefined) {
        throw new RpcError(
          ERROR_CODES.INTERNAL_ERROR,
          `供应商 ${session.providerId} 缺少 openai 协议端点`,
        );
      }
      sessionHome = session.home ?? this.sessionHome(session.id);
      if (session.home === undefined) {
        this.prepareSessionHome(sessionHome, true);
        session.home = sessionHome;
      }
      materializeCodexProviderConfig(
        sessionHome,
        session.providerId,
        credential.base_url,
        credential.auth_env,
      );
      providerEnv = {
        CODEX_HOME: sessionHome,
        [credential.auth_env]: credential.api_key,
      };
    }
    run.cli = new CodexCliRun({
      config: {
        ...this.options.cli,
        ...(session.model ? { model: session.model } : {}),
        ...(reasoning !== undefined ? { reasoning } : {}),
        // M7-06/ADR-024 A-4：会话创建的档位投影覆盖 CLI 缺省沙箱（仅新会话生效）。
        ...(session.sandbox !== undefined ? { sandbox: session.sandbox } : {}),
      },
      spec: {
        threadId: session.threadId,
        prompt,
        timeoutMs: this.runTimeoutMs,
        ...(developerInstructions !== undefined ? { developerInstructions } : {}),
      },
      env: this.cliEnv(providerEnv, sessionHome),
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
    this.sessionLocks.delete(sessionId);
    // ADR-028 v0.9：dispose 清理 per-session CODEX_HOME；legacy home 不动。
    if (session.home !== undefined) {
      rmSync(session.home, { recursive: true, force: true });
    }
    return { disposed: true };
  }

  /**
   * `session.pause`（M7-03/ADR-017）：核心侧受控中断收口后调用——取消残余工作
   * （在途 run 进程树回收）、释放会话运行时侧资源；会话与别名映射保留
   * （`native_id` 恢复能力不变，Mode R 重放依赖 `codex exec resume`）。
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

  private cliEnv(extra: NodeJS.ProcessEnv = {}, home?: string): NodeJS.ProcessEnv {
    const env = stripBrokerEnv(process.env);
    // ADR-028 v0.9：provider 会话不使用 legacy 消费映射 env（避免串 provider）。
    delete env.AETHER_PROVIDER_ID;
    delete env.AETHER_PROVIDER_BASE_URL;
    delete env.AETHER_PROVIDER_ENV_KEY;
    delete env.AETHER_PROVIDER_MODELS;
    delete env.AETHER_PROVIDER_MODEL;
    const targetHome = home ?? this.options.cli.home ?? process.env.CODEX_HOME;
    if (targetHome) {
      env.CODEX_HOME = targetHome;
    }
    return { ...env, ...extra, CODEX_HOME: extra.CODEX_HOME ?? env.CODEX_HOME };
  }

  private async requireCredential(providerId: string): Promise<BrokerCredential> {
    if (this.credentialClient === null) {
      throw new RpcError(
        ERROR_CODES.INTERNAL_ERROR,
        "凭据 broker 未接线：多供应商会话不可用（不回退单供应商 env）",
      );
    }
    try {
      return await this.credentialClient.get(providerId, "openai", "codex");
    } catch (error) {
      throw new RpcError(
        ERROR_CODES.INTERNAL_ERROR,
        `获取供应商凭据失败：${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }

  private legacyHome(): string {
    return this.options.cli.home ?? process.env.CODEX_HOME ?? join(homedir(), ".codex");
  }

  private sessionHome(alias: string): string {
    const root =
      this.options.sessionHomesDir ??
      join(dirname(this.legacyHome()), "aether-codex-sessions");
    return join(root, alias);
  }

  /** 首次补选：复制/采用 legacy home 的 thread/session 状态（不复制 config.toml）。 */
  private prepareSessionHome(target: string, copyState: boolean): void {
    const legacy = this.legacyHome();
    if (copyState && !existsSync(target)) {
      mkdirSync(target, { recursive: true });
      if (existsSync(legacy)) {
        try {
          cpSync(legacy, target, {
            recursive: true,
            force: false,
            errorOnExist: false,
            filter: (source) =>
              !source.endsWith("config.toml") && !source.includes("aether-codex-sessions"),
          });
        } catch (error) {
          rmSync(target, { recursive: true, force: true });
          throw new RpcError(
            ERROR_CODES.INTERNAL_ERROR,
            `复制 Codex 原生 thread 状态失败（已清理新目录）：${
              error instanceof Error ? error.message : String(error)
            }`,
          );
        }
      }
      return;
    }
    mkdirSync(target, { recursive: true });
  }

  /** 会话级串行锁（状态复制/配置物化不得并发写；ADR-028 §2.1-5）。 */
  private async withSessionLock<T>(alias: string, work: () => Promise<T>): Promise<T> {
    const previous = this.sessionLocks.get(alias) ?? Promise.resolve();
    const run = previous.then(work, work);
    this.sessionLocks.set(
      alias,
      run.then(
        () => undefined,
        () => undefined,
      ),
    );
    return run;
  }

  /**
   * M7-06/ADR-024 §2.1-1：从核心注入的消费映射 env 物化隔离 `CODEX_HOME/config.toml`。
   *
   * 触发条件：`AETHER_PROVIDER_BASE_URL` 存在（核心只在匹配 provider 时注入）。
   * 目标 home：`--codex-home`（cli.home）> `CODEX_HOME`（核心注入的隔离目录）；
   * 二者皆无 → 拒绝（消费映射必须有隔离 home，避免污染用户级 `~/.codex`）。
   * 失败抛 `RpcError`（start_failed；不静默降级到官方默认）。
   */
  private materializeProviderConfigFromEnv(): void {
    const env = process.env;
    const baseUrl = env.AETHER_PROVIDER_BASE_URL;
    if (baseUrl === undefined || baseUrl.length === 0) {
      return;
    }
    const providerId = env.AETHER_PROVIDER_ID ?? "aether";
    const envKey = env.AETHER_PROVIDER_ENV_KEY ?? "OPENAI_API_KEY";
    const home = this.options.cli.home ?? env.CODEX_HOME;
    if (home === undefined || home.length === 0) {
      throw new RpcError(
        ERROR_CODES.INTERNAL_ERROR,
        "供应商消费映射需要隔离 CODEX_HOME（核心未注入且未配置 --codex-home）",
      );
    }
    try {
      const path = materializeCodexProviderConfig(home, providerId, baseUrl, envKey);
      this.options.stderr?.(`[codex] 供应商消费映射已物化：${path}（不含密钥值）`);
    } catch (error) {
      throw new RpcError(
        ERROR_CODES.INTERNAL_ERROR,
        `供应商消费映射物化失败：${error instanceof Error ? error.message : String(error)}`,
      );
    }
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
