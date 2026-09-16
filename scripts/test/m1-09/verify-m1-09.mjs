/**
 * M1-09 验证脚本：线协议骨架与 Mock 适配器（D6、评审#6）。
 *
 * 覆盖 DoD：
 *   1) 一致性测试全绿：握手（10s/hello/major 校验）、流式、中断、dispose；
 *   2) 健壮性注入 5 类：半行/断流、坏 JSON（连续 20 次判不健康）、超长行、
 *      stdout 混入日志、未知方法（-32601 不断连）；JSON-RPC 外层未知成员忽略（单测）；
 *   3) 大行策略（DoD8，2MiB 读取器）：>2MiB 任意行立即断连；1–2MiB 非引用行正常解析；
 *      artifact_ref <1MiB 正常解析、1–2MiB 声称引用按契约违约断连（单测）；
 *   4) 版本不匹配 → disabled + status_reason + 升级提示（单测 + 真实进程 e2e）；
 *   5) Mock 吞吐基准 ≥1000 delta/s（Node 驱动编译产物实测）；
 *   6) 5 类工具调用注入清单（权威夹具 ↔ 真实 Mock 事件序列逐项对齐）；
 *   7) Mock「不响应模式」：停止响应 health/请求且不退出（脚本断言）。
 *
 * 环境：
 *   - Bun（AETHER_BUN 或 ~/.bun/bin/bun[.exe]）用于编译 Mock 单文件；
 *   - Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析。
 */
import { existsSync, readFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";
import { MockClient, RpcTimeoutError } from "./lib/mock-client.mjs";

const checks = [];
const record = (name, ok) => checks.push({ name, exit: ok ? 0 : 1, expect: 0 });

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

const cargo = bin("cargo");
const bun = resolveBun();
const mockBinary = path.join(
  repoRoot,
  "scripts",
  "test",
  ".tmp",
  "m1-09",
  process.platform === "win32" ? "aether-mock-adapter.exe" : "aether-mock-adapter",
);
const fixture = JSON.parse(
  readFileSync(new URL("./fixtures/tool-call-scenarios.json", import.meta.url), "utf8"),
);

// ===== 1. 编译 Mock 单文件（Bun） =====

record(
  "编译 Mock 适配器单文件（bun build --compile）",
  run(
    bun,
    ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockBinary],
    { cwd: repoRoot },
  ) === 0,
);

const mockEnv = {
  AETHER_MOCK_ADAPTER: mockBinary,
  AETHER_REQUIRE_MOCK_ADAPTER: "1",
};

// ===== 2. Rust 单测 / 集成测试 =====

record(
  "cargo test -p aether-adapters --lib（大行策略/版本不匹配/坏 JSON×20/未知方法 -32601/超时码）",
  run(cargo, ["test", "-p", "aether-adapters", "--lib"]) === 0,
);
record(
  "一致性测试（真实 Mock：握手 10s/流式/中断 5s/dispose/shutdown）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m1_09_consistency"], {
    env: mockEnv,
  }) === 0,
);
record(
  "健壮性注入 5 类（真实 Mock：半行/坏 JSON/超长行/stdout 日志/未知方法 + 版本不匹配 + 崩溃 + 2MiB/artifact_ref 契约）",
  run(cargo, ["test", "-p", "aether-adapters", "--test", "m1_09_robustness"], {
    env: mockEnv,
  }) === 0,
);

// ===== 3. TS SDK / Mock 单测（含 DoD6 权威夹具一致性） =====

const pnpm = pnpmCommand();
record(
  "adapter-sdk 单测（帧/信封/ULID/RPC/Adapter）",
  run(pnpm.command, [...pnpm.prefix, "--filter", "@aether/adapter-sdk", "test"]) === 0,
);
record(
  "adapter-mock 单测（5 类工具调用 + 5 类注入 + DoD6 权威夹具一致性）",
  run(pnpm.command, [...pnpm.prefix, "--filter", "@aether/adapter-mock", "test"]) === 0,
);

// ===== 4. Node 独立驱动：DoD5 吞吐基准 + DoD6 事件序列 + 握手/dispose e2e =====

