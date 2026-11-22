# Builds release binaries and a distributable zip in dist\.
$ErrorActionPreference = "Stop"
$root = Resolve-Path "$PSScriptRoot\..\.."
Push-Location $root
cargo build --release -p switchboard -p switchboard-app
$ver = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches.Groups[1].Value
$out = "dist\switchboard-$ver-windows-x64"
Remove-Item $out -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $out | Out-Null
Copy-Item target\release\switchboard.exe, target\release\switchboard-app.exe, packaging\windows\install.ps1, packaging\windows\uninstall.ps1, README.md $out
Compress-Archive -Path "$out\*" -DestinationPath "$out.zip" -Force
Pop-Location
Write-Host "Built $out.zip"
