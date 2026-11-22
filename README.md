# Switchboard

Use one mouse and keyboard across the computers on your desk. Push the pointer off the edge of one screen and it appears on the next computer; your keystrokes follow it, the clipboard comes along, and a monitor shared between the computers switches its input to match.

Switchboard is not remote desktop. Nothing is streamed: each computer drives its own displays, and Switchboard moves input (and clipboard contents) between them over your local network, encrypted.

Windows and Linux (X11) today; the architecture leaves room for macOS and Wayland.

## What it does

- **One pointer, many computers.** Cross configurable screen edges, or press <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Shift</kbd>+<kbd>S</kbd>. Clicks, drags, wheel (including horizontal and high-resolution), back/forward buttons.
- **Keyboard follows the pointer.** Type on whichever computer has the pointer, wherever the keyboard is physically connected. Hardware keyboard switching (e.g. a Bluetooth keyboard's host keys) keeps working.
- **Monitor input switching.** A monitor connected to two computers switches input (DDC/CI) when the pointer moves, and pressing the monitor's own input button moves the pointer and keyboard too.
- **Clipboard and files.** Text and images are shared as soon as you copy. Copied files and folders move when the pointer does, into `Downloads/Switchboard/<time>/`.
- **Pairing, not accounts.** Computers find each other on the LAN and pair by comparing a 6-digit code. No cloud, no sign-in.
- **Safe by design.** Losing the network, a crashed peer, or a stuck state returns control to the computer you're at within a second, and releases every held button and key. <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Shift</kbd>+<kbd>Esc</kbd> or holding all three mouse buttons always forces local control.

## Install

### Windows

```powershell
# from a release folder containing switchboard.exe and switchboard-app.exe
powershell -ExecutionPolicy Bypass -File packaging\windows\install.ps1
```

This installs to `%LOCALAPPDATA%\Programs\Switchboard`, adds a Start menu entry and starts Switchboard at login. Allowing it through Windows Firewall needs an administrator once (the installer offers to do it, scoped to your local subnet).

### Ubuntu / Debian

```bash
sudo apt install ./switchboard_0.1.0_amd64.deb
```

The package installs a udev rule that lets the desktop user create Switchboard's virtual pointer (`/dev/uinput`), so no root is needed at runtime. If `ufw` is enabled, allow the ports from your LAN:

```bash
sudo ufw allow from 192.168.0.0/16 to any port 47801,47802 proto udp
sudo ufw allow from 192.168.0.0/16 to any port 47801 proto tcp
```

## First run

1. Open Switchboard on both computers. Each appears on the other's desk.
2. Click **Pair**, check that both computers show the same code, and confirm on both.
3. Drag the displays in the desk view to match where they really are, then **Use this arrangement**.

From the command line instead:

```text
switchboard run                       start the service
switchboard pair 192.168.1.20         pair with another computer
switchboard confirm                   confirm the code shown on both
switchboard link right <peer>         the pointer leaves this computer's right edge to <peer>
switchboard status                    connection, latency, pointer location
switchboard monitor list|get|set hdmi monitor input via DDC/CI
```

## Performance

Measured on the development pair (two laptops on the same 5 GHz Wi-Fi), see [BENCHMARKS.md](BENCHMARKS.md):

| | |
|---|---|
| Switchboard's own processing per event (capture → encrypt → send, receive → decrypt → inject) | ~0.02 ms |
| Wi-Fi round trip, typical (p50) | 1.3–2.3 ms |
| Wi-Fi round trip, p99 | 5–21 ms (UDP), 10–104 ms (TCP) |
| Memory | 4 MB (Linux service), 9–14 MB (Windows service) |

Network latency dominates. On Wi-Fi, occasional delays of 15–20 ms are the medium, not Switchboard; a wired connection on either computer removes them.

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md): how it works
- [DECISIONS.md](DECISIONS.md): architecture decision records and rejected alternatives
- [BENCHMARKS.md](BENCHMARKS.md): measurements and method
- [TESTING.md](TESTING.md): what is tested, how, and what was verified on real hardware
- [ROADMAP.md](ROADMAP.md): what's next

## Building

```bash
cargo build --release                   # service + CLI
cargo build --release -p switchboard-app  # desktop app
```

Linux needs `libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev libxdo-dev` to build the desktop app; the service has no system library dependencies.

License: MIT.
