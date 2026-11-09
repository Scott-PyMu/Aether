#!/usr/bin/env node
/**
 * M10-02 真实供应商交付语义验证（使用 dsh_env.txt 中的 DeepSeek 凭据）。
 *
 * 不把密钥写入参数/日志/结果：只从文件读取后放入子进程 env。
 * 使用真实 Claude Code / Codex / DSH 二进制 + DeepSeek 真实 API：
 * - message_append：父 secret 可回忆；子结果 secret 可回忆；v2 后 v1/v2 均可见；
 * - origin_fork：新会话只含 origin 快照 + 结果，父 secret 不可回忆。
 *
 * 运行：node scripts/test/m10-02/real-provider-delivery.mjs
 */
import { createHash, randomUUID } from "node:crypto";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { spawn, spawnSync } from "node:child_process";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const tmpRoot = path.join(
  process.env.TEMP ?? process.env.TMP ?? repoRoot,
  "aether-m10-02-real-provider",
);
rmSync(tmpRoot, { recursive: true, force: true });
mkdirSync(tmpRoot, { recursive: true });

const keyFile = process.env.AETHER_DSH_ENV_FILE ?? path.join(repoRoot, "dsh_env.txt");
const lines = readFileSync(keyFile, "utf8").split(/\r?\n/);
const key = (lines.find((line) => /^API-KEY:/.test(line)) ?? "").replace(/^API-KEY:/, "").trim();
const configuredModel = (lines.find((line) => /^model:/.test(line)) ?? "").replace(/^model:/, "").trim();
if (!key) {
  console.error(`缺少 API key（${keyFile}）`);
  process.exit(2);
}
const model = process.env.AETHER_REAL_MODEL ?? (configuredModel === "deepseek-v4.1-flash" ? "deepseek-flash" : configuredModel || "deepseek-flash");

const checks = [];
function check(name, ok, detail = "") {
  checks.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? ` :: ${detail}` : ""}`);
}
function sha(text) {
  return createHash("sha256").update(String(text)).digest("hex");
}
function quoteCmdArg(value) {
  const text = String(value);
  if (!/[\s"&|<>^]/.test(text)) return text;
  return `"${text.replace(/"/g, '\\"')}"`;
}
function parseLastJson(text) {
  for (const line of String(text).trim().split(/\r?\n/).reverse()) {
    try {
      return JSON.parse(line);
    } catch {
      /* continue */
    }
  }
  return null;
}
function parseJsonLines(text) {
  const items = [];
  for (const line of String(text).split(/\r?\n/)) {
    if (!line.trim()) continue;
    try {
      items.push(JSON.parse(line));
    } catch {
      /* ignore */
    }
  }
  return items;
}
function runProcess(command, args, options = {}) {
  return new Promise((resolve) => {
    const child = spawn(command, args, {
      cwd: options.cwd ?? tmpRoot,
      env: { ...process.env, ...options.env },
      windowsHide: true,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    if (options.stdin !== undefined) child.stdin.write(options.stdin);
    child.stdin.end();
    const timer = setTimeout(() => {
      spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], {
        windowsHide: true,
        stdio: "ignore",
      });
    }, options.timeoutMs ?? 300_000);
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr });
    });
  });
}
function runCmd(commandLine, options = {}) {
  return new Promise((resolve) => {
    const child = spawn(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", commandLine], {
      cwd: options.cwd ?? tmpRoot,
      env: { ...process.env, ...options.env },
      windowsHide: true,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    if (options.stdin !== undefined) child.stdin.write(options.stdin);
    child.stdin.end();
    const timer = setTimeout(() => {
      spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], {
        windowsHide: true,
        stdio: "ignore",
      });
    }, options.timeoutMs ?? 300_000);
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr });
    });
  });
}
function envelope(version, code) {
  return `<aether:subagent-results version="${version}">{"source":"subagent","verdict":"approve","summary":"The subagent result secret code is ${code}.","result_refs":["subagent:${code}"]}</aether:subagent-results>`;
}

