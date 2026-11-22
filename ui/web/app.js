"use strict";

/* ------------------------------------------------------------------ bridge */
const sb = {
  seq: 0,
  pending: new Map(),
  call(req) {
    const id = ++this.seq;
    return new Promise((resolve) => {
      this.pending.set(id, resolve);
      window.ipc.postMessage(JSON.stringify({ id, req }));
      setTimeout(() => { if (this.pending.delete(id)) resolve({ ok: false, error: "timeout" }); }, 15000);
    });
  },
  _reply(id, v) { const r = this.pending.get(id); if (r) { this.pending.delete(id); r(v); } },
};
window.sb = sb;

const $ = (s) => document.querySelector(s);
const NS = "http://www.w3.org/2000/svg";
const el = (tag, attrs = {}, parent) => {
  const e = document.createElementNS(NS, tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
  if (parent) parent.appendChild(e);
  return e;
};
const h = (html) => { const t = document.createElement("template"); t.innerHTML = html.trim(); return t.content; };
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

const COLORS = ["var(--teal)", "var(--coral)", "#8f7fd6", "#6aa0e0"];

/* ------------------------------------------------------------------ state */
const S = {
  status: null,
  config: null,
  offline: false,
  order: null,          // display ids, left to right
  savedOrder: null,     // order before a drag (for undo)
  plan: null,           // proposed links per machine
  sel: null,            // { kind, id }
  lastSwitches: null,
  pairingShown: null,
  autostart: null,
};

/* ------------------------------------------------------------------ model */
function modelCore(name) {
  const words = String(name || "").split(/[^A-Za-z0-9]+/).filter((w) => w.length >= 5 && /\d/.test(w));
  return words.length ? words[words.length - 1] : "";
}

function isLaptopPanel(screen) {
  return /^eDP|^LVDS|^DSI|Generic PnP|Internal/i.test(screen.name);
}

function shortScreenName(n) {
  // "Alienware_AW2726DM(DisplayPort) [\\.\DISPLAY5]" -> "Alienware AW2726DM"; "AW2726DM [HDMI-1-0]" -> "AW2726DM"
  let s = n.replace(/\s*\[.*\]\s*$/, "").replace(/\((DisplayPort|HDMI|DVI)\)/, "").replace(/_/g, " ").trim();
  if (/^Generic PnP/i.test(s)) s = "Built-in display";
  if (/^eDP/i.test(s)) s = "Built-in display";
  return s;
}

function friendlyOs(os) { return os === "windows" ? "Windows" : os === "linux" ? "Linux" : os === "macos" ? "macOS" : os || "Computer"; }

function buildModel() {
  const st = S.status;
  if (!st) return null;
  const core = modelCore(st.monitor && st.monitor.model);
  const machines = [{ name: st.name, os: st.os, me: true, screens: st.screens || [], input: st.monitor && st.monitor.local_input, color: COLORS[0] }];
  (st.peers || []).filter((p) => p.trusted).forEach((p, i) => {
    machines.push({ name: p.name, os: p.os, me: false, screens: p.screens || [], peer: p, input: inputCode(p.monitor_input), color: COLORS[(i + 1) % COLORS.length] });
  });
  const displays = [];
  let shared = null;
  for (const m of machines) {
    for (const s of m.screens) {
      if (core && s.name.includes(core)) {
        if (!shared) { shared = { id: "shared", shared: true, name: shortScreenName(s.name), names: {}, rect: s.rect, laptop: false }; displays.push(shared); }
        shared.names[m.name] = s.name;
      } else {
        // Stable key: the monitor name without the OS device suffix, when unique on that computer.
        const bare = s.name.replace(/\s*\[.*\]\s*$/, "");
        const key = m.screens.filter((o) => o.name.startsWith(bare)).length === 1 ? bare : s.name;
        displays.push({ id: m.name + "::" + s.name, machine: m.name, name: shortScreenName(s.name), osName: key, rect: s.rect, laptop: isLaptopPanel(s), primary: s.primary });
      }
    }
  }
  // A computer keeps the same colour in every app: assign by sorted name.
  machines.map((m) => m.name).sort().forEach((n, i) => { machines.find((m) => m.name === n).color = COLORS[i % COLORS.length]; });
  const byName = Object.fromEntries(machines.map((m) => [m.name, m]));
  // Who is the monitor showing?
  let showing = null;
  if (shared && st.monitor) {
    const obs = st.monitor.observed ?? st.monitor.last_target;
    showing = machines.find((m) => m.input != null && m.input === obs) || null;
  }
  // Where are the pointer and keyboard?
  const mode = st.mode || "local";
  let focus = st.name;
  const m1 = mode.match(/^controlling (.+)$/);
  if (m1) focus = m1[1];
  else if (mode === "local" && st.focus) focus = st.focus;
  return { machines, byName, displays, shared, showing, focus, core };
}

function inputCode(name) {
  if (name == null) return null;
  if (typeof name === "number") return name;
  const map = { "DisplayPort-1": 0x0f, "DisplayPort-2": 0x10, "HDMI-1": 0x11, "HDMI-2": 0x12, "DVI-1": 0x03, "VGA-1": 0x01, "USB-C": 0x1b };
  return map[name] ?? null;
}
function inputLabel(code) {
  return ({ 0x0f: "DisplayPort", 0x10: "DisplayPort 2", 0x11: "HDMI", 0x12: "HDMI 2", 0x03: "DVI", 0x01: "VGA", 0x1b: "USB‑C" })[code] || (code != null ? "Input 0x" + code.toString(16) : "Unknown input");
}

/* Default arrangement: derived from the configured links, else OS order. */
function defaultOrder(M) {
  const ids = M.displays.map((d) => d.id);
  try {
    const saved = JSON.parse(localStorage.getItem("desk-order") || "null");
    if (saved && saved.length === ids.length && saved.every((x) => ids.includes(x))) return saved;
  } catch (_) { /* storage unavailable */ }
  const me = M.machines[0];
  const mine = M.displays.filter((d) => d.machine === me.name).sort((a, b) => a.rect.x - b.rect.x);
  const others = M.displays.filter((d) => d.machine && d.machine !== me.name).sort((a, b) => a.rect.x - b.rect.x);
  const leftLink = (S.status.links || []).find((l) => l.edge === "left" && !(l.screen || "").match(M.core || "^$"));
  const order = [];
  if (leftLink || !(S.status.links || []).length) order.push(...others.map((d) => d.id), ...mine.map((d) => d.id));
  else order.push(...mine.map((d) => d.id), ...others.map((d) => d.id));
  if (M.shared) order.push("shared");
  return order;
}

/* ------------------------------------------------------------------ connections
   A connection is one-way: "leave <screen> of <machine> through <edge>, arrive
   at <to_edge> of <to_screen> on <peer>". Each computer stores its own. */

/* Every computer's connections: this computer's from config, peers' from their Hello. */
function allLinks() {
  if (S.plan) return S.plan;
  const st = S.status, out = { [st.name]: (st.links || []).map((l) => ({ ...l })) };
  for (const p of st.peers || []) if (p.trusted) out[p.name] = (p.links || []).map((l) => ({ ...l }));
  return out;
}

function isShared(M, screenName) { return !!(M.core && screenName && screenName.includes(M.core)); }

/* What the OS's own layout puts beyond this edge, if anything. */
function beyond(M, machineName, screenSub, edge) {
  const m = M.byName[machineName];
  if (!m) return null;
  const seg = m.screens.find((s) => s.name.includes(screenSub));
  if (!seg) return null;
  const r = seg.rect;
  const probe = { left: [r.x - 1, r.y + r.h / 2], right: [r.x + r.w, r.y + r.h / 2], top: [r.x + r.w / 2, r.y - 1], bottom: [r.x + r.w / 2, r.y + r.h] }[edge];
  return m.screens.find((s) => s !== seg && probe[0] >= s.rect.x && probe[0] < s.rect.x + s.rect.w && probe[1] >= s.rect.y && probe[1] < s.rect.y + s.rect.h) || null;
}

/* "open" | "conditional" (works while the shared monitor shows another computer) | "blocked" */
function edgeState(M, machineName, link) {
  const b = beyond(M, machineName, link.screen, link.edge);
  if (!b) return { state: "open" };
  if (isShared(M, b.name) && link.when === "shared_shows_other") return { state: "conditional", by: shortScreenName(b.name) };
  return { state: "blocked", by: shortScreenName(b.name), shared: isShared(M, b.name) };
}
function blockedBy(M, machineName, link) {
  const s = edgeState(M, machineName, link);
  return s.state === "blocked" ? s.by : null;
}

/* Recommended connections for a left-to-right arrangement:
   - neighbouring displays of different computers connect both ways, without
     switching the shared monitor; an edge that the OS layout leads onto the
     shared monitor works while that monitor shows another computer;
   - a display next to the shared monitor reaches the computer the monitor is
     showing (same condition);
   - the shared monitor's outer edge "flips" it to the other computer. */
function recommendedLinks(M, order) {
  const links = Object.fromEntries(M.machines.map((m) => [m.name, []]));
  const disp = Object.fromEntries(M.displays.map((d) => [d.id, d]));
  const seq = order.map((id) => disp[id]).filter(Boolean);
  const holders = M.shared ? M.machines.filter((m) => M.shared.names[m.name]) : [];
  const add = (machine, l) => {
    const st = edgeState(M, machine, l);
    if (st.state === "blocked" && st.shared) l.when = "shared_shows_other";
    if (edgeState(M, machine, l).state === "blocked") return; // the OS layout makes this edge impossible
    links[machine].push(l);
  };
  for (let i = 0; i + 1 < seq.length; i++) {
    const L = seq[i], R = seq[i + 1];
    if (!L.shared && !R.shared) {
      if (L.machine === R.machine) continue;
      add(L.machine, { edge: "right", peer: R.machine, screen: L.osName, to_screen: R.osName, to_edge: "left", switch_monitor: false });
      add(R.machine, { edge: "left", peer: L.machine, screen: R.osName, to_screen: L.osName, to_edge: "right", switch_monitor: false });
    } else if (!L.shared || !R.shared) {
      const D = L.shared ? R : L, dEdge = L.shared ? "left" : "right", sEdge = L.shared ? "right" : "left";
      for (const h of holders) {
        if (h.name === D.machine) continue;
        add(D.machine, { edge: dEdge, peer: h.name, screen: D.osName, to_screen: M.core, to_edge: sEdge, switch_monitor: false, when: "shared_shows_other" });
        add(h.name, { edge: sEdge, peer: D.machine, screen: M.core, to_screen: D.osName, to_edge: dEdge, switch_monitor: false });
      }
    }
  }
  if (M.shared) {
    const idx = order.indexOf("shared");
    const outer = idx === order.length - 1 ? "right" : idx === 0 ? "left" : null;
    if (outer) for (const a of holders) for (const b of holders) if (a !== b) {
      links[a.name].push({ edge: outer, peer: b.name, screen: M.core, to_screen: M.core, to_edge: outer, dwell_ms: 250, switch_monitor: true });
    }
  }
  return links;
}

/* Plain-language description of a connection. */
function describeLink(M, machine, l) {
  const scr = (m, s) => (isShared(M, s) ? shortScreenName(M.shared ? M.shared.name : s) : `${m}'s ${shortScreenName(s).toLowerCase()}`);
  const flip = isShared(M, l.screen) && isShared(M, l.to_screen);
  if (flip) return `Push off the ${l.edge} edge of the ${shortScreenName(M.shared.name)} to hand it, and the pointer, to ${l.peer}.`;
  return `Leaving the ${l.edge} edge of ${scr(machine, l.screen)} arrives at the ${l.to_edge || "opposite"} edge of ${scr(l.peer, l.to_screen)} on ${l.peer}.`;
}

function startEditing() {
  if (!S.plan) { S.plan = JSON.parse(JSON.stringify(allLinks())); S.savedPlan = JSON.parse(JSON.stringify(S.plan)); }
}
function planChanged() {
  const changed = S.plan && JSON.stringify(S.plan) !== JSON.stringify(S.savedPlan);
  $("#plan").hidden = !changed;
  if (changed) {
    const n = Object.values(S.plan).reduce((s, l) => s + l.length, 0);
    $("#plan-text").textContent = `${n} connection${n === 1 ? "" : "s"}, not saved yet`;
  }
}

/* ------------------------------------------------------------------ desk rendering */
const desk = $("#desk");
let geom = null; // last layout: id -> {x,y,w,h}

function layoutDisplays(M, order, W, H) {
  const disp = Object.fromEntries(M.displays.map((d) => [d.id, d]));
  const items = order.map((id) => disp[id]).filter(Boolean);
  // Visual size: pixel aspect, laptop panels smaller than desk monitors.
  const sizes = items.map((d) => {
    const k = d.laptop ? 0.66 : 1;
    return { w: (d.rect.w / 2560) * 300 * k, h: (d.rect.h / 2560) * 300 * k };
  });
  let gapSame = 16, gapOther = 130;
  let total = 0;
  items.forEach((d, i) => {
    total += sizes[i].w;
    if (i) total += (items[i - 1].machine === d.machine && !d.shared ? gapSame : gapOther);
  });
  const scale = Math.min(1.35, (W - 260) / total, (H * 0.42) / Math.max(...sizes.map((s) => s.h)));
  let x = (W - total * scale) / 2;
  const base = H * 0.5;
  const out = {};
  items.forEach((d, i) => {
    if (i) x += (items[i - 1].machine === d.machine && !d.shared ? gapSame : gapOther) * scale;
    const w = sizes[i].w * scale, hh = sizes[i].h * scale;
    out[d.id] = { x, y: base - hh, w, h: hh, d };
    x += w;
  });
  return { boxes: out, base, scale };
}

function render() {
  const M = buildModel();
  desk.textContent = "";
  if (!M) return;
  if (!S.order || S.order.length !== M.displays.length || !S.order.every((id) => M.displays.some((d) => d.id === id))) {
    S.order = defaultOrder(M);
  }
  const W = desk.clientWidth || 800, H = desk.clientHeight || 520;
  desk.setAttribute("viewBox", `0 0 ${W} ${H}`);
  const defs = el("defs", {}, desk);
  const g = el("linearGradient", { id: "shine", x1: 0, y1: 0, x2: 1, y2: 1 }, defs);
  el("stop", { offset: "0", "stop-color": "#fff", "stop-opacity": ".07" }, g);
  el("stop", { offset: ".5", "stop-color": "#fff", "stop-opacity": "0" }, g);

  const L = layoutDisplays(M, S.order, W, H);
  geom = L;
  const machineColor = (name) => (M.byName[name] || {}).color || "var(--ink-3)";

  // Cables underneath the displays.
  const cablesLayer = el("g", {}, desk);
  const links = allLinks();
  drawCables(M, L, links, cablesLayer, machineColor);

  // Displays.
  for (const id of S.order) {
    const b = L.boxes[id];
    if (!b) continue;
    drawDisplay(M, b, machineColor);
  }

  // Peripherals glide to the computer that has focus.
  drawPeripherals(M, L, machineColor);

  if (typeof Desk3D !== "undefined" && Desk3D.active) Desk3D.sync();
  // Hint text.
  $("#desk-hint").textContent = M.machines.length < 2
    ? "Start Switchboard on your other computer. It will appear here."
    : "Drag screens to match your desk. Drag from a jack on a screen's edge to connect it to another computer.";
}

function currentLinks() { return allLinks(); }

function findBox(M, L, machine, screenSub) {
  if (!screenSub) return null;
  if (isShared(M, screenSub)) return L.boxes.shared || null;
  const d = M.displays.find((x) => x.machine === machine && x.osName.includes(screenSub));
  return d ? L.boxes[d.id] : null;
}

const edgeX = (b, edge) => (edge === "left" ? b.x : b.x + b.w);

function drawCables(M, L, links, layer, color) {
  // Count connections per unordered display pair so opposite directions sit side by side.
  const pairKey = (a, b) => [a, b].sort().join("|");
  const counts = {};
  for (const [machine, ls] of Object.entries(links)) for (const l of ls) {
    const a = findBox(M, L, machine, l.screen), b = findBox(M, L, l.peer, l.to_screen);
    if (a && b && a !== b) counts[pairKey(a.d.id, b.d.id)] = (counts[pairKey(a.d.id, b.d.id)] || 0) + 1;
  }
  const used = {};
  for (const [machine, ls] of Object.entries(links)) {
    ls.forEach((l, li) => {
      const a = findBox(M, L, machine, l.screen);
      const b = findBox(M, L, l.peer, l.to_screen);
      if (!a) return;
      const st = edgeState(M, machine, l);
      const col = st.state === "blocked" ? "var(--coral)" : color(machine);
      let d, end, tangent;
      const ax = edgeX(a, l.edge);
      if (!b || a === b) {
        // Flip: a loop off the outer edge, one loop per computer, stacked.
        const k = (used["flip" + a.d.id] = (used["flip" + a.d.id] || 0) + 1) - 1;
        const dir = l.edge === "left" ? -1 : 1;
        const ay = a.y + a.h * (0.38 + k * 0.26);
        const by = ay + a.h * 0.18;
        d = `M ${ax} ${ay} c ${dir * (64 + k * 18)} -26, ${dir * (64 + k * 18)} ${by - ay + 26}, ${dir * 6} ${by - ay}`;
        end = [ax + dir * 6, by];
        tangent = [-dir, 0];
      } else {
        const key = pairKey(a.d.id, b.d.id);
        const n = counts[key] || 1, k = (used[key] = (used[key] || 0) + 1) - 1;
        const off = (k - (n - 1) / 2) * 18;
        const ay = a.y + a.h * 0.55 + off;
        const bx = l.to_edge === "left" ? b.x : l.to_edge === "right" ? b.x + b.w : b.x + b.w / 2;
        const by = b.y + b.h * 0.55 + off;
        const sag = Math.max(40, Math.abs(bx - ax) * 0.4) + off * 0.6;
        const c1x = ax + (l.edge === "left" ? -34 : 34), c2x = bx + (l.to_edge === "left" ? -34 : 34);
        d = `M ${ax} ${ay} C ${c1x} ${ay + sag}, ${c2x} ${by + sag}, ${bx} ${by}`;
        end = [bx, by];
        const tx = bx - c2x, ty = by - (by + sag), len = Math.hypot(tx, ty) || 1;
        tangent = [tx / len, ty / len];
      }
      const path = el("path", { d, class: "cable" + (st.state === "blocked" ? " blocked" : "") + (st.state === "conditional" ? " conditional" : ""), stroke: col }, layer);
      path.dataset.machine = machine;
      // Arrowhead at the destination.
      const [ex, ey] = end, [tx, ty] = tangent;
      const s = 9, px = -ty, py = tx;
      el("path", { d: `M ${ex} ${ey} L ${ex - tx * s + px * s * 0.55} ${ey - ty * s + py * s * 0.55} L ${ex - tx * s - px * s * 0.55} ${ey - ty * s - py * s * 0.55} Z`, fill: col, class: "arrow" }, layer);
      el("circle", { cx: ax, cy: Number(d.split(" ")[2]), r: 5, class: "jack", fill: "var(--brass)" }, layer);
      if (st.state === "conditional") {
        // Small monitor badge: "only while the shared monitor shows the other computer".
        const mid = path.getPointAtLength(path.getTotalLength() * 0.5);
        const g = el("g", { transform: `translate(${mid.x - 9},${mid.y - 7})`, class: "badge" }, layer);
        el("rect", { x: 0, y: 0, width: 18, height: 12, rx: 2.5 }, g);
        el("rect", { x: 7, y: 12, width: 4, height: 3 }, g);
      }
      const hit = el("path", { d, class: "cable-hit" }, layer);
      const sel = S.sel && S.sel.kind === "link" && S.sel.machine === machine && S.sel.index === li;
      if (sel) path.classList.add("sel");
      hit.addEventListener("click", (e) => { e.stopPropagation(); select({ kind: "link", machine, index: li }); });
    });
  }
}

/* Edge jacks: drag from one to another computer's display to connect. */
function drawJacks(M, b, g) {
  if (M.machines.length < 2) return;
  for (const edge of ["left", "right"]) {
    const x = edge === "left" ? 0 : b.w;
    const j = el("circle", { cx: x, cy: b.h * 0.55, r: 7, class: "edge-jack", tabindex: -1 }, g);
    const t = el("title", {}, j);
    t.textContent = `Drag to connect the ${edge} edge to another computer`;
    j.addEventListener("pointerdown", (e) => { e.stopPropagation(); startConnect(e, b, edge); });
  }
}

function drawDisplay(M, b, color) {
  const d = b.d;
  const g = el("g", { class: "disp" + (S.sel && S.sel.id === d.id ? " sel" : ""), transform: `translate(${b.x},${b.y})`, tabindex: 0, role: "button", "aria-label": d.name }, desk);
  g.dataset.id = d.id;
  const owner = d.shared ? M.showing : M.byName[d.machine];
  const tint = owner ? owner.color : null;
  // Glass, lit in the owner's colour.
  const glass = el("rect", { class: "glass", x: 0, y: 0, width: b.w, height: b.h, rx: 6 }, g);
  if (tint) glass.style.fill = `color-mix(in srgb, ${tint} 22%, var(--screen-off))`;
  // A few "windows" inside to suggest a live desktop.
  if (tint) {
    const pad = Math.max(6, b.w * 0.05);
    el("rect", { x: pad, y: pad, width: b.w * 0.42, height: b.h * 0.5, rx: 3, fill: tint, opacity: 0.28 }, g);
    el("rect", { x: b.w * 0.5, y: b.h * 0.24, width: b.w * 0.42, height: b.h * 0.52, rx: 3, fill: tint, opacity: 0.18 }, g);
  }
  el("rect", { class: "shine", x: 3, y: 3, width: b.w - 6, height: b.h - 6, rx: 4 }, g);
  if (d.laptop) {
    el("path", { class: "base", d: `M ${-b.w * 0.06} ${b.h + 3} h ${b.w * 1.12} l ${-b.w * 0.04} ${8} h ${-b.w * 1.04} z` }, g);
  } else {
    el("rect", { class: "stand", x: b.w / 2 - 6, y: b.h, width: 12, height: 22 }, g);
    el("rect", { class: "stand", x: b.w / 2 - b.w * 0.14, y: b.h + 20, width: b.w * 0.28, height: 6, rx: 3 }, g);
  }
  // Labels under the display.
  const ly = b.h + (d.laptop ? 34 : 50);
  const label = el("text", { class: "label", x: b.w / 2, y: ly, "text-anchor": "middle" }, g);
  label.textContent = d.name;
  const sub = el("text", { class: "sub", x: b.w / 2, y: ly + 18, "text-anchor": "middle" }, g);
  if (d.shared) {
    sub.textContent = M.showing ? `Showing ${M.showing.name}` : "Shared monitor";
    // Input ports.
    const holders = M.machines.filter((m) => d.names[m.name]);
    holders.forEach((m, i) => {
      const t = el("text", { class: "port" + (M.showing === m ? " live" : ""), x: b.w / 2 + (i - (holders.length - 1) / 2) * 64, y: ly + 38, "text-anchor": "middle" }, g);
      t.textContent = inputLabel(m.input);
      if (M.showing === m) t.style.fill = m.color;
    });
  } else {
    const m = M.byName[d.machine];
    sub.textContent = m ? (m.me ? `This ${friendlyOs(m.os)} computer` : `${friendlyOs(m.os)} computer`) : "";
    sub.style.fill = m ? m.color : "";
  }
  drawJacks(M, b, g);
  // Interaction: select + drag to reorder.
  g.addEventListener("pointerdown", (e) => startDrag(e, d.id, g, b));
  g.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === " ") { select({ kind: "display", id: d.id }); e.preventDefault(); }
    if (e.key === "ArrowLeft" || e.key === "ArrowRight") { nudge(d.id, e.key === "ArrowLeft" ? -1 : 1); e.preventDefault(); }
  });
}

