"use strict";
/* Demo mode (switchboard-app --demo): a pretend desk that plays a short loop.
   No service, no hardware. Used for README screenshots and the demo GIF. */
(function () {
  if (!window.__SB_DEMO) return;
  const W = "windows-laptop", U = "ubuntu-laptop";
  const winLinks = [
    { edge: "left", peer: U, screen: "Generic PnP Monitor", to_screen: "eDP-1", to_edge: "right", switch_monitor: false },
    { edge: "right", peer: U, screen: "Generic PnP Monitor", to_screen: "AW2726DM", to_edge: "left", switch_monitor: false, when: "shared_shows_other" },
    { edge: "right", peer: U, screen: "AW2726DM", to_screen: "AW2726DM", to_edge: "right", dwell_ms: 250, switch_monitor: true },
  ];
  const ubuLinks = [
    { edge: "right", peer: W, screen: "eDP-1", to_screen: "Generic PnP Monitor", to_edge: "left", switch_monitor: false, when: "shared_shows_other" },
    { edge: "right", peer: W, screen: "AW2726DM", to_screen: "AW2726DM", to_edge: "right", dwell_ms: 250, switch_monitor: true },
  ];
  const config = {
    name: W, monitor: { enabled: true, follow_manual_switch: true }, keyboard: { follow_pointer: true },
    switching: { edge_resistance: 40, corner_deadzone: 8, lock_while_button_held: true },
    clipboard: { enabled: true, text: true, images: true, files: true, max_files_mib: 2048 },
    hotkeys: { emergency: "ctrl+alt+shift+escape", toggle: "ctrl+alt+shift+s" },
  };
  // The loop: [seconds, pointer on, monitor input]
  const script = [[0, W, 15], [3, U, 15], [6, W, 15], [9, U, 17], [12, W, 15]];
  const t0 = Date.now();
  const jitter = () => 1.6 + Math.sin(Date.now() / 900) * 0.5 + Math.random() * 0.4;
  function status() {
    const t = ((Date.now() - t0) / 1000) % 15;
    let step = script[0];
    for (const s of script) if (t >= s[0]) step = s;
    const [, on, input] = step;
    return {
      name: W, fingerprint: "K7QF-M2XA-93JD", version: "0.1.0", os: "windows", disabled: false,
      mode: on === W ? "local" : `controlling ${U}`, focus: null, keyboard_to: on === W ? null : U,
      screens: [
        { name: "Alienware_AW2726DM(DisplayPort) [\\\\.\\DISPLAY5]", rect: { x: 0, y: 0, w: 2560, h: 1440 }, primary: true },
        { name: "Generic PnP Monitor [\\\\.\\DISPLAY1]", rect: { x: -1920, y: 195, w: 1920, h: 1080 }, primary: false },
      ],
      links: winLinks,
      peers: [{
        name: U, fingerprint: "B33Z-ESLT-KTWM", address: "192.168.1.20:47801", os: "linux", trusted: true,
        rtt_ms: { p50: jitter(), p95: 9.8, p99: 14.2, samples: 1024 }, monitor_input: "HDMI-1", links: ubuLinks, connected_s: 3600,
        screens: [
          { name: "eDP-1", rect: { x: 0, y: 0, w: 2560, h: 1600 }, primary: true },
          { name: "AW2726DM [HDMI-1-0]", rect: { x: 2560, y: 0, w: 2560, h: 1440 }, primary: false },
        ],
      }],
      discovered: [],
      monitor: { model: "Alienware_AW2726DM(DisplayPort)", local_input: 15, observed: input, last_ms: 54, switches: 12, failures: 0 },
      transfers: [],
      stats: { switches: Math.floor((Date.now() - t0) / 3000), rss_bytes: 13.9 * 1048576, capture_events_per_s: 0, datagrams_sent_per_s: 0, capture_handler_us_p50: 2, sink_inject_us_p50: 0, cpu_s: 1.2 },
    };
  }
  const real = sb.call.bind(sb);
  sb.call = async (req) => {
    switch (req.cmd) {
      case "status": return status();
      case "config": return config;
      case "app.autostart_get": return { ok: true, enabled: true };
      default: return { ok: true };
    }
  };
  void real;
})();

/* Dev hook: open a panel on start (SWITCHBOARD_OPEN=settings | shared | link:<machine>:<index>). */
setTimeout(() => {
  const o = window.__SB_OPEN || "";
  if (o === "settings") document.querySelector("#open-settings").click();
  else if (o === "shared") select({ kind: "display", id: "shared" });
  else if (o.startsWith("link:")) { const [, m, i] = o.split(":"); select({ kind: "link", machine: m, index: Number(i) }); }
}, 1500);
