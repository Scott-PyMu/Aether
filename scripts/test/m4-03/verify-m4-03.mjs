/**
 * M4-03 验证入口：协议与并发类失败场景演练（D6×5 + D8×6 = 11 场景；实施计划 §5 M4-03）。
 *
 * 分层说明（实施计划 M4-03）：M2/M3 验证单点行为正确（单测/集成），本任务验证
 * 端到端脚本化执行稳定 + 每场景输出「触发 → 自动应对 → 恢复」三段证据日志并归档。
 *
 *   1) half-line       半行/断流（D6）：残行丢弃 → 触发监督重启；不崩溃
 *   2) invalid-json    无效 JSON（D6）：跳过 + 诊断计数；连续 20 次判不健康断连
 *   3) request-timeout 请求超时（D6）：方法超时表生效；错误回传；run 断流超时可重试
 *   4) version-mismatch 版本不匹配（D6）：disabled + status_reason=version_mismatch + 升级提示
 *                      （Mock major 2.0 + DSH 1003 门闩）
 *   5) stdout-log      stdout 混入日志（D6）：同无效帧计数；有效帧重置；恢复不重启
 *   6) slow-ui         慢 UI 消费者（D8）：gap=true + 补读；控制事件不丢
 *   7) outbound-backlog 适配器出站积压（D8）：ack 快路径；背压拒新 run；已有 run 继续
 *   8) cancel-storm    取消风暴（D8）：20 并发 dispose ≤10s；dump 空
 *   9) deadlock-task   死锁/任务不退出（D8）：看门狗 dump + 强制清理（tracing 上报）
 *  10) broadcast-full  广播通道满（D8）：Lagged(k) → 清积压 + 补读最终一致
 *  11) delivery-backlog 控制投递积压（D8）：>32MB/会话数×5000 → 熔断重启 + journal 零丢失；
 *                      存储侧临时高水位（>4096）→ 暂停 ≤2s → 隔离 → 回落自动解除
 *
 * 夜跑：由 .github/workflows/m4-nightly.yml 在 Windows runner 执行；可本地复跑。
 */
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot } from "../lib/exec.mjs";
import { createDrill, parseOnly } from "../m4/lib/drill.mjs";

const cargo = bin("cargo");
const exeSuffix = process.platform === "win32" ? ".exe" : "";
const mockAdapter = path.join(repoRoot, "scripts", "test", ".tmp", "m4-03", `aether-mock-adapter${exeSuffix}`);
const dshAdapter = path.join(repoRoot, "scripts", "test", ".tmp", "m4-03", `aether-dsh-adapter${exeSuffix}`);

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}
const bun = resolveBun();

const drill = createDrill({ task: "M4-03", title: "协议与并发类失败场景演练（11 场景）" });
const mockEnv = { AETHER_MOCK_ADAPTER: mockAdapter, AETHER_REQUIRE_MOCK_ADAPTER: "1" };
const dshEnv = { AETHER_DSH_ADAPTER: dshAdapter, AETHER_REQUIRE_DSH_ADAPTER: "1" };

// ===== 0. 前置构建（Mock / DSH 适配器单文件）=====

function build(label, args) {
  console.log(`\n$ ${bun} ${args.join(" ")}   # ${label}`);
  const result = drill.capture(bun, args, { cwd: repoRoot });
  process.stdout.write(result.out);
  console.log(`[m4-03 build] ${label} exit=${result.status}`);
  if (result.status !== 0) {
    console.error(`[m4-03] 前置构建失败：${label}`);
    process.exit(1);
  }
}

build("Mock 适配器单文件", ["build", "packages/adapter-mock/src/main.ts", "--compile", "--outfile", mockAdapter]);
build("DSH 适配器单文件（版本门闩）", ["build", "packages/adapter-dsh/src/main.ts", "--compile", "--outfile", dshAdapter]);

// ===== 场景定义 =====

drill.define({
  id: "half-line",
  name: "半行/断流",
  source: "D6 失败场景表 / 矩阵 #20",
  trigger: "Mock 注入 half-line：写入残行（无 LF）后断开进程",
  response: "有界增量读取器丢弃未完成行并记录残行字节数；判定 StreamClosed（不崩溃）",
  recovery: "连接收口并可经监督器重启流程恢复（D5 自动重启路径，T5b 演练覆盖）",
  steps: [
    {
      name: "m1_09_robustness half_line（残行丢弃 + 断连原因）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m1_09_robustness",
        "half_line",
        "--",
        "--nocapture",
      ],
      env: mockEnv,
    },
  ],
});

