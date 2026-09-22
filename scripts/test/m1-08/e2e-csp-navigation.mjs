/**
 * M1-08 E2E：CSP 生效 + 外链导航拦截 + withGlobalTauri:false（DoD 1 / DoD 2）。
 *
 * 在真实 WebView（WebView2）中加载本地测试页 `csp-probe.html`：
 *   - 页面内联脚本、远程脚本、远程样式、远程图片、远程 iframe 均被 CSP 阻断；
 *   - 页面通过 IPC 回报观测结果与 CSP 违规明细；
 *   - 页面随后尝试一次外链导航，应由 on_navigation 拦截并转交（探针模式记录）；
 *   - beacon 服务器必须收到 0 个远程资源请求（双重证据）。
 *
 * 用法：node scripts/test/m1-08/e2e-csp-navigation.mjs [--skip-frontend-build] [--skip-rust-build]
 */
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, lineFramer, pnpmCommand, repoRoot, run, summarize } from "../lib/exec.mjs";
import { buildEnv } from "./env.mjs";

const args = process.argv.slice(2);
const skipFrontendBuild = args.includes("--skip-frontend-build");
const skipRustBuild = args.includes("--skip-rust-build");

const READY_LINE = "AETHER_E2E_PROBE_READY";
const REPORT_LINE = "AETHER_E2E_PROBE_REPORT";
const NAV_EXTERNAL_LINE = "AETHER_E2E_NAV_EXTERNAL";

const binaryName = process.platform === "win32" ? "aether-tauri.exe" : "aether-tauri";
const binary = path.join(repoRoot, "target", "debug", binaryName);
const frontendDist = path.join(repoRoot, "apps", "desktop", "dist");
const fixtures = path.join(repoRoot, "scripts", "test", "m1-08", "fixtures");

const env = buildEnv();
const checks = [];
const record = (name, ok, detail) => {
  checks.push({ name: detail ? `${name}（${detail}）` : name, exit: ok ? 0 : 1, expect: 0 });
};

// ---------------------------------------------------------------------------
// 1. beacon 服务器：记录全部请求（CSP 生效时应为 0）
// ---------------------------------------------------------------------------
const hits = [];
const server = createServer((request, response) => {
  const url = request.url ?? "";
  hits.push(url);
  const contentType = url.endsWith(".png")
    ? "image/png"
    : url.endsWith(".css")
      ? "text/css"
      : url.endsWith(".html")
        ? "text/html"
        : "application/javascript";
  response.writeHead(200, { "content-type": contentType });
  response.end(
    url.endsWith(".js")
      ? "window.__remoteExecuted = true; fetch('/remote-executed');"
      : "/* probe */",
  );
});

const port = await new Promise((resolve, reject) => {
  server.once("error", reject);
  server.listen(0, "127.0.0.1", () => resolve(server.address().port));
});
const beaconOrigin = `http://127.0.0.1:${port}`;
console.log(`[e2e] beacon 服务器：${beaconOrigin}`);

