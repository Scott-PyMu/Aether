/**
 * M1-03 历史重复键审计（DoD5；ADR-003 §6 / ADR-005 §5-4）。
 *
 * 在 0002 增量迁移落地前，只读统计既有库的重复键：
 *   * `events(session_id, seq)`；
 *   * `messages(session_id, client_msg_id)`（列不存在时标注 pre-0002 schema）。
 *
 * 只读保证：通过 Rust 侧 `audit_duplicates` 示例以 `SQLITE_OPEN_READ_ONLY` 打开库，
 * 本脚本不写入数据库；报告可经 `--out` 落盘归档。
 *
 * 用法：
 *   node scripts/test/m1-03/audit-duplicate-keys.mjs <db 路径> [--out <报告路径>]
 *
 * 退出码：0 = 无重复；2 = 发现重复（迁移前必须先处置）；1 = 用法/读库错误。
 */
import { spawnSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot } from "../lib/exec.mjs";

const args = process.argv.slice(2);
const db = args.find((arg) => !arg.startsWith("--"));
const outIndex = args.indexOf("--out");
const out = outIndex >= 0 ? args[outIndex + 1] : null;

if (!db) {
  console.error("用法: node scripts/test/m1-03/audit-duplicate-keys.mjs <db 路径> [--out <报告路径>]");
  process.exit(1);
}

const result = spawnSync(
  bin("cargo"),
  ["run", "--quiet", "-p", "aether-store", "--example", "audit_duplicates", "--", "report", db],
  { cwd: repoRoot, encoding: "utf8", maxBuffer: 16 * 1024 * 1024, shell: false },
);

if (result.error) {
  console.error(`[audit] 无法运行审计器：${result.error.message}`);
  process.exit(1);
}
if (result.status !== 0 && result.status !== 2) {
  console.error(`[audit] 审计失败（exit=${result.status}）\n${result.stderr ?? ""}`);
  process.exit(1);
}

const stdout = (result.stdout ?? "").trim();
const firstBrace = stdout.indexOf("{");
if (firstBrace < 0) {
  console.error(`[audit] 审计输出不是 JSON：${stdout.slice(0, 300)}`);
  process.exit(1);
}
const report = JSON.parse(stdout.slice(firstBrace));

console.log(`[audit] 库：${report.database}`);
console.log(
  `[audit] schema：client_msg_id=${report.schema.messages_client_msg_id}，` +
    `events UNIQUE=${report.schema.events_unique_session_seq}，` +
    `messages UNIQUE=${report.schema.messages_unique_client_msg}`,
);
console.log(
  `[audit] events 重复组 ${report.events.duplicate_groups} / 多余行 ${report.events.duplicate_rows}` +
    (report.events.top.length > 0 ? `；Top: ${JSON.stringify(report.events.top.slice(0, 5))}` : ""),
);
console.log(
  `[audit] messages 重复组 ${report.messages.duplicate_groups} / 多余行 ${report.messages.duplicate_rows}` +
    (report.messages.checked === false ? `（${report.messages.note}）` : "") +
    (report.messages.top.length > 0
      ? `；Top: ${JSON.stringify(report.messages.top.slice(0, 5))}`
      : ""),
);

if (out) {
  const target = path.resolve(out);
  writeFileSync(target, `${JSON.stringify(report, null, 2)}\n`, "utf8");
  console.log(`[audit] 报告已写入 ${target}`);
}

if (report.ok) {
  console.log("[audit] 结论：无重复键，可执行 0002 增量迁移");
  process.exit(0);
}
console.error("[audit] 结论：存在重复键；请先人工处置，否则 0002 CREATE UNIQUE INDEX 将失败回滚");
process.exit(2);
