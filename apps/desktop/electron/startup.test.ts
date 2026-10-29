/**
 * 启动编排单测（M9-07 DoD-4；ADR-029 §3.5/§4.2/§4.3）。
 */
import { describe, expect, it } from "vitest";

import {
  AetherRpcError,
  AetherRpcTimeoutError,
  type DaemonExit,
  type InitializeResult,
} from "./daemonClient";
import {
  MIN_INITIALIZE_HARD_TIMEOUT_MS,
  fatalReasonForBusinessCode,
  fatalReasonForError,
  runStartupSequence,
  type StartupClient,
  type StartupOptions,
} from "./startup";

const RESULT: InitializeResult = {
  protocolVersion: "1.0",
  daemonVersion: "0.1.0",
  coreVersion: "0.1.0",
  dataDir: "C:\\Aether",
  phase: "warming",
  daemonCapabilities: ["events", "permissions", "dialogs"],
};

class FakeClient implements StartupClient {
  readonly pid = 9_001;
  stderrTail = "stderr-tail";
  bootstrapFailure: AetherRpcError | null = null;
  exitInfo: DaemonExit | null = null;
  killed = false;
  waited = false;
  started = false;
  readonly behavior: () => Promise<InitializeResult>;

  constructor(behavior: () => Promise<InitializeResult>) {
    this.behavior = behavior;
  }

  start(): void {
    this.started = true;
  }

  initialize(): Promise<InitializeResult> {
    return this.behavior();
  }

  kill(): void {
    this.killed = true;
  }

  waitForExit(): Promise<DaemonExit | null> {
    this.waited = true;
    return Promise.resolve(null);
  }
}

function options(
  create: (attempt: number) => FakeClient,
  overrides: Partial<StartupOptions> = {},
): StartupOptions {
  return {
    windowCreatedAt: 100,
    context: { logPath: "C:\\Aether\\logs\\aetherd.log", dataDir: "C:\\Aether" },
    initializeParams: {
      protocolVersion: "1.0",
      appVersion: "0.1.0",
      client: "electron",
      clientCapabilities: ["events"],
      requiredCapabilities: ["events"],
    },
    createClient: create,
    now: () => 200,
    ...overrides,
  };
}

