# Скриншот окна клиента (для отчётов о вёрстке): powershell -File scripts/shot.ps1 -Title Murmur -Out shot.png
# Окно ищется по части заголовка среди процессов murmur из target\debug (-PathLike — другой); снимок через PrintWindow (работает и под другими окнами).
param([string]$Title = "", [Parameter(Mandatory = $true)][string]$Out, [int]$WaitSec = 0, [string]$PathLike = "*\target\debug\*", [int]$ProcId = 0)
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Shot {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
"@
[Shot]::SetProcessDPIAware() | Out-Null
if ($WaitSec -gt 0) { Start-Sleep -Seconds $WaitSec }
$p = Get-Process murmur -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 -and $_.MainWindowTitle -like "*$Title*" -and $_.Path -like $PathLike -and ($ProcId -eq 0 -or $_.Id -eq $ProcId) } | Select-Object -First 1
if (-not $p) { Write-Error "окно не найдено"; exit 1 }
$r = New-Object Shot+RECT
[Shot]::GetWindowRect($p.MainWindowHandle, [ref]$r) | Out-Null
$w = $r.R - $r.L; $h = $r.B - $r.T
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
[Shot]::PrintWindow($p.MainWindowHandle, $hdc, 2) | Out-Null
$g.ReleaseHdc($hdc); $g.Dispose()
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
Write-Output "$Out $w x $h"