function drawPeripherals(M, L, color) {
  // Anchor under the focus computer's displays (or the shared one if it shows that computer).
  const fm = M.byName[M.focus] || M.machines[0];
  let boxes = M.displays.filter((d) => d.machine === fm.name).map((d) => L.boxes[d.id]).filter(Boolean);
  if (M.shared && M.showing === fm && L.boxes.shared) boxes = [L.boxes.shared];
  if (!boxes.length) return;
  const cx = boxes.reduce((s, b) => s + b.x + b.w / 2, 0) / boxes.length;
  const y = L.base + 118;
  const g = el("g", { class: "peripherals", "aria-label": `Mouse and keyboard are on ${fm.name}` }, desk);
  g.style.transform = `translate(${cx}px, ${y}px)`;
  // Previous position for a smooth glide.
  if (drawPeripherals.last != null && drawPeripherals.last !== cx) {
    g.style.transition = "none";
    g.style.transform = `translate(${drawPeripherals.last}px, ${y}px)`;
    requestAnimationFrame(() => requestAnimationFrame(() => { g.style.transition = ""; g.style.transform = `translate(${cx}px, ${y}px)`; }));
  }
  drawPeripherals.last = cx;
  el("ellipse", { class: "glow", cx: 0, cy: 22, rx: 120, ry: 16, fill: fm.color }, g);
  // Keyboard.
  el("rect", { class: "body", x: -112, y: 0, width: 168, height: 44, rx: 8 }, g);
  for (let r = 0; r < 3; r++) for (let c = 0; c < 11; c++) el("rect", { class: "keys", x: -104 + c * 14.5, y: 6 + r * 12, width: 11, height: 8.5, rx: 2 }, g);
  // Mouse.
  el("rect", { class: "body", x: 74, y: 2, width: 28, height: 42, rx: 14 }, g);
  el("line", { x1: 88, y1: 4, x2: 88, y2: 18, stroke: "var(--line)", "stroke-width": 1.2 }, g);
}