// ===== Claude Code =====
async function runClaude() {
  const env = {
    ANTHROPIC_BASE_URL: "https://api.deepseek.com/anthropic",
    ANTHROPIC_AUTH_TOKEN: key,
    ANTHROPIC_API_KEY: key,
    ANTHROPIC_MODEL: model,
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
  };
  const base = [
    "--strict-mcp-config",
    "--setting-sources",
    "local",
    "--permission-mode",
    "plan",
    "--tools",
    "",
    "--append-system-prompt",
    "Do not call any tools. Answer only from the provided conversation and context.",
    "-p",
    "--output-format",
    "json",
    "--model",
    model,
  ];
  const session = randomUUID();
  const runs = [];
  async function run(name, args, prompt) {
    const command = ["claude", ...args].map(quoteCmdArg).join(" ");
    const result = await runCmd(command, { env, stdin: prompt, timeoutMs: 300_000 });
    const parsed = parseLastJson(result.stdout);
    const text = String(parsed?.result ?? result.stdout);
    runs.push({ name, code: result.code, text, stderr: result.stderr.slice(0, 200) });
    return text;
  }
  await run("parent", [...base, "--session-id", session], "Remember the secret word PARENT_ALPHA. Reply exactly: OK");
  const firstCode = await run(
    "message_append v1",
    [...base, "--resume", session],
    `Subagent result follows.\n${envelope(1, "RESULT_BRAVO")}\nWhat is the secret code in the subagent result? Reply only the code.`,
  );
  const bothCodes = await run(
    "message_append v2",
    [...base, "--resume", session],
    `Another subagent result follows.\n${envelope(2, "RESULT_CHARLIE")}\nList both subagent secret codes in version order, separated by comma, and nothing else.`,
  );
  const parentWord = await run(
    "parent history",
    [...base, "--resume", session],
    "What is the parent secret word? Reply only the word.",
  );
  check("Claude 真实 message_append v1 可见", firstCode.includes("RESULT_BRAVO"), firstCode.trim());
  check(
    "Claude 真实 message_append v2 保留 v1 且最新生效",
    bothCodes.includes("RESULT_BRAVO") && bothCodes.includes("RESULT_CHARLIE"),
    bothCodes.trim(),
  );
  check("Claude 真实父历史保留", parentWord.includes("PARENT_ALPHA"), parentWord.trim());

  const forkSession = randomUUID();
  const forkText = await run(
    "origin_fork",
    [...base, "--session-id", forkSession],
    "The frozen origin word is ZEBRA7. The fork subagent result code is FORK_ECHO. Reply only with ZEBRA7,FORK_ECHO and do not use tools.",
  );
  check(
    "Claude 真实 origin_fork 只使用 origin+结果",
    forkText.includes("ZEBRA7") && forkText.includes("FORK_ECHO") && !forkText.includes("PARENT_ALPHA"),
    forkText.trim(),
  );
  return { session, runs, pass: runs.every((run) => run.code === 0) };
}

