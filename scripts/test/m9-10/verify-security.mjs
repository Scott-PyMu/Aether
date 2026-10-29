#!/usr/bin/env node
/**
 * `pnpm verify:security` / `pnpm verify:m9-10`（P2 计划 M9-10；ADR-029 §5）。
 *
 * 静态门禁（可执行；动态证据见 `docs/M9-10-证据.md`）：
 *  1. Electron flags / CSP / 导航 / webview / DevTools；
 *  2. preload 白名单、main 侧 RPC 防线（2MiB/64/未知方法）、daemon 校验矩阵；
 *  3. `@electron/fuses` 配置与构建后断言工具；
 *  4. WebContentsView 独立 partition / 无 preload/Node / 权限/证书/下载确认；
 *  5. 路径参数命令全集（P1-8）生成清单与集合断言；
 *  6. 密钥 0 泄漏（api_key 响应/日志/诊断/导出 + 扫描）；
 *  7. SBOM/许可证（Electron/Chromium/Node + license-checker/cargo deny）。
 */
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
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

function expectNotMatch(text, pattern, message) {
  return expect(!pattern.test(text), message);
}

function runNode(rel, label) {
  const result = spawnSync(process.execPath, [path.join(root, rel)], {
    cwd: root,
    encoding: "utf8",
  });
  expect(
    result.status === 0,
    `${label} 失败（exit=${result.status}）：${(result.stderr ?? "").trim()}`,
  );
  return result.stdout ?? "";
}

function walk(dir, extensions, skip = new Set(["node_modules", "dist", "dist-out", ".tmp"])) {
  const base = path.join(root, dir);
  const found = [];
  const stack = [base];
  while (stack.length > 0) {
    const current = stack.pop();
    if (current === undefined || !existsSync(current)) continue;
    for (const entry of readdirSync(current)) {
      const full = path.join(current, entry);
      const stats = statSync(full);
      if (stats.isDirectory()) {
        if (skip.has(entry)) continue;
        stack.push(full);
      } else if (extensions.some((ext) => entry.endsWith(ext))) {
        found.push(full);
      }
    }
  }
  return found;
}

// ===== 0. 包脚本与文件 =====
const rootPkg = readJson("package.json");
expect(
  rootPkg.scripts?.["verify:security"] === "node scripts/test/m9-10/verify-security.mjs",
  "缺少 verify:security",
);
expect(
  rootPkg.scripts?.["verify:m9-10"] === "node scripts/test/m9-10/verify-security.mjs",
  "缺少 verify:m9-10",
);
expect(
  rootPkg.scripts?.["e2e:m9-10"] === "node scripts/test/m9-10/e2e-security.mjs",
  "缺少 e2e:m9-10",
);
expect(
  rootPkg.scripts?.["sbom:m9-10"] === "node scripts/test/m9-10/sbom.mjs",
  "缺少 sbom:m9-10",
);
for (const file of [
  "apps/desktop/electron/security.ts",
  "apps/desktop/electron/preload.ts",
  "apps/desktop/electron/rpcGuard.ts",
  "apps/desktop/electron/rpcGuard.test.ts",
  "apps/desktop/electron/browserView.ts",
  "apps/desktop/electron/browserView.test.ts",
  "apps/desktop/electron/browserHost.ts",
  "apps/desktop/electron/downloadFiles.ts",
  "apps/desktop/electron/downloadFiles.test.ts",
  "apps/desktop/electron/fuses.json",
  "scripts/ci/electron-fuses.mjs",
  "scripts/ci/path-commands.mjs",
  "scripts/test/m9-10/verify-security.mjs",
  "scripts/test/m9-10/e2e-security.mjs",
  "scripts/test/m9-10/self-signed-cert.mjs",
  "scripts/test/m9-10/sbom.mjs",
  "packages/protocol/src/path-commands.json",
  "crates/aether-app/tests/m9_10_path_security.rs",
  "docs/M9-10-证据.md",
]) {
  expect(exists(file), `缺少文件：${file}`);
}
expect(
  exists("apps/desktop/electron/dist/main.cjs") && exists("apps/desktop/dist/index.html"),
  "缺少构建产物（先 pnpm build:desktop）",
);
const desktopPkg = readJson("apps/desktop/package.json");
expect(
  desktopPkg.devDependencies?.["@electron/fuses"] === "2.1.3",
  "@electron/fuses 必须精确 2.1.3（ADR-029 §5.1）",
);

