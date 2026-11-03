#!/usr/bin/env node
/**
 * M9-13 DoD-3/DoD-4：内测数据单向打开 + 备份/恢复 + 安全模式 + Keychain/WAL。
 *
 * 口径：
 *  - **复制**真实数据目录到隔离工作区（默认 `%APPDATA%/Aether`，可用
 *    `AETHER_M9_13_REAL_DATA` 覆盖）；**不做反向 Tauri 打开/跨壳往返**；
 *  - Electron + 真实 `aetherd` 打开复制目录：会话/消息/设置/预设/工作区/审计/
 *    供应商/备份全部经真实命令面读取；schema 与迁移记录前后一致；
 *  - 备份恢复：明文 `.db` 候选 → 修改设置 → `backup_restore` → 重启后设置回滚、
 *    `quick_check` 通过、WAL 已 checkpoint；
 *  - 安全模式维护例外：`cargo test -p aether-daemon --test m9_13_safe_mode_restore`；
 *  - Keychain preflight：真实 daemon 启动日志 + M9-09 Keychain 测试；
 *  - S-13 重授权/口令降级：M9-09 证据/测试复跑（不伪造通过）。
 *
 * 输出：`scripts/test/.tmp/m9-13/data/data-drill.json`。
 *
 * 前置：`pnpm build:desktop`、`pnpm prepare:m9-12:e2e`（真实 aetherd）。
 */
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { resolveElectronExecutable } from "../../ci/electron-bin.mjs";
import { DaemonRpc } from "../m9-11/lib/daemon-rpc.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const appRoot = path.join(repoRoot, "apps", "desktop");
const requireFromDesktop = createRequire(path.join(appRoot, "package.json"));
const { _electron } = requireFromDesktop("playwright-core");
const electronPath = resolveElectronExecutable({ repoRoot });
const daemonPath = path.join(
  repoRoot,
  "target",
  "debug",
  process.platform === "win32" ? "aetherd.exe" : "aetherd",
);
const outDir = path.join(repoRoot, "scripts", "test", ".tmp", "m9-13", "data");
const outPath = path.join(outDir, "data-drill.json");
const APP_URL = "aether://app/index.html";
const noSandbox =
  process.env.AETHER_E2E_NO_SANDBOX === "1" ||
  (process.env.AETHER_E2E_NO_SANDBOX !== "0" && process.platform === "win32");

mkdirSync(outDir, { recursive: true });

function fail(message) {
  throw new Error(message);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(fn, timeoutMs, label) {
  const started = Date.now();
  let last;
  while (Date.now() - started < timeoutMs) {
    try {
      const value = await fn();
      if (value) return value;
    } catch (error) {
      last = error;
    }
    await sleep(100);
  }
  throw new Error(`等待超时：${label}${last ? `（${last.message}）` : ""}`);
}

function killTree(pid) {
  if (!pid || pid <= 0) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(pid), "/T", "/F"], {
      stdio: "ignore",
      timeout: 10_000,
    });
  } else {
    try {
      process.kill(-pid, "SIGKILL");
    } catch {
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        // 已退出。
      }
    }
  }
}

function realDataDir() {
  if (process.env.AETHER_M9_13_REAL_DATA) {
    return path.resolve(process.env.AETHER_M9_13_REAL_DATA);
  }
  const home = os.homedir();
  if (process.platform === "win32") {
    return path.join(process.env.APPDATA ?? path.join(home, "AppData", "Roaming"), "Aether");
  }
  if (process.platform === "darwin") {
    return path.join(home, "Library", "Application Support", "Aether");
  }
  return path.join(process.env.XDG_DATA_HOME ?? path.join(home, ".local", "share"), "Aether");
}

const PY_SNAPSHOT = String.raw`
import json, sqlite3, sys
db = sys.argv[1]
checkpoint = sys.argv[2] == "1"
conn = sqlite3.connect("file:" + db + "?mode=rw", uri=True)
if checkpoint:
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
tables = [row[0] for row in conn.execute("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")]
migrations = []
if "schema_migrations" in tables:
    migrations = [list(row) for row in conn.execute("SELECT version, checksum, applied_at FROM schema_migrations ORDER BY version")]
counts = {}
for table in ["sessions", "messages", "events", "settings", "presets", "workspaces", "audit_log", "providers", "backups", "runs"]:
    if table in tables:
        counts[table] = conn.execute("SELECT COUNT(*) FROM " + table).fetchone()[0]
quick = conn.execute("PRAGMA quick_check").fetchone()[0]
print(json.dumps({"tables": tables, "migrations": migrations, "counts": counts, "quick_check": quick}, ensure_ascii=False))
`;