// ===== Codex =====
function findCodexJs() {
  const where = spawnSync(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", "where codex"], {
    encoding: "utf8",
  });
  const cmdPath = String(where.stdout ?? "")
    .split(/\r?\n/)
    .find((line) => line.toLowerCase().endsWith(".cmd"));
  if (!cmdPath) return null;
  const text = readFileSync(cmdPath, "utf8");
  const match = text.match(
    /["']([^"']*node_modules[\\/]@openai[\\/]codex[\\/]bin[\\/]codex\.js)["']/i,
  );
  if (!match) return null;
  return match[1].replace(/%dp0%/gi, path.dirname(cmdPath));
}
async function runCodex() {
  const codexJs = findCodexJs();
  if (!codexJs) throw new Error("找不到 codex.js");
  const codexHome = path.join(tmpRoot, "codex-home");
  mkdirSync(codexHome, { recursive: true });
  writeFileSync(
    path.join(codexHome, "config.toml"),
    [
      `model = "${model}"`,
      'model_provider = "deepseek"',
      "",
      "[model_providers.deepseek]",
      'name = "DeepSeek"',
      'base_url = "https://api.deepseek.com/v1"',
      'env_key = "DEEPSEEK_API_KEY"',
      'wire_api = "responses"',
      "",
    ].join("\n"),
    "utf8",
  );
  const env = { CODEX_HOME: codexHome, DEEPSEEK_API_KEY: key };
  const common = ["--json", "--skip-git-repo-check"];
  const runs = [];
  async function run(name, args, prompt) {
    const outFile = path.join(tmpRoot, `codex-${runs.length}.txt`);
    const result = await runProcess(
      process.execPath,
      [codexJs, ...args, "-o", outFile, "-"],
      { env, stdin: prompt, cwd: tmpRoot, timeoutMs: 300_000 },
    );
    const text = (() => {
      try {
        return readFileSync(outFile, "utf8").trim();
      } catch {
        return String(result.stdout ?? "").trim();
      }
    })();
    runs.push({ name, code: result.code, text, stderr: result.stderr.slice(0, 200) });
    return { text, stdout: result.stdout };
  }
  const parent = await run(
    "parent",
    ["exec", ...common],
    "Remember the secret word PARENT_ALPHA. Reply exactly: OK",
  );
  const threadId = parseJsonLines(parent.stdout ?? "").find(
    (item) => item?.type === "thread.started",
  )?.thread_id;
  if (!threadId) throw new Error("Codex 未返回 thread_id");
  async function resume(name, prompt) {
    const outFile = path.join(tmpRoot, `codex-${runs.length}.txt`);
    const result = await runProcess(
      process.execPath,
      [codexJs, "exec", "resume", ...common, "-o", outFile, threadId, "-"],
      { env, stdin: prompt, cwd: tmpRoot, timeoutMs: 300_000 },
    );
    const text = (() => {
      try {
        return readFileSync(outFile, "utf8").trim();
      } catch {
        return String(result.stdout ?? "").trim();
      }
    })();
    runs.push({ name, code: result.code, text, stderr: result.stderr.slice(0, 200) });
    return text;
  }
  const firstCode = await resume(
    "message_append v1",
    `Subagent result follows.\n${envelope(1, "RESULT_BRAVO")}\nWhat is the secret code in the subagent result? Reply only the code.`,
  );
  const bothCodes = await resume(
    "message_append v2",
    `Another subagent result follows.\n${envelope(2, "RESULT_CHARLIE")}\nList both subagent secret codes in version order, separated by comma, and nothing else.`,
  );
  const parentWord = await resume(
    "parent history",
    "What is the parent secret word? Reply only the word.",
  );
  check("Codex 真实 message_append v1 可见", firstCode.includes("RESULT_BRAVO"), firstCode);
  check(
    "Codex 真实 message_append v2 保留 v1 且最新生效",
    bothCodes.includes("RESULT_BRAVO") && bothCodes.includes("RESULT_CHARLIE"),
    bothCodes,
  );
  check("Codex 真实父历史保留", parentWord.includes("PARENT_ALPHA"), parentWord);

  const fork = await run(
    "origin_fork",
    ["exec", ...common],
    `[origin_snapshot] ZEBRA7\n${envelope(1, "FORK_ECHO")}\nWhat are the origin word and the fork secret code? Reply only: ORIGIN,FORK`,
  );
  check(
    "Codex 真实 origin_fork 只使用 origin+结果",
    fork.text.includes("ZEBRA7") &&
      fork.text.includes("FORK_ECHO") &&
      !fork.text.includes("PARENT_ALPHA"),
    fork.text,
  );
  return { thread_id: threadId, runs, pass: runs.every((run) => run.code === 0) };
}

// ===== DSH =====
function findDshJs() {
  const where = spawnSync(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", "where dsh"], {
    encoding: "utf8",
  });
  const cmdPath = String(where.stdout ?? "")
    .split(/\r?\n/)
    .find((line) => line.toLowerCase().endsWith(".cmd"));
  if (cmdPath) {
    const text = readFileSync(cmdPath, "utf8");
    const match = text.match(
      /["']([^"']*node_modules[\\/]@deepseek-ai[\\/]dsh[\\/]lib[\\/]bin\.js)["']/i,
    );
    if (match) return match[1].replace(/%dp0%/gi, path.dirname(cmdPath));
  }
  return path.join(
    process.env.APPDATA ?? "",
    "npm",
    "node_modules",
    "@deepseek-ai",
    "dsh",
    "lib",
    "bin.js",
  );
}