/* ------------------------------------------------------------------ drag: arrange displays, draw connections */
let drag = null;
function svgPoint(e) {
  const r = desk.getBoundingClientRect(), vb = desk.viewBox.baseVal;
  return [((e.clientX - r.left) / r.width) * vb.width, ((e.clientY - r.top) / r.height) * vb.height];
}

function startDrag(e, id, g, box) {
  if (e.button !== 0) return;
  drag = { kind: "move", id, g, box, x0: e.clientX, moved: false };
  g.setPointerCapture(e.pointerId);
  g.addEventListener("pointermove", onDrag);
  g.addEventListener("pointerup", endDrag, { once: true });
}
function onDrag(e) {
  if (!drag || drag.kind !== "move") return;
  const dx = (e.clientX - drag.x0) * (desk.viewBox.baseVal.width / desk.clientWidth);
  if (Math.abs(dx) > 4) drag.moved = true;
  if (!drag.moved) return;
  drag.g.classList.add("dragging");
  drag.g.setAttribute("transform", `translate(${drag.box.x + dx},${drag.box.y - 8})`);
  drag.dx = dx;
}
function endDrag() {
  const d = drag;
  drag = null;
  if (!d) return;
  d.g.removeEventListener("pointermove", onDrag);
  if (!d.moved) { select({ kind: "display", id: d.id }); return; }
  const center = d.box.x + d.box.w / 2 + d.dx;
  const others = S.order.filter((x) => x !== d.id);
  let idx = others.findIndex((x) => { const b = geom.boxes[x]; return b && center < b.x + b.w / 2; });
  if (idx < 0) idx = others.length;
  others.splice(idx, 0, d.id);
  reorder(others);
}
function nudge(id, dir) {
  const o = S.order.slice();
  const i = o.indexOf(id), j = i + dir;
  if (j < 0 || j >= o.length) return;
  [o[i], o[j]] = [o[j], o[i]];
  reorder(o);
}
/* Arranging only changes the picture (and the recommended connections);
   it never rewires anything by itself. */
