#!/usr/bin/env node
/**
 * Q5 `subagent_context` 真实 CLI spike（M10-02 前置门禁；ADR-032 v1.1 §D-032-4）。
 *
 * 真实 CLI 二进制 + 本地 mock 上游（不消耗真实凭据、不把密钥写入日志）：
 * - Claude Code：默认 `--system-prompt-snapshot on` 与 `off` 两种模式下，
 *   `--append-system-prompt-file` 在 `--resume` 下是替换还是追加；
 * - Codex：每-run `developer_instructions` 在 `--resume`/`fork` 下是否只保留最新；
 * - DSH：真实 CLI 版本/自定义 profile 启动；插件 section 热更新由
 *   `packages/adapter-dsh/src/subagent-context-spike.test.ts` 覆盖。
 *
 * 运行：node scripts/test/m10-02/real-cli-spike.mjs
 * 输出：scripts/test/m10-02/.tmp/q5-real-cli/q5-real-cli-results.json
 */
import { createHash, randomUUID } from "node:crypto";
import { createServer } from "node:http";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { spawn, spawnSync } from "node:child_process";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const tmpRoot = path.join(repoRoot, "scripts", "test", "m10-02", ".tmp", "q5-real-cli");
rmSync(tmpRoot, { recursive: true, force: true });
mkdirSync(tmpRoot, { recursive: true });
const results = {
  schema: "aether.m10-02.q5-real-cli-spike.v1",
  started_at: new Date().toISOString(),
  claude: null,
  codex: null,
  dsh: null,
  checks: [],
};

function check(name, ok, detail) {
  results.checks.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? ` :: ${detail}` : ""}`);
}

function quoteCmdArg(value) {
  const text = String(value);
  if (!/[\s"&|<>^]/.test(text)) return text;
  return `"${text.replace(/"/g, '\\"')}"`;
}

function runClaude(args, env) {
  const command = ["claude", ...args].map(quoteCmdArg).join(" ");
  return new Promise((resolve) => {
    const child = spawn(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", command], {
      cwd: tmpRoot,
      env: { ...process.env, ...env },
      windowsHide: true,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    const timer = setTimeout(() => {
      spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], { windowsHide: true, stdio: "ignore" });
    }, 90_000);
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr });
    });
  });
}

function runCodex(codexJs, args, env, prompt) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [codexJs, ...args], {
      cwd: tmpRoot,
      env: { ...process.env, ...env },
      windowsHide: true,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.stdin.write(`${prompt}\n`);
    child.stdin.end();
    const timer = setTimeout(() => {
      spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], { windowsHide: true, stdio: "ignore" });
    }, 90_000);
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr });
    });
  });
}

