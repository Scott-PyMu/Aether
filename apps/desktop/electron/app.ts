/**
 * Electron main（M9-07；ADR-029 D-029-1/D-029-7、§3.5、§5.1）。
 *
 * 职责（薄壳）：
 * - 单实例锁；窗口（1280×800 / min 960×640）；先窗口/启动页后 daemon；
 * - spawn `aetherd --stdio`、`initialize` 握手（10s 硬超时、超时重试 1 次）；
 * - `aether:rpc`：4 条壳本地命令本地处理，其余 59 条转发 daemon；
 * - 事件桥（M9-05）接到 daemon `aether.event`；`aether:event` 单通道；
 * - S-12 独立最小 renderer（fatal window/preload），离线导出诊断；
 * - CSP/导航/权限/DevTools 基线；外链交系统浏览器。
 *
 * 不在本任务：完整关闭预算/崩溃退避/daemon 单实例锁/Keychain preflight
 * （M9-09）、`@electron/fuses`/WebContentsView（M9-10）、打包（M9-11）。
 */
import { spawn } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";

import {
  BrowserWindow,
  app,
  dialog,
  ipcMain,
  Menu,
  protocol,
  session,
  shell,
  type IpcMainInvokeEvent,
  type WebContents,
} from "electron";

import type { AetherEvent } from "@aether/protocol";

import commandManifest from "../../../packages/protocol/src/command-manifest.json";
import { BrowserHost, type BrowserBounds } from "./browserHost";
import { BROWSER_PARTITION_PREFIX, isBrowserContents } from "./browserView";
import { DaemonClient, AetherRpcError, type DaemonNotification } from "./daemonClient";
import { DaemonRestartPolicy } from "./daemonLifecycle";
import {
  DAEMON_SHUTDOWN_BUDGET_MS,
  MAIN_EXIT_HARD_BUDGET_MS,
  T11_BUDGET_MS,
  daemonSpawnOptions,
  killDaemonTree,
} from "./daemonTree";
import {
  daemonLogPathFor,
  resolveDataDir,
  shellLogPathFor,
} from "./dataDir";
import {
  RUNTIME_BUNDLE_ENV,
  daemonArguments,
  initializeParams,
  resolveDaemonBinary,
} from "./daemonPaths";
import { MainEventBridge } from "./eventBridge";
import {
  FATAL_EXIT_CHANNEL,
  FATAL_EXPORT_CHANNEL,
  FATAL_RETRY_CHANNEL,
  FATAL_STATE_CHANNEL,
  KEYCHAIN_PASSPHRASE_CHANNEL,
  KEYCHAIN_RETRY_CHANNEL,
  KEYCHAIN_SKIP_CHANNEL,
  KEYCHAIN_STATE_CHANNEL,
  type KeychainActionResult,
  type KeychainViewState,
} from "./shellPageChannels";
import {
  DIAGNOSTICS_LOG_LINES,
  MANUAL_DATA_DIR_INSTRUCTIONS,
  buildOfflineDiagnostics,
  defaultDiagnosticsFileName,
  writeOfflineDiagnostics,
} from "./offlineDiagnostics";
import { createShellLogger, readFileTail, type ShellLogger } from "./shellLog";
import {
  APP_SCHEME,
  APP_URL,
  DEV_CSP,
  EVENT_IPC_CHANNEL,
  PRELOAD_API_KEYS,
  PRODUCTION_CSP,
  RPC_IPC_CHANNEL,
  VERSION_IPC_CHANNEL,
  WINDOW_HEIGHT,
  WINDOW_MIN_HEIGHT,
  WINDOW_MIN_WIDTH,
  WINDOW_WIDTH,
  contentTypeFor,
  decideNavigation,
  decideWindowOpen,
  devToolsAutoOpen,
  devToolsEnabled,
  rendererDistDir,
  resolveAppAsset,
  secureWebPreferences,
} from "./security";
import {
  handleShellLocalCommand,
  isShellLocalCommand,
  ShellLocalError,
  type RefPickKind,
} from "./shellLocal";
import { guardRpcRequest } from "./rpcGuard";
import { runStartupSequence, type StartupFailure } from "./startup";

/** 主 UI 命令白名单（63 条；main 第一道，daemon 最终权威）。 */
const COMMAND_NAMES: ReadonlySet<string> = new Set(
  commandManifest.commands.map((command) => command.name),
);
/** 单次离线诊断日志行数。 */
const DIAGNOSTICS_LINES = DIAGNOSTICS_LOG_LINES;

interface RuntimeVersions {
  daemon?: string;
  core?: string;
}

interface ShellRuntime {
  startedAt: number;
  dataDir: string;
  dataDirError?: string;
  logger: ShellLogger;
  mainWindow: BrowserWindow | null;
  fatalWindow: BrowserWindow | null;
  keychainWindow: BrowserWindow | null;
  client: DaemonClient | null;
  bridge: MainEventBridge | null;
  browserHost: BrowserHost | null;
  ready: boolean;
  starting: boolean;
  fatal: StartupFailure | null;
  phase: string | null;
  versions: RuntimeVersions;
  windowCreatedAt: number;
  windowReadyAt: number | null;
  uiReadyAt: number | null;
  sessionCreateMs: number | null;
  daemonSpawnedAt: number | null;
  daemonSpawnCount: number;
  daemonEverReady: boolean;
  secondInstanceCount: number;
  initializeMs: number | null;
  withinTarget: boolean | null;
  retried: boolean;
  lastStderr: string;
  externalOpens: string[];
  devServerUrl: string | null;
  quitting: boolean;
  suppressAllClosed: boolean;
  keychainState: KeychainViewState;
  keychainPrompted: boolean;
  secretsPassphrase: string | null;
  restartPolicy: DaemonRestartPolicy;
  restartTimer: ReturnType<typeof setTimeout> | null;
  restartScheduledAt: number | null;
  shutdownStartedAt: number | null;
  shutdownElapsedMs: number | null;
}

interface KeychainHooks {
  retry(): Promise<KeychainActionResult>;
  submitPassphrase(passphrase: string): Promise<KeychainActionResult>;
  skip(): Promise<KeychainActionResult>;
}

let keychainHooks: KeychainHooks | null = null;
let concurrentRpc = 0;
const hardenedContents = new WeakSet<WebContents>();

const initialDataDir = resolveDataDir();
const runtime: ShellRuntime = {
  startedAt: Date.now(),
  dataDir: initialDataDir.dataDir,
  ...(initialDataDir.error ? { dataDirError: initialDataDir.error } : {}),
  logger: createShellLogger({ dataDir: initialDataDir.dataDir }),
  mainWindow: null,
  fatalWindow: null,
  keychainWindow: null,
  client: null,
  bridge: null,
  browserHost: null,
  ready: false,
  starting: false,
  fatal: null,
  phase: null,
  versions: {},
  windowCreatedAt: 0,
  windowReadyAt: null,
  uiReadyAt: null,
  sessionCreateMs: null,
  daemonSpawnedAt: null,
  daemonSpawnCount: 0,
  daemonEverReady: false,
  secondInstanceCount: 0,
  initializeMs: null,
  withinTarget: null,
  retried: false,
  lastStderr: "",
  externalOpens: [],
  devServerUrl: devServerUrlFromEnv(),
  quitting: false,
  suppressAllClosed: false,
  keychainState: {
    scope: "provider",
    mode: "reauth",
    reason: "旧 Keychain 条目需要重新授权（M9-09 preflight）。",
    notice: "",
  },
  keychainPrompted: false,
  secretsPassphrase: null,
  restartPolicy: new DaemonRestartPolicy(),
  restartTimer: null,
  restartScheduledAt: null,
  shutdownStartedAt: null,
  shutdownElapsedMs: null,
};

