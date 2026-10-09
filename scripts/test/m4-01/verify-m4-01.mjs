/**
 * M4-01 验证入口：进程类失败场景演练（D2×4 + D5×7 = 11 场景；实施计划 §5 M4-01）。
 *
 * 分层说明（实施计划 M4-01）：M2/M3 验证单点行为正确（单测/集成），本任务验证
 * 端到端脚本化执行稳定 + 每场景输出「触发 → 自动应对 → 恢复」三段证据日志并归档。
 * 两者不可互相替代——本脚本不重复实现断言，编排既有测试夹具并提取其证据输出：
 *
 *   1) task-panic    任务 panic（D2）：m2_07_panic（JoinError 隔离 + 会话 failed + 续聊/提升重跑）
 *   2) core-oom      核心 OOM（D2）：m1_10_resources（真实跨进程 RSS 告警/限流/复位全链路）
 *                    + m2_07_resource（ResourcePatrol alert→throttle→reset）
 *                    + m2_09_oom（真实 1.5GiB 压力告警→限流→回落，不崩溃）
 *   3) exit-hang     退出卡死（D2）：m2_08_exit（T11 无响应适配器 ≤10s 退出 + 无残留 + 存储五步）
 *   4) ui-lag        UI 侧卡顿（D2）：m3_01_event_bridge（慢消费不反压 reader）
 *                    + m2_04_backpressure dod3（reader 心跳/中断 ≤2s）+ HealthMonitor UI 提示与重启入口
 *   5) start-crash   启动即崩（D5）：m1_10_supervisor（stderr 尾 50 行 + disabled+start_failed）
 *   6) untrusted     非官方适配器（D5）：m1_10_supervisor（untrusted + 审计）
 *   7) runtime-crash 运行中崩溃（D5）：m2_02_t5a（Claude；30s Ready + 在途收口 + Mode R 重放）
 *                    + m2_11_codex t5a（Codex）
 *   8) deaf-hang     卡死无响应（D5）：m2_08_t5b（严格 D5 心跳 10s/5s/连续 3 次 ≤45s → 120s Ready）
 *   9) orphan        孤儿进程（D5）：m2_08_orphans（强杀核心重启清理；PID 复用 0 误杀）
 *                    + m1_10_ledger（台账三条件）
 *  10) output-flood  输出洪水（D5）：m2_09_memory（超长行洪水内存受控）
 *                    + RSS 超限全链路（与 core-oom 同夹具，必过项）
 *  11) double-open    双开同一适配器（D5）：m1_10_ledger + T12 E2E（单实例锁 + 聚焦已有窗口）
 *
 * 夜跑：由 .github/workflows/m4-nightly.yml（schedule + workflow_dispatch）在
 * Windows runner 执行；本脚本可本地复跑（`node scripts/test/m4-01/verify-m4-01.mjs`）。
 * 参数：--skip-e2e（非 Windows 本地调试用，跳过真实 WebView E2E——CI 夜跑不使用）。
 */
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot } from "../lib/exec.mjs";
import { createDrill, parseOnly } from "../m4/lib/drill.mjs";

const cargo = bin("cargo");
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repoRoot, "target");
const fixture = path.join(targetDir, "debug", `aether-adapter-fixture${exeSuffix}`);
const mockAdapter = path.join(repoRoot, "scripts", "test", ".tmp", "m4-01", `aether-mock-adapter${exeSuffix}`);
const claudeAdapter = path.join(repoRoot, "scripts", "test", ".tmp", "m4-01", `aether-claude-adapter${exeSuffix}`);
const codexAdapter = path.join(repoRoot, "scripts", "test", ".tmp", "m4-01", `aether-codex-adapter${exeSuffix}`);
const skipE2e = process.argv.includes("--skip-e2e");

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}
const bun = resolveBun();

