#!/usr/bin/env node
/**
 * M10 文档冻结清单生成器（一次性/复核用）。
 *
 * 读取冻结文档列表，按字节计算 SHA256，写出 `docs/M10-文档冻结清单.json`。
 * `pnpm verify:m10-00` 会重新计算并比对，防止冻结后被静默修改。
 */
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const artifacts = [
  "设计文档.md",
  "AGENTS.md",
  "README.md",
  "docs/P2-架构迁移-实施计划与验收标准.md",
  "docs/UI-UX-设计规格.md",
  "docs/adr/ADR-031-m10-orchestration-dsl-state-machine.md",
  "docs/adr/ADR-032-m10-subagent-spawn-isolation-injection.md",
  "docs/adr/ADR-033-m10-aetherd-http-ws-cli.md",
  "docs/adr/ADR-034-m10-import-export-format-atomicity.md",
  "docs/adr/ADR-035-m10-multimodal-attachments.md",
  "docs/adr/ADR-036-m10-i18n-theme.md",
  "docs/adr/ADR-037-m10-scheduler-priority-retry-routing.md",
  "docs/adr/ADR-038-m10-event-sharding-preplan.md",
  "docs/M10-00-登记表.json",
  "docs/M10-00-ADR批次评审.md",
  "docs/M10-00-证据.md",
  "docs/M10-方案决策记录与修订建议.md",
  "docs/M10-实现方案-评审稿.md",
];

const manifest = {
  schema: "aether.m10.doc-freeze.v1",
  task: "M10-00",
  frozen_at: "2026-11-05",
  design_version: "v1.32",
  p2_plan_version: "v0.10",
  uiux_version: "v0.19",
  artifacts: artifacts.map((rel) => {
    const bytes = readFileSync(path.join(root, rel));
    return {
      path: rel,
      sha256: createHash("sha256").update(bytes).digest("hex"),
    };
  }),
};

const target = path.join(root, "docs", "M10-文档冻结清单.json");
writeFileSync(target, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
console.log(`[m10-freeze] 写入 ${path.relative(root, target)}（${manifest.artifacts.length} 项）`);
