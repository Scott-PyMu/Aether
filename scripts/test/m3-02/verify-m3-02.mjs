/**
 * M3-02 验证入口：会话工作台（设计 UI-01/02/05、D7；实施计划 v1.16 属主承接项；响应契约见 ADR-009）。
 *
 * 覆盖 DoD：
 *   1) E2E：创建 → 发送 → 流式渲染 → 中断 → 状态正确
 *      —— 前端集成（`SessionWorkbench.test.tsx`）+ 真实适配器链路
 *      （`m3_02_adapter_executor.rs`：Mock 适配器 + 监督器 + 生命周期 + IPC 后端）；
 *   2) 运行时选择器 cold/ready/degraded/disabled+reason 与能力徽标
 *      —— `SessionWorkbench.test.tsx` + `runtimes_list` 真实后端（`m3_02_session_backend.rs`）；
 *   3) 会话级模型覆盖透传（`session.create` 参数断言）
 *      —— 前端参数断言 + 后端落库断言（`sessions.model`）；
 *   4) 10k 消息会话滚动在帧预算内（基准）
 *      —— `workbenchScroll.test.tsx`（DOM 节点数 O(视口) + 滚动更新耗时 < 16ms）。
 *
 * 属主承接项（M3-01 生产补读缺口；Gate 3 核验）：
 *   - `messages_page` 真实后端（按 `last_seq` 补读 + 最近一页 + 缺口守卫）
 *     —— `m3_02_session_backend.rs`；
 *   - `aetherStore` 注入生产 `EventBackfillSource`（`readback_gap_too_large` 同码透传）
 *     —— `backfillSource.test.ts` + `workbenchBackfillE2E.test.tsx`（生产路径 E2E）；
 *   - 「缺口 >10k → 确认 → 清缓存重载最近 N 条」不重启核心/应用。
 *
 * 环境：Bun（AETHER_BUN 或 ~/.bun/bin/bun[.exe]）编译 Mock 单文件；
 *       Cargo / pnpm 经 scripts/test/lib/exec.mjs 解析。
 */
import { existsSync, mkdirSync } from "node:fs";
import { readFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

const cargo = bin("cargo");
const node = process.execPath;
const { command: pnpm, prefix: pnpmPrefix } = pnpmCommand();
const pnpmRun = (list, options) => run(pnpm, [...pnpmPrefix, ...list], options);
const bun = resolveBun();
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "m3-02");
const mockAdapter = path.join(tmpDir, `aether-mock-adapter${exeSuffix}`);

// ===== 1. 编译 Mock 适配器单文件（Bun；真实会话链路宿主）=====

mkdirSync(tmpDir, { recursive: true });
record(
  "编译 Mock 适配器单文件（bun build --compile；真实会话链路宿主）",
  run(bun, ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter], {
    cwd: repoRoot,
  }),
);

// ===== 2. 前端：工作台集成（DoD1/2/3/4）+ 生产补读路径 E2E =====

record(
  "pnpm --filter @aether/desktop test（工作台 E2E/运行时选择器/模型透传/10k 滚动/生产补读 E2E）",
  pnpmRun(["--filter", "@aether/desktop", "test"]),
);

// ===== 3. Rust：会话后端 + 真实适配器链路（DoD1/2/3 + 属主承接项）=====

record(
  "m3_02_session_backend（messages_page 补读/最近一页/缺口守卫；session_* 接线；model 落库；runtimes_list）",
  run(cargo, [
    "test",
    "-p",
    "aether-tauri",
    "--test",
    "m3_02_session_backend",
    "--",
    "--nocapture",
  ]),
);

record(
  "m3_02_adapter_executor（真实 Mock 适配器：创建→发送→流式→中断→dispose；delta 拼接=终稿；补读可读回）",
  run(
    cargo,
    ["test", "-p", "aether-tauri", "--test", "m3_02_adapter_executor", "--", "--nocapture"],
    {
      env: {
        AETHER_MOCK_ADAPTER: mockAdapter,
        AETHER_REQUIRE_MOCK_ADAPTER: "1",
      },
    },
  ),
);