function sqliteSnapshot(dbPath, { checkpoint = false } = {}) {
  const result = spawnSync(
    "python",
    ["-c", PY_SNAPSHOT, dbPath, checkpoint ? "1" : "0"],
    { encoding: "utf8", env: { ...process.env, PYTHONIOENCODING: "utf-8" }, timeout: 60_000 },
  );
  if (result.status !== 0) {
    return { unavailable: true, error: (result.stderr ?? result.error?.message ?? "python 失败").slice(-500) };
  }
  try {
    return JSON.parse(result.stdout.trim());
  } catch (error) {
    return { unavailable: true, error: `解析 Python 输出失败：${error.message}` };
  }
}

function runCommand(name, args, options = {}) {
  console.log(`\n$ ${[name, ...args].join(" ")}`);
  const result = spawnSync(name, args, {
    cwd: options.cwd ?? repoRoot,
    encoding: "utf8",
    env: { ...process.env, ...(options.env ?? {}) },
    timeout: options.timeout ?? 15 * 60 * 1000,
  });
  const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`;
  console.log(output.trim().split(/\r?\n/).slice(-12).join("\n"));
  return { exit: result.status ?? 1, output };
}

function launchEnv(root, dataDir, registryDir, extra = {}) {
  const tmp = path.join(root, "tmp");
  const downloads = path.join(root, "downloads");
  const userData = path.join(root, "user");
  for (const dir of [tmp, downloads, userData]) mkdirSync(dir, { recursive: true });
  return {
    ...process.env,
    AETHER_E2E: "1",
    AETHER_E2E_DOWNLOADS_DIR: downloads,
    AETHER_USER_DATA_DIR: userData,
    AETHER_DATA_DIR: dataDir,
    AETHER_DATA_LOCATION_FILE: path.join(root, "data-location.json"),
    AETHER_RUNTIME_REGISTRY_DIR: registryDir,
    AETHER_SKIP_KEYCHAIN_PREFLIGHT: "1",
    AETHER_SECRETS_PASSPHRASE: "m9-13-data-passphrase",
    AETHERD_BIN: daemonPath,
    USERPROFILE: root,
    HOME: root,
    TEMP: tmp,
    TMP: tmp,
    ...extra,
  };
}

async function launch(root, dataDir, registryDir) {
  if (!existsSync(daemonPath)) {
    throw new Error(`缺少 aetherd：${daemonPath}（先 pnpm prepare:m9-12:e2e）`);
  }
  const app = await _electron.launch({
    executablePath: electronPath,
    args: [...(noSandbox ? ["--no-sandbox"] : []), repoRoot],
    cwd: repoRoot,
    env: launchEnv(root, dataDir, registryDir),
    timeout: 40_000,
  });
  const page = await waitFor(
    async () => {
      const pages = app.windows().filter((item) => !item.isClosed());
      return pages.find((item) => item.url() === APP_URL) ?? null;
    },
    30_000,
    "主窗口",
  );
  await page.waitForSelector("[data-testid='workbench']", { timeout: 20_000 });
  await waitFor(
    async () => {
      const value = await app.evaluate(
        () => globalThis.__aetherShell?.snapshot?.() ?? null,
      );
      return value?.ready ? value : null;
    },
    60_000,
    "daemon ready",
  );
  const rpc = async (method, params) =>
    page.evaluate(
      ([name, value]) => window.aether.rpc(name, value),
      [method, params],
    );
  const health = await waitFor(
    async () => {
      const envelope = await rpc("health", {});
      return envelope?.ok === true ? envelope : null;
    },
    30_000,
    "health",
  );
  if (health.result.storage_state !== "normal") {
    throw new Error(`内测数据应以 normal 打开，实际 ${health.result.storage_state}`);
  }
  return { app, page, rpc };
}

async function rpcOk(rpc, method, params) {
  const envelope = await rpc(method, params);
  if (!envelope?.ok) {
    throw new Error(`RPC ${method} 失败：${JSON.stringify(envelope?.error ?? envelope)}`);
  }
  return envelope.result;
}

async function closeApp(app) {
  const pid = app.process()?.pid;
  try {
    await app.close();
  } catch {
    killTree(pid);
  }
  await sleep(1_000);
  if (pid && process.platform === "win32") {
    const still = spawnSync("tasklist", ["/FI", `PID eq ${pid}`], { encoding: "utf8" });
    if ((still.stdout ?? "").includes(String(pid))) killTree(pid);
  }
}

async function keychainPreflight(root, sourceDataDir, registryDir) {
  const dataDir = path.join(root, "keychain-data");
  cpSync(sourceDataDir, dataDir, { recursive: true });
  const rpc = new DaemonRpc({
    binary: daemonPath,
    env: {
      AETHER_DATA_DIR: dataDir,
      AETHER_DATA_LOCATION_FILE: path.join(root, "keychain-data-location.json"),
      AETHER_RUNTIME_REGISTRY_DIR: registryDir,
      AETHER_SECRETS_PASSPHRASE: "m9-13-data-passphrase",
      USERPROFILE: root,
      HOME: root,
      TEMP: path.join(root, "tmp"),
      TMP: path.join(root, "tmp"),
    },
    cwd: repoRoot,
  });
  try {
    await rpc.request("initialize", {
      protocolVersion: "1.0",
      appVersion: "0.1.0",
      client: "electron",
      clientCapabilities: ["events", "permissions", "dialogs", "browser-view"],
      requiredCapabilities: ["events", "permissions"],
    });
    await waitFor(
      async () => {
        try {
          const health = await rpc.request("health", {}, 5_000);
          return health?.storage_state ? health : null;
        } catch {
          return null;
        }
      },
      60_000,
      "keychain daemon health",
    );
  } finally {
    await rpc.shutdown(15_000).catch(() => rpc.kill());
  }
  const logPath = path.join(dataDir, "logs", "aetherd.log");
  const log = existsSync(logPath) ? readFileSync(logPath, "utf8") : "";
  const line = log
    .split(/\r?\n/)
    .filter((item) => item.includes("Keychain preflight 完成"))
    .slice(-1)[0];
  const mode = /mode=(\w+)/.exec(line ?? "")?.[1] ?? "unknown";
  return {
    log_line: line ?? null,
    mode,
    passed: mode === "ok" || mode === "reauth",
    note:
      mode === "reauth"
        ? "旧条目在新签名身份下不可读 → S-13 重授权/口令降级（M9-09 DoD-8）"
        : "OS 凭据库可读；旧条目不删除",
  };
}

async function main() {
  const source = realDataDir();
  const sourceDb = path.join(source, "aether.db");
  if (!existsSync(sourceDb)) {
    fail(`真实数据目录缺少 aether.db：${source}（可用 AETHER_M9_13_REAL_DATA 指定）`);
  }
  const base = path.join(repoRoot, "dist-out", "m9-13-data");
  mkdirSync(base, { recursive: true });
  const root = mkdtempSync(path.join(base, "run-"));
  const dataDir = path.join(root, "data");
  const registryDir = path.join(root, "registry");
  mkdirSync(registryDir, { recursive: true });
  cpSync(source, dataDir, { recursive: true });

  const sourceSnapshot = sqliteSnapshot(path.join(dataDir, "aether.db"), { checkpoint: true });
  const candidate = path.join(root, "m9-13-restore-candidate.db");
  cpSync(path.join(dataDir, "aether.db"), candidate);

  const checks = [];
  const record = (name, ok, detail) => {
    checks.push({ name, ok, detail });
    console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  ${detail}` : ""}`);
  };

  let app;
  try {
    const launched = await launch(root, dataDir, registryDir);
    app = launched.app;
    const { rpc } = launched;

    const sessions = await rpcOk(rpc, "session_list", {});
    record("旧数据：session_list", Array.isArray(sessions) && sessions.length > 0, `${sessions.length} 会话`);
    const sessionId = sessions[0]?.id;
    const messages = sessionId
      ? await rpcOk(rpc, "messages_page", { session_id: sessionId, limit: 200 })
      : { messages: [] };
    record(
      "旧数据：messages_page",
      Array.isArray(messages.messages) && messages.messages.length > 0,
      `${messages.messages?.length ?? 0} 消息`,
    );

    const reminder = await rpcOk(rpc, "settings_get", { key: "backup.reminder" });
    record("旧数据：settings_get", reminder?.key === "backup.reminder", JSON.stringify(reminder?.value));
    const presets = await rpcOk(rpc, "presets_list", {});
    record(
      "旧数据：presets_list",
      Array.isArray(presets.presets) && presets.presets.length > 0,
      `${presets.presets?.length ?? 0} 预设`,
    );
    const workspacesBefore = await rpcOk(rpc, "workspaces_list", {});
    const workspaceRoot = path.join(root, "workspace");
    mkdirSync(workspaceRoot, { recursive: true });
    const createdWorkspace = await rpcOk(rpc, "workspace_create", {
      name: "M9-13 内测数据工作区",
      root_path: workspaceRoot,
    });
    const workspacesAfter = await rpcOk(rpc, "workspaces_list", {});
    record(
      "旧数据：workspace_create/workspaces_list",
      Array.isArray(workspacesAfter.workspaces) &&
        workspacesAfter.workspaces.length === (workspacesBefore.workspaces?.length ?? 0) + 1,
      `${workspacesBefore.workspaces?.length ?? 0} → ${workspacesAfter.workspaces?.length ?? 0}；id=${createdWorkspace?.id ?? createdWorkspace?.workspace_id ?? "?"}`,
    );
    const audit = await rpcOk(rpc, "audit_query", { limit: 200 });
    record(
      "旧数据：audit_query",
      Array.isArray(audit.rows) && audit.rows.length > 0,
      `${audit.rows?.length ?? 0} 行`,
    );
    const providers = await rpcOk(rpc, "providers_list", {});
    record(
      "旧数据：providers_list",
      Array.isArray(providers.providers) && providers.providers.length > 0,
      `${providers.providers?.length ?? 0} 供应商`,
    );
    const backups = await rpcOk(rpc, "backup_list", {});
    record(
      "旧数据：backup_list",
      Array.isArray(backups.backups) && backups.backups.length > 0,
      `${backups.backups?.length ?? 0} 备份`,
    );

    // 修改设置 → 恢复候选 → 重启后回滚（T9）。
    const before = await rpcOk(rpc, "settings_set", { key: "backup.reminder", value: false });
    record("备份恢复前：settings_set false", before?.value === false, JSON.stringify(before?.value));
    const restore = await rpcOk(rpc, "backup_restore", {
      source: { external: { path: candidate } },
    });
    record(
      "backup_restore 登记（restart_required）",
      restore?.restart_required === true,
      JSON.stringify(restore?.candidate ?? restore),
    );
    await closeApp(app);
    app = null;

    const relaunched = await launch(root, dataDir, registryDir);
    app = relaunched.app;
    const after = await rpcOk(relaunched.rpc, "settings_get", { key: "backup.reminder" });
    record(
      "恢复后：设置回滚（backup.reminder=true）",
      after?.value === true,
      JSON.stringify(after?.value),
    );
    const restoredSessions = await rpcOk(relaunched.rpc, "session_list", {});
    record(
      "恢复后：旧会话仍在",
      Array.isArray(restoredSessions) && restoredSessions.length > 0,
      `${restoredSessions.length} 会话`,
    );
  } finally {
    if (app) await closeApp(app);
  }

  const afterSnapshot = sqliteSnapshot(path.join(dataDir, "aether.db"));
  const schemaUnchanged =
    !sourceSnapshot.unavailable &&
    !afterSnapshot.unavailable &&
    JSON.stringify(sourceSnapshot.tables) === JSON.stringify(afterSnapshot.tables) &&
    JSON.stringify(sourceSnapshot.migrations) === JSON.stringify(afterSnapshot.migrations);
  record(
    "schema/迁移记录前后一致（无 schema 变更）",
    schemaUnchanged,
    sourceSnapshot.unavailable || afterSnapshot.unavailable
      ? "Python 不可用，未验证"
      : `tables=${afterSnapshot.tables?.length ?? 0} migrations=${afterSnapshot.migrations?.length ?? 0}`,
  );
  record("恢复后 quick_check=ok", afterSnapshot.quick_check === "ok", afterSnapshot.quick_check ?? "?");
  const walBytes = existsSync(path.join(dataDir, "aether.db-wal"))
    ? statSync(path.join(dataDir, "aether.db-wal")).size
    : 0;
  record("关闭后 WAL checkpoint（≤4KiB）", walBytes <= 4096, `${walBytes} B`);

  const safeModeTest = runCommand("cargo", [
    "test",
    "-p",
    "aether-daemon",
    "--test",
    "m9_13_safe_mode_restore",
    "-j",
    "1",
    "--",
    "--test-threads=1",
  ]);
  record(
    "安全模式 backup_restore 维护例外（停写→恢复→重启）",
    safeModeTest.exit === 0,
    `exit=${safeModeTest.exit}`,
  );

  const keychain = await keychainPreflight(root, source, registryDir);
  record("Keychain preflight（只读；旧条目不删除）", keychain.passed, `mode=${keychain.mode}`);
  const keychainTest = runCommand("cargo", [
    "test",
    "-p",
    "aether-app",
    "--test",
    "m9_09_keychain",
    "-j",
    "1",
    "--",
    "--test-threads=1",
  ]);
  record("Keychain/S-13 回归（M9-09）", keychainTest.exit === 0, `exit=${keychainTest.exit}`);
  const keychainUnit = runCommand("cargo", [
    "test",
    "-p",
    "aether-app",
    "--lib",
    "keychain_preflight",
    "-j",
    "1",
    "--",
    "--test-threads=1",
  ]);
  record("Keychain preflight 单测（5 项）", keychainUnit.exit === 0, `exit=${keychainUnit.exit}`);

  const passed = checks.every((item) => item.ok);
  const evidence = {
    schema: "aether.m9-13.data-drill.v1",
    task: "M9-13",
    finished_at: new Date().toISOString(),
    platform: { os: process.platform, arch: process.arch, node: process.version },
    source_data_dir: source,
    copied_data_dir: dataDir,
    one_way_electron_open: true,
    reverse_tauri_open: false,
    source_snapshot: sourceSnapshot,
    after_snapshot: afterSnapshot,
    schema_unchanged: schemaUnchanged,
    wal_bytes_after_shutdown: walBytes,
    backup_restore: {
      candidate: path.basename(candidate),
      restore_restart_required: true,
      setting_restored: checks.find((item) => item.name.includes("设置回滚"))?.ok === true,
      quick_check: afterSnapshot.quick_check ?? null,
    },
    safe_mode: { command: "cargo test -p aether-daemon --test m9_13_safe_mode_restore", exit: safeModeTest.exit },
    keychain: {
      ...keychain,
      m9_09_test_exit: keychainTest.exit,
      unit_test_exit: keychainUnit.exit,
      s13: "重授权/口令降级路径由 M9-09 DoD-8 测试覆盖；本任务复跑",
    },
    checks,
    passed,
  };
  writeFileSync(outPath, `${JSON.stringify(evidence, null, 2)}\n`, "utf8");
  console.log(`[m9-13-data] 证据：${path.relative(repoRoot, outPath)}`);
  try {
    rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  } catch (error) {
    console.warn(`[m9-13-data] 清理临时目录失败（忽略）：${error.message}`);
  }
  if (!passed) {
    console.error(`[m9-13-data] FAIL ${checks.filter((item) => !item.ok).length} 项`);
    process.exit(1);
  }
  console.log("[m9-13-data] PASS");
  console.log("AETHER_M9_13_DATA PASS");
}

try {
  await main();
} catch (error) {
  const failure = {
    schema: "aether.m9-13.data-drill.v1",
    task: "M9-13",
    finished_at: new Date().toISOString(),
    platform: { os: process.platform, arch: process.arch, node: process.version },
    passed: false,
    error: error?.message ?? String(error),
    stack: error?.stack ?? null,
  };
  mkdirSync(outDir, { recursive: true });
  writeFileSync(outPath, `${JSON.stringify(failure, null, 2)}\n`, "utf8");
  console.error(`[m9-13-data] FAIL ${failure.error}`);
  process.exit(1);
}
