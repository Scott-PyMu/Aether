/**
 * M1-08 验证脚本共享环境解析（仅 Node 标准库）。
 *
 * windows-gnu 目标需要：
 *   1. windres/gcc（tauri 资源编译与链接）——PATH 上找不到时回退到常见 MinGW 安装位；
 *   2. `target/debug`（webview2-com 构建脚本输出的 WebView2Loader.dll，测试二进制在
 *      `target/debug/deps` 下加载该 DLL 时需要）。
 *
 * Windows 环境变量大小写不敏感但 Node 的 `process.env` 保留原键名（常见为 `Path`），
 * 因此这里统一探测 PATH 键，避免写入 `PATH` 造成重复键、子进程拿到旧值。
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { repoRoot, rustcHost } from "../lib/exec.mjs";

export function buildEnv() {
  const env = { ...process.env };
  const pathKey = Object.keys(env).find((key) => key.toLowerCase() === "path") ?? "PATH";
  const prepend = (dir) => {
    if (!dir) return;
    const current = env[pathKey] ?? "";
    if (current.toLowerCase().includes(dir.toLowerCase())) return;
    env[pathKey] = `${dir}${path.delimiter}${current}`;
  };

  // webview2-com 构建脚本产物（测试二进制加载 WebView2Loader.dll 需要）。
  prepend(path.join(repoRoot, "target", "debug"));
  // pnpm/action-setup（CI）：PNPM_HOME 含 pnpm.cmd，前端脚本内会再调用 pnpm。
  prepend(process.env.PNPM_HOME);
  const appData = process.env.APPDATA ?? path.join(process.env.USERPROFILE ?? "", "AppData", "Roaming");
  const npmGlobal = path.join(appData, "npm");
  if (existsSync(path.join(npmGlobal, "pnpm.cmd"))) prepend(npmGlobal);
  prepend(path.dirname(process.execPath));

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
    prepend(found);
  } else {
    console.warn(
      "[m1-08] 警告：未找到 windres（可设 AETHER_MINGW_BIN），aether-tauri 构建/测试可能失败",
    );
  }
  return env;
}
