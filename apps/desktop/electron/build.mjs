#!/usr/bin/env node
/**
 * Electron main/preload 构建（M9-07 DoD-1）。
 *
 * 为什么不用单个多入口构建：`sandbox:true` 的 preload 只能 `require('electron')`
 * 与少量内置模块，**不能 require 本地共享 chunk**。因此四个入口分别用 Vite
 * lib 模式打成自包含 CJS：
 *   - main.cjs
 *   - preload.cjs（主 UI，白名单 window.aether）
 *   - fatalPreload.cjs（S-12 独立 renderer）
 *   - keychainPreload.cjs（S-13 独立 renderer）
 *
 * 同时把 S-00/S-12/S-13 静态页复制到 `electron/dist/shell/`，并写
 * `build-manifest.json`（证据/排查用；不是协议生成物）。
 */
import { builtinModules } from "node:module";
import { cpSync, existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { build } from "vite";

const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const outDir = path.join(appRoot, "electron", "dist");
const entries = [
  { name: "main", input: path.join(appRoot, "electron", "main.ts") },
  { name: "preload", input: path.join(appRoot, "electron", "preload.ts") },
  { name: "fatalPreload", input: path.join(appRoot, "electron", "fatalPreload.ts") },
  { name: "keychainPreload", input: path.join(appRoot, "electron", "keychainPreload.ts") },
];
const shellPages = ["fatal", "startup", "keychain"];
const external = [
  "electron",
  ...builtinModules,
  ...builtinModules.map((name) => `node:${name}`),
];

rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });

for (const entry of entries) {
  if (!existsSync(entry.input)) {
    console.error(`[electron-build] 缺少入口：${entry.input}`);
    process.exit(1);
  }
  await build({
    root: appRoot,
    configFile: false,
    publicDir: false,
    logLevel: "warn",
    build: {
      outDir,
      emptyOutDir: false,
      target: "node22",
      minify: false,
      sourcemap: false,
      lib: {
        entry: entry.input,
        formats: ["cjs"],
        fileName: () => `${entry.name}.cjs`,
      },
      rollupOptions: {
        external,
        output: {
          // 单入口单文件；禁止共享 chunk（sandbox preload 不能 require 本地文件）。
          inlineDynamicImports: true,
        },
      },
    },
  });
  console.log(`[electron-build] ${entry.name}.cjs`);
}

for (const page of shellPages) {
  const source = path.join(appRoot, "electron", page);
  const target = path.join(outDir, "shell", page);
  if (!existsSync(source)) {
    console.error(`[electron-build] 缺少壳级页面：${source}`);
    process.exit(1);
  }
  cpSync(source, target, { recursive: true });
  console.log(`[electron-build] shell/${page}`);
}

const manifest = {
  schema: "aether.electron-build.v1",
  built_at: new Date().toISOString(),
  entries: entries.map((entry) => `${entry.name}.cjs`),
  shell_pages: shellPages,
  main: "main.cjs",
  preload: "preload.cjs",
  fatal_preload: "fatalPreload.cjs",
  keychain_preload: "keychainPreload.cjs",
};
writeFileSync(
  path.join(outDir, "build-manifest.json"),
  `${JSON.stringify(manifest, null, 2)}\n`,
  "utf8",
);

for (const entry of entries) {
  const output = path.join(outDir, `${entry.name}.cjs`);
  if (!existsSync(output)) {
    console.error(`[electron-build] 未生成：${output}`);
    process.exit(1);
  }
}
console.log("[electron-build] 完成");
