#!/usr/bin/env node
/**
 * `pnpm verify:m9-08`（P2 计划 M9-08；ADR-029 §7 / D-029-3 / D-029-4）。
 *
 * 可执行校验口径：
 *  1. `packages/protocol` 生成物存在且 diff=0：types.ts / client.ts /
 *     command-manifest.json / error_codes.json；types.ts 由 `aether-app` specta
 *     导出（`protocol-types.mjs check`）。
 *  2. 63 方法 typed；`AetherError { aether_code, business_code?, details? }`；
 *     事件 9 字段；生成物无手改。
 *  3. `apps/desktop/src` 0 处 `invoke` / 0 处 `@tauri-apps`；desktop 依赖移除
 *     `@tauri-apps/api`（协议包迁移期 `bindings.ts` 仅作 Tauri 对照，M9-14 清零）。
 *  4. `eventBridge.ts` 使用 `client.onEvent`；EventStore/补读不变；
 *     `health.test.ts` / `startup.test.ts` mock `@aether/protocol` client。
 *  5. 46 个 `src` 前端测试文件；UI-UX v0.17（M9 无启动门、S-01/S-01a 不可达、
 *     S-12/S-13 锚点、`data_dir_locked` / `db_open_failed`）由 M9-07/M9-09 承接
 *     并在本脚本断言存在。
 */
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import {
  buildCommandManifest,
  renderCommandManifest,
} from "../../ci/command-manifest.mjs";
import { buildErrorCodes, renderErrorCodes } from "../../ci/error-codes.mjs";
import { buildProtocolClient } from "../../ci/protocol-client.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const failures = [];
const checked = {
  commands: 0,
  methods: 0,
  requestTypes: 0,
  testFiles: 0,
  anchors: 0,
};

