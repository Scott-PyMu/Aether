/**
 * M9-13 性能证据：进程树 RSS 采样（Windows / Unix）。
 *
 * Windows 复用 M9-11 的 `measureProcessTree`（PowerShell/CIM）；Unix 用 `ps -eo
 * pid=,ppid=,rss=` 构建进程树（rss 为 KiB）。仅用于证据采样，不参与产品逻辑。
 */
import { spawnSync } from "node:child_process";

import { measureProcessTree as measureWindows } from "../../m9-11/lib/msi.mjs";

function measureUnix(rootPid) {
  const result = spawnSync("ps", ["-eo", "pid=,ppid=,rss="], { encoding: "utf8" });
  if (result.status !== 0) {
    throw new Error(`ps 失败：${result.stderr || result.status}`);
  }
  const rows = result.stdout
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const [pid, ppid, rss] = line.split(/\s+/).map((value) => Number(value));
      return { pid, ppid, rss_kib: rss };
    })
    .filter((row) => Number.isFinite(row.pid) && Number.isFinite(row.ppid));
  const byParent = new Map();
  for (const row of rows) {
    if (!byParent.has(row.ppid)) byParent.set(row.ppid, []);
    byParent.get(row.ppid).push(row);
  }
  const selected = [];
  const queue = [Number(rootPid)];
  const seen = new Set();
  while (queue.length > 0) {
    const pid = queue.shift();
    if (seen.has(pid)) continue;
    seen.add(pid);
    const row = rows.find((item) => item.pid === pid);
    if (row) selected.push(row);
    for (const child of byParent.get(pid) ?? []) queue.push(child.pid);
  }
  const totalBytes = selected.reduce((sum, row) => sum + row.rss_kib * 1024, 0);
  return {
    root_pid: Number(rootPid),
    total_bytes: totalBytes,
    total_mib: Math.round((totalBytes / 1048576) * 100) / 100,
    processes: selected.map((row) => ({
      pid: row.pid,
      parent: row.ppid,
      name: "",
      working_set: row.rss_kib * 1024,
    })),
  };
}

export function measureProcessTree(rootPid) {
  if (!Number.isInteger(rootPid) || rootPid <= 0) {
    throw new Error(`非法 root pid：${rootPid}`);
  }
  if (process.platform === "win32") return measureWindows(rootPid);
  return measureUnix(rootPid);
}
