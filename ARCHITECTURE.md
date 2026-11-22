# Architecture

```
 ┌──────────── Windows ────────────┐                 ┌───────────── Linux (X11) ─────────────┐
 │ physical mouse/keyboard         │                 │                                        │
 │   │ WH_MOUSE_LL / WH_KEYBOARD_LL│   UDP input     │  on_input → XTest / uinput  → X server │
 │   ▼ (only while forwarding)     │ ──────────────► │  edge check (XQueryPointer) → Leave    │
 │ hook → queue → engine ──────────┤ ◄────────────── │  acks (liveness + RTT)                 │
 │ Raw Input (local edge detection)│   TCP control   │                                        │
 │ SendInput ◄─────────────────────┤ ◄────────────► │  XI2 raw events + grabs (capture)      │
 │ dxva2 DDC/CI ──┐                │  (Noise XX)     │  i2c-dev DDC/CI ──┐                    │
 └────────────────┼────────────────┘                 └───────────────────┼────────────────────┘
                  └──────────── shared monitor (VCP 0x60) ───────────────┘
```

Two processes per computer:

- **`switchboard`**: the service. Rust, no async runtime, a handful of threads. Owns input capture, injection, networking, DDC, clipboard. About 4 MB (Linux) to 14 MB (Windows) of memory.
- **`switchboard-app`**: tray icon and desk UI in the system web view (WebView2 / WebKitGTK). It only talks to the service's local API, so it can crash or be closed without affecting input. The window and web view are destroyed when closed.

## Source map

| | |
|---|---|
| `src/proto.rs` | Wire formats: control messages (JSON in Noise frames), input/keys snapshots (binary UDP) |
| `src/net.rs` | Identity keys, Noise XX handshake, encrypted TCP framing, encrypted UDP datagrams, pairing code |
| `src/engine.rs` | Peers, pointer ownership state machine, source/sink paths, focus and keyboard forwarding, watchdogs, pairing |
| `src/geom.rs` | Screen edges, push-through resistance, dwell, arming, landing points |
| `src/clipboard.rs` | Clipboard sync policy, loop prevention, streaming file archive |
| `src/ddc.rs` | Monitor input coordinator: coalesced switching, input watching |
| `src/platform/windows` | Raw Input, LL hooks (via a worker queue), SendInput, dxva2 DDC, Win32 clipboard |
| `src/platform/linux` | RandR screens (EDID names), XTest + uinput injection, XI2 capture, i2c-dev DDC, X11 selections |
| `src/discovery.rs`, `src/ipc.rs` | LAN beacons; local JSON API for the CLI and app |
| `ui/` | Desktop app: tao/wry/tray-icon shell, `web/` (flat SVG desk + three.js 3D desk) |

## Pointer ownership

Each computer is in one of three modes:

- **Local**: the pointer is here and moves locally. Edge pushes are checked against the configured links.
- **Remote(peer)**: we are the *source*. Physical input is blocked locally and streamed to the peer.
- **Hosted(peer)**: we are the *sink*. The peer's input is injected here; when the cursor pushes through a return edge we send `Leave`.

Handover is `Enter{epoch, edge, pos, landing}` over TCP. Each handover starts a new **epoch**; input datagrams from older epochs are ignored. If input datagrams for a new epoch arrive before `Enter`, nothing is lost: see the snapshot design below.

**Focus** (who has the pointer) is shared between computers. It changes on Enter/Leave/Revoke, and when someone uses a computer's own mouse or touchpad (`Focus{owner}` message). Keyboards forward to the focus owner.

## Input transport: snapshots, not events

Every input datagram carries a **snapshot**:

- cumulative motion `cum_dx, cum_dy` since the epoch began,
- cumulative wheel (1/120-notch units, vertical and horizontal),
- the current button bitmask,
- a ring of the last 16 button transitions, each stamped with the motion counters at the instant it happened.

