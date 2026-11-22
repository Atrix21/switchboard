# Architecture decision records

Each record states the decision, the evidence, and what was rejected. Records marked *superseded* are kept because the reasoning matters.

## ADR-1: Rust, no async runtime

**Decision.** One Rust codebase for both OSes; plain threads and blocking std sockets.
**Why.** Native API access on both platforms (`windows-sys`, pure-Rust `x11rb`, `libc`), no GC pauses on the input path, small static binaries. The work is a handful of long-lived loops (capture, UDP, control readers, engine), where threads are simpler and have lower wake-up latency than an executor. `x11rb` being pure Rust also let the Linux backend be type-checked from Windows before SSH access existed.
**Rejected.** Go (GC, weaker Win32 ergonomics); C++ (safety); C#/.NET (no Linux input story, runtime size); tokio (no benefit for this shape, more dependencies).

## ADR-2: UDP snapshots for input, TCP for control

**Decision.** Input travels as encrypted UDP datagrams carrying cumulative snapshots (ARCHITECTURE.md); reliable, ordered events (handover, pairing, clipboard) go over one Noise-encrypted TCP connection.
**Evidence** (BENCHMARKS.md, real Wi-Fi, 1000 Hz probes, 15 s): UDP p99 5.1–21 ms vs TCP 10.5–104 ms Windows→Linux; Linux→Windows UDP p99 10.5 ms vs TCP 39.9 ms. Medians are equal; TCP's tail is retransmission head-of-line blocking. The snapshot design makes loss free (e2e passes at 50% loss) without retransmitting anything.
**Rejected.** TCP-only (tail latency on Wi-Fi). QUIC (`quinn`): datagrams plus streams plus TLS in one, but a large dependency, and its reliability machinery isn't needed once input is idempotent. Per-event reliable UDP with ACK/retransmit: adds latency on loss; the snapshots make it unnecessary.

## ADR-3: Noise XX with SAS pairing

**Decision.** `snow` (Noise_XX_25519_ChaChaPoly_BLAKE2s). Pairing compares 6 digits derived from the handshake hash. One key per direction, disjoint nonce spaces for TCP and UDP.
**Rejected.** TLS with pinned self-signed certificates (heavier, PKI concepts nobody needs on a desk); PIN-PSK pairing (requires typing on the other machine, which is awkward before the keyboard works there); unauthenticated LAN protocols (Synergy 1.x style).

## ADR-4: Windows capture = on-demand LL hooks; Raw Input only for edges (*revised*)

**First version.** Raw Input (`RIDEV_INPUTSINK`) for unaccelerated deltas, `WH_MOUSE_LL` only to block local movement.
**Evidence that changed it.** With the hook blocking, the hook saw 13,367 events while Raw Input delivered 2,907. Windows 11 throttles `WM_INPUT` for background processes (about 125 Hz), so the forwarded update rate was a fraction of the Razer's polling rate.
**Decision.** While forwarding, the LL hook is the data source: cursor parked at screen centre, `pt − park` per event = that event's motion after Windows ballistics. Linux injects it through a flat-profile virtual pointer, so the user's Windows pointer speed applies on both machines. Hooks are installed only while forwarding (zero overhead locally) and their callbacks only enqueue events (ADR-9).
**Rejected.** Making our window foreground to defeat throttling (steals focus). A kernel filter driver (Interception-style): rejected without benchmarking, because user-space reaches full rate with the hook and a driver needs signing and admin installation. Virtual HID Framework: only needed to *inject* as a real HID device (e.g. for elevated windows or the secure desktop); revisit if those limits matter (ROADMAP).

## ADR-5: Linux injection = uinput (flat profile) with XTest fallback

**Decision.** uinput when `/dev/uinput` is accessible (installed udev `uaccess` rule), otherwise XTest. Keys via XTest.
**Why.** uinput is a real kernel device: high-resolution wheel, the same path a physical mouse takes, and the only path that will work under Wayland compositors. XTest needs no setup on X11. Measured XTest motion+flush: 5.6 µs p50.
**Rejected.** XTest-only (no hi-res wheel, X11-only); `xdotool` (process spawn per event).

## ADR-6: Linux capture = XInput2 raw events + grabs

**Decision.** XI2 raw motion/button/key events on the root window; while forwarding, an active pointer grab (invisible cursor, parked) and keyboard grab divert input from applications.
**Rejected.** evdev with `EVIOCGRAB` (needs `input` group, and touchpads would need gesture processing that libinput already does).

## ADR-7: Monitor switching on its own thread, native DDC on both OSes