function reorder(order) {
  S.order = order;
  try { localStorage.setItem("desk-order", JSON.stringify(order)); } catch (_) { /* ignore */ }
  render();
}

/* Drag from an edge jack to another computer's display. */
function startConnect(e, box, edge) {
  const [x0, y0] = [edgeX(box, edge), box.y + box.h * 0.55];
  const rubber = el("path", { class: "rubber", d: "" }, desk);
  const move = (ev) => {
    const [x, y] = svgPoint(ev);
    rubber.setAttribute("d", `M ${x0} ${y0} C ${x0 + (edge === "left" ? -40 : 40)} ${y0 + 50}, ${x} ${y + 50}, ${x} ${y}`);
  };
  const up = (ev) => {
    window.removeEventListener("pointermove", move);
    rubber.remove();
    const [x] = svgPoint(ev);
    const hit = document.elementFromPoint(ev.clientX, ev.clientY);
    const g = hit && hit.closest && hit.closest(".disp");
    if (!g) return;
    const target = geom.boxes[g.dataset.id];
    if (!target) return;
    const toEdge = x < target.x + target.w / 2 ? "left" : "right";
    addConnection(box.d, edge, target.d, toEdge);
  };
  window.addEventListener("pointermove", move);
  window.addEventListener("pointerup", up, { once: true });
  move(e);
}