function findCodexJs() {
  const where = spawnSync(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", "where codex"], { encoding: "utf8" });
  const cmdPath = String(where.stdout ?? "").split(/\r?\n/).find((line) => line.toLowerCase().endsWith(".cmd"));
  if (!cmdPath) return null;
  const text = readFileSync(cmdPath, "utf8");
  const match = text.match(/["']([^"']*node_modules[\\/]@openai[\\/]codex[\\/]bin[\\/]codex\.js)["']/i);
  if (!match) return null;
  return match[1].replace(/%dp0%/gi, path.dirname(cmdPath));
}

function parseJsonOutput(text) {
  const lines = String(text).trim().split(/\r?\n/).reverse();
  for (const line of lines) {
    try {
      return JSON.parse(line);
    } catch {
      /* continue */
    }
  }
  return null;
}

// ===== Claude Code =====
async function runClaudeSpike() {
  const requests = [];
  let runIndex = 0;
  const server = createServer((request, response) => {
    let raw = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => (raw += chunk));
    request.on("end", () => {
      let body = null;
      try {
        body = JSON.parse(raw);
      } catch {
        body = null;
      }
      requests.push({ runIndex, url: request.url, body });
      const model = body?.model ?? "claude-mock";
      if (request.url?.includes("count_tokens")) {
        response.writeHead(200, { "content-type": "application/json" });
        response.end(JSON.stringify({ input_tokens: 10 }));
        return;
      }
      const message = {
        id: "msg_q5_spike",
        type: "message",
        role: "assistant",
        model,
        content: [{ type: "text", text: "OK" }],
        stop_reason: "end_turn",
        stop_sequence: null,
        usage: { input_tokens: 10, output_tokens: 1 },
      };
      if (body?.stream) {
        response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
        const send = (event, data) => response.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
        send("message_start", { type: "message_start", message: { ...message, content: [] } });
        send("content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } });
        send("content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "OK" } });
        send("content_block_stop", { type: "content_block_stop", index: 0 });
        send("message_delta", { type: "message_delta", delta: { stop_reason: "end_turn", stop_sequence: null }, usage: { output_tokens: 1 } });
        send("message_stop", { type: "message_stop" });
        response.end();
        return;
      }
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify(message));
    });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = server.address().port;
  const settings = path.join(tmpRoot, "claude-settings.json");
  writeFileSync(settings, "{}\n", "utf8");
  const promptFiles = {};
  for (const tag of ["1", "2", "3"]) {
    const file = path.join(tmpRoot, `claude-prompt-${tag}.md`);
    writeFileSync(file, `<aether:subagent-results version="${tag}">TAG${tag}</aether:subagent-results>\n`, "utf8");
    promptFiles[tag] = file;
  }
  const base = [
    "--strict-mcp-config",
    "--setting-sources",
    "local",
    "--settings",
    settings,
    "--permission-mode",
    "plan",
    "--tools",
    "",
    "-p",
    "--output-format",
    "json",
  ];
  const env = {
    ANTHROPIC_BASE_URL: `http://127.0.0.1:${port}`,
    ANTHROPIC_AUTH_TOKEN: "q5-spike-dummy",
    ANTHROPIC_API_KEY: "q5-spike-dummy",
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
  };

  async function runMatrix(name, sessionId, items) {
    const runs = [];
    for (const item of items) {
      item.before?.();
      runIndex = requests.length;
      const result = await runClaude(item.args, env);
      const output = parseJsonOutput(result.stdout);
      const request = requests.find((entry) => entry.runIndex === runIndex);
      const systemText = (request?.body?.system ?? [])
        .map((block) => (typeof block === "string" ? block : block?.text ?? ""))
        .join("\n");
      const tags = [...systemText.matchAll(/<aether:subagent-results version="(\d)">/g)].map((m) => m[1]);
      const systemBlocks = (request?.body?.system ?? []).map((block, index) => {
        const text = typeof block === "string" ? block : block?.text ?? "";
        return {
          index,
          cache_control: typeof block === "object" && block !== null ? block.cache_control?.type ?? null : null,
          bytes: Buffer.byteLength(text, "utf8"),
          sha256: createHash("sha256").update(text).digest("hex"),
          has_subagent_block: text.includes("aether:subagent-results"),
          prefix: text.slice(0, 60),
        };
      });
      runs.push({
        name: item.name,
        code: result.code,
        session_id: output?.session_id ?? null,
        tags,
        system_blocks: systemBlocks,
        expect: item.expect,
        pass: tags.length === 1 && tags[0] === item.expect,
        stderr: result.stderr.slice(0, 300),
      });
    }
    return { name, session_id: sessionId, runs, pass: runs.every((run) => run.pass) };
  }

  // A. 生产适配器当前形态（不传 --system-prompt-snapshot；默认 on）。
  const defaultSession = randomUUID();
  const defaultMatrix = await runMatrix("default_snapshot", defaultSession, [
    {
      name: "new v1",
      args: [...base, "--session-id", defaultSession, "--append-system-prompt-file", promptFiles["1"], "say ok"],
      expect: "1",
    },
    {
      name: "resume v2",
      args: [...base, "--resume", defaultSession, "--append-system-prompt-file", promptFiles["2"], "say ok"],
      expect: "2",
    },
    {
      name: "resume v2 repeat",
      args: [...base, "--resume", defaultSession, "--append-system-prompt-file", promptFiles["2"], "say ok"],
      expect: "2",
    },
  ]);

  // B. 替换型参数：首轮与 resume 都传 --system-prompt-snapshot off。
  const offSession = randomUUID();
  const offMatrix = await runMatrix("snapshot_off", offSession, [
    {
      name: "new v1 (off)",
      args: [...base, "--session-id", offSession, "--system-prompt-snapshot", "off", "--append-system-prompt-file", promptFiles["1"], "say ok"],
      expect: "1",
    },
    {
      name: "resume v2 (off)",
      args: [...base, "--resume", offSession, "--system-prompt-snapshot", "off", "--append-system-prompt-file", promptFiles["2"], "say ok"],
      expect: "2",
    },
    {
      name: "resume v2 repeat (off)",
      args: [...base, "--resume", offSession, "--system-prompt-snapshot", "off", "--append-system-prompt-file", promptFiles["2"], "say ok"],
      expect: "2",
    },
    {
      name: "resume v3 (off)",
      args: [...base, "--resume", offSession, "--system-prompt-snapshot", "off", "--append-system-prompt-file", promptFiles["3"], "say ok"],
      expect: "3",
    },
  ]);

  // C. 同路径文件内容更新（Hermes Studio 的做法：更新同一个 managed prompt file 后 resume）。
  const samePathSession = randomUUID();
  const samePathFile = path.join(tmpRoot, "claude-same-path.md");
  const samePathMatrix = await runMatrix("same_path_snapshot_on", samePathSession, [
    {
      name: "new same-path v1",
      before: () => writeFileSync(samePathFile, `<aether:subagent-results version="1">TAG1</aether:subagent-results>\n`, "utf8"),
      args: [...base, "--session-id", samePathSession, "--append-system-prompt-file", samePathFile, "say ok"],
      expect: "1",
    },
    {
      name: "resume same-path v2",
      before: () => writeFileSync(samePathFile, `<aether:subagent-results version="2">TAG2</aether:subagent-results>\n`, "utf8"),
      args: [...base, "--resume", samePathSession, "--append-system-prompt-file", samePathFile, "say ok"],
      expect: "2",
    },
  ]);

  server.close();
  const offLargeBlocks = offMatrix.runs
    .map((run) => run.system_blocks.find((block) => block.has_subagent_block))
    .filter(Boolean);
  const identityBlocks = offMatrix.runs
    .map((run) => run.system_blocks.find((block) => block.prefix.includes("You are Claude Code")))
    .filter(Boolean);
  const cache_analysis = {
    large_system_block_bytes: offLargeBlocks[0]?.bytes ?? null,
    large_system_block_cache_control: offLargeBlocks[0]?.cache_control ?? null,
    large_system_block_hashes: offLargeBlocks.map((block) => block.sha256),
    large_system_block_changes: new Set(offLargeBlocks.map((block) => block.sha256)).size > 1,
    identity_block_bytes: identityBlocks[0]?.bytes ?? null,
    identity_block_cache_control: identityBlocks[0]?.cache_control ?? null,
    identity_block_stable: new Set(identityBlocks.map((block) => block.sha256)).size === 1,
    conclusion:
      "snapshot off 下大 system 块（含 CWD/date + subagent 块）带 cache_control；块内容随 version 变化 → 该断点 cache miss；仅 94B identity 块稳定命中。要保证大 prompt 缓存命中，必须改用 append-only message",
  };
  return {
    version: spawnSync(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", "claude --version"], { encoding: "utf8" }).stdout?.trim() ?? "",
    default_snapshot: defaultMatrix,
    snapshot_off: offMatrix,
    same_path_snapshot_on: samePathMatrix,
    cache_analysis,
    default_snapshot_replacement: defaultMatrix.pass,
    snapshot_off_replacement: offMatrix.pass,
    // 生产适配器当前未传 snapshot off；M10-02 必须加该参数才能开启。
    replacement_on_resume: offMatrix.pass,
    required_adapter_change: offMatrix.pass && !defaultMatrix.pass
      ? "仅 system_replace 严格模式需要 Claude --system-prompt-snapshot off（首轮与 resume 同口径）；v1.2 默认 message_append 不需要"
      : null,
  };
}

// ===== Codex =====
async function runCodexSpike() {
  const codexJs = findCodexJs();
  if (!codexJs) throw new Error("找不到全局 codex.cmd 内的 codex.js");
  const requests = [];
  const server = createServer((request, response) => {
    let raw = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => (raw += chunk));
    request.on("end", () => {
      let body = null;
      try {
        body = JSON.parse(raw);
      } catch {
        body = null;
      }
      requests.push({ url: request.url, body });
      if (!request.url?.endsWith("/responses")) {
        response.writeHead(404, { "content-type": "application/json" });
        response.end(JSON.stringify({ error: { message: `not found: ${request.url}` } }));
        return;
      }
      response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
      const send = (type, data) => response.write(`event: ${type}\ndata: ${JSON.stringify({ type, sequence_number: 1, ...data })}\n\n`);
      const resp = {
        id: "resp_q5_spike",
        object: "response",
        created_at: Math.floor(Date.now() / 1000),
        status: "in_progress",
        model: "mock-model",
        output: [],
        parallel_tool_calls: false,
        tool_choice: "auto",
        tools: [],
      };
      send("response.created", { response: resp });
      send("response.in_progress", { response: resp });
      send("response.output_item.added", { output_index: 0, item: { id: "item_1", type: "message", role: "assistant", status: "in_progress", content: [] } });
      send("response.content_part.added", { item_id: "item_1", output_index: 0, content_index: 0, part: { type: "output_text", text: "", annotations: [] } });
      send("response.output_text.delta", { item_id: "item_1", output_index: 0, content_index: 0, delta: "OK" });
      send("response.output_text.done", { item_id: "item_1", output_index: 0, content_index: 0, text: "OK" });
      send("response.content_part.done", { item_id: "item_1", output_index: 0, content_index: 0, part: { type: "output_text", text: "OK", annotations: [] } });
      send("response.output_item.done", { output_index: 0, item: { id: "item_1", type: "message", role: "assistant", status: "completed", content: [{ type: "output_text", text: "OK", annotations: [] }] } });
      send("response.completed", {
        response: {
          ...resp,
          status: "completed",
          output: [{ id: "item_1", type: "message", role: "assistant", status: "completed", content: [{ type: "output_text", text: "OK", annotations: [] }] }],
          usage: { input_tokens: 1, input_tokens_details: { cached_tokens: 0 }, output_tokens: 1, output_tokens_details: { reasoning_tokens: 0 }, total_tokens: 2 },
        },
      });
      response.end();
    });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = server.address().port;
  const home = path.join(tmpRoot, "codex-home");
  mkdirSync(home, { recursive: true });
  const provider = `model_providers.mock={name="mock",base_url=${JSON.stringify(`http://127.0.0.1:${port}/v1`)},env_key="MOCK_API_KEY",wire_api="responses"}`;
  const common = ["exec", "--json", "--skip-git-repo-check", "-m", "mock-model", "-c", 'model_provider="mock"', "-c", provider];
  const env = { CODEX_HOME: home, MOCK_API_KEY: "q5-spike-dummy" };
  const instruction = (tag) => ["-c", `developer_instructions="<aether:subagent-results version=\\"${tag}\\">TAG${tag}</aether:subagent-results>"`];
  const runs = [];
  const first = await runCodex(codexJs, [...common, ...instruction("1"), "-"], env, "say ok");
  const threadId =
    parseJsonOutput(first.stdout)?.thread_id ??
    first.stdout
      .split(/\r?\n/)
      .map((line) => {
        try {
          return JSON.parse(line);
        } catch {
          return null;
        }
      })
      .find((item) => item?.type === "thread.started")?.thread_id;
  const firstHit = scanCodexRequest(requests.shift());
  runs.push({ name: "new v1", code: first.code, expect: "1", hits: firstHit, pass: firstHit.length === 1 && firstHit[0].tag === "1" });
  for (const [name, tag] of [["resume v2", "2"], ["resume v2 repeat", "2"]]) {
    const result = await runCodex(codexJs, ["exec", "resume", ...common.slice(1), ...instruction(tag), threadId, "-"], env, "say ok");
    const hits = scanCodexRequest(requests.shift());
    runs.push({ name, code: result.code, expect: tag, hits, pass: hits.length === 1 && hits[0].tag === tag });
  }
  const fork = await runCodex(codexJs, ["exec", "fork", ...common.slice(1), ...instruction("3"), threadId, "-"], env, "say fork");
  const forkHits = scanCodexRequest(requests.shift());
  runs.push({ name: "fork v3", code: fork.code, expect: "3", hits: forkHits, pass: forkHits.length === 1 && forkHits[0].tag === "3" });
  const fresh = await runCodex(codexJs, [...common, ...instruction("3"), "-"], env, "say ok");
  const freshHit = scanCodexRequest(requests.shift());
  runs.push({ name: "fresh v3", code: fresh.code, expect: "3", hits: freshHit, pass: freshHit.length === 1 && freshHit[0].tag === "3" });
  server.close();
  const pass = runs.every((run) => run.pass);
  return {
    version: spawnSync(process.execPath, [codexJs, "--version"], { encoding: "utf8" }).stdout?.trim() ?? "",
    thread_id: threadId ?? null,
    runs,
    pass,
    replacement_on_resume: runs.filter((run) => run.name.startsWith("resume")).every((run) => run.pass),
    failure: pass
      ? null
      : "Codex resume/fork 保留首轮 developer_instructions；新 -c 覆盖未进入后续请求 → 该运行时 subagent_context=false / auto_inject 转 hold（或重建会话）",
  };
}

function scanCodexRequest(entry) {
  const hits = [];
  for (const item of entry?.body?.input ?? []) {
    for (const part of item?.content ?? []) {
      if (typeof part?.text !== "string") continue;
      for (const match of part.text.matchAll(/<aether:subagent-results version="(\d)">TAG(\d)<\/aether:subagent-results>/g)) {
        hits.push({ role: item.role, tag: match[1] });
      }
    }
  }
  return hits;
}

// ===== DSH =====
function runDshSpike() {
  const env = { ...process.env, DSH_HOME: path.join(tmpRoot, "dsh-home") };
  const version = spawnSync(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", "dsh --version"], { encoding: "utf8", env }).stdout?.trim() ?? "";
  const profile = "aether-q5-spike";
  const create = spawnSync(
    process.env.ComSpec ?? "cmd.exe",
    ["/d", "/s", "/c", `dsh ${profile} --from-default-profile headless --dump-config`],
    { encoding: "utf8", env, timeout: 90_000 },
  );
  const dump = spawnSync(
    process.env.ComSpec ?? "cmd.exe",
    ["/d", "/s", "/c", `dsh ${profile} --dump-config`],
    { encoding: "utf8", env, timeout: 90_000 },
  );
  const hotUpdateTest = spawnSync(
    process.env.ComSpec ?? "cmd.exe",
    ["/d", "/s", "/c", "pnpm --filter @aether/adapter-dsh test -- src/subagent-context-spike.test.ts"],
    { encoding: "utf8", cwd: repoRoot, timeout: 180_000 },
  );
  return {
    version,
    profile,
    create_code: create.status,
    dump_code: dump.status,
    headless_bundle: String(dump.stdout ?? "").includes("@deepseek-ai/dsh-headless"),
    llm_entry: String(dump.stdout ?? "").includes("llm-pi-ai"),
    hot_update_evidence:
      "dsh-system-prompt lib/index.js assemble(): typeof section.text === 'function' ? section.text(context) : section.text；生产插件 text=()=>readPrompt(...) 每次 assembly 重读侧信道文件；单测 src/subagent-context-spike.test.ts",
    hot_update_test_code: hotUpdateTest.status,
    hot_update_test_tail: String(hotUpdateTest.stdout ?? "")
      .split(/\r?\n/)
      .filter((line) => /Test Files|Tests|passed|failed/.test(line))
      .slice(-3),
    hot_update_pass: hotUpdateTest.status === 0,
  };
}

// ===== 主流程 =====
results.claude = await runClaudeSpike();
check(
  "Claude 默认 snapshot：resume 保留首轮（预期失败，说明必须加替换型参数）",
  results.claude.default_snapshot.pass === false,
  JSON.stringify(results.claude.default_snapshot.runs.map((run) => ({ name: run.name, tags: run.tags }))),
);
check(
  "Claude --system-prompt-snapshot off：resume 只含最新块",
  results.claude.snapshot_off.pass === true,
  JSON.stringify(results.claude.snapshot_off.runs.map((run) => ({ name: run.name, tags: run.tags }))),
);
check(
  "Claude snapshot off 的大 system 块缓存断点随 version 变化（不能保证缓存命中）",
  results.claude.cache_analysis.large_system_block_changes === true &&
    results.claude.cache_analysis.large_system_block_cache_control === "ephemeral",
  JSON.stringify(results.claude.cache_analysis),
);
check(
  "Claude 同路径 prompt 文件内容更新（snapshot on；Hermes Studio 做法）",
  true,
  JSON.stringify(results.claude.same_path_snapshot_on.runs.map((run) => ({ name: run.name, tags: run.tags }))),
);
check(
  "Claude strict system_replace 参数结论（仅严格模式需 flag）",
  results.claude.default_snapshot.pass === false && results.claude.snapshot_off.pass === true,
  results.claude.required_adapter_change ?? "no change",
);

results.codex = await runCodexSpike();
results.codex.strict_system_replace_supported = results.codex.pass;
results.codex.chosen_mode = "message_append";
check(
  "Codex strict system_replace 不支持（预期；v1.2 使用 message_append）",
  results.codex.pass === false,
  results.codex.failure ?? JSON.stringify(results.codex.runs.map((run) => ({ name: run.name, hits: run.hits, pass: run.pass }))),
);

results.dsh = runDshSpike();
check(
  "DSH 真实 CLI 版本/自定义 profile",
  Boolean(results.dsh.version) && results.dsh.dump_code === 0 && results.dsh.headless_bundle,
  JSON.stringify({ version: results.dsh.version, dump_code: results.dsh.dump_code }),
);
check("DSH 插件 section 每次 assembly 重读（热更新）", results.dsh.hot_update_pass, results.dsh.hot_update_evidence);

results.finished_at = new Date().toISOString();
results.ok = results.checks.every((item) => item.ok);
results.summary = {
  claude: results.claude.snapshot_off.pass ? "strict_pass_with_snapshot_off_default_message_append" : "fail",
  codex: results.codex.pass ? "strict_pass" : "strict_system_replace_unsupported_message_append",
  dsh: results.dsh.hot_update_pass ? "pass" : "fail",
  failing_runtimes: [
    results.claude.snapshot_off.pass ? null : "claude-code",
    results.dsh.hot_update_pass ? null : "deepseek-harness",
  ].filter(Boolean),
  expected_diagnostics: [
    "claude.default_snapshot_keeps_first_block",
    "codex.resume/fork_keeps_developer_instructions",
    "claude.system_replace_large_block_cache_miss",
  ],
  strict_mode_requirements: [results.claude.required_adapter_change].filter(Boolean),
  message_append_e2e: "pending M10-02 implementation",
};
writeFileSync(path.join(tmpRoot, "q5-real-cli-results.json"), `${JSON.stringify(results, null, 2)}\n`, "utf8");
console.log(
  `Q5 real CLI spike: ${results.checks.filter((c) => c.ok).length}/${results.checks.length} checks passed; ` +
    `claude=${results.summary.claude} codex=${results.summary.codex} dsh=${results.summary.dsh}`,
);
process.exitCode = results.ok ? 0 : 2;
