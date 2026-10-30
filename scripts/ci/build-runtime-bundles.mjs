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

// M9-11 包体优化：三/四个适配器共享**一个** Bun 宿主二进制（各 92MB 运行时只保留
// 一份），通过 `--adapter <id>` 分发；线协议/参数/日志前缀不变。
// 参见 `packages/adapter-host/src/common.ts`。
const hostEntry = includeMock
  ? path.join(repoRoot, "packages", "adapter-host", "src", "main-test.ts")
  : path.join(repoRoot, "packages", "adapter-host", "src", "main.ts");
for (const runtime of runtimes) {
  const packageJson = path.join(repoRoot, runtime.packageDir, "package.json");
  if (!existsSync(packageJson)) {
    console.error(`[runtime-bundle] 缺少适配器包：${runtime.packageDir}`);
    process.exit(1);
  }
}
if (!existsSync(hostEntry)) {
  console.error(`[runtime-bundle] 缺少适配器宿主入口：${hostEntry}`);
  process.exit(1);
}

const hostName = `aether-adapter-host${exeSuffix}`;
const hostPath = path.join(outDir, hostName);
const hostArgs = ["build", hostEntry, "--compile", "--outfile", hostPath];
if (target) hostArgs.push("--target", target);
console.log(`$ ${bun} ${hostArgs.join(" ")}   # adapter-host（${includeMock ? "含 Mock" : "官方三运行时"}）`);
const hostCode = run(bun, hostArgs, { cwd: repoRoot });
if (hostCode !== 0) {
  console.error("[runtime-bundle] 适配器宿主编译失败");
  process.exit(hostCode);
}

const entries = runtimes.map((runtime) => ({
  id: runtime.id,
  name: runtime.name,
  kind: runtime.kind,
  version: packageVersion(runtime.packageDir),
  protocol: "1.0",
  program: hostName,
  args: ["--adapter", runtime.id, ...runtime.extraArgs],
}));

const manifest = { schema_version: 1, runtimes: entries };
writeFileSync(
  path.join(outDir, "runtimes.json"),
  `${JSON.stringify(manifest, null, 2)}\n`,
  "utf8",
);

const hostDigest = `sha256:${createHash("sha256").update(readFileSync(hostPath)).digest("hex")}`;
const digests = { [hostName]: hostDigest };
writeFileSync(
  path.join(outDir, "digests.json"),
  `${JSON.stringify({ schema_version: 1, digests }, null, 2)}\n`,
  "utf8",
);

console.log(
  `[runtime-bundle] 产出 ${entries.length} 个运行时（宿主 ${hostName}；目录 ${path.relative(repoRoot, outDir)}）：` +
    entries.map((entry) => entry.id).join(", ") +
    (includeMock ? "（含 Mock 测试构建）" : ""),
);
