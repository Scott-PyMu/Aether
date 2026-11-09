#!/usr/bin/env node
/**
 * M10-03 / ADR-033 D-033-3：从已实现命令清单生成 `docs/api/aetherd-v1.openapi.json`
 * 与 `docs/api/schemas/*.json`，并提供字段级校验（generate / check）。
 *
 * 数据源（只读）：
 * - `packages/protocol/src/command-manifest.json`（63 已实现；59 daemon + 4 shell）；
 * - `packages/protocol/src/types.ts`（aether-app 生成物；请求 DTO 字段/必填/枚举）；
 * - `packages/protocol/src/error_codes.json`（32 IPC + business 集合）。
 *
 * 约定：
 * - 外部 API 只列 daemon 命令（59；shell 命令不可经 HTTP 调用）；
 * - 请求 `params` 与 stdio 完全一致（内层 DTO，无 payload 包装）；
 * - 生成物禁止手改：`node scripts/ci/api-schema.mjs check` 必须 diff=0。
 */
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const manifestPath = path.join(root, "packages/protocol/src/command-manifest.json");
const typesPath = path.join(root, "packages/protocol/src/types.ts");
const errorCodesPath = path.join(root, "packages/protocol/src/error_codes.json");
const outDir = path.join(root, "docs/api");
const schemasDir = path.join(outDir, "schemas");
const openApiPath = path.join(outDir, "aetherd-v1.openapi.json");
const DEFAULT_TIMEOUT_MS = 30000;

function readJson(file) {
  return JSON.parse(readFileSync(file, "utf8"));
}