// ===== 1. Electron flags / CSP / 导航 / DevTools =====
const security = read("apps/desktop/electron/security.ts");
for (const [label, pattern] of [
  ["nodeIntegration:false", /nodeIntegration: false/],
  ["contextIsolation:true", /contextIsolation: true/],
  ["sandbox:true", /sandbox: true/],
  ["webSecurity:true", /webSecurity: true/],
  ["allowRunningInsecureContent:false", /allowRunningInsecureContent: false/],
  ["webviewTag:false", /webviewTag: false/],
  ["nodeIntegrationInSubFrames:false", /nodeIntegrationInSubFrames: false/],
  ["生产 CSP connect-src 'none'", /connect-src 'none'/],
  ["生产 CSP frame-src 'none'", /frame-src 'none'/],
  ["生产 CSP object-src 'none'", /object-src 'none'/],
  ["生产 CSP base-uri 'none'", /base-uri 'none'/],
  ["生产 CSP form-action 'none'", /form-action 'none'/],
  ["开发 CSP 仅 Vite localhost", /ws:\/\/127\.0\.0\.1:5173/],
]) {
  expectMatch(security, pattern, `security.ts 缺少${label}`);
}
const devCspBlock = /export const DEV_CSP = \[([\s\S]*?)\]\.join/.exec(security)?.[1] ?? "";
expectNotMatch(
  devCspBlock,
  /https?:\/\/(?!127\.0\.0\.1:5173)/,
  "开发 CSP 不得允许 localhost 之外的远程源",
);

