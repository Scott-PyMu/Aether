#!/usr/bin/env node
/**
 * `pnpm verify:m10-00` —— M10-00 P2 前置 ADR 批次与基线登记的可复跑检查。
 *
 * 检查面（对应 M10-00 DoD-1～DoD-8 + M10 决策记录 v0.5 + ADR-032 v1.2）：
 *  1. ADR-031～ADR-038 文件存在、v1.0/v1.1/v1.2 已接受；
 *  2. 《设计文档》v1.33 含 ADR-031～038、命令 63→87、业务码 9→35、设置键 9→11、
 *     UI-UX 锚点 29、迁移 0010+ 22 规划位、全部新命令/事件/设置键、子 Agent 交付模式；
 *  3. P2 计划 v0.11 标注 M10-00 已执行、ADR-032 v1.2 与 M10 解锁；
 *  4. UI-UX v0.19 含 P2 锚点 7 组/18 个 testid；
 *  5. 数据契约：0001 内 `sessions.parent_session_id`、`tasks`、`workflows`、
 *     `workflow_runs`、`node_runs` 存在；
 *  6. 0001–0009 迁移与 `packages/protocol` 生成物未被本任务修改；
 *  7. 证据/评审文件存在且 DoD-1～8 有记录。
 *
 * 注意：M10-00 是「登记先行」，不实现命令；`command-manifest.json` 仍为 63，
 * `error_codes.json` 仍为 ipc=32 / business=9。本脚本只校验登记与文档一致性，
 * 不要求生成物出现 87/35。
 */
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const failures = [];
const checks = [];
const notes = [];

function fail(message) {
  failures.push(message);
}