drill.define({
  id: "invalid-json",
  name: "无效 JSON",
  source: "D6 失败场景表 / 矩阵 #21",
  trigger: "Mock 注入 bad-json ×20（hello 后连续坏 JSON 帧）",
  response: "逐帧跳过 + 诊断计数；连续 20 次 → 判不健康并断连（InvalidFrameStreak）",
  recovery: "监督器按不健康路径重启适配器（心跳/重启链由 T5b 演练覆盖）",
  steps: [
    {
      name: "m1_09_robustness twenty_bad_json（阈值 20 断连 + 计数）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m1_09_robustness",
        "twenty_bad_json",
        "--",
        "--nocapture",
      ],
      env: mockEnv,
    },
  ],
});

drill.define({
  id: "request-timeout",
  name: "请求超时",
  source: "D6 失败场景表 / 矩阵 #22",
  trigger: "方法超时（interrupt 5s 边界）与 run 断流 120s 无事件；超时码 1002",
  response: "按方法超时表取消 future + 错误回传（1002）；run 标 failed（run_stream_timeout，recoverable）",
  recovery: "UI 重试可用；事件活动可重置断流计时；中断在 5s 内生效",
  steps: [
    {
      name: "D6 方法超时表单元矩阵（frozen）",
      command: cargo,
      args: ["test", "-p", "aether-adapters", "--lib", "method_table_matches_d6"],
    },
    {
      name: "m1_09_consistency interrupt（5s 内中断生效）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m1_09_consistency",
        "interrupt",
        "--",
        "--nocapture",
      ],
      env: mockEnv,
    },
    {
      name: "m2_01_lifecycle（120s 断流 → failed 且可重试；活动重置）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m2_01_lifecycle",
        "run_stream_timeout",
        "--",
        "--nocapture",
      ],
    },
  ],
});

drill.define({
  id: "version-mismatch",
  name: "版本不匹配",
  source: "D6 失败场景表 / 矩阵 #23",
  trigger: "Mock 以 protocol 2.0 握手；DSH 适配器以 0.1.1-rc.2 对 0.1.5-rc.2 门闩",
  response: "拒绝加载 → disabled + status_reason=version_mismatch + 升级提示（含对端版本）；DSH 返回应用码 1003 映射同状态",
  recovery: "升级适配器后重启（启动重扫）；retry/enable 被守卫拒绝，不得直接启用",
  steps: [
    {
      name: "m1_09_robustness version_mismatch（major 不符 → disabled + 提示）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m1_09_robustness",
        "version_mismatch",
        "--",
        "--nocapture",
      ],
      env: mockEnv,
    },
    {
      name: "m2_11_dsh version_latch_mismatch（1003 → disabled + version_mismatch）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m2_11_dsh",
        "version_latch_mismatch",
        "--",
        "--nocapture",
      ],
      env: dshEnv,
    },
  ],
});

drill.define({
  id: "stdout-log",
  name: "stdout 混入日志",
  source: "D6 失败场景表 / 矩阵 #24",
  trigger: "Mock 注入 stdout-log ×5（日志行写入 stdout）",
  response: "帧校验失败 → 同无效 JSON 计入诊断；不崩溃；有效帧到达后计数重置",
  recovery: "health.ping 仍 ok；连接继续服务（无需重启）",
  steps: [
    {
      name: "m1_09_robustness stdout_log（计数 + 重置 + health ok）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-adapters",
        "--test",
        "m1_09_robustness",
        "stdout_log",
        "--",
        "--nocapture",
      ],
      env: mockEnv,
    },
  ],
});

drill.define({
  id: "slow-ui",
  name: "慢 UI 消费者",
  source: "D8 失败场景表 / 矩阵 #25",
  trigger: "事件桥 sink 阻塞 + 前端入站队列有界溢出",
  response: "delta 丢弃 + gap=true；控制事件不丢（日志先行）；不反压 reader（心跳/中断有界）",
  recovery: "补读（last_seq 分页）收敛，渲染最终一致且无重复；gap 清零",
  steps: [
    {
      name: "m3_01_event_bridge slow（不反压 + 有界）",
      command: cargo,
      args: ["test", "-p", "aether-tauri", "--test", "m3_01_event_bridge", "--", "--nocapture"],
      markers: [
        { phase: "trigger", prefix: "[m3-01-bridge] slow-sink-blocked" },
        { phase: "response", prefix: "[m3-01-bridge] forwarded=" },
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
      name: "eventStore.integration（gap=true 可补齐：补读收敛无重复；vitest）",
      command: process.execPath,
      args: [
        path.join(repoRoot, "apps", "desktop", "node_modules", "vitest", "vitest.mjs"),
        "run",
        "src/eventStore.integration.test.tsx",
      ],
      cwd: path.join(repoRoot, "apps", "desktop"),
      timeoutMs: 5 * 60 * 1000,
      // CI 慢机下 vitest 实时等待偶发抖动；重试一次（断言不变）。
      retries: 1,
    },
  ],
});

drill.define({
  id: "outbound-backlog",
  name: "适配器出站积压",
  source: "D8 失败场景表 / 矩阵 #26",
  trigger: "会话在途时连续发送（第二条排队、第三条 session_busy）；执行器阻塞（模型未返回）",
  response: "ack 快路径：消息与 run 行提交后立即 ack，不等模型；run 串行（1 执行 + 1 排队）；超出回 session_busy；写队列高水位拒新 run（storage_backpressure）",
  recovery: "已有 run 继续完成；队列推进后等待 run 执行；回落恢复准入",
  steps: [
    {
      name: "m2_01_lifecycle ack 快路径（提交即 ack，不等模型）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m2_01_lifecycle",
        "ack_returns_after_commit_without_waiting_for_model",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "m2_01_lifecycle 串行（1 执行/1 排队/第 3 条 busy）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m2_01_lifecycle",
        "parallel_sends_serialize_one_executing_one_queued_third_busy",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "m2_04_backpressure dod1（L2 拒新 run；已有 run 继续）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m2_04_backpressure",
        "dod1",
        "--",
        "--nocapture",
      ],
    },
  ],
});