const drill = createDrill({ task: "M4-01", title: "进程类失败场景演练（11 场景）" });
const fixtureEnv = { AETHER_FIXTURE_BIN: fixture, AETHER_REQUIRE_FIXTURE: "1" };
const mockEnv = { AETHER_MOCK_ADAPTER: mockAdapter, AETHER_REQUIRE_MOCK_ADAPTER: "1" };
const claudeEnv = { AETHER_CLAUDE_ADAPTER: claudeAdapter, AETHER_REQUIRE_CLAUDE_ADAPTER: "1" };
const codexEnv = { AETHER_CODEX_ADAPTER: codexAdapter, AETHER_REQUIRE_CODEX_ADAPTER: "1" };

// ===== 0. 前置构建（夹具 + 适配器单文件）=====

function build(label, command, args, options) {
  console.log(`\n$ ${command} ${args.join(" ")}   # ${label}`);
  const result = drill.capture(command, args, options);
  process.stdout.write(result.out);
  console.log(`[m4-01 build] ${label} exit=${result.status}`);
  if (result.status !== 0) {
    console.error(`[m4-01] 前置构建失败：${label}`);
    process.exit(1);
  }
}

build("故障注入夹具（M1-10/M2-08 复用）", cargo, [
  "build",
  "-p",
  "aether-adapters",
  "--bin",
  "aether-adapter-fixture",
]);
build("Mock 适配器单文件", bun, [
  "build",
  "packages/adapter-mock/src/main.ts",
  "--compile",
  "--outfile",
  mockAdapter,
]);
build("Claude Code 适配器单文件（T5a）", bun, [
  "build",
  "packages/adapter-claude-code/src/main.ts",
  "--compile",
  "--outfile",
  claudeAdapter,
]);
build("Codex 适配器单文件（T5a）", bun, [
  "build",
  "packages/adapter-codex/src/main.ts",
  "--compile",
  "--outfile",
  codexAdapter,
]);

// ===== 场景定义 =====

drill.define({
  id: "task-panic",
  name: "任务 panic",
  source: "D2 进程专项 / 矩阵 #1",
  trigger: "注入 PanicExecutor：某会话任务在 run 执行中 panic（JoinError 捕获路径）",
  response: "仅该会话标 failed（task_panic）；其余会话事件流连续；sequencer 计数与事件流不中断",
  recovery: "原会话续聊完成；等待队列 run 提升后重跑成功（run 串行契约不变）",
  steps: [
    {
      name: "m2_07_panic（panic 隔离 + 续聊 + 提升重跑）",
      command: cargo,
      args: ["test", "-p", "aether-control", "--test", "m2_07_panic", "--", "--nocapture"],
      markers: [
        { phase: "response", prefix: "[m2-07 DoD1] " },
        { phase: "recovery", prefix: "[m2-07 DoD1+] " },
      ],
    },
  ],
});

drill.define({
  id: "core-oom",
  name: "核心 OOM（真实 RSS 全链路为必过项）",
  source: "D2 进程专项 / 矩阵 #2",
  trigger: "真实 RSS 超限：适配器进程分配 160MiB（阈值钩子）+ 核心 ResourcePatrol 真实 1.5GiB 压力注入",
  response: "监督器 RSS 告警（去重/限流、不杀进程）；ResourcePatrol alert→throttle（core_rss_alert/core_rss_throttle 落盘）；delta 窗口放宽",
  recovery: "崩溃重启后 RSS 回落 → 监视器复位 → 二次告警证明复位；释放 1.5GiB 后巡检回 Normal 且管线续写正常（不崩溃）",
  steps: [
    {
      name: "m1_10_resources（真实跨进程 RSS 告警 → 限流 → 重启回落复位 → 二次告警 → 全程不杀）",
      command: cargo,
      args: ["test", "-p", "aether-adapters", "--test", "m1_10_resources", "--", "--nocapture"],
      markers: [
        { phase: "trigger", prefix: "[m1-10-resources] 告警#1" },
        { phase: "response", prefix: "[m1-10-resources] 持续超限 3 次采样未重复告警" },
        { phase: "response", prefix: "[m1-10-resources] run#2 RSS 回落" },
        { phase: "recovery", prefix: "[m1-10-resources] 告警#2" },
        { phase: "recovery", prefix: "[m1-10-resources] 链路证据：" },
      ],
    },
    {
      name: "m2_07_resource（ResourcePatrol alert → throttle → reset）",
      command: cargo,
      args: ["test", "-p", "aether-control", "--test", "m2_07_resource", "--", "--nocapture"],
      markers: [
        { phase: "response", prefix: "[m2-07 DoD3] " },
        { phase: "recovery", prefix: "[m2-07 DoD3+] " },
      ],
    },
    {
      name: "m2_09_oom（真实 1.5GiB 压力 → 告警/限流/回落；需 ~1.5GiB 内存）",
      command: cargo,
      args: ["test", "-p", "aether-control", "--test", "m2_09_oom", "--", "--nocapture"],
      timeoutMs: 10 * 60 * 1000,
      markers: [
        { phase: "trigger", prefix: "[m2-09 DoD3] 第一步分配" },
        { phase: "response", prefix: "[m2-09 DoD3] 增长至" },
        { phase: "recovery", prefix: "[m2-09 DoD3] 已释放 1.5GiB" },
        { phase: "recovery", prefix: "[m2-09 DoD3] 真实 1.5GiB 压力：" },
      ],
    },
  ],
});

