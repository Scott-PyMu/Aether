/**
 * M1-06 DoD2 macOS 验证入口（macos-14 CI 专用；Gate 1 证据）。
 *
 * 步骤：
 *   1. 运行 `scripts/test/macos_sync_samples.sh --json` 构造真实样本目录
 *      （~/Library/CloudStorage/{Dropbox,GoogleDrive,OneDrive}、
 *       ~/Library/Mobile Documents、~/AetherTest/local）并归档清单；
 *   2. 运行 `cargo test -p aether-tauri --test m1_06_detection -- --nocapture`
 *      （注入样本 + macOS 原生上下文样本）；
 *   3. 断言：CloudStorage 三家 + iCloud 全部命中、本地对照放行、降级精度提示
 *      （「检测精度受限」+ 手动确认指引）存在；
 *   4. 归档测试输出与证据 JSON 到 `scripts/test/.tmp/m1-06-macos/<stamp>/`
 *      （CI 以 artifact `m1-06-macos-evidence` 上传）。
 *
 * 用法：node scripts/test/m1-06/verify-m1-06-macos.mjs
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot, summarize } from "../lib/exec.mjs";
import { buildEnv } from "../m1-08/env.mjs";

if (process.platform !== "darwin") {
  console.error(
    "[m1-06-macos] 本验证器仅可在 macOS 运行（M1-06 DoD2：mac runner 执行）",
  );
  process.exit(1);
}

const env = buildEnv();
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const outDir = path.join(repoRoot, "scripts", "test", ".tmp", "m1-06-macos", stamp);
mkdirSync(outDir, { recursive: true });

const checks = [];
const record = (name, ok, detail) =>
  checks.push({ name: detail ? `${name}（${detail}）` : name, exit: ok ? 0 : 1, expect: 0 });
const recordExit = (name, exit, expect = 0) => checks.push({ name, exit, expect });

// ---------------------------------------------------------------------------
// 1. 样本目录生成（脚本为唯一构造入口，测试内仅兜底）
// ---------------------------------------------------------------------------
const sampleScript = path.join(repoRoot, "scripts", "test", "macos_sync_samples.sh");
const sampleRun = spawnSync("bash", [sampleScript, "--json"], {
  cwd: repoRoot,
  env,
  encoding: "utf8",
});
process.stdout.write(sampleRun.stdout ?? "");
process.stderr.write(sampleRun.stderr ?? "");
writeFileSync(path.join(outDir, "samples.json"), sampleRun.stdout ?? "");
recordExit("样本生成脚本执行（macos_sync_samples.sh --json）", sampleRun.status ?? 1);

let samples = null;
try {
  samples = JSON.parse(sampleRun.stdout ?? "");
} catch {
  samples = null;
}
const sampleList = samples?.samples ?? [];
record(
  "样本清单 6 项（CloudStorage×3 + iCloud×2 + 本地对照×1）",
  sampleList.length === 6,
  `实际 ${sampleList.length}`,
);
for (const sample of sampleList) {
  record(`样本目录存在：${sample.id}`, existsSync(sample.path), sample.path);
}

// ---------------------------------------------------------------------------
// 2. 检测测试（原生上下文 + 注入样本）
// ---------------------------------------------------------------------------
const testRun = spawnSync(
  bin("cargo"),
  ["test", "-p", "aether-tauri", "--test", "m1_06_detection", "--", "--nocapture"],
  { cwd: repoRoot, env, encoding: "utf8" },
);
const output = `${testRun.stdout ?? ""}\n${testRun.stderr ?? ""}`;
process.stdout.write(output);
writeFileSync(path.join(outDir, "cargo-test-m1-06-detection.txt"), output);
recordExit("Rust 检测测试（macOS 原生 + 注入样本）", testRun.status ?? 1);

// ---------------------------------------------------------------------------
// 3. 断言样本命中 / 放行 / 降级提示
// ---------------------------------------------------------------------------
const requiredLines = [
  [
    "[m1-06] sample mac.file_provider_path（~/Library/CloudStorage/Dropbox）: hit (PASS)",
    "CloudStorage/Dropbox 命中（原生）",
  ],
  [
    "[m1-06] sample mac.file_provider_path（~/Library/CloudStorage/GoogleDrive）: hit (PASS)",
    "CloudStorage/GoogleDrive 命中（原生）",
  ],
  [
    "[m1-06] sample mac.file_provider_path（~/Library/CloudStorage/OneDrive）: hit (PASS)",
    "CloudStorage/OneDrive 命中（原生）",
  ],
  [
    "[m1-06] sample mac.icloud_ubiquitous（~/Library/Mobile Documents）: hit (PASS)",
    "iCloud 容器命中（原生；路径前缀降级）",
  ],
  [
    "[m1-06] macos-native control ~/AetherTest/local: allow (PASS)",
    "本地对照目录放行",
  ],
  ["[m1-06] macos-native summary: 5/5", "原生上下文样本 5/5"],
  ["[m1-06] macos-native precision-note:", "降级精度提示已输出"],
];
for (const [needle, label] of requiredLines) {
  record(label, output.includes(needle));
}
record("降级提示含「检测精度受限」", output.includes("检测精度受限"));
record(
  "降级提示含手动确认指引（请确认目录不在 iCloud/CloudStorage 下）",
  output.includes("请确认目录不在 iCloud/CloudStorage 下"),
);

// ---------------------------------------------------------------------------
// 4. 环境与证据归档
// ---------------------------------------------------------------------------
const swVers = spawnSync("sw_vers", [], { encoding: "utf8" });
const uname = spawnSync("uname", ["-a"], { encoding: "utf8" });
const rustc = spawnSync(bin("rustc"), ["-vV"], { encoding: "utf8" });
const evidence = {
  stamp,
  platform: process.platform,
  sw_vers: (swVers.stdout ?? "").trim(),
  uname: (uname.stdout ?? "").trim(),
  rustc: (rustc.stdout ?? "").trim(),
  samples,
  checks,
};
writeFileSync(
  path.join(outDir, "evidence.json"),
  `${JSON.stringify(evidence, null, 2)}\n`,
);
console.log(`[m1-06-macos] 证据归档：${outDir}`);
console.log(`[m1-06-macos] ${(swVers.stdout ?? "").trim()}`);

// 失败项单行输出（CI 日志 tail 可捕获）。
for (const check of checks) {
  const failed = !check.skip && (check.expect === "nonzero" ? check.exit === 0 : check.exit !== check.expect);
  if (failed) console.log(`[m1-06-macos] failed-check ${check.name}`);
}
// 不用 process.exit：避免管道输出未 flush 导致 CI 日志截断（诊断需要完整 FAIL 行）。
process.exitCode = summarize("verify-m1-06-macos", checks);
