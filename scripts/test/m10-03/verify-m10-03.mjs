#!/usr/bin/env node
/**
 * `pnpm verify:m10-03` —— M10-03 `aetherd` HTTP/WS + CLI 可复跑检查。
 *
 * 检查面（ADR-033 v1.1 D-033-1～D-033-6）：
 *  1. 依赖精确版本 + loopback/body/WS/鉴权常量；
 *  2. HTTP 路由/发现文件/CLI 子命令静态存在；
 *  3. OpenAPI/JSON Schema 生成物 diff=0（59 daemon 命令、字段级 schema）；
 *  4. 命令面/错误码/事件/设置/迁移 0 新增（M10-03 +0）；
 *  5. 生成物 `packages/protocol` 未被本任务修改；
 *  6. 证据文档存在且 DoD-1～8 有记录。
 */
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const failures = [];
const checks = [];

function check(name, condition, detail = "") {
  checks.push({ name, ok: Boolean(condition), detail });
  if (!condition) failures.push(`${name}${detail ? `：${detail}` : ""}`);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function mustContain(rel, needles) {
  const text = read(rel);
  for (const needle of needles) {
    check(`${rel} 含 ${needle}`, text.includes(needle));
  }
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

// 1) ADR / 依赖 / 常量
const adr = read("docs/adr/ADR-033-m10-aetherd-http-ws-cli.md");
check("ADR-033 v1.1 已接受", adr.includes("v1.1") && adr.includes("已接受"));
mustContain("crates/aether-daemon/Cargo.toml", [
  'axum = { version = "=0.8.9"',
  'tokio-tungstenite = "=0.29.0"',
  'ureq = { version = "=3.4.2"',
  'getrandom = "=0.3.4"',
  '"signal"',
  '"net"',
]);

const apiMod = read("crates/aether-daemon/src/api/mod.rs");
for (const needle of [
  'pub const DISCOVERY_FILE_NAME: &str = ".aetherd-api.json"',
  'pub const DEFAULT_BIND: &str = "127.0.0.1"',
  "pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024",
  "pub const MAX_CONNECTIONS: usize = 64",
  "pub const RATE_LIMIT_PER_SEC: f64 = 100.0",
  "pub const AUTH_TIMEOUT: Duration = Duration::from_secs(5)",
  "pub const WS_QUEUE_LIMIT: usize = 4_096",
  "pub const EXIT_HTTP_LISTENER_FAILED: u8 = 25",
  "pub const BUSINESS_REQUEST_TIMEOUT: &str = \"request_timeout\"",
  "AETHERD_HTTP_BIND",
  "AETHERD_HTTP_PORT",
  "AETHERD_HTTP_REQUIRED",
  "AETHERD_ALLOW_REMOTE",
  "mode(0o600)",
]) {
  check(`api/mod.rs 含 ${needle}`, apiMod.includes(needle));
}

mustContain("crates/aether-daemon/src/api/http.rs", [
  '"/v1/rpc"',
  '"/v1/rpc/cancel"',
  '"/v1/meta"',
  '"/v1/health"',
  '"/v1/events"',
  '"/v1/admin/rotate-token"',
  "DefaultBodyLimit::max(MAX_BODY_BYTES)",
  "auth required",
  "invalid token",
  "1013",
  "1012",
  "4401",
  "Authorization",
]);

mustContain("crates/aether-daemon/src/serve.rs", [
  "run_http_only",
  "Transport::null",
  "EXIT_HTTP_LISTENER_FAILED",
  "wait_for_shutdown_signal",
  "ctrl_close",
  "ctrl_shutdown",
  "install_pipeline",
]);

mustContain("crates/aether-daemon/src/cli.rs", [
  '"health"',
  '"session"',
  '"workflow"',
  '"subagent"',
  '"import"',
  '"export"',
  '"events"',
  '"api"',
  "connect_async",
  "AETHERD_API_TOKEN",
  "token 文件权限过宽",
]);

mustContain("crates/aether-daemon/src/lib.rs", [
  "Cli::HttpOnly",
  "--http-only",
  "--cli",
  "25 HTTP listener",
]);

// 2) OpenAPI / schema 生成物（check 模式 diff=0）
const packageJson = readJson("package.json");
for (const script of [
  "generate:api-schema",
  "verify:api-schema",
  "verify:m10-03",
  "verify:api",
  "external:m10-03",
]) {
  check(`package.json 含脚本 ${script}`, Boolean(packageJson.scripts?.[script]));
}
check(
  "外部消费者脚本存在",
  existsSync(path.join(root, "scripts/test/m10-03/external-client.mjs")),
);

try {
  const output = execFileSync(process.execPath, [path.join(root, "scripts/ci/api-schema.mjs"), "check"], {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
  check("api-schema check", output.includes("diff=0"), output);
} catch (error) {
  check("api-schema check", false, String(error.stderr || error.message));
}

const openapiPath = "docs/api/aetherd-v1.openapi.json";
check("OpenAPI 存在", existsSync(path.join(root, openapiPath)));
const openapi = readJson(openapiPath);
const catalog = openapi["x-aether-command-catalog"] ?? [];
check("OpenAPI daemon 命令 = 59", catalog.length === 59, `实际 ${catalog.length}`);
check("OpenAPI 无 shell 命令", !catalog.some((item) => item.name === "app_exit"));
check(
  "OpenAPI 路径齐全",
  ["/v1/rpc", "/v1/rpc/cancel", "/v1/meta", "/v1/health", "/v1/events", "/v1/admin/rotate-token"].every(
    (route) => openapi.paths[route],
  ),
);
check(
  "RpcRequest oneOf = 59",
  (openapi.components?.schemas?.RpcRequest?.oneOf ?? []).length === 59,
  `实际 ${(openapi.components?.schemas?.RpcRequest?.oneOf ?? []).length}`,
);
check("schema 数 = 80", Object.keys(openapi.components?.schemas ?? {}).length === 80);

// 3) 契约计数不变（M10-03 +0）
const manifest = readJson("packages/protocol/src/command-manifest.json");
check("command-manifest 仍 63", manifest.counts.total === 63);
check("daemon 命令仍 59", manifest.counts.daemon === 59);
check("shell 命令仍 4", manifest.counts.shell === 4);

const errorCodes = readJson("packages/protocol/src/error_codes.json");
check("IpcErrorCode 仍 32", (errorCodes.ipc ?? []).length === 32, `实际 ${(errorCodes.ipc ?? []).length}`);
check("business 码仍 9", (errorCodes.business ?? []).length === 9, `实际 ${(errorCodes.business ?? []).length}`);

const registry = readJson("docs/M10-00-登记表.json");
check("M10-03 登记命令 +0", !registry.commands.some((item) => item.task === "M10-03"));
check("M10-03 无事件新增", !registry.events.some((item) => item.task === "M10-03"));
check("M10-03 无设置键新增", !registry.settings.some((item) => item.task === "M10-03"));
check("M10-03 无迁移新增", !registry.migrations.some((item) => item.task === "M10-03"));

// 4) 生成物不被本任务修改
const protocolDiff = git(["diff", "--name-only", "--", "packages/protocol"]);
check("packages/protocol 生成物未修改", protocolDiff === "", protocolDiff);

// 5) 证据文档
const evidencePath = "docs/M10-03-证据.md";
check("证据文档存在", existsSync(path.join(root, evidencePath)));
if (existsSync(path.join(root, evidencePath))) {
  const evidence = read(evidencePath);
  for (let index = 1; index <= 8; index += 1) {
    check(`证据含 DoD-${index}`, evidence.includes(`DoD-${index}`));
  }
  for (const needle of ["HTTP", "WS", "CLI", "token", "OpenAPI", "0 差异"]) {
    check(`证据含 ${needle}`, evidence.includes(needle));
  }
}

// 输出
const passed = checks.filter((item) => item.ok).length;
for (const item of checks) {
  if (!item.ok) console.error(`[m10-03] FAIL ${item.name}${item.detail ? `：${item.detail}` : ""}`);
}
if (failures.length > 0) {
  console.error(`[m10-03] ${failures.length} 项失败（${passed}/${checks.length} 通过）`);
  process.exit(1);
}
console.log(`[m10-03] OK：${checks.length}/${checks.length} 项通过（ADR-033 v1.1；59 命令；OpenAPI diff=0）`);
