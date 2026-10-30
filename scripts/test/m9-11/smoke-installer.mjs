#!/usr/bin/env node
/**
 * `pnpm smoke:installer`（M9-11 DoD-4/DoD-7/DoD-8）。
 *
 * Windows（本机可执行）：
 *   1. MSI 静态断言：Property/Upgrade/RemoveFile（无旧 MSI 升级链、不删数据目录）；
 *   2. 已安装产品预检：发现任何旧 Aether（Tauri 内测包）→ 拒绝双安装并给出手工卸载指引；
 *   3. 静默安装 → 校验安装布局（`resources/bin/aetherd` + `runtime-bundle` + asar 无 sidecar）；
 *   4. 首启（真实安装产物）→ shell.log 计时（窗口/daemon/UI ready）+ idle RSS + Keychain preflight；
 *   5. 安装产物内 `aetherd --stdio` 功能闭环：initialize → runtimes_list → 会话（Mock 测试构建）
 *      → session_send → session_interrupt → session_list；10 会话 RSS 采样；
 *   6. 静默卸载 → 安装目录移除、数据目录与哨兵文件保留。
 *
 * macOS：挂载 DMG → 校验 `.app` 布局/嵌套签名 → 卸载镜像（UI 驱动与安装向导属 CI 真机，见证据）。
 *
 * 用法：
 *   node scripts/test/m9-11/smoke-installer.mjs [--variant release|test] [--msi <path>]
 *     [--keep-installed] [--keep-data] [--skip-uninstall]
 *
 * 诚实口径：发布形态（release）内置三官方运行时，无 Mock；「发送/中断」功能闭环在
 * **测试构建（--variant test）** 上执行（Mock 适配器仅测试构建，ADR-008）。release 冒烟
 * 覆盖安装/首启/卸载/数据保留与 daemon 注册清单；两者共同构成 DoD-4 证据。
 */
import { createHash } from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { summarize } from "../lib/exec.mjs";
import { listAsarFiles, asarContainsBasename } from "../../ci/asar.mjs";
import { DaemonRpc, isEvent } from "./lib/daemon-rpc.mjs";
import {
  closeMainWindow,
  listInstalledAetherProducts,
  measureProcessTree,
  readMsiProperties,
  readMsiTable,
} from "./lib/msi.mjs";
import {
  createChecks,
  findShellEvent,
  isPidAlive,
  readJsonLines,
  sleep,
  waitFor,
} from "./lib/util.mjs";

const repoRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "..",
);
const variant = process.argv.includes("--variant")
  ? process.argv[process.argv.indexOf("--variant") + 1]
  : "release";
const requestedMsi = process.argv.includes("--msi")
  ? path.resolve(process.argv[process.argv.indexOf("--msi") + 1])
  : null;
const keepInstalled = process.argv.includes("--keep-installed");
const keepData = process.argv.includes("--keep-data");
const skipUninstall = process.argv.includes("--skip-uninstall");
const packageJson = JSON.parse(readFileSync(path.join(repoRoot, "package.json"), "utf8"));
const thresholds = JSON.parse(
  readFileSync(path.join(repoRoot, "docs", "M9-01-baseline", "freeze", "thresholds.json"), "utf8"),
);
const { checks, record, skip } = createChecks();
const evidence = {
  schema: "aether.m9-11.installer-smoke.v1",
  variant,
  started_at: new Date().toISOString(),
  platform: process.platform,
  metrics: {},
  checks: checks,
};

function findMsi() {
  if (requestedMsi) return requestedMsi;
  const outDir = path.join(repoRoot, "dist-out");
  if (!existsSync(outDir)) return null;
  const suffix = variant === "test" ? "-test" : "";
  const name = `Aether-${packageJson.version}${suffix}-x64.msi`;
  const candidate = path.join(outDir, name);
  if (existsSync(candidate)) return candidate;
  const matches = readdirSync(outDir)
    .filter((entry) => entry.endsWith(".msi") && entry.includes(suffix))
    .sort();
  return matches.length > 0 ? path.join(outDir, matches[matches.length - 1]) : null;
}

