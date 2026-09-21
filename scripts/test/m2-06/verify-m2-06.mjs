/**
 * M2-06 验证入口：关闭序列与 checkpoint 纪律（设计 D2 / D3）。
 *
 * 覆盖 DoD：
 *   1) 顺序断言（shadow 日志）：drain → 关闭全部读连接 → wal_checkpoint(TRUNCATE)
 *      → 关闭写连接 → 退出，五步与 D2 完全一致（含 drain 超时兜底路径）；
 *   2) 退出后 `-wal` 为 0 字节、无残留句柄（平台文件语义断言：重命名/改回/删除）；
 *   3) 读锁占用下强制 checkpoint 失败 → 退避重试并记录诊断（单测 + 集成），
 *      并由运行期 256MB 阈值入口收口（不触发/触发/非法配置拒绝）。
 *
 * 另含管线侧 drain 语义回归（放弃未落盘 delta、保留控制事件；M1-05 冻结语义）。
 *
 * 环境：Cargo 经 scripts/test/lib/exec.mjs 解析；存储/控制层用例跨平台。
 */
import { readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const cargo = bin("cargo");

// ===== DoD3 单测：checkpoint 配置/退避曲线/读锁退避重试与诊断（含 busy_timeout 恢复） =====

record(
  "cargo test -p aether-store --lib（checkpoint：D3 常量/退避曲线/读锁退避诊断/配置校验）",
  run(cargo, ["test", "-p", "aether-store", "--lib"]),
);

// ===== DoD1/DoD2/DoD3 集成（--nocapture 输出 shadow 日志与平台断言证据） =====

record(
  "m2_06_shutdown（DoD1 五步顺序 shadow 日志；DoD2 -wal 0 字节 + 无残留句柄；DoD3 读锁退避诊断；drain 超时兜底；256MB 运行期入口）",
  run(cargo, [
    "test",
    "-p",
    "aether-store",
    "--test",
    "m2_06_shutdown",
    "--",
    "--nocapture",
  ]),
);

record(
  "m2_06_shutdown（控制层：管线 drain 放弃 delta/保留控制事件 × 存储五步，重开仅控制事件落库）",
  run(cargo, [
    "test",
    "-p",
    "aether-control",
    "--test",
    "m2_06_shutdown",
    "--",
    "--nocapture",
  ]),
);

// ===== 回归：存储层（M1-03/M1-04/M2-01 与关停接口兼容） =====

record("回归 cargo test -p aether-store（全量）", run(cargo, ["test", "-p", "aether-store"]));

// ===== 回归：控制层（M1-05 管线/降级 + M2-01/M2-03/M2-04/M2-05） =====

record(
  "回归 cargo test -p aether-control（全量：m1_05_* / m2_01 / m2_03 / m2_04 / m2_05）",
  run(cargo, ["test", "-p", "aether-control"]),
);

// ===== 静态检查：D2 五步顺序、checkpoint 纪律与运行期阈值接线在案 =====

{
  const problems = [];

  const checkpoint = readFileSync(
    path.join(repoRoot, "crates", "aether-store", "src", "checkpoint.rs"),
    "utf8",
  );
  const checkpointRequired = [
    ["WAL_FORCE_CHECKPOINT_BYTES: u64 = 256 * 1024 * 1024", "D3：WAL >256MB 运行期阈值"],
    ['"PRAGMA wal_checkpoint(TRUNCATE)"', "TRUNCATE checkpoint 语句"],
    ["pub fn checkpoint_truncate_with_backoff", "退避重试入口"],
    ["backoff_after_failure", "指数退避曲线"],
    ["std::thread::sleep(backoff)", "退避等待（单写者串行语义）"],
    ["busy_timeout(Duration::ZERO)", "单次尝试禁用 SQLite busy 等待（由本层退避）"],
    ["busy_timeout(timeout)", "尝试结束后恢复 busy_timeout"],
    ["pub fn diagnostic_summary", "退避重试诊断（M3-05 导出源）"],
    ["pub struct CheckpointAttempt", "逐次尝试记录"],
  ];
  for (const [needle, label] of checkpointRequired) {
    if (!checkpoint.includes(needle)) problems.push(`checkpoint.rs 缺少${label}: ${needle}`);
  }

  const writeQueue = readFileSync(
    path.join(repoRoot, "crates", "aether-store", "src", "write_queue.rs"),
    "utf8",
  );
  const order = [
    "ShutdownStep::DrainWriteQueue",
    "ShutdownStep::CloseReadConnections",
    "ShutdownStep::WalCheckpointTruncate",
    "ShutdownStep::CloseWriteConnection",
    "ShutdownStep::Exit",
  ];
  const orderIndexes = order.map((needle) => writeQueue.indexOf(needle));
  if (orderIndexes.some((index) => index < 0)) {
    problems.push("write_queue.rs 缺少 D2 五步定义");
  } else {
    const sorted = [...orderIndexes].sort((a, b) => a - b);
    if (orderIndexes.join(",") !== sorted.join(",")) {
      problems.push("write_queue.rs 五步顺序必须与 D2 一致（drain → 关读连接 → checkpoint → 关写连接 → 退出）");
    }
  }
  for (const [needle, label] of [
    ["pub async fn shutdown_with", "参数化关闭序列"],
    ["self.reads.close(config.read_close_timeout)", "步骤 2：关闭全部读连接先于 checkpoint"],
    ["wal_bytes {wal_before}→{wal_after}", "shadow 日志记录 WAL 归零证据"],
    ["await_shutdown_checkpoint", "drain 后写连接待命 checkpoint（写任务退出即关写连接）"],
    ["writer.abort()", "drain 超时终止写任务（退出优先）"],
    ["pub async fn maintenance_checkpoint", "D3 运行期 256MB 强制 checkpoint 入口"],
    ["SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(3)", "D2：drain 3s 上限"],
  ]) {
    if (!writeQueue.includes(needle)) problems.push(`write_queue.rs 缺少${label}: ${needle}`);
  }

  const lib = readFileSync(path.join(repoRoot, "crates", "aether-store", "src", "lib.rs"), "utf8");
  for (const needle of ["pub mod checkpoint", "ShutdownStep", "ShutdownReport", "checkpoint_truncate_with_backoff"]) {
    if (!lib.includes(needle)) problems.push(`lib.rs 缺少导出: ${needle}`);
  }

  if (problems.length > 0) console.error(problems.join("\n"));
  record("静态检查：M2-06 D2 五步/checkpoint 退避/运行期阈值接线在案", problems.length === 0 ? 0 : 1);
}

process.exit(summarize("verify-m2-06", checks));
