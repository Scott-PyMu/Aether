/**
 * DSH ACP 客户端（M2-11）：JSON-RPC 2.0 over stdio（`--profile acp`），
 * 负责 `initialize` / `session/*` 请求、`session/update` 通知与 `session/request_permission`
 * 服务端请求；进程生命周期与整树回收（D5：禁止裸 kill 单 PID）。
 */

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";

/** ACP `session/update` 参数（仅声明使用到的字段）。 */
export interface AcpUpdateParams {
  sessionId?: string;
  update?: {
    sessionUpdate?: string;
    content?: { type?: string; text?: string };
    toolCallId?: string;
    title?: string;
    kind?: string;
    status?: string;
    rawInput?: unknown;
    [key: string]: unknown;
  };
  [key: string]: unknown;
}

export interface AcpClientOptions {
  /** DSH 启动器（`@deepseek-ai/dsh/lib/bin.js`）。 */
  bin: string;
  /** Node 可执行文件（默认当前进程；DSH 为 Node 应用）。 */
  nodeBin?: string;
  profile: string;
  patchPath: string;
  home: string;
  workspace: string;
  /** 带外 delta 通道文件（注入子进程 `AETHER_DSH_DELTA_FILE`）。 */
  deltaFile: string;
  /** 追加参数（测试注入等）。 */
  extraArgs?: string[];
  env?: NodeJS.ProcessEnv;
  /** `session/update` 通知。 */
  onUpdate: (params: AcpUpdateParams) => void;
  /** `session/request_permission` 服务端请求（返回 ACP result）。 */
  onPermissionRequest: (params: unknown, requestId: number | string) => Promise<unknown> | unknown;
  onStderr?: (line: string) => void;
  /** 非预期断连（进程退出且未被 stop）。 */
  onClose?: (detail: string) => void;
}

interface Pending {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  timer: NodeJS.Timeout | null;
}

/** JSON-RPC/ACP 协议错误（携带服务端 code）。 */
export class AcpError extends Error {
  constructor(
    readonly code: number,
    message: string,
    readonly data?: unknown,
  ) {
    super(message);
    this.name = "AcpError";
  }
}

export class DshAcpClient {
  readonly pid: number | undefined;

  private readonly options: AcpClientOptions;
  private child: ChildProcessWithoutNullStreams;
  private stdoutBuffer = "";
  private readonly pending = new Map<number | string, Pending>();
  private nextId = 1;
  private closed = false;
  private stopped = false;
  private stderrTail = "";
  private readonly exitWaiters: Array<() => void> = [];

