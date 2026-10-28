#!/usr/bin/env node
/**
 * `packages/protocol/src/command-manifest.json` 生成器（M9-04 DoD-1；ADR-029 §4.6）。
 *
 * 单一事实来源（ADR-029 D-029-4）：
 * - 命令名称/顺序：`crates/aether-app/src/ipc/dispatch.rs` 的 `COMMANDS`
 *   （63 条；顺序 = HEAD `collect_commands!`）；
 * - 路由分组：ADR-029 §4.6/§16.1 表格（**唯一**分组口径）；
 * - 壳本地：`SHELL_COMMANDS`（4 条）；保留不暴露：`startup_get`/`startup_migrate`；
 * - 参数形状：`crates/aether-app/src/ipc/handlers.rs` 的严格解析
 *   （`parse_strict` → `strict`；`parse_no_params` → `no_params`；两者皆无 → `none`）。
 *
 * 计数冻结：63 = 59 daemon + 4 shell；UI 暴露 61/63；debug 探针不计入。
 *
 * 用法：
 *   node scripts/ci/command-manifest.mjs generate   # 写入生成物
 *   node scripts/ci/command-manifest.mjs check      # 逐字节比较（不写文件）
 */
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const DISPATCH_SRC = "crates/aether-app/src/ipc/dispatch.rs";
const HANDLERS_SRC = "crates/aether-app/src/ipc/handlers.rs";
const ADR_SRC = "docs/adr/ADR-029-electron-shell-rust-daemon.md";
const TARGET = "packages/protocol/src/command-manifest.json";
const EXPECTED_TOTAL = 63;
const EXPECTED_DAEMON = 59;
const EXPECTED_SHELL = 4;
const EXPECTED_UI = 61;
export const RESERVED_COMMANDS = ["startup_get", "startup_migrate"];

