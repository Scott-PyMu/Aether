#!/usr/bin/env node
/**
 * M9-13 CI 数据夹具：用真实 `aetherd` 生成一个可被 `m9-13:data` 复制的数据目录。
 *
 * 目的：CI runner 没有 `%APPDATA%/Aether` 内测数据；本脚本生成同 schema 的最小
 * 「旧数据」目录（会话/消息/设置/审计/备份），供 `AETHER_M9_13_REAL_DATA` 使用。
 * 本地仍默认使用真实 `%APPDATA%/Aether`（不改变 DoD-3 口径）。
 *
 * 用法：node scripts/test/m9-13/seed-fixture.mjs --out dist-out/m9-13-fixture
 */
import { existsSync, mkdirSync, rmSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { DaemonRpc } from "../m9-11/lib/daemon-rpc.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const daemonPath = path.join(
  repoRoot,
  "target",
  "debug",
  process.platform === "win32" ? "aetherd.exe" : "aetherd",
);
const bundleDir = path.resolve(
  repoRoot,
  process.env.AETHER_M9_12_BUNDLE ?? path.join("dist-out", "m9-12-runtime-bundle"),
);
const outIndex = process.argv.indexOf("--out");
const outDir = path.resolve(
  repoRoot,
  outIndex >= 0 ? process.argv[outIndex + 1] : path.join("dist-out", "m9-13-fixture"),
);

if (!existsSync(daemonPath)) {
  console.error(`[m9-13-fixture] 缺少 aetherd：${daemonPath}（先 pnpm prepare:m9-12:e2e）`);
  process.exit(1);
}
if (!existsSync(path.join(bundleDir, "runtimes.json"))) {
  console.error(`[m9-13-fixture] 缺少 Mock runtime-bundle：${bundleDir}`);
  process.exit(1);
}

rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });
const tmp = path.join(outDir, "..", "tmp");
mkdirSync(tmp, { recursive: true });

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

const rpc = new DaemonRpc({
  binary: daemonPath,
  env: {
    AETHER_DATA_DIR: outDir,
    AETHER_DATA_LOCATION_FILE: path.join(outDir, "..", "m9-13-fixture-location.json"),
    AETHER_RUNTIME_REGISTRY_DIR: bundleDir,
    AETHER_SECRETS_PASSPHRASE: "m9-13-fixture-passphrase",
    USERPROFILE: path.join(outDir, ".."),
    HOME: path.join(outDir, ".."),
    TEMP: tmp,
    TMP: tmp,
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
    "daemon health",
  );

  const session = await rpc.request("session_create", {
    runtime_id: "mock",
    title: "M9-13 CI fixture",
  });
  const sessionId = session?.id ?? session?.session_id;
  if (typeof sessionId !== "string") {
    throw new Error(`session_create 未返回会话 id：${JSON.stringify(session)}`);
  }
  await rpc.request(
    "session_send",
    {
      session_id: sessionId,
      text: "hello",
      client_msg_id: "01J00000000000000000000F01",
    },
    30_000,
  );
  await waitFor(
    async () => {
      const page = await rpc.request(
        "messages_page",
        { session_id: sessionId, limit: 50 },
        10_000,
      );
      return Array.isArray(page?.messages) && page.messages.length >= 1 ? page : null;
    },
    30_000,
    "messages_page",
  );

  // 明文备份，避免 CI 无 OS 凭据库时备份失败（data-drill 只需要台账/产物存在）。
  await rpc.request("settings_set", {
    key: "backup.encryption.enabled",
    value: false,
  });
  const backup = await rpc.request("backup_create", { label: "M9-13 CI fixture" }, 60_000);
  if (!backup?.backup?.id) {
    throw new Error(`backup_create 未返回备份：${JSON.stringify(backup)}`);
  }
  await rpc.shutdown(15_000);
  console.log(`AETHER_M9_13_FIXTURE ${outDir}`);
} catch (error) {
  console.error(`[m9-13-fixture] FAIL ${error.message}`);
  await rpc.shutdown(5_000).catch(() => rpc.kill());
  process.exit(1);
}