drill.define({
  id: "exit-hang",
  name: "退出卡死（T11）",
  source: "D2 进程专项 / 矩阵 #3",
  trigger: "deaf 夹具进入不响应模式（忽略 health/shutdown RPC，不退出）；触发应用退出序列",
  response: "关闭编排按 D2 顺序硬超时逐级执行：广播 shutdown → 适配器终止段（RPC 5s → 组/树终止）→ 存储五步；T11 有界",
  recovery: "≤10s 退出、适配器进程无残留、WAL 归零；Windows TerminateJobObject 优先（无 taskkill /T /F 兜底）",
  steps: [
    {
      name: "m2_08_exit（T11 退出编排）",
      command: cargo,
      args: ["test", "-p", "aether-tauri", "--test", "m2_08_exit", "--", "--nocapture"],
      env: fixtureEnv,
      markers: [
        {
          phase: "trigger",
          prefix: "AETHER_M2_08_T11 ",
          assert: (value) => value.within_budget === true,
        },
        {
          phase: "response",
          prefix: "AETHER_M2_08_T11 ",
          assert: (value) => value.adapter?.exited === true && value.deadline_expired === false,
        },
        {
          phase: "recovery",
          prefix: "AETHER_M2_08_T11 ",
          assert: (value) =>
            value.adapter_residual === false &&
            value.storage?.d2_order === true &&
            value.storage?.wal_bytes_after === 0,
        },
      ],
    },
  ],
});

drill.define({
  id: "ui-lag",
  name: "UI 侧卡顿",
  source: "D2 进程专项 / 矩阵 #4",
  trigger: "慢 UI 消费者注入：事件桥 sink 阻塞；核心侧慢订阅者阻塞广播消费",
  response: "桥转发积压不反压管线/health（max_submit/health/readback 有界）；reader 心跳与中断请求 ≤2s；前端「核心未响应」提示与重启入口可用",
  recovery: "消费恢复后事件零丢失（forwarded 追平）；UI health 恢复轮询",
  steps: [
    {
      name: "m3_01_event_bridge（慢消费不反压 reader；含 lagged 计数）",
      command: cargo,
      args: ["test", "-p", "aether-tauri", "--test", "m3_01_event_bridge", "--", "--nocapture"],
      markers: [
        { phase: "trigger", prefix: "[m3-01-bridge] slow-sink-blocked" },
        { phase: "recovery", prefix: "[m3-01-bridge] forwarded=" },
      ],
    },
    {
      name: "m2_04_backpressure dod3（reader 心跳/中断 ≤2s）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m2_04_backpressure",
        "dod3",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "HealthMonitor UI（15s 无响应提示 + 重启入口；vitest）",
      command: process.execPath,
      args: [
        path.join(repoRoot, "apps", "desktop", "node_modules", "vitest", "vitest.mjs"),
        "run",
        "src/HealthMonitor.test.tsx",
      ],
      cwd: path.join(repoRoot, "apps", "desktop"),
      timeoutMs: 5 * 60 * 1000,
      // CI 慢机下 vi.waitFor 实时等待存在偶发抖动；重试一次（断言不变）。
      retries: 1,
    },
  ],
});

