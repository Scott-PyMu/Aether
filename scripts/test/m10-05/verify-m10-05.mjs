#!/usr/bin/env node
/**
 * `pnpm verify:m10-05` —— 多模态输入（ADR-035 v1.1）可复跑检查。
 *
 * 检查面：
 *  1. ADR-035 v1.1、迁移 0012（attachments/索引/provider_models.capabilities）；
 *  2. 命令面 +3（86/82/4；attachment 分组 3）；业务码 +4（35）；
 *  3. 核心限额/魔数/像素上限；app 内容寻址/原子写/24h 孤儿清理；
 *  4. 适配器 `multimodal` 能力声明；`session.send.attachments` 线协议；
 *  5. Electron UtilityProcess/协议/CSP/asarUnpack/真实文件 staging；
 *  6. UI attachment 锚点（picker/item/preview）与无障碍属性；
 *  7. Rust/前端测试、Q3 解码矩阵（26/26）、生成物 diff=0；
 *  8. 证据文档 DoD-1～7。
 */
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const checks = [];
const failures = [];

function check(name, condition, detail = "") {
  checks.push(name);
  if (!condition) failures.push(`${name}${detail ? `：${detail}` : ""}`);
}

function read(rel) {
  return readFileSync(path.join(root, rel), "utf8");
}

function readJson(rel) {
  return JSON.parse(read(rel));
}

function mustContain(rel, needles) {
  const text = read(rel);
  for (const needle of needles) {
    check(`${rel} 含 ${needle}`, text.includes(needle));
  }
}

function tool(name) {
  const override = process.env[`AETHER_${name.toUpperCase()}`];
  if (override) return override;
  if (name === "cargo") {
    const candidate = path.join(
      os.homedir(),
      ".rustup",
      "toolchains",
      "stable-x86_64-pc-windows-msvc",
      "bin",
      process.platform === "win32" ? "cargo.exe" : "cargo",
    );
    if (process.platform === "win32" && existsSync(candidate)) return candidate;
  }
  return name;
}

function run(label, command, args, { allowFailure = false } = {}) {
  console.log(`\n$ ${command} ${args.join(" ")}`);
  try {
    execFileSync(command, args, {
      cwd: root,
      stdio: "inherit",
      env: process.env,
      shell: process.platform === "win32",
    });
    checks.push(label);
  } catch (error) {
    if (allowFailure) {
      checks.push(`${label}（允许失败）`);
      return;
    }
    checks.push(label);
    failures.push(`${label}：退出码 ${error.status ?? error.message}`);
  }
}

// ---------- 1) ADR / 迁移 ----------
mustContain("docs/adr/ADR-035-m10-multimodal-attachments.md", [
  "已接受 v1.1",
  "10 MiB",
  "25,000,000",
  "attachments",
  "multimodal",
  "aether-attachment://thumb/<sha256>",
  "attachment_type_unsupported",
  "attachment_too_large",
  "attachment_limit_exceeded",
  "attachment_storage_failed",
]);
mustContain("migrations/0012_attachments.sql", [
  "CREATE TABLE attachments",
  "UNIQUE (session_id, sha256)",
  "idx_attachments_session",
  "provider_models ADD COLUMN capabilities",
]);
const migrationNames = readdirSync(path.join(root, "migrations")).filter((name) => name.endsWith(".sql"));
check("迁移 0001–0012 存在", migrationNames.includes("0012_attachments.sql"), migrationNames.join(","));

// ---------- 2) 命令面 / 业务码 ----------
const dispatch = read("crates/aether-app/src/ipc/dispatch.rs");
for (const command of ["attachment_add", "attachment_list", "attachment_remove"]) {
  check(`命令面含 ${command}`, dispatch.includes(`"${command}"`));
}
check("COMMANDS = 86", /COMMANDS\.len\(\),\s*86/.test(dispatch));
check("daemon = 82", /daemon\.len\(\),\s*82/.test(dispatch));

const manifest = readJson("packages/protocol/src/command-manifest.json");
check("command-manifest 86 条", manifest.counts?.total === 86, String(manifest.counts?.total));
check("command-manifest daemon=82", manifest.counts?.daemon === 82);
check("command-manifest shell=4", manifest.counts?.shell === 4);
check("command-manifest ui=84", manifest.counts?.ui === 84);
check(
  "attachment 分组 = 3",
  manifest.groups?.some((group) => group.group === "attachment" && group.count === 3),
);

