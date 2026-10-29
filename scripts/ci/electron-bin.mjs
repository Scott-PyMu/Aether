#!/usr/bin/env node
/**
 * Electron 可执行文件解析（M9-07 开发/E2E）。
 *
 * 背景（2026-10-29 本机实测）：
 * - Electron 45.0.0 以 `electron.exe` 启动时，`app.requestSingleInstanceLock()`
 *   会因单实例命名管道与同名可执行文件冲突而崩溃/返回 false； * - 将同一二进制以产品名 `Aether.exe` 启动（M9-11 打包后的真实形态）后，
 *   单实例锁正常（返回 true、`second-instance` 可达）。
 *
 * 因此开发/E2E 在 Windows 上优先使用同目录的 `Aether.exe`（硬链接，失败则复制）；
 * 找不到/无法创建时回退 `electron.exe`。**不修改依赖版本**，生产打包入口仍是
 * `Aether.exe`；`AETHER_E2E_ELECTRON` 显式覆盖优先（用于特殊环境）。
 */
import { copyFileSync, existsSync, linkSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import process from "node:process";

/**
 * @param {{ repoRoot: string, env?: NodeJS.ProcessEnv }} options
 * @returns {string}
 */
export function resolveElectronExecutable({ repoRoot, env = process.env }) {
  const override = env.AETHER_E2E_ELECTRON;
  if (typeof override === "string" && override.trim().length > 0) {
    return path.resolve(override);
  }
  const requireFromDesktop = createRequire(path.join(repoRoot, "apps", "desktop", "package.json"));
  const original = requireFromDesktop("electron");
  if (process.platform !== "win32") {
    return original;
  }
  if (path.basename(original).toLowerCase() !== "electron.exe") {
    return original;
  }
  const renamed = path.join(path.dirname(original), "Aether.exe");
  if (existsSync(renamed)) {
    return renamed;
  }
  try {
    // 同卷硬链接：不额外占用磁盘；失败（只读/跨卷）再复制。
    linkSync(original, renamed);
    return renamed;
  } catch {
    try {
      copyFileSync(original, renamed);
      return renamed;
    } catch {
      return original;
    }
  }
}
