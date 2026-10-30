/**
 * Windows 安装/签名/RSS 辅助（M9-11 冒烟与校验）。
 *
 * 依赖 Windows 自带的 PowerShell + Windows Installer COM（无需额外工具）：
 *  - MSI `Property` / `Upgrade` / `RemoveFile` / `Directory` 表读取（升级链与数据保留断言）；
 *  - 已安装 Aether 产品枚举（拦截「双安装」）；
 *  - 进程树 RSS（`idle RSS ≤500MB` / `10 会话 ≤1.2GB` 证据）；
 *  - Authenticode 签名与时间戳（DoD-2）。
 *
 * 非 Windows 平台调用即抛错；调用方必须先判平台。
 */
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const POWERSHELL = process.env.SystemRoot
  ? `${process.env.SystemRoot}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe`
  : "powershell.exe";

function runPowerShell(script, env = {}) {
  // 用临时 .ps1 + `-File` 执行：`-Command -` 对多行 `while`/COM 记录循环存在
  // 解析/静默终止差异（2026-10-30 实测），文件模式行为稳定。
  const dir = mkdtempSync(path.join(os.tmpdir(), "aether-m9-11-ps-"));
  const file = path.join(dir, "script.ps1");
  writeFileSync(file, `[Console]::OutputEncoding = [System.Text.Encoding]::UTF8\n${script}\n`, "utf8");
  try {
    const result = spawnSync(
      POWERSHELL,
      ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", file],
      {
        encoding: "utf8",
        env: { ...process.env, ...env },
        timeout: 120_000,
        windowsHide: true,
      },
    );
    if (result.error) throw new Error(`PowerShell 无法执行：${result.error.message}`);
    if (result.status !== 0) {
      throw new Error(`PowerShell 失败（exit=${result.status}）：${(result.stderr ?? "").trim()}`);
    }
    return (result.stdout ?? "").trim();
  } finally {
    try {
      rmSync(dir, { recursive: true, force: true });
    } catch {
      // 忽略临时文件清理失败。
    }
  }
}

function parseJson(text, label) {
  if (text.length === 0) return null;
  try {
    return JSON.parse(text);
  } catch (error) {
    throw new Error(`${label} 输出不是 JSON：${text.slice(0, 400)}（${error.message}）`);
  }
}

function psQuote(value) {
  return `'${String(value).replace(/'/g, "''")}'`;
}

/** 读取 MSI 表（列名由调用方给出；按表名白名单避免注入）。 */
export function readMsiTable(msiPath, table, columns) {
  const allowed = new Set(["Property", "Upgrade", "RemoveFile", "Directory", "Component", "File"]);
  if (!allowed.has(table)) throw new Error(`不允许读取 MSI 表：${table}`);
  const select = columns.join(", ");
  const script = `
$ErrorActionPreference = 'Stop'
$installer = New-Object -ComObject WindowsInstaller.Installer
$db = $installer.OpenDatabase(${psQuote(msiPath)}, 0)
$view = $null
try { $view = $db.OpenView(${psQuote(`SELECT ${select} FROM ${table}`)}) } catch { '[]'; exit 0 }
if ($null -eq $view) { '[]'; exit 0 }
$view.Execute()
$rows = @()
while ($true) {
  $record = $view.Fetch()
  if ($null -eq $record) { break }
  $row = [ordered]@{}
  ${columns
    .map((column, index) => `$row['${column}'] = $record.StringData(${index + 1})`)
    .join("\n  ")}
  $rows += [pscustomobject]$row
}
$view.Close()
if ($rows.Count -eq 0) { '[]' } else { ConvertTo-Json -InputObject @($rows) -Compress -Depth 5 }
`;
  const parsed = parseJson(runPowerShell(script), `MSI ${table}`);
  return Array.isArray(parsed) ? parsed : [];
}

/** MSI Property 表 → 对象。 */
export function readMsiProperties(msiPath) {
  const rows = readMsiTable(msiPath, "Property", ["Property", "Value"]);
  const properties = {};
  for (const row of rows) properties[row.Property] = row.Value;
  return properties;
}

/** 已安装的 Aether 产品（拦截旧 Tauri 包并存/双安装）。
 *
 * MSI 的 `WindowsInstaller.Installer.Products` 在部分主机（HKLM per-machine 安装）返回空；
 * 注册表 Uninstall 键（HKLM/HKCU × 32/64 位视图）是稳定事实源。
 */