/** 主 UI preload 白名单（E2E/静态校验共享；防止新增成员）。 */
export const MAIN_PRELOAD_KEYS = PRELOAD_API_KEYS;

// 自定义协议必须在 app ready 前注册为特权 scheme（CSP/secure/fetch 语义）。
protocol.registerSchemesAsPrivileged([
  {
    scheme: APP_SCHEME,
    privileges: {
      standard: true,
      secure: true,
      supportFetchAPI: true,
      corsEnabled: false,
      stream: true,
    },
  },
]);

// 单实例：`userData` 会影响锁作用域；必须在 requestSingleInstanceLock 之前设置。
const userDataOverride = process.env.AETHER_USER_DATA_DIR;
if (typeof userDataOverride === "string" && userDataOverride.trim().length > 0) {
  app.setPath("userData", path.resolve(userDataOverride));
}

const gotSingleInstanceLock = app.requestSingleInstanceLock();
if (!gotSingleInstanceLock) {
  // 第二实例：不创建窗口、不 spawn daemon；立即退出（first instance 已聚焦）。
  app.quit();
} else {
  installSingleInstanceFocus();
  installLifecycle();
  void app.whenReady().then(onReady).catch((error: unknown) => {
    runtime.logger.log("error", "app.ready.failed", { error: errorText(error) });
    app.exit(1);
  });
}

/** 第二实例聚焦主窗口（S-12/Keychain 页存在时聚焦该页）。 */
function installSingleInstanceFocus(): void {
  app.on("second-instance", () => {
    runtime.secondInstanceCount += 1;
    runtime.logger.log("info", "app.second_instance", {
      count: runtime.secondInstanceCount,
      daemonPid: runtime.client?.pid ?? null,
    });
    const win = activeWindow();
    if (win === null) {
      return;
    }
    if (win.isMinimized()) {
      win.restore();
    }
    win.show();
    win.focus();
  });
}

/** 退出/窗口关闭/崩溃日志。 */
function installLifecycle(): void {
  app.on("window-all-closed", () => {
    if (runtime.suppressAllClosed || runtime.quitting) {
      return;
    }
    app.quit();
  });

  app.on("before-quit", (event) => {
    if (runtime.quitting) {
      return;
    }
    clearRestartTimer();
    runtime.browserHost?.dispose();
    const client = runtime.client;
    if (client === null) {
      return;
    }
    event.preventDefault();
    runtime.quitting = true;
    runtime.shutdownStartedAt = Date.now();
    runtime.logger.log("info", "app.shutdown.start", {
      daemonPid: client.pid,
      gracefulBudgetMs: DAEMON_SHUTDOWN_BUDGET_MS,
      hardBudgetMs: MAIN_EXIT_HARD_BUDGET_MS,
      t11BudgetMs: T11_BUDGET_MS,
    });
    void (async () => {
      const started = Date.now();
      const exit = await client.shutdown(DAEMON_SHUTDOWN_BUDGET_MS);
      if (exit === null) {
        const elapsed = Date.now() - started;
        const remaining = Math.max(0, MAIN_EXIT_HARD_BUDGET_MS - elapsed);
        runtime.logger.log("warn", "app.shutdown.force", {
          daemonPid: client.pid,
          elapsedMs: elapsed,
          remainingMs: remaining,
        });
        const killed = await killDaemonTree({
          pid: client.pid ?? 0,
          timeoutMs: remaining,
        });
        runtime.logger.log(killed.killed ? "warn" : "error", "app.shutdown.tree_kill", {
          daemonPid: client.pid,
          ...killed,
        });
        const untilT11 = Math.max(0, T11_BUDGET_MS - (Date.now() - started));
        await client.waitForExit(untilT11);
      }
      runtime.client = null;
      runtime.bridge?.dispose();
      runtime.bridge = null;
      runtime.shutdownElapsedMs = Date.now() - started;
      const withinT11 = runtime.shutdownElapsedMs <= T11_BUDGET_MS;
      runtime.logger.log(withinT11 ? "info" : "error", "app.shutdown.done", {
        exit,
        elapsedMs: runtime.shutdownElapsedMs,
        t11BudgetMs: T11_BUDGET_MS,
        withinT11,
      });
      app.quit();
    })();
  });

  app.on("render-process-gone", (_event, contents, details) => {
    runtime.logger.log("error", "renderer.gone", {
      reason: details.reason,
      exitCode: details.exitCode,
      url: safeUrl(contents),
    });
  });

  app.on("child-process-gone", (_event, details) => {
    runtime.logger.log("warn", "child_process.gone", {
      type: details.type,
      reason: details.reason,
      exitCode: details.exitCode,
    });
  });
}

async function onReady(): Promise<void> {
  Menu.setApplicationMenu(null);
  registerAppProtocol();
  installSessionSecurity();
  installWebContentsHardening();
  installCertificatePolicy();
  installBrowserHost();
  installIpcHandlers();
  installKeychainHooksOnce();
  installE2EHook();
  runtime.logger.log("info", "app.start", {
    version: app.getVersion(),
    electron: process.versions.electron,
    chrome: process.versions.chrome,
    node: process.versions.node,
    platform: process.platform,
    arch: process.arch,
    dataDir: runtime.dataDir,
    dataDirSource: initialDataDir.source,
    dataDirError: runtime.dataDirError ?? null,
    devServerUrl: runtime.devServerUrl,
  });
  await createMainWindowAndStart();
}

/** 生产 renderer 协议：`aether://app/<dist 内文件>`，响应头带 CSP。 */
function registerAppProtocol(): void {
  const distDir = rendererDistDir(app.getAppPath());
  protocol.handle(APP_SCHEME, async (request) => {
    const file = resolveAppAsset(distDir, request.url);
    if (file === null || !existsSync(file)) {
      return new Response("Not Found", {
        status: 404,
        headers: {
          "Content-Type": "text/plain; charset=utf-8",
          "Content-Security-Policy": PRODUCTION_CSP,
        },
      });
    }
    try {
      const body = readFileSync(file);
      return new Response(new Uint8Array(body), {
        status: 200,
        headers: {
          "Content-Type": contentTypeFor(file),
          "Content-Security-Policy": PRODUCTION_CSP,
          "X-Content-Type-Options": "nosniff",
        },
      });
    } catch (error) {
      runtime.logger.log("error", "app.protocol.read_failed", {
        file,
        error: errorText(error),
      });
      return new Response("Internal Error", {
        status: 500,
        headers: {
          "Content-Type": "text/plain; charset=utf-8",
          "Content-Security-Policy": PRODUCTION_CSP,
        },
      });
    }
  });
}

