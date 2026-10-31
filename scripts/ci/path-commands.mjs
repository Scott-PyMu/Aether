#!/usr/bin/env node
/**
 * 路径参数命令清单生成器（M9-10 DoD-5；ADR-029 §5.3/P1-8）。
 *
 * 单一事实来源：
 * - 命令 → 请求 DTO：`crates/aether-app/src/ipc/handlers.rs`（严格解析入口）；
 * - 路径字段：`crates/aether-app/src/ipc/dto.rs` 的 DTO 字段（`path` / `root_path` /
 *   `target_dir`，以及 `BackupSource::external.path`）；
 * - 必有命令：ADR-029 §5.3 的 P1-8 列表；
 * - 已复核全集：本文件 `REVIEWED_PATH_COMMANDS`（新增路径命令必须先评审并更新
 *   本清单，否则 check 失败——防止「新增路径命令漏过 canonicalize/白名单」）。
 *
 * 用法：
 *   node scripts/ci/path-commands.mjs generate   # 写入 packages/protocol/src/path-commands.json
 *   node scripts/ci/path-commands.mjs check      # 逐字节比较（不写文件）
 */
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const HANDLERS_SRC = "crates/aether-app/src/ipc/handlers.rs";
const DTO_SRC = "crates/aether-app/src/ipc/dto.rs";
const ADR_SRC = "docs/adr/ADR-029-electron-shell-rust-daemon.md";
const TARGET = "packages/protocol/src/path-commands.json";

/** 路径字段名（DTO 字段；`BackupSource` 另有 `external.path`）。 */
const PATH_FIELD_NAMES = new Set(["path", "root_path", "target_dir"]);
/** 已复核的路径参数命令全集（新增/删除须走评审；与 DTO 生成结果必须一致）。 */
const REVIEWED_PATH_COMMANDS = [
  "artifact_add",
  "workspace_set",
  "workspace_create",
  "startup_migrate",
  "backup_create",
  "backup_restore",
  "export_diagnostics",
  "session_export",
  "archive_export",
  "audit_export",
];
/** 校验调用 → 结构化错误码（与 DTO 实现一致）。 */
const VALIDATOR_ERROR = [
  ["resolve_path", "artifact_path_rejected"],
  ["canonical_external_path", "path_rejected"],
  ["canonical_target_dir", "path_rejected"],
  ["canonical_root_path", "path_rejected"],
  ["migrate(", "path_rejected"],
];

const MODE = process.argv.slice(2).find((arg) => !arg.startsWith("--")) ?? "check";
if (MODE !== "generate" && MODE !== "check") {
  console.error(`[path-commands] 未知模式：${MODE}（可选 generate / check）`);
  process.exit(2);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

/** 按大括号配对截取函数体（只处理本仓库格式）。 */
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

/** 按大括号配对截取任意 `struct Name { ... }` / `enum Name { ... }` 体。 */
function typeBody(source, kind, name) {
  const signature = new RegExp(`${kind} ${name}\\s*(?:<[^>]*>)?\\s*\\{`);
  const match = signature.exec(source);
  if (!match) return null;
  const brace = source.indexOf("{", match.index);
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

/** DTO 字段：`pub name: Type,`（忽略注解与注释）。 */
function structFields(body) {
  const fields = [];
  for (const line of body.split(/\r?\n/)) {
    const match = /^\s*pub\s+([a-z0-9_]+)\s*:\s*([^,]+),/.exec(line);
    if (match) {
      fields.push({ name: match[1], type: match[2].trim() });
    }
  }
  return fields;
}

/** `BackupSource::External { path }` 的路径字段（枚举体）。 */
function enumPathFields(dtoSource) {
  const body = typeBody(dtoSource, "enum", "BackupSource");
  if (body === null) return [];
  const external = /External\s*\{([^}]*)\}/s.exec(body);
  if (external === null) return [];
  return [...external[1].matchAll(/([a-z0-9_]+)\s*:\s*([^,}]+)/g)]
    .filter((match) => PATH_FIELD_NAMES.has(match[1]))
    .map((match) => ({ name: match[1], type: match[2].trim(), variant: "external" }));
}

