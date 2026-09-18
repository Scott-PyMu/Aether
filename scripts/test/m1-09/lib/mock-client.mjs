/**
 * M1-09 验证脚本共享驱动：以「核心侧」身份驱动 Mock 适配器进程。
 *
 * 仅使用 Node 标准库；翻译自 M1-09 的 Rust 线协议客户端（独立实现，互为对照）。
 */
import { spawn } from "node:child_process";
import readline from "node:readline";

export class RpcTimeoutError extends Error {}

export class MockClient {
  static HELLO_TIMEOUT_MS = 10_000;

  frames = [];
  events = [];
  /** 收到的 `permission.request` 通知（B3 边界：M1 预置 ④⑤ 应为 0 条）。 */
  permissionRequests = [];
  invalidLines = 0;
  exitCode = null;
  exitPromise;

  #child;
  #pending = new Map();
  #nextId = 1;
  #waiters = [];

  constructor(child) {
    this.#child = child;
    const rl = readline.createInterface({ input: child.stdout });
    rl.on("line", (line) => this.#onLine(line));
    this.exitPromise = new Promise((resolve) => {
      child.on("exit", (code) => {
        this.exitCode = code;
        resolve(code);
        this.#wake();
      });
    });
  }

  /** 启动 Mock 并等待 hello（10s 握手约束）。 */
  static async spawn(binary, args = []) {
    const child = spawn(binary, args, { stdio: ["pipe", "pipe", "inherit"] });
    const client = new MockClient(child);
    const started = Date.now();
    await client.waitFor(
      () => client.frames.some((frame) => frame.method === "hello"),
      MockClient.HELLO_TIMEOUT_MS,
      "hello",
    );
    const hello = client.frames.find((frame) => frame.method === "hello");
    return { client, hello, helloLatencyMs: Date.now() - started };
  }

  #onLine(line) {
    let frame;
    try {
      frame = JSON.parse(line);
    } catch {
      this.invalidLines += 1;
      return;
    }
    this.frames.push(frame);
    if (frame.method === "event") this.events.push(frame.params);
    if (frame.method === "permission.request") this.permissionRequests.push(frame.params);
    if (frame.id !== undefined && (frame.result !== undefined || frame.error !== undefined)) {
      const pending = this.#pending.get(frame.id);
      if (pending) {
        this.#pending.delete(frame.id);
        clearTimeout(pending.timer);
        if (frame.error) {
          pending.reject(new Error(`RPC ${frame.error.code}: ${frame.error.message}`));
        } else {
          pending.resolve(frame.result);
        }
      }
    }
    this.#wake();
  }

  #wake() {
    for (const waiter of this.#waiters.splice(0)) waiter();
  }

  async waitFor(predicate, timeoutMs, label) {
    const deadline = Date.now() + timeoutMs;
    while (!predicate()) {
      if (this.exitCode !== null) {
        throw new Error(`等待 ${label} 时 Mock 已退出（exit=${this.exitCode}）`);
      }
      const remaining = deadline - Date.now();
      if (remaining <= 0) throw new Error(`等待 ${label} 超时`);
      await Promise.race([
        new Promise((resolve) => this.#waiters.push(resolve)),
        new Promise((resolve) => setTimeout(resolve, Math.min(remaining, 25))),
      ]);
    }
  }

  write(frame) {
    this.#child.stdin.write(`${JSON.stringify(frame)}\n`);
  }

  async request(method, params = {}, timeoutMs = 10_000) {
    const id = this.#nextId;
    this.#nextId += 1;
    const promise = new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        reject(new RpcTimeoutError(`${method} 超时（${timeoutMs}ms）`));
      }, timeoutMs);
      this.#pending.set(id, { resolve, reject, timer });
    });
    this.write({ jsonrpc: "2.0", id, method, params });
    return promise;
  }

  requestExpectError(method, params = {}, timeoutMs = 10_000) {
    return this.request(method, params, timeoutMs).then(
      (result) => {
        throw new Error(`${method} 预期失败但返回: ${JSON.stringify(result)}`);
      },
      (error) => error,
    );
  }

  runEvents(runId) {
    return this.events.filter((event) => event.run_id === runId);
  }

  runTypes(runId) {
    return this.runEvents(runId).map((event) => event.type);
  }

  toolSequence(runId) {
    return this.runTypes(runId).filter(
      (type) => type.startsWith("tool.") || type.startsWith("permission."),
    );
  }

  async createSession() {
    const result = await this.request("session.create", { title: "m1-09" });
    return result.session_id;
  }

  async send(sessionId, text, clientMsgId) {
    const result = await this.request("session.send", {
      session_id: sessionId,
      client_msg_id: clientMsgId,
      text,
    });
    return result.run_id;
  }

  async waitTerminal(runId, timeoutMs = 15_000) {
    await this.waitFor(
      () =>
        this.runTypes(runId).some((type) =>
          ["run.completed", "run.failed", "run.cancelled"].includes(type),
        ),
      timeoutMs,
      `run ${runId} 终态`,
    );
  }

  /** 执行一个工具调用场景（含中断注入）；④⑤ 为 Mock 自包含预置（B1/B3）。 */
  async runScenario({ trigger, interruption, clientMsgId }) {
    const sessionId = await this.createSession();
    const runId = await this.send(sessionId, trigger, clientMsgId);
    if (interruption === "tool.call_started") {
      await this.waitFor(
        () => this.runTypes(runId).includes("tool.call_started"),
        5000,
        "tool.call_started",
      );
      await this.request("session.interrupt", { session_id: sessionId });
    }
    await this.waitTerminal(runId);
    return {
      sessionId,
      runId,
      types: this.runTypes(runId),
      toolSequence: this.toolSequence(runId),
      lastEvent: this.runEvents(runId).at(-1)?.payload,
      resolvedPayload: this.runEvents(runId).find(
        (event) => event.type === "permission.resolved",
      )?.payload,
    };
  }

  /** DoD5 吞吐基准：bench:<n> 连发 delta，返回计数与速率。 */
  async benchmarkDeltas(count) {
    const sessionId = await this.createSession();
    const started = process.hrtime.bigint();
    const runId = await this.send(sessionId, `bench:${count}`, `bench-${count}`);
    await this.waitTerminal(runId, 60_000);
    const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;
    const deltas = this.runTypes(runId).filter((type) => type === "message.delta").length;
    return { deltas, elapsedMs, rate: deltas / (elapsedMs / 1000) };
  }

  /** 请求 shutdown 并等待进程退出。 */
  async shutdown(timeoutMs = 5000) {
    const result = await this.request("shutdown", {}, timeoutMs);
    const exited = await Promise.race([
      this.exitPromise.then(() => true),
      new Promise((resolve) => setTimeout(() => resolve(false), timeoutMs)),
    ]);
    return { result, exited, exitCode: this.exitCode };
  }

  /** 兜底回收进程。 */
  async dispose() {
    if (this.exitCode !== null) return;
    try {
      this.#child.stdin.end();
    } catch {
      // stdin 可能已关闭
    }
    const code = await Promise.race([
      this.exitPromise,
      new Promise((resolve) => setTimeout(() => resolve(null), 500)),
    ]);
    if (code === null) {
      this.#child.kill();
      await Promise.race([
        this.exitPromise,
        new Promise((resolve) => setTimeout(resolve, 1000)),
      ]);
    }
  }
}