/** 权限默认拒绝 + 开发 CSP（生产 CSP 由自定义协议响应头下发）。 */
function installSessionSecurity(): void {
  const defaultSession = session.defaultSession;
  defaultSession.setPermissionRequestHandler((_contents, _permission, callback) => {
    callback(false);
  });
  defaultSession.setPermissionCheckHandler(() => false);
  defaultSession.setCertificateVerifyProc((_request, callback) => {
    // -3 = 使用 Chromium 默认校验（证书错误默认拒绝；不提供「继续访问」）。
    callback(-3);
  });
  defaultSession.webRequest.onHeadersReceived((details, callback) => {
    if (
      runtime.devServerUrl !== null &&
      details.url.startsWith(runtime.devServerUrl)
    ) {
      callback({
        responseHeaders: {
          ...details.responseHeaders,
          "Content-Security-Policy": [DEV_CSP],
        },
      });
      return;
    }
    callback({});
  });
}

/** 所有新建 WebContents 统一加固（ADR-029 §5.1）。 */
function installWebContentsHardening(): void {
  app.on("web-contents-created", (_event, contents) => {
    // `WebContentsView` 的创建事件同步触发；延迟到下一轮事件循环，等
    // `BrowserHost` 完成标记后再决定是否套用主 UI 规则。
    setImmediate(() => {
      if (contents.isDestroyed()) {
        return;
      }
      if (!isBrowserContents(contents)) {
        hardenContents(contents);
      }
    });
  });
}

/** 证书错误显式拒绝（浏览器视图/任何 contents 均不提供继续访问后门）。 */
function installCertificatePolicy(): void {
  app.on("certificate-error", (event, contents, url, error, _certificate, callback) => {
    event.preventDefault();
    runtime.logger.log("warn", "certificate.rejected", {
      url,
      error,
      type: contents.getType(),
    });
    callback(false);
  });
}

/** `WebContentsView` PoC 宿主（M9-10；完整 UI-06 属 P4）。 */
function installBrowserHost(): void {
  const downloadsDir =
    process.env.AETHER_E2E === "1" &&
    typeof process.env.AETHER_E2E_DOWNLOADS_DIR === "string"
      ? path.resolve(process.env.AETHER_E2E_DOWNLOADS_DIR)
      : app.getPath("downloads");
  runtime.browserHost = new BrowserHost({
    logger: runtime.logger,
    downloadsDir,
    getParent: () => runtime.mainWindow,
    openExternal,
  });
  runtime.logger.log("info", "browser.host.installed", { downloadsDir });
}

/** 创建主窗口（S-00 启动页）→ 窗口可见 → spawn daemon → initialize。 */
async function createMainWindowAndStart(): Promise<void> {
  if (runtime.starting) {
    return;
  }
  runtime.starting = true;
  runtime.fatal = null;
  runtime.ready = false;
  runtime.windowReadyAt = null;
  try {
    runtime.suppressAllClosed = true;
    const win = createSecureWindow(mainPreloadPath());
    runtime.mainWindow = win;
    runtime.windowCreatedAt = Date.now();
    runtime.logger.log("info", "window.created", {
      width: WINDOW_WIDTH,
      height: WINDOW_HEIGHT,
      minWidth: WINDOW_MIN_WIDTH,
      minHeight: WINDOW_MIN_HEIGHT,
    });
    hardenContents(win.webContents);
    const ready = waitForWindowReady(win);
    const loaded = win.loadFile(shellAsset("startup", "startup.html"));
    // 提前挂接拒绝处理，避免 ready 先完成时出现 unhandled rejection；后续仍会 await。
    void loaded.catch(() => {});
    // 窗口可见预算 = 进程启动 → `ready-to-show`/1.5s 兜底显示；页面加载完成不推迟
    // 可见计时（DoD-1 口径），但 daemon 仍在启动页加载完成后才 spawn。
    await ready;
    runtime.windowReadyAt = Date.now();
    runtime.logger.log("info", "window.ready", {
      visibleMs: runtime.windowReadyAt - runtime.startedAt,
    });
    await loaded;
    runtime.suppressAllClosed = false;
    await startDaemonForWindow(win, { loadRenderer: true });
  } catch (error) {
    runtime.logger.log("error", "startup.window_failed", { error: errorText(error) });
    runtime.suppressAllClosed = false;
    showFatalWindow({
      ok: false,
      client: null,
      fatal: {
        reason: "daemon_start_failed",
        message: `启动失败：${errorText(error)}`,
        logPath: shellLogPathFor(runtime.dataDir),
        dataDir: runtime.dataDir,
        attempts: 0,
      },
      attempts: [],
      retried: false,
      exit: null,
    });
  } finally {
    runtime.starting = false;
  }
}

async function startDaemonForWindow(
  win: BrowserWindow,
  options: { loadRenderer: boolean; restart?: boolean } = { loadRenderer: true },
): Promise<void> {
  const resolution = resolveDataDir();
  if (resolution.dataDir !== runtime.dataDir) {
    runtime.dataDir = resolution.dataDir;
    runtime.logger = createShellLogger({ dataDir: resolution.dataDir });
  }
  runtime.dataDirError = resolution.error;
  const logPath = daemonLogPathFor(runtime.dataDir);
  const binary = resolveDaemonBinary({
    appPath: app.getAppPath(),
    resourcesPath: process.resourcesPath,
    isPackaged: app.isPackaged,
  });

  runtime.logger.log("info", "daemon.spawn.prepare", {
    binary,
    dataDir: runtime.dataDir,
    dataDirSource: resolution.source,
    runtimeBundle: process.env[RUNTIME_BUNDLE_ENV] ?? null,
    restart: options.restart === true,
  });

  const outcome = await runStartupSequence({
    windowCreatedAt: runtime.windowCreatedAt,
    context: { logPath, dataDir: runtime.dataDir },
    initializeParams: initializeParams(app.getVersion()),
    createClient: (attempt) => createDaemonClient(binary, attempt),
    onAttemptEnd: (trace) => {
      runtime.logger.log(trace.outcome === "ready" ? "info" : "error", "startup.attempt", {
        ...trace,
      });
    },
  });

  if (outcome.ok) {
    runtime.client = outcome.client as DaemonClient;
    runtime.ready = true;
    runtime.daemonEverReady = true;
    runtime.initializeMs = outcome.initializeMs;
    runtime.withinTarget = outcome.withinTarget;
    runtime.retried = outcome.retried;
    runtime.phase = outcome.result.phase;
    runtime.versions = {
      daemon: outcome.result.daemonVersion,
      core: outcome.result.coreVersion,
    };
    runtime.dataDir = outcome.result.dataDir || runtime.dataDir;
    runtime.restartPolicy.markReady();
    runtime.restartScheduledAt = null;
    runtime.logger.log("info", options.restart ? "daemon.restart.ready" : "startup.ready", {
      attempts: outcome.attempts.length,
      initializeMs: outcome.initializeMs,
      withinTarget: outcome.withinTarget,
      retried: outcome.retried,
      daemonVersion: outcome.result.daemonVersion,
      coreVersion: outcome.result.coreVersion,
      dataDir: outcome.result.dataDir,
      daemonPid: runtime.client.pid,
      restartAttempt: runtime.restartPolicy.snapshot().attempt,
    });
    installEventBridge();
    if (options.loadRenderer) {
      await loadMainRenderer(win);
    }
    return;
  }

  if (options.restart === true) {
    // 重启失败：保留窗口/主 UI，按退避继续重试；不切 S-12（S-12 只用于首次启动失败）。
    runtime.ready = false;
    runtime.lastStderr = outcome.client?.stderrTail ?? runtime.lastStderr;
    outcome.client?.kill("SIGTERM");
    runtime.logger.log("error", "daemon.restart.failed", {
      reason: outcome.fatal.reason,
      message: outcome.fatal.message,
      attempts: outcome.attempts.length,
      retried: outcome.retried,
    });
    scheduleDaemonRestartAfterFailure();
    return;
  }

  runtime.ready = false;
  runtime.fatal = outcome;
  runtime.lastStderr = outcome.client?.stderrTail ?? runtime.lastStderr;
  const daemonLogPath =
    typeof outcome.client?.bootstrapFailure?.data?.log_path === "string"
      ? outcome.client.bootstrapFailure.data.log_path
      : logPath;
  outcome.fatal = { ...outcome.fatal, logPath: daemonLogPath };
  runtime.logger.log("error", "startup.fatal", {
    reason: outcome.fatal.reason,
    message: outcome.fatal.message,
    logPath: daemonLogPath,
    attempts: outcome.attempts.length,
    retried: outcome.retried,
  });
  outcome.client?.kill("SIGTERM");
  showFatalWindow(outcome);
}