drill.define({
  id: "start-crash",
  name: "启动即崩",
  source: "D5 进程专项 / 矩阵 #13",
  trigger: "夹具以 stderr-crash 模式启动（stderr 输出 60 行后以码 7 退出）",
  response: "start_failed 收口并抓取 stderr 尾 50 行（截断语义）；状态 disabled + status_reason=start_failed",
  recovery: "runtime_retry 白名单/状态守卫：仅 disabled+start_failed 可用（修复后可重试）",
  steps: [
    {
      name: "m1_10_supervisor（stderr 尾 50 行 + start_failed + retry 守卫）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m1_10_supervisor",
        "start_failure",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "m1_10_supervisor（retry 守卫：白名单 + 仅 start_failed）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m1_10_supervisor",
        "runtime_retry_requires_whitelist_and_start_failed",
        "--",
        "--nocapture",
      ],
    },
  ],
});

drill.define({
  id: "untrusted-adapter",
  name: "非官方适配器",
  source: "D5 进程专项 / 矩阵 #14",
  trigger: "加载未标记官方白名单的 manifest",
  response: "拒绝加载 → disabled + status_reason=untrusted；审计记录（准入白名单强制）",
  recovery: "无自动恢复路径（第三方须 P3 沙箱 + 信任确认流程）；断言不可被 retry/enable 绕过",
  steps: [
    {
      name: "m1_10_supervisor（untrusted + 审计）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m1_10_supervisor",
        "untrusted_manifest",
        "--",
        "--nocapture",
      ],
    },
  ],
});

drill.define({
  id: "runtime-crash",
  name: "运行中崩溃（T5a）",
  source: "D5 进程专项 / 矩阵 #15",
  trigger: "跨平台强杀 helper 外部强杀适配器进程（Windows TerminateJobObject 优先 / Unix SIGKILL）",
  response: "在途 run 收口为 failed（adapter_disconnected/cli_exit，recoverable）；监督器 degraded→starting 自动重启",
  recovery: "30s 内 Ready（新 PID）；Mode R 原生恢复重放成功",
  steps: [
    {
      name: "m2_02_t5a（Claude）",
      command: cargo,
      args: ["test", "-p", "aether-adapters", "--test", "m2_02_t5a", "--", "--nocapture"],
      env: claudeEnv,
      markers: [
        { phase: "response", prefix: "[m2-02 T5a] 在途 run 收口=" },
        { phase: "recovery", prefix: "[m2-02 T5a] 外部强杀" },
      ],
    },
    {
      name: "m2_11_codex t5a（Codex）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m2_11_codex",
        "t5a",
        "--",
        "--nocapture",
      ],
      env: codexEnv,
      markers: [{ phase: "recovery", prefix: "[m2-11 codex T5a]" }],
    },
  ],
});

drill.define({
  id: "deaf-hang",
  name: "卡死无响应（T5b）",
  source: "D5 进程专项 / 矩阵 #16",
  trigger: "deaf 夹具进入不响应模式（不使用 SIGSTOP）；心跳计时起点 = 注入时刻",
  response: "严格 D5 心跳 10s/5s、连续 3 次失败（≤45s）触发重启；终止序列无残留",
  recovery: "120s 内 Ready（新 PID）；旧进程无残留；survive-eof 孤儿对照用例通过",
  steps: [
    {
      name: "m2_08_t5b（心跳熔断 + 120s Ready）",
      command: cargo,
      args: ["test", "-p", "aether-adapters", "--test", "m2_08_t5b", "--", "--nocapture"],
      timeoutMs: 5 * 60 * 1000,
      markers: [
        { phase: "trigger", prefix: "AETHER_M2_08_T5B " },
        { phase: "recovery", prefix: "AETHER_M2_08_FIXTURE_EOF " },
      ],
    },
  ],
});

