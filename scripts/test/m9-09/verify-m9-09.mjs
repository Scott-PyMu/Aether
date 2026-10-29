#!/usr/bin/env node
/**
 * `pnpm verify:m9-09`（P2 计划 M9-09；ADR-029 D-029-9/D-029-12、§3.5/§3.6/§3.7）。
 *
 * 静态门禁（可执行；动态证据见 `docs/M9-09-证据.md`）：
 *  1. 启动/关闭预算常量（窗口 1.5s / initialize 2s·10s / UI ready 5s / 会话 2s；
 *     daemon 8.2s / main 9.5s / T11 10s）；
 *  2. daemon 退避重启 1/2/4/8/16/30s、60s 内 ≥5 次熔断；整树回收（taskkill /T /F、
 *     Unix -pgid）；stdin EOF；
 *  3. daemon OS 独占锁、锁生命周期（锁先于 A4/DB）、M9 不迁移；
 *  4. Keychain preflight/S-13（只读、不删除旧条目）；
 *  5. 平台断言与证据/脚本/文档齐备。
 */
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const failures = [];
let checks = 0;

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function exists(rel) {
  return existsSync(path.join(root, rel));
}

function expect(condition, message) {
  checks += 1;
  if (!condition) failures.push(message);
  return condition;
}

function expectMatch(text, pattern, message) {
  return expect(pattern.test(text), message);
}

// ===== 1. 包脚本/文件 =====
const rootPkg = readJson("package.json");
expect(
  rootPkg.scripts?.["verify:m9-09"] === "node scripts/test/m9-09/verify-m9-09.mjs",
  "缺少 verify:m9-09",
);
expect(
  rootPkg.scripts?.["e2e:m9-09"] === "node scripts/test/m9-09/e2e-lifecycle.mjs",
  "缺少 e2e:m9-09",
);
expect(
  rootPkg.scripts?.["e2e:m9-09:crash"] === "node scripts/test/m9-09/e2e-crash.mjs",
  "缺少 e2e:m9-09:crash",
);
expect(
  rootPkg.scripts?.["e2e:m9-09:real-session"] ===
    "node scripts/test/m9-09/e2e-real-session.mjs",
  "缺少 e2e:m9-09:real-session",
);
expect(
  rootPkg.scripts?.["prepare:m9-09:real-session"] ===
    "node scripts/test/m9-09/prepare-real-session.mjs",
  "缺少 prepare:m9-09:real-session",
);
for (const file of [
  "apps/desktop/electron/daemonTree.ts",
  "apps/desktop/electron/daemonLifecycle.ts",
  "apps/desktop/electron/daemonTree.test.ts",
  "apps/desktop/electron/daemonLifecycle.test.ts",
  "scripts/test/m9-09/verify-m9-09.mjs",
  "scripts/test/m9-09/e2e-lifecycle.mjs",
  "scripts/test/m9-09/e2e-crash.mjs",
  "scripts/test/m9-09/e2e-real-session.mjs",
  "scripts/test/m9-09/prepare-real-session.mjs",
  "scripts/test/m9-09/fixtures/stuck-daemon.mjs",
  "crates/aether-daemon/tests/m9_09_lifecycle.rs",
  "crates/aether-app/src/keychain_preflight.rs",
  "crates/aether-app/tests/m9_09_keychain.rs",
  "docs/M9-09-证据.md",
]) {
  expect(exists(file), `缺少文件：${file}`);
}

// ===== 2. 启动/关闭预算 =====
const tree = read("apps/desktop/electron/daemonTree.ts");
expectMatch(tree, /DAEMON_SHUTDOWN_BUDGET_MS = 8_200/, "daemon 关闭预算必须 8.2s");
expectMatch(tree, /MAIN_EXIT_HARD_BUDGET_MS = 9_500/, "main 硬等待必须 9.5s");
expectMatch(tree, /T11_BUDGET_MS = 10_000/, "T11 必须 10s");
expectMatch(tree, /UNIX_TERM_GRACE_MS = 500/, "Unix SIGTERM 余量必须 500ms");
expectMatch(tree, /taskkill[\s\S]*\/T[\s\S]*\/F/, "Windows 必须 taskkill /T /F 整树回收");
expectMatch(tree, /kill\(group, "SIGTERM"\)/, "Unix 必须向进程组发 SIGTERM");
expectMatch(tree, /kill\(group, "SIGKILL"\)/, "Unix 必须向进程组发 SIGKILL");
expectMatch(tree, /detached: platform !== "win32"/, "Unix daemon 必须独立进程组");

