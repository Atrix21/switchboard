# Benchmarks

Every number here was measured on the development pair. Nothing is extrapolated.

**Setup.** Windows 11 laptop (Razer Viper V3 Pro mouse, Bluetooth Keychron K8 Pro) and Ubuntu 24.04.4 X11 laptop (Intel AX Wi-Fi, RTX 3060), both on the same 5 GHz Wi-Fi access point, about 2 m apart. Alienware AW2726DM on DisplayPort (Windows) and HDMI (Ubuntu). Release builds.

## What is and isn't measured

- **Transport latency**: software timestamps on one machine for a round trip over the network (probe → echo → probe). One-way latency is roughly half, but Wi-Fi is asymmetric and bursty, so p50/2 is an estimate.
- **Processing cost**: time inside Switchboard per event (capture handler, encryption, injection call).
- **Not measured: physical end-to-end latency** (hand moves → photons change). That adds mouse polling (0.125–1 ms), OS input processing, compositor and display scan-out (about 7–17 ms at 60–144 Hz, plus monitor processing), which apply equally to a locally connected mouse. Measuring it needs a high-speed camera or a photodiode rig; it's on the roadmap. No end-to-end latency claim is made.

## 1. The Wi-Fi itself: power saving

ICMP ping Windows → Ubuntu, 100 probes at 10 Hz:

| Ubuntu Wi-Fi power save | min | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| on (Ubuntu default, `wifi.powersave = 3`) | 0 ms | **255 ms** | 818 ms | 907 ms | 909 ms |
| off for this network (`802-11-wireless.powersave 2`) | 1 ms | **2 ms** | 11 ms | 20 ms | 49 ms |

With power saving on, the laptop's radio sleeps between beacons and input arrives in clumps hundreds of milliseconds late. **This setting matters more than everything else in this document combined.** Switchboard should detect and explain it (ROADMAP).

## 2. Transport: UDP vs TCP

`switchboard bench net <host> --rate 1000 --secs 15`: 64-byte probes at 1000 Hz (a typical gaming-mouse rate), echoed by `switchboard bench echo`. TCP uses `TCP_NODELAY`.

| Direction | Proto | loss | p50 | p90 | p99 | p99.9 | max | jitter |
|---|---|---|---|---|---|---|---|---|
| Win → Linux, run 1 | UDP | 0% | 1.62 | 3.38 | 21.1 | 92.9 | 99.8 | 0.42 |
| | TCP | 0% | 1.93 | 17.95 | 104.4 | 120.5 | 125.2 | 0.58 |
| Win → Linux, run 2 | UDP | 0% | 1.34 | 2.71 | 5.14 | 11.8 | 13.8 | 0.32 |
| | TCP | 0% | 1.89 | 3.25 | 10.5 | 20.3 | 27.2 | 0.43 |
| Linux → Win | UDP | 0% | 1.97 | 3.15 | 10.5 | 31.7 | 45.6 | 0.50 |
| | TCP | 0% | 2.06 | 6.18 | 39.9 | 45.4 | 47.3 | 0.66 |

All values in ms (round trip). Medians are nearly identical; the **tail** is where TCP loses, by 2–5× at p99, because one delayed segment stalls everything behind it. Switchboard's input path is UDP with loss-proof snapshots (DECISIONS ADR-2).

## 3. Live input path (inside Switchboard, during real use)

From `switchboard status` after a session of real mouse use across both machines (about 6,000 forwarded events):

| | p50 | p99 |
|---|---|---|
| Capture handler (Windows, event → datagram sent) | 2 µs | 5 µs |
| Windows hook callback, worst case observed | | 11 µs |
| Sink injection (Linux uinput write) | <1 µs | 13 µs |
| Input-path round trip (datagram → ack, includes Wi-Fi) | 2.3 ms | 20.0 ms (p95 14.9) |
| Datagrams rejected (auth/replay) | 0 | |
| Sequence gaps at the sink | 0 | |

## Where latency applies

Not a measurement, but a property of the design that the numbers above support: **on the computer the mouse and keyboard are connected to, Switchboard adds no latency, because it isn't in the input path while the pointer is there.**

| Platform | While the pointer is local | While the pointer is on the other computer |
|---|---|---|
| Windows | Raw Input sink (observes only). No hooks installed. | `WH_MOUSE_LL` / `WH_KEYBOARD_LL` hooks installed; each callback costs 2 µs p50, 11 µs worst observed (section 3). |
| Linux X11 | XInput2 raw events (observe only). No grabs. | Pointer and keyboard grabs. |
| Linux Wayland | evdev read-only; devices not grabbed. | `EVIOCGRAB` on the devices. |

The other computer pays one network hop (about half the round trip in sections 2 and 3) plus injection (section 3: <1 µs p50). The desktop app states this under *Where the delay is*. Physical end-to-end latency is still unmeasured (see the top of this file).

## 4. Micro-benchmarks

| | p50 | p99 | Notes |
|---|---|---|---|
| Encode + encrypt + decrypt + decode one input datagram | 2.50 µs | 2.77 µs | 113-byte datagram, Linux, `bench crypto` |
| XTest relative motion + flush | 5.6 µs | 267 µs | Linux, `bench inject` (p99 = X server scheduling) |
| XQueryPointer round trip | 13.8 µs | 94.5 µs | Linux |
| DDC set input, Windows (dxva2) | 51–63 ms | | 13 switches observed |
| DDC get input, Linux native i2c | ~40 ms | | dominated by the 40 ms DDC/CI reply delay |
| DDC get input, `ddcutil` CLI | 364 ms | | process start and conservative sleeps |
| Monitor shows the new picture after a switch | 1–3 s | | the monitor's own resync; not under software control |

## 5. Polling rate

Windows 11 throttles `WM_INPUT` for background windows. In one forwarding session the LL hook saw **13,367** mouse events while Raw Input delivered **2,907** (22%). After moving forwarded capture to the hook (ADR-4), every hook event is forwarded, so the update rate equals the mouse's polling rate.

## 6. Resources

| | Windows service | Linux service | Desktop app |
|---|---|---|---|
| Resident memory | 13.9 MiB | 4.3 MiB | — (web view is separate, only while the window is open) |
| Idle CPU, 60 s, paired and connected | 0.62% of one core | 0.80% | 0 when closed |
| Idle CPU before adaptive ticking | 1.25% | 1.17% | |
| Binary size (release) | 1.9 MB | 2.3 MB | 3.8 MB incl. three.js (debug) |

## 7. Recovery

| Scenario | Measured |
|---|---|
| Source process killed while the sink holds a button | button released on the sink after **0.02 s** (TCP reset) |
| Source process killed: sink returns to local mode | **0.03 s** |
| Peer silent (network loss): source returns control | 750 ms by design (`peer_timeout_ms`), observed in the field |
| Sink with held buttons and no datagrams | released after 1 s by design |
| Pairing (handshake + both confirmations, loopback) | 0.16 s |
| Reconnect after a service restart | under 1 s (discovery beacon + dial) |
| Loss tolerance | identical final state at 0%, 33% and 50% injected datagram loss (e2e) |

## Reproducing

```bash
switchboard bench echo                          # on machine B
switchboard bench net <B> --rate 1000 --secs 15 # on machine A
switchboard bench inject 5000
switchboard bench crypto
SWITCHBOARD_JSON=1 switchboard status           # live input-path statistics
```