/** 构造一次 daemon 客户端（spawn + 通知/退出/崩溃恢复接线）。 */
function createDaemonClient(binary: string, attempt: number): DaemonClient {
  runtime.daemonSpawnCount += 1;
  if (runtime.daemonSpawnedAt === null) {
    runtime.daemonSpawnedAt = Date.now();
  }
  let captured: DaemonClient | null = null;
  const env = daemonEnv();
  const client = new DaemonClient({
    spawn: (command, args, options) =>
      spawn(command, [...args], {
        ...options,
        ...daemonSpawnOptions(),
        stdio: ["pipe", "pipe", "pipe"],
      }),
    spawnSpec: {
      command: binary,
      args: daemonArguments(env),
      env,
      cwd: app.getAppPath(),
    },
    onNotification: handleDaemonNotification,
    onBootstrapError: (error) => {
      runtime.logger.log("error", "daemon.bootstrap_error", {
        aetherCode: error.aetherCode,
        businessCode: error.businessCode,
        message: error.message,
        logPath: error.data?.log_path,
      });
    },
    onStderrLine: (line) => {
      // 持续 drain stderr（ADR-029 §3.5-3）：避免 pipe 满阻塞 daemon。
      runtime.lastStderr = `${runtime.lastStderr}\n${line}`.slice(-16_384);
      runtime.logger.log("debug", "daemon.stderr", { line });
    },
    onExit: (exit) => {
      handleDaemonExit(captured, exit.code, exit.signal);
    },
  });
  captured = client;
  runtime.logger.log("info", "daemon.spawn", {
    attempt,
    binary,
    args: daemonArguments(env),
    passphraseMode: runtime.secretsPassphrase === null ? "none" : "configured",
  });
  return client;
}

/** daemon 环境：口令只在内存中透传（不落盘/不日志；诊断包剔除）。 */
function daemonEnv(): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  if (runtime.secretsPassphrase !== null) {
    env.AETHER_SECRETS_PASSPHRASE = runtime.secretsPassphrase;
  }
  return env;
}

/**
 * daemon 在 ready 后退出：保留窗口 + 退避重启（M9-09 DoD-3）。
 *
 * 控制投递队列满由事件桥处理，不进入本函数（ADR-029 §3.7：不重启 daemon）。
 */
function handleDaemonExit(
  client: DaemonClient | null,
  code: number | null,
  signal: NodeJS.Signals | null,
): void {
  const isCurrent = client !== null && client === runtime.client;
  runtime.logger.log("warn", "daemon.exit", {
    code,
    signal,
    current: isCurrent,
    everReady: runtime.daemonEverReady,
    quitting: runtime.quitting,
  });
  if (client !== null && !isCurrent) {
    // 旧客户端（重启后）的迟到退出：只记录，不触发第二次重启。
    return;
  }
  runtime.ready = false;
  runtime.phase = "failed";
  runtime.bridge?.dispose();
  runtime.bridge = null;
  runtime.client = null;
  if (runtime.quitting) {
    return;
  }
  if (!runtime.daemonEverReady) {
    // 首次启动失败由 runStartupSequence/S-12 处理，不进入崩溃退避。
    return;
  }
  const decision = runtime.restartPolicy.recordStop(Date.now(), code, signal);
  runtime.logger.log(decision.restart ? "warn" : "error", "daemon.restart.decision", {
    ...decision,
    code,
    signal,
  });
  if (!decision.restart) {
    runtime.restartScheduledAt = null;
    runtime.logger.log("error", "daemon.restart.paused", {
      stopsInWindow: decision.stopsInWindow,
      totalStops: runtime.restartPolicy.snapshot().totalStops,
      message: "60s 内停止 ≥5 次，已停止自动重启；等待手动重试",
    });
    return;
  }
  scheduleDaemonRestart(decision.delayMs);
}

/** 安排一次退避重启（幂等：覆盖旧计时器）。 */
function scheduleDaemonRestart(delayMs: number): void {
  clearRestartTimer();
  if (runtime.quitting || runtime.fatal !== null) {
    return;
  }
  runtime.restartScheduledAt = Date.now() + delayMs;
  runtime.logger.log("info", "daemon.restart.scheduled", {
    delayMs,
    scheduledAt: runtime.restartScheduledAt,
    attempt: runtime.restartPolicy.snapshot().attempt,
  });
  runtime.restartTimer = setTimeout(() => {
    runtime.restartTimer = null;
    void restartDaemon();
  }, Math.max(0, delayMs));
}

/** 重启失败（进程未起来）：把本次失败计入停止窗口，继续下一档退避。 */
function scheduleDaemonRestartAfterFailure(): void {
  const decision = runtime.restartPolicy.recordStop(Date.now(), null, null);
  runtime.logger.log(decision.restart ? "warn" : "error", "daemon.restart.failure_decision", {
    ...decision,
  });
  if (!decision.restart) {
    runtime.restartScheduledAt = null;
    runtime.logger.log("error", "daemon.restart.paused", {
      stopsInWindow: decision.stopsInWindow,
      message: "60s 内停止 ≥5 次，已停止自动重启；等待手动重试",
    });
    return;
  }
  scheduleDaemonRestart(decision.delayMs);
}

/** 执行重启（保留窗口；不重复创建 BrowserWindow，不重新加载主 UI）。 */
async function restartDaemon(): Promise<void> {
  const win = runtime.mainWindow;
  if (runtime.quitting || runtime.fatal !== null || runtime.starting || runtime.ready) {
    return;
  }
  if (win === null || win.isDestroyed()) {
    return;
  }
  runtime.logger.log("info", "daemon.restart.start", {
    scheduledAt: runtime.restartScheduledAt,
  });
  runtime.starting = true;
  try {
    await startDaemonForWindow(win, { loadRenderer: false, restart: true });
  } catch (error) {
    runtime.logger.log("error", "daemon.restart.error", { error: errorText(error) });
    scheduleDaemonRestartAfterFailure();
  } finally {
    runtime.starting = false;
  }
}

/** 手动重试（S-09/诊断入口；清空熔断并立即重启）。 */
function retryDaemonNow(): void {
  runtime.logger.log("info", "daemon.restart.manual", {
    paused: runtime.restartPolicy.isPaused,
  });
  runtime.restartPolicy.manualRetry();
  scheduleDaemonRestart(0);
}

