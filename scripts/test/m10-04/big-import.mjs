#!/usr/bin/env node
/**
 * M10-04 ≥1GB 导入样本复跑入口（ADR-034 D-034-2/D-034-5）。
 *
 * 用法：
 *   node scripts/test/m10-04/big-import.mjs [MiB]
 *   AETHER_M10_04_BIG_MIB=1024 node scripts/test/m10-04/big-import.mjs
 *
 * 说明：默认 1024 MiB；实际执行 `m10_04_import::big_import_sample_is_streamed`
 * （ignored 测试）。会写入临时目录并在测试结束清理；需要足够磁盘空间与内存。
 */
import { spawnSync } from "node:child_process";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const mib = process.argv.slice(2).find((arg) => !arg.startsWith("-")) ?? process.env.AETHER_M10_04_BIG_MIB ?? "1024";
const cargo = process.env.AETHER_CARGO ?? (existsSync(path.join(os.homedir(), ".cargo", "bin", "cargo.exe")) ? path.join(os.homedir(), ".cargo", "bin", "cargo.exe") : "cargo");

console.log(`[m10-04] big import: ${mib} MiB`);
const result = spawnSync(
  cargo,
  [
    "test",
    "-p",
    "aether-store",
    "--test",
    "m10_04_import",
    "-j",
    "1",
    "--",
    "--ignored",
    "--test-threads=1",
    "--nocapture",
    "big_import_sample_is_streamed",
  ],
  {
    cwd: root,
    stdio: "inherit",
    env: { ...process.env, AETHER_M10_04_BIG_MIB: String(mib) },
  },
);
if (result.error) {
  console.error(`[m10-04] 无法启动 cargo：${result.error.message}`);
  process.exit(127);
}
process.exit(result.status ?? 1);
