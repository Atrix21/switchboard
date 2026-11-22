# Captures README images from the app's demo mode (no real hardware involved).
#   capture.ps1 -Out docs\screenshots\desk.png -View flat [-Open settings] [-Wait 4]
#   capture.ps1 -Out frames -View 3d -Frames 120 -Interval 100   (PNG sequence for the GIF)
param([string]$Out, [string]$View = "flat", [string]$Open = "", [double]$Wait = 4, [int]$Frames = 0, [int]$Interval = 100, [string]$Exe = "")
$ErrorActionPreference = "Stop"
if (-not $Exe) { $Exe = Join-Path $PSScriptRoot "..\target\release\switchboard-app.exe" }
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class Cap {
  [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
  public struct RECT { public int L, T, R, B; }
  public struct POINT { public int X, Y; }
}
'@
[Cap]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null

Get-Process switchboard-app -ErrorAction SilentlyContinue | Stop-Process; Start-Sleep -Milliseconds 500
$env:SWITCHBOARD_VIEW = $View; $env:SWITCHBOARD_OPEN = $Open
$p = Start-Process $Exe -ArgumentList "--demo" -PassThru
for ($i = 0; $i -lt 50 -and $p.MainWindowHandle -eq 0; $i++) { Start-Sleep -Milliseconds 100; $p.Refresh() }
$h = $p.MainWindowHandle
[Cap]::SetWindowPos($h, [IntPtr]::Zero, 40, 40, 1600, 1000, 0x0040) | Out-Null
Start-Sleep -Seconds $Wait

function Grab([string]$path) {
  $r = New-Object Cap+RECT; [Cap]::GetWindowRect($h, [ref]$r) | Out-Null
  $c = New-Object Cap+RECT; [Cap]::GetClientRect($h, [ref]$c) | Out-Null
  $pt = New-Object Cap+POINT; [Cap]::ClientToScreen($h, [ref]$pt) | Out-Null
  $w = $r.R - $r.L; $ht = $r.B - $r.T
  $bmp = New-Object System.Drawing.Bitmap $w, $ht
  $g = [System.Drawing.Graphics]::FromImage($bmp); $hdc = $g.GetHdc()
  [Cap]::PrintWindow($h, $hdc, 2) | Out-Null; $g.ReleaseHdc($hdc); $g.Dispose()
  # Crop to the client area (drop the OS title bar and shadow borders).
  $ox = $pt.X - $r.L; $oy = $pt.Y - $r.T
  $crop = $bmp.Clone((New-Object System.Drawing.Rectangle $ox, $oy, ($c.R), ($c.B)), $bmp.PixelFormat)
  $crop.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); $crop.Dispose(); $bmp.Dispose()
}

if ($Frames -gt 0) {
  New-Item -ItemType Directory -Force $Out | Out-Null
  $sw = [Diagnostics.Stopwatch]::StartNew()
  for ($i = 0; $i -lt $Frames; $i++) {
    Grab (Join-Path $Out ("f{0:D4}.png" -f $i))
    $next = ($i + 1) * $Interval
    $left = $next - $sw.ElapsedMilliseconds
    if ($left -gt 0) { Start-Sleep -Milliseconds $left }
  }
  "captured $Frames frames in $($sw.ElapsedMilliseconds) ms"
} else {
  New-Item -ItemType Directory -Force (Split-Path $Out) | Out-Null
  Grab $Out; "saved $Out"
}
Stop-Process -Id $p.Id