let app;
try {
  // -------------------------------------------------------------------------
  // 2. 前端构建 + 夹具注入（夹具体积小，仅为 E2E 生成，不进入版本库产物）
  // -------------------------------------------------------------------------
  if (!skipFrontendBuild) {
    const { command: pnpm, prefix } = pnpmCommand();
    const exit = run(pnpm, [...prefix, "--filter", "@aether/desktop", "build"], { env });
    if (exit !== 0) throw new Error(`前端构建失败（exit=${exit}）`);
  }
  if (!existsSync(path.join(frontendDist, "index.html"))) {
    throw new Error(`未找到前端产物 ${frontendDist}/index.html`);
  }

  mkdirSync(frontendDist, { recursive: true });
  const probeHtml = readFileSync(path.join(fixtures, "csp-probe.html"), "utf8").replaceAll(
    "{{BEACON}}",
    beaconOrigin,
  );
  writeFileSync(path.join(frontendDist, "csp-probe.html"), probeHtml);
  copyFileSync(
    path.join(fixtures, "csp-probe.js"),
    path.join(frontendDist, "csp-probe.js"),
  );

  // -------------------------------------------------------------------------
  // 3. 壳层构建（custom-protocol：加载内嵌 dist，按生产 CSP 生效）
  // -------------------------------------------------------------------------
  if (!skipRustBuild) {
    const exit = run(
      bin("cargo"),
      ["build", "-p", "aether-tauri", "--features", "custom-protocol"],
      { env },
    );
    if (exit !== 0) throw new Error(`cargo build 失败（exit=${exit}）`);
  }
  if (!existsSync(binary)) throw new Error(`未找到壳层产物 ${binary}`);

  // -------------------------------------------------------------------------
  // 4. 运行探针
  // -------------------------------------------------------------------------
  console.log(`\n$ ${binary}   # AETHER_E2E_CSP_PROBE=1`);
  app = spawn(binary, [], {
    env: { ...env, AETHER_E2E_CSP_PROBE: "1" },
    stdio: ["ignore", "pipe", "pipe"],
  });

  const lines = [];
  const stdoutDone = (async () => {
    // 行框定：跨 chunk 缓冲未闭合行（管道分片会截断 REPORT JSON → JSON.parse 崩溃）。
    const framer = lineFramer((line) => {
      lines.push(line);
      console.log(`[app] ${line}`);
    });
    for await (const chunk of app.stdout) framer.feed(chunk);
    framer.flush();
  })();
  const stderrDone = (async () => {
    for await (const chunk of app.stderr) {
      console.error(`[app:stderr] ${String(chunk).trimEnd()}`);
    }
  })();

  const exitCode = await new Promise((resolve) => {
    const timer = setTimeout(() => resolve(null), 45000);
    app.once("exit", (code) => {
      clearTimeout(timer);
      resolve(code ?? -1);
    });
  });
  if (exitCode === null) {
    app.kill();
    throw new Error("探针进程 45s 未退出（预期：回报 + 外链导航后自动退出）");
  }
  await Promise.all([stdoutDone, stderrDone]);

  // -------------------------------------------------------------------------
  // 5. 断言
  // -------------------------------------------------------------------------
  const ready = lines.some((line) => line.startsWith(READY_LINE));
  record("探针窗口就绪", ready);

  const reports = lines
    .filter((line) => line.startsWith(REPORT_LINE))
    .map((line) => JSON.parse(line.slice(REPORT_LINE.length).trim()));
  let report = reports.find((value) => typeof value.inlineExecuted === "boolean");
  if (!report) {
    // IPC 不可用时的兜底：报告随外链导航 URL 带出，导航被拦截时由宿主记录。
    const fallback = /report-nav\?payload=([^&\s]+)/.exec(
      lines.find((line) => line.includes("report-nav?payload=")) ?? "",
    );
    if (fallback) {
      try {
        report = JSON.parse(decodeURIComponent(fallback[1]));
      } catch {
        report = null;
      }
    }
  }
  const uiBootstrapped = reports.some((value) => value.event === "ui-bootstrapped");
  record(
    "应用主界面在加固 CSP 下成功引导（React 挂载，应用资源未被误伤）",
    uiBootstrapped,
  );
  if (!report) {
    record("收到 CSP 探针回报", false, "缺少 AETHER_E2E_PROBE_REPORT 行");
  } else {
    record("收到 CSP 探针回报", true);
    record("CSP 阻断内联脚本（inlineExecuted=false）", report.inlineExecuted === false, JSON.stringify(report.inlineExecuted));
    record("CSP 阻断远程脚本（remoteExecuted=false）", report.remoteExecuted === false, JSON.stringify(report.remoteExecuted));
    record("withGlobalTauri:false（window.__TAURI__ undefined）", report.globalTauri === false);
    record("IPC 内部通道可用（报告经 e2e_probe_report 直达宿主）", report.internalsAvailable === true);

    const directives = (report.violations ?? []).map((violation) => violation.directive ?? "");
    const has = (prefix) => directives.some((directive) => directive.startsWith(prefix));
    record(
      "CSP 违规明细覆盖 script-src",
      has("script-src"),
      directives.join(","),
    );
    record("CSP 违规明细覆盖 img-src", has("img-src"), directives.join(","));
    record("CSP 违规明细覆盖 frame-src", has("frame-src"), directives.join(","));
    record("CSP 违规明细覆盖 style-src", has("style-src"), directives.join(","));
  }

  const externalNav = lines.find((line) =>
    line.startsWith(`${NAV_EXTERNAL_LINE} https://aether-csp-probe.invalid/`),
  );
  record("外链导航被拦截并记录转交系统浏览器", Boolean(externalNav), externalNav ?? lines.filter((l) => l.startsWith("AETHER_E2E_NAV")).join(" | "));

  record("探针进程退出码为 0", exitCode === 0, `实际 ${exitCode}`);
  record(
    "beacon 服务器收到 0 个远程资源请求（CSP 双重证据）",
    hits.length === 0,
    hits.length === 0 ? "0" : hits.join(","),
  );
} catch (error) {
  record("E2E 执行", false, String(error && error.message ? error.message : error));
  if (app && app.exitCode === null) app.kill();
} finally {
  await new Promise((resolve) => server.close(resolve));
  for (const file of ["csp-probe.html", "csp-probe.js"]) {
    rmSync(path.join(frontendDist, file), { force: true });
  }
}

process.exit(summarize("m1-08-csp-navigation-e2e", checks));
