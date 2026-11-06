#!/usr/bin/env node
/**
 * Q5 `subagent_context` spike（M10-02 前置；ADR-032 v1.1 / 决策记录 §9-3）。
 *
 * 本脚本只跑 mock adapter + 单测，不写注入主路径：
 * - capability 缺失 → 不 auto_inject，转 hold；
 * - `session.send` ack `applied_context_version` >= version 才置 injected；
 * - context_version 单调（注入/折叠 bump）；
 * - 折叠：最近 8 条 envelope + 1 条摘要 ≤2KiB，总量 ≤16KiB；旧 payload 保留；
 * - 重启：重发当前快照，不重复注入；
 * - 失败/超时/低 ack：不置 injected、不丢 pending；
 * - N→1 批次、去重、冲突、verdict 优先级。
 *
 * 运行：node scripts/test/m10-02/context-spike.mjs
 * 输出：scripts/test/m10-02/.tmp/q5-mock-results.json
 */
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const tmpDir = path.join(repoRoot, "scripts", "test", "m10-02", ".tmp");
mkdirSync(tmpDir, { recursive: true });
const results = { schema: "aether.m10-02.q5-mock-spike.v1", started_at: new Date().toISOString(), checks: [] };

function check(name, ok, detail) {
  results.checks.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? ` :: ${detail}` : ""}`);
  if (!ok) throw new Error(`Q5 mock check failed: ${name}`);
}

const BLOCK_LIMIT = 16 * 1024;
const FOLD_SUMMARY_LIMIT = 2 * 1024;
const MAX_RAW_ENVELOPES = 8;

const VERDICT_PRIORITY = { info: 0, approve: 1, needs_changes: 2, reject: 3 };

function envelopeFor(child, verdict, summary, findings = []) {
  return {
    child_session_id: child,
    verdict,
    summary,
    findings_count: findings.length,
    top_findings: findings.slice(0, 5),
    result_ref: `subagent:${child}`,
  };
}

class MockAdapter {
  constructor({ capability = true, ackMode = "exact", failTimes = 0 } = {}) {
    this.capability = capability;
    this.ackMode = ackMode;
    this.failTimes = failTimes;
    this.calls = [];
  }

  send({ version, block }) {
    this.calls.push({ version, bytes: Buffer.byteLength(block, "utf8"), block });
    if (!this.capability) throw new Error("subagent_context_rejected");
    if (this.failTimes > 0) {
      this.failTimes -= 1;
      throw new Error("adapter send failed");
    }
    if (this.ackMode === "none") return {};
    if (this.ackMode === "stale") return { applied_context_version: version - 1 };
    if (this.ackMode === "lower") return { applied_context_version: 0 };
    return { applied_context_version: version };
  }
}

class ContextManager {
  constructor(state) {
    this.state = state ?? {
      version: 0,
      pending: [],
      injected: [],
      active: [],
      folded: [],
      batches: new Map(),
      snapshot: "",
    };
  }

  snapshotState() {
    return structuredClone(this.state);
  }

  capability(capability) {
    return capability;
  }

  addBatch(batchId, envelopes) {
    const existing = this.state.batches.get(batchId);
    if (existing !== undefined) {
      existing.envelopes.push(...envelopes);
      existing.count += envelopes.length;
    } else {
      this.state.batches.set(batchId, { batchId, envelopes: [...envelopes], count: envelopes.length });
    }
    this.state.pending.push(...envelopes);
    this.state.active.push(...envelopes);
    this.bump();
    return this.buildSnapshot();
  }

  bump() {
    this.state.version += 1;
    return this.state.version;
  }

  buildSnapshot() {
    const all = this.state.active;
    const raw = all.slice(-MAX_RAW_ENVELOPES);
    const older = all.slice(0, Math.max(0, all.length - MAX_RAW_ENVELOPES));
    const folded = this.state.folded;
    const allOlder = [...older, ...folded];
    const lines = [];
    if (allOlder.length > 0) {
      const verdicts = {};
      for (const env of allOlder) verdicts[env.verdict] = (verdicts[env.verdict] ?? 0) + 1;
      const refs = allOlder.slice(-16).map((env) => env.result_ref);
      const summary = {
        folded: allOlder.length,
        verdicts,
        result_refs: refs,
      };
      let text = `<aether:subagent-results-folded>${JSON.stringify(summary)}</aether:subagent-results-folded>`;
      while (Buffer.byteLength(text, "utf8") > FOLD_SUMMARY_LIMIT && summary.result_refs.length > 0) {
        summary.result_refs.pop();
        text = `<aether:subagent-results-folded>${JSON.stringify(summary)}</aether:subagent-results-folded>`;
      }
      lines.push(text);
    }
    for (const env of raw) {
      lines.push(`<aether:subagent-results version="${this.state.version}" child="${env.child_session_id}" verdict="${env.verdict}">${JSON.stringify(env)}</aether:subagent-results>`);
    }
    let block = lines.join("\n");
    while (Buffer.byteLength(block, "utf8") > BLOCK_LIMIT && lines.length > 1) {
      lines.splice(1, 1);
      block = lines.join("\n");
    }
    this.state.snapshot = block;
    return block;
  }

  async inject(adapter, { auto = true } = {}) {
    const version = this.state.version;
    const block = this.buildSnapshot();
    if (Buffer.byteLength(block, "utf8") > BLOCK_LIMIT) throw new Error("block_too_large");
    if (!adapter.capability) {
      return { status: "hold", reason: "capability_missing", version };
    }
    if (!auto) return { status: "hold", reason: "manual", version };
    try {
      const ack = adapter.send({ version, block });
      const applied = typeof ack?.applied_context_version === "number" ? ack.applied_context_version : -1;
      if (applied >= version) {
        for (const env of this.state.pending.splice(0)) this.state.injected.push(env);
        return { status: "injected", version, applied };
      }
      return { status: "hold", reason: "ack_missing_or_stale", version, applied };
    } catch (error) {
      return { status: "hold", reason: "send_failed", version, error: error instanceof Error ? error.message : String(error) };
    }
  }

  fold() {
    const all = this.state.active;
    const keep = all.slice(-MAX_RAW_ENVELOPES);
    const move = all.slice(0, Math.max(0, all.length - MAX_RAW_ENVELOPES));
    if (move.length > 0) {
      const moved = new Set(move);
      this.state.active = keep;
      this.state.pending = this.state.pending.filter((env) => !moved.has(env));
      this.state.injected = this.state.injected.filter((env) => !moved.has(env));
      this.state.folded.push(...move.map((env) => ({ ...env, folded_at: Date.now() })));
    }
    this.bump();
    return this.buildSnapshot();
  }

  restart() {
    const restored = new ContextManager(this.snapshotState());
    return restored;
  }
}

function verdictOf(envelopes) {
  return envelopes.reduce((acc, env) => (VERDICT_PRIORITY[env.verdict] > VERDICT_PRIORITY[acc] ? env.verdict : acc), "info");
}

function dedupeFindings(envelopes) {
  const seen = new Map();
  for (const env of envelopes) {
    for (const finding of env.top_findings ?? []) {
      const key = `${finding.title}|${finding.path ?? ""}|${finding.line ?? ""}`;
      if (!seen.has(key)) seen.set(key, finding);
    }
  }
  return [...seen.values()];
}

// ===== 1. capability / ack / 版本 =====
{
  const manager = new ContextManager();
  const adapter = new MockAdapter({ capability: false });
  manager.addBatch("b1", [envelopeFor("c1", "approve", "ok")]);
  const held = await manager.inject(adapter);
  check("capability 缺失 → hold，不 auto_inject", held.status === "hold" && held.reason === "capability_missing", JSON.stringify(held));
  check("capability 缺失不调用 adapter", adapter.calls.length === 0);
  check("未注入行仍在 pending（不静默丢失）", manager.state.pending.length === 1 && manager.state.injected.length === 0);
}

{
  const manager = new ContextManager();
  const adapter = new MockAdapter({ ackMode: "exact" });
  manager.addBatch("b1", [envelopeFor("c1", "approve", "ok")]);
  const result = await manager.inject(adapter);
  check("ack >= version → injected", result.status === "injected" && manager.state.injected.length === 1, JSON.stringify(result));
  check("ack 携带 version", adapter.calls[0].version === 1 && adapter.calls[0].bytes <= BLOCK_LIMIT, JSON.stringify({ version: adapter.calls[0].version, bytes: adapter.calls[0].bytes }));
}

for (const mode of ["none", "stale", "lower"]) {
  const manager = new ContextManager();
  const adapter = new MockAdapter({ ackMode: mode });
  manager.addBatch("b1", [envelopeFor("c1", "approve", "ok")]);
  const result = await manager.inject(adapter);
  check(`ack ${mode} → 不置 injected`, result.status === "hold" && manager.state.injected.length === 0 && manager.state.pending.length === 1, JSON.stringify(result));
}

{
  const manager = new ContextManager();
  const adapter = new MockAdapter({ failTimes: 1 });
  manager.addBatch("b1", [envelopeFor("c1", "reject", "bad")]);
  const first = await manager.inject(adapter);
  check("adapter 失败 → hold，不丢失", first.status === "hold" && first.reason === "send_failed" && manager.state.injected.length === 0 && manager.state.pending.length === 1);
  const second = await manager.inject(adapter);
  check("失败后重试成功 → injected", second.status === "injected" && manager.state.injected.length === 1, JSON.stringify(second));
}

// ===== 2. 版本单调 + 折叠 =====
{
  const manager = new ContextManager();
  const adapter = new MockAdapter();
  const versions = [];
  for (let i = 1; i <= 10; i += 1) {
    manager.addBatch(`b${i}`, [envelopeFor(`c${i}`, i % 2 ? "approve" : "info", `s${i}`)]);
    versions.push(manager.state.version);
  }
  check("context_version 注入 bump 单调", versions.every((v, i) => v === i + 1), versions.join(","));
  const beforeFold = manager.state.version;
  const block = manager.fold();
  check("fold bump version", manager.state.version === beforeFold + 1, `${beforeFold}→${manager.state.version}`);
  check("fold 保留最近 8 条原文 + 1 条摘要", (block.match(/<aether:subagent-results /g) ?? []).length === 8 && block.includes("<aether:subagent-results-folded>"), `raw=${(block.match(/<aether:subagent-results /g) ?? []).length}`);
  check("fold 摘要 ≤2KiB", Buffer.byteLength(block.match(/<aether:subagent-results-folded>.*?<\/aether:subagent-results-folded>/s)?.[0] ?? "", "utf8") <= FOLD_SUMMARY_LIMIT);
  check("fold 后总量 ≤16KiB", Buffer.byteLength(block, "utf8") <= BLOCK_LIMIT, `${Buffer.byteLength(block, "utf8")}B`);
  check("fold 行保留 payload（folded_at）", manager.state.folded.length === 2 && manager.state.folded.every((env) => typeof env.folded_at === "number"));
  const injected = await manager.inject(adapter);
  check("fold 后注入使用新 version", injected.status === "injected" && adapter.calls.at(-1).version === manager.state.version, JSON.stringify(injected));
}

// ===== 3. 重启重发当前快照 =====
{
  const manager = new ContextManager();
  const adapter = new MockAdapter();
  for (let i = 1; i <= 10; i += 1) manager.addBatch(`b${i}`, [envelopeFor(`c${i}`, "approve", `s${i}`)]);
  await manager.inject(adapter);
  manager.fold();
  await manager.inject(adapter);
  const version = manager.state.version;
  const injectedCount = manager.state.injected.length;
  const restarted = manager.restart();
  const resend = await restarted.inject(adapter);
  check("重启后重发当前快照", resend.status === "injected" && adapter.calls.at(-1).version === version && adapter.calls.at(-1).block.includes("subagent-results"), `version=${version}`);
  check("重启重发不重复注入行", restarted.state.injected.length === injectedCount, `${injectedCount}→${restarted.state.injected.length}`);
  check("重启后版本不倒退", restarted.state.version === version, `${version}→${restarted.state.version}`);
}

// ===== 4. 16KiB 上限 + 截断 =====
{
  const manager = new ContextManager();
  const huge = "x".repeat(15 * 1024);
  for (let i = 0; i < 10; i += 1) manager.addBatch(`b${i}`, [envelopeFor(`c${i}`, "approve", huge)]);
  const block = manager.buildSnapshot();
  check("快照总量 ≤16KiB（必要时丢弃旧原文）", Buffer.byteLength(block, "utf8") <= BLOCK_LIMIT, `${Buffer.byteLength(block, "utf8")}B`);
  let rejected = false;
  try {
    const tooLarge = new ContextManager();
    const hugeEnvelope = envelopeFor("c-big", "approve", "y".repeat(BLOCK_LIMIT + 1));
    tooLarge.state.pending.push(hugeEnvelope);
    tooLarge.state.active.push(hugeEnvelope);
    const adapter = new MockAdapter();
    await tooLarge.inject(adapter);
  } catch (error) {
    rejected = String(error).includes("block_too_large");
  }
  check("显式 >16KiB → 拒绝", rejected);
}

// ===== 5. N→1、去重、冲突、verdict 优先级 =====
{
  const manager = new ContextManager();
  const envelopes = [
    envelopeFor("c1", "approve", "ok", [{ title: "A", path: "x.ts", line: 1 }]),
    envelopeFor("c2", "needs_changes", "fix", [{ title: "A", path: "x.ts", line: 1 }, { title: "B", path: "y.ts", line: 2 }]),
    envelopeFor("c3", "reject", "no", [{ title: "C", path: "z.ts", line: 3 }]),
  ];
  manager.addBatch("batch-1", envelopes);
  check("同 batch N→1 行", manager.state.batches.size === 1 && manager.state.batches.get("batch-1").count === 3);
  check("verdict 优先级 reject > needs_changes > approve", verdictOf(envelopes) === "reject");
  const deduped = dedupeFindings(envelopes);
  check("findings 按 (title,path,line) 去重", deduped.length === 3 && deduped.map((f) => f.title).join(",") === "A,B,C", deduped.map((f) => f.title).join(","));
  const block = manager.buildSnapshot();
  check("N→1 快照包含 children/result_refs", block.includes("c1") && block.includes("c2") && block.includes("c3"));
}

results.finished_at = new Date().toISOString();
results.ok = results.checks.every((item) => item.ok);
writeFileSync(path.join(tmpDir, "q5-mock-results.json"), `${JSON.stringify(results, null, 2)}\n`, "utf8");
console.log(`Q5 mock spike: ${results.checks.filter((c) => c.ok).length}/${results.checks.length} checks passed`);
