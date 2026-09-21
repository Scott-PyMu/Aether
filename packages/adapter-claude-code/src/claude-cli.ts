/**
 * Claude Code CLI 驱动（M2-02）：参数构造、进程启动、stdout 行解析、进程树中断。
 *
 * 依据 `docs/spike/M1-11-接入笔记.md`：
 * - 首轮 `--session-id <uuid>` 显式建会话，后续/恢复用 `--resume <native_id>`（Mode R）；
 * - `-p --output-format stream-json --include-partial-messages --verbose` 逐 token 流式；
 * - 鉴权经隔离 settings（`--setting-sources local --settings <file>`）+ `--strict-mcp-config`
 *   注入（D10：密钥由核心从 Keychain 物化为临时 settings，不进仓库/日志）；
 * - Windows 下 `claude` 为 .cmd/.ps1 shim：经 `cmd.exe /d /s /c` 兜底（已知坑 7）。
 */

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";

/** 适配器 CLI 配置（`--claude-bin` 等，见 `cli.ts`）。 */
export interface ClaudeCliConfig {
  /** 可执行文件（默认 `claude`；测试指向 node/fake-cli）。 */
  bin: string;
  /** 追加在业务参数之前的额外参数（测试注入脚本路径等）。 */
  extraArgs: string[];
  /** 隔离 settings 文件（可选；核心从 keychain 物化）。 */
  settingsFile?: string;
  /** 工作目录（CLI cwd）。 */
  workspace: string;
  /** 会话级模型覆盖（`--model`）。 */
  model?: string;
  /** 工具策略：`none` → `--tools ""`；其它值原样透传；缺省不透传。 */
  tools?: string;
  /** 权限模式（透传 `--permission-mode`；缺省 `default`）。 */
  permissionMode?: string;
  /** `--max-turns`（可选；不设则用 CLI 默认）。 */
  maxTurns?: number;
}

/** 单次 run 的 CLI 调用规格。 */
export interface ClaudeRunSpec {
  /** 原生会话 id（Claude `--session-id`/`--resume` 的 UUID）。 */
  nativeId: string;
  /** true = `--resume`（Mode R 恢复/续聊）；false = `--session-id`（首轮创建）。 */
  resume: boolean;
  prompt: string;
  /** 适配器侧硬超时（兜底；核心 120s 断流看门狗优先）。 */
  timeoutMs: number;
}

/** 依据接入笔记固定参数形状构造 CLI 参数。 */
export function buildClaudeArgs(config: ClaudeCliConfig, spec: ClaudeRunSpec): string[] {
  // extraArgs 前置：测试以 `--claude-bin node --claude-arg <fake.mjs>` 注入替身时，
  // 脚本路径必须紧跟 node 可执行文件（node 选项解析顺序）；真实 CLI 的附加参数
  // 顺序不敏感，前置与后置等价。
  const args: string[] = [...config.extraArgs];
  if (config.settingsFile) {
    args.push("--setting-sources", "local", "--settings", config.settingsFile);
  }
  args.push("--strict-mcp-config");
  args.push("--permission-mode", config.permissionMode ?? "default");
  if (config.tools === "none") {
    args.push("--tools", "");
  } else if (config.tools !== undefined) {
    args.push("--tools", config.tools);
  }
  if (config.maxTurns !== undefined) {
    args.push("--max-turns", String(config.maxTurns));
  }
  if (config.model) {
    args.push("--model", config.model);
  }
  args.push(spec.resume ? "--resume" : "--session-id", spec.nativeId);
  args.push("-p", "--output-format", "stream-json", "--include-partial-messages", "--verbose");
  return args;
}

/**
 * Windows shim 归一化：`claude`（npm .cmd/.ps1 包装）经 `cmd.exe /d /s /c` 执行。
 *
 * 已知坑 7：直接 spawn `claude` 在 Windows 上会经过 cmd 二次解析；显式 `.exe` 路径
 * 或非 Windows 平台不受影响。测试固定传 `--claude-bin <node>`，不触发本分支。
 */
export function normalizeCommand(
  bin: string,
  args: string[],
  platform: string = process.platform,
  comspec: string | undefined = process.env.ComSpec,
): { command: string; args: string[] } {
  if (platform === "win32" && (bin === "claude" || /\.(cmd|bat)$/i.test(bin))) {
    return { command: comspec ?? "cmd.exe", args: ["/d", "/s", "/c", bin, ...args] };
  }
  return { command: bin, args };
}

/** 子进程 spawn 抽象（测试可注入；默认 `node:child_process.spawn`）。 */
export type SpawnFn = (
  command: string,
  args: string[],
  options: { cwd: string; detached: boolean; windowsHide: boolean },
) => ChildProcessWithoutNullStreams;

/** 进程树中断抽象（默认见 `interruptProcessTree`）。 */
export type KillTreeFn = (pid: number) => Promise<void>;

/** 默认 spawn：Unix 使用独立进程组（`detached`），保证整树回收（D5）。 */
export const defaultSpawn: SpawnFn = (command, args, options) =>
  spawn(command, args, {
    cwd: options.cwd,
    detached: options.detached,
    windowsHide: options.windowsHide,
    stdio: ["pipe", "pipe", "pipe"],
  });

/**
 * 进程树中断（D5：裸 kill 单 PID 禁止）。
 *
 * - Windows：`taskkill /PID <pid> /T /F`（整树回收）；
 * - POSIX：`process.kill(-pid, SIGTERM)` →（2s 宽限）`SIGKILL`（依赖 detached 进程组）。
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
export interface ClaudeExit {
  code: number | null;
  signal: NodeJS.Signals | null;
}

export interface ClaudeCliRunOptions {
  config: ClaudeCliConfig;
  spec: ClaudeRunSpec;
  /** 每解析出一条 JSON 事件回调（乱序/时序由调用方保证）。 */
  onEvent: (event: unknown) => void;
  /** 无法解析的 stdout 行（诊断计数；不崩溃）。 */
  onMalformedLine: (line: string) => void;
  /** stderr 行（诊断；绝不进 stdout）。 */
  onStderrLine: (line: string) => void;
  spawnFn?: SpawnFn;
  killTreeFn?: KillTreeFn;
}

/**
 * 单次 run 的 Claude CLI 进程封装：spawn → 行解析 → 超时回收 → 等待退出。
 *
 * 生命周期约定：`start()` 后必须 `waitExit()`（或外部 interrupt）收口。
 */
export class ClaudeCliRun {
  private child: ChildProcessWithoutNullStreams | null = null;
  private exited = false;
  private exit: ClaudeExit | null = null;
  private spawnError: string | null = null;
  private interrupted = false;
  private timedOut = false;
  private exitWaiters: Array<() => void> = [];
  private timeoutTimer: NodeJS.Timeout | null = null;
  private stdoutBuffer = "";

  private readonly spawnFn: SpawnFn;
  private readonly killTreeFn: KillTreeFn;

  constructor(private readonly options: ClaudeCliRunOptions) {
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
    const built = buildClaudeArgs(config, spec);
    const normalized = normalizeCommand(config.bin, built);
    let child: ChildProcessWithoutNullStreams;
    try {
      child = this.spawnFn(normalized.command, normalized.args, {
        cwd: config.workspace,
        detached: process.platform !== "win32",
        windowsHide: true,
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

  /** 适配器侧超时触发（外部诊断用；实际由内部定时器触发）。 */
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

  get exitInfo(): ClaudeExit | null {
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
