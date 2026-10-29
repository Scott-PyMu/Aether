#!/usr/bin/env node
/**
 * M9-10 DoD-7：SBOM 快照（Electron/Chromium/Node + Rust/npm workspace）。
 *
 * 输出 `scripts/test/.tmp/m9-10/sbom/sbom.json`：
 * - runtime：Electron/Chromium/Node/V8 版本（`ELECTRON_RUN_AS_NODE=1` 读取）；
 * - cargo_packages：`cargo metadata --no-deps`；
 * - npm_packages：workspace 包 + 直接依赖（name/version/license）；
 * - lockfiles：Cargo.lock / pnpm-lock.yaml SHA256。
 *
 * 许可证合规由 `pnpm check:licenses`（license-checker）与 `cargo deny check`
 * 独立门禁承担；本脚本只做 inventory 与版本归档（ADR-029 §5.5）。
 */
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { bin, repoRoot } from "../lib/exec.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const outDir = path.join(root, "scripts", "test", ".tmp", "m9-10", "sbom");
mkdirSync(outDir, { recursive: true });

function sha256(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

function runtimeVersions() {
  const requireFromDesktop = createRequire(
    path.join(root, "apps", "desktop", "package.json"),
  );
  const electronPath = requireFromDesktop("electron");
  const result = spawnSync(
    electronPath,
    ["-p", "JSON.stringify(process.versions)"],
    {
      env: { ...process.env, ELECTRON_RUN_AS_NODE: "1" },
      encoding: "utf8",
      timeout: 30_000,
    },
  );
  if (result.status !== 0 || !result.stdout) {
    throw new Error(
      `读取 Electron runtime 版本失败：${result.stderr || result.status}`,
    );
  }
  const versions = JSON.parse(result.stdout.trim());
  return {
    electron: versions.electron ?? null,
    chrome: versions.chrome ?? null,
    node: versions.node ?? null,
    v8: versions.v8 ?? null,
    modules: versions.modules ?? null,
    platform: process.platform,
    arch: process.arch,
  };
}

function cargoPackages() {
  const result = spawnSync(
    bin("cargo"),
    ["metadata", "--format-version", "1", "--no-deps"],
    { cwd: root, encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
  );
  if (result.status !== 0) {
    throw new Error(`cargo metadata 失败：${result.stderr ?? ""}`);
  }
  return JSON.parse(result.stdout).packages.map((pkg) => ({
    name: pkg.name,
    version: pkg.version,
    license: pkg.license ?? null,
    source: pkg.source ?? "workspace",
  }));
}

function npmPackages() {
  const packages = [];
  const roots = [
    "apps/desktop",
    "packages/protocol",
    "packages/adapter-sdk",
    "packages/adapter-mock",
    "packages/adapter-claude-code",
    "packages/adapter-codex",
    "packages/adapter-dsh",
  ];
  const seen = new Set();
  for (const relative of roots) {
    const manifestPath = path.join(root, relative, "package.json");
    if (!existsSync(manifestPath)) continue;
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    const deps = {
      ...(manifest.dependencies ?? {}),
      ...(manifest.devDependencies ?? {}),
    };
    for (const [name, version] of Object.entries(deps)) {
      const key = `${name}@${version}`;
      if (seen.has(key)) continue;
      seen.add(key);
      packages.push({ name, version, license: null, workspace: manifest.name });
    }
  }
  return packages;
}

const runtime = runtimeVersions();
const sbom = {
  format: "aether-sbom-1",
  generated_at: new Date().toISOString(),
  runtime,
  cargo_packages: cargoPackages(),
  npm_packages: npmPackages(),
  lockfiles: [
    ["Cargo.lock", existsSync(path.join(root, "Cargo.lock")) ? sha256(path.join(root, "Cargo.lock")) : null],
    ["pnpm-lock.yaml", existsSync(path.join(root, "pnpm-lock.yaml")) ? sha256(path.join(root, "pnpm-lock.yaml")) : null],
  ],
  licenses: {
    npm_gate: "pnpm check:licenses（license-checker + scripts/ci/npm-license-allowlist.json）",
    rust_gate: "cargo deny check（deny.toml）",
    forbidden: ["GPL", "AGPL", "BUSL"],
  },
};
writeFileSync(
  path.join(outDir, "sbom.json"),
  `${JSON.stringify(sbom, null, 2)}\n`,
  "utf8",
);

console.log(
  `[m9-10-sbom] Electron=${runtime.electron} Chromium=${runtime.chrome} Node=${runtime.node} V8=${runtime.v8}`,
);
console.log(
  `[m9-10-sbom] cargo=${sbom.cargo_packages.length} npm(直接)=${sbom.npm_packages.length}`,
);
console.log(`[m9-10-sbom] ${path.relative(root, path.join(outDir, "sbom.json"))}`);