describe("runStartupSequence", () => {
  it("成功：initialize 结果/耗时/目标达成；窗口先于 daemon", async () => {
    const client = new FakeClient(async () => RESULT);
    const times = [200, 250, 300];
    const outcome = await runStartupSequence(
      options(() => client, {
        now: () => times.shift() ?? 300,
      }),
    );
    expect(outcome.ok).toBe(true);
    if (!outcome.ok) return;
    expect(outcome.result.phase).toBe("warming");
    expect(client.started).toBe(true);
    expect(outcome.initializeMs).toBe(50);
    expect(outcome.withinTarget).toBe(true);
    expect(outcome.retried).toBe(false);
    expect(outcome.attempts).toHaveLength(1);
  });

  it("窗口必须先于 daemon（顺序断言）", async () => {
    const client = new FakeClient(async () => RESULT);
    await expect(
      runStartupSequence(
        options(() => client, {
          windowCreatedAt: 1_000,
          now: () => 200,
        }),
      ),
    ).rejects.toThrow("窗口必须先于 daemon 创建");
  });

  it("超时重试一次：第一次 timeout，第二次成功（kill + 等待退出）", async () => {
    const first = new FakeClient(async () => {
      throw new AetherRpcTimeoutError("initialize", 20);
    });
    const second = new FakeClient(async () => RESULT);
    const clients = [first, second];
    const outcome = await runStartupSequence(
      options((attempt) => {
        const client = clients[attempt - 1];
        if (client === undefined) throw new Error("未预期第三次尝试");
        return client;
      }),
    );
    expect(outcome.ok).toBe(true);
    if (!outcome.ok) return;
    expect(outcome.retried).toBe(true);
    expect(outcome.attempts.map((item) => item.outcome)).toEqual(["timeout", "ready"]);
    expect(first.killed).toBe(true);
    expect(first.waited).toBe(true);
    expect(second.started).toBe(true);
  });

  it("连续两次超时：handshake_failed（不再重试）", async () => {
    const clients = [
      new FakeClient(async () => {
        throw new AetherRpcTimeoutError("initialize", 20);
      }),
      new FakeClient(async () => {
        throw new AetherRpcTimeoutError("initialize", 20);
      }),
    ];
    const outcome = await runStartupSequence(
      options((attempt) => clients[attempt - 1] as FakeClient),
    );
    expect(outcome.ok).toBe(false);
    if (outcome.ok) return;
    expect(outcome.fatal.reason).toBe("handshake_failed");
    expect(outcome.retried).toBe(true);
    expect(outcome.attempts).toHaveLength(2);
  });

  it("spawn 失败：daemon_start_failed", async () => {
    const client = new FakeClient(async () => RESULT);
    client.start = () => {
      throw new AetherRpcError(-32603, "spawn ENOENT", { aether_code: "internal" });
    };
    const outcome = await runStartupSequence(options(() => client));
    expect(outcome.ok).toBe(false);
    if (outcome.ok) return;
    expect(outcome.fatal.reason).toBe("daemon_start_failed");
    expect(outcome.fatal.message).toContain("ENOENT");
  });

  it("预 initialize 致命：sync_dir_blocked → a4_blocked，携带 log_path/phase", async () => {
    const client = new FakeClient(async () => {
      const error = new AetherRpcError(-32005, "A4 阻断", {
        aether_code: "startup_blocked",
        business_code: "sync_dir_blocked",
        phase: "failed",
        log_path: "C:\\Aether\\logs\\aetherd.log",
      });
      client.bootstrapFailure = error;
      throw error;
    });
    const outcome = await runStartupSequence(options(() => client));
    expect(outcome.ok).toBe(false);
    if (outcome.ok) return;
    expect(outcome.fatal.reason).toBe("a4_blocked");
    expect(outcome.fatal.phase).toBe("failed");
    expect(outcome.fatal.message).toBe("A4 阻断");
  });

  it("DB 无法打开/checksum 不符/data_dir_locked 映射到 S-12 原因", () => {
    expect(fatalReasonForBusinessCode("db_open_failed")).toBe("db_open_failed");
    expect(fatalReasonForBusinessCode("schema_checksum_mismatch")).toBe(
      "schema_checksum_mismatch",
    );
    expect(fatalReasonForBusinessCode("data_dir_locked")).toBe("data_dir_locked");
    expect(fatalReasonForBusinessCode("session_busy")).toBeUndefined();
    expect(
      fatalReasonForError(
        new AetherRpcError(-32005, "x", { aether_code: "startup_blocked" }),
        "initialize",
      ),
    ).toBe("a4_blocked");
    expect(fatalReasonForError(new Error("x"), "spawn")).toBe("daemon_start_failed");
    expect(fatalReasonForError(new Error("x"), "initialize")).toBe("handshake_failed");
  });

  it("硬超时默认 10s（不得低于 ADR-029 §3.5）", () => {
    expect(MIN_INITIALIZE_HARD_TIMEOUT_MS).toBe(10_000);
  });

  it("2s 目标未达成：withinTarget=false（记录超时率）", async () => {
    const client = new FakeClient(async () => RESULT);
    const times = [200, 300, 2_600];
    const outcome = await runStartupSequence(
      options(() => client, { now: () => times.shift() ?? 2_400 }),
    );
    expect(outcome.ok).toBe(true);
    if (!outcome.ok) return;
    expect(outcome.initializeMs).toBe(2_300);
    expect(outcome.withinTarget).toBe(false);
  });

  it("onAttemptStart/onReady 回调按序触发", async () => {
    const order: string[] = [];
    const client = new FakeClient(async () => RESULT);
    await runStartupSequence(
      options(() => client, {
        onAttemptStart: (attempt) => order.push(`start-${attempt}`),
        onReady: () => order.push("ready"),
      }),
    );
    expect(order).toEqual(["start-1", "ready"]);
  });
});