function addConnection(src, edge, dst, toEdge) {
  const M = buildModel();
  const holders = (d) => (d.shared ? M.machines.filter((m) => d.names[m.name]).map((m) => m.name) : [d.machine]);
  const screenOf = (d) => (d.shared ? M.core : d.osName);
  const pairs = [];
  for (const a of holders(src)) for (const b of holders(dst)) if (a !== b) pairs.push([a, b]);
  if (!pairs.length) {
    toast(`Both are ${src.machine || dst.machine}'s displays. Moving between them is handled by its own display settings.`);
    return;
  }
  startEditing();
  for (const [a, b] of pairs) {
    const l = { edge, peer: b, screen: screenOf(src), to_screen: screenOf(dst), to_edge: toEdge, switch_monitor: !!dst.shared };
    if (src.shared && dst.shared) l.dwell_ms = 250;
    const st = edgeState(M, a, l);
    if (st.state === "blocked" && st.shared) l.when = "shared_shows_other";
    // Replace an existing connection from the same edge of the same screen.
    S.plan[a] = (S.plan[a] || []).filter((x) => !(x.edge === l.edge && x.screen === l.screen && x.peer === l.peer));
    S.plan[a].push(l);
    S.sel = { kind: "link", machine: a, index: S.plan[a].length - 1 };
  }
  planChanged();
  render();
  renderInspector();
}

$("#plan-reset").addEventListener("click", () => {
  S.plan = null; S.savedPlan = null; S.sel = null; $("#plan").hidden = true; render(); renderInspector();
});
$("#plan-apply").addEventListener("click", async () => {
  const M = buildModel();
  const me = M.machines[0].name;
  const peers = M.machines.slice(1).map((m) => ({ name: m.name, links: S.plan[m.name] || [] }));
  const r = await sb.call({ cmd: "layout", links: S.plan[me] || [], peers });
  if (r.ok === false) { toast(r.error || "Couldn't save the connections."); return; }
  S.plan = null; S.savedPlan = null; $("#plan").hidden = true;
  toast("Connections saved on both computers.");
  setTimeout(refresh, 400);
});

/* ------------------------------------------------------------------ inspector */
function select(sel) { S.sel = sel; render(); renderInspector(); }
desk.addEventListener("click", (e) => { if (e.target === desk) select(null); });

function renderInspector() {
  const box = $("#inspector");
  const M = buildModel();
  if (!M) {
    box.replaceChildren(h(`<h2>Not running</h2><p class="kind">The Switchboard service isn't responding.</p><div class="actions"><button class="primary" id="start-svc">Start Switchboard</button></div>`));
    $("#start-svc").onclick = async () => { await sb.call({ cmd: "app.start_service" }); refresh(); };
    return;
  }
  const sel = S.sel;
  if (sel && sel.kind === "display") {
    const d = M.displays.find((x) => x.id === sel.id);
    if (d && d.shared) return box.replaceChildren(sharedPanel(M, d));
    if (d) return box.replaceChildren(machinePanel(M, M.byName[d.machine], d));
  }
  if (sel && sel.kind === "link") return box.replaceChildren(linkPanel(M, sel.machine, sel.index));
  box.replaceChildren(bindOverview(overviewPanel(M), M));
}

function rttBlock(p) {
  if (!p) return "";
  const p50 = p.rtt_ms.p50, p99 = p.rtt_ms.p99;
  return `<div class="meter"><span class="big">${p50 ? p50.toFixed(1) : "–"}<small>ms</small></span><span class="p99">${p99 ? `${p99.toFixed(0)} ms at worst (p99)` : ""}</span></div>
    <p class="muted">Round trip over your network. Input is sent the moment it happens; Switchboard itself adds about 0.02 ms.</p>`;
}

