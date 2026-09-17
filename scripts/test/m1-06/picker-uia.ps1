param(
  [Parameter(Mandatory = $true)][int]$AppPid,
  [Parameter(Mandatory = $true)][string]$Action,
  [string]$Path = "",
  [Parameter(Mandatory = $true)][string]$OutLog
)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class AetherPickerDlg {
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr GetDlgItem(IntPtr hDlg, int nIDDlgItem);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
  [DllImport("user32.dll")] public static extern IntPtr SetFocus(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern bool SetWindowTextW(IntPtr hWnd, string text);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessageW(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam);
}
"@
$BM_CLICK = [uint32]0x00F5
$report = @{ ok = $false; action = $Action; detail = "" }
try {
  $root = [System.Windows.Automation.AutomationElement]::RootElement
  $pidCondition = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ProcessIdProperty, $AppPid)
  $windows = $root.FindAll([System.Windows.Automation.TreeScope]::Children, $pidCondition)
  $dialog = $null
  foreach ($window in $windows) {
    if ($window.Current.ClassName -eq "#32770") { $dialog = $window; break }
  }
  if ($null -eq $dialog) { throw "file dialog window not found for pid $AppPid" }

  if ($Action -eq "dump") {
    $all = $dialog.FindAll(
      [System.Windows.Automation.TreeScope]::Descendants,
      [System.Windows.Automation.Condition]::TrueCondition)
    $elements = @()
    foreach ($element in $all) {
      $current = $element.Current
      $elements += @{
        type = $current.ControlType.ProgrammaticName
        id = $current.AutomationId
        name = $current.Name
        enabled = $current.IsEnabled
        className = $current.ClassName
      }
    }
    $elements | ConvertTo-Json -Depth 4 | Out-File -Encoding utf8 $OutLog
    $report.ok = $true
    $report.detail = "elements=$($elements.Count)"
    $report | ConvertTo-Json -Compress | Out-File -Encoding utf8 "$OutLog.summary"
    exit 0
  }

  $dialogHandle = [IntPtr]$dialog.Current.NativeWindowHandle
  if ($dialogHandle -eq [IntPtr]::Zero) { throw "dialog native handle unavailable" }

  $editHandle = [IntPtr]::Zero
  foreach ($editId in @(1152, 1148)) {
    $candidate = [AetherPickerDlg]::GetDlgItem($dialogHandle, $editId)
    if ($candidate -ne [IntPtr]::Zero) { $editHandle = $candidate; break }
  }

  if ($Action -eq "fill") {
    if ($editHandle -eq [IntPtr]::Zero) { throw "folder name edit control not found" }
    [void][AetherPickerDlg]::SetWindowTextW($editHandle, $Path)
    $report.ok = $true
    $report.editId = $editHandle.ToInt64()
    $report | ConvertTo-Json -Compress | Out-File -Encoding utf8 $OutLog
    exit 0
  }

  if ($Action -eq "select") {
    # 安全边界：仅当对话框确实位于前台时才注入键盘事件（避免误输入其他应用）。
    $foreground = [AetherPickerDlg]::GetForegroundWindow()
    if ($foreground -ne $dialogHandle) {
      [void][AetherPickerDlg]::ShowWindow($dialogHandle, 9) # SW_RESTORE
      [void][AetherPickerDlg]::SetForegroundWindow($dialogHandle)
      Start-Sleep -Milliseconds 500
      $foreground = [AetherPickerDlg]::GetForegroundWindow()
    }
    if ($foreground -ne $dialogHandle) {
      throw "dialog is not foreground; typing skipped for safety"
    }
    if ($editHandle -eq [IntPtr]::Zero) { throw "folder name edit control not found" }
    # 跨进程 SetFocus 需 AttachThreadInput。
    [uint32]$dialogPid = 0
    $dialogThread = [AetherPickerDlg]::GetWindowThreadProcessId($dialogHandle, [ref]$dialogPid)
    $currentThread = [AetherPickerDlg]::GetCurrentThreadId()
    [void][AetherPickerDlg]::AttachThreadInput($currentThread, $dialogThread, $true)
    [void][AetherPickerDlg]::SetFocus($editHandle)
    [void][AetherPickerDlg]::AttachThreadInput($currentThread, $dialogThread, $false)
    Set-Clipboard -Value $Path
    [System.Windows.Forms.SendKeys]::SendWait("^a")
    Start-Sleep -Milliseconds 150
    [System.Windows.Forms.SendKeys]::SendWait("^v")
    Start-Sleep -Milliseconds 600
    [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
    $report.ok = $true
    $report.method = "foreground-keys"
    $report.editId = $editHandle.ToInt64()
    $report.detail = "dialog=$($dialog.Current.Name)"
    $report | ConvertTo-Json -Compress | Out-File -Encoding utf8 $OutLog
    exit 0
  }

  # cancel（或 accept）
  $acceptId = if ($Action -eq "accept") { 1 } else { 2 }
  $buttonHandle = [AetherPickerDlg]::GetDlgItem($dialogHandle, $acceptId)
  if ($buttonHandle -eq [IntPtr]::Zero) { throw "dialog button $acceptId not found" }
  [void][AetherPickerDlg]::SendMessageW($buttonHandle, $BM_CLICK, [IntPtr]::Zero, [IntPtr]::Zero)
  $report.ok = $true
  $report.method = "win32-message"
  $report.detail = "dialog=$($dialog.Current.Name)"
} catch {
  $report.detail = $_.Exception.Message
}
$report | ConvertTo-Json -Compress | Out-File -Encoding utf8 $OutLog
if ($report.ok) { exit 0 } else { exit 1 }
