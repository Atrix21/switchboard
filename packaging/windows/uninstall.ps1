# Removes Switchboard installed by install.ps1. Settings in %APPDATA%\Switchboard are kept unless -Purge.
param([switch]$Purge)
$dest = Join-Path $env:LOCALAPPDATA "Programs\Switchboard"
foreach ($p in "switchboard-app", "switchboard") { Get-Process $p -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$dest*" } | Stop-Process -Force }
Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "Switchboard" -ErrorAction SilentlyContinue
Remove-Item (Join-Path ([Environment]::GetFolderPath("Programs")) "Switchboard.lnk") -ErrorAction SilentlyContinue
Remove-Item $dest -Recurse -Force -ErrorAction SilentlyContinue
if ($Purge) { Remove-Item (Join-Path $env:APPDATA "Switchboard") -Recurse -Force -ErrorAction SilentlyContinue }
if (Get-NetFirewallRule -DisplayName "Switchboard*" -ErrorAction SilentlyContinue) {
  Write-Host "Firewall rules 'Switchboard (TCP/UDP)' remain; remove them as administrator with: Remove-NetFirewallRule -DisplayName 'Switchboard*'"
}
Write-Host "Switchboard removed."
