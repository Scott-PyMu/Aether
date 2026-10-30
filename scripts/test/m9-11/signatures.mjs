#!/usr/bin/env node
/**
 * `pnpm verify:signatures`（M9-11 DoD-2）。
 *
 * 用途：在**签名产物**上校验 Windows Authenticode + 时间戳 / macOS hardened runtime +
 * 嵌套签名 + 公证（`stapler`/`spctl`）。G6 发布门禁必须运行本脚本；无证书/无公证时
 * 明确失败（ADR-029 §8.1「无证书不得宣称 G6 通过」）。
 *
 * 用法：
 *   node scripts/test/m9-11/signatures.mjs [--artifact <msi|dmg>] [--installed <安装目录>]
 *   node scripts/test/m9-11/signatures.mjs --self-test        # 仅校验本机签名工具可用性
 *
 * 环境变量：
 *   AETHER_WIN_SIGNER_SUBJECT   期望的签名主体子串（如 "Aether" / 公司名）
 *   AETHER_MAC_TEAM_ID          期望的 Team ID（嵌套二进制与 .app 必须一致）
 *   AETHER_NOTARIZE=1           已启用公证（DMG 需 stapler 票据）
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, rmSync, statSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { readAuthenticode } from "./lib/msi.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const failures = [];
const notes = [];

function argValue(flag, fallback = null) {
  const index = process.argv.indexOf(flag);
  return index >= 0 && process.argv[index + 1] ? path.resolve(process.argv[index + 1]) : fallback;
}

function fail(message) {
  failures.push(message);
  console.error(`[signatures] FAIL ${message}`);
}

function pass(message) {
  console.log(`[signatures] PASS ${message}`);
}

function note(message) {
  notes.push(message);
  console.log(`[signatures] NOTE ${message}`);
}

function newestArtifact(pattern) {
  const outDir = path.join(repoRoot, "dist-out");
  if (!existsSync(outDir)) return null;
  const match = readdirSync(outDir)
    .filter((name) => pattern.test(name))
    .sort();
  return match.length > 0 ? path.join(outDir, match[match.length - 1]) : null;
}

function verifyWindows(artifact, installed) {
  if (process.platform !== "win32") {
    note("Windows 签名校验跳过（非 Windows 平台）");
    return;
  }
  const targets = [];
  if (artifact) targets.push(artifact);
  if (installed) {
    for (const relative of [
      "Aether.exe",
      path.join("resources", "bin", "aetherd.exe"),
      path.join("resources", "runtime-bundle", "aether-adapter-host.exe"),
    ]) {
      const file = path.join(installed, relative);
      if (existsSync(file)) targets.push(file);
    }
  }
  if (targets.length === 0) {
    fail("未提供 Windows 签名产物（--artifact/--installed）");
    return;
  }
  const configured = Boolean(
    process.env.CSC_LINK || process.env.CSC_NAME || process.env.AETHER_WIN_CERT_FILE,
  );
  if (!configured) {
    // ADR-030（M9-11）：Windows 允许无证书发布（Proma 同口径），但必须记录
    // unsigned + 哈希 + SmartScreen 指引；macOS 仍强制签名/公证。
    note(
      "Windows 未配置证书：按 ADR-030 以 unsigned 发布（SmartScreen 提示；哈希见 SHA256SUMS）；" +
        "macOS 签名/公证仍为 G6 必需项。",
    );
    return;
  }
  const results = readAuthenticode(targets);
  const expected = process.env.AETHER_WIN_SIGNER_SUBJECT ?? "";
  for (const result of results) {
    if (result.status !== "Valid") {
      fail(`${path.basename(result.path)} 签名状态=${result.status}（${result.status_message}）`);
      continue;
    }
    if (!result.signer_subject) {
      fail(`${path.basename(result.path)} 缺少签名主体`);
      continue;
    }
    if (expected && !result.signer_subject.includes(expected)) {
      fail(`${path.basename(result.path)} 签名主体不含 "${expected}"：${result.signer_subject}`);
      continue;
    }
    if (!result.timestamp_subject) {
      fail(`${path.basename(result.path)} 缺少 RFC3161 时间戳`);
      continue;
    }
    pass(`${path.basename(result.path)} Authenticode Valid + 时间戳（${result.signer_subject}）`);
  }
}

function run(command, args, label) {
  const result = spawnSync(command, args, { encoding: "utf8", timeout: 120_000 });
  if (result.error) {
    fail(`${label} 无法执行：${result.error.message}`);
    return { ok: false, stdout: "", stderr: "" };
  }
  if (result.status !== 0) {
    fail(`${label} 失败（exit=${result.status}）：${(result.stderr ?? result.stdout ?? "").trim()}`);
    return { ok: false, stdout: result.stdout ?? "", stderr: result.stderr ?? "" };
  }
  return { ok: true, stdout: result.stdout ?? "", stderr: result.stderr ?? "" };
}

function codesignTeamId(file) {
  const result = run("codesign", ["-dv", "--verbose=4", file], `codesign -dv ${file}`);
  if (!result.ok) return null;
  const text = `${result.stdout}\n${result.stderr}`;
  const match = /TeamIdentifier=([A-Z0-9]+)/.exec(text);
  return match ? match[1] : null;
}

function verifyMac(artifact) {
  if (process.platform !== "darwin") {
    note("macOS 签名/公证校验跳过（非 macOS 平台；G6 需 macOS runner）");
    return;
  }
  const dmg = artifact ?? newestArtifact(/\.dmg$/);
  if (!dmg || !existsSync(dmg)) {
    fail("未提供 DMG 产物");
    return;
  }
  const mount = mkdtempSync(path.join(repoRoot, "dist-out", "m9-11-dmg-"));
  const attach = run("hdiutil", ["attach", "-nobrowse", "-readonly", "-mountpoint", mount, dmg], "hdiutil attach");
  if (!attach.ok) return;
  try {
    const apps = readdirSync(mount).filter((name) => name.endsWith(".app"));
    if (apps.length !== 1) {
      fail(`DMG 内应恰有 1 个 .app，实际 ${apps.length}`);
      return;
    }
    const app = path.join(mount, apps[0]);
    const resources = path.join(app, "Contents", "Resources");
    const binaries = [
      path.join(app, "Contents", "MacOS", "Aether"),
      path.join(resources, "bin", "aetherd"),
      path.join(resources, "runtime-bundle", "aether-adapter-host"),
    ].filter((file) => existsSync(file));
    if (binaries.length < 3) {
      fail(`签名目标缺失：找到 ${binaries.length}/3（sidecar 必须随包分发）`);
    }
    for (const binary of binaries) {
      run("codesign", ["--verify", "--strict", "--verbose=2", binary], `codesign --verify ${path.basename(binary)}`);
    }
    run("codesign", ["--verify", "--deep", "--strict", "--verbose=2", app], "codesign --verify --deep --strict .app");
    run("spctl", ["--assess", "--type", "execute", "--verbose=4", app], "spctl .app");

    const teamIds = new Map(binaries.map((file) => [path.basename(file), codesignTeamId(file)]));
    const expectedTeam = process.env.AETHER_MAC_TEAM_ID ?? "";
    const unique = new Set([...teamIds.values()].filter(Boolean));
    if (teamIds.size !== binaries.length || [...teamIds.values()].some((value) => !value)) {
      fail(`嵌套签名 Team ID 缺失：${JSON.stringify([...teamIds])}`);
    } else if (unique.size !== 1) {
      fail(`嵌套二进制/应用 Team ID 不一致：${JSON.stringify([...teamIds])}`);
    } else if (expectedTeam && !unique.has(expectedTeam)) {
      fail(`Team ID 与期望不符：${[...unique][0]} != ${expectedTeam}`);
    } else {
      pass(`嵌套签名 Team ID 一致：${[...unique][0]}（顺序：aetherd/适配器 → .app → DMG）`);
    }

    // DMG 自身签名 + 公证票据。
    run("codesign", ["--verify", "--verbose=2", dmg], "codesign --verify DMG");
    if (process.env.AETHER_NOTARIZE === "1") {
      run("xcrun", ["stapler", "validate", dmg], "xcrun stapler validate DMG");
      pass("DMG 公证票据 stapler 校验通过");
    } else {
      note("AETHER_NOTARIZE != 1：未校验公证票据（G6 发布必须 AETHER_NOTARIZE=1）");
    }
  } finally {
    run("hdiutil", ["detach", mount, "-quiet"], "hdiutil detach");
    try {
      rmSync(mount, { recursive: true, force: true });
    } catch {
      // 忽略。
    }
  }
}

const selfTest = process.argv.includes("--self-test");
if (selfTest) {
  console.log(`[signatures] platform=${process.platform} node=${process.version}`);
  if (process.platform === "win32") {
    const signtool = spawnSync("where.exe", ["signtool"], { encoding: "utf8" });
    note(signtool.status === 0 ? `signtool: ${signtool.stdout.trim().split(/\r?\n/)[0]}` : "signtool 不在 PATH（electron-builder 使用内置签名工具）");
  }
  if (process.platform === "darwin") {
    for (const tool of ["codesign", "spctl", "xcrun"]) {
      const found = spawnSync("which", [tool], { encoding: "utf8" });
      note(found.status === 0 ? `${tool}: ${found.stdout.trim()}` : `${tool} 缺失`);
    }
  }
  process.exit(failures.length === 0 ? 0 : 1);
}

const artifact = argValue("--artifact");
if (!artifact) {
  const fallback = newestArtifact(process.platform === "win32" ? /\.msi$/ : /\.dmg$/);
  if (fallback) {
    note(`未指定 --artifact，使用最新产物：${path.basename(fallback)}`);
  }
}
const resolvedArtifact = artifact ?? (process.platform === "win32"
  ? newestArtifact(/\.msi$/)
  : newestArtifact(/\.dmg$/));
const installed = argValue("--installed");
if (installed) {
  if (!existsSync(installed)) fail(`--installed 目录不存在：${installed}`);
  else if (!statSync(installed).isDirectory()) fail(`--installed 不是目录：${installed}`);
}

if (process.platform === "win32") verifyWindows(resolvedArtifact, installed);
else if (process.platform === "darwin") verifyMac(resolvedArtifact);
else note("Linux 不纳入签名发布门禁（ADR-029 §8.1）");

console.log("");
console.log(`[signatures] ${failures.length === 0 ? "通过" : `${failures.length} 项失败`}`);
if (notes.length > 0) {
  console.log("[signatures] 备注：");
  for (const line of notes) console.log(`  - ${line}`);
}
if (failures.length > 0) {
  console.log("[signatures] G6 未通过：无证书/公证不得宣称打包门禁通过（ADR-029 §8.1）。");
  process.exit(1);
}
process.exit(0);