drill.define({
  id: "cancel-storm",
  name: "取消风暴",
  source: "D8 失败场景表 / 矩阵 #27",
  trigger: "20 会话并发 dispose（取消树级联）",
  response: "取消树级联取消执行中 run 与权限等待；全部会话置 Cancelled；任务全部退出",
  recovery: "10s 内全部退出；dump 数 = 0（无强制清理）",
  steps: [
    {
      name: "m2_05_cancel dod1（20 并发 dispose ≤10s；dump 空）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m2_05_cancel",
        "dod1",
        "--",
        "--nocapture",
      ],
      markers: [{ phase: "recovery", prefix: "[m2-05 DoD1] " }],
    },
  ],
});

drill.define({
  id: "deadlock-task",
  name: "死锁/任务不退出",
  source: "D8 失败场景表 / 矩阵 #28",
  trigger: "不响应 Executor 忽略取消（任务卡在执行态）",
  response: "10s 看门狗记录任务 dump（task/run/时间戳/动作）并强制清理（abort）",
  recovery: "dump 经 tracing 上报（诊断包源）；任务注册表清理；<10s 不误报",
  steps: [
    {
      name: "m2_05_cancel dod3（10s dump + 强制清理）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m2_05_cancel",
        "dod3",
        "--",
        "--nocapture",
      ],
      markers: [{ phase: "response", prefix: "[m2-05 DoD3] " }],
    },
    {
      name: "m2_05_watchdog_log（tracing 上报 dump）",
      command: cargo,
      args: ["test", "-p", "aether-control", "--test", "m2_05_watchdog_log", "--", "--nocapture"],
      markers: [{ phase: "recovery", prefix: "captured watchdog log: " }],
    },
  ],
});

drill.define({
  id: "broadcast-full",
  name: "广播通道满",
  source: "D8 失败场景表 / 矩阵 #29",
  trigger: "广播容量压缩（cap=8）+ 高频事件 60–100 条；消费者滞后",
  response: "`Lagged(k)` 计数；清空消费者内存积压并切换 journal 补读（不开启熔断）",
  recovery: "补读最终一致（collected == 1..=60；forwarded + lagged ≥ 总数）",
  steps: [
    {
      name: "m2_04_backpressure dod4（Lagged → 补读最终一致）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m2_04_backpressure",
        "dod4",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "m3_01_event_bridge lagged（计数 + 转发继续）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-tauri",
        "--test",
        "m3_01_event_bridge",
        "lagged",
        "--",
        "--nocapture",
      ],
      markers: [{ phase: "response", prefix: "[m3-01-bridge] lagged-sink" }],
    },
  ],
});

drill.define({
  id: "delivery-backlog",
  name: "控制投递积压",
  source: "D8 失败场景表 / 矩阵 #30",
  trigger: "控制投递积压超限（压缩阈值 4 条/会话 + 4KiB）；存储侧写队列 >4096 高水位",
  response: "溢出 → 切换 journal 补读模式 + 熔断（拒绝新会话/新 run）；30s 后重启并从 journal 恢复；存储侧暂停 ≤2s → 3 次超时隔离 storage_backpressure",
  recovery: "journal 零丢失（补读 from_seq 连续）；熔断释放重启；队列回落 ≤1024 持续 30s 自动解除",
  steps: [
    {
      name: "m2_04_backpressure dod2（L3 熔断 + 释放 + journal 零丢失）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m2_04_backpressure",
        "dod2",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "m2_04_backpressure dod5（隔离 → 回落自动解除；persist_degraded 区分）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m2_04_backpressure",
        "dod5",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "m2_04_isolation（适配器隔离 → 解除重启）",
      command: cargo,
      args: ["test", "-p", "aether-adapters", "--test", "m2_04_isolation", "--", "--nocapture"],
    },
  ],
});

await drill.runAll({ only: parseOnly() });
process.exit(drill.summarize());