drill.define({
  id: "orphan",
  name: "孤儿进程",
  source: "D5 进程专项 / 矩阵 #17",
  trigger: "子进程扮演核心并预热 deaf --survive-eof 夹具后被强杀；重启走启动清理",
  response: "启动清理三条件：PID 存活 + 启动时间一致 + launch_token 命中；PID 复用/令牌不符诱饵 0 误杀",
  recovery: "token 命中孤儿整树回收（Windows Job Object / TerminateJobObject 优先 + taskkill 兜底）；台账三条件单测",
  steps: [
    {
      name: "m2_08_orphans（强杀核心 → 重启清理；单线程）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-tauri",
        "--test",
        "m2_08_orphans",
        "--",
        "--nocapture",
        "--test-threads=1",
      ],
      env: fixtureEnv,
      timeoutMs: 10 * 60 * 1000,
      markers: [
        { phase: "trigger", prefix: "AETHER_M2_08_DOD1_HOST_READY " },
        { phase: "response", prefix: "AETHER_M2_08_DOD1_CLEANUP " },
        { phase: "recovery", prefix: "AETHER_M2_08_DOD1 " },
      ],
    },
    {
      name: "m1_10_ledger（台账三条件：全命中清理；时间/令牌不符不杀）",
      command: cargo,
      args: ["test", "-p", "aether-adapters", "--test", "m1_10_ledger", "--", "--nocapture"],
    },
  ],
});

drill.define({
  id: "output-flood",
  name: "输出洪水（真实 RSS 全链路为必过项）",
  source: "D5 进程专项 / 矩阵 #18",
  trigger: "Mock 适配器连续注入超长行洪水（line-over-2mib-burst / oversized-line-burst）",
  response: ">2MiB 任意行不缓冲立即断连记错；1–2MiB 非引用行正常解析；内存曲线受控（有界读取器）",
  recovery: "连接在风暴后恢复；RSS 超限全链路（告警 → 限流 → 回落复位，复用 M1-10 夹具与阈值钩子）",
  steps: [
    {
      name: "m2_09_memory（超长行洪水内存受控 + 风暴后恢复）",
      command: cargo,
      args: ["test", "-p", "aether-adapters", "--test", "m2_09_memory", "--", "--nocapture"],
      env: mockEnv,
      markers: [{ phase: "response", prefix: "[m2-09 DoD1] " }],
    },
    {
      name: "m1_10_resources（RSS 超限全链路必过项，与 core-oom 同夹具）",
      command: cargo,
      args: ["test", "-p", "aether-adapters", "--test", "m1_10_resources", "--", "--nocapture"],
      markers: [
        { phase: "response", prefix: "[m1-10-resources] 持续超限 3 次采样未重复告警" },
        { phase: "recovery", prefix: "[m1-10-resources] 链路证据：" },
      ],
    },
  ],
});

drill.define({
  id: "double-open-adapter",
  name: "双开同一适配器",
  source: "D5 进程专项 / 矩阵 #19",
  trigger: "二次启动应用（同一数据目录）以触发单实例锁；同一台账记录重复启动路径",
  response: "第二实例在 setup 前退出并转交 argv；首实例聚焦已有窗口；无双写连接（T12）",
  recovery: "台账去重与三条件清理语义保持（M1-10）；无残留进程",
  steps: [
    {
      name: "m1_10_ledger（台账去重/三条件）",
      command: cargo,
      args: ["test", "-p", "aether-adapters", "--test", "m1_10_ledger", "--", "--nocapture"],
    },
    ...(skipE2e
      ? [
          {
            name: "T12 E2E（--skip-e2e：非 Windows 本地调试跳过，夜跑不使用）",
            command: process.execPath,
            args: ["-e", "console.log('[skip-e2e] T12 E2E 跳过（Windows WebView2 专属）')"],
          },
        ]
      : [
          {
            name: "T12 E2E（单实例锁 + 聚焦已有窗口；真实 WebView2）",
            command: process.execPath,
            args: [path.join(repoRoot, "scripts", "test", "m1-06", "e2e-startup-guard.mjs")],
            timeoutMs: 15 * 60 * 1000,
            markers: [
              { phase: "response", prefix: "T12 ", contains: true },
              { phase: "recovery", prefix: "AETHER_M1_06_FOCUS", contains: true },
            ],
          },
        ]),
  ],
});

// ===== 执行与归档 =====

await drill.runAll({ only: parseOnly() });
process.exit(drill.summarize());