const errorCodes = readJson("packages/protocol/src/error_codes.json");
check("business 码 = 35", errorCodes.business?.length === 35, String(errorCodes.business?.length));
for (const code of [
  "attachment_type_unsupported",
  "attachment_too_large",
  "attachment_limit_exceeded",
  "attachment_storage_failed",
]) {
  check(`业务码含 ${code}`, errorCodes.business?.some((entry) => entry.code === code));
}
mustContain("crates/aether-app/src/ipc/business.rs", [
  "AttachmentTypeUnsupported",
  "AttachmentTooLarge",
  "AttachmentLimitExceeded",
  "AttachmentStorageFailed",
]);

// ---------- 3) 核心 / app / 适配器 ----------
mustContain("crates/aether-core/src/attachments.rs", [
  "ATTACHMENT_MAX_BYTES: u64 = 10 * 1024 * 1024",
  "ATTACHMENT_MAX_PER_MESSAGE: usize = 8",
  "ATTACHMENT_MAX_PER_SESSION: usize = 200",
  "ATTACHMENT_MAX_SESSION_BYTES: u64 = 1024 * 1024 * 1024",
  "ATTACHMENT_MAX_PIXELS: u64 = 25_000_000",
  "ATTACHMENT_MAX_EDGE: u32 = 8192",
  "MULTIMODAL_CAPABILITY",
  "VISION_CAPABILITY",
  "sniff_image",
]);
mustContain("crates/aether-app/src/attachment.rs", [
  "ATTACHMENT_DIR: &str = \"attachments\"",
  "ATTACHMENT_ORPHAN_AGE",
  "store_content",
  "resolve_storage_path",
  "cleanup_orphans",
  "AttachmentReplayResolver",
  "sync_all",
  "rename",
]);
mustContain("crates/aether-control/src/lifecycle.rs", [
  "resolve_replay_attachments",
  "AttachmentRejected",
]);
mustContain("crates/aether-app/src/boot.rs", ["with_attachment_resolver"]);
mustContain("crates/aether-app/src/session_backend.rs", [
  "fn attachment_add",
  "fn attachment_list",
  "fn attachment_remove",
  "send_with_attachments",
  "AttachmentTypeUnsupported",
  "MULTIMODAL_MODEL_UNVERIFIED",
]);
mustContain("crates/aether-control/src/lifecycle.rs", [
  "send_with_attachments",
  "content_parts",
  "attachment_ids",
]);
mustContain("crates/aether-adapters/src/session_client.rs", ["send_with_attachments", "attachments"]);
for (const adapter of [
  "packages/adapter-claude-code/src/claude-adapter.ts",
  "packages/adapter-codex/src/codex-adapter.ts",
  "packages/adapter-dsh/src/dsh-adapter.ts",
]) {
  mustContain(adapter, ["multimodal"]);
}
mustContain("crates/aether-core/src/domain.rs", ["MULTIMODAL_CAPABILITY"]);

// ---------- 4) Electron / 打包 ----------
mustContain("apps/desktop/electron/attachmentProtocol.ts", [
  "aether-attachment",
  "SHA_PATTERN = /^[0-9a-f]{64}$/",
  "utilityProcess.fork",
  "DECODE_TIMEOUT_MS = 10_000",
  "MAX_WORKERS = 2",
  "attachments",
]);
mustContain("apps/desktop/electron/attachmentWorker.cjs", [
  "MAX_PIXELS = 25_000_000",
  "MAX_EDGE = 8192",
  "THUMB_EDGE = 512",
  "GifReader",
  "webp(80)",
]);
mustContain("apps/desktop/electron/security.ts", ["aether-attachment:"]);
mustContain("apps/desktop/electron/app.ts", [
  "registerAttachmentProtocol",
  "scheme: \"aether-attachment\"",
]);
mustContain("apps/desktop/electron/build.mjs", ["attachmentWorker.cjs"]);
mustContain("apps/desktop/electron-builder.config.mjs", [
  "apps/desktop/native-runtime/**/*",
  "apps/desktop/native-runtime/**",
]);
mustContain("scripts/ci/stage-native-runtime.mjs", [
  "@napi-rs/image",
  "omggif",
  "dereference: true",
  ".node",
]);

