/**
 * M4-02 验证入口：存储类失败场景演练（D3×5 + D4×3 = 8 场景；实施计划 §5 M4-02）。
 *
 * 分层说明（实施计划 M4-02）：M2/M3 验证单点行为正确（单测/集成），本任务验证
 * 端到端脚本化执行稳定 + 每场景输出「触发 → 自动应对 → 恢复」三段证据日志并归档。
 *
 *   1) disk-full        磁盘满（D3，T8）：空间护栏 → 只读；外部路径备份/空间检查；修复+重启+自检恢复
 *   2) db-corruption    库损坏（D3）：quick_check 失败 → 安全模式（只读 + 导出 + 备份入口）
 *   3) wal-growth       WAL 膨胀（D3）：>256MB 阈值 TRUNCATE；读锁失败退避重试
 *   4) write-backlog    写队列积压（D3）：>1024 告警、>4096 拒准入；临时背压自动回落（与降级区分）
 *   5) double-open-write 双开写库（D3，T12）：单实例锁阻止；第二实例无写连接
 *   6) persist-failure  持久化失败（D4）：MAX_WRITE_ATTEMPTS=3 → persist_degraded + 只读 + 拒新写入/run
 *                       + 在途 run cancelled + 未落盘不广播；恢复 = 修复 + 重启 + 启动自检
 *   7) sequencer-crash  sequencer 崩溃（D4）：重启后 seq=max+1，无重复无缺口
 *   8) readback-gap     补读缺口过大（D4）：>10k 拒绝自动补发并返回明确错误码
 *
 * 夜跑：由 .github/workflows/m4-nightly.yml 在 Windows runner 执行；可本地复跑。
 * 参数：--skip-e2e（跳过 T12 真实 WebView E2E；夜跑不使用）。
 */
import { existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot } from "../lib/exec.mjs";
import { createDrill, parseOnly, stampNow } from "../m4/lib/drill.mjs";

const cargo = bin("cargo");
const skipE2e = process.argv.includes("--skip-e2e");
const m3_04Evidence = path.join(
  repoRoot,
  "scripts",
  "test",
  ".tmp",
  "m4-02",
  `m3-04-evidence-${stampNow()}`,
);
mkdirSync(m3_04Evidence, { recursive: true });

const drill = createDrill({ task: "M4-02", title: "存储类失败场景演练（8 场景）" });

drill.define({
  id: "disk-full",
  name: "磁盘满（T8）",
  source: "D3 存储专项 / 矩阵 #5",
  trigger: "磁盘满注入：写事务 SQLITE_FULL（code 13）；另含空间护栏样本（剩余 <500MB）",
  response: "写失败重试（MAX_WRITE_ATTEMPTS=3）→ persist_degraded 只读；空间护栏 → 只读；UI 降级横幅（发送入口禁用）；备份/导出走外部路径（选择器 + 目标空间检查 ×1.2）",
  recovery: "修复外部条件（磁盘/目录/权限）+ 重启核心 + 启动自检通过 → 恢复可写；备份产物可被恢复链消费",
  steps: [
    {
      name: "m1_05_store_integration（空间护栏 → 只读；修复 + 重启 + 自检恢复）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m1_05_store_integration",
        "real_store_startup_check_failure_is_recovered_only_by_restart",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "m3_04_backup_ipc（外部路径空间检查 ×1.2 + 备份产物链）",
      command: cargo,
      args: ["test", "-p", "aether-tauri", "--test", "m3_04_backup_ipc", "--", "--nocapture"],
      env: { AETHER_M3_04_EVIDENCE_DIR: m3_04Evidence },
      markers: [
        { phase: "response", prefix: "[m3-04] 证据 dod5_space_insufficient = " },
        { phase: "recovery", prefix: "[m3-04] 证据 dod5_create_list = " },
      ],
    },
    {
      name: "降级横幅 UI（只读 + 发送禁用 + 恢复引导；vitest）",
      command: process.execPath,
      args: [
        path.join(repoRoot, "apps", "desktop", "node_modules", "vitest", "vitest.mjs"),
        "run",
        "src/m3_06_recovery.test.tsx",
      ],
      cwd: path.join(repoRoot, "apps", "desktop"),
      timeoutMs: 5 * 60 * 1000,
    },
  ],
});

