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
 *   边界（非功能验证，见 docs/M1-09-证据.md B1–B5）：
 *   B3）预置 ④⑤ 零 permission.request 通知 + M1-09 链路无存储依赖（permissions 表不可达）；
 *   B4）M1-09 基线 process.rs 不含进程生命周期关键词；当前树含关键词时必须有 M1-10 承接。
 *
 * 环境：
 *   - Bun（AETHER_BUN 或 ~/.bun/bin/bun[.exe]）用于编译 Mock 单文件；
 *   - Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析；
 *   - B4 基线检查需要 git 历史（CI checkout 需 fetch-depth: 0；浅克隆时 SKIP）。
 */
import { spawnSync } from "node:child_process";
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

// ===== 3.5 边界静态检查（B3/B4；边界验证，不是功能验证） =====

const PROCESS_BASELINE_COMMIT = process.env.AETHER_M1_09_COMMIT ?? "962de37";
const PROCESS_LIFECYCLE_KEYWORDS = [
  "setsid",
  "CREATE_NEW_PROCESS_GROUP",
  "JobObject",
  "TerminateJobObject",
  "台账",
  "backoff",
  "crash_loop",
  "launch_token",
];

function gitOutput(args) {
  const result = spawnSync("git", args, { cwd: repoRoot, encoding: "utf8" });
  return result.status === 0 ? result.stdout : null;
}

/**
 * 剥离 Rust 注释（行注释 + 可嵌套块注释），仅保留可执行代码。
 * 边界 B4 断言的是「未实现生命周期管理」而不是「未提及」：M1-09 基线注释中
 * 用于声明「属 M1-10 范围」的关键词（如「台账」）不应被计为违规。
 */
function stripRustComments(source) {
  let result = "";
  let blockDepth = 0;
  for (const line of source.split(/\r?\n/)) {
    let kept = "";
    let index = 0;
    while (index < line.length) {
      if (blockDepth > 0) {
        const open = line.indexOf("/*", index);
        const close = line.indexOf("*/", index);
        if (close === -1) {
          index = line.length;
          break;
        }
        if (open !== -1 && open < close) {
          blockDepth += 1;
          index = open + 2;
          continue;
        }
        blockDepth -= 1;
        index = close + 2;
        continue;
      }
      const lineComment = line.indexOf("//", index);
      const blockOpen = line.indexOf("/*", index);
      if (lineComment === -1 && blockOpen === -1) {
        kept += line.slice(index);
        break;
      }
      if (lineComment !== -1 && (blockOpen === -1 || lineComment < blockOpen)) {
        kept += line.slice(index, lineComment);
        index = line.length;
      } else {
        kept += line.slice(index, blockOpen);
        blockDepth = 1;
        index = blockOpen + 2;
      }
    }
    result += `${kept}\n`;
  }
  return result;
}

// B3（边界验证，不是功能验证）：M1-09 链路不得依赖存储——permissions 表在本链路不可达。
function checkBoundaryB3NoStoreDependency() {
  const manifest = readFileSync(
    path.join(repoRoot, "crates", "aether-adapters", "Cargo.toml"),
    "utf8",
  );
  const hits = ["aether-store", "rusqlite"].filter((name) =>
    new RegExp(`^\\s*${name}[\\s.]`, "m").test(manifest),
  );
  console.log(
    `[boundary B3] aether-adapters 依赖中的存储项 = ${JSON.stringify(hits)}（应为空 → permissions 表不可达）`,
  );
  return hits.length === 0;
}

// B4（边界验证，不是功能验证）：
// - 基线断言（字面口径）：M1-09 基线提交的 process.rs **可执行代码**不含生命周期关键词
//   （注释中声明「属 M1-10」的措辞不计入，见 stripRustComments）；
// - 当前树演化守护：若当前 process.rs 已含关键词，则必须存在 M1-10 承接（supervisor/）。
function checkBoundaryB4ProcessBaseline() {
  const baseline = gitOutput([
    "show",
    `${PROCESS_BASELINE_COMMIT}:crates/aether-adapters/src/process.rs`,
  ]);
  if (baseline === null) {
    console.log(
      `[boundary B4] 基线提交 ${PROCESS_BASELINE_COMMIT} 不可用（浅克隆？）→ SKIP；CI 需 fetch-depth: 0`,
    );
    return true;
  }
  const baselineCode = stripRustComments(baseline);
  const baselineHits = PROCESS_LIFECYCLE_KEYWORDS.filter((keyword) =>
    baselineCode.includes(keyword),
  );
  console.log(
    `[boundary B4] M1-09 基线 process.rs 可执行代码关键词命中 = ${JSON.stringify(baselineHits)}`,
  );
  if (baselineHits.length > 0) return false;

  const current = readFileSync(
    path.join(repoRoot, "crates", "aether-adapters", "src", "process.rs"),
    "utf8",
  );
  const currentHits = PROCESS_LIFECYCLE_KEYWORDS.filter((keyword) =>
    stripRustComments(current).includes(keyword),
  );
  const supervisorExists = existsSync(
    path.join(repoRoot, "crates", "aether-adapters", "src", "supervisor", "mod.rs"),
  );
  const takeoverOk = currentHits.length === 0 || supervisorExists;
  console.log(
    `[boundary B4] 当前树 process.rs 可执行代码关键词命中 = ${JSON.stringify(currentHits)}；` +
      `M1-10 承接（supervisor/mod.rs）= ${supervisorExists} → ${takeoverOk ? "OK" : "缺少承接"}`,
  );
  return takeoverOk;
}

record(
  "边界验证（这是边界验证，不是功能验证）：B3 aether-adapters 无存储依赖（permissions 表不可达）",
  checkBoundaryB3NoStoreDependency(),
);
record(
  "边界验证（这是边界验证，不是功能验证）：B4 M1-09 基线 process.rs 不含生命周期关键词（当前树须有 M1-10 承接）",
  checkBoundaryB4ProcessBaseline(),
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
    // ④⑤ 为 Mock 自包含预置（边界 B1/B3）：不经核心权限网关、不发 permission.request。
    let permissionNotifyCount = 0;
    for (const scenario of fixture.scenarios) {
      const result = await client.runScenario({
        trigger: scenario.trigger,
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
      let decisionMatches = true;
      if (scenario.decision) {
        decisionMatches = result.resolvedPayload?.decision === scenario.decision;
      }
      const terminal = result.types.at(-1);
      console.log(
        `[e2e] ${scenario.label}：${result.toolSequence.join(" → ")}；终态=${terminal}`,
      );
      record(
        `e2e ${scenario.label}：事件序列 == ${scenario.events.join(" → ")}${scenario.errorCode ? `（error.code=${scenario.errorCode}）` : ""}${scenario.decision ? `（预置决策=${scenario.decision}）` : ""}，终态=${scenario.terminal}`,
        sequenceMatches && errorCodeMatches && decisionMatches && terminal === scenario.terminal,
      );
      if (scenario.decision) {
        permissionNotifyCount = client.permissionRequests.length;
      }
    }

    // 边界验证（B3，非功能验证）：M1 预置 ④⑤ 不得产生 permission.request 通知。
    console.log(
      `[e2e] B3：预置 ④⑤ 期间 permission.request 通知数 = ${permissionNotifyCount}`,
    );
    record(
      "边界 B3（非功能验证）：预置 ④⑤ 零 permission.request 通知（不经核心权限网关）",
      permissionNotifyCount === 0,
    );

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
