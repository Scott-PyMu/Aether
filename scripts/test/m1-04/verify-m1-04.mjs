/**
 * M1-04 验证脚本：单写队列与 group commit（设计 D3；实施计划 M1-04）。
 *
 * 覆盖 DoD：
 *   1) 1k 事件写入基准：事务延迟 P95 <50ms（D3 失效条件阈值）；队列深度进诊断
 *      → m1_04_bench（release + --ignored，debug 下 bundled SQLite 为 -O0 不具代表性）；
 *   2) 批量参数生效断言（16ms 或 ≥256 条触发提交）→ m1_04_write_queue：
 *      - 默认常量逐字断言（4096 / 256 / 16ms / 4 读连接，另含 L1 1024 / L2 4096）；
 *      - 行为断言：≥256 条触发 Count 批次；3 条走 16ms 定时（Timer）批次；
 *   3) 队列 >1024 发 L1 告警事件；>4096 暴露 storage_backpressure 接口（供 M2-04 联调）
 *      → m1_04_write_queue：阈值逻辑以参数化配置验证（默认常量由 DoD2 逐字断言）；
 *      L1/L2 边沿告警、admission() 返回 storage_backpressure 错误码、回落自动恢复；
 *   4) 写入压测期间读延迟 P95 <10ms（WAL 生效证明）→ m1_04_read_wal：
 *      4 读连接池、读写重叠断言、journal_mode=wal、补读分页/断点续传语义。
 * 另：静态检查默认常量与事件插入路径；M1-02/M1-03 存储层回归。
 */
import { readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { bin, repoRoot, run, summarize } from "../lib/exec.mjs";

const cargo = bin("cargo");
const writerSource = readFileSync(
  path.join(repoRoot, "crates", "aether-store", "src", "write_queue.rs"),
  "utf8",
);
const errorSource = readFileSync(
  path.join(repoRoot, "crates", "aether-store", "src", "error.rs"),
  "utf8",
);

const checks = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });

// ===== 静态检查：设计常量字面量与关键路径 =====

try {
  const required = [
    [/QUEUE_CAPACITY:\s*usize\s*=\s*4_?096\s*;/, "D3：mpsc 队列容量常量 4096"],
    [/MAX_BATCH_ENTRIES:\s*usize\s*=\s*256\s*;/, "D3：批量提交阈值常量 256"],
    [/FLUSH_INTERVAL:\s*Duration\s*=\s*Duration::from_millis\(16\)\s*;/, "D3：16ms 提交间隔常量"],
    [/L1_THRESHOLD:\s*usize\s*=\s*1_?024\s*;/, "D8：L1 阈值常量 1024"],
    [/L2_THRESHOLD:\s*usize\s*=\s*4_?096\s*;/, "D8：L2 阈值常量 4096"],
    [/READ_CONNECTION_COUNT:\s*usize\s*=\s*4\s*;/, "D3：读连接数常量 4"],
    [/mpsc::channel\(config\.capacity\)/, "写任务经 mpsc(config.capacity) 汇聚"],
    [/INSERT INTO events\s*\(id, session_id, run_id, runtime_id, seq, type, payload, ts, v\)/, "事件插入 9 列 ↔ 信封 9 字段（D4）"],
    [/tokio::time::timeout_at\(deadline, receiver\.recv\(\)\)/, "16ms 提交窗口（group commit）"],
    [/Semaphore::new\(size\)/, "读连接池信号量限流"],
  ];
  for (const [pattern, label] of required) {
    if (!pattern.test(writerSource)) throw new Error(`write_queue.rs 缺少：${label}`);
  }
  if (!/"storage_backpressure"/.test(errorSource)) {
    throw new Error("error.rs 缺少 storage_backpressure 错误码（D8）");
  }
  console.log(
    "[static] 默认常量与关键路径在案：4096 / 256 / 16ms / 1024 / 4096 / 4 读连接 + storage_backpressure 错误码",
  );
  record("静态：write_queue.rs 常量与 D3/D8 口径一致", 0);
} catch (error) {
  console.error(`[static] 失败：${error.message}`);
  record("静态：write_queue.rs 常量与 D3/D8 口径一致", 1);
}

// ===== DoD2/DoD3：批量参数与背压分级 =====

record(
  "cargo test -p aether-store --test m1_04_write_queue（DoD2 批量参数 / DoD3 L1-L2 与准入接口）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m1_04_write_queue", "--", "--nocapture"]),
);

// ===== DoD4：WAL 读延迟 =====

record(
  "cargo test -p aether-store --test m1_04_read_wal（DoD4 写入压测期间读延迟 P95<10ms）",
  run(cargo, ["test", "-p", "aether-store", "--test", "m1_04_read_wal", "--", "--nocapture"]),
);

// ===== DoD1：1k 事件写入基准（release + ignored） =====

record(
  "cargo test --release -p aether-store --test m1_04_bench -- --ignored（DoD1 事务延迟 P95<50ms）",
  run(cargo, [
    "test",
    "--release",
    "-p",
    "aether-store",
    "--test",
    "m1_04_bench",
    "--",
    "--ignored",
    "--nocapture",
  ]),
);

// ===== 回归：M1-02 / M1-03 存储层与 M1-04 默认用例 =====

record(
  "回归：cargo test -p aether-store（M1-02 往返 / M1-03 迁移与安全模式 / M1-04 单测）",
  run(cargo, ["test", "-p", "aether-store"]),
);

process.exit(summarize("verify-m1-04", checks));