export function listInstalledAetherProducts() {
  const script = `
$ErrorActionPreference = 'Stop'
$paths = @(
  'HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall',
  'HKLM:\\Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall',
  'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall',
  'HKCU:\\Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall'
)
$items = @()
foreach ($root in $paths) {
  if (-not (Test-Path $root)) { continue }
  foreach ($key in @(Get-ChildItem $root -ErrorAction SilentlyContinue)) {
    try {
      $props = Get-ItemProperty $key.PSPath -ErrorAction Stop
      $name = [string]$props.DisplayName
      if ($name -like '*Aether*') {
        $items += [pscustomobject][ordered]@{
          product_code = [string]$key.PSChildName
          name = $name
          version = [string]$props.DisplayVersion
          install_location = [string]$props.InstallLocation
          uninstall = [string]$props.UninstallString
          registry = [string]$key.PSPath
        }
      }
    } catch { }
  }
}
if ($items.Count -eq 0) { '[]' } else { ConvertTo-Json -InputObject @($items) -Compress -Depth 4 }
`;
  const parsed = parseJson(runPowerShell(script), "已安装产品");
  return Array.isArray(parsed) ? parsed : [];
}

/** 进程树（含 root 自身）RSS/工作集汇总（字节）。 */
export function measureProcessTree(rootPid) {
  const script = `
$ErrorActionPreference = 'Stop'
$root = [int]$env:AETHER_RSS_ROOT
$procs = @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, WorkingSetSize, Name)
$ids = New-Object 'System.Collections.Generic.HashSet[int]'
[void]$ids.Add($root)
$changed = $true
while ($changed) {
  $changed = $false
  foreach ($p in $procs) {
    if ($ids.Contains([int]$p.ParentProcessId) -and -not $ids.Contains([int]$p.ProcessId)) {
      [void]$ids.Add([int]$p.ProcessId)
      $changed = $true
    }
  }
}
$selected = @($procs | Where-Object { $ids.Contains([int]$_.ProcessId) })
$total = 0
$items = @()
foreach ($p in $selected) {
  $total += [int64]$p.WorkingSetSize
  $items += [pscustomobject][ordered]@{
    pid = [int]$p.ProcessId
    parent = [int]$p.ParentProcessId
    name = [string]$p.Name
    working_set = [int64]$p.WorkingSetSize
  }
}
[pscustomobject][ordered]@{
  root_pid = $root
  total_bytes = $total
  total_mib = [math]::Round($total / 1048576, 2)
  processes = @($items)
} | ConvertTo-Json -Compress -Depth 5
`;
  return parseJson(runPowerShell(script, { AETHER_RSS_ROOT: String(rootPid) }), "RSS");
}

/** 向应用主窗口发送 WM_CLOSE（优雅退出）；返回是否已投递。 */
export function closeMainWindow(pid) {
  const script = `
$ErrorActionPreference = 'Stop'
$target = Get-Process -Id ([int]$env:AETHER_APP_PID) -ErrorAction SilentlyContinue
if ($null -eq $target) { '{"exists":false,"closed":false}' ; exit 0 }
$closed = $target.CloseMainWindow()
[pscustomobject][ordered]@{ exists = $true; closed = [bool]$closed } | ConvertTo-Json -Compress
`;
  return parseJson(runPowerShell(script, { AETHER_APP_PID: String(pid) }), "关闭窗口");
}

/** Authenticode 签名/时间戳（DoD-2）。 */
export function readAuthenticode(paths) {
  const script = `
$ErrorActionPreference = 'Stop'
$paths = $env:AETHER_SIGN_PATHS | ConvertFrom-Json
$items = @()
foreach ($path in @($paths)) {
  $sig = Get-AuthenticodeSignature -FilePath $path
  $signer = $null
  $timestamp = $null
  if ($null -ne $sig.SignerCertificate) { $signer = $sig.SignerCertificate.Subject }
  if ($null -ne $sig.TimeStamperCertificate) { $timestamp = $sig.TimeStamperCertificate.Subject }
  $items += [pscustomobject][ordered]@{
    path = [string]$path
    status = [string]$sig.Status
    status_message = [string]$sig.StatusMessage
    signer_subject = $signer
    timestamp_subject = $timestamp
  }
}
ConvertTo-Json -InputObject @($items) -Compress -Depth 4
`;
  const parsed = parseJson(
    runPowerShell(script, { AETHER_SIGN_PATHS: JSON.stringify(paths) }),
    "Authenticode",
  );
  return Array.isArray(parsed) ? parsed : [];
}
