/**
 * 启动编排（M9-07 DoD-4；ADR-029 §3.5 / §4.2 / §4.3）。
 *
 * 顺序（窗口由调用方先创建，本模块只负责 daemon）：
 *   spawn `aetherd --stdio` → `initialize`（目标 ≤2s / 硬超时 ≥10s）
 *   → 超时断开并重试一次 → 仍失败 S-12；成功则加载主 UI（不等预热）。
 *
 * 口径：
 * - **先窗口后 daemon**：`windowCreatedAt` 必须早于 spawn（调用方断言）；
 * - 预 `initialize` 致命：首帧 `id:null` error 的 `business_code` 映射 S-12 原因
 *   （`sync_dir_blocked` → `a4_blocked`），daemon 退出码由 daemon 自己给出；
 * - `quick_check` 失败（`phase=degraded` / `daemon.status`）**不是**致命：
 *   主 UI 继续加载，S-08 由既有 `health.storage_state=persist_degraded` 呈现；
 * - M9 **不产生** S-01/S-01a：A4 与 DB 无法打开统一走 S-12，无迁移入口。
 */
import {
  AetherRpcError,
  AetherRpcTimeoutError,
  INITIALIZE_HARD_TIMEOUT_MS,
  type DaemonExit,
  type InitializeParams,
  type InitializeResult,
} from "./daemonClient";

/** S-12 `data-reason`（UI-UX v0.17 冻结集合）。 */
export type ShellFatalReason =
  | "daemon_start_failed"
  | "handshake_failed"
  | "a4_blocked"
  | "db_open_failed"
  | "schema_checksum_mismatch"
  | "data_dir_locked";

/** S-12 状态（main → 独立最小 renderer）。 */
export interface ShellFatalState {
  reason: ShellFatalReason;
  message: string;
  logPath: string;
  dataDir: string;
  /** daemon 首帧 `phase`（若有）。 */
  phase?: string;
  /** stderr 尾部（已脱敏；排障用）。 */
  stderrTail?: string;
  /** 重试次数（0 = 首次失败）。 */
  attempts: number;
}

/** 启动阶段（日志/证据）。 */
export interface StartupAttemptTrace {
  attempt: number;
  spawnedAt: number;
  initializeStartedAt: number;
  initializeFinishedAt?: number;
  elapsedMs?: number;
  daemonPid?: number | null;
  outcome: "ready" | "fatal" | "timeout" | "error";
  error?: string;
}

export interface StartupSuccess {
  ok: true;
  /** 成功握手的客户端（main 用于后续请求/事件）。 */
  client: StartupClient;
  result: InitializeResult;
  attempts: StartupAttemptTrace[];
  /** 是否发生超时重试。 */
  retried: boolean;
  /** `initialize` 耗时（最后一次成功尝试）。 */
  initializeMs: number;
  /** 握手目标 2s 是否达成（记录超时率；R-P1-2）。 */
  withinTarget: boolean;
}

export interface StartupFailure {
  ok: false;
  /** 最后一次尝试的客户端（main 负责关闭）。 */
  client: StartupClient | null;
  fatal: ShellFatalState;
  attempts: StartupAttemptTrace[];
  retried: boolean;
  /** 失败前的最后一次退出信息（若有）。 */
  exit?: DaemonExit | null;
}

export type StartupOutcome = StartupSuccess | StartupFailure;

/** 启动客户端的最小接口（生产 = DaemonClient；测试注入）。 */
export interface StartupClient {
  start(): void;
  initialize(params: InitializeParams, timeoutMs: number): Promise<InitializeResult>;
  kill(signal?: NodeJS.Signals): void;
  waitForExit(timeoutMs: number): Promise<DaemonExit | null>;
  readonly pid: number | null;
  readonly stderrTail: string;
  readonly bootstrapFailure: AetherRpcError | null;
  readonly exitInfo: DaemonExit | null;
}

export interface StartupOptions {
  /** 窗口创建时间（由调用方在创建 BrowserWindow 前记录）。 */
  windowCreatedAt: number;
  /** 每次尝试创建客户端（生产：新 spawn）。 */
  createClient: (attempt: number) => StartupClient;
  /** 握手参数。 */
  initializeParams: InitializeParams;
  /** S-12 展示用的日志路径与数据目录（启动前已解析）。 */
  context: { logPath: string; dataDir: string };
  /** 硬超时（默认 10s；不得小于 10s，ADR-029 §3.5）。 */
  hardTimeoutMs?: number;
  /** 握手目标（默认 2s；仅记录是否达成）。 */
  targetMs?: number;
  /** 两次尝试之间的等待（默认 0；测试可注入）。 */
  retryDelayMs?: number;
  now?: () => number;
  /** 每次尝试开始（写日志/证据）。 */
  onAttemptStart?: (attempt: number, client: StartupClient) => void;
  /** 尝试结束（日志/证据）。 */
  onAttemptEnd?: (trace: StartupAttemptTrace) => void;
  /** 成功（加载主 UI 前）。 */
  onReady?: (success: StartupSuccess) => void;
  /** 失败（加载 S-12 前）。 */
  onFatal?: (failure: StartupFailure) => void;
}

/** 最小硬超时（ADR-029 §3.5：硬超时 ≥10s）。 */
export const MIN_INITIALIZE_HARD_TIMEOUT_MS = INITIALIZE_HARD_TIMEOUT_MS;

const BUSINESS_REASON: Readonly<Record<string, ShellFatalReason>> = {
  sync_dir_blocked: "a4_blocked",
  data_dir_locked: "data_dir_locked",
  db_open_failed: "db_open_failed",
  schema_checksum_mismatch: "schema_checksum_mismatch",
};

