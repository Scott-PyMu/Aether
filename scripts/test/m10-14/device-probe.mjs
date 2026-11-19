#!/usr/bin/env node
/**
 * M10-14 设备探测证据（Windows）：
 *  - PowerShell `Get-PhysicalDisk` 记录真实 BusType/MediaType；
 *  - `m10_14_device_probe` 记录 `sysinfo` 自动分类与 `AETHER_STORAGE_DEVICE=nvme` 覆盖；
 *  - 输出 `docs/M10-14-device-probe.json`。
 *
 * 目的：明确 Windows NVMe BusType 自动识别缺口，并验证显式覆盖可用（不是静默跳过）。
 * 非 Windows：记录 skip 原因；由 CI macOS/Linux job 覆盖平台可用性。
 */
import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const outPath = path.join(root, "docs", "M10-14-device-probe.json");
mkdirSync(path.dirname(outPath), { recursive: true });

const result = {
  schema: "aether.m10-14.device-probe.v1",
  task: "M10-14",
  recorded_at: new Date().toISOString(),
  platform: { os: process.platform, arch: process.arch },
  physical_disks: [],
  rust_probe: null,
  override: null,
  notes: [],
  passed: false,
};

function run(command, args, options = {}) {
  return execFileSync(command, args, {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 10 * 60 * 1000,
    ...options,
  });
}

try {
  if (process.platform === "win32") {
    const json = run("powershell.exe", [
      "-NoProfile",
      "-NonInteractive",
      "-Command",
      "Get-PhysicalDisk | Select-Object FriendlyName,MediaType,BusType,Size | ConvertTo-Json -Compress",
    ]).trim();
    const parsed = JSON.parse(json);
    result.physical_disks = Array.isArray(parsed) ? parsed : [parsed];
    result.notes.push("BusType 来自 Get-PhysicalDisk；sysinfo 不暴露 BusType，NVMe 可能归为 SSD。");
  } else {
    result.notes.push("非 Windows：Get-PhysicalDisk 不适用；平台功能由 CI macOS/Linux job 覆盖。");
  }
} catch (error) {
  result.notes.push(`Get-PhysicalDisk 失败：${error.message}`);
}

try {
  const output = run("cargo", [
    "test",
    "-p",
    "aether-app",
    "--test",
    "m10_14_device_probe",
    "-j",
    "1",
    "--",
    "--nocapture",
    "--test-threads=1",
  ]);
  const lines = output
    .split(/\r?\n/)
    .filter((line) => line.includes("M10_14_DEVICE_PROBE"));
  result.rust_probe = lines;
  const autoLine = lines.find((line) => line.includes("auto=")) ?? "";
  const overrideLine = lines.find((line) => line.includes("override=")) ?? "";
  const autoMatch = /auto=StartupCheckProfile \{ device: (\w+), concurrency: (\d+), source: "([^"]+)"/.exec(
    autoLine,
  );
  const overrideMatch =
    /override=StartupCheckProfile \{ device: (\w+), concurrency: (\d+), source: "([^"]+)"/.exec(
      overrideLine,
    );
  if (autoMatch) {
    result.auto = {
      device: autoMatch[1].toLowerCase(),
      concurrency: Number(autoMatch[2]),
      source: autoMatch[3],
    };
  }
  if (overrideMatch) {
    result.override = {
      device: overrideMatch[1].toLowerCase(),
      concurrency: Number(overrideMatch[2]),
      source: overrideMatch[3],
    };
  }
} catch (error) {
  result.notes.push(`m10_14_device_probe 失败：${error.message}`);
}

const hasNvme =
  process.platform !== "win32" ||
  result.physical_disks.some((disk) => String(disk.BusType).toLowerCase() === "nvme");
const autoMayUnderDetect =
  process.platform !== "win32" || result.auto?.device === "ssd" || result.auto?.device === "unknown";
result.passed =
  hasNvme &&
  Boolean(result.auto) &&
  result.override?.device === "nvme" &&
  result.override?.concurrency === 8 &&
  result.override?.source === "env:device" &&
  autoMayUnderDetect;

writeFileSync(outPath, `${JSON.stringify(result, null, 2)}\n`, "utf8");
console.log(JSON.stringify(result, null, 2));
console.log(`[m10-14-device-probe] 证据：${path.relative(root, outPath)}`);
if (!result.passed) {
  console.error("[m10-14-device-probe] FAIL");
  process.exit(1);
}
console.log("AETHER_M10_14_DEVICE_PROBE PASS");
