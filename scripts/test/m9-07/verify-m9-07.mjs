#!/usr/bin/env node
/**
 * `pnpm verify:m9-07`（P2 计划 M9-07；ADR-029 D-029-1/D-029-7、§3.5、§5.1）。
 *
 * 静态门禁（可执行；动态证据见 `docs/M9-07-证据.md`）：
 *  1. Electron 精确 45.0.0、`pnpm build:desktop` / `dev:desktop`、生产入口 main；
 *  2. 窗口 1280×800 / min 960×640；preload 五成员（无额外）；webPreferences 安全基线；
 *  3. CSP 生产/开发断言；导航/新窗口/webview/权限/DevTools 拦截；
 *  4. 启动顺序（窗口→daemon）、initialize 10s 硬超时 + 重试 1 次、S-12 六原因、
 *     `quick_check` degraded 不退出、无 S-01/S-01a/`startup_*` UI 入口；
 *  5. 4 条壳本地命令与 63/59/4 路由；路径 canonicalize/校验；
 *  6. UI-UX v0.17 S-12/S-13 锚点、数据目录/手动移动、锚点 22；证据文件。
 */
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const failures = [];
const checked = {
  files: 0,
  anchors: 0,
  reasons: 0,
  routes: 0,
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

function expect(condition, message) {
  if (!condition) {
    fail(message);
  }
  return condition;
}

function expectMatch(text, pattern, message) {
  if (!pattern.test(text)) {
    fail(message);
  }
}

function walkFiles(dir, extensions) {
  const base = path.join(root, dir);
  const found = [];
  const stack = [base];
  while (stack.length > 0) {
    const current = stack.pop();
    if (!current || !existsSync(current)) continue;
    for (const entry of readdirSync(current)) {
      const full = path.join(current, entry);
      const stats = statSync(full);
      if (stats.isDirectory()) {
        if (entry === "node_modules" || entry === "dist") continue;
        stack.push(full);
      } else if (extensions.some((ext) => entry.endsWith(ext))) {
        found.push(full);
      }
    }
  }
  return found;
}

// ===== 1. 依赖/构建脚本 =====
const rootPkg = readJson("package.json");
const appPkg = readJson("apps/desktop/package.json");
expect(
  appPkg.devDependencies?.electron === "45.0.0",
  `Electron 必须精确 45.0.0（实际 ${appPkg.devDependencies?.electron}）`,
);
expect(
  rootPkg.main === "apps/desktop/electron/dist/main.cjs",
  "根 package.json main 必须指向 electron/dist/main.cjs",
);
expect(
  rootPkg.scripts?.["dev:desktop"] === "node scripts/ci/dev-desktop.mjs",
  "缺少 dev:desktop",
);
expect(
  rootPkg.scripts?.["build:desktop"] === "pnpm --filter @aether/desktop build",
  "缺少 build:desktop",
);
expect(
  appPkg.scripts?.["build:electron"] === "node electron/build.mjs",
  "缺少 build:electron",
);
expect(
  appPkg.scripts?.build?.includes("build:renderer") &&
    appPkg.scripts?.build?.includes("build:electron"),
  "desktop build 必须构建 renderer + Electron main/preload",
);
const workspace = read("pnpm-workspace.yaml");
expect(/onlyBuiltDependencies:[\s\S]*electron/.test(workspace), "pnpm 未允许 electron postinstall");
const buildScript = read("apps/desktop/electron/build.mjs");
for (const output of ["main.cjs", "preload.cjs", "fatalPreload.cjs", "keychainPreload.cjs"]) {
  expect(buildScript.includes(output), `Electron 构建缺少 ${output}`);
}
expect(
  buildScript.includes("inlineDynamicImports: true"),
  "preload 必须自包含（禁止共享 chunk）",
);

// ===== 2. 文件/产物 =====
const electronFiles = [
  "apps/desktop/electron/app.ts",
  "apps/desktop/electron/main.ts",
  "apps/desktop/electron/preload.ts",
  "apps/desktop/electron/fatalPreload.ts",
  "apps/desktop/electron/keychainPreload.ts",
  "apps/desktop/electron/daemonClient.ts",
  "apps/desktop/electron/startup.ts",
  "apps/desktop/electron/shellLocal.ts",
  "apps/desktop/electron/security.ts",
  "apps/desktop/electron/dataDir.ts",
  "apps/desktop/electron/shellLog.ts",
  "apps/desktop/electron/offlineDiagnostics.ts",
  "apps/desktop/electron/redact.ts",
  "apps/desktop/electron/eventBridge.ts",
  "apps/desktop/electron/fatal/fatal.html",
  "apps/desktop/electron/fatal/fatal.js",
  "apps/desktop/electron/startup/startup.html",
  "apps/desktop/electron/keychain/keychain.html",
];
for (const file of electronFiles) {
  checked.files += 1;
  expect(exists(file), `缺少文件：${file}`);
}
for (const artifact of [
  "apps/desktop/electron/dist/main.cjs",
  "apps/desktop/electron/dist/preload.cjs",
  "apps/desktop/electron/dist/fatalPreload.cjs",
  "apps/desktop/electron/dist/keychainPreload.cjs",
  "apps/desktop/electron/dist/shell/fatal/fatal.html",
  "apps/desktop/electron/dist/shell/startup/startup.html",
]) {
  expect(exists(artifact), `缺少构建产物：${artifact}（先 pnpm build:desktop）`);
}

// 壳层不得 import @tauri-apps；主 bundle 不得包含 preload 的 contextBridge 执行。
for (const file of walkFiles("apps/desktop/electron", [".ts", ".tsx", ".js", ".mjs", ".cjs", ".html"])) {
  if (file.includes(`${path.sep}dist${path.sep}`)) continue;
  const text = readFileSync(file, "utf8");
  if (text.includes("@tauri-apps")) {
    fail(`Electron 壳不得依赖 @tauri-apps：${path.relative(root, file)}`);
  }
}
if (exists("apps/desktop/electron/dist/main.cjs")) {
  const mainBundle = read("apps/desktop/electron/dist/main.cjs");
  expect(!mainBundle.includes("exposeInMainWorld"), "main bundle 不得执行 preload 的 exposeInMainWorld");
  expect(!mainBundle.includes("@tauri-apps"), "main bundle 不得包含 @tauri-apps");
}
if (exists("apps/desktop/electron/dist/preload.cjs")) {
  const preloadBundle = read("apps/desktop/electron/dist/preload.cjs");
  const requires = [...preloadBundle.matchAll(/require\(["']([^"']+)["']\)/g)].map(
    (match) => match[1],
  );
  expect(
    requires.every((name) => name === "electron"),
    `preload 只能 require electron（实际：${requires.join(",")}）`,
  );
}

// ===== 3. 窗口/preload/CSP =====
const security = read("apps/desktop/electron/security.ts");
expectMatch(security, /WINDOW_WIDTH = 1_280/, "窗口宽度必须 1280");
expectMatch(security, /WINDOW_HEIGHT = 800/, "窗口高度必须 800");
expectMatch(security, /WINDOW_MIN_WIDTH = 960/, "窗口最小宽度必须 960");
expectMatch(security, /WINDOW_MIN_HEIGHT = 640/, "窗口最小高度必须 640");
expectMatch(security, /nodeIntegration: false/, "nodeIntegration 必须 false");
expectMatch(security, /contextIsolation: true/, "contextIsolation 必须 true");
expectMatch(security, /sandbox: true/, "sandbox 必须 true");
expectMatch(security, /webSecurity: true/, "webSecurity 必须 true");
expectMatch(
  security,
  /allowRunningInsecureContent: false/,
  "allowRunningInsecureContent 必须 false",
);
expectMatch(security, /webviewTag: false/, "webviewTag 必须 false");
expectMatch(security, /connect-src 'none'/, "生产 CSP 必须 connect-src 'none'");
expectMatch(security, /frame-src 'none'/, "生产 CSP 必须 frame-src 'none'");
expectMatch(security, /script-src 'self'/, "生产 CSP 必须 script-src 'self'");
expectMatch(security, /PRELOAD_API_KEYS = \[/, "缺少 preload 白名单");
for (const key of ["rpc", "onEvent", "offEvent", "pathForFile", "version"]) {
  expect(security.includes(`"${key}"`), `preload 白名单缺少 ${key}`);
}
checked.anchors += 5;

const preload = read("apps/desktop/electron/preload.ts");
expectMatch(preload, /exposeInMainWorld\("aether"/, "preload 必须暴露 window.aether");
expectMatch(preload, /webUtils\.getPathForFile/, "pathForFile 必须使用 webUtils.getPathForFile");
expectMatch(preload, /contextBridge/, "preload 必须使用 contextBridge");
expectMatch(preload, /EVENT_IPC_CHANNEL/, "preload 必须订阅 aether:event");
expect(!/\brequire\s*\(/.test(preload), "preload 源码不得直接 require 任意模块");
expect(
  !/ipcRenderer\s*:/.test(preload) &&
    !/exposeInMainWorld\(\s*["']ipcRenderer/.test(preload) &&
    !/window\.ipcRenderer/.test(preload),
  "preload 不得直接暴露 ipcRenderer",
);
expect(!/\bprocess\b/.test(preload.replace(/\/\*[\s\S]*?\*\//g, "")), "preload 不得暴露 process");
expectMatch(
  preload,
  /return \{ ok: false, error: makeError/,
  "rpc 错误必须返回可克隆信封（contextBridge 不保留自定义 Error 字段）",
);

const app = read("apps/desktop/electron/app.ts");
for (const [label, pattern] of [
  ["单实例锁", /requestSingleInstanceLock\(\)/],
  ["第二实例聚焦", /second-instance/],
  ["自定义协议注册", /registerSchemesAsPrivileged/],
  ["生产协议处理", /protocol\.handle\(APP_SCHEME/],
  ["CSP 响应头", /Content-Security-Policy/],
  ["新窗口拦截", /setWindowOpenHandler/],
  ["顶层导航拦截", /will-navigate/],
  ["webview 拦截", /will-attach-webview/],
  ["权限请求默认拒绝", /setPermissionRequestHandler/],
  ["权限检查默认拒绝", /setPermissionCheckHandler/],
  ["外链系统浏览器", /openExternal/],
  ["DevTools 仅 debug", /devToolsEnabled\(/],
  ["S-12 独立窗口", /showFatalWindow/],
  ["S-13 页面", /showKeychainPrompt/],
  ["离线诊断", /exportDiagnosticsFromFatal/],
  ["事件桥", /MainEventBridge/],
  ["daemon 启动", /runStartupSequence\(/],
]) {
  expectMatch(app, pattern, `app.ts 缺少${label}`);
}

// 启动顺序：createSecureWindow → startDaemonForWindow（同一函数内）
const createWindowBlock = app.slice(
  app.indexOf("async function createMainWindowAndStart"),
  app.indexOf("async function startDaemonForWindow"),
);
expect(
  createWindowBlock.indexOf("createSecureWindow(mainPreloadPath())") >= 0 &&
    createWindowBlock.indexOf("waitForWindowReady") >= 0 &&
    createWindowBlock.indexOf("startDaemonForWindow") > createWindowBlock.indexOf("createSecureWindow"),
  "启动顺序必须：创建窗口/S-00 → 窗口可见 → spawn daemon",
);
expectMatch(app, /AETHER_E2E === "1"/, "E2E 钩子必须仅在 AETHER_E2E=1 启用");
expectMatch(app, /phase === "failed" && runtime\.ready/, "daemon failed 阶段必须走壳级兜底");

// ===== 4. 启动/错误映射 =====
const startup = read("apps/desktop/electron/startup.ts");
expectMatch(startup, /attempt <= 2/, "启动最多 2 次尝试（超时重试 1 次）");
expectMatch(startup, /MIN_INITIALIZE_HARD_TIMEOUT_MS = INITIALIZE_HARD_TIMEOUT_MS/, "硬超时下限必须绑定 10s");
expectMatch(startup, /sync_dir_blocked: "a4_blocked"/, "A4 映射缺失");
expectMatch(startup, /data_dir_locked: "data_dir_locked"/, "data_dir_locked 映射缺失");
expectMatch(startup, /db_open_failed: "db_open_failed"/, "db_open_failed 映射缺失");
expectMatch(
  startup,
  /schema_checksum_mismatch: "schema_checksum_mismatch"/,
  "schema_checksum_mismatch 映射缺失",
);
expect(!/degraded.*showFatal|showFatal.*degraded/s.test(startup), "degraded 不得当致命");
const daemonClient = read("apps/desktop/electron/daemonClient.ts");
expectMatch(daemonClient, /INITIALIZE_HARD_TIMEOUT_MS = 10_000/, "initialize 硬超时必须 10s");
expectMatch(daemonClient, /MAX_FRAME_BYTES = 2 \* 1024 \* 1024/, "帧上限必须 2MiB");
expectMatch(daemonClient, /MAX_FRAME_BYTES/, "缺少帧上限校验");

const fatal = read("apps/desktop/electron/fatal/fatal.html");
const fatalJs = read("apps/desktop/electron/fatal/fatal.js");
const reasons = [
  "daemon_start_failed",
  "handshake_failed",
  "a4_blocked",
  "db_open_failed",
  "schema_checksum_mismatch",
  "data_dir_locked",
];
for (const reason of reasons) {
  checked.reasons += 1;
  expect(fatalJs.includes(reason), `S-12 缺少 data-reason：${reason}`);
}
const s12Anchors = [
  "shell-fatal",
  "shell-fatal-reason",
  "shell-fatal-log-path",
  "shell-fatal-data-dir",
  "shell-fatal-retry",
  "shell-fatal-exit",
  "shell-fatal-export-diagnostics",
  "shell-fatal-manual-move",
];
for (const anchor of s12Anchors) {
  checked.anchors += 1;
  expect(fatal.includes(anchor), `S-12 缺少锚点：${anchor}`);
}
expectMatch(fatal, /Content-Security-Policy/, "S-12 独立页必须有 CSP meta");
expect(!fatal.includes("http://") && !fatal.includes("https://"), "S-12 不得加载远程资源");
expectMatch(fatalJs, /aetherFatal/, "S-12 必须使用专用 preload（aetherFatal）");

const keychain = read("apps/desktop/electron/keychain/keychain.html");
for (const anchor of [
  "keychain-reauth",
  "keychain-reauth-reason",
  "keychain-reauth-retry",
  "keychain-reauth-passphrase",
  "keychain-reauth-skip",
  "keychain-reauth-notice",
]) {
  checked.anchors += 1;
  expect(keychain.includes(anchor), `S-13 缺少锚点：${anchor}`);
}
expectMatch(keychain, /data-scope="provider"/, "S-13 缺少 data-scope");
expectMatch(keychain, /data-mode="reauth"/, "S-13 缺少 data-mode");

// M9 不产生 S-01/S-01a / startup_*：main 不得加载或路由这些命令
expect(!/startup_get|startup_migrate|blocked_sync_dir|blocked_error/.test(app), "M9 main 不得产生 S-01/S-01a/startup_* 路径");
expect(!exists("apps/desktop/electron/startup-gate"), "M9 不得创建启动门页面");

// ===== 5. 壳本地命令/路由 =====
const shellLocal = read("apps/desktop/electron/shellLocal.ts");
for (const method of ["app_exit", "app_restart", "ref_pick", "startup_pick_target"]) {
  expect(shellLocal.includes(`"${method}"`), `壳本地命令缺少 ${method}`);
}
expectMatch(shellLocal, /realpathSync\.native/, "选择器返回路径必须 canonicalize");
expectMatch(shellLocal, /statSync/, "选择器返回路径必须校验文件/目录");
expectMatch(shellLocal, /confirm:true/, "退出/重启必须显式 confirm:true");
const manifest = readJson("packages/protocol/src/command-manifest.json");
checked.routes = manifest.commands.length;
expect(manifest.counts?.total === 63, "命令总数必须 63");
expect(manifest.counts?.daemon === 59, "daemon 路由必须 59");
expect(manifest.counts?.shell === 4, "shell 路由必须 4");
expect(manifest.counts?.ui === 61, "UI 暴露必须 61");
expect(manifest.counts?.reserved === 2, "保留命令必须 2");
for (const name of ["startup_get", "startup_migrate"]) {
  const command = manifest.commands.find((item) => item.name === name);
  expect(command?.exposure === "reserved", `${name} 必须保留不暴露`);
}
for (const name of ["app_exit", "app_restart", "ref_pick", "startup_pick_target"]) {
  const command = manifest.commands.find((item) => item.name === name);
  expect(command?.route === "shell", `${name} 必须走 shell 路由`);
}
expect(manifest.commands.filter((item) => item.route === "shell").length === 4, "shell 路由必须 4 条");

// ===== 6. 文档/证据/脚本 =====
expect(rootPkg.scripts?.["verify:m9-07"] === "node scripts/test/m9-07/verify-m9-07.mjs", "缺少 verify:m9-07");
expect(rootPkg.scripts?.["e2e:m9-07"] === "node scripts/test/m9-07/e2e-electron.mjs", "缺少 e2e:m9-07");
expect(rootPkg.scripts?.["e2e:m9-07:dev"] === "node scripts/test/m9-07/e2e-dev.mjs", "缺少 e2e:m9-07:dev");
expect(exists("scripts/test/m9-07/e2e-electron.mjs"), "缺少 E2E 脚本");
expect(exists("scripts/test/m9-07/e2e-dev.mjs"), "缺少 dev E2E 脚本");
expect(exists("scripts/test/m9-07/fixtures/fake-daemon.mjs"), "缺少假 daemon");
const electronLauncher = read("scripts/ci/electron-bin.mjs");
expect(
  electronLauncher.includes("Aether.exe") &&
    electronLauncher.includes("requestSingleInstanceLock"),
  "Electron 开发/E2E 启动器缺少 Aether.exe 单实例绕行",
);
expect(
  read("scripts/ci/dev-desktop.mjs").includes("resolveElectronExecutable") &&
    read("scripts/test/m9-07/e2e-electron.mjs").includes("resolveElectronExecutable"),
  "dev/E2E 必须使用 electron-bin 启动器",
);
expect(
  read("scripts/test/m9-07/e2e-electron.mjs").includes("dist-out") &&
    read("scripts/test/m9-07/e2e-dev.mjs").includes("AETHER_E2E_NO_SANDBOX"),
  "E2E userData 必须在工作区 dist-out，且受限沙箱显式 no-sandbox",
);
expect(exists("docs/M9-07-证据.md"), "缺少 docs/M9-07-证据.md");
const uiux = read("docs/UI-UX-设计规格.md");
expect(uiux.includes("设计规格 v0.17"), "UI-UX 必须 v0.17");
expect(uiux.includes("22"), "UI-UX 必须登记锚点 22");
expect(uiux.includes("S-12") && uiux.includes("S-13"), "UI-UX 缺少 S-12/S-13");
expect(uiux.includes("data_dir_locked") && uiux.includes("db_open_failed"), "UI-UX 缺少业务码");
const historyNotice = read("apps/desktop/src/HistoryOverflowNotice.tsx");
expect(
  historyNotice.includes("history-overflow") && historyNotice.includes("history-reload"),
  "预算续页必须复用 history-overflow/history-reload 锚点（不新增锚点组）",
);
const p2 = read("docs/P2-架构迁移-实施计划与验收标准.md");
expect(p2.includes("M9-07"), "P2 计划缺少 M9-07");

if (failures.length > 0) {
  console.error(`[m9-07] FAIL（${failures.length} 项）`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(
  `[m9-07] PASS 文件=${checked.files} 锚点=${checked.anchors} S-12 原因=${checked.reasons} 路由=${checked.routes}`,
);
console.log("AETHER_M9_07 PASS");
process.exit(0);
