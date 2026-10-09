#!/usr/bin/env node
/**
 * M4-05：构建官方适配器运行时包（bun --compile）+ 注册清单 + 摘要记录。
 *
 * 产物（默认输出 `crates/aether-tauri/runtime-bundle/`，gitignored，随安装包资源分发）：
 *   - `aether-claude-adapter(.exe)` / `aether-codex-adapter(.exe)` / `aether-dsh-adapter(.exe)`
 *     （官方运行时集合三，ADR-008；适配器为 Bun 单文件编译产物，目标机无需 Node）；
 *   - `runtimes.json`：注册清单，形态对齐 ADR-015 §2.2 决策 4 schema
 *     （`{ schema_version, runtimes: [{ id, name, kind, version, protocol, program, args }] }`；
 *     `program` 为相对路径；不含密钥与 `api_key_ref`）。M7-01 的逐条校验/构建期摘要
 *     比对在 P1 落地时消费该产物（接口一致性义务，ADR-015 §2.2#6）；
 *   - `digests.json`：产物 sha256（打包冒烟与发布哈希证据来源）。
 *
 * 用法：
 *   node scripts/ci/build-runtime-bundles.mjs [--out <dir>] [--target <triple>]
 *   AETHER_BUNDLE_MOCK=1  # 测试构建：额外包含 Mock 适配器（Mock 仅测试构建，ADR-008）
 */
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import { repoRoot, run } from "../test/lib/exec.mjs";

function argValue(flag, fallback) {
  const index = process.argv.indexOf(flag);
  return index >= 0 && process.argv[index + 1] ? process.argv[index + 1] : fallback;
}

function resolveBun() {
  if (process.env.AETHER_BUN) return process.env.AETHER_BUN;
  const exe = process.platform === "win32" ? "bun.exe" : "bun";
  const candidate = path.join(os.homedir(), ".bun", "bin", exe);
  if (existsSync(candidate)) return candidate;
  return "bun";
}

function packageVersion(packageDir) {
  const raw = readFileSync(path.join(repoRoot, packageDir, "package.json"), "utf8");
  return JSON.parse(raw).version ?? "0.0.0";
}

const exeSuffix = process.platform === "win32" ? ".exe" : "";
const outDir = path.resolve(
  repoRoot,
  argValue("--out", path.join("crates", "aether-tauri", "runtime-bundle")),
);
const target = argValue("--target", "");
const includeMock = process.env.AETHER_BUNDLE_MOCK === "1";
const bun = resolveBun();

/** 官方运行时集合三（ADR-008）+ 可选 Mock（仅测试构建）。 */
const runtimes = [
  {
    id: "claude-code",
    name: "Claude Code",
    kind: "claude-code",
    packageDir: "packages/adapter-claude-code",
    binaryName: "aether-claude-adapter",
    extraArgs: [],
  },
  {
    id: "codex",
    name: "Codex",
    kind: "codex",
    packageDir: "packages/adapter-codex",
    binaryName: "aether-codex-adapter",
    extraArgs: [],
  },
  {
    id: "deepseek-harness",
    name: "DeepSeek Harness",
    kind: "deepseek-harness",
    packageDir: "packages/adapter-dsh",
    binaryName: "aether-dsh-adapter",
    extraArgs: [],
  },
];
if (includeMock) {
  runtimes.push({
    id: "mock",
    name: "Mock（测试构建）",
    kind: "mock",
    packageDir: "packages/adapter-mock",
    binaryName: "aether-mock-adapter",
    extraArgs: [],
  });
}

rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });

const entries = [];
for (const runtime of runtimes) {
  const packageJson = path.join(repoRoot, runtime.packageDir, "package.json");
  const mainTs = path.join(repoRoot, runtime.packageDir, "src", "main.ts");
  if (!existsSync(packageJson) || !existsSync(mainTs)) {
    console.error(`[runtime-bundle] 缺少适配器包：${runtime.packageDir}`);
    process.exit(1);
  }
  const programName = `${runtime.binaryName}${exeSuffix}`;
  const programPath = path.join(outDir, programName);
  const args = ["build", mainTs, "--compile", "--outfile", programPath];
  if (target) args.push("--target", target);
  console.log(`$ ${bun} ${args.join(" ")}   # ${runtime.id}`);
  const code = run(bun, args, { cwd: repoRoot });
  if (code !== 0) {
    console.error(`[runtime-bundle] 适配器编译失败：${runtime.id}`);
    process.exit(code);
  }
  entries.push({
    id: runtime.id,
    name: runtime.name,
    kind: runtime.kind,
    version: packageVersion(runtime.packageDir),
    protocol: "1.0",
    program: programName,
    args: runtime.extraArgs,
  });
}

const manifest = { schema_version: 1, runtimes: entries };
writeFileSync(
  path.join(outDir, "runtimes.json"),
  `${JSON.stringify(manifest, null, 2)}\n`,
  "utf8",
);

const digests = {};
for (const entry of entries) {
  const data = readFileSync(path.join(outDir, entry.program));
  digests[entry.program] = `sha256:${createHash("sha256").update(data).digest("hex")}`;
}
writeFileSync(
  path.join(outDir, "digests.json"),
  `${JSON.stringify({ schema_version: 1, digests }, null, 2)}\n`,
  "utf8",
);

console.log(
  `[runtime-bundle] 产出 ${entries.length} 个运行时（目录 ${path.relative(repoRoot, outDir)}）：` +
    entries.map((entry) => entry.id).join(", ") +
    (includeMock ? "（含 Mock 测试构建）" : ""),
);
