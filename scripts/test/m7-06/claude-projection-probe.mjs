#!/usr/bin/env node
/**
 * M7-06/ADR-024 DoD1 实测：Claude Code 档位投影等价性（工作区边界）。
 *
 * 口径：真实 Claude Code CLI（隔离 settings；核心消费映射同款 env）+ 记录型代理
 * （真实上游）逐档执行写文件探针：
 * - `workspace_write`（`--permission-mode acceptEdits`）：区内写 **允许**、区外写 **拒绝**；
 * - `danger_full_access`（`bypassPermissions`）：区外写 **允许**；
 * - `read_only`（`plan`）：区内写 **拒绝**。
 * 任一不符 → 退出码 1（等价性不成立须按 ADR-014 §6-4 置灰/降级；禁止静默按部分等价实现）。
 *
 * 依赖（opt-in；缺任一 → 打印显式 SKIP 并退出码 3，禁止静默跳过）：
 *   AETHER_M7_06_CLAUDE_UPSTREAM / AETHER_M7_06_CLAUDE_TOKEN / AETHER_M7_06_CLAUDE_MODEL
 *   （或本地凭据文件 claud_test_env.txt；模型须为网关可用名）。
 *
 * 产物：`<evidence>/claude-projection.json`（逐档结果；证据归档用）。
 * 运行：node scripts/test/m7-06/claude-projection-probe.mjs
 */
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import readline from "node:readline";

import { repoRoot } from "../lib/exec.mjs";

function parseEnvFile(file, { keyPattern, urlLike }) {
  const filePath = path.join(repoRoot, file);
  if (!existsSync(filePath)) return null;
  const lines = readFileSync(filePath, "utf8")
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
  const token = lines.find((line) => keyPattern.test(line))?.split(":").slice(1).join(":").trim();
  const url = lines.find((line) => urlLike.test(line))?.match(/https?:\/\/\S+/)?.[0];
  const model = lines.find((line) => /^model:/i.test(line))?.split(":").slice(1).join(":").trim();
  if (!token || !url) return null;
  return { token, url, model };
}

const upstream = process.env.AETHER_M7_06_CLAUDE_UPSTREAM
  ? {
      url: process.env.AETHER_M7_06_CLAUDE_UPSTREAM,
      token: process.env.AETHER_M7_06_CLAUDE_TOKEN,
      model: process.env.AETHER_M7_06_CLAUDE_MODEL,
    }
  : parseEnvFile("claud_test_env.txt", { keyPattern: /^api-?key/i, urlLike: /https?:\/\//i });
if (!upstream || !upstream.url || !upstream.token) {
  console.log("SKIP（显式）：claude-projection-probe 缺少 AETHER_M7_06_CLAUDE_UPSTREAM/_TOKEN（或 claud_test_env.txt）");
  process.exit(3);
}
const model = upstream.model;
if (!model) {
  console.log("SKIP（显式）：claude-projection-probe 缺少模型（AETHER_M7_06_CLAUDE_MODEL）");
  process.exit(3);
}

const evidenceDir =
  process.env.AETHER_M7_06_EVIDENCE_DIR ??
  path.join(repoRoot, "scripts", "test", ".tmp", "m7-06", "projection-probe");
mkdirSync(evidenceDir, { recursive: true });
const root = path.join(evidenceDir, "projection-probe-workspace");
rmSync(root, { recursive: true, force: true });
const outside = path.join(root, "outside");
const inside = path.join(root, "inside-workspace");
mkdirSync(outside, { recursive: true });
mkdirSync(inside, { recursive: true });

const baseUrl = new URL(upstream.url);
const testKey = `sk-ant-m7-06-projection-${Date.now()}`;
const proxyLog = path.join(evidenceDir, "claude-projection-proxy.jsonl");
const proxy = spawn(
  "node",
  [
    path.join(repoRoot, "scripts", "test", "m7-06", "recording-proxy.mjs"),
    "--listen",
    "0",
    "--log",
    proxyLog,
    "--runtime",
    "projection",
    "--upstream",
    baseUrl.origin,
    "--token",
    upstream.token,
    "--expect-key",
    testKey,
  ],
  { stdio: ["ignore", "pipe", "inherit"], windowsHide: true },
);
const proxyPort = await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("代理启动超时")), 15000);
  readline.createInterface({ input: proxy.stdout }).once("line", (line) => {
    clearTimeout(timer);
    resolve(JSON.parse(line).port);
  });
});
const pathname = baseUrl.pathname.replace(/\/+$/, "");
const prefix = pathname.endsWith("/v1") ? pathname.slice(0, -3) : pathname;

