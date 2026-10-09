/**
 * M4-06 新成员演练：按《适配器接入指南》从零接入 Mock 适配器并跑通（记录时长）。
 *
 * 以「未参与开发者视角」在干净临时目录执行指南第 2 节最小骨架步骤：
 *   ① 编译 Mock 适配器单文件（Bun；目标机无需 Node）；
 *   ② 进程启动 → 10s 内 hello（protocol 1.0）；
 *   ③ session.create → session.send（流式）→ run.completed；
 *   ④ session.dispose → shutdown → 进程退出 0。
 *
 * 证据：`AETHER_M4_06_DRILL {json}`（含分步耗时与总时长）+ 退出码。
 * 诚实口径：本脚本为「文档可执行性 + 接入路径」的自动化演练记录；
 * 真实未参与开发者的人工演练建议在发布检查时复做（记录见 M4-06 证据文档）。
 */
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { MockClient } from "../m1-09/lib/mock-client.mjs";
import { repoRoot, run } from "../lib/exec.mjs";

const exeSuffix = process.platform === "win32" ? ".exe" : "";
const drillRoot = path.join(repoRoot, "scripts", "test", ".tmp", "m4-06", `drill-${Date.now()}`);
const binary = path.join(drillRoot, `aether-mock-adapter${exeSuffix}`);
mkdirSync(drillRoot, { recursive: true });

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

const started = Date.now();
const steps = [];

// ① 编译适配器单文件
{
  const stepStarted = Date.now();
  const exit = run(
    resolveBun(),
    ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", binary],
    { cwd: repoRoot },
  );
  steps.push({ step: "build", exit, elapsed_ms: Date.now() - stepStarted });
  if (exit !== 0 || !existsSync(binary)) {
    console.error("[m4-06] 编译 Mock 适配器失败");
    process.exit(1);
  }
}

// ②–④ 按线协议驱动（复用 M1-09 独立 Node 客户端，互为对照实现）
let client;
try {
  const stepStarted = Date.now();
  const spawned = await MockClient.spawn(binary);
  client = spawned.client;
  const protocol = spawned.hello?.params?.protocol;
  steps.push({ step: "hello", elapsed_ms: Date.now() - stepStarted, protocol });
  if (protocol !== "1.0") throw new Error(`hello 协议版本不符：${protocol}`);

  const sessionStarted = Date.now();
  const sessionId = await client.createSession();
  const clientMsgId = `01J8ZQ5R0N7W9Y8X6V4T${String(Date.now()).slice(-5)}`;
  const runId = await client.send(sessionId, "hello from m4-06 drill", clientMsgId);
  await client.waitTerminal(runId);
  const types = client.runTypes(runId);
  steps.push({
    step: "session-run",
    elapsed_ms: Date.now() - sessionStarted,
    terminal: types.at(-1),
    has_completed: types.includes("message.completed"),
  });
  if (types.at(-1) !== "run.completed" || !types.includes("message.completed")) {
    throw new Error(`run 未正常完成：${types.join(",")}`);
  }

  const disposeStarted = Date.now();
  await client.request("session.dispose", { session_id: sessionId }, 15_000);
  const shutdown = await client.shutdown();
  steps.push({
    step: "dispose-shutdown",
    elapsed_ms: Date.now() - disposeStarted,
    exited: shutdown.exited,
    exit_code: shutdown.exitCode,
  });
  if (!shutdown.exited || shutdown.exitCode !== 0) {
    throw new Error(`shutdown 未正常退出：${JSON.stringify(shutdown)}`);
  }

  const evidence = {
    drill: "m4-06-new-member",
    guide: "docs/适配器接入指南.md",
    total_elapsed_ms: Date.now() - started,
    steps,
    pass: true,
  };
  writeFileSync(
    path.join(drillRoot, "evidence.json"),
    `${JSON.stringify(evidence, null, 2)}\n`,
    "utf8",
  );
  console.log(`AETHER_M4_06_DRILL ${JSON.stringify(evidence)}`);
  console.log(`[m4-06] 演练总耗时 ${evidence.total_elapsed_ms}ms；证据：${drillRoot}`);
} catch (error) {
  console.error(`[m4-06] 演练失败：${error instanceof Error ? error.message : error}`);
  if (client) await client.dispose();
  process.exit(1);
} finally {
  if (client) await client.dispose();
}