function clearRestartTimer(): void {
  if (runtime.restartTimer !== null) {
    clearTimeout(runtime.restartTimer);
    runtime.restartTimer = null;
  }
}

/** 事件桥：daemon `aether.event` → 单通道 FIFO + 16ms 批处理 → renderer。 */
function installEventBridge(): void {
  runtime.bridge?.dispose();
  runtime.bridge = new MainEventBridge({
    send: (channel, event) => {
      const win = runtime.mainWindow;
      if (win && !win.isDestroyed()) {
        win.webContents.send(channel, event);
      }
    },
    readback: async (sessionId, lastSeq) => {
      const client = runtime.client;
      if (client === null) {
        throw AetherRpcError.coreNotReady("核心未连接，无法补读");
      }
      const response = (await client.request("messages_page", {
        session_id: sessionId,
        last_seq: lastSeq,
      })) as { events?: AetherEvent[]; complete?: boolean };
      return {
        events: Array.isArray(response.events) ? response.events : [],
        complete: response.complete === true,
      };
    },
    audit: (record) => {
      runtime.logger.log("error", "event.bridge.escalated", { ...record });
    },
    rebuildConnection: (sessionId) => {
      // stdio 传输下“重建连接”= 重建 main 侧事件投递队列（不重启 daemon）；
      // 真实连接/进程重建属 M9-09。
      runtime.logger.log("warn", "event.bridge.reconnect", { sessionId });
      installEventBridge();
    },
  });
  runtime.logger.log("info", "event.bridge.installed", {
    channel: EVENT_IPC_CHANNEL,
  });
}

function handleDaemonNotification(notification: DaemonNotification): void {
  if (notification.method === "aether.event") {
    const params = notification.params;
    if (isEventEnvelope(params)) {
      runtime.bridge?.push(params);
    } else {
      runtime.logger.log("error", "event.invalid_envelope", {});
    }
    return;
  }
  if (notification.method === "daemon.status") {
    const params = notification.params as { phase?: unknown; reason?: unknown } | undefined;
    const phase = typeof params?.phase === "string" ? params.phase : null;
    const reason = typeof params?.reason === "string" ? params.reason : null;
    runtime.phase = phase;
    runtime.logger.log("info", "daemon.status", {
      phase,
      reason,
    });
    if (
      reason === "keychain_reauth_required" &&
      process.env.AETHER_SKIP_KEYCHAIN_PREFLIGHT !== "1"
    ) {
      // M9-09 DoD-8：旧 Tauri 条目在新签名身份下不可读 → S-13（不删除旧条目）。
      runtime.logger.log("warn", "keychain.preflight.reauth", { phase });
      runtime.keychainPrompted = true;
      showKeychainPrompt({
        scope: "provider",
        mode: "reauth",
        reason:
          "旧 Keychain 条目无法在新应用身份下读取，或 OS 凭据库不可用；请重试授权，或输入口令以降级方式打开。",
        notice: "不会删除任何旧条目。",
      });
    }
    if (phase === "failed" && runtime.ready) {
      // M9-09：保留窗口，由健康横幅提示；进程退出后走退避重启（不切 S-12）。
      runtime.logger.log("error", "daemon.status.failed", { reason });
    }
    return;
  }
  // permission.pending：M9-06 负责 UI 刷新；本任务只记录。
  runtime.logger.log("debug", "daemon.notification", {
    method: notification.method,
  });
}

/** 主窗口加载主 UI（dev = Vite；生产 = `aether://app`）。 */
async function loadMainRenderer(win: BrowserWindow): Promise<void> {
  const devServerUrl = runtime.devServerUrl;
  if (devServerUrl !== null) {
    runtime.logger.log("info", "renderer.load", { url: devServerUrl, mode: "dev" });
    await win.loadURL(devServerUrl);
    if (devToolsAutoOpen(process.env)) {
      win.webContents.openDevTools({ mode: "detach" });
    }
    return;
  }
  const indexFile = path.join(rendererDistDir(app.getAppPath()), "index.html");
  if (!existsSync(indexFile)) {
    runtime.logger.log("error", "renderer.missing", { indexFile });
    showFatalWindow({
      ok: false,
      client: runtime.client,
      fatal: {
        reason: "daemon_start_failed",
        message: `renderer 构建产物缺失：${indexFile}（请先执行 pnpm build:desktop）`,
        logPath: daemonLogPathFor(runtime.dataDir),
        dataDir: runtime.dataDir,
        attempts: 1,
      },
      attempts: [],
      retried: false,
      exit: null,
    });
    return;
  }
  runtime.logger.log("info", "renderer.load", { url: APP_URL, mode: "production" });
  await win.loadURL(APP_URL);
  if (devToolsAutoOpen(process.env)) {
    win.webContents.openDevTools({ mode: "detach" });
  }
}

/** S-12：关闭主窗口并显示独立最小 renderer（不加载主 UI）。 */
function showFatalWindow(failure: StartupFailure): void {
  if (runtime.fatalWindow !== null && !runtime.fatalWindow.isDestroyed()) {
    runtime.fatalWindow.focus();
    return;
  }
  runtime.fatal = failure;
  runtime.suppressAllClosed = true;
  const win = createSecureWindow(fatalPreloadPath());
  runtime.fatalWindow = win;
  hardenContents(win.webContents);
  const ready = waitForWindowReady(win);
  void win
    .loadFile(shellAsset("fatal", "fatal.html"))
    .then(() => ready)
    .then(() => {
      const mainWindow = runtime.mainWindow;
      runtime.mainWindow = null;
      if (mainWindow !== null && !mainWindow.isDestroyed()) {
        mainWindow.destroy();
      }
      runtime.suppressAllClosed = false;
    })
    .catch((error: unknown) => {
      runtime.suppressAllClosed = false;
      runtime.logger.log("error", "fatal.window_failed", { error: errorText(error) });
    });
}

/** S-13：Keychain 重授权/降级页（触发与 preflight 属 M9-09）。 */
export function showKeychainPrompt(state?: Partial<KeychainViewState>): void {
  runtime.keychainState = { ...runtime.keychainState, ...(state ?? {}) };
  if (runtime.keychainWindow !== null && !runtime.keychainWindow.isDestroyed()) {
    runtime.keychainWindow.focus();
    return;
  }
  const win = createSecureWindow(keychainPreloadPath());
  runtime.keychainWindow = win;
  hardenContents(win.webContents);
  void win.loadFile(shellAsset("keychain", "keychain.html"));
}

/** M9-09 注入 Keychain 动作实现（重授权/口令降级/跳过）。 */
export function installKeychainHooks(hooks: KeychainHooks): void {
  keychainHooks = hooks;
}

/** 生产接线（S-13；不删除旧条目）。 */
function installKeychainHooksOnce(): void {
  installKeychainHooks({
    retry: async () => {
      runtime.logger.log("info", "keychain.retry.restart", {});
      const restarted = await restartDaemonForKeychain();
      if (!restarted) {
        return { ok: false, error: "Keychain 重授权启动失败；旧密钥条目未删除" };
      }
      closeKeychainWindow();
      runtime.keychainPrompted = false;
      return { ok: true };
    },
    submitPassphrase: async (passphrase: string) => {
      if (passphrase.trim().length === 0) {
        return { ok: false, error: "口令不能为空" };
      }
      runtime.logger.log("info", "keychain.passphrase.restart", {});
      const restarted = await restartDaemonForKeychain(passphrase);
      if (restarted) {
        closeKeychainWindow();
        runtime.keychainPrompted = false;
        return { ok: true };
      }
      return { ok: false, error: "口令降级启动失败；旧密钥条目未删除" };
    },
    skip: async () => {
      runtime.logger.log("warn", "keychain.preflight.skipped", {
        scope: runtime.keychainState.scope,
        mode: runtime.keychainState.mode,
      });
      runtime.keychainState = {
        ...runtime.keychainState,
        mode: "degraded",
        notice: "已跳过 Keychain 授权；相关密钥功能不可用（旧条目未删除）。",
      };
      runtime.keychainPrompted = false;
      closeKeychainWindow();
      return { ok: true };
    },
  });
}

