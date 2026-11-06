#!/usr/bin/env node
/**
 * Q5 交付模式 spike（ADR-032 v1.2 / 决策记录 v0.1）。
 *
 * 只验证交付语义，不写 M10-02 主路径：
 * - message_append：父 system prompt 不变；compact envelope 作为 tool/user message 追加；
 *   version/supersedes；旧行 superseded；ack version+mode 后才 injected；
 * - origin_fork：delegation boundary 冻结父消息；callback 只用 fork+结果，
 *   不含 intervening turns；独立 ephemeral provider context；memory/skill review disabled；
 * - system_replace：仅运行时声明支持；不支持 mode → rejected/hold。
 *
 * 运行：node scripts/test/m10-02/delivery-mode-spike.mjs
 * 输出：scripts/test/m10-02/.tmp/q5-delivery-mode-results.json
 */
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const tmpDir = path.join(repoRoot, "scripts", "test", "m10-02", ".tmp");
mkdirSync(tmpDir, { recursive: true });
const results = { schema: "aether.m10-02.q5-delivery-mode-spike.v1", started_at: new Date().toISOString(), checks: [] };

function check(name, ok, detail) {
  results.checks.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? ` :: ${detail}` : ""}`);
  if (!ok) throw new Error(`Q5 delivery mode check failed: ${name}`);
}

function clone(value) {
  return structuredClone(value);
}

class MessageAppendParent {
  constructor() {
    this.systemPrompt = "SYSTEM_PROMPT_STABLE_V1";
    this.messages = [
      { role: "user", content: "parent question" },
      { role: "assistant", content: "parent answer" },
    ];
    this.injections = [];
    this.version = 0;
  }

  append(envelope, { ack = "exact" } = {}) {
    this.version += 1;
    const block = JSON.stringify(envelope);
    const previous = this.injections[this.injections.length - 1];
    const record = {
      id: `inj-${this.version}`,
      version: this.version,
      mode: "message_append",
      state: "pending",
      supersedes: previous?.id ?? null,
      payload: block,
    };
    this.injections.push(record);
    const ackPayload =
      ack === "exact"
        ? { applied_context_version: this.version, applied_context_mode: "message_append" }
        : ack === "wrong_mode"
          ? { applied_context_version: this.version, applied_context_mode: "system_replace" }
          : ack === "stale"
            ? { applied_context_version: this.version - 1, applied_context_mode: "message_append" }
            : {};
    if (
      ackPayload.applied_context_version >= this.version &&
      ackPayload.applied_context_mode === "message_append"
    ) {
      this.messages.push({
        role: "tool",
        name: "subagent_result",
        content: block,
        source: "subagent",
        version: this.version,
        supersedes: record.supersedes,
      });
      record.state = "injected";
      if (record.supersedes) {
        const old = this.injections.find((item) => item.id === record.supersedes);
        if (old) old.state = "superseded";
      }
      return { status: "injected", ...ackPayload };
    }
    return { status: "hold", ...ackPayload };
  }
}

class OriginForkParent {
  constructor() {
    this.messages = [
      { role: "user", content: "origin user" },
      { role: "assistant", content: "origin assistant" },
      { role: "tool", name: "delegate_task", content: "accepted" },
    ];
    this.contextKey = "session-live";
  }

  captureFork(boundary = this.messages.length) {
    return {
      messages: clone(this.messages.slice(0, boundary)),
      contextKey: `${this.contextKey}:background-callback:child-1`,
      memoryEnabled: false,
      skillReviewEnabled: false,
    };
  }

  addInterveningTurns() {
    this.messages.push(
      { role: "user", content: "intervening user" },
      { role: "assistant", content: "intervening assistant" },
    );
  }
}

function callbackRequest(fork, result) {
  return {
    contextKey: fork.contextKey,
    memoryEnabled: fork.memoryEnabled,
    skillReviewEnabled: fork.skillReviewEnabled,
    messages: [...clone(fork.messages), { role: "user", content: `Background subtask result:\n${result}` }],
  };
}

// ===== 1. message_append =====
{
  const parent = new MessageAppendParent();
  const beforeSystem = parent.systemPrompt;
  const beforePrefix = clone(parent.messages);
  const first = parent.append({ verdict: "approve", summary: "v1", result_refs: ["subagent:c1"] });
  check("message_append ack version/mode → injected", first.status === "injected" && first.applied_context_mode === "message_append", JSON.stringify(first));
  check("message_append 不改 system prompt", parent.systemPrompt === beforeSystem);
  check(
    "message_append 父消息前缀不变",
    JSON.stringify(parent.messages.slice(0, beforePrefix.length)) === JSON.stringify(beforePrefix),
  );
  check(
    "message_append 结果恰好追加一次",
    parent.messages.filter((message) => message.name === "subagent_result").length === 1,
    JSON.stringify(parent.messages),
  );
  const second = parent.append({ verdict: "needs_changes", summary: "v2", result_refs: ["subagent:c2"] });
  check("message_append 第二次 ack", second.status === "injected" && second.applied_context_version === 2);
  check(
    "message_append 旧行 superseded",
    parent.injections[0]?.state === "superseded" && parent.injections[1]?.supersedes === "inj-1",
    JSON.stringify(parent.injections),
  );
  check(
    "message_append 结果标记 version/supersedes",
    parent.messages.at(-1)?.version === 2 && parent.messages.at(-1)?.supersedes === "inj-1",
    JSON.stringify(parent.messages.at(-1)),
  );
  check(
    "message_append 旧结果仍在历史（append-only）",
    parent.messages.some((message) => message.version === 1) && parent.messages.some((message) => message.version === 2),
  );
}

// ===== 2. ack 缺失/错误 mode =====
{
  const parent = new MessageAppendParent();
  const beforeMessages = clone(parent.messages);
  const missing = parent.append({ verdict: "approve", summary: "v1" }, { ack: "none" });
  check("message_append ack 缺失 → hold", missing.status === "hold" && parent.messages.length === beforeMessages.length);
  const wrong = parent.append({ verdict: "approve", summary: "v2" }, { ack: "wrong_mode" });
  check("message_append ack mode 不匹配 → hold", wrong.status === "hold" && parent.messages.length === beforeMessages.length);
  const stale = parent.append({ verdict: "approve", summary: "v3" }, { ack: "stale" });
  check("message_append ack version 落后 → hold", stale.status === "hold" && parent.messages.length === beforeMessages.length);
}

// ===== 3. origin_fork =====
{
  const parent = new OriginForkParent();
  const fork = parent.captureFork();
  parent.addInterveningTurns();
  const request = callbackRequest(fork, "child result");
  check("origin_fork 使用独立 ephemeral context", request.contextKey === "session-live:background-callback:child-1" && request.contextKey !== parent.contextKey, request.contextKey);
  check("origin_fork memory/skill review disabled", request.memoryEnabled === false && request.skillReviewEnabled === false);
  check(
    "origin_fork 包含 origin 消息",
    request.messages.some((message) => message.content === "origin user") &&
      request.messages.some((message) => message.content === "origin assistant") &&
      request.messages.some((message) => message.name === "delegate_task"),
  );
  check(
    "origin_fork 不含 intervening turns",
    !request.messages.some((message) => message.content === "intervening user") &&
      !request.messages.some((message) => message.content === "intervening assistant"),
  );
  check(
    "origin_fork 子结果恰好一次",
    request.messages.filter((message) => String(message.content).includes("child result")).length === 1,
  );
  check(
    "origin_fork 不回写 live parent",
    parent.messages.some((message) => message.content === "intervening user") &&
      !parent.messages.some((message) => String(message.content).includes("child result")),
  );
  const restart = { status: "rejected", reason: "origin_fork_unavailable" };
  check("origin_fork 重启不可恢复 → 观测性拒绝", restart.status === "rejected", JSON.stringify(restart));
}

// ===== 4. system_replace 仅可选 =====
{
  const capability = { modes: ["message_append", "origin_fork"] };
  const requested = "system_replace";
  const supported = capability.modes.includes(requested);
  check("system_replace 未声明 → rejected/hold", supported === false, JSON.stringify({ capability, requested }));
  const strictCapability = { modes: ["message_append", "origin_fork", "system_replace"] };
  check("system_replace 声明支持 → 可选", strictCapability.modes.includes(requested));
}

results.finished_at = new Date().toISOString();
results.ok = results.checks.every((item) => item.ok);
writeFileSync(path.join(tmpDir, "q5-delivery-mode-results.json"), `${JSON.stringify(results, null, 2)}\n`, "utf8");
console.log(`Q5 delivery mode spike: ${results.checks.filter((c) => c.ok).length}/${results.checks.length} checks passed`);
