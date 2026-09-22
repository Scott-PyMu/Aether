/**
 * M2-11 真实运行时适配器 E2E 驱动（ADR-008 §6）：
 * 直接 spawn 编译后的适配器单文件，经 Aether 线协议（D6）驱动真实运行时
 * （Codex CLI / DSH ACP），完成初始化、流式、Mode R 恢复、中断与 N 次回归。
 *
 * 与 verify-m2-11 的夹具用例分工：夹具验证协议/异常路径（可 CI 复现）；
 * 本驱动验证真实端点链路（opt-in，需要凭证/隔离 HOME）。
 *
 * 用法：
 *   node scripts/test/m2-11/real-adapter-e2e.mjs --runtime codex \
 *     --adapter <编译产物> [--codex-bin codex] [--codex-home <dir>] [--model <m>] [--runs 20]
 *   node scripts/test/m2-11/real-adapter-e2e.mjs --runtime dsh \
 *     --adapter <编译产物> --dsh-bin <lib/bin.js> --dsh-home <dir> \
 *     [--dsh-profile acp] [--dsh-provider streamax] [--dsh-model deepseek-v4-pro] [--runs 20]
 *
 * 输出：`scripts/test/.tmp/m2-11/real-<runtime>-<ts>/summary.json` + 控制台 PASS/FAIL。
 */
import { spawn } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import readline from "node:readline";

import { repoRoot } from "../lib/exec.mjs";

const args = parseArgs(process.argv.slice(2));
const runtime = args.runtime;
if (runtime !== "codex" && runtime !== "dsh") {
  console.error("用法：--runtime codex|dsh");
  process.exit(2);
}
if (!args.adapter) {
  console.error("缺少 --adapter <编译产物>");
  process.exit(2);
}
const runs = Number(args.runs ?? 1);
const timeoutMs = Number(args["timeout-ms"] ?? 180_000);
const baselinePrompt =
  "Output exactly the following three lines and nothing else. Do not add quotes, markdown or comments.\n" +
  "Aether M2-11 real adapter baseline line 1\n" +
  "Aether M2-11 real adapter baseline line 2\n" +
  "Aether M2-11 real adapter baseline line 3";
const longPrompt =
  "Write an extremely long detailed essay about distributed systems. At least 3000 words. Do not stop early.";

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const evidenceDir = path.join(repoRoot, "scripts", "test", ".tmp", "m2-11", `real-${runtime}-${stamp}`);
mkdirSync(evidenceDir, { recursive: true });
/** 适配器 cwd 必须真实存在（uv_spawn 对缺失 cwd 报 ENOENT）。 */
const workspaceDir = path.join(evidenceDir, "workspace");
mkdirSync(workspaceDir, { recursive: true });

function parseArgs(argv) {
  const out = {};
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (!flag.startsWith("--")) continue;
    const key = flag.slice(2);
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) out[key] = true;
    else {
      out[key] = value;
      index += 1;
    }
  }
  return out;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, Math.max(0, ms)));
}

/** 极简线协议客户端（JSON-RPC over JSON-Lines；单连接单适配器进程）。 */
class WireClient {
  constructor(adapterArgs, env, options = {}) {
    this.adapterArgs = adapterArgs;
    this.env = env;
    this.frames = [];
    this.pending = new Map();
    this.nextId = 1;
    this.child = null;
    this.closed = false;
    this.permissionRequests = [];
    this.permissionTrace = [];
    this.stderrTail = "";
    this.idleWaiters = [];
    this.firstDeltaAt = new Map();
    this.sendStartedAt = new Map();
    /** 权限决议策略：`allow`（默认）/`delayed-allow`/`deny`。 */
    this.permissionPolicy = options.permissionPolicy ?? "allow";
    this.permissionResolveDelayMs = options.permissionResolveDelayMs ?? 0;
  }

  /** 等待首个满足条件的帧（含已收到的帧）。 */
  async waitFrame(predicate, timeout = 15_000) {
    const deadline = Date.now() + timeout;
    for (;;) {
      const found = this.frames.find(predicate);
      if (found) return found;
      if (Date.now() > deadline) throw new Error("等待帧超时");
      await sleep(50);
    }
  }