**Evidence.** Windows `SetVCPFeature(0x60)`: 51–63 ms over a dozen switches. The AW2726DM keeps Windows' desktop topology intact while showing HDMI for 6 s (no DisplayPort hot-unplug, no windows moved), and Windows can switch it back while its input is inactive. Linux native i2c read: about 40 ms; `ddcutil` CLI: 360 ms.
**Decision.** Coordinator thread with coalescing (latest request wins), one retry, fall back to the peer. Linux talks i2c-dev directly; `ddcutil` is the fallback. Bus discovery opens only display-adapter buses (by sysfs adapter name) and matches the EDID model, never probing SMBus/SPD.
**Rejected.** `ddcutil` as the primary path (process spawn, about 10× slower); probing every i2c bus.

## ADR-8: Keyboard follows the pointer in software

**Context.** The Keychron K8 Pro's Bluetooth host switch is a key combination on the keyboard; there's no documented way to trigger it from the host (unlike Logitech's HID++ "Easy-Switch", which Logitech Flow uses).
**Decision.** Wherever the keyboard is connected, keystrokes are forwarded to the focus owner (LL keyboard hook on Windows, XI2 raw keys + keyboard grab on Linux). Same loss-proof snapshot design as the mouse. Hardware switching keeps working: whichever machine gets the keyboard forwards as needed. Local hotkeys are detected in the forwarded stream, because `RegisterHotKey` can't fire while keys are swallowed.

## ADR-9: Hook callbacks never block (after a field failure)

**Incident.** A half-applied edit left the Windows hook handler unregistered: while forwarding, the hooks swallowed the user's real mouse and keyboard and forwarded nothing. The network keepalives kept the watchdog satisfied, so control never came back; the user recovered via the lock screen. The loopback tests use synthetic capture and couldn't see it.
**Decisions.**
1. Hook callbacks decide swallow/pass from atomics and enqueue; a worker thread runs the engine. While a low-level hook is installed, Windows waits for it before delivering *any* input, so a blocking callback freezes the desktop.
2. Invariant: an event is swallowed only if it was handed to a consumer.
3. Engine-level dead-forwarding detection and a keyboard acknowledgement watchdog.
4. `switchboard selftest` drives the real hook path with tagged synthetic input; the deploy script refuses to ship if it fails.

## ADR-10: Shared-monitor topology and user-chosen connections (*revised*)

**Context.** Physical desk, left to right: Ubuntu laptop, Windows laptop, shared Alienware. Each OS places the Alienware next to its own panel, so "right of the Ubuntu laptop" means two different things depending on which computer the Alienware is showing.
**First version.** The monitor always followed the pointer; Ubuntu could only return to Windows through the Alienware flip. The desk view drew one cable between the laptops, which read as two-way. The user found this confusing.
**Decision (chosen by the user).**
- Connections are one-way, drawn with arrows, and **chosen by the user in the app** (drag from an edge jack to another computer's screen; click a cable to edit or delete it). "Use recommended connections" restores the default.
- Per connection: **switch the shared monitor or not**, **hold time**, and the condition **"only while the shared monitor shows another computer"**. While that condition holds, the shared monitor's screen beyond the edge (invisible at that moment) doesn't block the crossing. Computers announce every monitor switch (`MonitorShows`), so the condition is evaluated on current information on both sides.
- Recommended default for this desk: laptops are neighbours both ways and don't switch the monitor; the Windows laptop's right edge reaches Ubuntu's Alienware while it shows Ubuntu; the Alienware's outer edge flips it (switches the monitor, 250 ms hold).
- Edges that an OS layout makes impossible are drawn dashed red with an explanation; conditional ones are dashed with a monitor badge. Arriving on an edge disarms it until the pointer leaves it (no bounce-back).
- Every computer's connections travel in its Hello, so each app shows every direction truthfully.

## ADR-11: Desktop app = system web view, separate process

**Decision.** `tao` + `wry` + `tray-icon` (Tauri's low-level crates, no Node toolchain), one hand-written HTML/CSS/JS interface, three.js bundled for the 3D desk. Separate process from the service, talking to it over the local API.
**Why.** One design that looks deliberate on both OSes, without Electron (system WebView2/WebKitGTK, about 4 MB binary). Process separation means the UI can never affect input.
**Rejected.** Electron (size, memory, explicitly excluded). Native WinUI 3 + GTK4/libadwaita (two UIs to build and keep in sync). egui (looks like a developer tool). Loading three.js from a CDN (no cloud dependencies at runtime).

## ADR-12: Discovery = UDP broadcast beacons

**Decision.** Small JSON beacons every 2 s on UDP 47802 (limited and directed broadcast), carrying name, public key and port. They grant nothing; connections are still authenticated and must match a paired key. Static addresses in config serve as a fallback.
**Rejected.** mDNS/DNS-SD (needs a dependency or the system responder on each OS; broadcasts are sufficient on a desk's LAN).
