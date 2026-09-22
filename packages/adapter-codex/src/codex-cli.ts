/**
 * Codex CLI 驱动（ADR-008 / M2-11 Codex 路径）：参数构造、进程启动、stdout JSONL 行解析、进程树中断。
 *
 * 依据 `docs/spike/M1-11-接入笔记.md`：
 * - 首轮 `codex exec --json ... -`（prompt 走 stdin），会话 id 来自 `thread.started.thread_id`；
 * - 续聊/恢复 `codex exec resume <thread_id> -`（resume **不接受** `--sandbox`，改 `-c sandbox_mode=<mode>`，已知坑 7）；
 * - 必须隔离 `CODEX_HOME`（已知坑 9：用户级 MCP 配置污染输入量级）；
 * - Windows `codex` 为 .cmd 垫片：经 `cmd.exe /d /s /c` 兜底（已知坑 7）。
 */

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";

/** 适配器 CLI 配置（`--codex-bin` 等，见 `cli.ts`）。 */
export interface CodexCliConfig {
  /** 可执行文件（默认 `codex`；测试指向 node）。 */
  bin: string;
  /** 追加在子命令之前的额外参数（测试注入脚本路径等）。 */
  extraArgs: string[];
  /** 隔离 CODEX_HOME（spike 已知坑 9；缺省时使用用户级配置）。 */
  home?: string;
  /** 工作目录（CLI `--cd`）。 */
  workspace: string;
  /** 模型（`-m`；spike 已知坑 6：必须显式指定）。 */
  model?: string;
  /** 沙箱模式（`read-only` / `workspace-write` / `danger-full-access`）。 */
  sandbox: string;
  /** 推理强度（`-c model_reasoning_effort=<值>`，默认 low）。 */
  reasoning?: string;
  /** 附加 `-c` 覆写（可重复）。 */
  configOverrides?: string[];
  /** 跳过 git 仓库检查（默认 true；Aether 工作区不要求 git）。 */
  skipGitRepoCheck?: boolean;
}

/** 单次 run 的 CLI 调用规格。 */
export interface CodexRunSpec {
  /** 原生 thread id（`None` = 新会话 `exec`；有值 = `exec resume`）。 */
  threadId?: string | null;
  prompt: string;
  /** 适配器侧硬超时（兜底；核心 120s 断流看门狗优先）。 */
  timeoutMs: number;
}

/** 依据 M1-11 固定形状构造 CLI 参数。 */
export function buildCodexArgs(config: CodexCliConfig, spec: CodexRunSpec): string[] {
  const common: string[] = ["--json"];
  if (config.skipGitRepoCheck !== false) {
    common.push("--skip-git-repo-check");
  }
  if (config.model) {
    common.push("-m", config.model);
  }
  if (config.reasoning) {
    common.push("-c", `model_reasoning_effort=${config.reasoning}`);
  }
  for (const override of config.configOverrides ?? []) {
    common.push("-c", override);
  }
  const args: string[] = [...config.extraArgs];
  if (spec.threadId) {
    // `resume` 子命令：不接受 --sandbox / --cd（已知坑 7），沙箱经 -c 覆写。
    args.push(
      "exec",
      "resume",
      ...common,
      "-c",
      `sandbox_mode=${config.sandbox}`,
      spec.threadId,
      "-",
    );
  } else {
    args.push("exec", ...common, "--sandbox", config.sandbox, "--cd", config.workspace, "-");
  }
  return args;
}

/**
 * Windows shim 归一化：`codex`（npm .cmd 垫片）经 `cmd.exe /d /s /c` 执行。
 */
export function normalizeCommand(
  bin: string,
  args: string[],
  platform: string = process.platform,
  comspec: string | undefined = process.env.ComSpec,
): { command: string; args: string[] } {
  if (platform === "win32" && (bin === "codex" || /\.(cmd|bat)$/i.test(bin))) {
    return { command: comspec ?? "cmd.exe", args: ["/d", "/s", "/c", bin, ...args] };
  }
  return { command: bin, args };
}

/** 子进程 spawn 抽象（测试可注入；默认 `node:child_process.spawn`）。 */
export type SpawnFn = (
  command: string,
  args: string[],
  options: { cwd: string; detached: boolean; windowsHide: boolean; env?: NodeJS.ProcessEnv },
) => ChildProcessWithoutNullStreams;

/** 进程树中断抽象（默认见 `interruptProcessTree`）。 */
export type KillTreeFn = (pid: number) => Promise<void>;

/** 默认 spawn：Unix 使用独立进程组（`detached`），保证整树回收（D5）。 */
export const defaultSpawn: SpawnFn = (command, args, options) =>
  spawn(command, args, {
    cwd: options.cwd,
    detached: options.detached,
    windowsHide: options.windowsHide,
    env: options.env,
    stdio: ["pipe", "pipe", "pipe"],
  });

/**
 * 进程树中断（D5：裸 kill 单 PID 禁止）。
 *
 * - Windows：`taskkill /PID <pid> /T /F`（整树回收）；
 * - POSIX：进程组 `SIGTERM` →（2s 宽限）`SIGKILL`（依赖 detached 进程组）。
 */
export async function interruptProcessTree(
  pid: number,
  platform: string = process.platform,
): Promise<void> {
  if (platform === "win32") {
    await new Promise<void>((resolve) => {
      const killer = spawn("taskkill", ["/PID", String(pid), "/T", "/F"], {
        windowsHide: true,
        stdio: "ignore",
      });
      killer.once("error", () => resolve());
      killer.once("close", () => resolve());
    });
    return;
  }
  try {
    process.kill(-pid, "SIGTERM");
  } catch {
    try {
      process.kill(pid, "SIGTERM");
    } catch {
      return;
    }
  }
  await new Promise((resolve) => setTimeout(resolve, 2000));
  try {
    process.kill(-pid, "SIGKILL");
  } catch {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      /* 已退出 */
    }
  }
}