function stableStringify(value) {
  if (Array.isArray(value)) {
    return `[${value.map((item) => stableStringify(item)).join(",")}]`;
  }
  if (value && typeof value === "object") {
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${stableStringify(value[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value);
}

function deepEqual(left, right) {
  return stableStringify(left) === stableStringify(right);
}

function stripComments(text) {
  return text
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .split(/\r?\n/)
    .map((line) => line.replace(/\/\/.*$/, ""))
    .join("\n");
}

function parseTypeDefinitions(source) {
  const lines = stripComments(source).split(/\r?\n/);
  const types = new Map();
  for (let index = 0; index < lines.length; index += 1) {
    const match = lines[index].match(/^export type ([A-Za-z0-9_]+)\s*=\s*(.*)$/);
    if (!match) continue;
    const name = match[1];
    let body = match[2];
    let cursor = index;
    while (!body.trimEnd().endsWith(";")) {
      cursor += 1;
      if (cursor >= lines.length) {
        throw new Error(`类型 ${name} 未以分号结束`);
      }
      body += `\n${lines[cursor]}`;
    }
    index = cursor;
    types.set(name, body.trim().replace(/;$/, "").trim());
  }
  return types;
}

const PRIMITIVES = {
  string: { type: "string" },
  number: { type: "number" },
  integer: { type: "integer" },
  boolean: { type: "boolean" },
  unknown: {},
  any: {},
  null: { type: "null" },
};

const FIELD_CONSTRAINTS = {
  title: { minLength: 1, maxLength: 256 },
  text: { maxLength: 1048576 },
  name: { maxLength: 128 },
  base_url: { maxLength: 2048 },
  api_key: { maxLength: 8192 },
  query: { maxLength: 256 },
  limit: { minimum: 1, maximum: 500 },
  offset: { minimum: 0 },
  last_seq: { minimum: 0 },
  max_bytes: { minimum: 65536, maximum: 1572864 },
  thinking_depth: { minimum: 0, maximum: 4 },
  priority: { minimum: -100, maximum: 100 },
};

function isUlidField(field) {
  return (
    field === "id" ||
    field.endsWith("_id") ||
    field === "client_msg_id" ||
    field === "request_id"
  );
}

function withFieldConstraints(field, schema) {
  const result = { ...schema };
  if (isUlidField(field)) {
    result.pattern = "^[0-9A-HJKMNP-TV-Z]{26}$";
  }
  if (FIELD_CONSTRAINTS[field]) {
    Object.assign(result, FIELD_CONSTRAINTS[field]);
  }
  return result;
}

function splitTopLevelUnion(expression) {
  const parts = [];
  let depth = 0;
  let current = "";
  for (const char of expression) {
    if (char === "{" || char === "<" || char === "(" || char === "[") depth += 1;
    if (char === "}" || char === ">" || char === ")" || char === "]") depth -= 1;
    if (char === "|" && depth === 0) {
      parts.push(current.trim());
      current = "";
      continue;
    }
    current += char;
  }
  if (current.trim()) parts.push(current.trim());
  return parts.filter(Boolean);
}

function typeToSchema(expression, knownTypes) {
  let text = expression.trim();
  if (!text) return {};
  while (text.startsWith("(") && text.endsWith(")") && text.length > 1) {
    text = text.slice(1, -1).trim();
  }

  const parts = splitTopLevelUnion(text);
  const nonNull = parts.filter((part) => part !== "null" && part !== "undefined");
  const nullable = nonNull.length !== parts.length;
  let schema;
  if (nonNull.length === 0) {
    schema = { type: "null" };
  } else if (nonNull.length === 1) {
    schema = singleTypeToSchema(nonNull[0], knownTypes);
  } else {
    schema = { anyOf: nonNull.map((part) => singleTypeToSchema(part, knownTypes)) };
  }
  if (nullable && schema.type !== "null") {
    return { anyOf: [schema, { type: "null" }] };
  }
  return schema;
}

function singleTypeToSchema(text, knownTypes) {
  const value = text.trim();
  if (PRIMITIVES[value]) return { ...PRIMITIVES[value] };
  if (/^"[^"]*"$/.test(value)) return { const: JSON.parse(value) };
  if (/^-?\d+$/.test(value)) return { const: Number(value) };
  if (value.endsWith("[]")) {
    return { type: "array", items: typeToSchema(value.slice(0, -2), knownTypes) };
  }
  const arrayMatch = value.match(/^Array<(.+)>$/);
  if (arrayMatch) {
    return { type: "array", items: typeToSchema(arrayMatch[1], knownTypes) };
  }
  const recordMatch = value.match(/^Record<[^,]+,\s*(.+)>$/);
  if (recordMatch) {
    if (recordMatch[1].trim() === "never") {
      return { type: "object", maxProperties: 0, additionalProperties: false };
    }
    return { type: "object", additionalProperties: typeToSchema(recordMatch[1], knownTypes) };
  }
  if (value.startsWith("{")) {
    return objectSchemaFromBody(value.slice(1, -1), knownTypes);
  }
  if (knownTypes.has(value)) {
    return { $ref: `#/components/schemas/${value}` };
  }
  return {};
}

function objectSchemaFromBody(body, knownTypes) {
  const properties = {};
  const required = [];
  const lines = body.split(/\r?\n/);
  for (const line of lines) {
    const match = line.match(/^\s*([A-Za-z_][A-Za-z0-9_]*)(\?)?\s*:\s*(.+?)\s*,?\s*$/);
    if (!match) continue;
    const field = match[1];
    const optional = Boolean(match[2]);
    const typeText = match[3].replace(/,\s*$/, "").trim();
    properties[field] = withFieldConstraints(field, typeToSchema(typeText, knownTypes));
    if (!optional) required.push(field);
  }
  const schema = { type: "object", additionalProperties: false, properties };
  if (required.length > 0) schema.required = required;
  else schema.maxProperties = 0;
  return schema;
}

const SCHEMA_OVERRIDES = {
  BackupSource: {
    anyOf: [
      {
        type: "object",
        additionalProperties: false,
        required: ["internal"],
        properties: {
          internal: {
            type: "object",
            additionalProperties: false,
            required: ["id"],
            properties: { id: { type: "string", pattern: "^[0-9A-HJKMNP-TV-Z]{26}$" } },
          },
        },
      },
      {
        type: "object",
        additionalProperties: false,
        required: ["external"],
        properties: {
          external: {
            type: "object",
            additionalProperties: false,
            required: ["path"],
            properties: { path: { type: "string", minLength: 1 } },
          },
        },
      },
    ],
  },
};

function buildSchemas(types) {
  const knownTypes = new Set(types.keys());
  const schemas = {};
  for (const [name, body] of types) {
    if (SCHEMA_OVERRIDES[name]) {
      schemas[name] = SCHEMA_OVERRIDES[name];
      continue;
    }
    const trimmed = body.trim();
    if (trimmed.startsWith("{") && trimmed.endsWith("}")) {
      schemas[name] = objectSchemaFromBody(trimmed.slice(1, -1), knownTypes);
      continue;
    }
    const quoted = splitTopLevelUnion(trimmed).filter((part) => /^"[^"]*"$/.test(part));
    if (quoted.length > 0 && quoted.length === splitTopLevelUnion(trimmed).length) {
      schemas[name] = { type: "string", enum: quoted.map((part) => JSON.parse(part)) };
      continue;
    }
    const parts = splitTopLevelUnion(trimmed);
    if (parts.length > 1) {
      schemas[name] = {
        anyOf: parts.map((part) => singleTypeToSchema(part, knownTypes)),
      };
      continue;
    }
    schemas[name] = singleTypeToSchema(trimmed, knownTypes);
  }
  return schemas;
}

function noParamsSchema() {
  return { anyOf: [{ type: "null" }, { type: "object", maxProperties: 0, additionalProperties: false }] };
}

function commandTimeout(command) {
  return Number.isInteger(command.timeout_ms) ? command.timeout_ms : DEFAULT_TIMEOUT_MS;
}

function buildOpenApi(manifest, schemas, errorCodes) {
  const daemonCommands = manifest.commands.filter((command) => command.route === "daemon");
  const commandVariants = daemonCommands.map((command) => {
    const params =
      command.params.style === "strict" && command.params.dto
        ? { $ref: `#/components/schemas/${command.params.dto}` }
        : noParamsSchema();
    return {
      type: "object",
      additionalProperties: false,
      required: ["jsonrpc", "id", "method"],
      properties: {
        jsonrpc: { const: "2.0" },
        id: { anyOf: [{ type: "integer" }, { type: "string" }] },
        method: { const: command.name },
        params,
      },
    };
  });

  const ipcCodes = (errorCodes.ipc ?? []).map((item) => item.code ?? item);
  const businessCodes = (errorCodes.business ?? []).map((item) => item.code ?? item);

  const rpcRequest = { oneOf: commandVariants };
  const rpcError = {
    type: "object",
    additionalProperties: false,
    required: ["code", "message"],
    properties: {
      code: { type: "integer" },
      message: { type: "string" },
      data: {
        type: "object",
        additionalProperties: true,
        properties: {
          aether_code: { type: "string", enum: ipcCodes },
          business_code: { anyOf: [{ type: "string", enum: businessCodes }, { type: "null" }] },
          reason: { type: "string" },
          details: {},
        },
      },
    },
  };
  const rpcResponse = {
    type: "object",
    additionalProperties: false,
    required: ["jsonrpc", "id"],
    properties: {
      jsonrpc: { const: "2.0" },
      id: { anyOf: [{ type: "integer" }, { type: "string" }, { type: "null" }] },
      result: {},
      error: { $ref: "#/components/schemas/RpcError" },
    },
  };

  const components = { ...schemas, RpcRequest: rpcRequest, RpcResponse: rpcResponse, RpcError: rpcError };

  const commandCatalog = daemonCommands.map((command) => ({
    name: command.name,
    timeout_ms: commandTimeout(command),
    route: command.route,
    params_style: command.params.style,
    params_dto: command.params.dto,
    request_schema: `#/components/schemas/RpcRequest`,
  }));

  return {
    openapi: "3.1.0",
    info: {
      title: "Aether daemon HTTP/WS API",
      version: "1.0.0",
      description:
        "M10-03/ADR-033 v1.1：loopback + bearer token；POST /v1/rpc 的 params 与 stdio 完全一致（内层 DTO）。stdio 仍是 UI 专用通道，HTTP/WS/CLI 为并行 API。",
    },
    servers: [
      {
        url: "http://127.0.0.1:{port}",
        variables: { port: { default: "0", description: "发现文件中的随机端口（或 AETHERD_HTTP_PORT）" } },
      },
    ],
    security: [{ bearerAuth: [] }],
    paths: {
      "/v1/rpc": {
        post: {
          summary: "JSON-RPC 2.0 单请求（不支持 batch）",
          requestBody: {
            required: true,
            content: { "application/json": { schema: { $ref: "#/components/schemas/RpcRequest" } } },
          },
          responses: {
            200: {
              description: "JSON-RPC 响应（业务错误也在 200 body）",
              content: { "application/json": { schema: { $ref: "#/components/schemas/RpcResponse" } } },
            },
            400: { description: "畸形 JSON / 非法请求" },
            401: { description: "缺少或错误 token" },
            413: { description: "body > 2MiB" },
            429: { description: "速率/并发限制" },
            503: { description: "daemon 未就绪（core_not_ready）" },
          },
        },
      },
      "/v1/rpc/cancel": {
        post: {
          summary: "取消在途请求（未知 id → 204；不改变已落库状态）",
          requestBody: {
            required: true,
            content: {
              "application/json": {
                schema: {
                  type: "object",
                  additionalProperties: false,
                  required: ["request_id"],
                  properties: { request_id: { anyOf: [{ type: "integer" }, { type: "string" }] } },
                },
              },
            },
          },
          responses: { 204: { description: "已登记取消/未知 id" } },
        },
      },
      "/v1/meta": {
        get: {
          summary: "已实现命令元数据（只列 daemon 命令；不含内部路径/密钥）",
          responses: { 200: { description: "protocol/version/commands/timeout_ms/capabilities" } },
        },
      },
      "/v1/health": {
        get: {
          summary: "health 命令 + health.api 状态",
          responses: { 200: { description: "HealthReport" }, 503: { description: "core_not_ready" } },
        },
      },
      "/v1/events": {
        get: {
          summary: "原生 WebSocket 事件流（首帧 auth + subscribe）",
          description:
            "首帧 5s 内 {\"type\":\"auth\",\"token\":\"<token>\"}；随后 {\"type\":\"subscribe\",\"session_id?\":\"<ULID>\",\"last_seq?\":N}。服务端发送 {\"type\":\"event\",\"envelope\":{...9 字段...}}；慢消费者 close 1013。",
          responses: { 101: { description: "Switching Protocols" }, 401: { description: "鉴权失败" } },
        },
      },
      "/v1/admin/rotate-token": {
        post: {
          summary: "轮换 token（需当前 token；旧 token 立即失效；新 token 同步发现文件）",
          responses: { 200: { description: "{token}" }, 401: { description: "鉴权失败" } },
        },
      },
    },
    components: {
      securitySchemes: { bearerAuth: { type: "http", scheme: "bearer" } },
      schemas: components,
    },
    "x-aether-command-catalog": commandCatalog,
  };
}

function writeJson(file, value) {
  writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`, "utf8");
}

function build() {
  const manifest = readJson(manifestPath);
  const errorCodes = readJson(errorCodesPath);
  const types = parseTypeDefinitions(readFileSync(typesPath, "utf8"));
  const schemas = buildSchemas(types);
  const openapi = buildOpenApi(manifest, schemas, errorCodes);
  return { manifest, openapi, schemas: openapi.components.schemas };
}

function generate() {
  const built = build();
  rmSync(schemasDir, { recursive: true, force: true });
  mkdirSync(schemasDir, { recursive: true });
  for (const [name, schema] of Object.entries(built.schemas)) {
    writeJson(path.join(schemasDir, `${name}.json`), schema);
  }
  writeJson(openApiPath, built.openapi);
  return built;
}

function resolveRef(rootDocument, schema) {
  if (!schema || typeof schema !== "object") return schema;
  if (schema.$ref) {
    const match = schema.$ref.match(/^#\/components\/schemas\/(.+)$/);
    if (!match) throw new Error(`非法 $ref：${schema.$ref}`);
    const target = rootDocument.components?.schemas?.[match[1]];
    if (!target) throw new Error(`$ref 无法解析：${schema.$ref}`);
    return target;
  }
  return schema;
}

function sampleFromSchema(rootDocument, schema, depth = 0) {
  if (depth > 8) return null;
  const resolved = resolveRef(rootDocument, schema);
  if (!resolved) return null;
  if (resolved.const !== undefined) return resolved.const;
  if (resolved.enum) return resolved.enum[0];
  if (resolved.anyOf) return sampleFromSchema(rootDocument, resolved.anyOf[0], depth + 1);
  if (resolved.oneOf) return sampleFromSchema(rootDocument, resolved.oneOf[0], depth + 1);
  const type = Array.isArray(resolved.type) ? resolved.type[0] : resolved.type;
  if (type === "null") return null;
  if (type === "object" || resolved.properties) {
    const object = {};
    for (const [key, property] of Object.entries(resolved.properties ?? {})) {
      if ((resolved.required ?? []).includes(key)) {
        object[key] = sampleFromSchema(rootDocument, property, depth + 1);
      }
    }
    return object;
  }
  if (type === "array") return [sampleFromSchema(rootDocument, resolved.items ?? {}, depth + 1)];
  if (type === "string") {
    if (resolved.pattern === "^[0-9A-HJKMNP-TV-Z]{26}$") {
      return "01J00000000000000000000000";
    }
    return "x";
  }
  if (type === "integer" || type === "number") return 1;
  if (type === "boolean") return true;
  return null;
}

function validateSchema(rootDocument, schema, value, location = "$") {
  const resolved = resolveRef(rootDocument, schema);
  if (!resolved || Object.keys(resolved).length === 0) return [];
  if (resolved.const !== undefined && value !== resolved.const) {
    return [`${location}: 期望 const ${JSON.stringify(resolved.const)}`];
  }
  if (resolved.enum && !resolved.enum.includes(value)) {
    return [`${location}: ${JSON.stringify(value)} 不在 enum 内`];
  }
  if (resolved.anyOf) {
    const results = resolved.anyOf.map((option) => validateSchema(rootDocument, option, value, location));
    return results.some((errors) => errors.length === 0) ? [] : [`${location}: 不匹配 anyOf`];
  }
  if (resolved.oneOf) {
    const matches = resolved.oneOf.filter(
      (option) => validateSchema(rootDocument, option, value, location).length === 0,
    );
    return matches.length === 1 ? [] : [`${location}: oneOf 匹配 ${matches.length} 个分支`];
  }
  const type = Array.isArray(resolved.type) ? resolved.type : resolved.type ? [resolved.type] : [];
  if (type.length > 0) {
    const actual =
      value === null ? "null" : Array.isArray(value) ? "array" : typeof value;
    const numeric = actual === "number" && Number.isInteger(value) ? "integer" : actual;
    if (!type.includes(actual) && !(actual === "number" && type.includes("integer")) && !(numeric === "integer" && type.includes("number"))) {
      return [`${location}: 类型 ${actual} 不在 ${type.join("|")}`];
    }
  }
  if (typeof value === "string") {
    if (resolved.minLength !== undefined && value.length < resolved.minLength) {
      return [`${location}: 长度 < ${resolved.minLength}`];
    }
    if (resolved.maxLength !== undefined && value.length > resolved.maxLength) {
      return [`${location}: 长度 > ${resolved.maxLength}`];
    }
    if (resolved.pattern && !new RegExp(resolved.pattern).test(value)) {
      return [`${location}: 不匹配 pattern ${resolved.pattern}`];
    }
  }
  if (typeof value === "number") {
    if (resolved.minimum !== undefined && value < resolved.minimum) {
      return [`${location}: < minimum ${resolved.minimum}`];
    }
    if (resolved.maximum !== undefined && value > resolved.maximum) {
      return [`${location}: > maximum ${resolved.maximum}`];
    }
  }
  if (Array.isArray(value) && resolved.items) {
    const errors = [];
    value.forEach((item, index) => {
      errors.push(...validateSchema(rootDocument, resolved.items, item, `${location}[${index}]`));
    });
    return errors;
  }
  if (value && typeof value === "object" && !Array.isArray(value)) {
    const errors = [];
    for (const key of resolved.required ?? []) {
      if (!(key in value)) errors.push(`${location}: 缺少必填字段 ${key}`);
    }
    if (resolved.maxProperties !== undefined && Object.keys(value).length > resolved.maxProperties) {
      errors.push(`${location}: 字段数 > ${resolved.maxProperties}`);
    }
    for (const [key, item] of Object.entries(value)) {
      if (resolved.properties && resolved.properties[key]) {
        errors.push(...validateSchema(rootDocument, resolved.properties[key], item, `${location}.${key}`));
      } else if (resolved.additionalProperties === false) {
        errors.push(`${location}: 未知字段 ${key}`);
      }
    }
    return errors;
  }
  return [];
}

function checkGenerated(openapi, schemas) {
  const failures = [];
  const generatedOpenApi = readJson(openApiPath);
  if (!deepEqual(generatedOpenApi, openapi)) {
    failures.push("docs/api/aetherd-v1.openapi.json 与源不一致；请运行 pnpm generate:api-schema");
  }
  const files = existsSync(schemasDir) ? readdirSync(schemasDir).filter((name) => name.endsWith(".json")) : [];
  if (files.length !== Object.keys(schemas).length) {
    failures.push(`schema 文件数不一致：文件 ${files.length} / 组件 ${Object.keys(schemas).length}`);
  }
  for (const [name, schema] of Object.entries(schemas)) {
    const file = path.join(schemasDir, `${name}.json`);
    if (!existsSync(file)) {
      failures.push(`缺少 schema 文件：${name}.json`);
      continue;
    }
    const generated = readJson(file);
    if (!deepEqual(generated, schema)) {
      failures.push(`schema ${name}.json 与源不一致；请运行 pnpm generate:api-schema`);
    }
  }
  return failures;
}

function checkContract(manifest, openapi) {
  const failures = [];
  const commands = manifest.commands.filter((command) => command.route === "daemon");
  const catalog = openapi["x-aether-command-catalog"] ?? [];
  if (catalog.length !== commands.length) {
    failures.push(`命令目录数 ${catalog.length} ≠ daemon 命令 ${commands.length}`);
  }
  const catalogNames = new Set(catalog.map((item) => item.name));
  const commandNames = new Set(commands.map((command) => command.name));
  for (const name of commandNames) {
    if (!catalogNames.has(name)) failures.push(`命令目录缺少 ${name}`);
  }
  for (const name of catalogNames) {
    if (!commandNames.has(name)) failures.push(`命令目录含未登记命令 ${name}`);
  }
  if (commands.some((command) => command.name === "app_exit")) {
    failures.push("shell 命令 app_exit 不得进入 daemon 目录");
  }
  for (const command of commands) {
    if (command.params.style === "strict" && command.params.dto) {
      if (!openapi.components.schemas[command.params.dto]) {
        failures.push(`命令 ${command.name} 缺少请求 DTO schema：${command.params.dto}`);
      }
    }
  }

  // 代表性请求字段级校验：必填字段缺失/枚举非法必须被 schema 拒绝。
  for (const command of commands) {
    const dto = command.params.dto;
    if (command.params.style !== "strict" || !dto) continue;
    const schema = openapi.components.schemas[dto];
    const required = schema.required ?? [];
    const sample = sampleFromSchema(openapi, schema);
    const baseErrors = validateSchema(openapi, schema, sample);
    if (baseErrors.length > 0) {
      failures.push(`${command.name} 样本不合法：${baseErrors.join("；")}`);
    }
    for (const field of required) {
      const missing = { ...sample };
      delete missing[field];
      if (validateSchema(openapi, schema, missing).length === 0) {
        failures.push(`${command.name} 缺少必填字段 ${field} 未被 schema 拒绝`);
      }
    }
    for (const [field, property] of Object.entries(schema.properties ?? {})) {
      if (property.enum) {
        const invalid = { ...sample, [field]: "__invalid__" };
        if (validateSchema(openapi, schema, invalid).length === 0) {
          failures.push(`${command.name} 枚举字段 ${field} 非法值未被拒绝`);
        }
      }
    }
  }

  // 分页/预算字段必须出现在对应请求 schema（ADR-009 / M9-04 DoD-4）。
  const pagination = {
    messages_page: ["last_seq", "limit", "max_bytes", "next_cursor"],
    session_list: ["limit"],
    session_search: ["limit", "offset"],
    audit_query: ["limit", "offset"],
  };
  for (const [commandName, fields] of Object.entries(pagination)) {
    const command = commands.find((item) => item.name === commandName);
    if (!command || !command.params.dto) continue;
    const schema = openapi.components.schemas[command.params.dto];
    for (const field of fields) {
      if (!schema.properties?.[field]) {
        failures.push(`${commandName} 缺少分页/预算字段 schema：${field}`);
      }
    }
  }

  // RpcError 的 aether_code / business_code 必须来自生成集合。
  const rpcError = openapi.components.schemas.RpcError;
  const aetherEnum = rpcError.properties.data.properties.aether_code.enum;
  const businessEnum = rpcError.properties.data.properties.business_code.anyOf[0].enum;
  if (aetherEnum.length !== 32) failures.push(`aether_code 枚举 ${aetherEnum.length} ≠ 32`);
  if (businessEnum.length !== 24) failures.push(`business_code 枚举 ${businessEnum.length} ≠ 24`);

  // RpcRequest / RpcResponse 自校验（成功/错误 envelope 字段级）。
  const success = { jsonrpc: "2.0", id: 1, result: {} };
  const error = {
    jsonrpc: "2.0",
    id: 1,
    error: { code: -32601, message: "unknown", data: { aether_code: "not_implemented" } },
  };
  for (const [label, value] of [
    ["RpcResponse.success", success],
    ["RpcResponse.error", error],
  ]) {
    const errors = validateSchema(openapi, openapi.components.schemas.RpcResponse, value);
    if (errors.length > 0) failures.push(`${label} 不合法：${errors.join("；")}`);
  }
  return failures;
}

function main() {
  const mode = process.argv[2] ?? "generate";
  const built = mode === "check" ? build() : generate();
  const { manifest, openapi, schemas } = built;
  if (mode === "check") {
    const failures = [
      ...checkGenerated(openapi, schemas),
      ...checkContract(manifest, openapi),
    ];
    if (failures.length > 0) {
      for (const failure of failures) console.error(`[api-schema] ${failure}`);
      process.exit(1);
    }
    console.log(
      `[api-schema] OK：daemon 命令 ${openapi["x-aether-command-catalog"].length}；schema ${Object.keys(schemas).length}；diff=0`,
    );
    return;
  }
  if (mode === "generate") {
    const failures = checkContract(manifest, openapi);
    if (failures.length > 0) {
      for (const failure of failures) console.error(`[api-schema] ${failure}`);
      process.exit(1);
    }
    console.log(
      `[api-schema] 已生成 docs/api/aetherd-v1.openapi.json + ${Object.keys(schemas).length} 个 schema（daemon 命令 ${openapi["x-aether-command-catalog"].length}）`,
    );
    return;
  }
  console.error(`未知模式：${mode}（generate|check）`);
  process.exit(2);
}

// 保留给未来 CI 使用（当前 check 不依赖 git；生成物 diff 由 check 自身保证）。
export { buildOpenApi, buildSchemas, parseTypeDefinitions, validateSchema };

main();