/** 口令降级：重启 daemon 并注入一次性环境变量（不落盘/不日志）。 */
async function restartDaemonForKeychain(passphrase?: string): Promise<boolean> {
  const win = runtime.mainWindow;
  if (win === null || win.isDestroyed()) {
    return false;
  }
  const previous = runtime.client;
  runtime.client = null;
  runtime.ready = false;
  runtime.bridge?.dispose();
  runtime.bridge = null;
  if (typeof passphrase === "string" && passphrase.length > 0) {
    runtime.secretsPassphrase = passphrase;
  }
  if (previous !== null) {
    const exit = await previous.shutdown(DAEMON_SHUTDOWN_BUDGET_MS);
    if (exit === null) {
      await killDaemonTree({ pid: previous.pid ?? 0, timeoutMs: 2_000 });
      await previous.waitForExit(1_000);
    }
  }
  runtime.starting = true;
  try {
    await startDaemonForWindow(win, { loadRenderer: false, restart: true });
  } finally {
    runtime.starting = false;
  }
  return runtime.ready;
}

function closeKeychainWindow(): void {
  const win = runtime.keychainWindow;
  runtime.keychainWindow = null;
  if (win !== null && !win.isDestroyed()) {
    win.destroy();
  }
}

/** 创建安全 BrowserWindow（DoD-2/DoD-5）。 */
function createSecureWindow(preload: string): BrowserWindow {
  const win = new BrowserWindow({
    width: WINDOW_WIDTH,
    height: WINDOW_HEIGHT,
    minWidth: WINDOW_MIN_WIDTH,
    minHeight: WINDOW_MIN_HEIGHT,
    show: false,
    backgroundColor: "#101318",
    autoHideMenuBar: true,
    webPreferences: secureWebPreferences(preload),
  });
  return win;
}

/** 等待 `ready-to-show`（最多 1.5s），到点直接显示（窗口可见预算）。 */
function waitForWindowReady(win: BrowserWindow, timeoutMs = 1_500): Promise<void> {
  return new Promise((resolve) => {
    let done = false;
    const finish = (): void => {
      if (done) {
        return;
      }
      done = true;
      clearTimeout(timer);
      if (!win.isDestroyed() && !win.isVisible()) {
        win.show();
      }
      resolve();
    };
    const timer = setTimeout(finish, timeoutMs);
    win.once("ready-to-show", finish);
    win.once("closed", () => {
      if (!done) {
        done = true;
        clearTimeout(timer);
        resolve();
      }
    });
  });
}

/** 统一加固 WebContents（导航/新窗口/webview/权限/DevTools）。 */
function hardenContents(contents: WebContents): void {
  if (hardenedContents.has(contents)) {
    return;
  }
  hardenedContents.add(contents);
  if (isBrowserContents(contents)) {
    return;
  }
  const type = contents.getType();
  const lastPreferences = (
    contents as unknown as {
      getLastWebPreferences?: () => { partition?: unknown } | undefined;
    }
  ).getLastWebPreferences?.();
  const partition =
    typeof lastPreferences?.partition === "string"
      ? lastPreferences.partition
      : "";
  if (type === "browserView" || partition.startsWith(BROWSER_PARTITION_PREFIX)) {
    // WebContentsView 由 `BrowserHost` 按 ADR-029 §5.2 单独接线；
    // 主 UI 的 `aether://app` 导航白名单不得套用到浏览器视图
    // （Electron 45 的 `WebContents.getType()` 对 WebContentsView 仍返回
    // `window`，因此以 partition 前缀为准）。
    return;
  }
  if (type === "webview") {
    runtime.logger.log("warn", "webview.destroyed", {});
    contents.close();
    return;
  }
  contents.setWindowOpenHandler(({ url }) => {
    const decision = decideWindowOpen(url);
    if (decision === "external") {
      openExternal(url, "window.open");
    }
    return { action: "deny" };
  });

  contents.on("will-navigate", (event, url) => {
    const decision = decideNavigation(url, {
      ...(runtime.devServerUrl ? { devServerUrl: runtime.devServerUrl } : {}),
    });
    if (decision === "allow") {
      return;
    }
    event.preventDefault();
    if (decision === "external") {
      openExternal(url, "will-navigate");
    } else {
      runtime.logger.log("warn", "navigation.blocked", { url });
    }
  });

  contents.on("will-attach-webview", (event) => {
    event.preventDefault();
    runtime.logger.log("warn", "webview.blocked", {});
  });

  if (!devToolsEnabled({ isPackaged: app.isPackaged, env: process.env })) {
    contents.on("before-input-event", (event, input) => {
      const key = input.key.toLowerCase();
      if (key === "f12" || (input.control && input.shift && key === "i")) {
        event.preventDefault();
      }
    });
  }
}

/** 外链交系统浏览器；E2E 只记录不真正拉起浏览器。 */
function openExternal(url: string, source: string): void {
  runtime.externalOpens.push(url);
  runtime.logger.log("info", "external.open", { url, source });
  if (process.env.AETHER_E2E === "1") {
    return;
  }
  void shell.openExternal(url).catch((error: unknown) => {
    runtime.logger.log("warn", "external.open_failed", {
      url,
      error: errorText(error),
    });
  });
}

