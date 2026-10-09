/**
 * M4-05 安装产物冒烟：MSI 构建 → 哈希记录 → 管理式解包（近似干净环境）→
 * 首启自检 → 安装产物内置注册清单 → 会话闭环 + 内联权限回环。
 *
 * 两段式（对应「Mock 仅测试构建」口径）：
 *   A. **发布形态** MSI（三官方集合、无 Mock）：构建 → sha256 → `msiexec /a` 解包 →
 *      注册清单恰为三官方 → `--aether-diagnostics` 首启自检；
 *   B. **测试构建** MSI（`AETHER_BUNDLE_MOCK=1 --debug`，探针仅 debug 可达）：
 *      构建 → 解包 → 注册清单含 Mock → 真实 WebView 探针驱动会话闭环 + 内联权限回环。
 *
 * 诚实口径：真实「无 Node/无开发工具的干净虚拟机」在本机不可得；以 `msiexec /a`
 * （管理式安装，不改系统、不依赖开发工具链）解包产物再运行——应用与内置适配器
 * 均为自包含二进制，运行期不需要 Node/开发工具。残余风险（未覆盖真实干净 VM 的
 * 系统组件缺失场景）在 M4-05 证据中明示。
 *
 * 用法：node scripts/test/m4-05/installer-smoke.mjs [--skip-build] [--skip-debug]
 */
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { lineFramer, repoRoot, run, summarize } from "../lib/exec.mjs";
import { buildEnv } from "../m1-08/env.mjs";

const REPORT_LINE = "AETHER_M4_05_REPORT";
const EXIT_LINE = "AETHER_M4_05_EXIT";
const skipBuild = process.argv.includes("--skip-build");
const skipDebug = process.argv.includes("--skip-debug");

const tmpRoot = path.join(repoRoot, "scripts", "test", ".tmp", "m4-05");
const releaseOut = path.join(tmpRoot, "release-msi");
const debugOut = path.join(tmpRoot, "debug-msi");
const releaseExtract = path.join(tmpRoot, "install-extract-release");
const debugExtract = path.join(tmpRoot, "install-extract-debug");
const env = buildEnv();
const checks = [];
const record = (name, ok, detail) => {
  checks.push({ name: detail ? `${name}（${detail}）` : name, exit: ok ? 0 : 1, expect: 0 });
};

/** 整树回收（Windows `taskkill /T /F`；Unix 单杀回退）。 */
function killTree(child) {
  if (!child || child.exitCode !== null) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { stdio: "ignore" });
  } else {
    child.kill("SIGKILL");
  }
}

function findBinary(root) {
  const entries = readdirSync(root, { withFileTypes: true });
  for (const entry of entries) {
    const full = path.join(root, entry.name);
    if (entry.isDirectory()) {
      const found = findBinary(full);
      if (found) return found;
    } else if (entry.name === "aether-tauri.exe" || entry.name === "aether-tauri") {
      return full;
    }
  }
  return null;
}

function buildInstaller({ label, outDir, extraEnv, extraArgs }) {
  const exit = run(
    process.execPath,
    [
      path.join(repoRoot, "scripts", "ci", "build-desktop-installer.mjs"),
      "--out",
      outDir,
      ...(extraArgs ?? []),
    ],
    { env: { ...env, ...(extraEnv ?? {}) } },
  );
  if (exit !== 0) throw new Error(`${label} 安装器构建失败（exit=${exit}）`);
  const installers = readdirSync(outDir).filter((name) => name.endsWith(".msi"));
  if (installers.length === 0) throw new Error(`${label} 未找到 MSI：${outDir}`);
  return path.join(outDir, installers[0]);
}

function extractMsi(msi, targetDir) {
  rmSync(targetDir, { recursive: true, force: true });
  mkdirSync(targetDir, { recursive: true });
  const msiexec = path.join(process.env.SystemRoot ?? "C:\\Windows", "System32", "msiexec.exe");
  const extract = spawnSync(msiexec, ["/a", msi, "/qn", `TARGETDIR=${targetDir}`], {
    encoding: "utf8",
    timeout: 300_000,
  });
  if (extract.status !== 0) {
    throw new Error(`msiexec /a 解包失败（exit=${extract.status}）：${extract.stderr ?? ""}`);
  }
  const binary = findBinary(targetDir);
  if (!binary) throw new Error(`解包目录未找到 aether-tauri(.exe)：${targetDir}`);
  return binary;
}

