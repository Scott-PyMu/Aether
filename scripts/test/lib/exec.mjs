/**
 * 验证脚本共享工具（M1-01）。
 * 仅使用 Node 标准库；不引入任何依赖。
 */
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { StringDecoder } from "node:string_decoder";
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
 * Windows 下 pnpm 可能是 .cmd 垫片或独立 exe，Node 无法直接 spawn .cmd，
 * 因此按序探测：AETHER_PNPM 覆盖 → PNPM_HOME（pnpm/action-setup）→
 * 全局 npm 安装的 pnpm.cjs → %LOCALAPPDATA%\pnpm\pnpm.exe；最后回退 PATH。
 */
export function pnpmCommand() {
  if (process.env.AETHER_PNPM) return { command: process.env.AETHER_PNPM, prefix: [] };
  const appData = process.env.APPDATA ?? path.join(os.homedir(), "AppData", "Roaming");
  const localAppData = process.env.LOCALAPPDATA ?? path.join(os.homedir(), "AppData", "Local");
  const candidates = [
    // pnpm/action-setup：PNPM_HOME 指向 node_modules\.bin，里面有 pnpm.cmd / pnpm.exe。
    process.env.PNPM_HOME && path.join(process.env.PNPM_HOME, "pnpm.cmd"),
    process.env.PNPM_HOME && path.join(process.env.PNPM_HOME, "pnpm.exe"),
    process.env.PNPM_HOME && path.join(process.env.PNPM_HOME, "pnpm.cjs"),
    path.join(appData, "npm", "node_modules", "pnpm", "bin", "pnpm.cjs"),
    path.join(appData, "npm", "node_modules", "pnpm", "bin", "pnpm.js"),
    localAppData && path.join(localAppData, "pnpm", "pnpm.exe"),
  ].filter(Boolean);
  for (const candidate of candidates) {
    if (!existsSync(candidate)) continue;
    if (/\.(cjs|js)$/i.test(candidate)) return { command: process.execPath, prefix: [candidate] };
    return { command: candidate, prefix: [] };
  }
  return { command: "pnpm", prefix: [] };
}

/**
 * 子进程 stdout/stderr 行框定器（M1-06/M1-08 E2E 加固）。
 *
 * 管道可能在**任意字节**处把一行拆成多个 chunk：直接对每个 chunk 做
 * `split(/\r?\n/)` 会把一条长行（如含中文标签的 PHASE 快照 JSON）切成两段
 * 互相不匹配的行，导致 E2E 谓词永不命中（CI annotation：
 * `等待超时：启动门阻塞快照（已收到 3 行 stdout…）`）。
 *
 * 返回 `{ feed, flush }`：`feed` 逐 chunk 投喂，仅当遇到 `\n` 才回调完整行；
 * 进程结束时调用 `flush` 补发无尾换行的余量。空行与行首 BOM / 行尾 CR 归一化。
 */
export function lineFramer(onLine) {
  const decoder = new StringDecoder("utf8");
  let pending = "";
  const emit = (raw) => {
    const line = raw.replace(/^\uFEFF/, "").replace(/\r+$/, "");
    if (line.trim()) onLine(line);
  };
  return {
    feed(chunk) {
      pending += typeof chunk === "string" ? chunk : decoder.write(chunk);
      let index = pending.indexOf("\n");
      while (index >= 0) {
        emit(pending.slice(0, index));
        pending = pending.slice(index + 1);
        index = pending.indexOf("\n");
      }
    },
    flush() {
      pending += decoder.end();
      const rest = pending;
      pending = "";
      if (rest.trim()) emit(rest);
    },
  };
}

/** 读取 rustc host triple（如 x86_64-pc-windows-msvc）。 */
export function rustcHost() {
  const result = spawnSync(bin("rustc"), ["-vV"], { encoding: "utf8" });
  const match = /host:\s*(\S+)/.exec(result.stdout ?? "");
  return match ? match[1] : "";
}

function quoteWindowsArg(arg) {
  if (!/[ \t"&|<>^]/.test(arg)) return arg;
  return `"${`${arg}`.replace(/(\\*)"/g, '$1$1\\"').replace(/(\\+)$/, "$1$1")}"`;
}

/**
 * Windows 上按 PATH 解析命令（where.exe）。
 * 优先返回 .exe；若只有 .cmd/.bat（如 pnpm/action-setup 安装的 pnpm.cmd），
 * 由 run() 用 cmd.exe 包裹执行。
 */
function resolveOnWindows(name) {
  if (process.platform !== "win32") return name;
  if (path.extname(name) !== "" || name.includes("/") || name.includes("\\")) return name;
  const result = spawnSync("where.exe", [name], { encoding: "utf8" });
  if (result.status === 0 && result.stdout) {
    const candidates = result.stdout
      .split(/\r?\n/)
      .map((line) => line.trim())
      .filter(Boolean);
    const exe = candidates.find((candidate) => /\.exe$/i.test(candidate));
    if (exe) return exe;
    const script = candidates.find((candidate) => /\.(cmd|bat)$/i.test(candidate));
    if (script) return script;
    if (candidates.length > 0) return candidates[0];
  }
  return name;
}

/** 同步执行命令并透传输出；返回退出码（无法启动时返回 127）。 */
export function run(name, args, options = {}) {
  const resolved = resolveOnWindows(name);
  const printable = [name, ...args].join(" ");
  console.log(`\n$ ${printable}${options.env ? "   # 附加环境变量" : ""}`);

  let result;
  // .cmd/.bat 垫片需要 cmd.exe；未解析到具体路径的裸命令同样交给 cmd.exe 做 PATH 解析
  //（Node 直接 spawn 裸命令在部分 CI 环境会 ENOENT）。
  const useWindowsShell =
    process.platform === "win32" && (/\.(cmd|bat)$/i.test(resolved) || resolved === name);
  if (useWindowsShell) {
    const commandLine = [resolved, ...args].map(quoteWindowsArg).join(" ");
    result = spawnSync(commandLine, {
      cwd: options.cwd ?? repoRoot,
      stdio: options.capture ? "pipe" : "inherit",
      encoding: "utf8",
      env: { ...process.env, ...(options.env ?? {}) },
      shell: true,
    });
  } else {
    result = spawnSync(resolved, args, {
      cwd: options.cwd ?? repoRoot,
      stdio: options.capture ? "pipe" : "inherit",
      encoding: "utf8",
      env: { ...process.env, ...(options.env ?? {}) },
      shell: false,
    });
  }

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