  constructor(options: AcpClientOptions) {
    this.options = options;
    const nodeBin = options.nodeBin ?? process.execPath;
    const args = [
      options.bin,
      "--profile",
      options.profile,
      "--patch",
      options.patchPath,
      ...(options.extraArgs ?? []),
    ];
    this.child = spawn(nodeBin, args, {
      cwd: options.workspace,
      env: {
        ...process.env,
        ...(options.env ?? {}),
        DSH_HOME: options.home,
        AETHER_DSH_DELTA_FILE: options.deltaFile,
      },
      windowsHide: true,
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.pid = this.child.pid ?? undefined;
    this.child.stdout.setEncoding("utf8");
    this.child.stdout.on("data", (chunk: string) => this.onStdoutChunk(chunk));
    this.child.stderr.setEncoding("utf8");
    this.child.stderr.on("data", (chunk: string) => {
      for (const line of chunk.split(/\r?\n/)) {
        if (line.length === 0) continue;
        this.stderrTail = `${this.stderrTail}${line}\n`.slice(-8000);
        options.onStderr?.(`[dsh-acp] ${line}`);
      }
    });
    this.child.once("error", (error) => {
      this.failAll(new Error(`DSH 进程错误：${error.message}`));
      this.markClosed();
    });
    this.child.once("close", (code, signal) => {
      this.markClosed();
      if (!this.stopped) {
        options.onClose?.(
          `DSH ACP 进程退出（code=${code ?? "null"} signal=${signal ?? "null"}）`,
        );
      }
    });
  }

  get stderr(): string {
    return this.stderrTail;
  }

  get isClosed(): boolean {
    return this.closed;
  }

  /** 发送 JSON-RPC 请求（超时后 reject）。 */
  request(method: string, params: unknown, timeoutMs = 60_000): Promise<unknown> {
    if (this.closed) return Promise.reject(new Error("DSH ACP 连接已关闭"));
    const id = this.nextId;
    this.nextId += 1;
    return new Promise<unknown>((resolve, reject) => {
      const timer =
        timeoutMs > 0
          ? setTimeout(() => {
              this.pending.delete(id);
              reject(new Error(`${method} 超时（${timeoutMs}ms）`));
            }, timeoutMs)
          : null;
      this.pending.set(id, { resolve, reject, timer });
      this.send({ id, method, params });
    });
  }

  notify(method: string, params: unknown): void {
    if (this.closed) return;
    this.send({ method, params });
  }

  /** 关闭：end stdin → 等待进程退出 → 超时整树回收。 */
  async close(timeoutMs = 5_000): Promise<void> {
    if (this.stopped) return;
    this.stopped = true;
    try {
      this.child.stdin.end();
    } catch {
      /* 已关闭 */
    }
    const exited = await this.waitClose(timeoutMs);
    if (!exited) await this.killTree();
  }

  /** 立即整树回收（中断兜底）。 */
  async kill(): Promise<void> {
    this.stopped = true;
    await this.killTree();
  }

  private send(message: Record<string, unknown>): void {
    try {
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`);
    } catch (error) {
      this.failAll(error instanceof Error ? error : new Error(String(error)));
    }
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

  private handleLine(line: string): void {
    let message: Record<string, unknown>;
    try {
      const parsed = JSON.parse(line) as unknown;
      if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return;
      message = parsed as Record<string, unknown>;
    } catch {
      this.options.onStderr?.(`[dsh-acp] 无法解析的 stdout 行：${line.slice(0, 200)}`);
      return;
    }
    const id = message["id"];
    if (id !== undefined && id !== null && this.pending.has(id as number | string)) {
      const pending = this.pending.get(id as number | string);
      if (!pending) return;
      this.pending.delete(id as number | string);
      if (pending.timer) clearTimeout(pending.timer);
      const error = message["error"] as { code?: number; message?: string; data?: unknown } | undefined;
      if (error) {
        pending.reject(new AcpError(error.code ?? -32603, error.message ?? "ACP 错误", error.data));
      } else {
        pending.resolve(message["result"] ?? null);
      }
      return;
    }
    const method = message["method"];
    if (method === "session/update") {
      this.options.onUpdate((message["params"] ?? {}) as AcpUpdateParams);
      return;
    }
    if (method === "session/request_permission") {
      const requestId = (id ?? this.nextId++) as number | string;
      void Promise.resolve(this.options.onPermissionRequest(message["params"] ?? {}, requestId))
        .then((result) => {
          if (!this.closed) this.send({ id: requestId, result: result ?? {} });
        })
        .catch((error: unknown) => {
          if (!this.closed) {
            this.send({
              id: requestId,
              error: { code: -32603, message: error instanceof Error ? error.message : String(error) },
            });
          }
        });
      return;
    }
    // 其余服务端请求：应答空结果（前向兼容）。
    if (id !== undefined && id !== null) {
      this.send({ id, result: {} });
    }
  }

  private failAll(error: Error): void {
    for (const [id, pending] of this.pending) {
      if (pending.timer) clearTimeout(pending.timer);
      pending.reject(error);
      this.pending.delete(id);
    }
  }

  private markClosed(): void {
    if (this.closed) return;
    this.closed = true;
    this.failAll(new Error("DSH ACP 连接已关闭"));
    for (const waiter of this.exitWaiters.splice(0)) waiter();
  }

  private waitClose(timeoutMs: number): Promise<boolean> {
    if (this.closed) return Promise.resolve(true);
    return new Promise<boolean>((resolve) => {
      const timer = setTimeout(() => resolve(false), Math.max(1, timeoutMs));
      this.exitWaiters.push(() => {
        clearTimeout(timer);
        resolve(true);
      });
    });
  }

  /** 整树回收（Windows `taskkill /T /F`；POSIX 进程组 SIGKILL）。 */
  private async killTree(): Promise<void> {
    const pid = this.pid;
    if (pid === undefined) return;
    if (process.platform === "win32") {
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
      process.kill(-pid, "SIGKILL");
    } catch {
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        /* 已退出 */
      }
    }
  }
}