const MODE = process.argv.slice(2).find((arg) => !arg.startsWith("--")) ?? "check";
if (MODE !== "generate" && MODE !== "check") {
  console.error(`[command-manifest] 未知模式：${MODE}（可选 generate / check）`);
  process.exit(2);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function extractStringArray(source, constName) {
  const block = source.match(
    new RegExp(`pub const ${constName}: &\\[&str\\] = &\\[([\\s\\S]*?)\\];`),
  );
  if (!block) throw new Error(`[command-manifest] 未找到 ${constName}`);
  return [...block[1].matchAll(/"([^"]+)"/g)].map((match) => match[1]);
}

/** 按大括号配对截取函数体（只处理本仓库格式，足以覆盖 handlers）。 */
function functionBody(source, name) {
  const signature = new RegExp(`pub\\(crate\\) fn ${name}\\s*\\(`);
  const match = signature.exec(source);
  if (!match) return null;
  const brace = source.indexOf("{", match.index);
  if (brace < 0) return null;
  let depth = 0;
  for (let index = brace; index < source.length; index += 1) {
    const char = source[index];
    if (char === "{") depth += 1;
    else if (char === "}") {
      depth -= 1;
      if (depth === 0) return source.slice(brace + 1, index);
    }
  }
  return null;
}

/** 解析 ADR §16.1 分组表（行：`| 组 | 数量 | \`a\`、\`b\` |`）。 */
export function parseAdrGroups() {
  const source = read(ADR_SRC);
  const section = source.match(/### 16\.1 命令路由分类（63）([\s\S]*?)### 16\.2/);
  if (!section) throw new Error("[command-manifest] 未找到 ADR-029 §16.1");
  const groups = [];
  for (const line of section[1].split(/\r?\n/)) {
    const row = line.match(/^\|\s*([^|]+?)\s*\|\s*(\d+)\s*\|\s*(.*?)\s*\|$/);
    if (!row) continue;
    const label = row[1].trim().split("（")[0].trim();
    if (label === "组" || label === "**合计**") continue;
    // 行尾括号是注记（如 artifacts 行的「`ref_pick` 归 shell」），不是分组成员。
    const methodsText = row[3].split("（")[0].split("(")[0];
    const methods = [...methodsText.matchAll(/`([a-z0-9_]+)`/g)].map((match) => match[1]);
    if (methods.length === 0) continue;
    groups.push({ group: label, count: Number(row[2]), methods });
  }
  if (groups.length === 0) throw new Error("[command-manifest] ADR §16.1 分组表为空");
  return groups;
}

function commandParams(handlers, command) {
  const body = functionBody(handlers, command);
  if (body === null) return { style: "none", dto: null };
  const dto = body.match(/let _?request:\s*(\w+)/)?.[1] ?? null;
  if (/\bparse_no_params\s*\(/.test(body)) {
    return { style: "no_params", dto };
  }
  if (/\bparse_strict\s*\(/.test(body)) {
    return { style: "strict", dto };
  }
  if (command === "runtimes_list") return { style: "none", dto: null };
  return { style: "none", dto };
}

/** 构建清单（命令顺序 = `COMMANDS`；分组来自 ADR §16.1）。 */
export function buildCommandManifest() {
  const dispatch = read(DISPATCH_SRC);
  const handlers = read(HANDLERS_SRC);
  const commands = extractStringArray(dispatch, "COMMANDS");
  const shell = extractStringArray(dispatch, "SHELL_COMMANDS");
  const adrGroups = parseAdrGroups();

  if (commands.length !== EXPECTED_TOTAL) {
    throw new Error(`[command-manifest] COMMANDS=${commands.length} ≠ ${EXPECTED_TOTAL}`);
  }
  if (shell.length !== EXPECTED_SHELL) {
    throw new Error(`[command-manifest] SHELL_COMMANDS=${shell.length} ≠ ${EXPECTED_SHELL}`);
  }
  if (new Set(commands).size !== commands.length) {
    throw new Error("[command-manifest] COMMANDS 存在重复命令");
  }

  const groupOf = new Map();
  for (const group of adrGroups) {
    if (group.methods.length !== group.count) {
      throw new Error(
        `[command-manifest] ADR 分组 ${group.group} 声明 ${group.count} 条，实际列出 ${group.methods.length} 条`,
      );
    }
    for (const method of group.methods) {
      if (groupOf.has(method)) {
        throw new Error(`[command-manifest] 命令 ${method} 在 ADR §16.1 中重复分组`);
      }
      groupOf.set(method, group.group);
    }
  }
  for (const command of commands) {
    if (!groupOf.has(command)) {
      throw new Error(`[command-manifest] 命令 ${command} 未在 ADR §16.1 分组表中登记`);
    }
  }
  for (const method of groupOf.keys()) {
    if (!commands.includes(method)) {
      throw new Error(`[command-manifest] ADR §16.1 的 ${method} 不在 COMMANDS 清单内`);
    }
  }

  const entries = commands.map((command, index) => {
    const route = shell.includes(command) ? "shell" : "daemon";
    const exposure = RESERVED_COMMANDS.includes(command) ? "reserved" : "ui";
    return {
      index: index + 1,
      name: command,
      route,
      exposure,
      group: groupOf.get(command),
      params: commandParams(handlers, command),
    };
  });

  const daemon = entries.filter((entry) => entry.route === "daemon").length;
  const shellCount = entries.filter((entry) => entry.route === "shell").length;
  const ui = entries.filter((entry) => entry.exposure === "ui").length;
  if (daemon !== EXPECTED_DAEMON || shellCount !== EXPECTED_SHELL) {
    throw new Error(`[command-manifest] 路由计数 ${daemon}/${shellCount} ≠ 59/4`);
  }
  if (ui !== EXPECTED_UI) {
    throw new Error(`[command-manifest] UI 暴露 ${ui} ≠ 61`);
  }

  return {
    schema: "aether.command-manifest.v1",
    source: {
      commands: DISPATCH_SRC,
      params: HANDLERS_SRC,
      groups: `${ADR_SRC} §4.6/§16.1`,
    },
    counts: {
      total: commands.length,
      daemon,
      shell: shellCount,
      ui,
      reserved: RESERVED_COMMANDS.length,
      groups: adrGroups.length,
    },
    groups: adrGroups.map((group) => ({ group: group.group, count: group.count })),
    commands: entries,
  };
}

/** 稳定序列化：2 空格缩进 + 末尾换行 + LF。 */
export function renderCommandManifest(model) {
  return `${JSON.stringify(model, null, 2)}\n`;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const rendered = renderCommandManifest(buildCommandManifest());
  const targetPath = path.join(root, TARGET);
  if (MODE === "generate") {
    writeFileSync(targetPath, rendered, "utf8");
    console.log(`[command-manifest] 生成完成：${TARGET}（63/59/4；UI 61）`);
    process.exit(0);
  }
  let current;
  try {
    current = readFileSync(targetPath, "utf8");
  } catch {
    console.error(`[command-manifest] 阻断：生成物缺失（${TARGET}）。先运行 generate。`);
    process.exit(1);
  }
  if (current !== rendered) {
    console.error(`[command-manifest] 阻断：${TARGET} 与重新生成结果不一致（diff=0 要求）。`);
    process.exit(1);
  }
  console.log("[command-manifest] 通过：63/59/4；UI 61；分组与 ADR §16.1 一致；diff=0");
}