drill.define({
  id: "db-corruption",
  name: "库损坏",
  source: "D3 存储专项 / 矩阵 #6",
  trigger: "损坏库样本（末页损坏 → quick_check 失败）；完整性失败样本",
  response: "拒绝写入；安全模式（只读打开 + 可读数据导出 + 备份入口可用）；主库文件不被改写",
  recovery: "从备份恢复（T9 行数 + 哈希抽查一致）；恢复后读写正常",
  steps: [
    {
      name: "m1_03_safe_mode（安全模式 + 导出 + 备份入口）",
      command: cargo,
      args: ["test", "-p", "aether-store", "--test", "m1_03_safe_mode", "--", "--nocapture"],
    },
    {
      name: "m1_05_degraded dod6_startup_check_failure_starts_read_only（完整性失败 → 只读）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m1_05_degraded",
        "dod6_startup_check_failure_starts_read_only",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "m3_04_backup（T9：备份 → 清库 → 恢复 → 行数/哈希一致）",
      command: cargo,
      args: ["test", "-p", "aether-store", "--test", "m3_04_backup", "--", "--nocapture"],
    },
  ],
});

drill.define({
  id: "wal-growth",
  name: "WAL 膨胀",
  source: "D3 存储专项 / 矩阵 #7",
  trigger: "WAL >256MB 触发运行期强制 checkpoint；读锁占用使 TRUNCATE 失败",
  response: "`wal_checkpoint(TRUNCATE)`；读锁失败 → 退避重试并记录诊断（尝试次数/错误上报）",
  recovery: "读锁释放后重试成功、WAL 归零；关闭序列五步后 `-wal` 0 字节、无残留句柄",
  steps: [
    {
      name: "m2_06_shutdown（存储侧：退避重试 + 阈值 + D2 五步 + WAL 归零）",
      command: cargo,
      args: ["test", "-p", "aether-store", "--test", "m2_06_shutdown", "--", "--nocapture"],
      markers: [
        { phase: "response", prefix: "[m2-06 DoD3]" },
        { phase: "recovery", prefix: "[m2-06 DoD2]" },
      ],
    },
    {
      name: "m1_03_pragma（wal_autocheckpoint / journal_size_limit 契约）",
      command: cargo,
      args: ["test", "-p", "aether-store", "--test", "m1_03_pragma"],
    },
    {
      name: "m2_06_shutdown（控制×存储：管线 drain 后五步顺序）",
      command: cargo,
      args: ["test", "-p", "aether-control", "--test", "m2_06_shutdown", "--", "--nocapture"],
      markers: [{ phase: "recovery", prefix: "[m2-06 管线×存储]" }],
    },
  ],
});