function fail(message) {
  failures.push(message);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function exists(rel) {
  return existsSync(path.join(root, rel));
}

function expect(condition, message) {
  if (!condition) fail(message);
}

function camel(name) {
  return name.replace(/_([a-z0-9])/g, (_, char) => char.toUpperCase());
}

function walk(dir, predicate, out = []) {
  for (const entry of readdirSync(path.join(root, dir), { withFileTypes: true })) {
    const rel = path.join(dir, entry.name).replaceAll("\\", "/");
    if (entry.isDirectory()) {
      walk(rel, predicate, out);
    } else if (predicate(rel)) {
      out.push(rel);
    }
  }
  return out;
}

// ===== 1. 生成物与 diff=0 =====
for (const rel of [
  "packages/protocol/src/types.ts",
  "packages/protocol/src/client.ts",
  "packages/protocol/src/command-manifest.json",
  "packages/protocol/src/error_codes.json",
]) {
  expect(exists(rel), `[DoD-1] 缺少生成物 ${rel}`);
}
if (
  exists("packages/protocol/src/command-manifest.json") &&
  read("packages/protocol/src/command-manifest.json") !==
    renderCommandManifest(buildCommandManifest())
) {
  fail("[DoD-1] command-manifest.json 与重新生成结果不一致（diff=0）");
}
if (
  exists("packages/protocol/src/error_codes.json") &&
  read("packages/protocol/src/error_codes.json") !== renderErrorCodes(buildErrorCodes())
) {
  fail("[DoD-1] error_codes.json 与重新生成结果不一致（diff=0）");
}
if (
  exists("packages/protocol/src/client.ts") &&
  read("packages/protocol/src/client.ts") !== buildProtocolClient()
) {
  fail("[DoD-1] client.ts 与重新生成结果不一致（diff=0）");
}

const typesCheck = spawnSync(
  process.execPath,
  [path.join(root, "scripts", "ci", "protocol-types.mjs"), "check"],
  { cwd: root, stdio: "inherit", env: process.env },
);
if (typesCheck.status !== 0) {
  fail("[DoD-1] types.ts 重新生成/比对失败（见上方 protocol-types 输出）");
}

// ===== 2. 63 方法 / 错误 / 事件 =====
const manifest = readJson("packages/protocol/src/command-manifest.json");
const errors = readJson("packages/protocol/src/error_codes.json");
const clientSource = read("packages/protocol/src/client.ts");
const typesSource = read("packages/protocol/src/types.ts");
checked.commands = manifest.commands?.length ?? 0;
expect(checked.commands === 63, `[DoD-2] 命令数 ${checked.commands} ≠ 63`);
expect(manifest.counts?.daemon === 59, "[DoD-2] daemon 路由 ≠ 59");
expect(manifest.counts?.shell === 4, "[DoD-2] shell 路由 ≠ 4");
expect(manifest.counts?.ui === 61, "[DoD-2] UI 暴露 ≠ 61");
expect(manifest.counts?.reserved === 2, "[DoD-2] 保留命令 ≠ 2");

const methodPattern = /^\s{2}([a-zA-Z][A-Za-z0-9]*)<T = unknown>\(/gm;
const interfaceBody = clientSource.split("export interface AetherClient {")[1]?.split("\n}")[0] ?? "";
const methods = [...interfaceBody.matchAll(methodPattern)]
  .map((match) => match[1])
  .filter((name) => name !== "rpc");
checked.methods = methods.length;
expect(methods.length === 63, `[DoD-2] AetherClient 方法 ${methods.length} ≠ 63`);
for (const command of manifest.commands) {
  expect(
    methods.includes(camel(command.name)),
    `[DoD-2] client 缺少方法 ${camel(command.name)}（${command.name}）`,
  );
}
expect(/export class AetherError extends Error/.test(clientSource), "[DoD-2] 缺少 AetherError");
for (const token of ["aether_code", "business_code", "details", "primary_code"]) {
  expect(clientSource.includes(token), `[DoD-2] AetherError 缺少 ${token}`);
}
expect(
  /onEvent\(handler: AetherEventHandler\): \(\) => void/.test(clientSource),
  "[DoD-2] client.onEvent 签名缺失",
);
expect(
  /export function createAetherClient/.test(clientSource),
  "[DoD-2] 缺少 createAetherClient",
);

const eventBlock = typesSource.split("export type AetherEvent = {")[1]?.split("\n};")[0] ?? "";
for (const field of [
  "v",
  "id",
  "session_id",
  "run_id",
  "runtime_id",
  "seq",
  "ts",
  "type",
  "payload",
]) {
  expect(
    new RegExp(`^\\s*${field}:`, "m").test(eventBlock),
    `[DoD-2] types.ts 的 AetherEvent 缺少字段 ${field}`,
  );
}
expect(errors.ipc?.length === 32, "[DoD-2] error_codes.json ipc ≠ 32");
expect(errors.business?.length === 9, "[DoD-2] error_codes.json business ≠ 9");
expect(
  clientSource.includes("export type AetherErrorCode =") &&
    clientSource.includes("export type AetherBusinessCode ="),
  "[DoD-2] client.ts 缺少错误码联合类型",
);

const requestTypes = [
  ...new Set(
    manifest.commands
      .filter((entry) => entry.params?.style !== "none")
      .map((entry) => entry.params.dto)
      .filter((dto) => typeof dto === "string"),
  ),
];
checked.requestTypes = requestTypes.length;
for (const dto of requestTypes) {
  expect(
    new RegExp(`export type ${dto} =`).test(typesSource),
    `[DoD-2] types.ts 缺少请求 DTO ${dto}`,
  );
}

// ===== 3. renderer 去 Tauri / invoke =====
const rendererFiles = walk("apps/desktop/src", (rel) => /\.(ts|tsx)$/.test(rel));
let invokeCount = 0;
for (const rel of rendererFiles) {
  const text = read(rel).replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  if (text.includes("@tauri-apps")) fail(`[DoD-3] renderer 仍 import @tauri-apps：${rel}`);
  invokeCount += (text.match(/\binvoke\b/g) ?? []).length;
}
expect(invokeCount === 0, `[DoD-3] apps/desktop/src 仍有 ${invokeCount} 处 invoke`);
const desktopPackage = readJson("apps/desktop/package.json");
expect(
  !Object.keys(desktopPackage.dependencies ?? {}).some((name) => name.startsWith("@tauri-apps")),
  "[DoD-3] apps/desktop/package.json 仍依赖 @tauri-apps",
);
expect(
  !Object.keys(desktopPackage.devDependencies ?? {}).some((name) => name.startsWith("@tauri-apps")),
  "[DoD-3] apps/desktop/package.json 仍依赖 @tauri-apps（dev）",
);
expect(
  !Object.prototype.hasOwnProperty.call(desktopPackage.scripts ?? {}, "tauri"),
  "[DoD-3] apps/desktop 仍暴露 tauri 构建脚本",
);
expect(
  !read("packages/protocol/src/index.ts").includes("./bindings"),
  "[DoD-3] protocol 入口仍导出 Tauri bindings",
);
const protocolPackage = readJson("packages/protocol/package.json");
expect(
  protocolPackage.scripts?.generate?.includes("protocol-types.mjs") &&
    protocolPackage.scripts.generate.includes("protocol-client.mjs") &&
    protocolPackage.scripts.generate.includes("command-manifest.mjs") &&
    protocolPackage.scripts.generate.includes("error-codes.mjs"),
  "[DoD-1] @aether/protocol generate 未接入 4 个生成器",
);
expect(
  protocolPackage.scripts?.check?.includes("protocol-types.mjs") &&
    protocolPackage.scripts.check.includes("protocol-client.mjs"),
  "[DoD-1] @aether/protocol check 未接入 types/client 校验",
);

// ===== 4. eventBridge / EventStore / 测试 mock =====
const eventBridge = read("apps/desktop/src/eventBridge.ts");
expect(eventBridge.includes("client.onEvent"), "[DoD-4] eventBridge 未使用 client.onEvent");
expect(!eventBridge.includes("@tauri-apps"), "[DoD-4] eventBridge 仍依赖 @tauri-apps");
const eventStore = read("apps/desktop/src/eventStore.ts");
for (const token of ["READBACK_GAP_LIMIT", "ingest(", "last_seq"]) {
  expect(eventStore.includes(token), `[DoD-4] EventStore 契约 token 缺失：${token}`);
}
expect(
  read("apps/desktop/src/backfillSource.ts").includes("messages_page") ||
    read("apps/desktop/src/session.ts").includes("messagesPage"),
  "[DoD-4] 补读仍须走 messages_page",
);
for (const rel of ["apps/desktop/src/health.test.ts", "apps/desktop/src/startup.test.ts"]) {
  expect(
    read(rel).includes('vi.mock("@aether/protocol"'),
    `[DoD-4] ${rel} 未 mock @aether/protocol client`,
  );
  expect(
    !read(rel).includes("@tauri-apps"),
    `[DoD-4] ${rel} 仍 mock @tauri-apps`,
  );
}

// ===== 5. 46 前端测试文件 / UI-UX v0.17 =====
const testFiles = walk("apps/desktop/src", (rel) => /\.test\.(ts|tsx)$/.test(rel));
checked.testFiles = testFiles.length;
expect(testFiles.length === 46, `[DoD-5] src 测试文件 ${testFiles.length} ≠ 46`);

const uiux = read("docs/UI-UX-设计规格.md");
expect(uiux.includes("设计规格 v0.17"), "[DoD-5] UI-UX 必须 v0.17");
for (const token of ["S-12", "S-13"]) {
  expect(uiux.includes(token), `[DoD-5] UI-UX 缺少 ${token}`);
}
const fatal = read("apps/desktop/electron/fatal/fatal.html");
const fatalJs = read("apps/desktop/electron/fatal/fatal.js");
for (const anchor of [
  "shell-fatal",
  "shell-fatal-reason",
  "shell-fatal-log-path",
  "shell-fatal-data-dir",
  "shell-fatal-retry",
  "shell-fatal-exit",
  "shell-fatal-export-diagnostics",
]) {
  checked.anchors += 1;
  expect(fatal.includes(anchor), `[DoD-5] S-12 缺少锚点 ${anchor}`);
}
for (const reason of [
  "daemon_start_failed",
  "handshake_failed",
  "a4_blocked",
  "db_open_failed",
  "schema_checksum_mismatch",
  "data_dir_locked",
]) {
  expect(fatalJs.includes(reason), `[DoD-5] S-12 缺少 data-reason ${reason}`);
}
const keychain = read("apps/desktop/electron/keychain/keychain.html");
for (const anchor of [
  "keychain-reauth",
  "keychain-reauth-reason",
  "keychain-reauth-retry",
  "keychain-reauth-passphrase",
  "keychain-reauth-skip",
  "keychain-reauth-notice",
]) {
  checked.anchors += 1;
  expect(keychain.includes(anchor), `[DoD-5] S-13 缺少锚点 ${anchor}`);
}
expect(
  errors.business.some((entry) => entry.code === "data_dir_locked") &&
    errors.business.some((entry) => entry.code === "db_open_failed"),
  "[DoD-5] error_codes.json 缺少 data_dir_locked/db_open_failed",
);
expect(
  !read("apps/desktop/src/App.tsx").includes("StartupGate") &&
    !read("apps/desktop/src/App.tsx").includes("startup_get") &&
    !read("apps/desktop/src/App.tsx").includes("startup_migrate"),
  "[DoD-5] M9 主 UI 不得渲染启动门或调用 startup_*（S-01/S-01a 不可达）",
);
expect(
  read("apps/desktop/electron/app.ts").includes("loadMainRenderer") &&
    read("apps/desktop/electron/app.ts").indexOf("runStartupSequence") <
      read("apps/desktop/electron/app.ts").indexOf("loadMainRenderer"),
  "[DoD-5] main 必须先完成启动序列再加载主 UI（M9 无启动门）",
);

// ===== 6. 脚本与证据 =====
const rootPackage = readJson("package.json");
expect(
  rootPackage.scripts?.["verify:m9-08"] === "node scripts/test/m9-08/verify-m9-08.mjs",
  "[DoD-6] package.json 缺少 verify:m9-08",
);
expect(
  rootPackage.scripts?.["generate:protocol-types"] ===
    "node scripts/ci/protocol-types.mjs generate",
  "[DoD-6] package.json 缺少 generate:protocol-types",
);
const evidence = "docs/M9-08-证据.md";
expect(exists(evidence), `[DoD-6] 缺少证据文档 ${evidence}`);
if (exists(evidence)) {
  const text = read(evidence);
  for (const section of ["DoD-1", "DoD-2", "DoD-3", "DoD-4", "DoD-5"]) {
    expect(text.includes(`| ${section} `), `[DoD-6] 证据缺少 ${section} 对照行`);
  }
}

// ===== 汇总 =====
if (failures.length > 0) {
  console.error(`[m9-08] FAIL（${failures.length} 项）`);
  for (const message of failures) console.error(`  - ${message}`);
  process.exit(1);
}

console.log(
  `[m9-08] OK：命令 ${checked.commands}（59+4；UI 61/保留 2）· ` +
    `client 方法 ${checked.methods} · 请求 DTO ${checked.requestTypes} · ` +
    `错误码 32+9 · renderer invoke=0 / @tauri=0 · ` +
    `测试文件 ${checked.testFiles} · S-12/S-13 锚点 ${checked.anchors} · diff=0`,
);
console.log("AETHER_M9_08 PASS");