/** IPC：`aether:rpc` / `aether:version` / S-12 / S-13。 */
function installIpcHandlers(): void {
  ipcMain.handle(RPC_IPC_CHANNEL, async (event, payload: unknown) => {
    assertMainSender(event);
    try {
      const request = parseRpcEnvelope(payload);
      const result = await dispatchRpc(request.method, request.params);
      return { ok: true, result };
    } catch (error) {
      return { ok: false, error: toWireError(error) };
    }
  });

  ipcMain.handle(VERSION_IPC_CHANNEL, (event) => {
    assertMainSender(event);
    return versionInfo();
  });

  ipcMain.handle(FATAL_STATE_CHANNEL, (event) => {
    assertFatalSender(event);
    const failure = runtime.fatal;
    const fatal = failure?.fatal ?? {
      reason: "daemon_start_failed" as const,
      message: "核心未能启动",
      logPath: shellLogPathFor(runtime.dataDir),
      dataDir: runtime.dataDir,
      attempts: 0,
    };
    return {
      reason: fatal.reason,
      message: fatal.message,
      logPath: fatal.logPath,
      dataDir: fatal.dataDir,
      attempts: fatal.attempts,
      manualMove: MANUAL_DATA_DIR_INSTRUCTIONS,
    };
  });

  ipcMain.handle(FATAL_RETRY_CHANNEL, async (event) => {
    assertFatalSender(event);
    if (runtime.starting) {
      return { ok: false };
    }
    const win = runtime.fatalWindow;
    runtime.fatalWindow = null;
    runtime.fatal = null;
    runtime.suppressAllClosed = true;
    void createMainWindowAndStart().finally(() => {
      if (win !== null && !win.isDestroyed()) {
        win.destroy();
      }
    });
    return { ok: true };
  });

  ipcMain.handle(FATAL_EXIT_CHANNEL, (event) => {
    assertFatalSender(event);
    setImmediate(() => {
      requestQuit();
    });
    return { ok: true };
  });

  ipcMain.handle(FATAL_EXPORT_CHANNEL, async (event, targetPath?: unknown) => {
    assertFatalSender(event);
    return exportDiagnosticsFromFatal(targetPath);
  });

  ipcMain.handle(KEYCHAIN_STATE_CHANNEL, (event) => {
    assertKeychainSender(event);
    return runtime.keychainState;
  });
  ipcMain.handle(KEYCHAIN_RETRY_CHANNEL, async (event) => {
    assertKeychainSender(event);
    return keychainHooks?.retry() ?? notWired();
  });
  ipcMain.handle(KEYCHAIN_PASSPHRASE_CHANNEL, async (event, passphrase: unknown) => {
    assertKeychainSender(event);
    if (typeof passphrase !== "string" || passphrase.length === 0) {
      return { ok: false, error: "口令不能为空" };
    }
    return keychainHooks?.submitPassphrase(passphrase) ?? notWired();
  });
  ipcMain.handle(KEYCHAIN_SKIP_CHANNEL, async (event) => {
    assertKeychainSender(event);
    return keychainHooks?.skip() ?? notWired();
  });
}

function parseRpcEnvelope(payload: unknown): { method: string; params?: unknown } {
  if (typeof payload !== "object" || payload === null || Array.isArray(payload)) {
    throw new ShellLocalError(-32600, "invalid_json", "IPC 请求必须是对象");
  }
  const object = payload as Record<string, unknown>;
  for (const key of Object.keys(object)) {
    if (key !== "method" && key !== "params") {
      throw new ShellLocalError(-32600, "unknown_field", `未知 IPC 字段：${key}`, {
        field: key,
      });
    }
  }
  if (typeof object.method !== "string" || object.method.length === 0) {
    throw new ShellLocalError(-32600, "invalid_json", "method 必须是非空字符串");
  }
  return {
    method: object.method,
    ...(Object.prototype.hasOwnProperty.call(object, "params")
      ? { params: object.params }
      : {}),
  };
}

async function dispatchRpc(method: string, params: unknown): Promise<unknown> {
  const decision = guardRpcRequest({
    method,
    params,
    inFlight: concurrentRpc,
    commands: COMMAND_NAMES,
  });
  if (!decision.ok) {
    throw new AetherRpcError(decision.rejection.code, decision.rejection.message, {
      aether_code: decision.rejection.aetherCode,
      ...(decision.rejection.field === undefined
        ? {}
        : { field: decision.rejection.field }),
    });
  }

  concurrentRpc += 1;
  try {
    if (isShellLocalCommand(method)) {
      const outcome = await handleShellLocalCommand(method, params, {
        pick: pickPath,
        requestExit: () => requestQuit(),
        requestRestart: requestRestart,
      });
      if (outcome.after !== undefined) {
        setImmediate(outcome.after);
      }
      return outcome.result;
    }

    const client = runtime.client;
    if (!runtime.ready || client === null || client.isClosed) {
      throw AetherRpcError.coreNotReady();
    }
    const requestStartedAt = Date.now();
    const result = await client.request(method, params);
    if (method === "health" && runtime.uiReadyAt === null) {
      runtime.uiReadyAt = Date.now();
      runtime.logger.log("info", "ui.ready", {
        uiReadyMs: runtime.uiReadyAt - runtime.startedAt,
      });
    }
    if (method === "session_create" && runtime.sessionCreateMs === null) {
      const elapsed = Date.now() - requestStartedAt;
      runtime.sessionCreateMs = elapsed;
      runtime.logger.log("info", "session.create.timing", {
        sessionCreateMs: elapsed,
        uiReadyAt: runtime.uiReadyAt,
        withinTarget: elapsed < 2_000,
      });
    }
    return result;
  } finally {
    concurrentRpc -= 1;
  }
}

async function pickPath(kind: RefPickKind): Promise<string | null> {
  const win = runtime.mainWindow;
  const options = {
    properties: (kind === "file" ? ["openFile"] : ["openDirectory", "createDirectory"]) as
      | ["openFile"]
      | ["openDirectory", "createDirectory"],
    title: kind === "file" ? "选择引用文件" : "选择目录",
  };
  const result =
    win === null || win.isDestroyed()
      ? await dialog.showOpenDialog(options)
      : await dialog.showOpenDialog(win, options);
  if (result.canceled || result.filePaths.length === 0) {
    return null;
  }
  return result.filePaths[0] ?? null;
}

/** S-12 离线导出诊断（不依赖 daemon）。 */
async function exportDiagnosticsFromFatal(
  targetPath: unknown,
): Promise<{ ok: boolean; path?: string; error?: string }> {
  let resolved: string | null =
    typeof targetPath === "string" && targetPath.length > 0
      ? path.resolve(targetPath)
      : null;
  if (resolved === null) {
    const win = runtime.fatalWindow;
    const defaultPath = path.join(
      app.getPath("downloads"),
      defaultDiagnosticsFileName(),
    );
    const options = {
      title: "导出离线诊断",
      defaultPath,
      filters: [{ name: "Aether 诊断", extensions: ["json"] }],
    };
    const picked =
      win === null || win.isDestroyed()
        ? await dialog.showSaveDialog(options)
        : await dialog.showSaveDialog(win, options);
    if (picked.canceled || typeof picked.filePath !== "string") {
      return { ok: false, error: "cancelled" };
    }
    resolved = picked.filePath;
  }
  try {
    const fatal = runtime.fatal?.fatal;
    const payload = buildOfflineDiagnostics({
      fatal:
        fatal === undefined
          ? {
              reason: "daemon_start_failed",
              message: "核心未能启动",
              logPath: shellLogPathFor(runtime.dataDir),
              dataDir: runtime.dataDir,
              attempts: 0,
            }
          : fatal,
      versions: {
        app: app.getVersion(),
        electron: process.versions.electron ?? "unknown",
        chrome: process.versions.chrome ?? "unknown",
        node: process.versions.node,
        protocol: "1.0",
        ...(runtime.versions.daemon ? { daemon: runtime.versions.daemon } : {}),
        ...(runtime.versions.core ? { core: runtime.versions.core } : {}),
      },
      logs: {
        shell: runtime.logger.tail(DIAGNOSTICS_LINES),
        daemon: readFileTail(daemonLogPathFor(runtime.dataDir), DIAGNOSTICS_LINES),
        daemonStderr: (runtime.lastStderr || runtime.client?.stderrTail || "").slice(-16_384),
      },
      env: process.env,
    });
    const written = writeOfflineDiagnostics(resolved, payload);
    runtime.logger.log("info", "fatal.diagnostics.exported", { path: written });
    return { ok: true, path: written };
  } catch (error) {
    runtime.logger.log("error", "fatal.diagnostics.failed", {
      error: errorText(error),
    });
    return { ok: false, error: errorText(error) };
  }
}