drill.define({
  id: "write-backlog",
  name: "写队列积压（临时背压，与持久化降级区分）",
  source: "D3 存储专项 / 矩阵 #8",
  trigger: "写队列 >1024（L1 告警）并持续增长至 >4096（L2）；控制侧慢消费者",
  response: "L1 告警；L2 拒绝新 run（storage_backpressure），已有 run 不受影响；存储侧临时高水位暂停控制读取 ≤2s → 超阈值隔离 `storage_backpressure`",
  recovery: "队列回落 ≤1024 → 自动解除（持续 30s）并重启适配器；**临时背压自动回落，与 persist_degraded（修复+重启+自检）区分**",
  steps: [
    {
      name: "m1_04_write_queue（L1/L2 告警 + 准入拒绝 + drain 回落）",
      command: cargo,
      args: ["test", "-p", "aether-store", "--test", "m1_04_write_queue", "--", "--nocapture"],
    },
    {
      name: "m2_04_backpressure dod1（L2 拒新 run；已有 run 继续；回落恢复）",
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
    {
      name: "m2_04_backpressure dod5（临时高水位隔离/自动解除；persist_degraded 不适用）",
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
      name: "m2_04_isolation（适配器侧隔离 → 解除重启，storage_backpressure）",
      command: cargo,
      args: ["test", "-p", "aether-adapters", "--test", "m2_04_isolation", "--", "--nocapture"],
    },
  ],
});

drill.define({
  id: "double-open-write",
  name: "双开写库（T12）",
  source: "D3 存储专项 / 矩阵 #9",
  trigger: "二次启动应用（同一数据目录）",
  response: "单实例锁阻止第二实例；第二进程在 setup 前退出转交 argv（无第二写连接）；首实例聚焦已有窗口",
  recovery: "单写队列唯一写者语义保持；关闭多余实例路径不存在（进程已退出）",
  steps: [
    {
      name: "m1_04_read_wal（并发读不阻塞写；WAL 生效）",
      command: cargo,
      args: ["test", "-p", "aether-store", "--test", "m1_04_read_wal", "--", "--nocapture"],
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

drill.define({
  id: "persist-failure",
  name: "持久化失败",
  source: "D4 存储/一致性专项 / 矩阵 #10",
  trigger: "写事务连续失败（盘满/损坏；真实库以关闭写队列注入）",
  response: "连续 3 次写事务尝试失败（含首次；日志 attempt=1/3…3/3）→ persist_degraded + 只读；拒绝新写入/新 run；未落盘事件不广播；在途 run cancelled",
  recovery: "修复外部条件 + 重启核心 + 启动自检（quick_check/空间检查）→ 恢复；P0 无热恢复（2 失败 1 成功不降级）",
  steps: [
    {
      name: "m1_05_degraded（重试口径三态 + 只读语义 + 在途取消 + 无热恢复）",
      command: cargo,
      args: ["test", "-p", "aether-control", "--test", "m1_05_degraded", "--", "--nocapture"],
    },
    {
      name: "m1_05_attempt_log（tracing 捕获 attempt=1/3…3/3）",
      command: cargo,
      args: ["test", "-p", "aether-control", "--test", "m1_05_attempt_log", "--", "--nocapture"],
      markers: [{ phase: "response", prefix: "captured attempt log: " }],
    },
    {
      name: "m1_05_store_integration（真实库写失败 → 降级；读仍可用）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m1_05_store_integration",
        "real_store_write_failure_degrades_while_reads_stay_available",
        "--",
        "--nocapture",
      ],
    },
  ],
});

drill.define({
  id: "sequencer-crash",
  name: "sequencer 崩溃",
  source: "D4 存储/一致性专项 / 矩阵 #11",
  trigger: "会话 sequencer 任务崩溃后重启（restart_session）",
  response: "重启后 seq 从库中 max(seq)+1 恢复；期间事件排队；未落盘 delta 丢弃",
  recovery: "无重复无缺口（continuity 断言）；duplicate_seq_bugs=0",
  steps: [
    {
      name: "m1_05_pipeline dod5（max+1 恢复 + 排队 + 无重复）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m1_05_pipeline",
        "dod5",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "m1_05_store_integration（真实库 max(seq)+1 恢复 + 补读连续）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m1_05_store_integration",
        "real_store_seq_monotonic_and_restart_resumes_from_db",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "sequencer 单元测试（单调/恢复/饱和）",
      command: cargo,
      args: ["test", "-p", "aether-control", "--lib", "sequencer"],
    },
  ],
});

drill.define({
  id: "readback-gap",
  name: "补读缺口过大",
  source: "D4 存储/一致性专项 / 矩阵 #12",
  trigger: "UI 带 last_seq 请求补读，缺口 = 10001（>10k 上限）",
  response: "拒绝自动补发，返回明确错误码 `readback_gap_too_large`（核心与 IPC 同码透传）；边界 10000 仍可补",
  recovery: "UI 收到同码 → 提示「历史消息过多」→ 确认后清缓存重载最近 N 条（不重启核心/应用）",
  steps: [
    {
      name: "m1_05_pipeline dod4（>10k 拒绝 + 边界值）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-control",
        "--test",
        "m1_05_pipeline",
        "dod4",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "m3_02_session_backend（IPC 同码透传 + 缺口守卫）",
      command: cargo,
      args: [
        "test",
        "-p",
        "aether-tauri",
        "--test",
        "m3_02_session_backend",
        "messages_page_pages_backfill_latest_and_guards_gap",
        "--",
        "--nocapture",
      ],
    },
    {
      name: "生产补读 E2E（缺口 → 确认 → 重载最近 N 条；vitest）",
      command: process.execPath,
      args: [
        path.join(repoRoot, "apps", "desktop", "node_modules", "vitest", "vitest.mjs"),
        "run",
        "src/workbenchBackfillE2E.test.tsx",
      ],
      cwd: path.join(repoRoot, "apps", "desktop"),
      timeoutMs: 5 * 60 * 1000,
    },
  ],
});

await drill.runAll({ only: parseOnly() });
process.exit(drill.summarize());