function overviewPanel(M) {
  const st = S.status;
  const peer = M.machines[1];
  const hk = (S.config && S.config.hotkeys) || {};
  const keys = (s) => (s || "").split("+").map((k) => `<kbd>${esc(k.charAt(0).toUpperCase() + k.slice(1))}</kbd>`).join(" ");
  return h(`
    <h2>${peer ? `${esc(M.machines[0].name)} and ${esc(peer.name)}` : esc(M.machines[0].name)}</h2>
    <p class="kind">${peer ? (M.focus === M.machines[0].name ? "The pointer is on this computer." : `The pointer is on ${esc(M.focus)}.`) : "Waiting for your other computer."}</p>
    ${peer ? rttBlock(peer.peer) : `<div class="note">Install and open Switchboard on the other computer. When it's on the same network it shows up at the top of the desk, ready to pair.</div>`}
    ${peer ? `<h3>Connections</h3>
    <p class="muted">Each cable is one way: the pointer leaves where the jack is and arrives at the arrow. Drag from a jack on a screen's edge to another computer's screen to add one; click a cable to change it.</p>
    <div class="actions"><button class="quiet" data-act="recommend">Use recommended connections</button></div>` : ""}
    <h3>Moving between computers</h3>
    <ul class="tips">
      <li>Push the pointer through a cabled edge.</li>
      <li>${keys(hk.toggle)} sends it to the other computer or brings it back.</li>
      <li>The monitor's own input button works too.</li>
      <li>${keys(hk.emergency)} always returns control to the computer you're typing on.</li>
    </ul>
    <h3>Right now</h3>
    <dl class="facts">
      <dt>Keyboard</dt><dd>${st.keyboard_to ? `Typing on ${esc(st.keyboard_to)}` : "Typing here"}</dd>
      <dt>Switches</dt><dd>${st.stats.switches}</dd>
      <dt>Memory</dt><dd>${(st.stats.rss_bytes / 1048576).toFixed(1)} MB</dd>
    </dl>`);
}

function bindOverview(frag, M) {
  frag.querySelector('[data-act="recommend"]')?.addEventListener("click", () => {
    startEditing();
    S.plan = recommendedLinks(M, S.order);
    S.sel = null;
    planChanged();
    render();
    renderInspector();
  });
  return frag;
}

function machinePanel(M, m, d) {
  if (!m) return h(`<h2>${esc(d.name)}</h2>`);
  const p = m.peer;
  const here = M.focus === m.name;
  const frag = h(`
    <h2>${esc(m.name)}</h2>
    <p class="kind">${m.me ? `This ${friendlyOs(m.os)} computer` : `${friendlyOs(m.os)} computer at ${esc((p.address || "").split(":")[0])}`}</p>
    ${p ? rttBlock(p) : ""}
    <h3>${esc(d.name)}</h3>
    <dl class="facts">
      <dt>Resolution</dt><dd>${d.rect.w} × ${d.rect.h}</dd>
      <dt>Position</dt><dd>${d.rect.x}, ${d.rect.y} in ${friendlyOs(m.os)}'s layout</dd>
    </dl>
    <div class="actions">
      ${here ? "" : `<button class="primary" data-act="switch">Move the pointer here</button>`}
      ${!m.me && p ? `<button class="ghost" data-act="forget">Unpair</button>` : ""}
    </div>`);
  frag.querySelector('[data-act="switch"]')?.addEventListener("click", () => sb.call({ cmd: "switch", to: m.me ? "local" : m.name }).then(refresh));
  frag.querySelector('[data-act="forget"]')?.addEventListener("click", async () => {
    if (!confirm(`Unpair ${m.name}? You'll need to pair again to share the mouse.`)) return;
    await sb.call({ cmd: "forget", peer: m.name }); refresh();
  });
  return frag;
}

function sharedPanel(M, d) {
  const holders = M.machines.filter((m) => d.names[m.name]);
  const ms = S.status.monitor || {};
  const cfg = S.config || { monitor: {} };
  const frag = h(`
    <h2>${esc(d.name)}</h2>
    <p class="kind">Shared by ${holders.map((m) => esc(m.name)).join(" and ")}</p>
    <div class="inputs">
      ${holders.map((m) => `<div class="input-row${M.showing === m ? " live" : ""}"><span class="swatch" style="background:${m.color}"></span>
        <span class="grow"><b>${inputLabel(m.input)}</b><br><span class="muted">${esc(m.name)}</span></span>
        ${M.showing === m ? `<span class="muted">On screen</span>` : `<button class="quiet" data-input="${m.input}">Show</button>`}</div>`).join("")}
    </div>
    <div class="row"><div><div class="t">Switch with the pointer</div><div class="d">The monitor follows the pointer to whichever computer has it.</div></div>
      <label class="switch"><input type="checkbox" data-set="monitor.enabled" ${cfg.monitor.enabled !== false ? "checked" : ""}></label></div>
    <div class="row"><div><div class="t">Follow the monitor's buttons</div><div class="d">Switching input on the monitor moves the pointer and keyboard too.</div></div>
      <label class="switch"><input type="checkbox" data-set="monitor.follow_manual_switch" ${cfg.monitor.follow_manual_switch !== false ? "checked" : ""}></label></div>
    <dl class="facts" style="margin-top:14px">
      <dt>Last switch</dt><dd>${ms.last_ms != null ? `${ms.last_ms} ms to send the command` : "None yet"}</dd>
    </dl>
    <p class="muted">The monitor itself takes a second or two to show the new picture after a switch.</p>`);
  frag.querySelectorAll("[data-input]").forEach((b) => b.addEventListener("click", () => sb.call({ cmd: "monitor_set", input: "0x" + Number(b.dataset.input).toString(16) }).then(refresh)));
  bindSettings(frag);
  return frag;
}

