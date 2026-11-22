#!/usr/bin/env bash
# Copy the working tree (tracked + new files, no target/) to the Ubuntu dev box and build.
set -euo pipefail
cd "$(dirname "$0")/.."
HOST=${SB_LINUX_HOST:-sbl}
{ git ls-files; git ls-files --others --exclude-standard; } | sort -u | tar -cf - -T - | ssh "$HOST" 'mkdir -p ~/switchboard && tar -xf - -C ~/switchboard'
[ "${1:-}" = "--no-build" ] && exit 0
ssh "$HOST" bash -s <<REMOTE
source ~/.cargo/env && cd ~/switchboard
cargo build ${CARGO_ARGS:-} 2>&1 | grep -E "^(error|warning)" -A6 | head -80
cargo build ${CARGO_ARGS:-} 2>&1 | tail -1
REMOTE