const appSource = read("apps/desktop/electron/app.ts");
for (const [label, pattern] of [
  ["全局 web-contents-created 加固", /web-contents-created/],
  ["统一 hardenContents", /hardenContents\(/],
  ["新窗口拦截", /setWindowOpenHandler/],
  ["顶层导航拦截", /will-navigate/],
  ["webview 拦截", /will-attach-webview/],
  ["权限请求默认拒绝", /setPermissionRequestHandler/],
  ["权限检查默认拒绝", /setPermissionCheckHandler/],
  ["证书错误显式拒绝", /certificate-error[\s\S]{0,400}callback\(false\)/],
  ["DevTools 仅 debug", /devToolsEnabled\(/],
  ["生产 CSP 响应头", /Content-Security-Policy/],
  ["外链系统浏览器", /openExternal/],
  ["main RPC 防线", /guardRpcRequest\(/],
  ["BrowserHost 接线", /new BrowserHost\(/],
]) {
  expectMatch(appSource, pattern, `app.ts 缺少${label}`);
}
expectNotMatch(
  appSource,
  /appendSwitch\(\s*["']remote-debugging-port/,
  "main 不得开启远程调试端口",
);
for (const artifact of [
  "apps/desktop/index.html",
  "apps/desktop/dist/index.html",
]) {
  if (!exists(artifact)) continue;
  const html = read(artifact);
  expectNotMatch(
    html,
    /(src|href)=["']https?:\/\//,
    `${artifact} 不得加载远程脚本/样式/字体`,
  );
  expectNotMatch(html, /<iframe\b/i, `${artifact} 不得内嵌 iframe`);
}
for (const file of walk("apps/desktop/dist", [".js"])) {
  const text = readFileSync(file, "utf8");
  expectNotMatch(
    text,
    /\bnew Function\s*\(/,
    `renderer 构建产物不得使用 new Function：${path.relative(root, file)}`,
  );
}

// ===== 2. preload / main / daemon 校验 =====
const preload = read("apps/desktop/electron/preload.ts");
for (const [label, pattern] of [
  ["contextBridge", /contextBridge\.exposeInMainWorld\("aether"/],
  ["webUtils.getPathForFile", /webUtils\.getPathForFile/],
  ["2MiB 上限", /MAX_PAYLOAD_BYTES = 2 \* 1024 \* 1024/],
  ["事件通道", /EVENT_IPC_CHANNEL/],
]) {
  expectMatch(preload, pattern, `preload.ts 缺少${label}`);
}
expectNotMatch(preload, /\brequire\s*\(/, "preload 源码不得直接 require 任意模块");
expectNotMatch(
  preload,
  /exposeInMainWorld\(\s*["']ipcRenderer|window\.ipcRenderer/,
  "preload 不得暴露 ipcRenderer",
);
expectNotMatch(
  preload.replace(/\/\*[\s\S]*?\*\//g, ""),
  /\bprocess\b/,
  "preload 不得暴露 process",
);
const preloadKeys = /export const PRELOAD_API_KEYS = \[([\s\S]*?)\] as const;/.exec(security)?.[1] ?? "";
const keys = [...preloadKeys.matchAll(/"([^"]+)"/g)].map((match) => match[1]);
expect(
  JSON.stringify(keys) ===
    JSON.stringify(["rpc", "onEvent", "offEvent", "pathForFile", "version"]),
  `preload 白名单必须严格为 5 项（实际：${keys.join(",")}）`,
);

const guard = read("apps/desktop/electron/rpcGuard.ts");
for (const [label, pattern] of [
  ["2MiB", /MAX_RPC_PAYLOAD_BYTES = 2 \* 1024 \* 1024/],
  ["并发 64", /MAX_CONCURRENT_RPC = 64/],
  ["未知方法", /not_implemented/],
  ["超限错误码", /too_large/],
  ["并发拒绝", /inFlight >= MAX_CONCURRENT_RPC/],
]) {
  expectMatch(guard, pattern, `rpcGuard.ts 缺少${label}`);
}
const validate = read("crates/aether-app/src/ipc/validate.rs");
expectMatch(validate, /deny_unknown_fields|parse_strict/, "daemon 缺少严格反序列化");
const ipcValidation = read("crates/aether-app/tests/ipc_validation.rs");
expectMatch(
  ipcValidation,
  /fn every_command_is_routed_and_rejects_unknown_members/,
  "缺少 63 命令未知字段矩阵",
);
const pathSecurity = read("crates/aether-app/tests/m9_10_path_security.rs");
expectMatch(
  pathSecurity,
  /fn every_path_command_rejects_relative_path_before_backend/,
  "缺少路径命令 0 绕过矩阵",
);
expectMatch(pathSecurity, /fn path_command_manifest_covers_p1_8_and_reviewed_set/, "缺少路径清单集合断言");

// ===== 3. fuses =====
const fuses = readJson("apps/desktop/electron/fuses.json");
expect(fuses.version === "1", "fuses.json version 必须为 1");
expect(fuses.strictlyRequireAllFuses === true, "必须 strictlyRequireAllFuses=true");
for (const [name, value] of Object.entries({
  runAsNode: false,
  enableNodeOptionsEnvironmentVariable: false,
  enableNodeCliInspectArguments: false,
  onlyLoadAppFromAsar: true,
})) {
  expect(fuses.options?.[name] === value, `fuses.json ${name} 必须为 ${value}`);
}
const fuseScript = read("scripts/ci/electron-fuses.mjs");
for (const [label, pattern] of [
  ["flipFuses", /flipFuses/],
  ["getCurrentFuseWire", /getCurrentFuseWire/],
  ["onlyLoadAppFromAsar", /onlyLoadAppFromAsar|OnlyLoadAppFromAsar/],
  ["runAsNode", /runAsNode|RunAsNode/],
  ["9 fuse 完整性", /strictlyRequireAllFuses/],
]) {
  expectMatch(fuseScript, pattern, `electron-fuses.mjs 缺少${label}`);
}
expect(
  read("apps/desktop/electron/build.mjs").includes("fuses: fusesConfig"),
  "build-manifest 必须记录 fuses 配置",
);

// ===== 4. WebContentsView =====
const browserView = read("apps/desktop/electron/browserView.ts");
for (const [label, pattern] of [
  ["独立 partition 前缀", /BROWSER_PARTITION_PREFIX = "persist:aether-browser:"/],
  ["导航仅 http/https", /url\.protocol === "http:" \|\| url\.protocol === "https:"/],
  ["权限默认拒绝", /decideBrowserPermission[\s\S]{0,80}return false/],
  ["文件名清洗", /sanitizeDownloadFileName/],
  ["确认状态机", /class BrowserDownloadGate/],
]) {
  expectMatch(browserView, pattern, `browserView.ts 缺少${label}`);
}
const prefsBody =
  /export function browserWebPreferences\(\)[\s\S]*?\n}/.exec(browserView)?.[0] ?? "";
expectNotMatch(prefsBody, /preload\s*:/, "浏览器视图 webPreferences 不得设置 preload");
expectMatch(prefsBody, /sandbox: true/, "浏览器视图 sandbox 必须 true");
expectMatch(prefsBody, /nodeIntegration: false/, "浏览器视图 nodeIntegration 必须 false");
expectMatch(prefsBody, /contextIsolation: true/, "浏览器视图 contextIsolation 必须 true");
expectMatch(prefsBody, /webviewTag: false/, "浏览器视图 webviewTag 必须 false");

const browserHost = read("apps/desktop/electron/browserHost.ts");
for (const [label, pattern] of [
  ["WebContentsView", /new WebContentsView\(/],
  ["partition 注入", /webPreferences: \{ \.\.\.browserWebPreferences\(\), partition \}/],
  ["权限拒绝", /setPermissionRequestHandler[\s\S]{0,160}callback\(false\)/],
  ["权限检查拒绝", /setPermissionCheckHandler\(\(\) => false\)/],
  ["证书默认校验", /setCertificateVerifyProc/],
  ["下载确认", /will-download/],
  ["隔离区", /quarantineDir/],
  ["确认后移动", /promoteDownloadedFile/],
  ["外链系统浏览器", /openExternal\(url, "browser\.window\.open"\)/],
]) {
  expectMatch(browserHost, pattern, `browserHost.ts 缺少${label}`);
}
expectNotMatch(
  browserHost,
  /shell\.openPath|shell\.openExternal\(.*download/i,
  "下载不得自动打开/执行",
);
const downloadFiles = read("apps/desktop/electron/downloadFiles.ts");
expectMatch(downloadFiles, /renameSync/, "下载确认后必须移动隔离区文件");
expectMatch(downloadFiles, /promoteDownloadedFile/, "缺少下载落盘工具");
const e2e = read("scripts/test/m9-10/e2e-security.mjs");
for (const [label, pattern] of [
  ["独立 partition", /persist:aether-browser:/],
  ["localStorage/cookie 隔离", /localStorage\/cookie 互不可读/],
  ["window.open", /window\.open/],
  ["file:// 拒绝", /file:\/\/\/C:\/Windows\/win\.ini/],
  ["证书错误", /ERR_CERT|CERT_/],
  ["权限拒绝", /getUserMedia/],
  ["下载确认", /confirmDownload/],
  ["下载取消", /cancelDownload/],
]) {
  expectMatch(e2e, pattern, `e2e-security.mjs 缺少${label}`);
}

// ===== 5. 路径命令全集（P1-8） =====
const pathCheckOutput = runNode("scripts/ci/path-commands.mjs", "path-commands check");
expect(
  pathCheckOutput.includes("diff=0"),
  "path-commands check 未报告 diff=0",
);
const pathManifest = readJson("packages/protocol/src/path-commands.json");
expect(pathManifest.schema === "aether.path-commands.v1", "path-commands schema 不符");
expect(pathManifest.counts?.commands === 10, "路径命令数必须 10");
expect(pathManifest.counts?.required === 8, "P1-8 必有命令必须 8");
const pathNames = (pathManifest.commands ?? []).map((entry) => entry.name).sort();
expect(
  JSON.stringify(pathNames) ===
    JSON.stringify([
      "archive_export",
      "artifact_add",
      "audit_export",
      "backup_create",
      "backup_restore",
      "export_diagnostics",
      "session_export",
      "startup_migrate",
      "workspace_create",
      "workspace_set",
    ]),
  `路径命令集合漂移：${pathNames.join(",")}`,
);
for (const entry of pathManifest.commands ?? []) {
  expect(
    Array.isArray(entry.path_fields) && entry.path_fields.length > 0,
    `${entry.name} 缺少 path_fields`,
  );
  expect(
    typeof entry.validator === "string" && entry.validator.length > 0,
    `${entry.name} 缺少 validator`,
  );
  expect(
    ["path_rejected", "artifact_path_rejected"].includes(entry.error_code),
    `${entry.name} 错误码不符`,
  );
}
for (const required of pathManifest.required ?? []) {
  expect(pathNames.includes(required), `清单缺少 P1-8 命令：${required}`);
}
expect(
  read("packages/protocol/package.json").includes("path-commands.mjs generate") &&
    read("packages/protocol/package.json").includes("path-commands.mjs check"),
  "protocol generate/check 未接入 path-commands",
);
const ipcPath = read("crates/aether-app/src/ipc/path.rs");
for (const [label, pattern] of [
  ["canonicalize", /std::fs::canonicalize/],
  ["Windows 特殊路径拒绝", /reject_windows_special_forms/],
  ["前缀比较", /pub fn is_within/],
  ["同步盘拒绝", /reject_cloud_sync_path/],
]) {
  expectMatch(ipcPath, pattern, `path.rs 缺少${label}`);
}
for (const [file, label] of [
  ["crates/aether-app/src/backup_auto.rs", "备份"],
  ["crates/aether-app/src/archive_control.rs", "归档/导出"],
  ["crates/aether-app/src/diagnostics_control.rs", "诊断导出"],
  ["crates/aether-app/src/disk.rs", "磁盘"],
]) {
  const text = read(file);
  expectMatch(text, /ensure_writable|free_space|available_bytes/, `${label}缺少可写/空间护栏（${file}）`);
}
expect(
  read("crates/aether-app/src/backup_auto.rs").includes("required_space_bytes"),
  "备份缺少 db+wal ×1.2 空间护栏",
);
expect(
  read("crates/aether-app/src/archive_control.rs").includes("required_space_bytes"),
  "归档导出缺少空间护栏",
);

// ===== 6. 密钥零泄漏 =====
const secretScan = runNode("scripts/test/m7-10/secret-scan.mjs", "密钥扫描");
expect(secretScan.includes("0 命中"), "密钥扫描未报告 0 命中");
const redact = read("apps/desktop/electron/redact.ts");
for (const pattern of [/sk-/, /api[_-]?key/i, /Authorization/, /redactEnv/]) {
  expectMatch(redact, pattern, "redact.ts 缺少密钥脱敏规则");
}
const offline = read("apps/desktop/electron/offlineDiagnostics.ts");
expectMatch(offline, /redactValue/, "离线诊断必须脱敏");
expectMatch(offline, /redactEnv/, "离线诊断环境变量必须脱敏");
const diagnostics = read("crates/aether-app/src/diagnostics_control.rs");
expectMatch(diagnostics, /Redactor::new\(\)/, "daemon 诊断包必须脱敏");
expectMatch(diagnostics, /is_clean/, "daemon 诊断包必须自检 0 泄漏");
const archive = read("crates/aether-app/src/archive_control.rs");
expectMatch(archive, /redactor\.redact_json/, "会话/归档导出必须脱敏");
const provider = read("crates/aether-app/src/provider_control.rs");
const providerJson =
  /fn provider_json[\s\S]*?\n}/.exec(provider)?.[0] ??
  /fn provider_view[\s\S]*?\n}/.exec(provider)?.[0] ??
  "";
expectNotMatch(providerJson, /"api_key"\s*:/, "provider 响应不得包含 api_key 字段");
expectMatch(providerJson, /api_key_ref/, "provider 响应应只返回 api_key_ref");

// DoD-6：Keychain preflight / S-13（M9-09 实现，M9-10 静态回归）。
const preflight = read("crates/aether-app/src/keychain_preflight.rs");
const preflightProduction = preflight.split("#[cfg(test)]")[0] ?? preflight;
expectMatch(preflightProduction, /store\.get\(/, "Keychain preflight 必须只读探测");
expectNotMatch(preflightProduction, /store\.delete/, "Keychain preflight 不得删除旧条目");
expectMatch(appSource, /showKeychainPrompt/, "main 缺少 S-13 页面");
expectMatch(
  read("apps/desktop/electron/keychain/keychain.html"),
  /keychain-reauth/,
  "S-13 缺少 keychain-reauth 锚点",
);

// ===== 7. SBOM / 许可证 =====
const sbom = read("scripts/test/m9-10/sbom.mjs");
for (const [label, pattern] of [
  ["Electron 版本", /runtime\.electron|versions\.electron/],
  ["Chromium 版本", /versions\.chrome/],
  ["Node 版本", /versions\.node/],
  ["license-checker", /license-checker|check:licenses/],
  ["cargo deny", /cargo deny/],
]) {
  expectMatch(sbom, pattern, `sbom.mjs 缺少${label}`);
}
const npmAllow = readJson("scripts/ci/npm-license-allowlist.json");
const npmAllowed = Array.isArray(npmAllow.allow) ? npmAllow.allow : [];
expect(
  npmAllowed.every((license) => !/^(A?GPL|BUSL)/i.test(license)),
  "npm 许可证白名单不得包含 GPL/AGPL/BUSL",
);
const deny = read("deny.toml");
const denyAllow = /\[licenses\][\s\S]*?allow = \[([\s\S]*?)\]/.exec(deny)?.[1] ?? "";
expectNotMatch(denyAllow, /"AGPL|"GPL|"BUSL/, "deny.toml 不得允许 GPL/AGPL/BUSL");
expectMatch(denyAllow, /"BSL-1\.0"/, "deny.toml 应保留 Boost BSL-1.0（非 Business Source）");

// ===== 8. 文档/证据 =====
const evidence = read("docs/M9-10-证据.md");
for (const needle of [
  "DoD-1",
  "DoD-2",
  "DoD-3",
  "DoD-4",
  "DoD-5",
  "DoD-6",
  "DoD-7",
  "未验证",
  "verify:security",
]) {
  expect(evidence.includes(needle), `证据缺少 ${needle}`);
}
expect(
  read("docs/P2-架构迁移-实施计划与验收标准.md").includes("### M9-10"),
  "P2 计划缺少 M9-10",
);

if (failures.length > 0) {
  console.error(`[verify:security] FAIL（${failures.length}/${checks} 项）`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(`[verify:security] PASS 检查=${checks}`);
console.log("AETHER_M9_10_SECURITY PASS");
process.exit(0);