function versionInfo(): Record<string, string | undefined> {
  return {
    app: app.getVersion(),
    daemon: runtime.versions.daemon,
    core: runtime.versions.core,
    protocol: "1.0",
    electron: process.versions.electron,
    chrome: process.versions.chrome,
    node: process.versions.node,
  };
}

function assertMainSender(event: IpcMainInvokeEvent): void {
  const win = runtime.mainWindow;
  if (win === null || win.isDestroyed() || event.sender !== win.webContents) {
    throw new Error("拒绝：非主窗口 IPC");
  }
}

function assertFatalSender(event: IpcMainInvokeEvent): void {
  const win = runtime.fatalWindow;
  if (win === null || win.isDestroyed() || event.sender !== win.webContents) {
    throw new Error("拒绝：非 S-12 窗口 IPC");
  }
}

function assertKeychainSender(event: IpcMainInvokeEvent): void {
  const win = runtime.keychainWindow;
  if (win === null || win.isDestroyed() || event.sender !== win.webContents) {
    throw new Error("拒绝：非 S-13 窗口 IPC");
  }
}

function toWireError(error: unknown): {
  code: number;
  message: string;
  data: Record<string, unknown>;
} {
  if (error instanceof ShellLocalError) {
    return error.toWire();
  }
  if (error instanceof AetherRpcError) {
    return {
      code: error.code,
      message: error.message,
      data: error.data ?? { aether_code: "internal" },
    };
  }
  return {
    code: -32603,
    message: errorText(error),
    data: { aether_code: "internal" },
  };
}

function requestQuit(): void {
  runtime.logger.log("info", "app.exit.requested", { daemonPid: runtime.client?.pid ?? null });
  app.quit();
}

function requestRestart(): void {
  runtime.logger.log("info", "app.restart.requested", { daemonPid: runtime.client?.pid ?? null });
  app.relaunch();
  app.quit();
}

function activeWindow(): BrowserWindow | null {
  for (const win of [runtime.fatalWindow, runtime.mainWindow, runtime.keychainWindow]) {
    if (win !== null && !win.isDestroyed()) {
      return win;
    }
  }
  return null;
}

function mainPreloadPath(): string {
  return path.join(__dirname, "preload.cjs");
}

function fatalPreloadPath(): string {
  return path.join(__dirname, "fatalPreload.cjs");
}

function keychainPreloadPath(): string {
  return path.join(__dirname, "keychainPreload.cjs");
}

/** shell 页面资源：构建后 `dist/shell/...`；源码目录兜底便于调试。 */
function shellAsset(page: string, file: string): string {
  const built = path.join(__dirname, "shell", page, file);
  if (existsSync(built)) {
    return built;
  }
  return path.join(__dirname, "..", page, file);
}

function isEventEnvelope(value: unknown): value is AetherEvent {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const event = value as Record<string, unknown>;
  return (
    event.v === 1 &&
    typeof event.id === "string" &&
    typeof event.session_id === "string" &&
    typeof event.seq === "number" &&
    typeof event.ts === "number" &&
    typeof event.type === "string" &&
    typeof event.payload === "object"
  );
}

function notWired(): KeychainActionResult {
  return { ok: false, error: "keychain_not_wired（M9-09 接线）" };
}

function devServerUrlFromEnv(): string | null {
  const raw = process.env.AETHER_RENDERER_URL;
  if (typeof raw !== "string" || raw.length === 0) {
    return null;
  }
  try {
    const url = new URL(raw);
    if (url.protocol !== "http:" || (url.hostname !== "127.0.0.1" && url.hostname !== "localhost")) {
      return null;
    }
    return `${url.origin}${url.pathname === "/" ? "" : url.pathname}`;
  } catch {
    return null;
  }
}

function safeUrl(contents: WebContents): string {
  try {
    return contents.getURL();
  } catch {
    return "";
  }
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** E2E 只读快照（仅 `AETHER_E2E=1`；不暴露给 renderer）。 */
function installE2EHook(): void {
  if (process.env.AETHER_E2E !== "1") {
    return;
  }
  const hook = {
    snapshot: () => ({
      pid: process.pid,
      startedAt: runtime.startedAt,
      windowCreatedAt: runtime.windowCreatedAt,
      windowReadyAt: runtime.windowReadyAt,
      uiReadyAt: runtime.uiReadyAt,
      sessionCreateMs: runtime.sessionCreateMs,
      daemonSpawnedAt: runtime.daemonSpawnedAt,
      daemonPid: runtime.client?.pid ?? null,
      daemonSpawnCount: runtime.daemonSpawnCount,
      daemonEverReady: runtime.daemonEverReady,
      ready: runtime.ready,
      phase: runtime.phase,
      fatal: runtime.fatal?.fatal ?? null,
      dataDir: runtime.dataDir,
      shellLogPath: runtime.logger.path,
      daemonLogPath: daemonLogPathFor(runtime.dataDir),
      initializeMs: runtime.initializeMs,
      withinTarget: runtime.withinTarget,
      retried: runtime.retried,
      secondInstanceCount: runtime.secondInstanceCount,
      rendererUrl: runtime.mainWindow?.webContents.getURL() ?? null,
      fatalUrl: runtime.fatalWindow?.webContents.getURL() ?? null,
      preloadPath: mainPreloadPath(),
      windows: {
        main: runtime.mainWindow !== null && !runtime.mainWindow.isDestroyed(),
        fatal: runtime.fatalWindow !== null && !runtime.fatalWindow.isDestroyed(),
      },
      externalOpens: [...runtime.externalOpens],
      devToolsAutoOpen: devToolsAutoOpen(process.env),
      restart: {
        ...runtime.restartPolicy.snapshot(),
        scheduledAt: runtime.restartScheduledAt,
        timerPending: runtime.restartTimer !== null,
      },
      shutdown: {
        startedAt: runtime.shutdownStartedAt,
        elapsedMs: runtime.shutdownElapsedMs,
        gracefulBudgetMs: DAEMON_SHUTDOWN_BUDGET_MS,
        hardBudgetMs: MAIN_EXIT_HARD_BUDGET_MS,
        t11BudgetMs: T11_BUDGET_MS,
      },
      keychain: {
        ...runtime.keychainState,
        prompted: runtime.keychainPrompted,
        passphraseSupplied: runtime.secretsPassphrase !== null,
      },
    }),
    retryDaemon: () => {
      retryDaemonNow();
      return true;
    },
    browser: {
      open: (input: { id: string; url: string; bounds?: BrowserBounds }) =>
        runtime.browserHost?.open(input) ?? Promise.resolve(null),
      close: (id: string) => runtime.browserHost?.close(id) ?? false,
      evaluate: (id: string, script: string) =>
        runtime.browserHost?.evaluate(id, script) ?? Promise.resolve(null),
      snapshot: () => runtime.browserHost?.snapshot() ?? [],
      downloads: () => runtime.browserHost?.downloads() ?? [],
      requestDownload: (id: string, url: string) =>
        runtime.browserHost?.requestDownload(id, url) ?? false,
      confirmDownload: (downloadId: string) =>
        runtime.browserHost?.confirmDownload(downloadId) ?? false,
      cancelDownload: (downloadId: string) =>
        runtime.browserHost?.cancelDownload(downloadId) ?? false,
    },
  };
  (globalThis as unknown as { __aetherShell: typeof hook }).__aetherShell = hook;
}