/** 进程退出结果。 */
export interface CodexExit {
  code: number | null;
  signal: NodeJS.Signals | null;
}

export interface CodexCliRunOptions {
  config: CodexCliConfig;
  spec: CodexRunSpec;
  /** 每解析出一条 JSON 事件回调。 */
  onEvent: (event: unknown) => void;
  /** 无法解析的 stdout 行（诊断计数；不崩溃）。 */
  onMalformedLine: (line: string) => void;
  /** stderr 行（诊断；绝不进 stdout）。 */
  onStderrLine: (line: string) => void;
  /** 隔离环境（`CODEX_HOME`；由适配器注入）。 */
  env?: NodeJS.ProcessEnv;
  spawnFn?: SpawnFn;
  killTreeFn?: KillTreeFn;
}

/**
 * 单次 run 的 Codex CLI 进程封装：spawn → 行解析 → 超时回收 → 等待退出。
 */
export class CodexCliRun {
  private child: ChildProcessWithoutNullStreams | null = null;
  private exited = false;
  private exit: CodexExit | null = null;
  private spawnError: string | null = null;
  private interrupted = false;
  private timedOut = false;
  private exitWaiters: Array<() => void> = [];
  private timeoutTimer: NodeJS.Timeout | null = null;
  private stdoutBuffer = "";

  private readonly spawnFn: SpawnFn;
  private readonly killTreeFn: KillTreeFn;

  constructor(private readonly options: CodexCliRunOptions) {
    this.spawnFn = options.spawnFn ?? defaultSpawn;
    this.killTreeFn = options.killTreeFn ?? interruptProcessTree;
  }

  get pid(): number | undefined {
    return this.child?.pid;
  }

  get wasInterrupted(): boolean {
    return this.interrupted;
  }

  get wasTimedOut(): boolean {
    return this.timedOut;
  }

  /** 启动进程；spawn 失败不抛出（记录 `spawnError`，由 `waitExit` 呈现）。 */
  start(): void {
    const { config, spec } = this.options;
    const built = buildCodexArgs(config, spec);
    const normalized = normalizeCommand(config.bin, built);
    let child: ChildProcessWithoutNullStreams;
    try {
      child = this.spawnFn(normalized.command, normalized.args, {
        cwd: config.workspace,
        detached: process.platform !== "win32",
        windowsHide: true,
        ...(this.options.env ? { env: this.options.env } : {}),
      });
    } catch (error) {
      this.spawnError = error instanceof Error ? error.message : String(error);
      this.markExited();
      return;
    }
    this.child = child;
    child.once("error", (error) => {
      this.spawnError = error.message;
      this.markExited();
    });
    child.once("close", (code, signal) => {
      this.flushStdout();
      this.exit = { code, signal };
      this.markExited();
    });
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => this.onStdoutChunk(chunk));
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk: string) => this.onStderrChunk(chunk));

    this.timeoutTimer = setTimeout(() => {
      this.timedOut = true;
      void this.killTree();
    }, Math.max(1, spec.timeoutMs));
    if (typeof this.timeoutTimer.unref === "function") this.timeoutTimer.unref();

    child.stdin.write(`${spec.prompt}\n`);
    child.stdin.end();
  }

  /** 中断：标记 + 进程树回收（幂等）。 */
  async interrupt(): Promise<void> {
    if (this.exited) return;
    this.interrupted = true;
    await this.killTree();
  }

  /** 适配器侧超时触发。 */
  async timeout(): Promise<void> {
    if (this.exited) return;
    this.timedOut = true;
    await this.killTree();
  }

  /** 等待进程退出（含 spawn 失败）；超时返回 false。 */
  async waitExit(timeoutMs: number): Promise<boolean> {
    if (this.exited) return true;
    return new Promise<boolean>((resolve) => {
      const timer = setTimeout(() => resolve(false), Math.max(1, timeoutMs));
      this.exitWaiters.push(() => {
        clearTimeout(timer);
        resolve(true);
      });
    });
  }

  get exitInfo(): CodexExit | null {
    return this.exit;
  }

  get spawnFailure(): string | null {
    return this.spawnError;
  }

  private async killTree(): Promise<void> {
    const pid = this.pid;
    if (pid === undefined) return;
    await this.killTreeFn(pid);
  }

  private markExited(): void {
    if (this.exited) return;
    this.exited = true;
    if (this.timeoutTimer) {
      clearTimeout(this.timeoutTimer);
      this.timeoutTimer = null;
    }
    for (const waiter of this.exitWaiters.splice(0)) waiter();
  }

  private onStdoutChunk(chunk: string): void {
    this.stdoutBuffer += chunk;
    let index = this.stdoutBuffer.indexOf("\n");
    while (index >= 0) {
      const line = this.stdoutBuffer.slice(0, index).replace(/\r$/, "");
      this.stdoutBuffer = this.stdoutBuffer.slice(index + 1);
      if (line.trim().length > 0) this.handleLine(line);
      index = this.stdoutBuffer.indexOf("\n");
    }
  }

  private flushStdout(): void {
    const line = this.stdoutBuffer.replace(/\r$/, "");
    this.stdoutBuffer = "";
    if (line.trim().length > 0) this.handleLine(line);
  }

  private handleLine(line: string): void {
    try {
      this.options.onEvent(JSON.parse(line));
    } catch {
      this.options.onMalformedLine(line);
    }
  }

  private onStderrChunk(chunk: string): void {
    for (const line of chunk.split(/\r?\n/)) {
      if (line.length > 0) this.options.onStderrLine(line);
    }
  }
}
