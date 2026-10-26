#!/usr/bin/env node
/**
 * M9-01 HEAD golden 基线录制器（P2 计划 M9-01 DoD-9；ADR-029 §9.4-1）。
 *
 * 录制内容（`docs/M9-01-baseline/`，录完冻结，M9-04/M9-13 只新增不改期望值）：
 * - `commands/NN-<name>.json` × 63：路由（daemon/shell）、UI 暴露（61 可达 + 2 保留）、
 *   请求 DTO 形状、**可证明的**拒绝用例（unknown_field / missing_field / invalid_type /
 *   invalid_json，以及分页/时间窗语义边界）；
 * - `contract/error_codes.json`：32 枚举 + 9 业务码现状（与 `packages/protocol/src/error_codes.json` 同源）；
 * - `contract/events.json`：EventEnvelope 9 字段 + 附录 B 事件类型（含 MVP 19 项标记）；
 * - `contract/settings.json`：设置键白名单 9 项；
 * - `contract/migrations.json`：迁移 0001–0009 清单；
 * - `contract/diagnostics.json`：`export_diagnostics` 响应字段与段清单；
 * - `contract/lifecycle.json`：启动/关闭时序预算（M9-01 冻结口径）；
 * - `manifest.json`：`recorded_at/head_commit/platform/version/commands[63]/entries[{path,sha256}]`、
 *   辅助产物哈希、计数与 `contract_digest`。
 *
 * 基线**只记录可证明的期望值**：`rejects` 里的错误码来自「strict 解析 + deny_unknown_fields」
 * 的确定性行为；`accepted_shape` 是合法请求形态的契约文档，**不**声明后端执行成功
 * （响应体由 M9-04 RPC 层 golden 追加，不改本基线）。
 *
 * 用法：
 *   node scripts/ci/m9-01-baseline.mjs record   # 重新录制（仅 M9-01 录制期使用）
 */
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { collectStats } from "./m9-01-stats.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");

export const BASELINE_DIR = "docs/M9-01-baseline";
const COMMANDS_SRC = "crates/aether-tauri/src/ipc/commands.rs";
const DTO_SRC = "crates/aether-tauri/src/ipc/dto.rs";
const ERROR_SRC = "crates/aether-tauri/src/ipc/error.rs";
const EVENT_SRC = "crates/aether-core/src/event.rs";
const MIGRATIONS_DIR = "migrations";

export const EXPECTED_COMMANDS = 63;
const RESERVED_COMMANDS = ["startup_get", "startup_migrate"];
const SHELL_COMMANDS = ["app_exit", "app_restart", "ref_pick", "startup_pick_target"];

/** 合法 ULID 样例（`is_ulid`：26 位 Crockford Base32，首字符 ≤ '7'）。 */
const ULID = `01J${"0".repeat(23)}`;
const SAMPLE_PATH = String.raw`E:\aether-baseline\sample`;

/**
 * 响应侧类型的来源文件。
 *
 * `ipc/dto.rs` 在 HEAD 只有请求 DTO（`*Request`）+ 嵌套请求体 `ReferenceEntry` + 5 个序列化枚举，
 * **没有**按命令的响应结构体；真正的响应/序列化结构体分散在 error（错误响应）、session_backend
 * （messages_page/会话摘要）、core_health（health）、core/domain（会话/运行时/工作区等域对象）。
 * 本清单按「定义位置」取，响应字段/可选性一律从这些文件的 `#[derive(...Serialize...)]` 类型解析。
 */
const RESPONSE_SOURCES = [
  "crates/aether-tauri/src/ipc/dto.rs",
  "crates/aether-tauri/src/ipc/error.rs",
  "crates/aether-tauri/src/session_backend.rs",
  "crates/aether-tauri/src/core_health.rs",
  "crates/aether-core/src/domain.rs",
];

const LIFECYCLE = {
  source: "docs/P2-架构迁移-实施计划与验收标准.md §2.15 M9-09 + ADR-029 §5/§4.7",
  phases: [
    { phase: "window_shown", budget_ms: 1500, kind: "hard_threshold", note: "窗口 ≤1.5s" },
    { phase: "daemon_ready", budget_ms: 2000, kind: "target", note: "initialize 握手目标 ≤2s" },
    {
      phase: "daemon_ready_timeout",
      budget_ms: 10000,
      kind: "hard_timeout",
      note: "握手硬超时 ≥10s，超时重试 1 次",
    },
    { phase: "ui_ready", budget_ms: 5000, kind: "hard_threshold", note: "UI ready ≤5s" },
    {
      phase: "session_created",
      budget_ms: 2000,
      kind: "hard_threshold",
      note: "会话创建 <2s（M9-09）",
    },
    {
      phase: "shutdown_exit",
      budget_ms: 10000,
      kind: "hard_timeout",
      note: "关闭序列总预算（M9-09：先 daemon 收口再退出进程）",
    },
  ],
};