function linkPanel(M, machine, index) {
  const l = (allLinks()[machine] || [])[index];
  if (!l) return overviewPanel(M);
  const st = edgeState(M, machine, l);
  const flip = isShared(M, l.screen) && isShared(M, l.to_screen);
  const mon = M.shared ? shortScreenName(M.shared.name) : null;
  const nearShared = st.by && isShared(M, (beyond(M, machine, l.screen, l.edge) || {}).name);
  const frag = h(`
    <h2>${flip ? "Monitor flip" : `${esc(machine)} to ${esc(l.peer)}`}</h2>
    <p class="kind">${esc(describeLink(M, machine, l))}</p>
    ${st.state === "conditional" ? `<div class="note">Works while the ${esc(mon)} shows another computer. While it shows ${esc(machine)}, the pointer moves onto it instead, because ${esc(friendlyOs(M.byName[machine].os))} places it beyond this edge.</div>` : ""}
    ${st.state === "blocked" ? `<div class="note warn">${esc(friendlyOs(M.byName[machine].os))} places ${esc(st.by)} beyond this edge, so the pointer can't leave here${nearShared ? `. Turn on "Only while the ${esc(mon)} shows another computer" to use it whenever the ${esc(mon)} isn't showing ${esc(machine)}.` : `. Rearrange the displays in ${esc(friendlyOs(M.byName[machine].os))}'s settings, or use another edge.`}</div>` : ""}
    ${M.shared ? `<div class="row"><div><div class="t">Switch the ${esc(mon)} to ${esc(l.peer)}</div><div class="d">Off: the monitor keeps showing whatever it shows now.</div></div>
      <label class="switch"><input type="checkbox" data-k="switch_monitor" ${l.switch_monitor !== false ? "checked" : ""}></label></div>` : ""}
    ${M.shared && (nearShared || l.when) ? `<div class="row"><div><div class="t">Only while the ${esc(mon)} shows another computer</div><div class="d">Lets this edge cross over when the monitor beyond it isn't showing ${esc(machine)}.</div></div>
      <label class="switch"><input type="checkbox" data-k="when" ${l.when === "shared_shows_other" ? "checked" : ""}></label></div>` : ""}
    <div class="row"><div><div class="t">Hold against the edge</div><div class="d">Guards against crossing by accident.</div></div>
      <div style="display:flex;align-items:center;gap:10px"><input type="range" min="0" max="600" step="50" value="${l.dwell_ms || 0}" data-k="dwell_ms"><span class="range-val">${l.dwell_ms ? l.dwell_ms + " ms" : "Off"}</span></div></div>
    <div class="actions"><button class="ghost" data-act="delete">Delete connection</button></div>`);
  const edit = (fn) => {
    startEditing();
    const t = S.plan[machine][index];
    fn(t);
    planChanged();
    render();
    renderInspector();
  };
  frag.querySelectorAll("[data-k]").forEach((inp) => {
    const k = inp.dataset.k;
    if (inp.type === "range") inp.addEventListener("input", () => { inp.nextElementSibling.textContent = inp.value > 0 ? inp.value + " ms" : "Off"; });
    inp.addEventListener("change", () => edit((t) => {
      if (k === "switch_monitor") t.switch_monitor = inp.checked;
      if (k === "when") { if (inp.checked) t.when = "shared_shows_other"; else delete t.when; }
      if (k === "dwell_ms") { const v = Number(inp.value); if (v > 0) t.dwell_ms = v; else delete t.dwell_ms; }
    }));
  });
  frag.querySelector('[data-act="delete"]').addEventListener("click", () => {
    startEditing();
    S.plan[machine].splice(index, 1);
    S.sel = null;
    planChanged();
    render();
    renderInspector();
  });
  return frag;
}

/* ------------------------------------------------------------------ settings */
function getPath(o, p) { return p.split(".").reduce((a, k) => (a == null ? a : a[k]), o); }
function setPathObj(p, v) { return p.split(".").reverse().reduce((acc, k) => ({ [k]: acc }), v); }

function bindSettings(root) {
  root.querySelectorAll("[data-set]").forEach((inp) => {
    inp.addEventListener("change", async () => {
      const v = inp.type === "checkbox" ? inp.checked : Number(inp.value);
      const r = await sb.call({ cmd: "settings", settings: setPathObj(inp.dataset.set, v) });
      if (r.ok === false) toast(r.error || "Couldn't save that setting.");
      refreshConfig();
    });
    if (inp.type === "range") {
      const out = root.querySelector(`[data-out="${inp.dataset.set}"]`);
      inp.addEventListener("input", () => { if (out) out.textContent = inp.value + (inp.dataset.unit || ""); });
    }
  });
}

function settingsBody() {
  const c = S.config || {};
  const sw = (path, title, desc) => `<div class="row"><div><div class="t">${title}</div><div class="d">${desc}</div></div>
    <label class="switch"><input type="checkbox" data-set="${path}" ${getPath(c, path) ? "checked" : ""}></label></div>`;
  const range = (path, title, desc, min, max, step, unit) => `<div class="row"><div><div class="t">${title}</div><div class="d">${desc}</div></div>
    <div style="display:flex;align-items:center;gap:10px"><input type="range" min="${min}" max="${max}" step="${step}" value="${getPath(c, path) ?? 0}" data-set="${path}" data-unit="${unit}"><span class="range-val" data-out="${path}">${getPath(c, path) ?? 0}${unit}</span></div></div>`;
  const frag = h(`
    <h3>General</h3>
    <div class="row"><div><div class="t">Start at login</div><div class="d">Switchboard runs quietly in the tray.</div></div>
      <label class="switch"><input type="checkbox" id="autostart" ${S.autostart ? "checked" : ""}></label></div>
    <h3>Pointer</h3>
    ${range("switching.edge_resistance", "Edge resistance", "How far you push past an edge before the pointer crosses.", 0, 300, 10, " px")}
    ${range("switching.corner_deadzone", "Corner gap", "Screen corners never cross, so hot corners and close buttons stay easy to hit.", 0, 80, 2, " px")}
    ${sw("switching.lock_while_button_held", "Stay put while dragging", "The pointer won't cross while a mouse button is held.")}
    <h3>Keyboard</h3>
    ${sw("keyboard.follow_pointer", "Keyboard follows the pointer", "Type on whichever computer has the pointer, wherever the keyboard is connected.")}
    <h3>Clipboard</h3>
    ${sw("clipboard.enabled", "Share the clipboard", "Copy on one computer, paste on the other.")}
    ${sw("clipboard.text", "Text", "Sent as soon as you copy.")}
    ${sw("clipboard.images", "Images", "Sent as soon as you copy.")}
    ${sw("clipboard.files", "Files and folders", "Sent when the pointer moves to the other computer.")}
    <div class="row"><div><div class="t">Received files</div><div class="d">Each transfer gets its own folder in Downloads › Switchboard.</div></div>
      <button class="quiet" id="open-downloads">Open folder</button></div>
    <h3>This computer</h3>
    <dl class="facts"><dt>Name</dt><dd>${esc(S.status?.name)}</dd><dt>Fingerprint</dt><dd>${esc(S.status?.fingerprint)}</dd><dt>Version</dt><dd>${esc(S.status?.version)}</dd></dl>`);
  bindSettings(frag);
  frag.querySelector("#autostart").addEventListener("change", async (e) => {
    const r = await sb.call({ cmd: "app.autostart_set", enabled: e.target.checked });
    if (r.ok === false) { toast(r.error); e.target.checked = !e.target.checked; }
    S.autostart = e.target.checked;
  });
  frag.querySelector("#open-downloads").addEventListener("click", () => sb.call({ cmd: "app.open_downloads" }));
  return frag;
}

$("#open-settings").addEventListener("click", async () => {
  await refreshConfig();
  S.autostart = (await sb.call({ cmd: "app.autostart_get" })).enabled;
  $("#settings-body").replaceChildren(settingsBody());
  $("#settings").hidden = false;
  $("#close-settings").focus();
});
$("#close-settings").addEventListener("click", () => { $("#settings").hidden = true; });
document.addEventListener("keydown", (e) => { if (e.key === "Escape") { $("#settings").hidden = true; } });
$("#settings").addEventListener("click", (e) => { if (e.target.id === "settings") $("#settings").hidden = true; });

$("#pause").addEventListener("click", async () => {
  await sb.call({ cmd: S.status && S.status.disabled ? "enable" : "disable" });
  refresh();
});

/* ------------------------------------------------------------------ pairing & nearby */
function renderNearby() {
  const box = $("#nearby");
  const st = S.status;
  const items = (st && st.discovered || []).filter((d) => !d.paired && !(st.peers || []).some((p) => p.fingerprint === d.fingerprint));
  if (!items.length) { box.hidden = true; box.textContent = ""; return; }
  box.hidden = false;
  box.replaceChildren(...items.map((d) => {
    const c = h(`<div class="chip"><span><span class="n">${esc(d.name)}</span> <span class="a">${esc(d.address.split(":")[0])}</span></span><button class="primary">Pair</button></div>`).firstElementChild;
    c.querySelector("button").addEventListener("click", async (e) => {
      e.target.disabled = true; e.target.textContent = "Connecting…";
      await sb.call({ cmd: "pair", address: d.address.split(":")[0] });
    });
    return c;
  }));
}

function renderPairing() {
  const st = S.status;
  const p = st && (st.peers || []).find((x) => !x.trusted && x.pairing_code);
  const sheet = $("#pairing");
  if (!p) { sheet.hidden = true; S.pairingShown = null; return; }
  if (S.pairingShown !== p.fingerprint) {
    S.pairingShown = p.fingerprint;
    $("#pair-name").textContent = p.name;
    $("#pair-code").textContent = p.pairing_code;
    $("#pair-accept").disabled = false;
  }
  $("#pair-note").textContent = p.confirmed_here ? `Confirmed here. Now confirm on ${p.name}.` : "Then confirm on both computers.";
  sheet.hidden = false;
}
$("#pair-accept").addEventListener("click", async () => {
  $("#pair-accept").disabled = true;
  const r = await sb.call({ cmd: "confirm", code: $("#pair-code").textContent.replace(/\s/g, "") });
  if (r.ok === false) toast(r.error);
  refresh();
});
$("#pair-reject").addEventListener("click", async () => { await sb.call({ cmd: "confirm", accept: false }); $("#pairing").hidden = true; refresh(); });

/* ------------------------------------------------------------------ status bar & toasts */
function renderBar() {
  const st = S.status, bar = $("#status");
  if (!st) { bar.innerHTML = `<span class="dot off"></span>The Switchboard service isn't running`; return; }
  $("#pause").textContent = st.disabled ? "Resume" : "Pause";
  const peer = (st.peers || []).find((p) => p.trusted);
  if (st.disabled) bar.innerHTML = `<span class="dot"></span>Paused. The mouse and keyboard stay on each computer.`;
  else if (!peer) bar.innerHTML = `<span class="dot"></span>Looking for your other computer`;
  else if (st.mode !== "local") bar.innerHTML = `<span class="dot away"></span>${esc(st.mode.charAt(0).toUpperCase() + st.mode.slice(1))}`;
  else bar.innerHTML = `<span class="dot on"></span>Connected to <b>${esc(peer.name)}</b> &nbsp;${peer.rtt_ms.p50 ? peer.rtt_ms.p50.toFixed(1) + " ms" : ""}`;
}

let toastTimer = null;
function toast(msg, progress) {
  const t = $("#toast");
  t.hidden = false;
  t.innerHTML = esc(msg) + (progress != null ? `<div class="bar-line"><i style="width:${Math.round(progress * 100)}%"></i></div>` : "");
  clearTimeout(toastTimer);
  if (progress == null) toastTimer = setTimeout(() => { t.hidden = true; }, 3500);
}

function renderTransfers() {
  const ts = (S.status && S.status.transfers || []).filter((t) => !t.done);
  if (!ts.length) return;
  const t = ts[0];
  const mib = (b) => (b / 1048576).toFixed(b > 10485760 ? 0 : 1);
  toast(`${t.outgoing ? "Sending files to" : "Receiving files from"} ${t.peer}: ${mib(t.bytes)} of ${mib(t.total)} MB`, t.total ? t.bytes / t.total : 0);
}

/* ------------------------------------------------------------------ loop */
async function refresh() {
  const st = await sb.call({ cmd: "status" });
  if (!st || st.ok === false || st.error) { S.status = null; S.offline = true; }
  else {
    const flash = S.lastSwitches != null && st.stats.switches !== S.lastSwitches;
    S.lastSwitches = st.stats.switches;
    const changed = JSON.stringify([st.mode, st.focus, st.links, st.screens, (st.peers || []).map((p) => [p.name, p.trusted, p.screens, p.monitor_input, p.links]), st.monitor && st.monitor.observed, st.disabled]) !== refresh.sig;
    refresh.sig = JSON.stringify([st.mode, st.focus, st.links, st.screens, (st.peers || []).map((p) => [p.name, p.trusted, p.screens, p.monitor_input, p.links]), st.monitor && st.monitor.observed, st.disabled]);
    S.status = st; S.offline = false;
    if (changed && !drag) render();
    if (flash) document.querySelectorAll(".cable").forEach((c) => { c.classList.remove("hot"); void c.getBBox(); c.classList.add("hot"); });
  }
  renderBar();
  renderNearby();
  renderPairing();
  renderTransfers();
  if (!document.activeElement || !$("#inspector").contains(document.activeElement)) renderInspector();
}
async function refreshConfig() {
  const c = await sb.call({ cmd: "config" });
  if (c && c.ok !== false && !c.error) S.config = c;
}

window.addEventListener("resize", () => render());
// Boot after every script has loaded (demo mode replaces the bridge first).
window.addEventListener("DOMContentLoaded", async function boot() {
  await refreshConfig();
  await refresh();
  render();
  setInterval(refresh, 700);
  setInterval(refreshConfig, 5000);
});
