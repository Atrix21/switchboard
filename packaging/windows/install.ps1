# Switchboard per-user installer for Windows.
# Run from the release folder:  powershell -ExecutionPolicy Bypass -File install.ps1
param([switch]$NoFirewall, [switch]$NoAutostart)
$ErrorActionPreference = "Stop"
$src = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not (Test-Path "$src\switchboard.exe")) { $src = Resolve-Path "$src\..\.." ; $src = "$src\target\release" }
$dest = Join-Path $env:LOCALAPPDATA "Programs\Switchboard"
New-Item -ItemType Directory -Force $dest | Out-Null

# Stop a running copy so the files can be replaced.
foreach ($p in "switchboard-app", "switchboard") { Get-Process $p -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$dest*" } | Stop-Process -Force }
Copy-Item "$src\switchboard.exe", "$src\switchboard-app.exe" $dest -Force
Write-Host "Installed to $dest"

# Start menu shortcut.
$lnk = Join-Path ([Environment]::GetFolderPath("Programs")) "Switchboard.lnk"
$sh = New-Object -ComObject WScript.Shell
$s = $sh.CreateShortcut($lnk); $s.TargetPath = "$dest\switchboard-app.exe"; $s.WorkingDirectory = $dest; $s.Description = "Share one mouse and keyboard across your computers"; $s.Save()

# Start at login (tray app; it starts the service).
if (-not $NoAutostart) {
  New-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "Switchboard" -Value "`"$dest\switchboard-app.exe`" --background" -PropertyType String -Force | Out-Null
}

# Firewall: inbound from the local subnet only (needs administrator once).
if (-not $NoFirewall) {
  $rules = Get-NetFirewallRule -DisplayName "Switchboard*" -ErrorAction SilentlyContinue
  if (-not $rules) {
    Write-Host "Allowing Switchboard through Windows Firewall for your local network (administrator approval needed)..."
    $cmd = 'New-NetFirewallRule -DisplayName "Switchboard (TCP)" -Direction Inbound -Protocol TCP -LocalPort 47801 -RemoteAddress LocalSubnet -Action Allow -Profile Any | Out-Null; ' +
           'New-NetFirewallRule -DisplayName "Switchboard (UDP)" -Direction Inbound -Protocol UDP -LocalPort 47801,47802 -RemoteAddress LocalSubnet -Action Allow -Profile Any | Out-Null'
    try { Start-Process powershell -Verb RunAs -Wait -ArgumentList "-NoProfile", "-Command", $cmd } catch { Write-Warning "Firewall rules not added: $_" }
  }
}

Start-Process "$dest\switchboard-app.exe"
Write-Host "Switchboard is running. Open it from the tray or the Start menu."