function readRegistry(binary) {
  const manifestPath = path.join(path.dirname(binary), "runtime-bundle", "runtimes.json");
  if (!existsSync(manifestPath)) return null;
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  return { manifest_path: manifestPath, ids: (manifest.runtimes ?? []).map((entry) => entry.id) };
}

/** 清理 WebView2 用户数据目录（仅 Windows；best-effort）。 */
function resetWebviewData() {
  if (process.platform !== "win32") return;
  const base = process.env.LOCALAPPDATA;
  if (!base) return;
  const target = path.join(base, "dev.aether.desktop", "EBWebView");
  try {
    rmSync(target, { recursive: true, force: true });
    console.log(`[install] 已清理 WebView2 数据目录：${target}`);
  } catch (error) {
    console.error(`[install] 清理 WebView2 数据目录失败（忽略）：${error}`);
  }
}

/** 单次探针运行（真实 WebView2）。 */
async function attemptProbe({ binary, dataDir, locationFile, targetFile, timeoutMs }) {
  rmSync(dataDir, { recursive: true, force: true });
  mkdirSync(dataDir, { recursive: true });
  writeFileSync(targetFile, "M4-05 installer smoke target\n", "utf8");

  console.log(`\n$ ${binary}   # 安装产物内联回环冒烟`);
  const app = spawn(binary, [], {
    env: {
      ...env,
      AETHER_E2E_M4_05_PROBE: "1",
      AETHER_DATA_DIR: dataDir,
      AETHER_DATA_LOCATION_FILE: locationFile,
      AETHER_E2E_M4_05_TARGET: targetFile,
      AETHER_E2E_M4_05_RUNTIME: "mock",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });

  const lines = [];
  const stdoutDone = (async () => {
    const framer = lineFramer((line) => {
      lines.push(line);
      console.log(`[install] ${line}`);
    });
    for await (const chunk of app.stdout) framer.feed(chunk);
    framer.flush();
  })();
  const stderrDone = (async () => {
    for await (const chunk of app.stderr) {
      console.error(`[install:stderr] ${String(chunk).trimEnd()}`);
    }
  })();

  const exitCode = await new Promise((resolve) => {
    const timer = setTimeout(() => resolve(null), timeoutMs);
    app.once("exit", (code) => {
      clearTimeout(timer);
      resolve(code ?? -1);
    });
  });
  const timedOut = exitCode === null;
  if (timedOut) killTree(app);
  await Promise.all([stdoutDone, stderrDone]);

  const reports = lines
    .filter((line) => line.startsWith(REPORT_LINE))
    .map((line) => JSON.parse(line.slice(REPORT_LINE.length).trim()));
  const exitLine = lines.find((line) => line.startsWith(EXIT_LINE));
  return { reports, exitCode, exitLine, timedOut };
}

/** 运行安装产物探针（WebView2 初始化异常时最多重试一次；不改变断言）。 */
async function runProbe({ binary, dataDir, locationFile, targetFile, timeoutMs = 300_000 }) {
  let result = null;
  for (let attempt = 1; attempt <= 2; attempt += 1) {
    if (attempt > 1) {
      console.warn("[install] 上次探针未进入运行时选择（页面未回报），清理 WebView2 数据目录后重试一次");
      resetWebviewData();
    }
    result = await attemptProbe({ binary, dataDir, locationFile, targetFile, timeoutMs });
    if (result.reports.some((report) => report.stage === "runtime-selected")) return result;
    if (attempt < 2) {
      console.warn(
        `[install] 尝试 ${attempt}/2 未达运行时选择（timeout=${result.timedOut} exit=${result.exitCode}）`,
      );
    }
  }
  return result;
}

try {
  // ===== A. 发布形态 MSI（三官方、无 Mock）=====
  let releaseMsi;
  if (!skipBuild) {
    releaseMsi = buildInstaller({ label: "发布形态", outDir: releaseOut });
  } else {
    releaseMsi = path.join(
      releaseOut,
      readdirSync(releaseOut).find((name) => name.endsWith(".msi")) ?? "",
    );
  }
  if (!existsSync(releaseMsi)) throw new Error(`未找到发布形态 MSI：${releaseMsi}`);

  const msiHash = createHash("sha256").update(readFileSync(releaseMsi)).digest("hex");
  const sums = readFileSync(path.join(releaseOut, "SHA256SUMS.txt"), "utf8");
  record(
    "产物哈希记录（SHA256SUMS.txt 含 MSI 且与实测一致）",
    sums.includes(msiHash) && sums.includes(path.basename(releaseMsi)),
    `${path.basename(releaseMsi)} sha256=${msiHash.slice(0, 16)}…`,
  );

  const releaseBinary = extractMsi(releaseMsi, releaseExtract);
  record("发布形态安装产物解包并定位主程序", true, path.relative(releaseExtract, releaseBinary));

  const releaseRegistry = readRegistry(releaseBinary);
  const expectedOfficial = ["claude-code", "codex", "deepseek-harness"];
  const releaseRegistryOk =
    releaseRegistry &&
    releaseRegistry.ids.length === 3 &&
    expectedOfficial.every((id) => releaseRegistry.ids.includes(id)) &&
    !releaseRegistry.ids.includes("mock");
  record(
    "发布形态内置注册清单恰为三官方集合（无 Mock）",
    Boolean(releaseRegistryOk),
    releaseRegistry ? releaseRegistry.ids.join(",") : "缺少 runtimes.json",
  );

  const diagnostics = spawnSync(releaseBinary, ["--aether-diagnostics"], {
    encoding: "utf8",
    timeout: 60_000,
  });
  const diagnosticsOut = `${diagnostics.stdout ?? ""}${diagnostics.stderr ?? ""}`;
  record(
    "首启自检（--aether-diagnostics 输出版本与核心版本）",
    diagnostics.status === 0 && /Aether \d/.test(diagnosticsOut) && /core \d/.test(diagnosticsOut),
    diagnosticsOut.trim().split(/\r?\n/)[0] ?? "",
  );

  // ===== B. 调试测试构建 MSI（含 Mock；探针闭环）=====
  if (skipDebug) {
    record("安装产物内联回环冒烟（--skip-debug）", true, "skipped");
  } else {
    let debugMsi;
    if (!skipBuild) {
      debugMsi = buildInstaller({
        label: "测试构建（debug）",
        outDir: debugOut,
        extraEnv: { AETHER_BUNDLE_MOCK: "1" },
        extraArgs: ["--debug"],
      });
    } else {
      debugMsi = path.join(
        debugOut,
        readdirSync(debugOut).find((name) => name.endsWith(".msi")) ?? "",
      );
    }
    if (!existsSync(debugMsi)) throw new Error(`未找到调试测试 MSI：${debugMsi}`);

    const debugBinary = extractMsi(debugMsi, debugExtract);
    const debugRegistry = readRegistry(debugBinary);
    record(
      "测试构建内置注册清单含 Mock（并含三官方）",
      Boolean(
        debugRegistry &&
          debugRegistry.ids.includes("mock") &&
          expectedOfficial.every((id) => debugRegistry.ids.includes(id)),
      ),
      debugRegistry ? debugRegistry.ids.join(",") : "缺少 runtimes.json",
    );

    const dataDir = path.join(tmpRoot, "install-data");
    const locationFile = path.join(tmpRoot, "install-data-location.json");
    const targetFile = path.join(dataDir, "permission-target.txt");
    const { reports, exitCode, exitLine } = await runProbe({
      binary: debugBinary,
      dataDir,
      locationFile,
      targetFile,
    });
    const byStage = (stage) => reports.find((report) => report.stage === stage);
    const selected = byStage("runtime-selected");
    const ask = byStage("ask");
    const complete = byStage("complete");

    record(
      "安装产物启动并加载内置注册清单（运行时选择器含 mock）",
      Boolean(selected && (selected.available ?? []).includes("mock")),
      selected ? (selected.available ?? []).join(",") : "缺少 runtime-selected",
    );
    record(
      "会话闭环：发送 → 审批卡 → 允许 → 工具完成（安装产物 + 真实 WebView）",
      Boolean(
        byStage("sent") &&
          ask &&
          ask.card === true &&
          byStage("allowed") &&
          complete &&
          complete.tool_status === "completed",
      ),
    );
    record(
      "探针正常终止（terminal，exit=0）",
      exitCode === 0 && Boolean(exitLine && exitLine.includes('"terminal"')),
      `exit=${exitCode}`,
    );
  }
} catch (error) {
  record("安装产物冒烟执行", false, String(error && error.message ? error.message : error));
}

process.exit(summarize("m4-05-installer-smoke", checks));