// ---------- 5) UI ----------
mustContain("apps/desktop/src/SessionWorkbench.tsx", [
  "attachment-picker",
  "attachment-item",
  "attachment-preview",
  "data-state={item.state}",
  "data-error-code={item.errorCode}",
  "runtimeSupportsMultimodal",
  "onComposerPaste",
  "run-retry-error",
  "data-business-code",
]);
mustContain("apps/desktop/src/attachments.ts", [
  "ATTACHMENT_MAX_BYTES",
  "ATTACHMENT_MAX_PER_MESSAGE",
  "ATTACHMENT_MIME_WHITELIST",
  "attachmentThumbUrl",
  "attachmentErrorMessage",
  "attachmentFailureHint",
  "MULTIMODAL_MODEL_UNVERIFIED",
]);
mustContain("apps/desktop/src/startup.ts", ["ipcBusinessCode"]);
mustContain("apps/desktop/src/session.ts", ["attachments?: string[]", "MessageContentPart"]);

// ---------- 6) 证据 ----------
check("证据文档存在", existsSync(path.join(root, "docs/M10-05-证据.md")));
if (existsSync(path.join(root, "docs/M10-05-证据.md"))) {
  mustContain("docs/M10-05-证据.md", [
    "DoD-1",
    "DoD-2",
    "DoD-3",
    "DoD-4",
    "DoD-5",
    "DoD-6",
    "DoD-7",
  ]);
}

// ---------- 7) 可执行验证 ----------
const cargo = tool("cargo");
run("core 附件单测", cargo, ["test", "-p", "aether-core", "--lib", "attachments", "-j", "1", "--", "--test-threads=1"]);
run("store 附件集成", cargo, ["test", "-p", "aether-store", "--test", "m10_05_attachments", "-j", "1", "--", "--test-threads=1"]);
run("control 附件集成", cargo, ["test", "-p", "aether-control", "--test", "m10_05_attachments", "-j", "1", "--", "--test-threads=1"]);
run("app 附件集成", cargo, ["test", "-p", "aether-app", "--test", "m10_05_attachments", "-j", "1", "--", "--test-threads=1"]);
run("daemon 附件 E2E", cargo, ["test", "-p", "aether-daemon", "--test", "m10_05_attachments", "-j", "1", "--", "--test-threads=1"]);
run("UI 附件测试", "pnpm", ["--filter", "@aether/desktop", "test", "src/m10_05_attachments.test.tsx"]);
run("Q3 解码矩阵 26/26", "node", ["scripts/test/m10-05/decode-spike.mjs"]);
run("native 真实文件 staging", "node", ["scripts/ci/stage-native-runtime.mjs"]);
run("worker 夹具", "node", ["scripts/test/m10-05/make-worker-fixture.mjs"]);
run("worker 缩略图 smoke", "node", [
  "apps/desktop/electron/attachmentWorker.cjs",
  "scripts/test/m10-05/.tmp/worker-fixture.png",
  "scripts/test/m10-05/.tmp/worker-thumb.webp",
]);
run("worker 真实四格式矩阵", "node", ["scripts/test/m10-05/worker-format-matrix.mjs"]);
{
  const thumb = path.join(root, "scripts/test/m10-05/.tmp/worker-thumb.webp");
  const magic = existsSync(thumb) ? readFileSync(thumb).subarray(0, 12).toString("latin1") : "";
  check("worker 输出 WebP（RIFF/WEBP）", magic.startsWith("RIFF") && magic.includes("WEBP"), magic);
}
run("command-manifest diff=0", "node", ["scripts/ci/command-manifest.mjs", "check"]);
run("error-codes diff=0", "node", ["scripts/ci/error-codes.mjs", "check"]);
run("protocol-client diff=0", "node", ["scripts/ci/protocol-client.mjs", "check"]);
run("api-schema diff=0", "node", ["scripts/ci/api-schema.mjs", "check"]);
run("M9-01 基线", "node", ["scripts/test/verify-m9-01-baseline.mjs"]);

const passed = checks.length - failures.length;
if (failures.length > 0) {
  console.error(`\n[M10-05] FAIL：${failures.length} 项失败`);
  for (const failure of failures) console.error(` - ${failure}`);
  process.exit(1);
}
console.log(
  `\n[m10-05] OK：${passed}/${checks.length} 项通过（ADR-035 v1.1；86 命令 / 35 业务码；附件内容寻址/UtilityProcess/能力门）`,
);
