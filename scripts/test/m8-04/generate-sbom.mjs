#!/usr/bin/env node
/**
 * M8-04 SBOM 与产物哈希归档。
 *
 * 产出（scripts/test/.tmp/m8-04/sbom-<stamp>/）：
 * - sbom.json：Cargo workspace 包清单（name/version/license/source）+ Node workspace 包清单
 *   + Cargo.lock / pnpm-lock.yaml 的 SHA256；
 * - SHA256SUMS.txt：安装产物（dist-out/**\/*.msi、*.dmg、*.exe）与关键二进制哈希。
 *
 * 说明：本清单为发布归档用依赖 inventory（非完整 CycloneDX）；许可证合规由
 * `cargo deny check` / `deny.toml` 白名单门禁承担。
 */
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

import { bin, repoRoot } from "../lib/exec.mjs";

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const outDir = path.join(repoRoot, "scripts", "test", ".tmp", "m8-04", `sbom-${stamp}`);
mkdirSync(outDir, { recursive: true });

function sha256(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

function cargoPackages() {
  const result = spawnSync(
    bin("cargo"),
    ["metadata", "--format-version", "1", "--no-deps"],
    { cwd: repoRoot, encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
  );
  if (result.status !== 0) {
    throw new Error(`cargo metadata 失败：${result.stderr ?? ""}`);
  }
  const metadata = JSON.parse(result.stdout);
  return metadata.packages.map((pkg) => ({
    name: pkg.name,
    version: pkg.version,
    license: pkg.license ?? null,
    source: pkg.source ?? "workspace",
  }));
}

function nodePackages() {
  const packages = [];
  const roots = ["apps/desktop", "packages/protocol", "packages/adapter-sdk", "packages/adapter-mock",
    "packages/adapter-claude-code", "packages/adapter-codex", "packages/adapter-dsh"];
  for (const root of roots) {
    const manifest = path.join(repoRoot, root, "package.json");
    if (!existsSync(manifest)) continue;
    const json = JSON.parse(readFileSync(manifest, "utf8"));
    packages.push({
      name: json.name,
      version: json.version,
      license: json.license ?? null,
      path: root,
    });
  }
  return packages;
}

const sbom = {
  format: "aether-sbom-1",
  generated_at: new Date().toISOString(),
  platform: process.platform,
  cargo_packages: cargoPackages(),
  node_packages: nodePackages(),
  lockfiles: [
    ["Cargo.lock", existsSync(path.join(repoRoot, "Cargo.lock")) ? sha256(path.join(repoRoot, "Cargo.lock")) : null],
    ["pnpm-lock.yaml", existsSync(path.join(repoRoot, "pnpm-lock.yaml")) ? sha256(path.join(repoRoot, "pnpm-lock.yaml")) : null],
  ],
};
writeFileSync(path.join(outDir, "sbom.json"), `${JSON.stringify(sbom, null, 2)}\n`, "utf8");

const hashTargets = [];
const artifactRoot = path.join(repoRoot, "dist-out");
if (existsSync(artifactRoot)) {
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (/\.(msi|dmg|exe|appimage|deb|rpm)$/i.test(entry.name)) hashTargets.push(full);
    }
  };
  walk(artifactRoot);
}
for (const candidate of ["target/debug/aether-tauri.exe", "target/release/aether-tauri.exe"]) {
  const full = path.join(repoRoot, candidate);
  if (existsSync(full)) hashTargets.push(full);
}

const lines = hashTargets.map((file) => `${sha256(file)}  ${path.relative(repoRoot, file).replaceAll("\\", "/")}`);
writeFileSync(path.join(outDir, "SHA256SUMS.txt"), `${lines.join("\n")}\n`, "utf8");

console.log(`[m8-04] SBOM：${path.relative(repoRoot, outDir)}`);
console.log(`[m8-04] Cargo packages=${sbom.cargo_packages.length} Node packages=${sbom.node_packages.length}`);
console.log(`[m8-04] hashed artifacts=${lines.length}`);