function check(name, condition, detail = "") {
  checks.push({ name, ok: Boolean(condition), detail });
  if (!condition) fail(`${name}${detail ? `：${detail}` : ""}`);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function mustExist(rel) {
  check(`存在 ${rel}`, existsSync(path.join(root, rel)));
}

function git(args) {
  try {
    return execFileSync("git", ["-C", root, ...args], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
  } catch {
    return "";
  }
}

function gitChanged(paths) {
  return git(["status", "--porcelain", "--", ...paths]);
}

function count(text, needle) {
  return text.split(needle).length - 1;
}

// ---------- 1. 登记表 ----------
const registryPath = "docs/M10-00-登记表.json";
mustExist(registryPath);
let registry = null;
try {
  registry = JSON.parse(read(registryPath));
} catch (error) {
  fail(`登记表 JSON 解析失败：${error.message}`);
}

if (registry) {
  const commands = registry.commands ?? [];
  const events = registry.events ?? [];
  const settings = registry.settings ?? [];
  const business = registry.business_codes ?? [];
  const migrations = registry.migrations ?? [];
  const anchors = registry.uiux_anchors ?? [];
  const adrs = registry.adrs ?? [];

  check("登记 ADR 数 = 8", adrs.length === 8, `实际 ${adrs.length}`);
  check("登记新命令数 = 24", commands.length === 24, `实际 ${commands.length}`);
  check("目标命令数 = 87", registry.target?.command_count === 87);
  check("基线命令数 = 63", registry.baseline?.command_count === 63);
  check("登记业务码数 = 26", business.length === 26, `实际 ${business.length}`);
  check(
    "目标业务码数 = 35",
    registry.target?.business_codes === 35 && 9 + business.length === 35,
  );
  check("登记设置键数 = 2", settings.length === 2, `实际 ${settings.length}`);
  check("目标设置键数 = 11", registry.target?.settings_keys === 11);
  check("登记事件数 = 11", events.length === 11, `实际 ${events.length}`);
  check("登记迁移规划位 = 22", migrations.length === 22, `实际 ${migrations.length}`);
  check("登记锚点组 = 7", anchors.length === 7, `实际 ${anchors.length}`);
  const testids = anchors.flatMap((group) => group.testids ?? []);
  check("登记锚点 testid = 18", testids.length === 18, `实际 ${testids.length}`);
  check("目标锚点组 = 29", registry.target?.uiux_anchor_groups === 29);
  check("迁移规划位为 0010+", registry.target?.migration_planning_slots === "0010+");
  check("登记含 subagent_result_get", commands.some((x) => x.name === "subagent_result_get"));
  check("登记含 session_has_children", business.some((x) => x.code === "session_has_children"));

  const byTask = new Map();
  for (const command of commands) {
    byTask.set(command.task, (byTask.get(command.task) ?? 0) + 1);
    check(`命令名 ${command.name} 唯一`, commands.filter((x) => x.name === command.name).length === 1);
    check(`命令 ${command.name} 有超时`, Number.isInteger(command.timeout_ms) && command.timeout_ms > 0);
    check(`命令 ${command.name} 有 ADR`, /^ADR-03[1-8]$/.test(command.adr));
    check(`命令 ${command.name} 未实现（登记先行）`, command.implemented === false);
  }
  check("M10-01 命令 +11", byTask.get("M10-01") === 11, `实际 ${byTask.get("M10-01")}`);
  check("M10-02 命令 +5", byTask.get("M10-02") === 5, `实际 ${byTask.get("M10-02")}`);
  check("M10-03 命令 +0", !byTask.has("M10-03"), `实际 ${byTask.get("M10-03") ?? 0}`);
  check("M10-04 命令 +4", byTask.get("M10-04") === 4, `实际 ${byTask.get("M10-04")}`);
  check("M10-05 命令 +3", byTask.get("M10-05") === 3, `实际 ${byTask.get("M10-05")}`);

  for (const event of events) {
    check(
      `事件 ${event.type} 使用预留行`,
      event.type.startsWith("workflow.") || event.type.startsWith("subagent."),
    );
  }
  for (const setting of settings) {
    check(`设置键 ${setting.key} 为 ui.*`, setting.key.startsWith("ui."));
    check(`设置键 ${setting.key} 有缺省`, typeof setting.default === "string" && setting.default.length > 0);
  }
}

// ---------- 1b. 文档冻结清单 ----------
const freezePath = "docs/M10-文档冻结清单.json";
mustExist(freezePath);
let freeze = null;
if (existsSync(path.join(root, freezePath))) {
  try {
    freeze = JSON.parse(read(freezePath));
  } catch (error) {
    fail(`文档冻结清单 JSON 解析失败：${error.message}`);
  }
}
if (freeze) {
  check("冻结清单 schema", freeze.schema === "aether.m10.doc-freeze.v1");
  check("冻结清单任务 M10-00", freeze.task === "M10-00");
  check("冻结清单设计 v1.32", freeze.design_version === "v1.32");
  check("冻结清单 P2 v0.10", freeze.p2_plan_version === "v0.10");
  check("冻结清单 UI-UX v0.19", freeze.uiux_version === "v0.19");
  check(
    "冻结清单条目 = 18",
    (freeze.artifacts ?? []).length === 18,
    `实际 ${(freeze.artifacts ?? []).length}`,
  );
  for (const artifact of freeze.artifacts ?? []) {
    const absolute = path.join(root, artifact.path);
    if (!existsSync(absolute)) {
      if (artifact.path === "设计文档.md" || artifact.path === "AGENTS.md") {
        notes.push(`SKIP 冻结哈希 ${artifact.path}：本地受控文档缺失（CI checkout 不含）`);
        continue;
      }
      check(`冻结文件存在：${artifact.path}`, false);
      continue;
    }
    const actual = createHash("sha256").update(readFileSync(absolute)).digest("hex");
    check(
      `冻结哈希：${artifact.path}`,
      actual === artifact.sha256,
      `期望 ${artifact.sha256} 实际 ${actual}`,
    );
  }
  mustExist("docs/M10-文档冻结记录.md");
}

// ---------- 2. ADR 文件 ----------
const adrFiles = [
  "docs/adr/ADR-031-m10-orchestration-dsl-state-machine.md",
  "docs/adr/ADR-032-m10-subagent-spawn-isolation-injection.md",
  "docs/adr/ADR-033-m10-aetherd-http-ws-cli.md",
  "docs/adr/ADR-034-m10-import-export-format-atomicity.md",
  "docs/adr/ADR-035-m10-multimodal-attachments.md",
  "docs/adr/ADR-036-m10-i18n-theme.md",
  "docs/adr/ADR-037-m10-scheduler-priority-retry-routing.md",
  "docs/adr/ADR-038-m10-event-sharding-preplan.md",
];
for (const [index, file] of adrFiles.entries()) {
  mustExist(file);
  if (!existsSync(path.join(root, file))) continue;
  const text = read(file);
  const id = `ADR-${String(31 + index).padStart(3, "0")}`;
  check(`${file} 含编号 ${id}`, text.includes(id));
  check(`${file} 状态已接受`, text.includes("已接受"));
  check(`${file} 版本 v1.0`, text.includes("v1.0"));
  check(`${file} 含回退条件`, text.includes("回退条件"));
  check(`${file} 含评审记录`, text.includes("评审记录"));
}

// ---------- 2b. 决策记录 v0.7 澄清 ----------
const adr032 = read("docs/adr/ADR-032-m10-subagent-spawn-isolation-injection.md");
check("ADR-032 不加 subagent_results 索引", adr032.includes("不加 `(parent_session_id, created_at)` 索引"));
check("ADR-032 summary 512 字节（UTF-8）", adr032.includes("512 字节"));
check("ADR-032 Q5 不阻塞 M10-03", adr032.includes("不阻塞 M10-03"));
check("ADR-032 v1.2 交付模式", adr032.includes("message_append") && adr032.includes("origin_fork") && adr032.includes("system_replace"));
const adr033 = read("docs/adr/ADR-033-m10-aetherd-http-ws-cli.md");
check("ADR-033 最终计数 87", adr033.includes("最终 87") && !adr033.includes("最终 86"));
check("ADR-033 discovery token 64hex", adr033.includes('"<64hex>"') && !adr033.includes("<base64url>"));
check("ADR-033 Windows 使用 Tokio signal", adr033.includes("tokio::signal::windows"));
check("ADR-033 不引入 unsafe/windows-sys", adr033.includes("不引入 `unsafe`/`windows-sys`"));
const adr035 = read("docs/adr/ADR-035-m10-multimodal-attachments.md");
check("ADR-035 未签名 DMG 烟测", adr035.includes("未签名 DMG"));
const cargoToml = read("Cargo.toml");
check("Cargo rust-version = 1.85", /rust-version\s*=\s*"1\.85"/.test(cargoToml));

// ---------- 3. 设计文档（本地受控；.gitignore 排除，CI 可能缺失） ----------
const designAvailable = existsSync(path.join(root, "设计文档.md"));
const design = designAvailable ? read("设计文档.md") : "";
if (!designAvailable) {
  notes.push("SKIP 设计文档.md 标记校验：本地受控文档缺失（CI checkout 不含）");
}
function checkDesign(name, condition, detail = "") {
  if (!designAvailable) return;
  check(name, condition, detail);
}
checkDesign("设计文档版本 v1.33", design.includes("v1.33"));
for (const id of ["ADR-031", "ADR-032", "ADR-033", "ADR-034", "ADR-035", "ADR-036", "ADR-037", "ADR-038"]) {
  checkDesign(`设计文档引用 ${id}`, design.includes(id));
}
checkDesign("设计文档含 63 已实现/87 登记", design.includes("63 已实现") && design.includes("87"));
checkDesign("设计文档含业务码 9 → 35 登记", design.includes("9 → 35"));
checkDesign("设计文档含设置键 9 → 11", design.includes("9 → 11"));
checkDesign("设计文档含锚点 22 → 29", design.includes("22 → 29"));
checkDesign("设计文档含 0010+ 规划位", design.includes("0010+"));
checkDesign("设计文档含 M10 命令面登记", design.includes("M10 命令面登记"));

if (registry) {
  for (const command of registry.commands) {
    checkDesign(`设计文档含命令 ${command.name}`, design.includes(command.name));
  }
  for (const event of registry.events) {
    checkDesign(`设计文档含事件 ${event.type}`, design.includes(event.type));
  }
  for (const setting of registry.settings) {
    checkDesign(`设计文档含设置键 ${setting.key}`, design.includes(setting.key));
  }
}

// ---------- 4. P2 计划 ----------
const plan = read("docs/P2-架构迁移-实施计划与验收标准.md");
check("P2 计划版本 v0.11", plan.includes("v0.11") && plan.includes("计划版本"));
check("P2 计划 M10-00 已执行", plan.includes("M10-00 已执行"));
check("P2 计划 M10 解锁", plan.includes("M10-01～M10-08 解锁"));
check("P2 计划含 verify:m10-00", plan.includes("verify:m10-00"));
for (const id of ["ADR-031", "ADR-032", "ADR-033", "ADR-034", "ADR-035", "ADR-036", "ADR-037", "ADR-038"]) {
  check(`P2 计划引用 ${id}`, plan.includes(id));
}

// ---------- 5. UI-UX ----------
const uiux = read("docs/UI-UX-设计规格.md");
check("UI-UX 版本 v0.19", uiux.includes("v0.19"));
check("UI-UX 锚点 29", uiux.includes("29"));
check("UI-UX 含 C11 i18n/主题", uiux.includes("C11"));
for (const anchor of registry?.uiux_anchors ?? []) {
  for (const testid of anchor.testids) {
    check(`UI-UX 含锚点 ${testid}`, uiux.includes(testid));
  }
}

// ---------- 6. 数据契约（0001） ----------
const migration = read("migrations/0001_init.sql");
check("数据契约 sessions.parent_session_id", /parent_session_id\s+TEXT\s+REFERENCES\s+sessions\(id\)/.test(migration));
for (const table of ["tasks", "workflows", "workflow_runs", "node_runs"]) {
  check(`数据契约表 ${table}`, new RegExp(`CREATE TABLE ${table}\\s*\\(`).test(migration));
}
check("数据契约 workflow_runs.workflow_id", migration.includes("workflow_id TEXT NOT NULL REFERENCES workflows(id)"));
check("数据契约 node_runs.attempt", migration.includes("attempt         INTEGER NOT NULL DEFAULT 1"));
check("数据契约 messages.content_parts", migration.includes("content_parts     TEXT"));

// ---------- 7. 冻结面未被修改 ----------
const frozen = [
  "migrations/0001_init.sql",
  "migrations/0002_unique_keys.sql",
  "migrations/0003_p0_ui_extensions.sql",
  "migrations/0004_permission_mode_and_rules.sql",
  "migrations/0005_audit_indexes.sql",
  "migrations/0006_runtime_enabled.sql",
  "migrations/0007_presets.sql",
  "migrations/0008_session_search_fts.sql",
  "migrations/0009_provider_endpoints.sql",
  "packages/protocol/src/types.ts",
  "packages/protocol/src/client.ts",
  "packages/protocol/src/command-manifest.json",
  "packages/protocol/src/error_codes.json",
];
// 决策记录 v0.5：Cargo.toml/Cargo.lock/deny.toml/pnpm-lock.yaml 在 M10-03/05
// 依赖落地时由 verify:m10-03/05 校验“精确版本 + 许可证”，不再由 M10-00 冻结。
const changed = gitChanged(frozen);
check("冻结面（迁移/生成物）未被修改", changed === "", changed);

// ---------- 8. 证据/评审 ----------
mustExist("docs/M10-00-证据.md");
mustExist("docs/M10-00-ADR批次评审.md");
if (existsSync(path.join(root, "docs/M10-00-证据.md"))) {
  const evidence = read("docs/M10-00-证据.md");
  for (let dod = 1; dod <= 8; dod += 1) {
    check(`证据记录 DoD-${dod}`, new RegExp(`DoD-${dod}`).test(evidence));
  }
}

// ---------- 输出 ----------
const passed = checks.filter((item) => item.ok).length;
console.log(`M10-00 校验：${passed}/${checks.length} 项通过`);
for (const note of notes) console.log(`NOTE  ${note}`);
if (failures.length > 0) {
  console.error(`\n失败 ${failures.length} 项：`);
  for (const message of failures) console.error(` - ${message}`);
  process.exit(1);
}
console.log("M10-00 登记/版本/迁移/锚点/生成物一致性：PASS");
