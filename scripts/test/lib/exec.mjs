/**
 * 验证脚本共享工具（M1-01）。
 * 仅使用 Node 标准库；不引入任何依赖。
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

export const repoRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "..",
);

/** 解析工具路径：支持 AETHER_<NAME> 环境变量覆盖，其次 ~/.cargo/bin，最后依赖 PATH。 */
export function bin(name) {
  const override = process.env[`AETHER_${name.toUpperCase().replace(/[^A-Z0-9]/g, "_")}`];
  if (override) return override;
  const exe = process.platform === "win32" ? ".exe" : "";
  const cargoCandidate = path.join(os.homedir(), ".cargo", "bin", `${name}${exe}`);
  if (existsSync(cargoCandidate)) return cargoCandidate;
  return name;
}

/**
 * 解析 pnpm 调用方式。
 * Windows 下 pnpm 是 .cmd 垫片，Node 无法直接 spawn，因此优先定位全局安装的
 * pnpm.cjs 并用当前 Node 执行；找不到时回退到 PATH 上的 pnpm。
 */
export function pnpmCommand() {
  if (process.env.AETHER_PNPM) return { command: process.env.AETHER_PNPM, prefix: [] };
  const appData = process.env.APPDATA ?? path.join(os.homedir(), "AppData", "Roaming");
  const npmGlobalCandidates = [
    path.join(appData, "npm", "node_modules", "pnpm", "bin", "pnpm.cjs"),
    path.join(appData, "npm", "node_modules", "pnpm", "bin", "pnpm.js"),
  ];
  for (const candidate of npmGlobalCandidates) {
    if (existsSync(candidate)) return { command: process.execPath, prefix: [candidate] };
  }
  return { command: "pnpm", prefix: [] };
}

/** 读取 rustc host triple（如 x86_64-pc-windows-msvc）。 */
export function rustcHost() {
  const result = spawnSync(bin("rustc"), ["-vV"], { encoding: "utf8" });
  const match = /host:\s*(\S+)/.exec(result.stdout ?? "");
  return match ? match[1] : "";
}

/** 同步执行命令并透传输出；返回退出码（无法启动时返回 127）。 */
export function run(name, args, options = {}) {
  console.log(`\n$ ${[name, ...args].join(" ")}${options.env ? "   # 附加环境变量" : ""}`);
  const result = spawnSync(name, args, {
    cwd: options.cwd ?? repoRoot,
    stdio: options.capture ? "pipe" : "inherit",
    encoding: "utf8",
    env: { ...process.env, ...(options.env ?? {}) },
    shell: false,
  });
  if (result.error) {
    console.error(`[exec] 无法执行 ${name}：${result.error.message}`);
    return 127;
  }
  if (options.capture) {
    if (result.stdout) process.stdout.write(result.stdout);
    if (result.stderr) process.stderr.write(result.stderr);
  }
  const exit = result.status ?? 1;
  console.log(`[exec] exit=${exit}`);
  return exit;
}

/**
 * 汇总检查结果并打印报告。
 * checks: [{ name, exit, expect, skip? }]，expect 可为数字或 "nonzero"。
 */
export function summarize(title, checks) {
  console.log(`\n===== ${title} =====`);
  let failed = 0;
  let skipped = 0;
  for (const check of checks) {
    if (check.skip) {
      skipped += 1;
      console.log(`SKIP  ${check.name}${check.reason ? `（${check.reason}）` : ""}`);
      continue;
    }
    const ok =
      check.expect === "nonzero" ? check.exit !== 0 : check.exit === check.expect;
    if (!ok) failed += 1;
    console.log(
      `${ok ? "PASS" : "FAIL"}  期望=${check.expect} 实际=${check.exit}  ${check.name}`,
    );
  }
  const suffix = skipped > 0 ? `，${skipped} 项跳过` : "";
  console.log(`----- ${failed === 0 ? `全部通过${suffix}` : `${failed} 项失败${suffix}`} -----`);
  return failed === 0 ? 0 : 1;
}
