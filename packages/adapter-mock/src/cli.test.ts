/**
 * Mock CLI `--launch-token` 单测（M1-09 增量修订）。
 *
 * 覆盖：
 * 1) 无参数正常启动（stdout 仍为 hello 帧）；
 * 2) 合法 ULID 解析成功、stderr 含 token、stdout 不泄漏 token；
 * 3) 非法值（非 ULID / 空值 / 缺值）warn 到 stderr 后忽略，仍能启动。
 */

import { describe, expect, it } from "vitest";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { parseArgs, runCli } from "./cli";

interface Frame {
  jsonrpc?: string;
  id?: number | string;
  method?: string;
  params?: Record<string, unknown>;
  result?: Record<string, unknown>;
}

/** 规范 ULID 示例（26 位 Crockford Base32）。 */
const VALID_TOKEN = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

/**
 * 测试桩的 stderr 与生产接线一致：裸行经 `stderrLogger("[adapter-mock]")` 前缀
 * （见 `main.ts`），因此断言的正是最终 stderr 行。
 */
function prefixedStderr(sink: string[]): (line: string) => void {
  return (line: string) => {
    sink.push(`[adapter-mock] ${line}`);
  };
}

class CliHarness {
  readonly stderr: string[] = [];
  readonly stdoutLines: string[] = [];
  readonly stdoutRaw = { text: "" };
  readonly exits: number[] = [];
  readonly done: Promise<void>;

  private readonly pendingLines: string[] = [];
  private wake?: () => void;
  private readonly lines = this.lineStream();

  constructor(argv: string[]) {
    const stderrLines = this.stderr;
    this.done = runCli({
      argv,
      lines: this.lines,
      writeLine: async (line) => {
        this.stdoutLines.push(line);
      },
      rawWrite: (chunk) => {
        this.stdoutRaw.text += chunk;
      },
      stderr: prefixedStderr(stderrLines),
      exit: (code) => {
        this.exits.push(code);
      },
    });
  }

  private async *lineStream(): AsyncGenerator<string> {
    for (;;) {
      if (this.pendingLines.length === 0) {
        await new Promise<void>((resolve) => {
          this.wake = resolve;
        });
      }
      while (this.pendingLines.length > 0) {
        yield this.pendingLines.shift()!;
      }
    }
  }

  send(line: string): void {
    this.pendingLines.push(line);
    const wake = this.wake;
    this.wake = undefined;
    wake?.();
  }

  request(method: string, params: unknown, id: number): void {
    this.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
  }

  frames(): Frame[] {
    return this.stdoutLines.map((line) => JSON.parse(line) as Frame);
  }

  /** stdout 全量文本（协议帧 + 原始写入），用于纯净性断言。 */
  stdoutText(): string {
    return [...this.stdoutLines, this.stdoutRaw.text].join("\n");
  }

  async waitFor(predicate: (frame: Frame) => boolean, label: string): Promise<Frame> {
    const deadline = Date.now() + 5000;
    for (;;) {
      const found = this.frames().find(predicate);
      if (found) return found;
      if (Date.now() > deadline) {
        throw new Error(`等待 ${label} 超时；stdout=${this.stdoutText()}`);
      }
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
  }

  waitForHello(): Promise<Frame> {
    return this.waitFor((frame) => frame.method === "hello", "hello");
  }
}

describe("Mock CLI --launch-token（M1-09 增量修订）", () => {
  it("无 --launch-token：正常启动且无 warn", async () => {
    const harness = new CliHarness([]);
    const hello = await harness.waitForHello();
    expect(hello.jsonrpc).toBe("2.0");
    expect(harness.stderr).toEqual([]);
    expect(harness.exits).toEqual([]);
  });

  it("合法 ULID：解析成功、stderr 含 token、stdout 不泄漏 token", async () => {
    const harness = new CliHarness([`--launch-token=${VALID_TOKEN}`]);
    await harness.waitForHello();

    // 纯解析断言（不经过进程）。
    const parsed = parseArgs([`--launch-token=${VALID_TOKEN}`], () => {});
    expect(parsed.launchToken).toBe(VALID_TOKEN);

    expect(harness.stderr).toContain(`[adapter-mock] launch-token=${VALID_TOKEN}`);

    // stdout 纯净性：真实请求/响应帧中不得出现 token。
    harness.request("health.ping", {}, 1);
    const response = await harness.waitFor((frame) => frame.id === 1, "health.ping 响应");
    expect(response.result?.status).toBe("ok");
    expect(harness.frames().length).toBeGreaterThanOrEqual(2);
    expect(harness.stdoutText()).not.toContain(VALID_TOKEN);
  });

  it("非法值（非 ULID / 空值 / 缺值）：warn 后忽略且仍能启动", async () => {
    const argv = ["--launch-token=not-a-ulid", "--launch-token=", "--launch-token"];
    const warnings: string[] = [];
    expect(() => parseArgs(argv, (line) => warnings.push(line))).not.toThrow();
    expect(warnings).toHaveLength(3);
    expect(warnings.every((line) => line.includes("--launch-token"))).toBe(true);

    const harness = new CliHarness(argv);
    await harness.waitForHello();

    // 未宣告任何 token；非法值仅 warn。
    const announced = harness.stderr.filter(
      (line) => line.startsWith("[adapter-mock] launch-token=") && !line.includes("忽略"),
    );
    expect(announced).toEqual([]);
    expect(harness.stderr.some((line) => line.includes("忽略非法 --launch-token"))).toBe(true);
    expect(harness.exits).toEqual([]);
  });
});

describe("Mock CLI --artifacts-dir（M2-09/D6 附件外置）", () => {
  it("--artifacts-dir 解析并注入 Mock；env 回退生效", async () => {
    const dir = mkdtempSync(path.join(tmpdir(), "aether-m2-09-cli-"));
    const previous = process.env.AETHER_ARTIFACTS_DIR;
    try {
      // 1) 显式 --artifacts-dir。
      const parsed = parseArgs(["--artifacts-dir", dir], () => {});
      expect(parsed.artifactsDir).toBe(dir);

      // 2) env 回退：未传 CLI 时读取 AETHER_ARTIFACTS_DIR。
      process.env.AETHER_ARTIFACTS_DIR = dir;
      const harness = new CliHarness([]);
      await harness.waitForHello();
      harness.request("session.create", { title: "x" }, 1);
      await harness.waitFor((frame) => frame.id === 1, "session.create");
      harness.request("session.send", { session_id: "mock-sess-1", client_msg_id: "c1", text: "artifact:env.png:512" }, 2);
      await harness.waitFor((frame) => frame.id === 2, "session.send");
      await harness.waitFor((frame) => frame.method === "artifact_ref", "artifact_ref");
      expect(existsSync(path.join(dir, "env.png"))).toBe(true);
    } finally {
      if (previous === undefined) delete process.env.AETHER_ARTIFACTS_DIR;
      else process.env.AETHER_ARTIFACTS_DIR = previous;
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
