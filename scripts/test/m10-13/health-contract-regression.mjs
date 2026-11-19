#!/usr/bin/env node
/**
 * M10-13 DoD-5：M10-03 发布门 —— 旧 v1 客户端 + 新 v2 daemon 回归。
 *
 * 口径（ADR-040 v1.0 / UI-UX v0.21）：
 *  - 新 daemon `health.storage_check` 返回 `contract_version=2` + `planned_shards` /
 *    `skipped_shards` / `full_coverage`；
 *  - 旧 v1 客户端仍能读取 `state` / `checked_shards` / `total_shards`（不崩溃），
 *    但不得把 `state=ok` 当作“全量覆盖”；
 *  - 旧 daemon fixture 不含 v2 字段，新 UI 走兼容分支（x/8，不声称全量）。
 *
 * 只使用 Node 标准库；真实 `aetherd --stdio`。输出
 * `scripts/test/.tmp/m10-13/health-contract.json`。
 */
import { existsSync, mkdirSync, readFileSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { DaemonRpc } from "../m9-11/lib/daemon-rpc.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const workRoot = path.join(root, "scripts", "test", ".tmp", "m10-13");
mkdirSync(workRoot, { recursive: true });
const outPath = path.join(workRoot, "health-contract.json");
const failures = [];
const checks = [];

function check(name, ok, detail = "") {
  checks.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `：${detail}` : ""}`);
  if (!ok) failures.push(name);
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

function resolveDaemon() {
  const exe = process.platform === "win32" ? "aetherd.exe" : "aetherd";
  const candidates = [
    process.env.AETHERD_BIN,
    path.join(root, "target", "debug", exe),
    path.join(root, "target", "release", exe),
  ].filter(Boolean);
  return candidates.find((candidate) => existsSync(candidate)) ?? null;
}

const daemon = resolveDaemon();
if (!daemon) {
  console.error("[m10-13] 找不到 aetherd（先 cargo build -p aether-daemon）");
  process.exit(1);
}

const work = mkdtempSync(path.join(workRoot, "health-"));
const dataDir = path.join(work, "data");
const registryDir = path.join(work, "registry");
mkdirSync(dataDir, { recursive: true });
mkdirSync(registryDir, { recursive: true });

const rpc = new DaemonRpc({
  binary: daemon,
  env: {
    AETHER_DATA_DIR: dataDir,
    AETHER_DATA_LOCATION_FILE: path.join(work, "data-location.json"),
    AETHER_RUNTIME_REGISTRY_DIR: registryDir,
    AETHER_SECRETS_PASSPHRASE: "m10-13-release-gate-passphrase",
    USERPROFILE: work,
    HOME: work,
  },
  cwd: root,
});

let health = null;
try {
  await rpc.request(
    "initialize",
    {
      protocolVersion: "1.0",
      appVersion: "0.1.0",
      client: "m10-13-release-gate",
      clientCapabilities: ["events"],
      requiredCapabilities: ["events"],
    },
    120_000,
  );
  health = await waitFor(
    async () => {
      try {
        const value = await rpc.request("health", {}, 10_000);
        return value?.storage_state ? value : null;
      } catch (error) {
        if (error.code === -32002) return null;
        throw error;
      }
    },
    120_000,
    "health",
  );

  const storageCheck = health.storage_check;
  check("新 daemon 返回 health.storage_check", Boolean(storageCheck));
  if (storageCheck) {
    check("contract_version=2", storageCheck.contract_version === 2, String(storageCheck.contract_version));
    check(
      "v2 字段齐全",
      Number.isInteger(storageCheck.planned_shards) &&
        Number.isInteger(storageCheck.skipped_shards) &&
        typeof storageCheck.full_coverage === "boolean",
      JSON.stringify({
        planned_shards: storageCheck.planned_shards,
        skipped_shards: storageCheck.skipped_shards,
        full_coverage: storageCheck.full_coverage,
      }),
    );
    check(
      "v1 字段保留",
      Number.isInteger(storageCheck.checked_shards) && Number.isInteger(storageCheck.total_shards),
      `${storageCheck.checked_shards}/${storageCheck.total_shards}`,
    );

    // 模拟旧 v1 消费者：只读 state/checked_shards/total_shards，不因新字段报错。
    const v1View = (() => {
      const value = health.storage_check ?? {};
      return {
        state: value.state,
        checked_shards: value.checked_shards,
        total_shards: value.total_shards,
      };
    })();
    check("旧 v1 消费者可读取（不崩溃）", ["checking", "ok", "failed"].includes(v1View.state), v1View.state);
    check(
      "旧 v1 消费者不会把选择性完成当作全量",
      v1View.state !== "ok" || storageCheck.full_coverage === true || v1View.checked_shards < v1View.total_shards,
      JSON.stringify(v1View),
    );
  }
} catch (error) {
  check("真实 daemon health v2 回归", false, error.message);
} finally {
  await rpc.shutdown(20_000).catch(() => rpc.kill());
  try {
    rmSync(work, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  } catch {
    // 句柄滞后：保留临时目录供排查。
  }
}

// 旧 daemon fixture：新 UI 必须走兼容分支，不声称全量。
const fixturePath = path.join(root, "scripts", "test", "m10-14", "level2-fixture.json");
if (existsSync(fixturePath)) {
  const fixture = JSON.parse(readFileSync(fixturePath, "utf8"));
  const oldHealth = fixture.health_v1 ?? {};
  check("旧 daemon fixture 无 contract_version", oldHealth.contract_version === undefined);
  check(
    "旧 daemon fixture 无 v2 字段",
    oldHealth.planned_shards === undefined &&
      oldHealth.skipped_shards === undefined &&
      oldHealth.full_coverage === undefined,
  );
  check("旧 daemon fixture checked_shards=8", oldHealth.checked_shards === 8, String(oldHealth.checked_shards));
} else {
  check("旧 daemon fixture 存在", false, fixturePath);
}

const result = {
  schema: "aether.m10-13.health-contract.v1",
  task: "M10-13",
  recorded_at: new Date().toISOString(),
  daemon,
  checks,
  passed: failures.length === 0,
};
writeFileSync(outPath, `${JSON.stringify(result, null, 2)}\n`, "utf8");
console.log(`\n[m10-13] 证据：${path.relative(root, outPath)}`);
if (failures.length > 0) {
  console.error(`[m10-13] health v2 回归 FAIL（${failures.length} 项）`);
  process.exit(1);
}
console.log("AETHER_M10_13_HEALTH_CONTRACT PASS");