  start() {
    this.child = spawn(this.adapterArgs[0], this.adapterArgs.slice(1), {
      env: { ...process.env, ...this.env },
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    const stdout = readline.createInterface({ input: this.child.stdout });
    stdout.on("line", (line) => this.onLine(line));
    this.child.stderr.on("data", (chunk) => {
      this.stderrTail = `${this.stderrTail}${chunk.toString("utf8")}`.slice(-8000);
    });
    this.child.once("close", () => {
      this.closed = true;
      for (const pending of this.pending.values()) pending.reject(new Error("适配器进程退出"));
      this.pending.clear();
      for (const waiter of this.idleWaiters.splice(0)) waiter();
    });
    return this;
  }

  onLine(line) {
    let frame;
    try {
      frame = JSON.parse(line);
    } catch {
      return;
    }
    frame.__at = Date.now();
    this.frames.push(frame);
    if (frame.id !== undefined && this.pending.has(frame.id)) {
      const { resolve, reject, timer } = this.pending.get(frame.id);
      this.pending.delete(frame.id);
      clearTimeout(timer);
      if (frame.error) reject(new Error(`RPC ${frame.error.code}: ${frame.error.message}`));
      else resolve(frame.result);
      return;
    }
    if (frame.method === "permission.request") {
      this.permissionRequests.push(frame.params);
      const at = Date.now();
      const decision = this.permissionPolicy === "deny" ? "deny" : "allow";
      const resolve = () => {
        this.permissionTrace.push({
          request: frame.params,
          requestedAt: at,
          decision,
          resolvedAt: Date.now(),
        });
        void this.request("permission.resolve", {
          request_id: frame.params?.request_id,
          decision,
        }).catch(() => undefined);
      };
      if (this.permissionResolveDelayMs > 0) setTimeout(resolve, this.permissionResolveDelayMs);
      else resolve();
      return;
    }
    if (frame.method === "event") {
      const envelope = frame.params;
      if (envelope?.type === "message.delta" && envelope.run_id && !this.firstDeltaAt.has(envelope.run_id)) {
        this.firstDeltaAt.set(envelope.run_id, Date.now());
      }
    }
  }

  request(method, params, timeout = timeoutMs) {
    if (this.closed) return Promise.reject(new Error("连接已关闭"));
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${method} 超时（${timeout}ms）`));
      }, timeout);
      this.pending.set(id, { resolve, reject, timer });
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    });
  }

  events(runId) {
    return this.frames
      .filter((frame) => frame.method === "event")
      .map((frame) => frame.params)
      .filter((envelope) => runId === undefined || envelope.run_id === runId);
  }

  deltaText(runId) {
    return this.events(runId)
      .filter((envelope) => envelope.type === "message.delta")
      .map((envelope) => envelope.payload?.text ?? "")
      .join("");
  }

  completedText(runId) {
    const completed = this.events(runId).find((envelope) => envelope.type === "message.completed");
    return completed?.payload?.message?.content ?? null;
  }

  async waitTerminal(runId, timeout = timeoutMs) {
    const deadline = Date.now() + timeout;
    for (;;) {
      const terminal = this.events(runId).find((envelope) =>
        ["run.completed", "run.failed", "run.cancelled"].includes(envelope.type),
      );
      if (terminal) return terminal;
      if (Date.now() > deadline) return null;
      await sleep(100);
    }
  }

  async stop(timeout = 8000) {
    try {
      await this.request("shutdown", {}, 5000);
    } catch {
      /* 已退出 */
    }
    const deadline = Date.now() + timeout;
    while (!this.closed && Date.now() < deadline) await sleep(50);
    if (!this.closed) this.child.kill();
  }
}

function dshAdapterArgs() {
  if (!args["dsh-bin"] || !args["dsh-home"]) {
    throw new Error("dsh 模式需要 --dsh-bin 与 --dsh-home");
  }
  return [
    "--dsh-bin",
    args["dsh-bin"],
    "--dsh-node",
    process.execPath,
    "--dsh-home",
    args["dsh-home"],
    "--dsh-profile",
    args["dsh-profile"] ?? "acp",
    ...(args["dsh-provider"] ? ["--dsh-provider", args["dsh-provider"]] : []),
    ...(args["dsh-model"] ? ["--dsh-model", args["dsh-model"]] : []),
    ...(args["dsh-version"] ? ["--dsh-version", args["dsh-version"]] : []),
    // 官方分层：provider 定义走 llm-pi-ai composition base（同步注册，消冷启动竞态）。
    ...(args["dsh-provider-config"]
      ? ["--dsh-provider-config", args["dsh-provider-config"]]
      : []),
    "--workspace",
    workspaceDir,
  ];
}

function codexAdapterArgs() {
  return [
    "--codex-bin",
    args["codex-bin"] ?? "codex",
    ...(args["codex-home"] ? ["--codex-home", args["codex-home"]] : []),
    ...(args.model ? ["--model", args.model] : []),
    "--sandbox",
    args.sandbox ?? "read-only",
    "--reasoning",
    args.reasoning ?? "low",
    "--workspace",
    workspaceDir,
  ];
}

function launchClient(options = {}, extraEnv = {}) {
  const extra = runtime === "codex" ? codexAdapterArgs() : dshAdapterArgs();
  return new WireClient(
    [args.adapter, ...extra],
    {
      ...(runtime === "codex" && args["codex-home"] ? { CODEX_HOME: args["codex-home"] } : {}),
      ...extraEnv,
    },
    options,
  ).start();
}

const summary = {
  runtime,
  adapter: args.adapter,
  runs,
  startedAt: new Date().toISOString(),
  environment: {
    model: args.model ?? args["dsh-model"] ?? "(runtime default)",
    codexHome: args["codex-home"] ?? "(user default)",
    dshHome: args["dsh-home"] ?? null,
    dshProvider: args["dsh-provider"] ?? null,
  },
  steps: {},
};

function ok(name, detail = "") {
  summary.steps[name] = { status: "PASS", detail };
  console.log(`PASS  ${name}${detail ? `（${detail}）` : ""}`);
}

function setStep(name, status, detail) {
  summary.steps[name] = { status, detail };
}

function fail(name, detail) {
  summary.steps[name] = { status: "FAIL", detail };
  console.error(`FAIL  ${name}：${detail}`);
}

function skip(name, detail) {
  summary.steps[name] = { status: "SKIP", detail };
  console.log(`SKIP  ${name}：${detail}`);
}

async function initialize(client) {
  const hello = await client
    .waitFrame((frame) => frame.method === "hello", 15_000)
    .catch(() => null);
  if (!hello) throw new Error("hello 未在 15s 内到达");
  if (hello.params?.protocol !== "1.0") {
    throw new Error(`hello.protocol=${hello.params?.protocol}`);
  }
  const initialized = await client.request("initialize", { config: {} });
  if (initialized?.acknowledged !== true) throw new Error("initialize 未确认");
  return hello.params;
}

async function main() {
  const client = launchClient();
  try {
    // 1. 握手 + 初始化
    await client.waitFrame((frame) => frame.method === "hello", 15_000);
    const hello = await initialize(client);
    ok("initialize", `runtime=${hello.runtime?.name} v=${hello.runtime?.version}`);

    // 2. 流式基线
    const session = await client.request("session.create", { title: "m2-11-real" });
    const startedAt = Date.now();
    const ackHolder = await client.request("session.send", {
      session_id: session.session_id,
      client_msg_id: `m2-11-${runtime}-baseline`,
      text: baselinePrompt,
    });
    const baselineTerminal = await client.waitTerminal(ackHolder.run_id);
    if (!baselineTerminal) throw new Error("基线 run 未到达终态（挂起）");
    if (baselineTerminal.type !== "run.completed") {
      const detail = JSON.stringify(baselineTerminal.payload ?? {});
      throw new Error(`基线 run 终态=${baselineTerminal.type} payload=${detail.slice(0, 500)}`);
    }
    const completed = client.completedText(ackHolder.run_id) ?? "";
    const deltas = client.deltaText(ackHolder.run_id);
    if (!completed.includes("Aether M2-11 real adapter baseline")) {
      throw new Error(`终稿内容不符：${completed.slice(0, 120)}`);
    }
    const wallMs = Date.now() - startedAt;
    const firstDeltaMs = client.firstDeltaAt.has(ackHolder.run_id)
      ? client.firstDeltaAt.get(ackHolder.run_id) - startedAt
      : null;
    summary.steps.baseline = {
      status: "PASS",
      wallMs,
      firstDeltaMs,
      deltaChars: deltas.length,
      completedChars: completed.length,
      deltaMatched: deltas === completed,
    };
    console.log(
      `PASS  基线流式（wall=${wallMs}ms firstDelta=${firstDeltaMs ?? "n/a"}ms delta=${deltas.length}B completed=${completed.length}B 拼接一致=${deltas === completed}）`,
    );

    // 3. Mode R：记住口令 → 重启适配器 → native_id 恢复 → recall
    const remember = await client.request("session.send", {
      session_id: session.session_id,
      client_msg_id: `m2-11-${runtime}-remember`,
      text: "Remember this passphrase: AETHER-M2-11-REAL. Reply with exactly: STORED",
    });
    if ((await client.waitTerminal(remember.run_id))?.type !== "run.completed") {
      throw new Error("remember run 未完成");
    }
    const nativeId = session.native_id;
    await client.stop();

    const resumedClient = launchClient();
    try {
      await resumedClient.waitFrame((frame) => frame.method === "hello", 15_000);
      await initialize(resumedClient);
      const resumed = await resumedClient.request("session.create", { native_id: nativeId });
      if (resumed.resumed !== true) throw new Error("session.create 未标记 resumed");
      const recall = await resumedClient.request("session.send", {
        session_id: resumed.session_id,
        client_msg_id: `m2-11-${runtime}-recall`,
        text: "What passphrase did I ask you to remember? Reply with just the passphrase.",
      });
      const recallTerminal = await resumedClient.waitTerminal(recall.run_id);
      const recallText = resumedClient.completedText(recall.run_id) ?? "";
      if (recallTerminal?.type !== "run.completed" || !recallText.includes("AETHER-M2-11-REAL")) {
        throw new Error(
          `恢复未命中原生上下文：terminal=${recallTerminal?.type} text=${recallText.slice(0, 120)}`,
        );
      }
      ok("Mode R（native_id 跨进程恢复）", "命中原生上下文");
    } finally {
      await resumedClient.stop();
    }

    // 4. 中断（新进程，协议级/进程树）
    const interruptClient = launchClient();
    try {
      await interruptClient.waitFrame((frame) => frame.method === "hello", 15_000);
      await initialize(interruptClient);
      const session2 = await interruptClient.request("session.create", { title: "m2-11-interrupt" });
      const long = await interruptClient.request("session.send", {
        session_id: session2.session_id,
        client_msg_id: `m2-11-${runtime}-interrupt`,
        text: longPrompt,
      });
      await sleep(1500);
      const interruptStarted = Date.now();
      const interrupted = await interruptClient.request("session.interrupt", {
        session_id: session2.session_id,
      });
      const interruptMs = Date.now() - interruptStarted;
      const terminal = await interruptClient.waitTerminal(long.run_id, 15_000);
      if (!interrupted?.interrupted || terminal?.type !== "run.cancelled") {
        throw new Error(`中断结果异常：resp=${JSON.stringify(interrupted)} terminal=${terminal?.type}`);
      }
      if (interruptMs > 5000) throw new Error(`interrupt 返回耗时 ${interruptMs}ms > 5s`);
      ok("中断（≤5s + run.cancelled）", `${interruptMs}ms`);
    } finally {
      await interruptClient.stop();
    }

    // 5. N 次回归
    const regressionClient = launchClient();
    try {
      await regressionClient.waitFrame((frame) => frame.method === "hello", 15_000);
      await initialize(regressionClient);
      const session3 = await regressionClient.request("session.create", { title: "m2-11-rate" });
      let completedCount = 0;
      let hang = 0;
      let deltaMatched = 0;
      let deviationSum = 0;
      let expectedFrames = 0;
      let receivedFrames = 0;
      const perRun = [];
      for (let index = 0; index < runs; index += 1) {
        const ack = await regressionClient.request("session.send", {
          session_id: session3.session_id,
          client_msg_id: `m2-11-${runtime}-rate-${index}`,
          text: baselinePrompt,
        });
        const terminal = await regressionClient.waitTerminal(ack.run_id);
        const text = regressionClient.completedText(ack.run_id) ?? "";
        const deltas = regressionClient.deltaText(ack.run_id);
        const frames = regressionClient
          .events(ack.run_id)
          .filter((envelope) => envelope.type === "message.delta").length;
        if (terminal?.type === "run.completed") {
          completedCount += 1;
          if (deltas === text) deltaMatched += 1;
          // 帧级丢失率仅对 token 级 delta 运行时（DSH 插件通道）有意义；
          // Codex 正文为整段 `message.delta`（M1-11 已知坑 4），不以 8 字符切片口径衡量。
          if (runtime === "dsh") {
            expectedFrames += Math.ceil(text.length / 8);
            receivedFrames += frames;
          }
          deviationSum += text.length === 0 ? 0 : Math.abs(deltas.length - text.length) / text.length;
        } else {
          hang += 1;
        }
        perRun.push({ index, terminal: terminal?.type ?? "none", chars: text.length, deltaFrames: frames });
        console.log(
          `[real ${runtime}] run ${index + 1}/${runs}: ${terminal?.type ?? "none"}（${text.length} 字符，${frames} delta）`,
        );
      }
      const rate = completedCount / runs;
      const lossRate =
        runtime !== "dsh" || expectedFrames === 0
          ? 0
          : Math.max(0, expectedFrames - receivedFrames) / expectedFrames;
      const deviation = runs === 0 ? 0 : deviationSum / runs;
      summary.regression = {
        runs,
        completed: completedCount,
        hang,
        completionRate: rate,
        deltaMatched,
        frameLossRate: lossRate,
        lengthDeviationAvg: deviation,
        perRun,
      };
      if (rate < 0.95) throw new Error(`完成率 ${completedCount}/${runs} < 95%`);
      if (runtime === "dsh" && lossRate > 0.01) {
        throw new Error(`帧级丢失率 ${lossRate.toFixed(4)} > 1%`);
      }
      if (deviation > 0.05) throw new Error(`平均长度偏差 ${deviation.toFixed(4)} > 5%`);
      ok(
        `${runs} 次回归`,
        runtime === "dsh"
          ? `完成 ${completedCount}/${runs}；挂起 ${hang}；拼接一致 ${deltaMatched}；丢失率 ${lossRate.toFixed(4)}；偏差 ${deviation.toFixed(4)}`
          : `完成 ${completedCount}/${runs}；挂起 ${hang}；拼接一致 ${deltaMatched}；偏差 ${deviation.toFixed(4)}（整段 delta，不适用帧丢失口径）`,
      );
    } finally {
      await regressionClient.stop();
    }

    // 6. DSH 真实权限 ask 回环（零直通）：
    //    `DSH_PERMISSION_MODE=read-only`（沙箱升级需审批）+ 延迟 1.2s 决议，
    //    断言 permission.request ↔ tool.call_started 一一映射、工具收口晚于决议。
    if (runtime === "dsh" && args["skip-permission"] !== true) {
      const deny = args["permission-deny"] === true;
      const permissionClient = launchClient(
        {
          permissionPolicy: deny ? "deny" : "delayed-allow",
          permissionResolveDelayMs: 1200,
        },
        { DSH_PERMISSION_MODE: "read-only" },
      );
      try {
        await permissionClient.waitFrame((frame) => frame.method === "hello", 15_000);
        await initialize(permissionClient);
        const session = await permissionClient.request("session.create", {
          title: "m2-11-permission",
        });
        const ack = await permissionClient.request("session.send", {
          session_id: session.session_id,
          client_msg_id: `m2-11-${runtime}-${deny ? "permission-deny" : "permission-allow"}`,
          text:
            "Create a file named aether-perm.txt in the current working directory containing " +
            "AETHER_PERM_LOOP, then reply with exactly: DONE",
        });
        const askDeadline = Date.now() + 90_000;
        for (;;) {
          if (permissionClient.permissionTrace.length > 0) break;
          const early = permissionClient
            .events(ack.run_id)
            .find((event) => ["run.completed", "run.failed", "run.cancelled"].includes(event.type));
          if (early || Date.now() > askDeadline) break;
          await sleep(100);
        }
        const terminal = await permissionClient.waitTerminal(ack.run_id, 120_000);
        const trace = permissionClient.permissionTrace;
        if (trace.length === 0) {
          throw new Error("真实 DSH 未触发权限 ask（检查 DSH_PERMISSION_MODE 与工具配置）");
        }
        const startedEvents = permissionClient.frames
          .filter((frame) => frame.method === "event" && frame.params?.type === "tool.call_started")
          .map((frame) => frame.params);
        for (const item of trace) {
          const toolCallId = item.request?.tool_call_id;
          const startedEvent = startedEvents.find(
            (event) => event.payload?.tool_call_id === toolCallId,
          );
          if (!startedEvent) {
            throw new Error(`permission.request 未映射到 tool.call_started（${toolCallId}）`);
          }
        }
        const toolTerminals = permissionClient.frames.filter(
          (frame) =>
            frame.method === "event" &&
            ["tool.call_completed", "tool.call_failed"].includes(frame.params?.type),
        );
        let terminalsAfterResolve = 0;
        for (const item of trace) {
          const toolCallId = item.request?.tool_call_id;
          const terminalEvent = toolTerminals.find(
            (frame) => frame.params?.payload?.tool_call_id === toolCallId,
          );
          if (!terminalEvent) continue;
          if (terminalEvent.__at < item.resolvedAt) {
            throw new Error(`工具在权限决议前已收口（疑似直通）：${toolCallId}`);
          }
          terminalsAfterResolve += 1;
        }
        if (!deny && terminalsAfterResolve === 0) {
          throw new Error("allow 场景缺少工具收口（无法证明回环生效）");
        }
        if (deny) {
          if (terminal?.type !== "run.completed" && terminal?.type !== "run.failed") {
            throw new Error(`deny 场景 run 终态异常：${terminal?.type}`);
          }
        } else if (terminal?.type !== "run.completed") {
          throw new Error(`权限场景 run 终态=${terminal?.type}`);
        }
        summary.permission = {
          decision: trace[0].decision,
          asks: trace.length,
          toolsStarted: startedEvents.length,
          terminalsAfterResolve,
          request: trace[0].request,
          perAskDelayMs: trace.map((item) => item.resolvedAt - item.requestedAt),
        };
        ok(
          `权限 ask 回环（${deny ? "deny" : "allow"}，零直通）`,
          `ask=${trace.length}；映射 tool.call_started ${trace.length}/${trace.length}；` +
            `收口晚于决议 ${terminalsAfterResolve}；决议延迟 ${summary.permission.perAskDelayMs.join("/")}ms`,
        );
      } catch (error) {
        setStep("permission-ask", "FAIL", error instanceof Error ? error.message : String(error));
        console.error(
          `FAIL  权限 ask 回环：${error instanceof Error ? error.message : String(error)}`,
        );
        console.error(`[real dsh] 适配器 stderr 尾部：\n${permissionClient.stderrTail.slice(-1500)}`);
        throw error;
      } finally {
        await permissionClient.stop();
      }
    }
  } catch (error) {
    fail("real-e2e", error instanceof Error ? error.message : String(error));
    console.error(`[real ${runtime}] 适配器 stderr 尾部：\n${client.stderrTail.slice(-2000)}`);
    summary.adapterStderrTail = client.stderrTail.slice(-4000);
    summary.finishedAt = new Date().toISOString();
    writeFileSync(path.join(evidenceDir, "summary.json"), JSON.stringify(summary, null, 2), "utf8");
    await client.stop(3000).catch(() => undefined);
    process.exit(1);
  }

  summary.finishedAt = new Date().toISOString();
  writeFileSync(path.join(evidenceDir, "summary.json"), JSON.stringify(summary, null, 2), "utf8");
  console.log(`[real ${runtime}] 证据：${path.relative(repoRoot, path.join(evidenceDir, "summary.json"))}`);
  process.exitCode = 0;
}

process.on("unhandledRejection", (error) => {
  fail("unhandled", error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
});

void main();
