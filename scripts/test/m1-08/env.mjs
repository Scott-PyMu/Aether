/**
 * M1-08 验证脚本共享环境解析（仅 Node 标准库）。
 *
 * windows-gnu 目标需要：
 *   1. windres/gcc（tauri 资源编译与链接）——PATH 上找不到时回退到常见 MinGW 安装位；
 *   2. `target/debug`（webview2-com 构建脚本输出的 WebView2Loader.dll，测试二进制在
 *      `target/debug/deps` 下加载该 DLL 时需要）。
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { repoRoot, rustcHost } from "../lib/exec.mjs";

export function buildEnv() {
  const env = { ...process.env };
  const targetDebug = path.join(repoRoot, "target", "debug");
  env.PATH = `${targetDebug}${path.delimiter}${env.PATH ?? ""}`;

  // 前端脚本内部会再调用 `pnpm` / `node`（package.json scripts），确保二者可见。
  const appData = process.env.APPDATA ?? path.join(process.env.USERPROFILE ?? "", "AppData", "Roaming");
  const npmGlobal = path.join(appData, "npm");
  if (existsSync(path.join(npmGlobal, "pnpm.cmd"))) {
    env.PATH = `${npmGlobal}${path.delimiter}${env.PATH}`;
  }
  const nodeDir = path.dirname(process.execPath);
  if (!(env.PATH ?? "").toLowerCase().includes(nodeDir.toLowerCase())) {
    env.PATH = `${nodeDir}${path.delimiter}${env.PATH}`;
  }

  const host = rustcHost();
  if (process.platform !== "win32" || !host.includes("windows-gnu")) return env;

  const windres = spawnSync("where.exe", ["windres"], { encoding: "utf8" });
  if (windres.status === 0) return env;

  const candidates = [];
  if (process.env.AETHER_MINGW_BIN) candidates.push(process.env.AETHER_MINGW_BIN);
  const temp = process.env.TEMP ?? process.env.TMP ?? "";
  candidates.push(
    path.join(temp, "opencode", "msys2", "mingw64-root", "mingw64", "bin"),
    "C:\\msys64\\mingw64\\bin",
    "C:\\mingw64\\bin",
  );
  const found = candidates.find((dir) => dir && existsSync(path.join(dir, "windres.exe")));
  if (found) {
    console.log(`[m1-08] windows-gnu：使用 MinGW 工具链 ${found}`);
    env.PATH = `${found}${path.delimiter}${env.PATH}`;
  } else {
    console.warn(
      "[m1-08] 警告：未找到 windres（可设 AETHER_MINGW_BIN），aether-tauri 构建/测试可能失败",
    );
  }
  return env;
}