const DIAGNOSTICS = {
  source: "crates/aether-tauri/src/diagnostics_control.rs",
  command: "export_diagnostics",
  response_keys: [
    "path",
    "file_name",
    "bytes",
    "generated_at",
    "sections",
    "scanned_clean",
    "log_lines",
    "task_dumps",
  ],
  sections: [
    "app",
    "host",
    "health",
    "security",
    "capacity",
    "store",
    "backup_auto",
    "retention",
    "export",
    "config",
    "logs",
    "task_dumps",
    "session_pause",
  ],
  redaction: "密钥零泄漏（D10）：脱敏后仍命中密钥模式即拒绝写出（diagnostics_write_failed）",
};

const PAGING_SEMANTIC_CASES = {
  messages_page: [
    {
      id: "paging_limit_zero",
      stage: "semantic_validation",
      params: { session_id: ULID, limit: 0 },
      expect: { result: "rejected", code: "out_of_range", field: "limit" },
    },
    {
      id: "paging_limit_over_max",
      stage: "semantic_validation",
      params: { session_id: ULID, limit: 501 },
      expect: { result: "rejected", code: "out_of_range", field: "limit" },
    },
  ],
  audit_query: [
    {
      id: "paging_limit_zero",
      stage: "semantic_validation",
      params: { limit: 0 },
      expect: { result: "rejected", code: "out_of_range", field: "limit" },
    },
    {
      id: "paging_limit_over_max",
      stage: "semantic_validation",
      params: { limit: 501 },
      expect: { result: "rejected", code: "out_of_range", field: "limit" },
    },
    {
      id: "ts_window_inverted",
      stage: "semantic_validation",
      params: { from_ts: 10, to_ts: 5 },
      expect: { result: "rejected", code: "invalid_value" },
    },
  ],
};

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function sha256(text) {
  return createHash("sha256").update(text, "utf8").digest("hex");
}

function stableJson(value) {
  return `${JSON.stringify(value, null, 2)}\n`;
}

/** 从 `collect_commands![...]` 取命令声明顺序。 */
function parseCommandNames(source) {
  const block = source.match(/tauri_specta::collect_commands!\[([\s\S]*?)\n {4}\]/);
  if (!block) throw new Error(`[m9-01-baseline] ${COMMANDS_SRC}：未找到 collect_commands!`);
  return block[1]
    .split(/\r?\n/)
    .map((line) => line.trim().replace(/,$/, ""))
    .filter(Boolean);
}

/** 提取命令函数签名与函数体（花括号配平；命令体内无裸字符串花括号）。 */
function parseCommandBody(source, name) {
  const marker = `fn ${name}(`;
  const start = source.indexOf(marker);
  if (start < 0) throw new Error(`[m9-01-baseline] ${COMMANDS_SRC}：未找到命令 ${name}`);
  const bodyStart = source.indexOf("{", start);
  let depth = 0;
  for (let index = bodyStart; index < source.length; index += 1) {
    if (source[index] === "{") depth += 1;
    else if (source[index] === "}") {
      depth -= 1;
      if (depth === 0) {
        return {
          signature: source.slice(start, source.indexOf("->", start)),
          body: source.slice(bodyStart, index + 1),
        };
      }
    }
  }
  throw new Error(`[m9-01-baseline] 命令 ${name} 的花括号未配平`);
}

/** 按花括号配平取体（兼容 `pub struct X {}` 这种单行空体）。 */
function braceBody(source, start) {
  const bodyStart = source.indexOf("{", start);
  if (bodyStart < 0) return null;
  let depth = 0;
  for (let index = bodyStart; index < source.length; index += 1) {
    if (source[index] === "{") depth += 1;
    else if (source[index] === "}") {
      depth -= 1;
      if (depth === 0) return { body: source.slice(bodyStart + 1, index), end: index };
    }
  }
  return null;
}