const startup = read("apps/desktop/electron/startup.ts");
expectMatch(startup, /MIN_INITIALIZE_HARD_TIMEOUT_MS = INITIALIZE_HARD_TIMEOUT_MS/, "initialize 硬超时下限");
expectMatch(startup, /attempt <= 2/, "initialize 超时重试 1 次");
const client = read("apps/desktop/electron/daemonClient.ts");
expectMatch(client, /INITIALIZE_HARD_TIMEOUT_MS = 10_000/, "initialize 硬超时必须 10s");
expectMatch(client, /STDERR_RING_LINES/, "main 必须持续 drain stderr");

const app = read("apps/desktop/electron/app.ts");
for (const [label, pattern] of [
  ["启动计时 uiReadyAt", /uiReadyAt/],
  ["会话创建计时 sessionCreateMs", /sessionCreateMs/],
  ["窗口可见日志 window.ready", /window\.ready/],
  ["9.5s 强杀", /MAIN_EXIT_HARD_BUDGET_MS/],
  ["整树回收", /killDaemonTree/],
  ["退避策略", /DaemonRestartPolicy/],
  ["崩溃熔断日志", /daemon\.restart\.paused/],
  ["手动重试", /retryDaemonNow/],
  ["Keychain preflight 通知", /keychain_reauth_required/],
  ["S-13 口令降级", /restartDaemonForKeychain/],
  ["S-13 重授权", /keychain\.retry\.restart/],
]) {
  expectMatch(app, pattern, `app.ts 缺少${label}`);
}
expect(
  !/startup_get|startup_migrate/.test(app),
  "M9 main 不得产生 startup_* 路径（M9-07 冻结）",
);
expect(
  !/showFatalWindow\(\{[\s\S]{0,200}phase === "failed" && runtime\.ready/.test(app),
  "daemon ready 后 failed 不得切 S-12（保留窗口）",
);
expect(
  app.includes("AETHER_SECRETS_PASSPHRASE") && app.includes("passphraseSupplied"),
  "口令降级必须走一次性 daemon env 且快照只暴露布尔值",
);

const lifecycle = read("apps/desktop/electron/daemonLifecycle.ts");
for (const delay of ["1_000", "2_000", "4_000", "8_000", "16_000", "30_000"]) {
  expect(lifecycle.includes(delay), `退避序列缺少 ${delay}`);
}
expectMatch(lifecycle, /CRASH_WINDOW_MS = 60_000/, "崩溃窗口必须 60s");
expectMatch(lifecycle, /MAX_STOPS_IN_WINDOW = 5/, "熔断阈值必须 5 次");
const e2eLifecycle = read("scripts/test/m9-09/e2e-lifecycle.mjs");
expect(
  e2eLifecycle.includes('scenarioShutdownStall("checkpoint")') &&
    e2eLifecycle.includes('scenarioShutdownStall("write_queue")'),
  "E2E 缺少 checkpoint / 写队列卡住故障注入",
);

// ===== 3. daemon 锁/退出/不迁移 =====
const lock = read("crates/aether-daemon/src/lock.rs");
expectMatch(lock, /LOCK_FILE_NAME: &str = "\.aetherd\.lock"/, "锁文件名必须 .aetherd.lock");
expectMatch(lock, /FileExt::try_lock/, "必须 OS 独占锁（handle-based）");
expect(!/busy_timeout/.test(lock), "禁止用 busy_timeout 替代互斥");

const bootstrap = read("crates/aether-daemon/src/bootstrap.rs");
expectMatch(
  bootstrap,
  /DataDirLock::acquire\(&data_dir\)[\s\S]{0,900}snapshot\.phase == StartupPhase::BlockedSyncDir[\s\S]{0,900}aether_store::preflight/,
  "锁生命周期顺序必须是：取锁 → A4 → DB",
);
expectMatch(bootstrap, /lock_is_acquired_before_a4_check/, "缺少锁先于 A4 的回归测试");
expect(!/migrate\(/.test(bootstrap), "M9 daemon 启动不得执行迁移");

const serve = read("crates/aether-daemon/src/serve.rs");
expectMatch(serve, /DAEMON_SHUTDOWN_BUDGET: Duration = Duration::from_millis\(8_200\)/, "daemon 关闭预算必须 8.2s");
expectMatch(serve, /join_with_budget/, "关闭阶段必须有硬预算");
expectMatch(serve, /AETHER_TEST_SHUTDOWN_STALL/, "缺少 checkpoint/写队列卡住故障注入");
expectMatch(serve, /stdin EOF/, "必须有 stdin EOF 反向孤儿保护");
expectMatch(serve, /keychain_reauth_required/, "Keychain preflight 必须通知 main（S-13）");
expectMatch(serve, /join_with_budget_returns_none_when_stuck/, "缺少关闭预算故障注入单测");

// ===== 4. 适配器平台路径（DoD-7） =====
const processRs = read("crates/aether-adapters/src/process.rs");
expectMatch(processRs, /ProcessSession/, "Unix 必须 setsid 独立进程组");
expectMatch(processRs, /JobObject/, "Windows 必须 Job Object");
expectMatch(processRs, /TerminateJobObject|start_kill/, "Windows 强杀必须优先 TerminateJobObject");
const ledger = read("crates/aether-adapters/src/supervisor/ledger.rs");
expectMatch(ledger, /LEDGER_RELATIVE_PATH: &str = "\.aether\/run\/adapters\.json"/, "PID 台账路径");
expectMatch(ledger, /pub fn cleanup/, "启动孤儿清理");
expect(exists("crates/aether-adapters/tests/m1_10_termination.rs"), "缺少进程组/Job Object 平台测试");
expect(exists("crates/aether-adapters/tests/m1_10_ledger.rs"), "缺少台账清理测试（macOS 回归）");

// ===== 5. Keychain preflight =====
const preflight = read("crates/aether-app/src/keychain_preflight.rs");
const preflightProduction = preflight.split("#[cfg(test)]")[0] ?? preflight;
expectMatch(preflight, /MODE_REAUTH/, "Keychain preflight 必须报告 reauth");
expectMatch(preflight, /store\.get\(&reference\)/, "preflight 必须只读探测");
expect(!/store\.delete/.test(preflightProduction), "preflight 不得删除旧条目");
expectMatch(preflight, /unavailable_entry_requests_reauth_without_deleting/, "缺少不删除旧条目的测试");
expectMatch(preflight, /unmounted_store_requests_reauth_only_when_providers_exist/, "缺少未挂载存储的 S-13 边界测试");
const boot = read("crates/aether-app/src/boot.rs");
expectMatch(boot, /keychain_preflight/, "boot 必须接线 Keychain preflight");

// ===== 6. S-13 / 文档 / 旧包手工卸载 =====
const uiux = read("docs/UI-UX-设计规格.md");
for (const anchor of [
  "keychain-reauth",
  "keychain-reauth-reason",
  "keychain-reauth-retry",
  "keychain-reauth-passphrase",
  "keychain-reauth-skip",
  "keychain-reauth-notice",
]) {
  expect(uiux.includes(anchor) || read("apps/desktop/electron/keychain/keychain.html").includes(anchor), `S-13 缺少锚点 ${anchor}`);
}
const readme = read("README.md");
expect(
  readme.includes("旧内测") || readme.includes("手工卸载"),
  "README 必须说明旧内测包手工卸载（无回退包/跨壳锁）",
);
const p2 = read("docs/P2-架构迁移-实施计划与验收标准.md");
expect(p2.includes("### M9-09"), "P2 计划缺少 M9-09");
const evidence = read("docs/M9-09-证据.md");
for (const needle of ["DoD", "8.2s", "9.5s", "1/2/4/8/16/30", "Keychain", "未验证"]) {
  expect(evidence.includes(needle), `证据缺少 ${needle}`);
}

if (failures.length > 0) {
  console.error(`[m9-09] FAIL（${failures.length}/${checks} 项）`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(`[m9-09] PASS 检查=${checks}`);
console.log("AETHER_M9_09 PASS");
process.exit(0);