async function runDsh() {
  const dshHome = path.join(tmpRoot, "dsh-home");
  mkdirSync(dshHome, { recursive: true });
  const profile = "aether-real-delivery";
  const env = { DSH_HOME: dshHome, DEEPSEEK_API_KEY: key };
  const create = spawnSync(
    process.env.ComSpec ?? "cmd.exe",
    ["/d", "/s", "/c", `dsh ${profile} --from-default-profile headless --dump-config`],
    { encoding: "utf8", env: { ...process.env, ...env }, timeout: 120_000 },
  );
  if (create.status !== 0) throw new Error(`DSH profile 创建失败：${create.stderr}`);
  const patch = path.join(tmpRoot, "dsh-patch.yml");
  writeFileSync(
    patch,
    [
      "- id: llm-pi-ai",
      "  config:",
      "    providers:",
      "      deepseek:",
      "        displayName: DeepSeek",
      "        apiKeyEnv: DEEPSEEK_API_KEY",
      "        api: openai-completions",
      "        baseURL: https://api.deepseek.com/v1",
      "        models:",
      `          - id: ${model}`,
      `            name: ${model}`,
      "- id: agent-default-model",
      "  config:",
      "    provider: deepseek",
      `    model: ${model}`,
      "",
    ].join("\n"),
    "utf8",
  );
  const base = ["--profile", profile, "--patch", patch, "headless"];
  const dshJs = findDshJs();
  const runs = [];
  async function run(name, prompt, sessionId) {
    const args = [...base, "--json"];
    if (sessionId) args.push("--session-id", sessionId);
    args.push(prompt);
    const result = await runProcess(process.execPath, [dshJs, ...args], {
      env,
      timeoutMs: 300_000,
    });
    const events = parseJsonLines(result.stdout);
    const text = String(
      events.find((event) => event?.type === "final")?.text ??
        result.stdout,
    ).trim();
    const id = events.find((event) => event?.type === "session")?.sessionId;
    runs.push({ name, code: result.code, text, stderr: result.stderr.slice(0, 200) });
    return { text, sessionId: id };
  }
  const parent = await run("parent", "Remember the secret word PARENT_ALPHA. Reply exactly: OK");
  if (!parent.sessionId) throw new Error("DSH 未返回 sessionId");
  const v1 = await run(
    "message_append v1",
    `Subagent result follows.\n${envelope(1, "RESULT_BRAVO")}\nWhat is the secret code in the subagent result? Reply only the code.`,
    parent.sessionId,
  );
  const v2 = await run(
    "message_append v2",
    `Another subagent result follows.\n${envelope(2, "RESULT_CHARLIE")}\nList both subagent secret codes in version order, separated by comma, and nothing else.`,
    parent.sessionId,
  );
  const parentWord = await run(
    "parent history",
    "What is the parent secret word? Reply only the word.",
    parent.sessionId,
  );
  check("DSH 真实 message_append v1 可见", v1.text.includes("RESULT_BRAVO"), v1.text);
  check(
    "DSH 真实 message_append v2 保留 v1 且最新生效",
    v2.text.includes("RESULT_BRAVO") && v2.text.includes("RESULT_CHARLIE"),
    v2.text,
  );
  check("DSH 真实父历史保留", parentWord.text.includes("PARENT_ALPHA"), parentWord.text);
  const fork = await run(
    "origin_fork",
    `[origin_snapshot] ZEBRA7\n${envelope(1, "FORK_ECHO")}\nWhat are the origin word and the fork secret code? Reply only: ORIGIN,FORK`,
  );
  check(
    "DSH 真实 origin_fork 只使用 origin+结果",
    fork.text.includes("ZEBRA7") &&
      fork.text.includes("FORK_ECHO") &&
      !fork.text.includes("PARENT_ALPHA"),
    fork.text,
  );
  return { session_id: parent.sessionId, runs, pass: runs.every((run) => run.code === 0) };
}

const results = { schema: "aether.m10-02.real-provider-delivery.v1", model, started_at: new Date().toISOString(), claude: null, codex: null, dsh: null, checks: [] };
try {
  results.claude = await runClaude();
  results.codex = await runCodex();
  results.dsh = await runDsh();
} catch (error) {
  check("真实供应商交付测试执行", false, String(error?.message ?? error));
} finally {
  results.checks = checks;
  results.passed = checks.filter((item) => item.ok).length;
  results.failed = checks.filter((item) => !item.ok).length;
  writeFileSync(
    path.join(tmpRoot, "results.json"),
    `${JSON.stringify(results, null, 2)}\n`,
    "utf8",
  );
}
console.log(`\n真实供应商交付：${results.passed}/${checks.length} checks passed（model=${model}）`);
process.exit(results.failed === 0 ? 0 : 1);