The sink applies unseen transitions in order (moving to each transition's recorded position first), then the remaining motion and wheel, then reconciles against the authoritative bitmask. Consequences:

- **Loss costs nothing.** The next datagram carries the totals; the e2e test passes byte-for-byte with 50% loss.
- **No head-of-line blocking.** Old datagrams are simply superseded; there are no retransmissions.
- **Clicks land exactly where they happened**, even when motion is coalesced.
- **Reordering and replay** are rejected by the sequence check (nonces are the sequence numbers).

Keys use the same design (`KeyState`: 32-transition ring + 256-bit held-key set), plus resend-until-acknowledged because keystrokes are sparse.

The source sends a datagram per physical event (full polling rate) and a keepalive every 50 ms. The sink acks at most every 20 ms. Acks give liveness and RTT.

## Security

- **Identity**: each computer has a long-term X25519 key (`identity.key`, mode 0600 on Linux, per-user `%APPDATA%` on Windows).
- **Handshake**: `Noise_XX_25519_ChaChaPoly_BLAKE2s` on every TCP connection. Mutual authentication; only paired keys may do anything beyond pairing.
- **Pairing**: both sides display 6 digits derived from the handshake hash, which binds both static and ephemeral keys. If a man in the middle is present, the codes differ.
- **Transport**: after the handshake, snow's stateless transport (explicit nonces). TCP frames use nonces counting from 0; UDP datagrams use nonces with the top bit set, so one key never reuses a nonce across channels. UDP replays and stale datagrams are rejected.
- **Exposure**: listeners accept only private, link-local and loopback addresses. The local API binds 127.0.0.1 and requires a per-user secret token.

## Safety and recovery

Losing local control of the mouse or keyboard is the worst possible failure, so the design has several independent guards:

| Guard | Trigger | Effect |
|---|---|---|
| Hook pass-through invariant (Windows) | Hook has no consumer | Events pass to Windows, never swallowed |
| Non-blocking hooks (Windows) | (always) | Hook callbacks only enqueue (max observed 11 µs), so they can never stall system input |
| Dead-forwarding check | Hook blocking input but engine receiving none | Forwarding ends within ~0.4 s |
| Peer watchdog | No acks for 750 ms | Source returns to local; sink releases buttons after 1 s of silence |
| Keyboard watchdog | Keys unacknowledged for 1.2 s | Keyboard returns to the computer it's plugged into |
| Emergency hotkey | <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Shift</kbd>+<kbd>Esc</kbd> on either computer | Local control, forwarding paused |
| Mouse chord | Left+right+middle held 1.2 s | Local control |
| Button/key release | Any mode change, disconnect, shutdown | Everything held is released on the sink |
| Selftest gate | Every deploy | Real Windows capture path must forward and block correctly or nothing ships |

LL hooks are installed only while forwarding, so normal local use runs with no hooks at all.

## Capture details

**Windows.** Windows 11 throttles `WM_INPUT` for background windows (about 125 Hz), so Raw Input is only used for edge detection while local. While forwarding, the `WH_MOUSE_LL` hook is both the blocker and the data source: the cursor is parked mid-screen, and each event's `pt − park` is exactly its motion after Windows pointer ballistics. Hooks run for every event at the mouse's full polling rate. Linux's virtual pointer then uses a flat acceleration profile so motion isn't accelerated twice, and pointer speed feels the same on both computers.

**Linux.** XInput2 raw events on the root window (device deltas, all buttons, keys), with an active pointer grab (invisible cursor) and keyboard grab while forwarding. Events from our own virtual devices and XTEST are filtered out by device id.

## Injection

- **Windows**: `SendInput` (relative motion, so Windows ballistics apply when Linux forwards to Windows; scan-code keys).
- **Linux**: uinput virtual pointer when `/dev/uinput` is accessible (kernel device, high-resolution wheel, libinput, future Wayland path), otherwise XTest. Keys via XTest (keycode = evdev + 8). Wire keycodes are Linux evdev codes (layout-independent positions); Windows maps them to scan code set 1.

## Monitor control

`ddc::Coordinator` runs on its own thread, so DDC never blocks input. It always applies only the latest requested input, retries once, and falls back to asking the peer to switch if local DDC fails. Every 1.5 s, when no switch happened in the last 5 s, it reads the active input; a change it didn't make (the monitor's own button) moves pointer focus to the computer on screen.

- **Windows**: dxva2 `SetVCPFeature(0x60)`. About 60 ms, and it works while the monitor shows the other input.
- **Linux**: native i2c-dev DDC/CI. Only buses whose adapter name identifies a display bus are opened, matched by EDID model. `ddcutil` is the fallback.

The monitor's input for each computer is auto-detected from the connector type (DisplayPort→0x0F, HDMI→0x11) and overridable in config.

## Clipboard

Text and images are pushed on copy; files are offered on copy and sent when the pointer moves to that peer. Content written from a peer is hashed, so the resulting local change notification is ignored (no echo loops), and the same content is never sent twice to one peer. Files stream as a simple archive in 60 KiB frames interleaved with control messages, into a fresh timestamped folder. Paths are sanitized against traversal and reserved characters.

## Configuration

`switchboard.toml` in `%APPDATA%\Switchboard` or `~/.config/switchboard`. Links describe edges:

```toml
[[links]]
edge = "right"          # leave this computer's display through its right edge…
screen = "AW2726"       # …of this display (substring of its name; omit = whole desktop)
peer = "ubuntu-laptop"
to_screen = "AW2726"    # land on this display of the peer…
to_edge = "right"       # …at this edge (default: opposite of `edge`)
dwell_ms = 250          # hold against the edge this long first (optional)
```

The desk view in the app writes these for every computer at once (`layout` API, `SetLinks` message).
