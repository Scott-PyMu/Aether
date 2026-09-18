/**
 * M1-05 验证脚本：事件管线（设计 D4；实施计划 M1-05）。
 *
 * 覆盖 DoD：
 *   1) 乱序/重复注入 10k 事件，seq 单调唯一
 *      → m1_05_pipeline: dod1_*（10k + 第二组种子属性复跑；到达顺序=落盘顺序）；
 *   2) 故障注入 journal 写失败：重试 3 次失败 → persist_degraded + 只读；拒绝新写入/新 run；
 *      未落盘不广播（调用序断言）；在途 run 转 cancelled
 *      → m1_05_degraded: dod2_*（含降级通知落盘失败分支、重复 seq 管线 bug、跨会话 delta 丢弃）；
 *      → m1_05_store_integration: 真实写队列关闭注入（读路径保持可用、无部分写入）；
 *   3) delta 合并阈值生效；message.completed 终稿不受影响
 *      → m1_05_pipeline: dod3_*（16ms 窗口 / 8KB 阈值 / completed 保序冲刷）；
 *      → m1_05_store_integration: 真实库合并行与终稿回读一致；
 *   4) 补读 last_seq；>10k 拒绝并返回错误码
 *      → m1_05_pipeline: dod4_*（缺口 10_000 补齐 / 10_001 返回 readback_gap_too_large）；
 *   5) sequencer 崩溃后 seq=max+1
 *      → m1_05_pipeline: dod5_*（恢复无重复无缺口；恢复期间事件排队）；
 *      → m1_05_store_integration: 真实库 max_seq 续接；
 *   6) 降级进入/退出断言；写队列临时高水位不得进入本状态；降级通知经 health 返回
 *      → m1_05_degraded: dod6_*（写失败/空间护栏/完整性失败进入；重启 + 自检通过退出；
 *        L2 高水位保持 normal；health.storage_state=persist_degraded）；
 *      → m1_05_store_integration: 启动自检失败 → 只读 → 重启恢复。
 *
 * 另有静态检查：D4 常量（3 次重试 / 16ms / 8KB / 10k 上限 / 4096 队列与广播）、
 * 先日志后广播调用点、`persist_degraded`/`readback_gap_too_large` 错误码、
 * 核心 crate 无 unwrap/expect/panic 逃逸（src 非测试代码）。
 *
 * 证据归档：`scripts/test/.tmp/m1-05/<timestamp>/`（逐命令 stdout/stderr + summary.txt）。
 */
import { spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { bin, repoRoot, summarize } from "../lib/exec.mjs";

const cargo = bin("cargo");
const controlSrc = path.join(repoRoot, "crates", "aether-control", "src");
const outDir = path.join(
  repoRoot,
  "scripts",
  "test",
  ".tmp",
  "m1-05",
  new Date().toISOString().replace(/[:.]/g, "-"),
);
mkdirSync(outDir, { recursive: true });

const checks = [];
const evidence = [];
const record = (name, exit, expect = 0) => checks.push({ name, exit, expect });
const readSource = (name) => readFileSync(path.join(controlSrc, name), "utf8");

/** 执行命令并归档 stdout/stderr 到证据目录。 */
function runCapture(name, command, args) {
  const result = spawnSync(command, args, {
    cwd: repoRoot,
    encoding: "utf8",
    maxBuffer: 128 * 1024 * 1024,
  });
  const output = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  const file = `${String(evidence.length + 1).padStart(2, "0")}-${name}.txt`;
  writeFileSync(path.join(outDir, file), `$ ${command} ${args.join(" ")}\n\n${output}`, "utf8");
  evidence.push(file);
  console.log(output.trimEnd());
  console.log(`[exec] exit=${result.status ?? 1} → 证据 ${file}`);
  return result.status ?? 1;
}

function cargoTest(name, args) {
  return runCapture(name, cargo, args);
}

// ===== 静态检查 1：D4 常量与关键实现点 =====

try {
  const pipeline = readSource("pipeline.rs");
  const delta = readSource("delta.rs");
  const error = readSource("error.rs");
  const storage = readSource("storage_state.rs");
  const required = [
    [pipeline, /PERSIST_ATTEMPTS:\s*usize\s*=\s*3\s*;/, "D4：写事务重试 3 次常量"],
    [pipeline, /READBACK_MAX_GAP:\s*u64\s*=\s*10_?000\s*;/, "D4：补读上限 10k 常量"],
    [pipeline, /BROADCAST_CAPACITY:\s*usize\s*=\s*4_?096\s*;/, "D8：broadcast(4096)"],
    [pipeline, /SUBMIT_QUEUE_CAPACITY:\s*usize\s*=\s*4_?096\s*;/, "管线入站队列 4096"],
    [pipeline, /READBACK_PAGE_SIZE:\s*usize\s*=\s*500\s*;/, "D7：补读分页 ≤500"],
    [delta, /DELTA_FLUSH_INTERVAL:\s*Duration\s*=\s*Duration::from_millis\(16\)\s*;/, "D4：delta 合并 16ms"],
    [delta, /DELTA_FLUSH_BYTES:\s*usize\s*=\s*8\s*\*\s*1024\s*;/, "D4：delta 合并 8KB"],
    [pipeline, /let _ = self\.events\.send\(envelope\);/, "广播调用点"],
    [pipeline, /self\.journal\.append\(events\.clone\(\)\)\.await/, "journal 落盘调用点"],
    [pipeline, /DuplicateSeq/, "DB UNIQUE 兜底分类"],
    [storage, /PersistDegraded => "persist_degraded"/, "health storage_state 取值"],
    [storage, /SPACE_GUARD_MIN_FREE_BYTES:\s*u64\s*=\s*500\s*\*\s*1024\s*\*\s*1024\s*;/, "D3：空间护栏 500MB"],
    [error, /readback_gap_too_large/, "补读错误码"],
    [error, /persist_degraded/, "降级错误码"],
  ];
  for (const [source, pattern, label] of required) {
    if (!pattern.test(source)) throw new Error(`缺少：${label}`);
  }
  // 先日志后广播：广播只出现在 persist_pending/enter_degraded（append 成功之后）。
  const broadcasts = [...pipeline.matchAll(/self\.events\.send\(/g)].length;
  if (broadcasts !== 2) {
    throw new Error(`events.send 调用点应为 2 处（落盘成功后/降级通知落盘成功后），实际 ${broadcasts}`);
  }
  const staticReport =
    "D4 常量与关键路径在案：3 次重试 / 16ms / 8KB / 10k 上限 / 4096 广播 / 500MB 护栏 / 先日志后广播";
  writeFileSync(path.join(outDir, "static-constants.txt"), `${staticReport}\n`, "utf8");
  evidence.push("static-constants.txt");
  console.log(`[static] ${staticReport}`);
  record("静态：D4 常量与先日志后广播调用点", 0);
} catch (error) {
  console.error(`[static] 失败：${error.message}`);
  record("静态：D4 常量与先日志后广播调用点", 1);
}

// ===== 静态检查 2：核心 crate 非测试代码无 unwrap/expect/panic 逃逸 =====

try {
  const violations = [];
  for (const name of readdirSync(controlSrc).filter((entry) => entry.endsWith(".rs"))) {
    const text = readSource(name);
    // 截断到第一个 #[cfg(test)]（测试代码允许 unwrap/expect/panic；库级已显式豁免）。
    const testStart = text.indexOf("#[cfg(test)]");
    const production = testStart === -1 ? text : text.slice(0, testStart);
    // 去掉行注释与文档注释（AGENTS.md 引文不构成逃逸）。
    const codeLines = production
      .split(/\r?\n/)
      .map((line) => {
        const trimmed = line.trimStart();
        if (trimmed.startsWith("//")) return "";
        const comment = line.indexOf("//");
        return comment === -1 ? line : line.slice(0, comment);
      })
      .join("\n");
    const matches = codeLines.match(/\.unwrap\(\)|\.expect\(|panic!\(/g) ?? [];
    if (matches.length > 0) violations.push(`${name}: ${matches.join(", ")}`);
  }
  if (violations.length > 0) throw new Error(`生产代码出现 wrap 逃逸：${violations.join("；")}`);
  const staticReport = "aether-control 生产代码 0 处 unwrap/expect/panic";
  writeFileSync(path.join(outDir, "static-wrap-scan.txt"), `${staticReport}\n`, "utf8");
  evidence.push("static-wrap-scan.txt");
  console.log(`[static] ${staticReport}`);
  record("静态：核心 crate 生产代码无 unwrap/expect/panic", 0);
} catch (error) {
  console.error(`[static] 失败：${error.message}`);
  record("静态：核心 crate 生产代码无 unwrap/expect/panic", 1);
}

// ===== DoD1/3/4/5 + 先日志后广播：管线集成测试 =====

record(
  "cargo test -p aether-control --test m1_05_pipeline（DoD1/3/4/5 与调用序断言）",
  cargoTest("m1-05-pipeline", [
    "test",
    "-p",
    "aether-control",
    "--test",
    "m1_05_pipeline",
    "--",
    "--nocapture",
  ]),
);

// ===== DoD2/6：故障注入与降级状态机 =====

record(
  "cargo test -p aether-control --test m1_05_degraded（DoD2/6 故障注入与降级断言）",
  cargoTest("m1-05-degraded-fault-injection", [
    "test",
    "-p",
    "aether-control",
    "--test",
    "m1_05_degraded",
    "--",
    "--nocapture",
  ]),
);

// ===== 真实存储集成（真实 SQLite 写队列/读连接池） =====

record(
  "cargo test -p aether-control --test m1_05_store_integration（真实写失败注入与回读）",
  cargoTest("m1-05-store-integration", [
    "test",
    "-p",
    "aether-control",
    "--test",
    "m1_05_store_integration",
    "--",
    "--nocapture",
  ]),
);

// ===== 回归：aether-control 单测 + aether-store 全量（M1-02/03/04） =====

record(
  "回归：cargo test -p aether-control（含 normalizer/sequencer/delta/状态机单测）",
  cargoTest("regression-aether-control", ["test", "-p", "aether-control", "--lib"]),
);
record(
  "回归：cargo test -p aether-store（M1-02 往返 / M1-03 迁移与约束 / M1-04 写队列）",
  cargoTest("regression-aether-store", ["test", "-p", "aether-store"]),
);

const exit = summarize("verify-m1-05", checks);
writeFileSync(
  path.join(outDir, "summary.txt"),
  [
    "M1-05 事件管线（设计 D4）验证证据",
    `时间：${new Date().toISOString()}`,
    "",
    ...checks.map(
      (check) =>
        `${check.exit === check.expect ? "PASS" : "FAIL"}  期望=${check.expect} 实际=${check.exit}  ${check.name}`,
    ),
    "",
    `证据文件：${evidence.join(", ")}`,
    "",
    `结论：${exit === 0 ? "全部通过" : "存在失败项"}`,
  ].join("\n") + "\n",
  "utf8",
);
console.log(`[m1-05] 证据目录：${path.relative(repoRoot, outDir)}`);
process.exit(exit);