/** ADR §5.3 的 P1-8 必有命令（反引号列表 ∩ 63 命令清单）。 */
function adrRequiredCommands() {
  const source = read(ADR_SRC);
  const section = /### 5\.3 daemon 侧([\s\S]*?)### 5\.4/.exec(source);
  if (section === null) throw new Error("[path-commands] 未找到 ADR-029 §5.3");
  const dispatch = read("crates/aether-app/src/ipc/dispatch.rs");
  const commands = new Set(
    [...(/pub const COMMANDS: &\[&str\] = &\[([\s\S]*?)\];/.exec(dispatch)?.[1] ?? "")
      .matchAll(/"([^"]+)"/g)]
      .map((match) => match[1]),
  );
  const list = /至少含([\s\S]*?)；/.exec(section[1]);
  if (list === null) throw new Error("[path-commands] ADR §5.3 未找到「至少含」清单");
  const names = [...list[1].matchAll(/`([a-z][a-z0-9_]+)`/g)].map((match) => match[1]);
  const required = [...new Set(names)].filter((name) => commands.has(name));
  if (required.length === 0) throw new Error("[path-commands] ADR §5.3 未列出路径命令");
  return required;
}

export function buildPathCommands() {
  const handlers = read(HANDLERS_SRC);
  const dto = read(DTO_SRC);
  const required = adrRequiredCommands();
  const backupPathFields = enumPathFields(dto);
  const commands = [];

  for (const name of REVIEWED_PATH_COMMANDS) {
    const body = functionBody(handlers, name);
    if (body === null) {
      throw new Error(`[path-commands] handlers.rs 缺少命令 ${name}`);
    }
    const dtoName = /let _?request:\s*(\w+)/.exec(body)?.[1];
    if (dtoName === undefined) {
      throw new Error(`[path-commands] 命令 ${name} 未找到请求 DTO`);
    }
    const fields = structFields(typeBody(dto, "struct", dtoName) ?? "");
    const pathFields = [];
    for (const field of fields) {
      if (PATH_FIELD_NAMES.has(field.name)) {
        pathFields.push(field);
      } else if (field.type === "BackupSource") {
        pathFields.push(...backupPathFields);
      }
    }
    if (pathFields.length === 0) {
      throw new Error(`[path-commands] ${name}（${dtoName}）未检测到路径字段`);
    }
    const validator = VALIDATOR_ERROR.find(([needle]) => body.includes(needle));
    if (validator === undefined) {
      throw new Error(
        `[path-commands] ${name} 未调用已知路径校验器（canonicalize/白名单）`,
      );
    }
    commands.push({
      name,
      dto: dtoName,
      path_fields: pathFields,
      validator: validator[0].replace("(", ""),
      error_code: validator[1],
    });
  }

  const generated = commands.map((command) => command.name).sort();
  const reviewed = [...REVIEWED_PATH_COMMANDS].sort();
  if (JSON.stringify(generated) !== JSON.stringify(reviewed)) {
    throw new Error("[path-commands] 路径命令集合与已复核清单不一致");
  }
  for (const name of required) {
    if (!generated.includes(name)) {
      throw new Error(`[path-commands] 缺少 ADR §5.3 必有命令：${name}`);
    }
  }

  return {
    schema: "aether.path-commands.v1",
    sources: {
      commands: HANDLERS_SRC,
      dto: DTO_SRC,
      required: `${ADR_SRC} §5.3`,
    },
    counts: {
      commands: commands.length,
      required: required.length,
    },
    required,
    commands,
  };
}

export function renderPathCommands(model) {
  return `${JSON.stringify(model, null, 2)}\n`;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const rendered = renderPathCommands(buildPathCommands());
  const targetPath = path.join(root, TARGET);
  if (MODE === "generate") {
    writeFileSync(targetPath, rendered, "utf8");
    console.log(`[path-commands] 生成完成：${TARGET}`);
    process.exit(0);
  }
  let current;
  try {
    current = readFileSync(targetPath, "utf8");
  } catch {
    console.error(`[path-commands] 阻断：生成物缺失（${TARGET}）。先运行 generate。`);
    process.exit(1);
  }
  const normalize = (value) => value.replace(/\r\n/g, "\n");
  if (normalize(current) !== normalize(rendered)) {
    console.error(`[path-commands] 阻断：${TARGET} 与重新生成结果不一致（diff=0 要求）。`);
    process.exit(1);
  }
  const model = JSON.parse(rendered);
  console.log(
    `[path-commands] 通过：路径命令 ${model.counts.commands}（P1-8 必有 ${model.counts.required}）；diff=0`,
  );
}
