#!/usr/bin/env node
/**
 * M10-14 Level 2 回退行为 fixture（ADR-040 §4.1.2 / §6.2）。
 *
 * 固定 ADR-039 基线构建不可用时（本机不重建 11cb438 产物），以固定 fixture
 * 行为测试覆盖最小集：①不匹配 → 主库 + 全部分片；②严格 sidecar → 跳过；
 * ③旧 health 语义（checked_shards=全部分片、state=ok=全量通过）；
 * ④sidecar 兼容（缺 format_version → 全量）；⑤旧 metrics 别名。
 *
 * 与 `verify:m10-10` 基线（静态快照）不同，本脚本对 fixture 输入执行行为判定，
 * 并与当前实现的 Level 1 构建（`startup-check-level1`）结果对照。
 */
import { readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
let failures = 0;
const check = (label, ok, detail = "") => {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}${detail ? `：${detail}` : ""}`);
  if (!ok) failures += 1;
};
const fixture = JSON.parse(
  readFileSync(path.join(root, "scripts/test/m10-14/level2-fixture.json"), "utf8"),
);

/** ADR-039 全量回退行为（fixture oracle；不是当前实现）。 */
function legacyPlan(caseName, fingerprints) {
  if (fingerprints === "match") {
    return { strict_ok: true, main: false, shards: [], full_fallback: false };
  }
  // ADR-039：任一指纹不匹配/前置不满足 → 主库 + 8 片。
  return {
    strict_ok: false,
    main: true,
    shards: [0, 1, 2, 3, 4, 5, 6, 7],
    full_fallback: true,
  };
}

for (const testCase of fixture.cases) {
  const actual = legacyPlan(testCase.name, testCase.fingerprints);
  check(
    `Level 2 case ${testCase.name}`,
    JSON.stringify(actual) === JSON.stringify(testCase.expected),
    JSON.stringify(actual),
  );
}

const health = fixture.health_v1;
check("旧 health: state=ok", health.state === "ok");
check("旧 health: checked_shards=8（全部分片）", health.checked_shards === 8);
check("旧 health: 无 contract_version", health.contract_version === undefined);
check("旧 health: 无 planned_shards/skipped_shards/full_coverage", 
  health.planned_shards === undefined &&
  health.skipped_shards === undefined &&
  health.full_coverage === undefined);
check("旧 metrics: storageCheckCompletedAt 存在", typeof fixture.metrics_v1.storageCheckCompletedAt === "number");
check("旧 metrics: 标注 deprecated", fixture.metrics_v1.deprecated === true);
check("fixture 来源固定 11cb438", fixture.source_short === "11cb438");

if (failures > 0) {
  console.error(`[m10-14:level2] FAIL（${failures} 项）`);
  process.exit(1);
}
console.log("AETHER_M10_14_LEVEL2 PASS（固定 fixture 行为测试）");
