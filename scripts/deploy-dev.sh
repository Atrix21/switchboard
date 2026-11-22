#!/usr/bin/env bash
# Build release binaries on both dev machines and restart both daemons.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
./target/release/switchboard.exe quit >/dev/null 2>&1 || true
cargo build --release 2>&1 | grep -E "^(error|warning)" -A6 | head -40 || true
# Gate: never deploy a Windows build whose real capture path is broken.
./target/release/switchboard.exe selftest >/tmp/sb-selftest.log 2>&1 || { cat /tmp/sb-selftest.log; echo "SELFTEST FAILED - not deploying"; exit 1; }
tail -1 /tmp/sb-selftest.log
./scripts/sync-linux.sh --no-build
ssh "${SB_LINUX_HOST:-sbl}" bash -s <<'REMOTE'
pkill -f "[s]witchboard run" || true
source ~/.cargo/env; cd ~/switchboard
cargo build --release 2>&1 | grep -E "^(error|warning)" -A6 | head -40
export DISPLAY=:1 XAUTHORITY=/run/user/1001/gdm/Xauthority
nohup target/release/switchboard run --log-file >/dev/null 2>&1 &
REMOTE
powershell.exe -NoProfile -c 'Start-Process -FilePath "'"$(pwd -W)"'/target/release/switchboard.exe" -ArgumentList "run","--log-file" -WindowStyle Hidden'
sleep 4
./target/release/switchboard.exe status | head -4