function runClaude(permissionMode, prompt) {
  return new Promise((resolve) => {
    const settingsFile = path.join(root, `isolated-${permissionMode}.json`);
    writeFileSync(settingsFile, "{}\n", "utf8");
    const child = spawn(
      process.env.ComSpec || "cmd.exe",
      [
        "/d",
        "/s",
        "/c",
        "claude",
        "-p",
        "--setting-sources",
        "local",
        "--settings",
        settingsFile,
        "--permission-mode",
        permissionMode,
        "--model",
        model,
        prompt,
      ],
      {
        env: {
          ...process.env,
          ANTHROPIC_BASE_URL: `http://127.0.0.1:${proxyPort}${prefix}`,
          ANTHROPIC_API_KEY: testKey,
          ANTHROPIC_MODEL: model,
        },
        cwd: inside,
        windowsHide: true,
        stdio: ["ignore", "pipe", "pipe"],
      },
    );
    let output = "";
    child.stdout.on("data", (chunk) => (output += chunk.toString()));
    child.stderr.on("data", (chunk) => (output += chunk.toString()));
    child.once("close", () => resolve(output.slice(0, 800)));
  });
}

const insideFile = path.join(inside, "inside.txt");
const outsideFile = path.join(outside, "outside.txt");
const results = {};

// 1) workspace_write：区内允许 / 区外拒绝。
rmSync(insideFile, { force: true });
rmSync(outsideFile, { force: true });
results.workspace_write_output = await runClaude(
  "acceptEdits",
  `Use the Write tool to create two files: (1) ${insideFile} with content INSIDE-OK; (2) ${outsideFile} with content OUTSIDE-OK. If writing a file is denied by permissions, do not retry it; finish and reply with a single line: DONE inside=<yes|no> outside=<yes|no>.`,
);
results.workspace_write = { inside_written: existsSync(insideFile), outside_written: existsSync(outsideFile) };

// 2) danger_full_access：区外允许。
rmSync(outsideFile, { force: true });
results.danger_full_access_output = await runClaude(
  "bypassPermissions",
  `Use the Write tool to create the file ${outsideFile} with content OUTSIDE-OK. Finish and reply with a single line: DONE.`,
);
results.danger_full_access = { outside_written: existsSync(outsideFile) };

// 3) read_only：写拒绝。
rmSync(insideFile, { force: true });
results.read_only_output = await runClaude(
  "plan",
  `Use the Write tool to create the file ${insideFile} with content INSIDE-OK. If it is denied, do not retry; reply with a single line: DONE.`,
);
results.read_only = { inside_written: existsSync(insideFile) };

const verdict = {
  task: "M7-06",
  probe: "claude-projection",
  model,
  checked_at: new Date().toISOString(),
  results,
  equivalent:
    results.workspace_write.inside_written === true &&
    results.workspace_write.outside_written === false &&
    results.danger_full_access.outside_written === true &&
    results.read_only.inside_written === false,
};
writeFileSync(path.join(evidenceDir, "claude-projection.json"), `${JSON.stringify(verdict, null, 2)}\n`, "utf8");
console.log(JSON.stringify(verdict, null, 2));
try {
  proxy.kill();
} catch {
  /* 已退出 */
}
process.exit(verdict.equivalent ? 0 : 1);