function sha256(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

function versionMatches(actual, expected) {
  const normalize = (value) => {
    const parts = String(value ?? "").split(".").map((part) => Number.parseInt(part, 10) || 0);
    while (parts.length < 4) parts.push(0);
    return parts.slice(0, 4).join(".");
  };
  return normalize(actual) === normalize(expected);
}

function msiexec(args, label) {
  const exe = path.join(process.env.SystemRoot ?? "C:\\Windows", "System32", "msiexec.exe");
  console.log(`\n$ msiexec ${args.join(" ")}   # ${label}`);
  const result = spawnSync(exe, args, { encoding: "utf8", timeout: 900_000 });
  if (result.error) throw new Error(`msiexec 无法执行：${result.error.message}`);
  const code = result.status ?? -1;
  console.log(`[smoke] msiexec exit=${code}`);
  return { code, stderr: result.stderr ?? "", stdout: result.stdout ?? "" };
}

function installDirCandidates(properties, products) {
  const candidates = [];
  const product = products.find(
    (item) => item.product_code?.toUpperCase() === (properties.ProductCode ?? "").toUpperCase(),
  );
  if (product?.install_location) candidates.push(product.install_location);
  const programFiles = process.env.ProgramFiles ?? "C:\\Program Files";
  const localPrograms = process.env.LOCALAPPDATA
    ? path.join(process.env.LOCALAPPDATA, "Programs")
    : null;
  for (const base of [programFiles, localPrograms].filter(Boolean)) {
    for (const name of ["Aether", "@aetherroot"]) {
      candidates.push(path.join(base, name));
    }
  }
  return candidates.filter((candidate) => existsSync(path.join(candidate, "Aether.exe")));
}

function assertLayout(installDir) {
  const resources = path.join(installDir, "resources");
  const exe = path.join(installDir, "Aether.exe");
  record("安装目录包含 Aether.exe", existsSync(exe), exe);
  const daemon = path.join(resources, "bin", "aetherd.exe");
  record("extraResources: resources/bin/aetherd.exe", existsSync(daemon), daemon);
  const manifestPath = path.join(resources, "runtime-bundle", "runtimes.json");
  record("extraResources: resources/runtime-bundle/runtimes.json", existsSync(manifestPath), manifestPath);
  let ids = [];
  if (existsSync(manifestPath)) {
    ids = (JSON.parse(readFileSync(manifestPath, "utf8")).runtimes ?? []).map((entry) => entry.id);
  }
  const official = ["claude-code", "codex", "deepseek-harness"];
  record(
    "runtime-bundle 含三官方运行时",
    official.every((id) => ids.includes(id)),
    ids.join(","),
  );
  record(
    variant === "release" ? "release 产物无 Mock" : "test 产物含 Mock",
    variant === "release" ? !ids.includes("mock") : ids.includes("mock"),
    ids.join(","),
  );
  const asarPath = path.join(resources, "app.asar");
  record("resources/app.asar 存在", existsSync(asarPath), asarPath);
  record(
    "壳级页面已解包（asarUnpack；file:// 不读 asar）",
    existsSync(
      path.join(
        resources,
        "app.asar.unpacked",
        "apps",
        "desktop",
        "electron",
        "dist",
        "shell",
        "startup",
        "startup.html",
      ),
    ),
  );
  if (existsSync(asarPath)) {
    const files = listAsarFiles(asarPath);
    const sidecars = [
      "aetherd",
      "aetherd.exe",
      "aether-claude-adapter",
      "aether-codex-adapter",
      "aether-dsh-adapter",
      "aether-mock-adapter",
    ].filter((name) => asarContainsBasename(files, name));
    record("asar 内无 sidecar（sidecar 仅 extraResources）", sidecars.length === 0, sidecars.join(",") || "0");
    record(
      "asar 含 renderer/main 入口",
      files.includes("/apps/desktop/dist/index.html") &&
        files.includes("/apps/desktop/electron/dist/main.cjs"),
      `${files.length} 个条目`,
    );
    evidence.metrics.asar_entries = files.length;
  }
  if (existsSync(daemon)) {
    const version = spawnSync(daemon, ["--version"], { encoding: "utf8", timeout: 30_000 });
    record(
      "安装产物 aetherd --version 可执行",
      version.status === 0 && /\d+\.\d+\.\d+/.test(`${version.stdout}${version.stderr}`),
      `${version.stdout ?? ""}${version.stderr ?? ""}`.trim().split(/\r?\n/)[0] ?? "",
    );
  }
  return { resources, daemon, ids };
}

/** 首启：真实安装产物；返回计时/RSS/日志。 */
async function firstLaunch({ exe, dataDir, userData, root }) {
  mkdirSync(dataDir, { recursive: true });
  mkdirSync(userData, { recursive: true });
  // Electron `app.getPath("downloads")` 需要 `%USERPROFILE%\Downloads` 存在；
  // 冒烟隔离 USERPROFILE 时显式创建（否则 main 启动即失败）。
  mkdirSync(path.join(root, "Downloads"), { recursive: true });
  const startedAt = Date.now();
  const child = spawn(exe, [], {
    cwd: path.dirname(exe),
    env: {
      ...process.env,
      AETHER_DATA_DIR: dataDir,
      AETHER_DATA_LOCATION_FILE: path.join(root, "data-location.json"),
      AETHER_USER_DATA_DIR: userData,
      // 隔离 D5 台账（~/.aether/run），避免宿主机临时目录/ACL 影响。
      USERPROFILE: root,
      HOME: root,
    },
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  const stderr = [];
  child.stderr.on("data", (chunk) => {
    if (stderr.length < 200) stderr.push(chunk.toString("utf8").trimEnd());
  });

  const shellLog = path.join(dataDir, "logs", "shell.log");
  const waitShell = async (event, timeoutMs) =>
    waitFor(
      () => {
        const records = readJsonLines(shellLog);
        return findShellEvent(records, event);
      },
      timeoutMs,
      `shell.log ${event}`,
    );

  const result = { pid: child.pid, startedAt, exited: null, shellLog, daemonLog: path.join(dataDir, "logs", "aetherd.log") };
  child.on("exit", (code, signal) => {
    result.exited = { code, signal };
  });

  let windowReady = null;
  let uiReady = null;
  try {
    windowReady = await waitShell("window.ready", 20_000);
    record(
      "首启窗口可见 ≤1.5s（安装产物）",
      Number(windowReady.visibleMs) <= thresholds.startup.window_shown_max_ms,
      `visibleMs=${windowReady.visibleMs}`,
    );
    uiReady = await waitShell("ui.ready", 30_000);
    // M9-11 DoD-7 要求**记录**冷启动/首窗/daemon ready；`UI ready ≤8s` 的硬阈值属
    // M9-13/G7（P2 计划 §2.13 DoD-5）。此处不掩盖测量值：超限写入证据并标注。
    record(
      "首启 UI ready 记录（阈值 ≤5s；G7 判定）",
      true,
      `uiReadyMs=${uiReady.uiReadyMs}${
        Number(uiReady.uiReadyMs) <= thresholds.startup.ui_ready_max_ms ? "" : "（超阈值，G7 关注）"
      }`,
    );
  } catch (error) {
    record("首启窗口/UI ready", false, error.message);
  }

  // 等待稳定后测 idle RSS（含 renderer/GPU/daemon/适配器进程树）。
  await sleep(2_500);
  let rss = null;
  try {
    rss = measureProcessTree(child.pid);
    // 硬阈值（idle RSS ≤500MiB）由 `verify:m9-11` 依据本证据 JSON 判定；冒烟本身
    // 只负责**采集**，避免 DoD-4/DoD-8 的安装/卸载语义被性能门禁耦合。
    record(
      "idle RSS 记录（硬阈值 ≤500MiB；G6 判定见 verify:m9-11）",
      rss.processes.length > 0,
      `${rss.total_mib} MiB / ${rss.processes.length} 进程${
        Number(rss.total_bytes) <= thresholds.runtime.idle_rss_max_bytes ? "" : "（超阈值）"
      }`,
    );
  } catch (error) {
    record("idle RSS 采样", false, error.message);
  }

  const shellRecords = readJsonLines(shellLog);
  const daemonStatus = shellRecords.filter((entry) => entry.event === "daemon.status").slice(-3);
  const keychainReauth = findShellEvent(shellRecords, "keychain.preflight.reauth");
  record(
    "Keychain preflight 未要求重新授权（安装产物，Windows Credential Manager）",
    keychainReauth === null,
    keychainReauth ? "S-13 已显示（见证据）" : "mode=ok/未触发",
  );
  const daemonRecords = readJsonLines(result.daemonLog);
  const keychainLine = daemonRecords.filter((entry) =>
    JSON.stringify(entry).includes("keychain"),
  );
  evidence.metrics.keychain_preflight = keychainLine.slice(-2);

  // 数据目录与 DB 初始化（首启真实产物）。
  record("首启创建 data 目录 DB", existsSync(path.join(dataDir, "aether.db")), path.join(dataDir, "aether.db"));
  const sentinel = path.join(dataDir, "m9-11-sentinel.txt");
  writeFileSync(sentinel, "M9-11 uninstall must keep this file\n", "utf8");
  evidence.data_dir = dataDir;
  evidence.sentinel = sentinel;

  evidence.metrics.first_launch = {
    window_visible_ms: windowReady?.visibleMs ?? null,
    window_within_threshold:
      windowReady !== null &&
      Number(windowReady.visibleMs) <= thresholds.startup.window_shown_max_ms,
    ui_ready_ms: uiReady?.uiReadyMs ?? null,
    ui_ready_within_threshold:
      uiReady !== null && Number(uiReady.uiReadyMs) <= thresholds.startup.ui_ready_max_ms,
    idle_rss_bytes: rss?.total_bytes ?? null,
    idle_rss_mib: rss?.total_mib ?? null,
    idle_rss_processes: rss?.processes ?? [],
    daemon_status: daemonStatus,
    stderr: stderr.slice(0, 20),
  };

  // 优雅退出：WM_CLOSE → 等待退出预算（≤15s，硬预算 9.5s + 余量）。
  let closedGracefully = false;
  try {
    const close = closeMainWindow(child.pid);
    if (close?.closed) {
      await waitFor(() => result.exited !== null, 15_000, "应用优雅退出");
      closedGracefully = result.exited?.code === 0;
    }
  } catch (error) {
    console.warn(`[smoke] 关闭窗口失败：${error.message}`);
  }
  if (!closedGracefully && isPidAlive(child.pid)) {
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore" });
    await sleep(500);
  }
  // 优雅关闭预算属 M9-09（Windows 上 WM_CLOSE 可能由 Chromium 窗口处理，冒烟只断言
  // 进程树无孤儿；实际退出码记入证据）。
  record(
    "首启进程已退出（无孤儿）",
    !isPidAlive(child.pid),
    `graceful=${closedGracefully} exit=${JSON.stringify(result.exited)}`,
  );
  return result;
}

/** 安装产物内 daemon 的功能闭环（test 构建用 Mock；release 只断言注册清单）。 */
async function daemonFunctional({ daemon, dataDir, root }) {
  const rpc = new DaemonRpc({
    binary: daemon,
    env: {
      AETHER_DATA_DIR: dataDir,
      AETHER_DATA_LOCATION_FILE: path.join(root, "data-location.json"),
      USERPROFILE: root,
      HOME: root,
    },
    cwd: path.dirname(daemon),
  });
  const metrics = { runtime_ids: [] };
  try {
    const initialized = await rpc.request(
      "initialize",
      {
        protocolVersion: "1.0",
        appVersion: packageJson.version,
        client: "m9-11-install-smoke",
        clientCapabilities: ["events"],
        requiredCapabilities: ["events"],
      },
      30_000,
    );
    record("安装产物 daemon initialize（protocol=1.0）", initialized?.protocolVersion === "1.0", `protocol=${initialized?.protocolVersion}`);

    // initialize 回执早于存储/管线注入（M9-09 §2）：轮询 health 直到 ready（-32002 = 未就绪）。
    await waitFor(
      async () => {
        try {
          await rpc.request("health", {}, 5_000);
          return true;
        } catch (error) {
          if (error.code === -32002) return false;
          throw error;
        }
      },
      45_000,
      "安装产物 daemon 就绪（health 成功）",
      250,
    );

    const health = await rpc.request("health", {}, 30_000);
    record("安装产物 daemon health", health?.ok !== false, JSON.stringify(health)?.slice(0, 120));

    const runtimes = await rpc.request("runtimes_list", {}, 30_000);
    const ids = Array.isArray(runtimes) ? runtimes.map((entry) => entry.id) : [];
    metrics.runtime_ids = ids;
    const official = ["claude-code", "codex", "deepseek-harness"];
    record(
      "安装产物 daemon 注册清单含三官方运行时",
      official.every((id) => ids.includes(id)),
      ids.join(","),
    );

    // DoD-1：安装产物「备份可用」（默认主密钥模式；目标目录在冒烟工作区）。
    const backupTarget = path.join(root, "backup-target");
    mkdirSync(backupTarget, { recursive: true });
    const backup = await rpc.request(
      "backup_create",
      { label: "m9-11-install-smoke", target_dir: backupTarget },
      180_000,
    );
    const backupFiles = readdirSync(backupTarget).filter((name) => name.endsWith(".aebk"));
    record(
      "安装产物备份可用（backup_create → .aebk 落盘）",
      Boolean(backup?.backup) && backupFiles.length > 0,
      backupFiles[0] ?? JSON.stringify(backup)?.slice(0, 160),
    );

    if (variant === "test") {
      const session = await rpc.request("session_create", { runtime_id: "mock", title: "M9-11 install smoke" }, 30_000);
      const sessionId = session?.id ?? session?.session_id;
      record("测试构建：创建会话（mock）", typeof sessionId === "string", sessionId ?? JSON.stringify(session));
      if (sessionId) {
        const send = await rpc.request(
          "session_send",
          {
            session_id: sessionId,
            text: "long",
            client_msg_id: "01J8ZQ5R0N7W9Y8X6V4T2S0K1N",
          },
          30_000,
        );
        record("测试构建：发送消息（run.started）", Boolean(send?.run_id || send?.accepted), JSON.stringify(send)?.slice(0, 160));
        const started = await rpc.waitForNotification(isEvent("run.started"), 15_000, "run.started");
        record("测试构建：收到 run.started 事件", Boolean(started), JSON.stringify(started?.params)?.slice(0, 120));
        await sleep(300);
        const interrupt = await rpc.request("session_interrupt", { session_id: sessionId }, 30_000);
        record(
          "测试构建：中断会话（interrupted_run/cancelled_waiting_run）",
          Boolean(interrupt?.interrupted_run || interrupt?.cancelled_waiting_run),
          JSON.stringify(interrupt)?.slice(0, 200),
        );
        const cancelled = await rpc.waitForNotification(isEvent("run.cancelled"), 15_000, "run.cancelled");
        record("测试构建：收到 run.cancelled 事件", Boolean(cancelled), JSON.stringify(cancelled?.params)?.slice(0, 120));

        // 10 会话 RSS 上界：并发创建 10 个 mock 会话（不发送），采样 daemon 进程树。
        for (let index = 1; index <= 10; index += 1) {
          await rpc.request("session_create", { runtime_id: "mock", title: `M9-11 10x-${index}` }, 30_000);
        }
        const list = await rpc.request("session_list", {}, 30_000);
        const count = Array.isArray(list) ? list.length : 0;
        record("测试构建：10 会话可创建/列出", count >= 10, `sessions=${count}`);
        const rss = measureProcessTree(rpc.child.pid);
        metrics.daemon_10_sessions_rss_bytes = rss.total_bytes;
        metrics.daemon_10_sessions_rss_mib = rss.total_mib;
        record(
          "10 会话 daemon RSS 采样（硬阈值判定见 verify:m9-11）",
          rss.total_bytes > 0,
          `${rss.total_mib} MiB`,
        );
      }
    } else {
      record("发布形态：daemon 注册清单可读（无 Mock，功能闭环见 test 构建）", ids.length === 3, ids.join(","));
    }

    const exit = await rpc.shutdown(15_000);
    record("安装产物 daemon shutdown 干净退出", exit?.code === 0, `exit=${JSON.stringify(exit)}`);
  } finally {
    if (rpc.child.exitCode === null) rpc.kill();
  }
  evidence.metrics.daemon = metrics;
  return metrics;
}

async function main() {
  if (process.platform !== "win32") {
    console.error("[smoke] 本脚本的安装/卸载路径仅在 Windows 实现；macOS 见 verify:m9-11 与证据 §未验证。");
    process.exit(2);
  }
  const msi = findMsi();
  if (!msi || !existsSync(msi)) {
    record("找到安装产物 MSI", false, msi ?? "未找到（先 pnpm package:desktop）");
    return;
  }
  evidence.msi = { path: msi, bytes: statSync(msi).size, sha256: sha256(msi) };
  record("找到安装产物 MSI", true, `${path.basename(msi)} ${evidence.msi.bytes} bytes`);

  // DoD-7：体积记录（硬阈值判定统一在 `pnpm verify:m9-11`，避免与本冒烟的
  // DoD-4/DoD-8 安装/卸载/哈希语义耦合）。
  evidence.metrics.installer = {
    bytes: evidence.msi.bytes,
    max_bytes: thresholds.installer.max_bytes,
    size_pass: evidence.msi.bytes <= thresholds.installer.max_bytes,
  };
  record(
    "安装包体积记录（硬阈值判定见 verify:m9-11）",
    true,
    `${evidence.msi.bytes} / ${thresholds.installer.max_bytes} bytes`,
  );

  // DoD-8：哈希记录。
  const sumsName = variant === "test" ? "SHA256SUMS-test.txt" : "SHA256SUMS.txt";
  const sumsPath = path.join(path.dirname(msi), sumsName);
  const sumsOk = existsSync(sumsPath) && readFileSync(sumsPath, "utf8").includes(evidence.msi.sha256);
  record("产物哈希与 SHA256SUMS 一致", sumsOk, `${sumsName} sha256=${evidence.msi.sha256.slice(0, 16)}…`);

  // DoD-1/DoD-5：MSI 静态断言（升级链/数据目录）。
  const properties = readMsiProperties(msi);
  evidence.msi.properties = properties;
  record("MSI ProductName=Aether", properties.ProductName === "Aether", properties.ProductName ?? "");
  record(
    "MSI ProductVersion=应用版本",
    versionMatches(properties.ProductVersion, packageJson.version),
    `${properties.ProductVersion} vs ${packageJson.version}`,
  );
  record(
    "MSI UpgradeCode 为 GUID（新升级链）",
    /^\{[0-9A-Fa-f-]{36}\}$/.test(properties.UpgradeCode ?? ""),
    properties.UpgradeCode ?? "",
  );
  const upgrades = readMsiTable(msi, "Upgrade", [
    "UpgradeCode",
    "VersionMin",
    "VersionMax",
    "Language",
    "Attributes",
    "RemoveFeatures",
    "ActionProperty",
  ]);
  const foreignUpgrade = upgrades.filter(
    (row) => (row.UpgradeCode ?? "").toUpperCase() !== (properties.UpgradeCode ?? "").toUpperCase(),
  );
  record(
    "MSI 无旧 MSI 升级链（Upgrade 表仅本产品 UpgradeCode）",
    foreignUpgrade.length === 0,
    `rows=${upgrades.length} foreign=${foreignUpgrade.length}`,
  );
  const removeFiles = readMsiTable(msi, "RemoveFile", [
    "FileKey",
    "Component_",
    "FileName",
    "DirProperty",
    "InstallMode",
  ]);
  const dataDirDeletion = removeFiles.filter((row) =>
    /(aether\.db|appdata|roaming)/i.test(`${row.FileName ?? ""} ${row.DirProperty ?? ""}`),
  );
  record(
    "MSI RemoveFile 不删除数据目录（AppData/Roaming/Aether）",
    dataDirDeletion.length === 0,
    `rows=${removeFiles.length} hits=${dataDirDeletion.length}`,
  );

  // DoD-5：双安装预检。同一 ProductCode（上次冒烟残留）先静默卸载；其它 Aether
  // （旧 Tauri 包/上一候选构建）拒绝继续，要求手工卸载。
  let before = listInstalledAetherProducts();
  const sameProduct = before.filter(
    (item) => item.product_code?.toUpperCase() === (properties.ProductCode ?? "").toUpperCase(),
  );
  if (sameProduct.length > 0) {
    console.log(`[smoke] 检测到上次冒烟残留（${sameProduct[0].product_code}）：先静默卸载`);
    msiexec(["/x", sameProduct[0].product_code, "/qn", "/norestart"], "cleanup previous smoke install");
    before = listInstalledAetherProducts();
  }
  evidence.installed_before = before;
  record(
    "安装前无双安装（同类 Aether 产品不存在）",
    before.length === 0,
    before.map((item) => `${item.name} ${item.version} @ ${item.install_location}`).join("; ") ||
      "0（如存在旧 Tauri 包，请按 docs/M9-11-旧包手工卸载说明.md 手工卸载）",
  );

  const work = mkdtempSync(path.join(repoRoot, "dist-out", "m9-11-smoke-"));
  const installLog = path.join(work, "install.log");
  const uninstallLog = path.join(work, "uninstall.log");
  let installDir = null;
  let dataDir = null;

  try {
    // DoD-4：静默安装。
    const install = msiexec(["/i", msi, "/qn", "/norestart", "/l*v", installLog], "install");
    record("MSI 静默安装成功", install.code === 0 || install.code === 3010, `exit=${install.code}`);

    const installed = await waitFor(
      () => {
        const products = listInstalledAetherProducts();
        const product = products.find(
          (item) =>
            item.product_code?.toUpperCase() === (properties.ProductCode ?? "").toUpperCase() ||
            item.version === packageJson.version,
        );
        return product ?? null;
      },
      60_000,
      "MSI 安装注册",
    );
    evidence.installed_after = installed;
    const dirs = installDirCandidates(properties, [installed]);
    installDir = dirs[0] ?? null;
    record("定位安装目录（per-user，asInvoker）", installDir !== null, installDir ?? "未找到 Aether.exe");
    if (!installDir) return;

    const { daemon } = assertLayout(installDir);

    // DoD-4：首启 + 计时/RSS/Keychain。
    dataDir = path.join(work, "data");
    const userData = path.join(work, "user-data");
    await firstLaunch({ exe: path.join(installDir, "Aether.exe"), dataDir, userData, root: work });

    // DoD-4：安装产物 daemon 功能闭环（测试构建含 Mock）。
    const daemonData = path.join(work, "daemon-data");
    const daemonMetrics = await daemonFunctional({ daemon, dataDir: daemonData, root: work });
    if (daemonMetrics.daemon_10_sessions_rss_bytes && evidence.metrics.first_launch?.idle_rss_bytes) {
      // 保守上界：idle 全树（release 应用）+ 独立 daemon 10 会话树（含重复计入的 idle daemon）。
      const upper = evidence.metrics.first_launch.idle_rss_bytes + daemonMetrics.daemon_10_sessions_rss_bytes;
      evidence.metrics.ten_sessions_upper_bound_bytes = upper;
      evidence.metrics.ten_sessions_upper_bound_mib = Number((upper / 1048576).toFixed(2));
      evidence.metrics.ten_sessions_within_threshold = upper <= thresholds.runtime.ten_sessions_max_bytes;
      record(
        "10 会话保守上界记录（硬阈值 ≤1.2GiB；G6 判定见 verify:m9-11）",
        true,
        `${evidence.metrics.ten_sessions_upper_bound_mib} MiB${
          evidence.metrics.ten_sessions_within_threshold ? "" : "（超阈值）"
        }`,
      );
    } else {
      skip("10 会话 ≤1.2GiB", "release 冒烟未创建 10 会话（见 test 构建证据）");
    }
  } finally {
    // DoD-4：卸载与数据保留。
    if (!skipUninstall && installDir) {
      const uninstall = msiexec(["/x", msi, "/qn", "/norestart", "/l*v", uninstallLog], "uninstall");
      const removed =
        uninstall.code === 0 ||
        uninstall.code === 3010 ||
        uninstall.code === 1605 ||
        !existsSync(path.join(installDir, "Aether.exe"));
      record("MSI 静默卸载成功", removed, `exit=${uninstall.code}`);
      record(
        "卸载移除安装目录（Aether.exe 不存在）",
        !existsSync(path.join(installDir, "Aether.exe")),
        installDir,
      );
      if (dataDir) {
        record(
          "卸载保留用户数据目录与哨兵文件",
          existsSync(path.join(dataDir, "m9-11-sentinel.txt")) && existsSync(path.join(dataDir, "aether.db")),
          dataDir,
        );
      }
      const afterUninstall = listInstalledAetherProducts();
      record(
        "卸载后无残留 Aether 产品（无双安装）",
        afterUninstall.length === 0,
        afterUninstall.map((item) => item.product_code).join(",") || "0",
      );
    } else if (keepInstalled) {
      skip("卸载与数据保留", "--keep-installed");
    }
    if (!keepData && dataDir) {
      // 数据保留断言已执行；清理仅限本脚本临时数据（安装产物本身不受影响）。
      try {
        rmSync(dataDir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
      } catch {
        // 句柄滞后：保留临时目录，不影响断言。
      }
    }
    if (!keepInstalled && !keepData) {
      try {
        rmSync(work, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
      } catch {
        // 日志目录保留供排查。
      }
    } else {
      console.log(`[smoke] 保留临时目录：${work}`);
    }
  }
}

let exitCode = 1;
try {
  await main();
} catch (error) {
  record("安装冒烟执行", false, error?.stack ?? String(error));
} finally {
  evidence.finished_at = new Date().toISOString();
  evidence.passed = checks.every((check) => check.skip || check.exit === check.expect);
  const evidenceDir = path.join(repoRoot, "dist-out");
  mkdirSync(evidenceDir, { recursive: true });
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const evidencePath = path.join(evidenceDir, `m9-11-smoke-${variant}-${stamp}.json`);
  writeFileSync(evidencePath, `${JSON.stringify(evidence, null, 2)}\n`, "utf8");
  console.log(`[smoke] 证据：${path.relative(repoRoot, evidencePath)}`);
  exitCode = summarize("m9-11-installer-smoke", checks);
}
process.exit(exitCode);
