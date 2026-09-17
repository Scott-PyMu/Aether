param(
  [Parameter(Mandatory = $true)][string]$OutPath
)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class AetherPickerWin32 {
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr hWnd, StringBuilder text, int count);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
"@
$hwnd = [AetherPickerWin32]::GetForegroundWindow()
if ($hwnd -eq [IntPtr]::Zero) { Write-Error "no foreground window"; exit 2 }
$rect = New-Object AetherPickerWin32+RECT
[void][AetherPickerWin32]::GetWindowRect($hwnd, [ref]$rect)
$width = $rect.Right - $rect.Left
$height = $rect.Bottom - $rect.Top
if ($width -le 0 -or $height -le 0) { Write-Error "invalid window rect $width x $height"; exit 3 }
$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, (New-Object System.Drawing.Size $width, $height))
$bitmap.Save($OutPath, [System.Drawing.Imaging.ImageFormat]::Png)
$graphics.Dispose()
$bitmap.Dispose()
[uint32]$windowPid = 0
[void][AetherPickerWin32]::GetWindowThreadProcessId($hwnd, [ref]$windowPid)
Write-Output "captured=$OutPath size=${width}x${height} pid=$windowPid"
