/**
 * 验证「供应链门禁可阻断」（M1-01 DoD2）。
 *
 * 步骤：
 *   1) 基线：cargo-deny（licenses/bans/sources）、相似度扫描、npm 许可证检查均通过；
 *   2) 注入一个 GPL-3.0-only 依赖（本地 path 包模拟）→ cargo-deny 必须失败；
 *   3) 注入一个模拟相似度告警标记 → similarity-scan 必须失败；
 *   4) 还原并复验基线（防止注入未清理）。
 *
 * 说明：
 *   - cargo-deny 的 advisories 检查依赖 GitHub 上的 rustsec 数据库，在本机网络不可达时
 *     无法运行；CI（GitHub runner）执行完整 `cargo deny check`。本脚本验证 licenses 门禁
 *     对注入依赖的阻断行为（DoD2 的核心）。
 *   - 相似度标记由片段拼接生成，避免扫描脚本自身命中。
 */
import { copyFileSync, mkdirSync, readFileSync, rmSync, writeFileSync, unlinkSync, existsSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { bin, repoRoot, run, summarize } from "./lib/exec.mjs";

const cargo = bin("cargo");
const node = process.execPath;

const tmpDir = path.join(repoRoot, "scripts", "test", ".tmp", "supply-chain-gate");
const storeManifest = path.join(repoRoot, "crates", "aether-store", "Cargo.toml");
const lockFile = path.join(repoRoot, "Cargo.lock");
const injectionCrate = path.join(tmpDir, "gpl-injection");
const similarityInjectionFile = path.join(
  repoRoot,
  "packages",
  "protocol",
  "src",
  "__similarity_injection__.ts",
);

const similarityMarker = ["AETHER", "SIMILARITY", "TEST", "INJECTION"].join("_");
const denyLicenses = ["deny", "check", "licenses", "bans", "sources"];

function backupFiles() {
  mkdirSync(tmpDir, { recursive: true });
  copyFileSync(storeManifest, path.join(tmpDir, "aether-store.Cargo.toml.bak"));
  if (existsSync(lockFile)) copyFileSync(lockFile, path.join(tmpDir, "Cargo.lock.bak"));
}

function restoreFiles() {
  copyFileSync(path.join(tmpDir, "aether-store.Cargo.toml.bak"), storeManifest);
  if (existsSync(path.join(tmpDir, "Cargo.lock.bak"))) {
    copyFileSync(path.join(tmpDir, "Cargo.lock.bak"), lockFile);
  }
  if (existsSync(similarityInjectionFile)) unlinkSync(similarityInjectionFile);
  rmSync(injectionCrate, { recursive: true, force: true });
}

function injectGplDependency() {
  mkdirSync(path.join(injectionCrate, "src"), { recursive: true });
  writeFileSync(
    path.join(injectionCrate, "Cargo.toml"),
    [
      "[package]",
      'name = "gpl-injection"',
      'version = "0.0.0"',
      'edition = "2021"',
      'license = "GPL-3.0-only"',
      "",
      "[dependencies]",
      "",
    ].join("\n"),
    "utf8",
  );
  writeFileSync(path.join(injectionCrate, "src", "lib.rs"), "// GPL 注入夹具（验证用）\n", "utf8");

  const original = readFileSync(storeManifest, "utf8");
  const injected = original.replace(
    "aether-core.workspace = true",
    'aether-core.workspace = true\ngpl-injection = { path = "../../scripts/test/.tmp/supply-chain-gate/gpl-injection" }',
  );
  if (injected === original) {
    throw new Error("注入失败：aether-store/Cargo.toml 中未找到锚点行");
  }
  writeFileSync(storeManifest, injected, "utf8");
}

function injectSimilarityMarker() {
  writeFileSync(
    similarityInjectionFile,
    `// 相似度门禁注入夹具（验证用）：${similarityMarker}\nexport const injected = true;\n`,
    "utf8",
  );
}

const checks = [];
backupFiles();
try {
  checks.push({
    name: "基线：cargo deny check licenses/bans/sources",
    expect: 0,
    exit: run(cargo, denyLicenses),
  });
  checks.push({
    name: "基线：相似度扫描",
    expect: 0,
    exit: run(node, [path.join(repoRoot, "scripts", "ci", "similarity-scan.mjs"), "--root", "."]),
  });
  checks.push({
    name: "基线：npm 许可证检查",
    expect: 0,
    exit: run(node, [path.join(repoRoot, "scripts", "ci", "check-npm-licenses.mjs")]),
  });

  injectGplDependency();
  checks.push({
    name: "注入 GPL-3.0-only 依赖后：cargo deny 必须阻断（非零）",
    expect: "nonzero",
    exit: run(cargo, denyLicenses),
  });
  restoreFiles();

  checks.push({
    name: "还原 GPL 注入后：cargo deny 恢复通过",
    expect: 0,
    exit: run(cargo, denyLicenses),
  });

  injectSimilarityMarker();
  checks.push({
    name: "注入模拟相似度告警后：similarity-scan 必须阻断（非零）",
    expect: "nonzero",
    exit: run(node, [path.join(repoRoot, "scripts", "ci", "similarity-scan.mjs"), "--root", "."]),
  });
  unlinkSync(similarityInjectionFile);

  checks.push({
    name: "移除相似度注入后：similarity-scan 恢复通过",
    expect: 0,
    exit: run(node, [path.join(repoRoot, "scripts", "ci", "similarity-scan.mjs"), "--root", "."]),
  });
} finally {
  restoreFiles();
}

process.exit(summarize("verify-supply-chain-gate", checks));
