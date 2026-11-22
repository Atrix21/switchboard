"use strict";
/* 3D desk: the same model, arrangement and selection as the flat view,
   rendered with three.js (bundled; no network). */

const Desk3D = (() => {
  let renderer, scene, camera, root, raf = 0, active = false;
  let objects = {};          // display id -> { group, screen, box }
  let periph = null, periphX = 0, periphTarget = 0;
  let cableGroup = null;
  let yaw = -0.3, pitch = 0.38, zoom = 1, target = new THREE.Vector3(0, 0.9, 0.4);
  let sceneWidth = 12;
  /* Distance that fits the whole desk horizontally for the current aspect. */
  const fitDist = () => {
    const hfov = 2 * Math.atan(Math.tan((camera.fov * Math.PI) / 360) * camera.aspect);
    return ((sceneWidth / 2 + 1.2) / Math.tan(hfov / 2)) * zoom;
  };
  let intro = 0;
  const ray = new THREE.Raycaster();
  const deskPlane = new THREE.Plane(new THREE.Vector3(0, 1, 0), 0);
  let drag = null;
  const container = () => document.querySelector("#desk3d");

  const css = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const colorOf = (v) => {
    // Resolve "var(--teal)" etc. to a THREE.Color.
    const m = /var\((--[\w-]+)\)/.exec(v || "");
    return new THREE.Color(m ? css(m[1]) : v || "#888");
  };

  function init() {
    const c = container();
    renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true });
    renderer.setPixelRatio(Math.min(2, window.devicePixelRatio || 1));
    renderer.shadowMap.enabled = true;
    renderer.shadowMap.type = THREE.PCFSoftShadowMap;
    renderer.outputColorSpace = THREE.SRGBColorSpace;
    c.appendChild(renderer.domElement);
    scene = new THREE.Scene();
    camera = new THREE.PerspectiveCamera(34, 1, 0.1, 100);
    scene.add(new THREE.HemisphereLight(0xdfe8f0, 0x1a2230, 0.9));
    const key = new THREE.DirectionalLight(0xffffff, 1.4);
    key.position.set(-4, 8, 6);
    key.castShadow = true;
    key.shadow.mapSize.set(2048, 2048);
    Object.assign(key.shadow.camera, { left: -10, right: 10, top: 6, bottom: -6, near: 1, far: 30 });
    key.shadow.radius = 6;
    scene.add(key);
    // Desk mat.
    const mat = new THREE.Mesh(new THREE.BoxGeometry(18, 0.12, 7.5), new THREE.MeshStandardMaterial({ color: colorOf("var(--mat-grain)"), roughness: 0.95 }));
    mat.position.y = -0.06;
    mat.receiveShadow = true;
    mat.name = "mat";
    scene.add(mat);
    root = new THREE.Group();
    scene.add(root);
    bindInput(renderer.domElement);
    new ResizeObserver(resize).observe(c);
    resize();
  }

  function resize() {
    const c = container();
    if (!renderer || !c.clientWidth) return;
    renderer.setSize(c.clientWidth, c.clientHeight, false);
    camera.aspect = c.clientWidth / c.clientHeight;
    camera.updateProjectionMatrix();
  }

  function screenTexture(tint, laptop) {
    const cv = document.createElement("canvas");
    cv.width = 256; cv.height = laptop ? 160 : 144;
    const g = cv.getContext("2d");
    const base = new THREE.Color(css("--screen-off") || "#0f151d").lerp(tint, 0.25);
    g.fillStyle = "#" + base.getHexString();
    g.fillRect(0, 0, cv.width, cv.height);
    g.fillStyle = "#" + tint.getHexString();
    g.globalAlpha = 0.55; g.fillRect(14, 14, 110, 72);
    g.globalAlpha = 0.32; g.fillRect(132, 34, 108, 84);
    g.globalAlpha = 0.9; g.fillRect(0, cv.height - 10, cv.width, 10);
    const t = new THREE.CanvasTexture(cv);
    t.colorSpace = THREE.SRGBColorSpace;
    return t;
  }

  const bezelMat = new THREE.MeshStandardMaterial({ color: 0x0b1016, roughness: 0.45, metalness: 0.3 });
  const metalMat = new THREE.MeshStandardMaterial({ color: 0x2a3340, roughness: 0.35, metalness: 0.7 });

  function buildDisplay(d, w, h, tint) {
    const g = new THREE.Group();
    const screenMat = new THREE.MeshBasicMaterial({ map: screenTexture(tint, d.laptop) });
    if (d.laptop) {
      const base = new THREE.Mesh(new THREE.BoxGeometry(w * 1.04, 0.05, h * 0.72), metalMat);
      base.position.set(0, 0.025, h * 0.36);
      base.castShadow = true; base.receiveShadow = true;
      g.add(base);
      const lid = new THREE.Group();
      const shell = new THREE.Mesh(new THREE.BoxGeometry(w, h, 0.04), bezelMat);
      shell.castShadow = true;
      const screen = new THREE.Mesh(new THREE.PlaneGeometry(w * 0.94, h * 0.9), screenMat);
      screen.position.z = 0.021;
      shell.position.y = h / 2; screen.position.y = h / 2;
      lid.add(shell, screen);
      lid.rotation.x = -0.22;
      lid.position.set(0, 0.05, 0);
      g.add(lid);
      g.userData.screen = screen;
    } else {
      const shell = new THREE.Mesh(new THREE.BoxGeometry(w, h, 0.07), bezelMat);
      shell.position.y = h / 2 + 0.55;
      shell.castShadow = true;
      const screen = new THREE.Mesh(new THREE.PlaneGeometry(w * 0.965, h * 0.94), screenMat);
      screen.position.set(0, h / 2 + 0.55, 0.036);
      const neck = new THREE.Mesh(new THREE.BoxGeometry(0.12, 0.6, 0.08), metalMat);
      neck.position.set(0, 0.3, -0.08); neck.castShadow = true;
      const foot = new THREE.Mesh(new THREE.BoxGeometry(w * 0.32, 0.03, 0.55), metalMat);
      foot.position.set(0, 0.015, 0.05); foot.castShadow = true; foot.receiveShadow = true;
      g.add(shell, screen, neck, foot);
      g.userData.screen = screen;
    }
    g.userData.id = d.id;
    g.traverse((o) => { o.userData.id = d.id; });
    return g;
  }

  function buildPeripherals() {
    const g = new THREE.Group();
    const kb = new THREE.Mesh(new THREE.BoxGeometry(1.7, 0.06, 0.55), new THREE.MeshStandardMaterial({ color: 0x2a3442, roughness: 0.6 }));
    kb.position.set(-0.35, 0.03, 0); kb.castShadow = true; kb.receiveShadow = true;
    g.add(kb);
    const keyGeo = new THREE.BoxGeometry(0.11, 0.035, 0.11);
    const keyMat = new THREE.MeshStandardMaterial({ color: 0x3a4657, roughness: 0.5 });
    const keys = new THREE.InstancedMesh(keyGeo, keyMat, 13 * 4);
    const m = new THREE.Matrix4();
    let i = 0;
    for (let r = 0; r < 4; r++) for (let c = 0; c < 13; c++) { m.makeTranslation(-1.1 + c * 0.125, 0.08, -0.19 + r * 0.125); keys.setMatrixAt(i++, m); }
    keys.castShadow = true;
    g.add(keys);
    const mouse = new THREE.Mesh(new THREE.SphereGeometry(0.16, 24, 16), new THREE.MeshStandardMaterial({ color: 0x2a3442, roughness: 0.4 }));
    mouse.scale.set(0.8, 0.45, 1.25);
    mouse.position.set(0.85, 0.07, 0.05); mouse.castShadow = true;
    g.add(mouse);
    const glow = new THREE.Mesh(new THREE.CircleGeometry(1.4, 48), new THREE.MeshBasicMaterial({ color: 0xffffff, transparent: true, opacity: 0.14 }));
    glow.rotation.x = -Math.PI / 2; glow.position.y = 0.002; glow.scale.set(1.25, 0.45, 1);
    g.add(glow);
    g.userData.glow = glow;
    return g;
  }

  /* Rebuild the scene from the current model/order. Cheap: a handful of meshes. */
  function sync() {
    if (!active) return;
    const M = buildModel();
    if (!M) return;
    if (!S.order) S.order = defaultOrder(M);
    root.clear();
    objects = {};
    // Lay displays out along x using the same proportions as the flat view.
    const L = layoutDisplays(M, S.order, 1000, 600);
    const xs = Object.values(L.boxes);
    const minX = Math.min(...xs.map((b) => b.x)), maxX = Math.max(...xs.map((b) => b.x + b.w));
    const k = 11 / (maxX - minX);
    for (const id of S.order) {
      const b = L.boxes[id];
      if (!b) continue;
      const d = b.d;
      const owner = d.shared ? M.showing : M.byName[d.machine];
      const tint = owner ? colorOf(owner.color) : new THREE.Color(0x334154);
      const w = b.w * k, h = b.h * k;
      const g = buildDisplay(d, w, h, tint);
      g.position.set((b.x + b.w / 2 - minX) * k - 5.5, 0, d.laptop ? 0.9 : -0.6);
      if (S.sel && S.sel.id === id) g.position.y = 0.04;
      root.add(g);
      objects[id] = { group: g, w, h };
    }
    // Cables as tubes, sagging to the desk.
    cableGroup = new THREE.Group();
    const links = currentLinks(M);
    const edgePoint = (o, edge) => {
      if (!o) return null;
      const p = o.group.position.clone();
      const lap = o.group.userData.screen && o.group.userData.screen.parent !== o.group;
      p.x += (edge === "left" ? -1 : 1) * o.w / 2;
      p.y = lap ? o.h * 0.45 : o.h * 0.55 + 0.55;
      return p;
    };
    for (const [machine, ls] of Object.entries(links)) {
      const col = colorOf((M.byName[machine] || {}).color);
      for (const l of ls) {
        const ab = findBox(M, L, machine, l.screen), bb = findBox(M, L, l.peer, l.to_screen);
        if (!ab) continue;
        const a = edgePoint(objects[ab.d.id], l.edge);
        let pts;
        if (!bb || ab === bb) {
          const dir = l.edge === "left" ? -1 : 1;
          pts = [a, a.clone().add(new THREE.Vector3(dir * 0.5, -0.4, 0.3)), a.clone().add(new THREE.Vector3(dir * 0.6, -1.2, 0.6)), a.clone().add(new THREE.Vector3(dir * 0.1, -a.y + 0.03, 0.9))];
        } else {
          const b = edgePoint(objects[bb.d.id], l.to_edge || "left");
          const mid = a.clone().lerp(b, 0.5);
          pts = [a, a.clone().lerp(mid, 0.45).setY(0.25).add(new THREE.Vector3(0, 0, 0.4)), mid.clone().setY(0.03).add(new THREE.Vector3(0, 0, 0.7)), b.clone().lerp(mid, 0.45).setY(0.25).add(new THREE.Vector3(0, 0, 0.4)), b];
        }
        const curve = new THREE.CatmullRomCurve3(pts);
        const blocked = blockedBy(M, machine, l);
        const tube = new THREE.Mesh(new THREE.TubeGeometry(curve, 48, 0.028, 8, false), new THREE.MeshStandardMaterial({ color: col, roughness: 0.5, transparent: blocked, opacity: blocked ? 0.35 : 1 }));
        tube.castShadow = true;
        tube.userData.link = { machine, link: l };
        cableGroup.add(tube);
        const jackMat = new THREE.MeshStandardMaterial({ color: colorOf("var(--brass)"), metalness: 0.8, roughness: 0.3 });
        for (const p of [pts[0], pts[pts.length - 1]]) {
          const j = new THREE.Mesh(new THREE.CylinderGeometry(0.05, 0.05, 0.08, 16), jackMat);
          j.position.copy(p); j.rotation.z = Math.PI / 2;
          cableGroup.add(j);
        }
      }
    }
    root.add(cableGroup);
    // Peripherals.
    periph = buildPeripherals();
    const fm = M.byName[M.focus] || M.machines[0];
    let ids = M.displays.filter((d) => d.machine === fm.name).map((d) => d.id);
    if (M.shared && M.showing === fm) ids = ["shared"];
    const xsF = ids.map((id) => objects[id] && objects[id].group.position.x).filter((x) => x != null);
    periphTarget = xsF.length ? xsF.reduce((a, b) => a + b, 0) / xsF.length : 0;
    if (sync.first !== false) { periphX = periphTarget; sync.first = false; }
    periph.position.set(periphX, 0, 2.3);
    periph.userData.glow.material.color = colorOf(fm.color);
    root.add(periph);
  }

  function frame(t) {
    raf = requestAnimationFrame(frame);
    if (intro < 1) intro = Math.min(1, intro + 0.012);
    const e = 1 - Math.pow(1 - intro, 3);
    const fd = fitDist();
    const d = fd * (1 + (1 - e) * 0.45), p = pitch + (1 - e) * 0.25, y = yaw - (1 - e) * 0.5;
    camera.position.set(target.x + d * Math.sin(y) * Math.cos(p), target.y + d * Math.sin(p), target.z + d * Math.cos(y) * Math.cos(p));
    camera.lookAt(target);
    if (periph) {
      periphX += (periphTarget - periphX) * 0.08;
      periph.position.x = periphX;
    }
    renderer.render(scene, camera);
  }

  function pick(ev, objs) {
    const r = renderer.domElement.getBoundingClientRect();
    const v = new THREE.Vector2(((ev.clientX - r.left) / r.width) * 2 - 1, -((ev.clientY - r.top) / r.height) * 2 + 1);
    ray.setFromCamera(v, camera);
    return ray.intersectObjects(objs, true)[0];
  }

  function bindInput(cv) {
    cv.addEventListener("pointerdown", (e) => {
      cv.setPointerCapture(e.pointerId);
      const hit = pick(e, Object.values(objects).map((o) => o.group));
      if (hit && hit.object.userData.id) {
        const id = hit.object.userData.id;
        const p = new THREE.Vector3();
        ray.ray.intersectPlane(deskPlane, p);
        drag = { kind: "display", id, x0: e.clientX, start: objects[id].group.position.x, px: p ? p.x : 0, moved: false };
      } else {
        const c = cableGroup && pick(e, cableGroup.children);
        if (c && c.object.userData.link) { select({ kind: "link", ...c.object.userData.link }); return; }
        drag = { kind: "orbit", x0: e.clientX, y0: e.clientY, yaw, pitch };
      }
    });
    cv.addEventListener("pointermove", (e) => {
      if (!drag) return;
      if (drag.kind === "orbit") {
        yaw = drag.yaw - (e.clientX - drag.x0) * 0.006;
        pitch = Math.max(0.12, Math.min(1.2, drag.pitch + (e.clientY - drag.y0) * 0.004));
      } else {
        if (Math.abs(e.clientX - drag.x0) > 4) drag.moved = true;
        if (!drag.moved) return;
        const p = new THREE.Vector3();
        pick(e, []);
        if (ray.ray.intersectPlane(deskPlane, p)) {
          const o = objects[drag.id];
          o.group.position.x = drag.start + (p.x - drag.px);
          o.group.position.y = 0.12;
        }
      }
    });
    cv.addEventListener("pointerup", () => {
      const d = drag;
      drag = null;
      if (!d || d.kind !== "display") return;
      if (!d.moved) { select({ kind: "display", id: d.id }); return; }
      const order = Object.entries(objects).sort((a, b) => a[1].group.position.x - b[1].group.position.x).map(([id]) => id);
      reorder(order);
    });
    cv.addEventListener("wheel", (e) => { zoom = Math.max(0.45, Math.min(1.6, zoom * (1 + Math.sign(e.deltaY) * 0.08))); e.preventDefault(); }, { passive: false });
  }

  return {
    show(on) {
      active = on;
      container().hidden = !on;
      document.querySelector("#desk").style.display = on ? "none" : "";
      if (on) {
        if (!renderer) { try { init(); } catch (err) { console.error(err); toast("3D view isn't available on this graphics setup."); return this.show(false); } }
        intro = 0; sync.first = true; resize(); sync();
        if (!raf) raf = requestAnimationFrame(frame);
      } else if (raf) { cancelAnimationFrame(raf); raf = 0; }
    },
    sync,
    get active() { return active; },
  };
})();

/* View toggle */
(function () {
  const btns = document.querySelectorAll("[data-view]");
  const set = (v) => {
    btns.forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.view === v)));
    Desk3D.show(v === "3d");
    if (v !== "3d") render();
    try { localStorage.setItem("desk-view", v); } catch (_) { /* ignore */ }
  };
  btns.forEach((b) => b.addEventListener("click", () => set(b.dataset.view)));
  let v = "flat";
  try { v = localStorage.getItem("desk-view") || "flat"; } catch (_) { /* ignore */ }
  if (window.__SB_VIEW) v = window.__SB_VIEW;
  setTimeout(() => set(v), 50);
})();
