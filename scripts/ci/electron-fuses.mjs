#!/usr/bin/env node
/**
 * `@electron/fuses` 构建后断言（M9-10 DoD-3；ADR-029 §5.1）。
 *
 * 配置单一来源：`apps/desktop/electron/fuses.json`（9 个 fuse 显式声明）。
 *
 * 用法：
 *   node scripts/ci/electron-fuses.mjs dev                 # 复制开发 Electron 到 dist-out，翻转并断言
 *   node scripts/ci/electron-fuses.mjs apply <electron>    # 翻转指定二进制（原地；打包后使用）
 *   node scripts/ci/electron-fuses.mjs assert <electron>   # 只读断言
 *   node scripts/ci/electron-fuses.mjs print <electron>    # 打印当前 fuse 状态
 *
 * 退出码：0 = 全部符合期望；1 = 断言失败；2 = 用法/环境错误。
 */
import { copyFileSync, existsSync, mkdirSync, readFileSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const requireFromDesktop = createRequire(path.join(root, "apps", "desktop", "package.json"));
const { FuseVersion, FuseV1Options, FuseState, flipFuses, getCurrentFuseWire } =
  requireFromDesktop("@electron/fuses");
const configPath = path.join(root, "apps", "desktop", "electron", "fuses.json");

const OPTION_BY_NAME = {
  runAsNode: FuseV1Options.RunAsNode,
  enableCookieEncryption: FuseV1Options.EnableCookieEncryption,
  enableNodeOptionsEnvironmentVariable: FuseV1Options.EnableNodeOptionsEnvironmentVariable,
  enableNodeCliInspectArguments: FuseV1Options.EnableNodeCliInspectArguments,
  enableEmbeddedAsarIntegrityValidation: FuseV1Options.EnableEmbeddedAsarIntegrityValidation,
  onlyLoadAppFromAsar: FuseV1Options.OnlyLoadAppFromAsar,
  loadBrowserProcessSpecificV8Snapshot: FuseV1Options.LoadBrowserProcessSpecificV8Snapshot,
  grantFileProtocolExtraPrivileges: FuseV1Options.GrantFileProtocolExtraPrivileges,
  wasmTrapHandlers: FuseV1Options.WasmTrapHandlers,
};

function loadConfig() {
  const raw = JSON.parse(readFileSync(configPath, "utf8"));
  if (raw.version !== "1") {
    throw new Error(`fuses.json version 必须为 "1"（实际 ${raw.version}）`);
  }
  const keys = Object.keys(OPTION_BY_NAME);
  const actual = Object.keys(raw.options ?? {});
  for (const key of keys) {
    if (!(key in (raw.options ?? {}))) {
      throw new Error(`fuses.json 缺少 ${key}（9 个 fuse 必须显式声明）`);
    }
    if (typeof raw.options[key] !== "boolean") {
      throw new Error(`fuses.json 的 ${key} 必须是布尔值`);
    }
  }
  const extra = actual.filter((key) => !keys.includes(key));
  if (extra.length > 0) {
    throw new Error(`fuses.json 含未知 fuse：${extra.join(", ")}`);
  }
  return raw;
}

function fuseConfig(raw) {
  const config = {
    version: FuseVersion.V1,
    strictlyRequireAllFuses: raw.strictlyRequireAllFuses === true,
  };
  for (const [name, option] of Object.entries(OPTION_BY_NAME)) {
    config[option] = raw.options[name];
  }
  return config;
}

function expectedStates(raw) {
  const expected = new Map();
  for (const [name, option] of Object.entries(OPTION_BY_NAME)) {
    expected.set(option, raw.options[name] ? FuseState.ENABLE : FuseState.DISABLE);
  }
  return expected;
}

function evaluate(wire, raw) {
  const expected = expectedStates(raw);
  const failures = [];
  for (const [name, option] of Object.entries(OPTION_BY_NAME)) {
    const actual = wire[option];
    const want = expected.get(option);
    if (actual !== want) {
      failures.push(
        `${name} 期望 ${stateName(want)}，实际 ${stateName(actual)}`,
      );
    }
  }
  return failures;
}

function stateName(state) {
  switch (state) {
    case FuseState.ENABLE:
      return "enabled";
    case FuseState.DISABLE:
      return "disabled";
    case FuseState.REMOVED:
      return "removed";
    case FuseState.INHERIT:
      return "inherit";
    default:
      return `unknown(${String(state)})`;
  }
}

function printWire(wire) {
  for (const [name, option] of Object.entries(OPTION_BY_NAME)) {
    console.log(`  ${name.padEnd(42)} ${stateName(wire[option])}`);
  }
}

function devElectronPath() {
  const desktopRequire = createRequire(
    path.join(root, "apps", "desktop", "package.json"),
  );
  return desktopRequire("electron");
}

async function assertBinary(binary, raw) {
  if (!existsSync(binary)) {
    throw new Error(`找不到 Electron 二进制：${binary}`);
  }
  const wire = await getCurrentFuseWire(binary);
  const failures = evaluate(wire, raw);
  if (failures.length > 0) {
    console.error(`[fuses] FAIL ${binary}`);
    for (const failure of failures) console.error(`  - ${failure}`);
    return false;
  }
  console.log(`[fuses] PASS ${binary}`);
  return true;
}

async function applyBinary(binary, raw) {
  if (!existsSync(binary)) {
    throw new Error(`找不到 Electron 二进制：${binary}`);
  }
  await flipFuses(binary, fuseConfig(raw));
  return assertBinary(binary, raw);
}

async function main() {
  const [mode, target] = process.argv.slice(2);
  const raw = loadConfig();

  if (mode === "print") {
    const file = target ? path.resolve(target) : devElectronPath();
    const wire = await getCurrentFuseWire(file);
    console.log(`[fuses] ${file}`);
    printWire(wire);
    return 0;
  }
  if (mode === "assert") {
    return (await assertBinary(path.resolve(target ?? ""), raw)) ? 0 : 1;
  }
  if (mode === "apply") {
    return (await applyBinary(path.resolve(target ?? ""), raw)) ? 0 : 1;
  }
  if (mode === "dev") {
    const source = devElectronPath();
    const outDir = path.join(root, "dist-out", "electron-fuses");
    mkdirSync(outDir, { recursive: true });
    const copy = path.join(outDir, path.basename(source));
    copyFileSync(source, copy);
    console.log(`[fuses] 复制 ${source} → ${copy}（${statSync(copy).size} bytes）`);
    const ok = await applyBinary(copy, raw);
    if (!ok) return 1;
    console.log(
      "[fuses] PASS：runAsNode/NODE_OPTIONS/--inspect 关闭；onlyLoadAppFromAsar 开启",
    );
    return 0;
  }
  console.error(
    `[fuses] 用法：node scripts/ci/electron-fuses.mjs dev | apply <electron> | assert <electron> | print <electron>`,
  );
  return 2;
}

main()
  .then((code) => process.exit(code))
  .catch((error) => {
    console.error(`[fuses] 错误：${error instanceof Error ? error.message : String(error)}`);
    process.exit(2);
  });
