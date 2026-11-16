#!/usr/bin/env node
/**
 * 将 `dist-out/m10-09/bench-600s.json` 的标准 600s 结果合并到
 * `docs/M10-09-metrics.json`（保留短基准与故障注入记录）。
 *
 * 用法：
 *   node scripts/test/m10-09/merge-bench.mjs
 *   node scripts/test/m10-09/merge-bench.mjs --bench <path> --metrics <path>
 */
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const options = {
  bench: path.join(root, "dist-out", "m10-09", "bench-600s.json"),
  metrics: path.join(root, "docs", "M10-09-metrics.json"),
};
for (let index = 2; index < process.argv.length; index += 1) {
  if (process.argv[index] === "--bench") options.bench = process.argv[++index];
  else if (process.argv[index] === "--metrics") options.metrics = process.argv[++index];
}
if (!existsSync(options.bench)) {
  throw new Error(`标准基准结果不存在：${options.bench}`);
}
const bench = JSON.parse(readFileSync(options.bench, "utf8"));
const metrics = JSON.parse(readFileSync(options.metrics, "utf8"));
if (bench.failures !== 0) {
  throw new Error(`标准基准失败数 ${bench.failures}，拒绝写入 metrics`);
}
if (bench.write_p95_ms > 50) {
  throw new Error(`写 P95 ${bench.write_p95_ms}ms > 50ms，拒绝写入 metrics`);
}
if (bench.read_p95_ms >= 150) {
  throw new Error(`events_page P95 ${bench.read_p95_ms}ms >= 150ms，拒绝写入 metrics`);
}

metrics.standard_600s = {
  concurrency: bench.concurrency,
  duration_seconds: bench.duration_seconds,
  profile: bench.profile,
  data_dir: bench.dataset?.data_dir,
  write_samples: bench.write_samples,
  write_p50_ms: bench.write_p50_ms,
  write_p95_ms: bench.write_p95_ms,
  write_max_ms: bench.write_max_ms,
  read_samples: bench.read_samples,
  read_p50_ms: bench.read_p50_ms,
  read_p95_ms: bench.read_p95_ms,
  read_max_ms: bench.read_max_ms,
  failures: bench.failures,
  main_events: bench.main_events,
  clean_shutdown: bench.clean_shutdown,
};
metrics.write_p95_ms = bench.write_p95_ms;
metrics.write_samples = bench.write_samples;
metrics.write_max_ms = bench.write_max_ms;
metrics.write_duration_seconds = bench.duration_seconds;
metrics.events_page_p95_ms = bench.read_p95_ms;
metrics.events_page_samples = bench.read_samples;
metrics.main_events_after_write = bench.main_events;
metrics.unverified = (metrics.unverified ?? []).filter(
  (entry) =>
    !String(entry).includes("10 concurrent x 600s") &&
    !String(entry).includes("5.22GiB/20M rows events_page"),
);
writeFileSync(options.metrics, `${JSON.stringify(metrics, null, 2)}\n`, "utf8");
console.log(
  `[m10-09-metrics] 写入 standard_600s：write_p95=${bench.write_p95_ms}ms ` +
    `read_p95=${bench.read_p95_ms}ms failures=${bench.failures}`,
);
