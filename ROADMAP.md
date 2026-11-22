# Roadmap

## Done (0.1)

- [x] Milestone 1: connectivity, discovery, Noise-authenticated pairing, health checks
- [x] Milestone 2: Windows capture → Ubuntu injection, measured on real Wi-Fi
- [x] Milestone 3: reverse direction (XI2 capture, SendInput injection) *(implemented; the user has no second mouse in daily use, so it's lightly exercised)*
- [x] Milestone 4: monitor control (native DDC on both OSes), coordinated switching, hotplug behaviour investigated
- [x] Milestone 5: edges with resistance, dwell, corner gaps and arming; manual switching; stuck-input prevention; fail-safes
- [x] Milestone 6: clipboard (text, images, files and folders), automatic reconnection, keyboard follows the pointer
- [x] Milestone 7 (partly): desktop app with flat and 3D desk views on Windows, docs, benchmark harness

## Next

1. **Build and verify the desktop app on Ubuntu** (needs `libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev libxdo-dev`).
2. **Installers**: signed MSI for Windows (WiX); `.deb` for Ubuntu that installs the uinput udev rule and autostart entry. Script-based installers are in `packaging/` for now.
3. **Wi-Fi power-save detection**: warn when the link shows the sleep pattern (RTT clusters of 100+ ms) and explain how to fix it. This was the single biggest latency factor measured.
4. **Physical latency rig**: photodiode on the screen plus a button-actuated mouse, to publish real end-to-end numbers next to the local-mouse baseline.
5. **Sleep/wake and network-change tests** on real hardware; listen for `WM_POWERBROADCAST` / logind `PrepareForSleep` to reconnect immediately on resume.
6. **Elevated windows on Windows**: optional elevated helper (or a service in the user session) so input works in admin windows; investigate VHF only if injection into the secure desktop is required.
7. **Drag and drop of files across the edge** (currently copy/paste only).
8. **Wayland**: injection already uses uinput; capture needs the compositor's input-capture portal (`org.freedesktop.portal.InputCapture`) and libei.
9. **macOS**: CGEventTap capture, CGEvent injection, IOAVService DDC (Apple Silicon) or IOKit I2C.
10. **More than two computers**: pointer hand-off chains (A→B→C) and per-pair layouts in the desk view.

## Known limitations

- On Windows, input can't reach elevated windows or the secure desktop while Switchboard runs unelevated.
- The shared monitor needs 1–3 s to show the new picture after a switch; that's the monitor.
- With a monitor shared between two computers, each OS's own display arrangement can block some edges. The desk view shows these as dashed cables with an explanation.