// ===== 4. 静态检查：接线与契约在案 =====

{
  const problems = [];
  const read = (relative) => readFileSync(path.join(repoRoot, relative), "utf8");
  const sessionBackend = read("crates/aether-tauri/src/session_backend.rs");
  const writeQueue = read("crates/aether-store/src/write_queue.rs");
  const executor = read("crates/aether-tauri/src/adapter_executor.rs");
  const lib = read("crates/aether-tauri/src/lib.rs");
  const error = read("crates/aether-tauri/src/ipc/error.rs");
  const aetherStore = read("apps/desktop/src/aetherStore.ts");
  const backfillSource = read("apps/desktop/src/backfillSource.ts");
  const workbench = read("apps/desktop/src/SessionWorkbench.tsx");
  const required = [
    [sessionBackend, "fn messages_page", "messages_page 真实后端"],
    [sessionBackend, "events_page", "按 last_seq 补读 events 表"],
    [sessionBackend, "events_latest", "最近一页（缓存清空重载）"],
    [writeQueue, "fn messages_latest", "最近一页消息（工作台基线，尾部升序）"],
    [sessionBackend, "messages_latest", "最近一页消息接线（尾部语义）"],
    [sessionBackend, "ReadbackGapTooLarge", "缺口守卫同码透传"],
    [sessionBackend, "fn session_create", "session_create 接线"],
    [sessionBackend, "fn session_send", "session_send 接线"],
    [sessionBackend, "fn session_interrupt", "session_interrupt 接线"],
    [sessionBackend, "fn session_dispose", "session_dispose 接线"],
    [sessionBackend, "fn runtimes_list", "runtimes_list 真实快照"],
    [executor, "native_id", "Mode R native_id 映射（sessions.config）"],
    [executor, "pipeline.submit", "适配器事件经管线（先日志后广播）"],
    [executor, "RunExecutor for AdapterRunExecutor", "RunExecutor 生产接线"],
    [lib, "SessionBackend::new", "壳层装配会话后端"],
    [lib, "SessionManager::new", "壳层装配生命周期"],
    [error, "readback_gap_too_large", "IPC 错误码同码"],
    [aetherStore, "productionBackfillSource", "aetherStore 注入生产补读源"],
    [backfillSource, "backfill", "EventBackfillSource.backfill 映射 messages_page"],
    [backfillSource, "readback_gap_too_large", "同码透传判定"],
    [workbench, "runtime-selector", "运行时选择器"],
    [workbench, "capability-badge", "能力徽标"],
    [workbench, "session-status", "状态条"],
    [workbench, "VirtualList", "消息流虚拟滚动"],
    [workbench, "Markdown", "消息流 Markdown"],
  ];
  for (const [source, needle, label] of required) {
    if (!source.includes(needle)) problems.push(`${label}: 缺少 ${needle}`);
  }
  // `MessagesPageResponse` 字段表（ADR-009 决策 1）：六个字段齐备且 messages 为 Option。
  const responseFields = ["session_id", "last_seq", "max_seq", "events", "messages", "complete"];
  const structStart = sessionBackend.indexOf("pub struct MessagesPageResponse");
  const structEnd = structStart < 0 ? -1 : sessionBackend.indexOf("}", structStart);
  const structBody =
    structStart < 0 || structEnd < 0 ? "" : sessionBackend.slice(structStart, structEnd);
  if (structStart < 0) problems.push("MessagesPageResponse 结构体缺失");
  for (const field of responseFields) {
    if (!new RegExp(`pub ${field}:`).test(structBody)) {
      problems.push(`MessagesPageResponse 缺少字段 ${field}`);
    }
  }
  if (!/pub messages: Option<Vec<Message>>/.test(structBody)) {
    problems.push("MessagesPageResponse.messages 必须为 Option<Vec<Message>>（空值口径）");
  }
  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：会话后端/执行器/工作台接线与契约在案", problems.length === 0 ? 0 : 1);
}

process.exit(summarize("verify-m3-02", checks));