/** 业务码 → S-12 原因（未登记业务码返回 `undefined`）。 */
export function fatalReasonForBusinessCode(
  businessCode: string | undefined,
): ShellFatalReason | undefined {
  if (businessCode === undefined) {
    return undefined;
  }
  return BUSINESS_REASON[businessCode];
}

/** `initialize` 失败 → S-12 原因（阶段区分 spawn/握手）。 */
export function fatalReasonForError(
  error: unknown,
  stage: "spawn" | "initialize",
): ShellFatalReason {
  if (error instanceof AetherRpcError) {
    const mapped = fatalReasonForBusinessCode(error.businessCode);
    if (mapped !== undefined) {
      return mapped;
    }
    if (error.aetherCode === "startup_blocked") {
      return "a4_blocked";
    }
  }
  return stage === "spawn" ? "daemon_start_failed" : "handshake_failed";
}

/** 从 `initialize` 错误构造 S-12 状态。 */
export function fatalStateForError(
  error: unknown,
  stage: "spawn" | "initialize",
  context: { logPath: string; dataDir: string; attempts: number; stderrTail?: string },
): ShellFatalState {
  const reason = fatalReasonForError(error, stage);
  const rpc = error instanceof AetherRpcError ? error : null;
  const message =
    rpc?.message ??
    (error instanceof Error ? error.message : "核心未能启动（未知错误）");
  const phase = typeof rpc?.data?.phase === "string" ? rpc.data.phase : undefined;
  return {
    reason,
    message,
    logPath: context.logPath,
    dataDir: context.dataDir,
    ...(phase ? { phase } : {}),
    ...(context.stderrTail ? { stderrTail: context.stderrTail } : {}),
    attempts: context.attempts,
  };
}

/** 运行启动序列（最多两次尝试：首次 + 超时重试一次）。 */
export async function runStartupSequence(
  options: StartupOptions,
): Promise<StartupOutcome> {
  const now = options.now ?? Date.now;
  const hardTimeoutMs = Math.max(
    MIN_INITIALIZE_HARD_TIMEOUT_MS,
    options.hardTimeoutMs ?? MIN_INITIALIZE_HARD_TIMEOUT_MS,
  );
  const targetMs = options.targetMs ?? 2_000;
  const retryDelayMs = options.retryDelayMs ?? 0;
  const attempts: StartupAttemptTrace[] = [];
  let retried = false;
  const fail = (
    client: StartupClient | null,
    fatal: ShellFatalState,
    exit: DaemonExit | null,
  ): StartupFailure => {
    const failure: StartupFailure = {
      ok: false,
      client,
      fatal,
      attempts,
      retried,
      exit,
    };
    options.onFatal?.(failure);
    return failure;
  };

  for (let attempt = 1; attempt <= 2; attempt += 1) {
    const client = options.createClient(attempt);
    const spawnAt = now();
    if (spawnAt < options.windowCreatedAt) {
      throw new Error("启动顺序错误：窗口必须先于 daemon 创建");
    }
    options.onAttemptStart?.(attempt, client);
    try {
      client.start();
    } catch (error) {
      const trace: StartupAttemptTrace = {
        attempt,
        spawnedAt: spawnAt,
        initializeStartedAt: now(),
        outcome: "error",
        error: errorText(error),
      };
      attempts.push(trace);
      options.onAttemptEnd?.(trace);
      return fail(
        client,
        fatalStateForError(error, "spawn", {
          logPath: options.context.logPath,
          dataDir: options.context.dataDir,
          attempts: attempt,
        }),
        null,
      );
    }

    const initializeStartedAt = now();
    try {
      const result = await client.initialize(options.initializeParams, hardTimeoutMs);
      const finishedAt = now();
      const elapsedMs = finishedAt - initializeStartedAt;
      const trace: StartupAttemptTrace = {
        attempt,
        spawnedAt: spawnAt,
        initializeStartedAt,
        initializeFinishedAt: finishedAt,
        elapsedMs,
        daemonPid: client.pid,
        outcome: "ready",
      };
      attempts.push(trace);
      options.onAttemptEnd?.(trace);
      const success: StartupSuccess = {
        ok: true,
        client,
        result,
        attempts,
        retried,
        initializeMs: elapsedMs,
        withinTarget: elapsedMs <= targetMs,
      };
      options.onReady?.(success);
      return success;
    } catch (error) {
      const finishedAt = now();
      const elapsedMs = finishedAt - initializeStartedAt;
      const timeout =
        error instanceof AetherRpcTimeoutError ||
        (error instanceof AetherRpcError && /超时/.test(error.message));
      const trace: StartupAttemptTrace = {
        attempt,
        spawnedAt: spawnAt,
        initializeStartedAt,
        initializeFinishedAt: finishedAt,
        elapsedMs,
        daemonPid: client.pid,
        outcome: timeout ? "timeout" : "fatal",
        error: errorText(error),
      };
      attempts.push(trace);
      options.onAttemptEnd?.(trace);

      if (timeout && attempt === 1) {
        retried = true;
        client.kill("SIGTERM");
        await client.waitForExit(2_000);
        if (retryDelayMs > 0) {
          await delay(retryDelayMs);
        }
        continue;
      }

      const stage =
        client.exitInfo !== null && !timeout ? "spawn" : "initialize";
      return fail(
        client,
        fatalStateForError(error, stage, {
          logPath: options.context.logPath,
          dataDir: options.context.dataDir,
          attempts: attempt,
          stderrTail: client.stderrTail,
        }),
        client.exitInfo,
      );
    }
  }

  // 循环最多 2 次；不可达（保持类型完整）。
  return fail(
    null,
    {
      reason: "handshake_failed",
      message: "核心握手失败（重试后仍不可用）",
      logPath: options.context.logPath,
      dataDir: options.context.dataDir,
      attempts: attempts.length,
    },
    null,
  );
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}