/** 解析结构体/枚举体的字段（含可选性来源）。 */
function parseFields(body) {
  const fields = [];
  let pending = [];
  for (const rawLine of body.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (line.startsWith("#[serde(")) {
      pending.push(line);
      continue;
    }
    if (line.startsWith("#[") || line.startsWith("///")) continue;
    const field = line.match(/^pub (\w+): (.+),$/);
    if (!field) continue;
    const type = field[2].trim();
    const flags = [];
    if (type.startsWith("Option<")) flags.push("type_option");
    if (pending.some((attr) => attr.includes("skip_serializing_if"))) {
      flags.push("skip_serializing_if");
    }
    if (pending.some((attr) => /(^|[\s(,])default\b/.test(attr))) flags.push("serde_default");
    fields.push({
      name: field[1],
      type,
      required: flags.length === 0,
      optional_when: flags,
    });
    pending = [];
  }
  return fields;
}

/** 取类型声明上方的属性块（以空行为边界，避免误抓上一个类型的 derive）。 */
function attributeBlock(source, index) {
  const boundary = source.lastIndexOf("\n\n", index);
  return source.slice(boundary < 0 ? 0 : boundary + 2, index);
}

/** 解析 DTO 结构体字段（顺序即 serde 报错顺序）。 */
function parseStructs(source) {
  const structs = new Map();
  for (const match of source.matchAll(/pub struct (\w+)/g)) {
    const scanned = braceBody(source, match.index);
    if (!scanned) continue;
    structs.set(match[1], parseFields(scanned.body));
  }
  return structs;
}

/** 解析「派生 Serialize」的结构体/枚举：响应侧字段与可选性、枚举线上值。 */
function parseSerializableTypes(rel) {
  const source = read(rel);
  const types = [];
  for (const match of source.matchAll(/pub (struct|enum) (\w+)/g)) {
    const head = attributeBlock(source, match.index);
    const derives = [...head.matchAll(/#\[derive\(([^)]*)\)\]/g)].at(-1);
    if (!derives || !/\bSerialize\b/.test(derives[1])) continue;
    const scanned = braceBody(source, match.index);
    if (!scanned) continue;
    if (match[1] === "struct") {
      types.push({
        id: `${rel}::${match[2]}`,
        name: match[2],
        kind: "struct",
        source: rel,
        fields: parseFields(scanned.body),
      });
      continue;
    }
    const renameAll = head.match(/#\[serde\(rename_all = "(\w+)"\)\]\s*$/);
    const variants = [];
    let pendingRename = null;
    for (const rawLine of scanned.body.split(/\r?\n/)) {
      const line = rawLine.trim();
      const rename = line.match(/^#\[serde\(rename = "([^"]+)"\)\]$/);
      if (rename) {
        pendingRename = rename[1];
        continue;
      }
      if (line.startsWith("#[") || line.startsWith("///")) continue;
      const variant = line.match(/^([A-Z]\w+),$/);
      if (!variant) continue;
      variants.push({
        variant: variant[1],
        wire: pendingRename ?? toSnakeCase(variant[1], renameAll?.[1]),
      });
      pendingRename = null;
    }
    types.push({ id: `${rel}::${match[2]}`, name: match[2], kind: "enum", source: rel, variants });
  }
  return types;
}

/** 响应侧契约（命令→响应恒为自由 JSON；类型清单按定义位置解析）。 */
function buildResponses(commands, dtoStructNames) {
  const types = RESPONSE_SOURCES.flatMap((rel) => parseSerializableTypes(rel));
  const requests = dtoStructNames.filter((name) => name.endsWith("Request"));
  const nested = dtoStructNames.filter((name) => !name.endsWith("Request"));
  return {
    schema: "aether.m9-01.responses-baseline.v1",
    source: RESPONSE_SOURCES,
    command_envelope: {
      kind: "json",
      typed: false,
      note:
        "63 条命令统一返回 JsonPayload（自由 JSON）；HEAD 没有按命令的响应 DTO，" +
        "故 commands[].response.type 恒为 null，响应契约按『序列化类型清单』冻结。",
    },
    dto_note:
      `ipc/dto.rs 只有 ${requests.length} 个 *Request 请求 DTO、` +
      `${nested.length} 个非请求结构体（${nested.join("/") || "无"}）与若干序列化枚举；` +
      "响应结构体分布在 error/session_backend/core_health/core::domain。",
    commands: commands.map((command) => ({
      command: command.command,
      response: { kind: "json", type: null },
    })),
    types,
    counts: { commands: commands.length, types: types.length },
  };
}

/** 附录 B 事件 → payload 结构体 → 字段（含可选性）。 */
function buildEventPayloads(events, eventSource) {
  const payloadEnum = eventSource.match(/pub enum EventPayload \{([\s\S]*?)\n\}/);
  if (!payloadEnum) throw new Error(`[m9-01-baseline] ${EVENT_SRC}：未找到 EventPayload`);
  const payloadTypes = new Map();
  for (const match of payloadEnum[1].matchAll(/^\s*(\w+)\((\w+)\),$/gm)) {
    payloadTypes.set(match[1], match[2]);
  }
  const structs = parseStructs(eventSource);
  const types = events.types.map(({ variant, wire }) => {
    const payload = payloadTypes.get(variant) ?? null;
    if (!payload) throw new Error(`[m9-01-baseline] 事件 ${variant} 缺少 EventPayload 映射`);
    const fields = structs.get(payload) ?? [];
    return {
      type: wire,
      variant,
      mvp: events.mvp.includes(variant),
      payload,
      fields: fields.map((field) => ({
        name: field.name,
        type: field.type,
        optional: !field.required,
        optional_when: field.optional_when,
      })),
    };
  });
  return {
    schema: "aether.m9-01.event-payloads-baseline.v1",
    source: EVENT_SRC,
    envelope_version: events.envelope_version,
    envelope_fields: events.envelope_fields,
    types,
    counts: { types: types.length, mvp: types.filter((type) => type.mvp).length },
  };
}

/** 解析 DTO 枚举（变体名 + serde 重命名后的线上值）。 */
function parseEnums(source) {
  const enums = new Map();
  for (const match of source.matchAll(/pub enum (\w+)/g)) {
    const scanned = braceBody(source, match.index);
    if (!scanned) continue;
    const renameAll = attributeBlock(source, match.index).match(
      /#\[serde\(rename_all = "(\w+)"\)\]\s*$/,
    );
    const variants = [];
    let pendingRename = null;
    for (const rawLine of scanned.body.split(/\r?\n/)) {
      const line = rawLine.trim();
      const rename = line.match(/^#\[serde\(rename = "([^"]+)"\)\]$/);
      if (rename) {
        pendingRename = rename[1];
        continue;
      }
      if (line.startsWith("#[") || line.startsWith("///")) continue;
      const variant = line.match(/^([A-Z]\w+),$/);
      if (!variant) continue;
      variants.push({
        variant: variant[1],
        wire: pendingRename ?? toSnakeCase(variant[1], renameAll?.[1]),
      });
      pendingRename = null;
    }
    enums.set(match[1], { renameAll: renameAll?.[1] ?? null, variants });
  }
  return enums;
}

function toSnakeCase(name, renameAll) {
  if (renameAll === "snake_case" || renameAll === null || renameAll === undefined) {
    return name
      .replace(/([a-z0-9])([A-Z])/g, "$1_$2")
      .replace(/([A-Z])([A-Z][a-z])/g, "$1_$2")
      .toLowerCase();
  }
  return name;
}

function isUlidField(name) {
  return /(^|_)id$/.test(name);
}

function isPathField(name) {
  return /(path|dir)$/.test(name);
}

/** 构造合法请求形态里的样例值（只用于文档化形状，不声明后端成功）。 */
function sampleValue(fieldName, type, enums) {
  if (type.startsWith("Option<")) return undefined;
  if (type === "String") {
    if (isUlidField(fieldName) && /(session|run|rule|preset|workspace|artifact)/.test(fieldName)) {
      return ULID;
    }
    if (isPathField(fieldName)) return SAMPLE_PATH;
    return "sample";
  }
  if (/^(u8|u16|u32|u64|i8|i16|i32|i64|usize)$/.test(type)) {
    if (fieldName === "limit") return 50;
    if (fieldName === "thinking_depth") return 2;
    return 1;
  }
  if (type === "bool") return true;
  if (type.startsWith("Vec<")) return [];
  if (type.startsWith("BTreeMap<") || type === "serde_json::Value" || type === "Value") return {};
  const enumerated = enums.get(type);
  if (enumerated && enumerated.variants.length > 0) return enumerated.variants[0].wire;
  if (isUlidField(fieldName)) return ULID;
  if (isPathField(fieldName)) return SAMPLE_PATH;
  return "sample";
}

function wrongTypedValue(type) {
  if (type === "String" || type.startsWith("Option<String>")) return 12345;
  if (/^(u8|u16|u32|u64|i8|i16|i32|i64|usize|f32|f64)$/.test(type)) return "not-a-number";
  if (type === "bool") return "not-a-boolean";
  if (type.startsWith("Vec<")) return "not-an-array";
  if (type.startsWith("BTreeMap<")) return "not-an-object";
  return 12345;
}

function buildAcceptedShape(fields, enums) {
  const shape = {};
  for (const field of fields) {
    if (!field.required) continue;
    const value = sampleValue(field.name, field.type, enums);
    if (value !== undefined) shape[field.name] = value;
  }
  return shape;
}

function buildRejects(fields, enums, style) {
  const rejects = [];
  const required = fields.filter((field) => field.required);

  if (style !== "none") {
    rejects.push({
      id: "unknown_field",
      stage: "strict_parse",
      params: { __unexpected_field: true },
      expect: { result: "rejected", code: "unknown_field", field: "__unexpected_field" },
    });
    rejects.push({
      id: "invalid_json",
      stage: "strict_parse",
      params: [],
      expect: { result: "rejected", code: "invalid_json" },
    });
  }

  if (required.length > 0) {
    const first = required[0];
    rejects.push({
      id: "missing_field",
      stage: "strict_parse",
      params: {},
      expect: { result: "rejected", code: "missing_field", field: first.name },
    });
    rejects.push({
      id: "invalid_type",
      stage: "strict_parse",
      params: { [first.name]: wrongTypedValue(first.type) },
      expect: { result: "rejected", code: "invalid_type" },
    });
  }

  return rejects;
}

/** 汇总基线契约（录制与校验共用；`contract_digest` 的输入）。 */
export function collectContract() {
  const commandsSource = read(COMMANDS_SRC);
  const dtoSource = read(DTO_SRC);
  const errorSource = read(ERROR_SRC);
  const eventSource = read(EVENT_SRC);

  const names = parseCommandNames(commandsSource);
  const structs = parseStructs(dtoSource);
  const enums = parseEnums(dtoSource);

  const commands = names.map((name, index) => {
    const { signature, body } = parseCommandBody(commandsSource, name);
    const hasPayload = /payload:\s*JsonPayload/.test(signature);
    const parsed = body.match(
      /let _?request:\s*([A-Z]\w*)\s*=\s*(parse_strict|parse_no_params)\(/,
    );
    let style = "none";
    let dto = null;
    if (parsed) {
      dto = parsed[1];
      style = parsed[2] === "parse_no_params" ? "no_params" : "strict";
    } else if (hasPayload) {
      throw new Error(`[m9-01-baseline] 命令 ${name} 带 payload 但未走 parse_strict/parse_no_params`);
    }
    if (dto && !structs.has(dto)) {
      throw new Error(`[m9-01-baseline] 命令 ${name} 的 DTO ${dto} 未在 ${DTO_SRC} 解析到`);
    }
    const fields = dto ? structs.get(dto) : [];
    const route = SHELL_COMMANDS.includes(name) ? "shell" : "daemon";
    const reserved = RESERVED_COMMANDS.includes(name);
    return {
      command: name,
      index: index + 1,
      route,
      exposure: reserved ? "reserved" : "ui",
      reachable: !reserved,
      params: {
        style,
        dto,
        required: fields.filter((field) => field.required).map((field) => field.name),
        optional: fields.filter((field) => !field.required).map((field) => field.name),
        field_types: Object.fromEntries(fields.map((field) => [field.name, field.type])),
      },
      accepted_shape: buildAcceptedShape(fields, enums),
      rejects: buildRejects(fields, enums, style),
    };
  });

  const ipcCodes = parseIpcCodes(errorSource);
  const events = parseEvents(eventSource);
  const settings = parseSettings(dtoSource);
  const migrations = readdirSync(path.join(root, MIGRATIONS_DIR))
    .filter((file) => /^\d{4}_.*\.sql$/.test(file))
    .sort();

  const responses = buildResponses(commands, [...structs.keys()]);
  const eventPayloads = buildEventPayloads(events, eventSource);

  return { commands, ipcCodes, events, settings, migrations, responses, eventPayloads };
}

function parseIpcCodes(source) {
  const body = source.match(/pub enum IpcErrorCode \{([\s\S]*?)\n\}/);
  if (!body) throw new Error(`[m9-01-baseline] ${ERROR_SRC}：未找到 IpcErrorCode`);
  const variants = [...body[1].matchAll(/^\s*([A-Z]\w+),$/gm)].map((match) => match[1]);
  const impl = source.match(/pub const fn as_str\(self\) -> &'static str \{([\s\S]*?)\n {4}\}/);
  const map = new Map(
    [...(impl?.[1] ?? "").matchAll(/Self::(\w+)\s*=>\s*"([a-z0-9_]+)"/g)].map((match) => [
      match[1],
      match[2],
    ]),
  );
  return variants.map((name) => map.get(name));
}

function parseEvents(source) {
  const fields = source.match(/pub const ENVELOPE_FIELDS: \[&str; (\d+)\] = \[([\s\S]*?)\n\];/);
  if (!fields) throw new Error(`[m9-01-baseline] ${EVENT_SRC}：未找到 ENVELOPE_FIELDS`);
  const envelopeFields = [...fields[2].matchAll(/"([^"]+)"/g)].map((match) => match[1]);
  const eventType = source.match(/pub enum EventType \{([\s\S]*?)\n\}/);
  if (!eventType) throw new Error(`[m9-01-baseline] ${EVENT_SRC}：未找到 EventType`);
  const types = [];
  let pendingRename = null;
  for (const rawLine of eventType[1].split(/\r?\n/)) {
    const line = rawLine.trim();
    const rename = line.match(/^#\[serde\(rename = "([^"]+)"\)\]$/);
    if (rename) {
      pendingRename = rename[1];
      continue;
    }
    if (line.startsWith("#[") || line.startsWith("///")) continue;
    const variant = line.match(/^([A-Z]\w+),$/);
    if (!variant) continue;
    types.push({ variant: variant[1], wire: pendingRename ?? variant[1] });
    pendingRename = null;
  }
  const mvp = source.match(/pub const MVP: \[Self; (\d+)\] = \[([\s\S]*?)\n {4}\];/);
  const mvpTypes = mvp
    ? [...mvp[2].matchAll(/Self::(\w+)/g)].map((match) => match[1])
    : [];
  return {
    envelope_version: Number(source.match(/pub const EVENT_ENVELOPE_VERSION: u32 = (\d+);/)?.[1]),
    envelope_fields: envelopeFields,
    types,
    mvp: mvpTypes,
  };
}

function parseSettings(source) {
  const allowlist = source.match(
    /pub const SETTINGS_KEY_ALLOWLIST: &\[&str\] = &\[([\s\S]*?)\n\];/,
  );
  if (!allowlist) throw new Error(`[m9-01-baseline] ${DTO_SRC}：未找到 SETTINGS_KEY_ALLOWLIST`);
  return [...allowlist[1].matchAll(/"([^"]+)"/g)].map((match) => match[1]);
}

/** 契约摘要：命令集合/DTO 形状/错误码/事件/设置/迁移的稳定哈希（源漂移即失败）。 */
export function computeContractDigest() {
  const contract = collectContract();
  return sha256(
    stableJson({
      commands: contract.commands.map((command) => ({
        command: command.command,
        route: command.route,
        exposure: command.exposure,
        style: command.params.style,
        dto: command.params.dto,
        required: command.params.required,
        optional: command.params.optional,
        rejects: command.rejects.map((reject) => [
          reject.id,
          reject.expect.code,
          reject.expect.field ?? null,
        ]),
      })),
      ipc_codes: contract.ipcCodes,
      envelope_fields: contract.events.envelope_fields,
      event_types: contract.events.types.map((type) => type.wire),
      settings: contract.settings,
      migrations: contract.migrations,
      responses: contract.responses.types.map((type) => [
        type.source,
        type.name,
        type.kind,
        type.kind === "struct"
          ? type.fields.map((field) => [field.name, field.type, field.required])
          : type.variants.map((variant) => variant.wire),
      ]),
      event_payloads: contract.eventPayloads.types.map((type) => [
        type.type,
        type.payload,
        type.fields.map((field) => [field.name, field.type, field.optional]),
      ]),
    }),
  );
}

function isoLocal(date = new Date()) {
  const pad = (value, width = 2) => String(Math.abs(value)).padStart(width, "0");
  const offset = -date.getTimezoneOffset();
  const sign = offset >= 0 ? "+" : "-";
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}` +
    `T${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}` +
    `${sign}${pad(Math.trunc(offset / 60))}:${pad(offset % 60)}`
  );
}

/**
 * 读取 HEAD commit（直接读 `.git/HEAD`，不起子进程）。
 *
 * 沙箱内受限进程无法通过管道捕获孙进程输出（libuv 命名管道 EPERM），
 * 因此这里不 spawn `git rev-parse`，改为解析 `.git/HEAD` + 松散 ref / packed-refs。
 */
function gitHead() {
  const gitDir = path.join(root, ".git");
  const head = readFileSync(path.join(gitDir, "HEAD"), "utf8").trim();
  if (!head.startsWith("ref: ")) {
    if (/^[0-9a-f]{40}$/.test(head)) return head;
    throw new Error(`[m9-01-baseline] 无法解析 .git/HEAD：${head}`);
  }
  const ref = head.slice("ref: ".length);
  const loose = path.join(gitDir, ...ref.split("/"));
  if (existsSync(loose)) {
    const commit = readFileSync(loose, "utf8").trim();
    if (/^[0-9a-f]{40}$/.test(commit)) return commit;
  }
  const packed = path.join(gitDir, "packed-refs");
  if (existsSync(packed)) {
    for (const line of readFileSync(packed, "utf8").split(/\r?\n/)) {
      const [commit, name] = line.trim().split(" ");
      if (name === ref && /^[0-9a-f]{40}$/.test(commit)) return commit;
    }
  }
  throw new Error(`[m9-01-baseline] 无法解析 git HEAD（ref=${ref}）`);
}

function writeFile(rel, content) {
  const absolute = path.join(root, rel);
  mkdirSync(path.dirname(absolute), { recursive: true });
  writeFileSync(absolute, content, "utf8");
}

function record() {
  const contract = collectContract();
  if (contract.commands.length !== EXPECTED_COMMANDS) {
    throw new Error(
      `[m9-01-baseline] 命令数 ${contract.commands.length} ≠ ${EXPECTED_COMMANDS}（63 = 59 daemon + 4 shell）`,
    );
  }
  const shell = contract.commands.filter((command) => command.route === "shell").length;
  const reachable = contract.commands.filter((command) => command.reachable).length;
  if (shell !== SHELL_COMMANDS.length) {
    throw new Error(`[m9-01-baseline] 壳本地命令数 ${shell} ≠ ${SHELL_COMMANDS.length}`);
  }
  if (reachable !== EXPECTED_COMMANDS - RESERVED_COMMANDS.length) {
    throw new Error(`[m9-01-baseline] 可达命令数 ${reachable} ≠ 61`);
  }

  const entries = [];
  for (const command of contract.commands) {
    const rel = `${BASELINE_DIR}/commands/${String(command.index).padStart(2, "0")}-${command.command}.json`;
    const payload = {
      schema: "aether.m9-01.command-baseline.v1",
      command: command.command,
      index: command.index,
      route: command.route,
      exposure: command.exposure,
      reachable: command.reachable,
      params: command.params,
      accepted_shape: command.accepted_shape,
      rejects: command.rejects,
      ...(PAGING_SEMANTIC_CASES[command.command]
        ? { semantic_cases: PAGING_SEMANTIC_CASES[command.command] }
        : {}),
      note:
        "rejects 的期望码由 strict 解析（deny_unknown_fields）确定性给出；accepted_shape 只声明请求形态，" +
        "不声明后端成功。响应体 golden 由 M9-04 追加，不改本文件。",
    };
    const text = stableJson(payload);
    writeFile(rel, text);
    entries.push({ path: rel, command: command.command, sha256: sha256(text) });
  }

  const errorCodes = JSON.parse(read("packages/protocol/src/error_codes.json"));
  const artifacts = [];
  artifacts.push(
    writeArtifact(`${BASELINE_DIR}/contract/error_codes.json`, {
      schema: "aether.m9-01.error-codes-baseline.v1",
      source: "packages/protocol/src/error_codes.json",
      ipc: errorCodes.ipc,
      business: errorCodes.business,
    }),
  );
  artifacts.push(
    writeArtifact(`${BASELINE_DIR}/contract/events.json`, {
      schema: "aether.m9-01.events-baseline.v1",
      source: EVENT_SRC,
      envelope_version: contract.events.envelope_version,
      envelope_fields: contract.events.envelope_fields,
      event_types: contract.events.types,
      mvp_types: contract.events.mvp,
      rules: [
        "seq 会话内单调唯一（DB UNIQUE(session_id,seq) 兜底，ADR-003/ADR-004）",
        "先写日志后广播（D4）；控制事件不丢，delta 可丢并按 last_seq 补读",
        "事件类型只取附录 B 清单，禁止自造（AGENTS §2.3）",
      ],
    }),
  );
  artifacts.push(
    writeArtifact(`${BASELINE_DIR}/contract/settings.json`, {
      schema: "aether.m9-01.settings-baseline.v1",
      source: DTO_SRC,
      keys: contract.settings,
      rule: "未登记键一律 invalid_enum（默认拒绝，ADR-018 A-6）",
    }),
  );
  artifacts.push(
    writeArtifact(`${BASELINE_DIR}/contract/migrations.json`, {
      schema: "aether.m9-01.migrations-baseline.v1",
      source: MIGRATIONS_DIR,
      files: contract.migrations,
      rule: "0001–0009 已发布不可修改；新约束走 0002+ 增量迁移（ADR-004）",
    }),
  );
  artifacts.push(
    writeArtifact(`${BASELINE_DIR}/contract/diagnostics.json`, {
      schema: "aether.m9-01.diagnostics-baseline.v1",
      ...DIAGNOSTICS,
    }),
  );
  artifacts.push(
    writeArtifact(`${BASELINE_DIR}/contract/lifecycle.json`, {
      schema: "aether.m9-01.lifecycle-baseline.v1",
      ...LIFECYCLE,
    }),
  );
  artifacts.push(writeArtifact(`${BASELINE_DIR}/contract/responses.json`, contract.responses));
  artifacts.push(
    writeArtifact(`${BASELINE_DIR}/contract/event_payloads.json`, contract.eventPayloads),
  );
  artifacts.push(writeArtifact(`${BASELINE_DIR}/contract/stats.json`, collectStats()));

  const authoredFiles = [
    { rel: `${BASELINE_DIR}/contract/risks.json`, purpose: "DoD-5 风险登记" },
    { rel: `${BASELINE_DIR}/contract/decisions.json`, purpose: "DoD-6/DoD-7 裁定证据" },
    { rel: `${BASELINE_DIR}/freeze/electron.json`, purpose: "Electron 精确 patch 冻结" },
    { rel: `${BASELINE_DIR}/freeze/thresholds.json`, purpose: "硬阈值冻结" },
  ];
  for (const { rel, purpose } of authoredFiles) {
    if (!existsSync(path.join(root, rel))) {
      throw new Error(`[m9-01-baseline] 缺少人工冻结文件：${rel}（先写入再录制）`);
    }
    artifacts.push({ path: rel, sha256: sha256(read(rel)), purpose });
  }

  const manifest = {
    schema: "aether.m9-01-baseline.v1",
    version: 1,
    recorded_at: isoLocal(),
    head_commit: gitHead(),
    platform: {
      os: process.platform,
      arch: process.arch,
      node: process.version,
    },
    source_of_truth: {
      commands: COMMANDS_SRC,
      dto: DTO_SRC,
      errors: ERROR_SRC,
      events: EVENT_SRC,
      migrations: MIGRATIONS_DIR,
    },
    counts: {
      commands: contract.commands.length,
      reachable,
      reserved: RESERVED_COMMANDS.length,
      daemon: contract.commands.length - shell,
      shell,
      ipc_error_codes: contract.ipcCodes.length,
      business_codes: errorCodes.business.length,
      event_envelope_fields: contract.events.envelope_fields.length,
      event_types: contract.events.types.length,
      settings_keys: contract.settings.length,
      migrations: contract.migrations.length,
      response_types: contract.responses.types.length,
      event_payload_types: contract.eventPayloads.types.length,
    },
    commands: contract.commands.map((command) => command.command),
    entries,
    artifacts,
    contract_digest: computeContractDigest(),
    frozen: {
      rule: "M9-04/M9-13 只新增，不改期望值；命令/错误码/事件/设置/迁移变更必须先走 ADR。",
      recorded_before: "M9-02（抽取 aether-app 之前）",
    },
  };

  const manifestText = stableJson(manifest);
  writeFile(`${BASELINE_DIR}/manifest.json`, manifestText);

  console.log(
    `[m9-01-baseline] 已录制 ${entries.length} 条命令基线 + ${artifacts.length} 个辅助产物` +
      `（daemon=${manifest.counts.daemon} / shell=${shell} / 可达=${reachable}）`,
  );
  console.log(`[m9-01-baseline] head_commit=${manifest.head_commit}`);
  console.log(`[m9-01-baseline] contract_digest=${manifest.contract_digest}`);
}

function writeArtifact(rel, value) {
  const text = stableJson(value);
  writeFile(rel, text);
  return { path: rel, sha256: sha256(text) };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const mode = process.argv[2] ?? "record";
  if (mode !== "record") {
    console.error(`[m9-01-baseline] 未知模式：${mode}（只支持 record）`);
    process.exit(2);
  }
  record();
}
