# Testing

## Automated

| Suite | Command | What it proves |
|---|---|---|
| Unit tests (15) | `cargo test` | Wire formats, Noise handshake + framing + UDP replay/tamper rejection, edge geometry (real layout), resistance/dwell/arming, keymaps, archive parser at every chunk size, path traversal rejection, DIB↔PNG, URI encoding |
| Loopback end-to-end | `python tests/e2e_loopback.py <switchboard binary> [drop_every_n]` | Two real daemons on one machine with synthetic capture and a recording injector: pairing codes match, edge entry point, 200 motion events + drag + all buttons + both wheels land at exact positions, keystrokes in order, held key released when the pointer leaves, edge doesn't bounce after return, clipboard text both ways with no echo, files only after the pointer moves and byte-identical, stuck-button release when the source dies. Run with 0, 2 (50% loss) and 3 (33% loss). |
| Windows capture selftest | `switchboard selftest` | The **real** hooks + worker queue + handler path forwards motion, buttons, wheel and keys, blocks them locally, and passes input to Windows again afterwards. Gate in `scripts/deploy-dev.sh`. |
| Linux type-check from Windows | `cargo check --target x86_64-unknown-linux-gnu` | |

The loopback test never touches the real mouse, keyboard or monitor (`SWITCHBOARD_INJECT=null`, `SWITCHBOARD_CAPTURE=none`, monitor disabled).

## Verified on the real machines

Done with the user at the desk unless noted as automated.

| Feature | Result |
|---|---|
| Discovery across Wi-Fi | ✅ found in under 1 s both ways |
| Pairing (6-digit codes matched, confirmed both sides) | ✅ |
| Monitor switch Windows→HDMI→DP via DDC (automated, manual switch command) | ✅ 54/56 ms |
| Windows keeps desktop topology while the AW2726DM shows HDMI | ✅ 6 s test, no topology change, switch back from Windows works |
| Edge crossing with the physical Razer mouse, clicks, scroll, drags on Ubuntu | ✅ user test ("it works") |
| Monitor follows the pointer across edges | ✅ |
| Lag after the Windows 11 throttling fix and flat profile | ✅ user reported "noticeable but not that bad" before the fix; fixed afterwards |
| Keyboard follows the pointer | ✅ user test |
| Clipboard text Windows→Ubuntu | ✅ automated on real machines (`Set-Clipboard` → `switchboard clip get` on Ubuntu) |
| Clipboard images, files on real machines | ⚠️ e2e only (files byte-identical in loopback) |
| Desktop app on Windows (flat + 3D views, live status) | ✅ screenshots reviewed |
| Desktop app on Ubuntu | ⏳ needs the GTK/WebKit development packages to build |
| Monitor's own input button moves the pointer | ⏳ implemented; awaiting a user test |
| Recovery: the incident in DECISIONS ADR-9 | Fixed; guarded by the selftest gate and fail-safes |

## Not yet tested

- Sleep/wake and network changes on real hardware (reconnect logic is in place: TCP timeouts, 2 s redial, discovery, UDP roaming on authenticated datagrams).
- Elevated windows on Windows (UIPI blocks hooks and `SendInput` for admin windows when Switchboard isn't elevated) and the secure desktop (lock screen, UAC prompts): input can't be forwarded to or from them. Known limitation.
- Fullscreen games that confine the cursor: edge switching is suppressed while the cursor is clipped (implemented, not tried with a game).
- Wayland sessions.
- Physical end-to-end latency (photodiode/camera rig).

## Manual test script

1. Start both services (`scripts/deploy-dev.sh` in development).
2. Cross every configured edge both ways; check that the monitor switches and the cursor lands where expected.
3. On the remote machine: left/right/middle click, drag a window, scroll both axes, back/forward buttons.
4. Type on the remote machine with the keyboard connected to the other one; hold Shift while crossing back and confirm nothing sticks.
5. Copy text, an image, and a folder on each machine; paste on the other.
6. Pull the Wi-Fi on one machine while controlling it: control must return within about 1 s and nothing stays pressed.
7. Press the emergency hotkey on each machine; hold all three mouse buttons for a second.
8. Press the monitor's input button; the pointer should follow.