async function nodeDrivenChecks() {
  const { client, hello, helloLatencyMs } = await MockClient.spawn(mockBinary);
  try {
    const helloOk =
      hello?.params?.protocol === "1.0" &&
      helloLatencyMs < MockClient.HELLO_TIMEOUT_MS &&
      typeof hello?.params?.runtime?.name === "string";
    console.log(
      `[e2e] hello 到达耗时 ${helloLatencyMs}ms，protocol=${hello?.params?.protocol}，runtime=${hello?.params?.runtime?.name}`,
    );
    record("e2e 握手：hello 10s 内到达且 protocol=1.0", helloOk);

    // 未知方法 -32601 不断连（DoD2 的独立实现对照）
    const unknownError = await client.requestExpectError("session.listen", {});
    const unknownOk =
      unknownError instanceof Error && unknownError.message.includes("-32601");
    const pong = await client.request("health.ping", {});
    console.log(`[e2e] 未知方法响应：${unknownError.message}；随后 health.ping=${pong.status}`);
    record("e2e 未知方法回 -32601 且连接可用", unknownOk && pong.status === "ok");

    // DoD6：5 类工具调用注入清单逐项对齐权威夹具
    for (const scenario of fixture.scenarios) {
      const result = await client.runScenario({
        trigger: scenario.trigger,
        decision: scenario.decision,
        interruption: scenario.interruption,
        clientMsgId: `verify-${scenario.id}`,
      });
      const sequenceMatches =
        JSON.stringify(result.toolSequence) === JSON.stringify(scenario.events);
      let errorCodeMatches = true;
      if (scenario.errorCode) {
        const failed = client
          .runEvents(result.runId)
          .find((event) => event.type === "tool.call_failed");
        errorCodeMatches = failed?.payload?.error?.code === scenario.errorCode;
      }
      const terminal = result.types.at(-1);
      console.log(
        `[e2e] ${scenario.label}：${result.toolSequence.join(" → ")}；终态=${terminal}`,
      );
      record(
        `e2e ${scenario.label}：事件序列 == ${scenario.events.join(" → ")}${scenario.errorCode ? `（error.code=${scenario.errorCode}）` : ""}，终态=${scenario.terminal}`,
        sequenceMatches && errorCodeMatches && terminal === scenario.terminal,
      );
    }

    // DoD5：吞吐基准
    const bench = await client.benchmarkDeltas(5000);
    console.log(
      `[bench] ${bench.deltas} 条 delta / ${bench.elapsedMs.toFixed(0)}ms = ${Math.round(bench.rate)} delta/s`,
    );
    record(
      `e2e Mock 吞吐基准 ≥1000 delta/s（实测 ${Math.round(bench.rate)} delta/s，${bench.deltas} 条无丢帧）`,
      bench.deltas === 5000 && bench.rate >= 1000,
    );

    // DoD1：dispose + shutdown 退出码 0
    const sessionId = await client.createSession();
    const disposed = await client.request("session.dispose", { session_id: sessionId });
    const shutdown = await client.shutdown();
    console.log(
      `[e2e] dispose=${JSON.stringify(disposed)}；shutdown exited=${shutdown.exited} exitCode=${shutdown.exitCode}`,
    );
    record(
      "e2e dispose 会话 + shutdown 优雅退出（exit code 0）",
      disposed.disposed === true && shutdown.exited === true && shutdown.exitCode === 0,
    );
  } finally {
    await client.dispose();
  }

  // 版本不匹配注入（真实进程 hello 携带 2.0）
  const mismatch = await MockClient.spawn(mockBinary, ["--protocol", "2.0"]);
  try {
    console.log(`[e2e] --protocol 2.0 → hello.protocol=${mismatch.hello?.params?.protocol}`);
    record(
      "e2e 版本不匹配注入：hello 携带 2.0（Rust 侧 disabled + status_reason + 升级提示见 m1_09_robustness）",
      mismatch.hello?.params?.protocol === "2.0",
    );
  } finally {
    await mismatch.client.dispose();
  }

  // DoD7：Mock 不响应模式（hang）——hello 送达后停止响应 health/请求且不退出
  const hang = await MockClient.spawn(mockBinary, ["--inject", "hang"]);
  try {
    const pingError = await hang.client.requestExpectError("health.ping", {}, 1500);
    const createError = await hang.client.requestExpectError(
      "session.create",
      { title: "hang" },
      1500,
    );
    const bothTimeout =
      pingError instanceof RpcTimeoutError && createError instanceof RpcTimeoutError;
    const stillAlive = hang.client.exitCode === null;
    console.log(
      `[e2e] hang：health.ping=${pingError.message}；session.create=${createError.message}；alive=${stillAlive}`,
    );
    record(
      "e2e DoD7 Mock 不响应模式：hello 后 health/请求均无响应且进程不退出（T5b/M2-08/M4-01 复用）",
      bothTimeout && stillAlive,
    );
  } finally {
    await hang.client.dispose();
  }
}

try {
  await nodeDrivenChecks();
} catch (error) {
  console.error(`[e2e] 失败：${error.stack ?? error}`);
  record(`e2e 自定义检查（异常中断：${error.message}）`, false);
}

process.exit(summarize("verify-m1-09", checks));
