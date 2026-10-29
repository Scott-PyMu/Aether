#!/usr/bin/env node
/**
 * `pnpm verify:m9-05`（P2 计划 M9-05；ADR-029 D-029-5 / §4.4 / §4.7）。
 *
 * 可执行校验口径（静态 + golden；动态证据见 `docs/M9-05-证据.md`）：
 *  1. EventEnvelope 9 字段与附录 B 23 类型集合不变；DB `UNIQUE(session_id, seq)` 兜底；
 *  2. 先日志后广播：`persist_pending` 仅在 `write_with_retry` 成功后 `events.send`；
 *  3. daemon 事件桥：`aether.event` 通知、4096 有界队列、仅 3 类可丢、
 *     连续 3 次补读失败升级审计、`readback_gap_too_large` 不自动重试；
 *  4. main 事件桥：`aether:event` 单通道、16ms 批处理、有界队列、补读/重建/审计；
 *  5. renderer 15s 无 health →「核心未响应」沿用既有实现；
 *  6. 关键测试用例名与 10×50 报告齐备。
 */
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const failures = [];
const checked = {
  envelopeFields: 0,
  eventTypes: 0,
  fixtures: 0,
  rustTests: 0,
  tsTests: 0,
};

function fail(message) {
  failures.push(message);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function exists(rel) {
  return existsSync(path.join(root, rel));
}

function between(source, start, end) {
  const from = source.indexOf(start);
  if (from < 0) return "";
  const to = source.indexOf(end, from + start.length);
  return source.slice(from, to < 0 ? undefined : to);
}

// ===== 1. EventEnvelope 9 字段 / 附录 B 23 类型 =====
const expectedFields = [
  "v",
  "id",
  "session_id",
  "run_id",
  "runtime_id",
  "seq",
  "ts",
  "type",
  "payload",
];
const eventSource = read("crates/aether-core/src/event.rs");
const fieldsBlock = between(
  eventSource,
  "pub const ENVELOPE_FIELDS: [&str; 9] = [",
  "];",
);
const actualFields = [...fieldsBlock.matchAll(/"([^"]+)"/g)].map((match) => match[1]);
checked.envelopeFields = actualFields.length;
if (actualFields.length !== 9) {
  fail(`[DoD-1] ENVELOPE_FIELDS 必须 9 项，实际 ${actualFields.length}`);
}
if (actualFields.join(",") !== expectedFields.join(",")) {
  fail(`[DoD-1] ENVELOPE_FIELDS 顺序/内容漂移：${actualFields.join(",")}`);
}

const eventTypes = readJson("scripts/test/m9-05/fixtures/event-types.json");
const expectedTypes = [...eventTypes.mvp, ...eventTypes.reserved];
const enumBlock = between(eventSource, "pub enum EventType {", "impl EventType {");
const actualTypes = [...enumBlock.matchAll(/#\[serde\(rename = "([^"]+)"\)\]/g)].map(
  (match) => match[1],
);
checked.eventTypes = actualTypes.length;
if (actualTypes.length !== 23) {
  fail(`[DoD-1] EventType 必须 23 项（19 MVP + 4 预留），实际 ${actualTypes.length}`);
}
for (const type of expectedTypes) {
  if (!actualTypes.includes(type)) fail(`[DoD-1] 缺少附录 B 事件类型：${type}`);
}
for (const type of actualTypes) {
  if (!expectedTypes.includes(type)) fail(`[DoD-1] 出现附录 B 之外的事件类型：${type}`);
}
if (!/pub const MVP: \[Self; 19\]/.test(eventSource)) {
  fail("[DoD-1] MVP 事件类型集合必须为 19");
}
if (!/pub const RESERVED: \[Self; 4\]/.test(eventSource)) {
  fail("[DoD-1] 预留事件类型集合必须为 4");
}

// DB UNIQUE(session_id, seq) 兜底（0002 增量迁移，不改 0001）。
const migration = read("migrations/0002_unique_keys.sql");
if (
  !/CREATE UNIQUE INDEX idx_events_session_seq_uq ON events\(session_id, seq\);/.test(
    migration,
  )
) {
  fail("[DoD-1] 缺少 events UNIQUE(session_id, seq) 兜底索引");
}

// ===== 2. 先日志后广播 =====
const pipeline = read("crates/aether-control/src/pipeline.rs");
const persistBlock = between(
  pipeline,
  "async fn persist_pending(",
  "/// 落盘重试",
);
const appendIndex = persistBlock.indexOf("write_with_retry");
const broadcastIndex = persistBlock.indexOf("self.events.send");
if (appendIndex < 0) fail("[DoD-2] persist_pending 缺少 journal 写入");
if (broadcastIndex < 0) fail("[DoD-2] persist_pending 缺少广播");
if (appendIndex >= 0 && broadcastIndex >= 0 && appendIndex > broadcastIndex) {
  fail("[DoD-2] 广播发生在写日志之前（违反先日志后广播）");
}

// ===== 3. daemon 事件桥 =====
const bridgeSource = read("crates/aether-app/src/event_bridge.rs");
for (const [label, pattern] of [
  ["有界队列 4096", /pub const DELIVERY_QUEUE_CAPACITY: usize = 4_096;/],
  ["连续 3 次补读失败", /pub const DELIVERY_READBACK_MAX_ATTEMPTS: u32 = 3;/],
  [
    "可丢类型仅 3 类",
    /pub const DROPPABLE_EVENT_TYPES: \[EventType; 3\] =\s*\[EventType::MessageDelta, EventType::Usage, EventType::Log\];/,
  ],
  ["升级审计动作", /ESCALATION_AUDIT_ACTION: &str = "event\.bridge\.escalated"/],
  ["缺口过大不自动重试", /PipelineError::ReadbackGapTooLarge/],
]) {
  if (!pattern.test(bridgeSource)) fail(`[DoD-3] 事件桥缺少${label}`);
}
if (!/is_droppable\(event_type\)/.test(bridgeSource)) {
  fail("[DoD-3] 队列满分支必须按可丢集合分流");
}
if (!/self\.feed\.readback\(session_id, from\)/.test(bridgeSource)) {
  fail("[DoD-3] 控制事件队列满必须切 journal 补读");
}
if (!/self\.receiver = self\.feed\.subscribe\(\);/.test(bridgeSource)) {
  fail("[DoD-3] 连续补读失败必须重建事件订阅");
}

const daemonServe = read("crates/aether-daemon/src/serve.rs");
if (!/pub const EVENT_METHOD: &str = "aether\.event";/.test(daemonServe)) {
  fail("[DoD-3] daemon 缺少 aether.event 通知方法");
}
if (!/EventBridge::new\(/.test(daemonServe) || !/\.spawn\(&bridge_runtime\)/.test(daemonServe)) {
  fail("[DoD-3] 预热完成必须启动事件桥");
}
if (!/event_notification\(event\)/.test(daemonServe)) {
  fail("[DoD-3] 事件出口必须编码为 aether.event 通知");
}

// ===== 4. main 事件桥（Electron main） =====
const mainBridge = read("apps/desktop/electron/eventBridge.ts");
for (const [label, pattern] of [
  ["aether:event 单通道", /EVENT_IPC_CHANNEL = "aether:event"/],
  ["16ms 批处理", /EVENT_BATCH_INTERVAL_MS = 16/],
  ["有界队列 4096", /EVENT_QUEUE_CAPACITY = 4_096/],
  ["连续 3 次失败", /READBACK_MAX_ATTEMPTS = 3/],
  ["补读缺口过大", /READBACK_GAP_TOO_LARGE = "readback_gap_too_large"/],
]) {
  if (!pattern.test(mainBridge)) fail(`[DoD-4] main 事件桥缺少${label}`);
}
for (const type of ["message.delta", "usage", "log"]) {
  if (!mainBridge.includes(`"${type}"`)) {
    fail(`[DoD-4] main 可丢集合缺少 ${type}`);
  }
}
if (!/event\.bridge\.escalated/.test(mainBridge)) {
  fail("[DoD-4] main 升级必须审计");
}
if (!/rebuildConnection\?\.\(sessionId\)/.test(mainBridge)) {
  fail("[DoD-4] 连续 3 次补读失败必须重建连接");
}

// ===== 5. renderer 15s 无 health =====
const health = read("apps/desktop/src/health.ts");
if (!/HEALTH_TIMEOUT_MS = 15_000/.test(health)) {
  fail("[DoD-4] renderer 15s 健康判定漂移");
}
if (!/核心未响应/.test(read("apps/desktop/src/HealthMonitor.tsx"))) {
  fail("[DoD-4] 缺少「核心未响应」UI 文案");
}

// ===== 6. golden fixtures / 测试用例 / 报告 =====
const envelope = readJson("scripts/test/m9-05/fixtures/event-envelope.json");
checked.fixtures += 1;
if (envelope.method !== "aether.event") fail("[DoD-1] golden method 漂移");
if (envelope.channel !== "aether:event") fail("[DoD-1] IPC 通道漂移");
if (envelope.domain_channel !== "aether://event") fail("[DoD-1] 域通道漂移");
if (envelope.fields.join(",") !== expectedFields.join(",")) {
  fail("[DoD-1] golden fields 漂移");
}
if (envelope.notification.jsonrpc !== "2.0") fail("[DoD-1] golden 非 JSON-RPC 2.0");
if ("id" in envelope.notification) fail("[DoD-1] 通知不得携带 id");
const notificationParams = Object.keys(envelope.notification.params).sort();
if (notificationParams.length !== 9) {
  fail(`[DoD-1] golden params 必须 9 字段，实际 ${notificationParams.length}`);
}
// JSON 对象字段顺序不参与契约（serde_json 默认 Map 排序）；按集合比较。
if (notificationParams.join(",") !== [...expectedFields].sort().join(",")) {
  fail("[DoD-1] golden params 字段集合漂移");
}

const report = readJson("scripts/test/m9-05/fixtures/10x50-report.json");
checked.fixtures += 1;
if (report.report.sessions !== 10) fail("[DoD-3] 10×50 报告 sessions ≠ 10");
if (report.report.deltas_per_second !== 50) fail("[DoD-3] 10×50 报告 delta/s ≠ 50");
if (report.report.control_lost !== 0) fail("[DoD-3] 10×50 报告控制事件丢失");
if (!(report.report.p95_ms < report.thresholds.p95_ms_max)) {
  fail(`[DoD-3] 10×50 P95 超阈值：${report.report.p95_ms}ms`);
}

const appTest = read("crates/aether-app/tests/m9_05_event_bridge.rs");
for (const name of [
  "write_failure_is_not_broadcast_to_the_event_bridge",
  "ten_sessions_fifty_deltas_per_second_keep_all_control_events",
  "control_overflow_uses_journal_readback_and_keeps_control_events",
  "three_consecutive_readback_failures_escalate_with_audit",
]) {
  if (!appTest.includes(`async fn ${name}`)) {
    fail(`[DoD-3/4] 缺少集成测试：${name}`);
  } else {
    checked.rustTests += 1;
  }
}
const daemonTest = read("crates/aether-daemon/tests/m9_05_events.rs");
if (!daemonTest.includes("fn daemon_pushes_aether_event_after_log_with_monotonic_seq")) {
  fail("[DoD-1] 缺少 daemon 进程级 aether.event 测试");
} else {
  checked.rustTests += 1;
}
for (const needle of ["session_list", "messages_page", "last_seq"]) {
  if (!daemonTest.includes(needle)) {
    fail(`[DoD-5] daemon 重启同步测试缺少 ${needle}`);
  }
}
for (const rel of [
  "apps/desktop/src/eventStore.integration.test.tsx",
  "apps/desktop/src/workbenchBackfillE2E.test.tsx",
  "apps/desktop/src/backfillSource.test.ts",
]) {
  if (!exists(rel)) fail(`[DoD-5] 缺少既有补读/重载测试：${rel}`);
}
if (
  !/readback_gap_too_large/.test(
    read("apps/desktop/src/eventStore.integration.test.tsx") +
      read("apps/desktop/src/workbenchBackfillE2E.test.tsx") +
      read("apps/desktop/src/backfillSource.test.ts"),
  )
) {
  fail("[DoD-5] 缺少 readback_gap_too_large 边界回归");
}

const tsTest = read("apps/desktop/electron/eventBridge.test.ts");
for (const name of [
  "16ms 批处理 + 单通道 FIFO（批内顺序不变）",
  "队列满：delta 丢弃并计数；控制事件仍入队",
  "控制事件队列满：丢队列 delta + 触发 journal 补读 + FIFO 发送",
  "连续 3 次补读失败才审计 + 重建连接（不重启 daemon）",
  "readback_gap_too_large 不自动重试/不升级（UI 既有提示路径）",
]) {
  if (!tsTest.includes(name)) {
    fail(`[DoD-4] 缺少 main 事件桥测试：${name}`);
  } else {
    checked.tsTests += 1;
  }
}

// ===== 7. 脚本/证据登记 =====
const pkg = readJson("package.json");
if (pkg.scripts?.["verify:m9-05"] !== "node scripts/test/m9-05/verify-m9-05.mjs") {
  fail("[DoD-5] package.json 缺少 verify:m9-05");
}
if (!exists("docs/M9-05-证据.md")) {
  fail("[DoD-5] 缺少 docs/M9-05-证据.md");
}

if (failures.length > 0) {
  console.error("[m9-05] FAIL");
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(
  `[m9-05] PASS 字段=${checked.envelopeFields} 事件类型=${checked.eventTypes} ` +
    `fixtures=${checked.fixtures} Rust 用例=${checked.rustTests} TS 用例=${checked.tsTests}`,
);
console.log("AETHER_M9_05 PASS");
process.exit(0);
