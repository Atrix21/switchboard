//! Wayland (any compositor) backend: evdev capture + uinput injection.
//!
//! Wayland gives applications no way to see or grab global input, so this
//! backend works one level below the compositor:
//!
//! * Capture: read `/dev/input/event*` directly. While forwarding, the physical
//!   devices are grabbed (`EVIOCGRAB`) so the compositor never sees them.
//!   Needs read access to the event devices (see packaging/linux/70-switchboard-input.rules).
//! * Injection: two uinput devices (an absolute pointer and a keyboard).
//!   The pointer is absolute so that forwarded motion (which already carries
//!   the other computer's pointer acceleration) is not accelerated a second
//!   time, and so `warp` is exact.
//! * Position: Wayland cannot report the cursor position, so it is tracked
//!   here: raw deltas from physical devices, and our own absolute moves.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{CapEvent, Capture, CaptureHandler, CaptureOptions, Injector};
use crate::proto::{Button, Rect};

const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
const UI_SET_PROPBIT: libc::c_ulong = 0x4004_556e;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const REL_WHEEL_HI_RES: u16 = 11;
const REL_HWHEEL_HI_RES: u16 = 12;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const BTN_FORWARD: u16 = 0x115;
const BTN_BACK: u16 = 0x116;
const BTN_TOOL_FINGER: u16 = 0x145;
const BTN_TOUCH: u16 = 0x14a;
const ABS_MAX_VAL: i32 = 65535;

fn put(buf: &mut Vec<u8>, t: u16, code: u16, value: i32) {
    let mut e = [0u8; 24]; // struct input_event, 64-bit: timeval(16) + type + code + value
    e[16..18].copy_from_slice(&t.to_ne_bytes());
    e[18..20].copy_from_slice(&code.to_ne_bytes());
    e[20..24].copy_from_slice(&value.to_ne_bytes());
    buf.extend_from_slice(&e);
}

fn chk(r: libc::c_int) -> io::Result<()> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ------------------------------------------------------------ shared state

/// Tracked cursor position (global logical pixels) and the screen layout.
struct Track {
    x: f64,
    y: f64,
    rects: Vec<Rect>,
}

fn track() -> &'static Mutex<Track> {
    static T: OnceLock<Mutex<Track>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(Track { x: 0.0, y: 0.0, rects: vec![] }))
}

pub fn set_screens(rects: Vec<Rect>) {
    let mut t = track().lock().unwrap();
    if t.rects.is_empty() {
        if let Some(r) = rects.first() {
            t.x = (r.x + r.w / 2) as f64;
            t.y = (r.y + r.h / 2) as f64;
        }
    }
    t.rects = rects;
}

impl Track {
    fn clamp(&mut self) {
        if self.rects.is_empty() {
            return;
        }
        let (px, py) = (self.x, self.y);
        let mut best = (f64::MAX, px, py);
        for r in &self.rects {
            let cx = px.clamp(r.x as f64, (r.x + r.w - 1) as f64);
            let cy = py.clamp(r.y as f64, (r.y + r.h - 1) as f64);
            let d = (cx - px).powi(2) + (cy - py).powi(2);
            if d < best.0 {
                best = (d, cx, cy);
            }
        }
        self.x = best.1;
        self.y = best.2;
    }
    fn pos(&self) -> (i32, i32) {
        (self.x.round() as i32, self.y.round() as i32)
    }
    fn bounds(&self) -> Rect {
        crate::geom::bbox(&self.rects)
    }
}

fn pos() -> (i32, i32) {
    track().lock().unwrap().pos()
}

fn track_move(dx: f64, dy: f64) -> (i32, i32) {
    let mut t = track().lock().unwrap();
    t.x += dx;
    t.y += dy;
    t.clamp();
    t.pos()
}

fn track_set(x: i32, y: i32) {
    let mut t = track().lock().unwrap();
    t.x = x as f64;
    t.y = y as f64;
    t.clamp();
}

// ------------------------------------------------------------ uinput devices

fn uinput_open() -> io::Result<File> {
    OpenOptions::new().write(true).open("/dev/uinput").map_err(|e| io::Error::new(e.kind(), format!("/dev/uinput: {e} (needs write access)")))
}

fn setup(fd: RawFd, name: &str) -> io::Result<()> {
    #[repr(C)]
    struct Setup {
        bustype: u16,
        vendor: u16,
        product: u16,
        version: u16,
        name: [u8; 80],
        ff: u32,
    }
    let mut s = Setup { bustype: 0x06, vendor: 0x5357, product: 0x4244, version: 1, name: [0; 80], ff: 0 };
    s.name[..name.len()].copy_from_slice(name.as_bytes());
    unsafe {
        chk(libc::ioctl(fd, UI_DEV_SETUP, &s))?;
        chk(libc::ioctl(fd, UI_DEV_CREATE))
    }
}

fn create_pointer() -> io::Result<File> {
    let f = uinput_open()?;
    let fd = f.as_raw_fd();
    unsafe {
        for ev in [EV_KEY, EV_REL, EV_ABS] {
            chk(libc::ioctl(fd, UI_SET_EVBIT, ev as libc::c_int))?;
        }
        chk(libc::ioctl(fd, UI_SET_PROPBIT, 0 as libc::c_int))?; // INPUT_PROP_POINTER
        for b in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
            chk(libc::ioctl(fd, UI_SET_KEYBIT, b as libc::c_int))?;
        }
        for r in [REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
            chk(libc::ioctl(fd, UI_SET_RELBIT, r as libc::c_int))?;
        }
        #[repr(C)]
        struct AbsSetup {
            code: u16,
            value: i32,
            min: i32,
            max: i32,
            fuzz: i32,
            flat: i32,
            res: i32,
        }
        for a in [ABS_X, ABS_Y] {
            chk(libc::ioctl(fd, UI_SET_ABSBIT, a as libc::c_int))?;
            let s = AbsSetup { code: a, value: 0, min: 0, max: ABS_MAX_VAL, fuzz: 0, flat: 0, res: 0 };
            chk(libc::ioctl(fd, UI_ABS_SETUP, &s))?;
        }
    }
    setup(fd, "Switchboard Virtual Pointer")?;
    Ok(f)
}

fn create_keyboard() -> io::Result<File> {
    let f = uinput_open()?;
    let fd = f.as_raw_fd();
    unsafe {
        chk(libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int))?;
        for k in 1..=248 {
            chk(libc::ioctl(fd, UI_SET_KEYBIT, k as libc::c_int))?;
        }
    }
    setup(fd, "Switchboard Virtual Keyboard")?;
    Ok(f)
}

/// The shared absolute pointer, created on first use.
fn pointer() -> io::Result<&'static Mutex<File>> {
    static P: OnceLock<Mutex<File>> = OnceLock::new();
    if let Some(p) = P.get() {
        return Ok(p);
    }
    let f = create_pointer()?;
    std::thread::sleep(Duration::from_millis(300)); // let the compositor adopt the device
    Ok(P.get_or_init(|| Mutex::new(f)))
}

/// Events that move the cursor to the tracked position.
fn abs_events(buf: &mut Vec<u8>) {
    let t = track().lock().unwrap();
    let b = t.bounds();
    if b.w <= 0 || b.h <= 0 {
        return;
    }
    let ax = (((t.x - b.x as f64 + 0.5) * 65536.0 / b.w as f64) as i32).clamp(0, ABS_MAX_VAL);
    let ay = (((t.y - b.y as f64 + 0.5) * 65536.0 / b.h as f64) as i32).clamp(0, ABS_MAX_VAL);
    put(buf, EV_ABS, ABS_X, ax);
    put(buf, EV_ABS, ABS_Y, ay);
    put(buf, EV_SYN, 0, 0);
}

fn abs_move_now(x: i32, y: i32) -> io::Result<()> {
    track_set(x, y);
    let mut buf = vec![];
    abs_events(&mut buf);
    pointer()?.lock().unwrap().write_all(&buf)
}

pub struct WlInjector {
    kbd: File,
    buf: Vec<u8>,
    moved: bool,
    wheel: (i32, i32),
}

impl WlInjector {
    pub fn new() -> io::Result<Self> {
        pointer()?;
        let kbd = create_keyboard()?;
        Ok(WlInjector { kbd, buf: Vec::with_capacity(24 * 16), moved: false, wheel: (0, 0) })
    }
    /// Emit pending motion as one absolute position.
    fn sync_motion(&mut self) {
        if self.moved {
            abs_events(&mut self.buf);
            self.moved = false;
        }
    }
}

impl Injector for WlInjector {
    fn name(&self) -> &'static str {
        "uinput (Wayland)"
    }
    fn motion(&mut self, dx: i32, dy: i32) -> io::Result<()> {
        if dx != 0 || dy != 0 {
            track_move(dx as f64, dy as f64);
            self.moved = true;
        }
        Ok(())
    }
    fn button(&mut self, b: Button, down: bool) -> io::Result<()> {
        self.sync_motion();
        let code = match b {
            Button::Left => BTN_LEFT,
            Button::Right => BTN_RIGHT,
            Button::Middle => BTN_MIDDLE,
            Button::Back => BTN_SIDE,
            Button::Forward => BTN_EXTRA,
        };
        put(&mut self.buf, EV_KEY, code, down as i32);
        put(&mut self.buf, EV_SYN, 0, 0);
        Ok(())
    }
    fn wheel(&mut self, v: i32, h: i32) -> io::Result<()> {
        self.sync_motion();
        for (amount, hi, lo, acc) in [(v, REL_WHEEL_HI_RES, REL_WHEEL, 0usize), (h, REL_HWHEEL_HI_RES, REL_HWHEEL, 1)] {
            if amount == 0 {
                continue;
            }
            put(&mut self.buf, EV_REL, hi, amount);
            let a = if acc == 0 { &mut self.wheel.0 } else { &mut self.wheel.1 };
            *a += amount;
            let n = *a / 120;
            if n != 0 {
                put(&mut self.buf, EV_REL, lo, n);
                *a -= n * 120;
            }
            put(&mut self.buf, EV_SYN, 0, 0);
        }
        Ok(())
    }
    fn warp(&mut self, x: i32, y: i32) -> io::Result<()> {
        self.flush()?;
        abs_move_now(x, y)
    }
    fn cursor_pos(&mut self) -> Option<(i32, i32)> {
        Some(pos())
    }
    fn key(&mut self, code: u16, down: bool) -> io::Result<()> {
        if code > 248 {
            return Ok(());
        }
        let mut b = vec![];
        put(&mut b, EV_KEY, code, down as i32);
        put(&mut b, EV_SYN, 0, 0);
        self.kbd.write_all(&b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.sync_motion();
        if self.buf.is_empty() {
            return Ok(());
        }
        let r = pointer()?.lock().unwrap().write_all(&self.buf);
        self.buf.clear();
        r
    }
}

// ------------------------------------------------------------ capture

static FORWARDING: AtomicBool = AtomicBool::new(false);
static KB_FORWARDING: AtomicBool = AtomicBool::new(false);
static PARK_X: AtomicI32 = AtomicI32::new(0);
static PARK_Y: AtomicI32 = AtomicI32::new(0);

const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

fn ioc_read(nr: u32, len: u32) -> libc::c_ulong {
    ((2u32 << 30) | (len << 16) | (0x45 << 8) | nr) as libc::c_ulong
}

fn bits(fd: RawFd, ev: u32, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    unsafe { libc::ioctl(fd, ioc_read(0x20 + ev, len as u32), v.as_mut_ptr()) };
    v
}

fn bit(v: &[u8], n: usize) -> bool {
    v.get(n / 8).is_some_and(|b| b & (1 << (n % 8)) != 0)
}

struct Dev {
    f: File,
    path: String,
    name: String,
    ptr: bool,
    touchpad: bool,
    kbd: bool,
    hires: bool,
    grabbed: bool,
    // per-frame state
    rx: i32,
    ry: i32,
    // touchpad
    tp: (i32, i32),
    tp_last: Option<(i32, i32)>,
    tp_touch: bool,
    tp_per_px: f64,
}

impl Dev {
    fn wants_grab(&self) -> bool {
        ((self.ptr || self.touchpad) && FORWARDING.load(Ordering::SeqCst)) || (self.kbd && KB_FORWARDING.load(Ordering::SeqCst))
    }
    fn apply_grab(&mut self) {
        let want = self.wants_grab();
        if want == self.grabbed {
            return;
        }
        let r = unsafe { libc::ioctl(self.f.as_raw_fd(), EVIOCGRAB, want as libc::c_int) };
        if r < 0 {
            log::warn!("{}: grab {} failed: {}", self.name, want, io::Error::last_os_error());
            return;
        }
        self.grabbed = want;
    }
}

type Devs = Arc<Mutex<Vec<Dev>>>;

struct WlCapture {
    devs: Devs,
}

impl Capture for WlCapture {
    fn set_forwarding(&self, on: bool, park: (i32, i32)) {
        PARK_X.store(park.0, Ordering::Relaxed);
        PARK_Y.store(park.1, Ordering::Relaxed);
        FORWARDING.store(on, Ordering::SeqCst);
        if on {
            let _ = abs_move_now(park.0, park.1);
        }
        for d in self.devs.lock().unwrap().iter_mut() {
            d.apply_grab();
        }
    }
    fn set_keyboard_forwarding(&self, on: bool) {
        KB_FORWARDING.store(on, Ordering::SeqCst);
        for d in self.devs.lock().unwrap().iter_mut() {
            d.apply_grab();
        }
    }
}

impl Drop for WlCapture {
    fn drop(&mut self) {
        FORWARDING.store(false, Ordering::SeqCst);
        KB_FORWARDING.store(false, Ordering::SeqCst);
        for d in self.devs.lock().unwrap().iter_mut() {
            d.apply_grab();
        }
    }
}

fn scan(devs: &Devs) {
    let Ok(rd) = std::fs::read_dir("/dev/input") else { return };
    let mut denied = 0;
    for ent in rd.flatten() {
        let fname = ent.file_name().to_string_lossy().to_string();
        if !fname.starts_with("event") {
            continue;
        }
        let path = ent.path().to_string_lossy().to_string();
        if devs.lock().unwrap().iter().any(|d| d.path == path) {
            continue;
        }
        let f = match OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&path) {
            Ok(f) => f,
            Err(e) => {
                if e.kind() == io::ErrorKind::PermissionDenied {
                    denied += 1;
                }
                continue;
            }
        };
        let fd = f.as_raw_fd();
        let mut nm = [0u8; 128];
        unsafe { libc::ioctl(fd, ioc_read(0x06, 128), nm.as_mut_ptr()) };
        let name = String::from_utf8_lossy(&nm).trim_end_matches('\0').to_string();
        if name.starts_with("Switchboard") {
            continue;
        }
        let key = bits(fd, EV_KEY as u32, 96);
        let rel = bits(fd, EV_REL as u32, 8);
        let abs = bits(fd, EV_ABS as u32, 8);
        let ptr = bit(&rel, REL_X as usize) && bit(&rel, REL_Y as usize) && bit(&key, BTN_LEFT as usize);
        let touchpad = !ptr && bit(&abs, ABS_X as usize) && bit(&abs, ABS_Y as usize) && bit(&key, BTN_TOOL_FINGER as usize);
        let kbd = bit(&key, 1) && bit(&key, 28) && bit(&key, 30) && bit(&key, 57);
        if !(ptr || touchpad || kbd) {
            continue;
        }
        let mut tp_per_px = 1.0;
        if touchpad {
            // EVIOCGABS: {value, min, max, fuzz, flat, resolution} (units/mm); ~7px per mm feels like a laptop pad.
            let mut a = [0i32; 6];
            unsafe { libc::ioctl(fd, ioc_read(0x40 + ABS_X as u32, 24), a.as_mut_ptr()) };
            tp_per_px = (a[5].max(1) as f64) / 7.0;
        }
        log::info!("evdev: {name} ({path}){}{}{}", if ptr { " pointer" } else { "" }, if touchpad { " touchpad" } else { "" }, if kbd { " keyboard" } else { "" });
        let hires = bit(&rel, REL_WHEEL_HI_RES as usize);
        let mut d = Dev { f, path, name, ptr, touchpad, kbd, hires, grabbed: false, rx: 0, ry: 0, tp: (0, 0), tp_last: None, tp_touch: false, tp_per_px };
        d.apply_grab();
        devs.lock().unwrap().push(d);
    }
    if denied > 0 && devs.lock().unwrap().is_empty() {
        log::error!("no access to /dev/input/event* ({denied} devices denied). Install packaging/linux/70-switchboard-input.rules or add your user to the 'input' group");
    }
}

struct Reader {
    h: Arc<dyn CaptureHandler>,
    gain: f64,
    frac: (f64, f64),
    mods: u8, // ctrl=1 alt=2 shift=4 meta=8
    hotkeys: Vec<(u32, u16, u8)>,
}

fn mod_bit(code: u16) -> u8 {
    match code {
        29 | 97 => 1,
        56 | 100 => 2,
        42 | 54 => 4,
        125 | 126 => 8,
        _ => 0,
    }
}

impl Reader {
    fn cursor(&self) -> (i32, i32) {
        if FORWARDING.load(Ordering::Relaxed) {
            (PARK_X.load(Ordering::Relaxed), PARK_Y.load(Ordering::Relaxed))
        } else {
            pos()
        }
    }

    fn motion(&mut self, dx: f64, dy: f64) {
        self.frac.0 += dx * self.gain;
        self.frac.1 += dy * self.gain;
        let (ix, iy) = (self.frac.0.trunc() as i32, self.frac.1.trunc() as i32);
        self.frac.0 -= ix as f64;
        self.frac.1 -= iy as f64;
        if ix == 0 && iy == 0 {
            return;
        }
        let c = if FORWARDING.load(Ordering::Relaxed) { self.cursor() } else { track_move(ix as f64, iy as f64) };
        self.h.on_event(CapEvent::Motion { dx: ix, dy: iy }, c);
    }

    fn flush_rel(&mut self, d: &mut Dev) {
        if d.rx != 0 || d.ry != 0 {
            let (x, y) = (d.rx as f64, d.ry as f64);
            d.rx = 0;
            d.ry = 0;
            self.motion(x, y);
        }
    }

    fn button(&mut self, code: u16, down: bool) {
        let button = match code {
            BTN_LEFT => Button::Left,
            BTN_RIGHT => Button::Right,
            BTN_MIDDLE => Button::Middle,
            BTN_SIDE | BTN_BACK => Button::Back,
            BTN_EXTRA | BTN_FORWARD => Button::Forward,
            _ => return,
        };
        self.h.on_event(CapEvent::Button { button, down }, self.cursor());
    }

    fn event(&mut self, d: &mut Dev, t: u16, code: u16, value: i32) {
        match t {
            EV_REL if d.ptr => match code {
                REL_X => d.rx += value,
                REL_Y => d.ry += value,
                REL_WHEEL_HI_RES => {
                    self.flush_rel(d);
                    self.h.on_event(CapEvent::Wheel { v: value, h: 0 }, self.cursor());
                }
                REL_HWHEEL_HI_RES => {
                    self.flush_rel(d);
                    self.h.on_event(CapEvent::Wheel { v: 0, h: value }, self.cursor());
                }
                REL_WHEEL if !d.hires => {
                    self.flush_rel(d);
                    self.h.on_event(CapEvent::Wheel { v: value * 120, h: 0 }, self.cursor());
                }
                REL_HWHEEL if !d.hires => {
                    self.flush_rel(d);
                    self.h.on_event(CapEvent::Wheel { v: 0, h: value * 120 }, self.cursor());
                }
                _ => {}
            },
            EV_ABS if d.touchpad => match code {
                ABS_X => d.tp.0 = value,
                ABS_Y => d.tp.1 = value,
                _ => {}
            },
            EV_SYN if d.touchpad && code == 0 => {
                if d.tp_touch {
                    if let Some(l) = d.tp_last {
                        let (dx, dy) = ((d.tp.0 - l.0) as f64 / d.tp_per_px, (d.tp.1 - l.1) as f64 / d.tp_per_px);
                        if dx != 0.0 || dy != 0.0 {
                            self.motion(dx, dy);
                        }
                    }
                    d.tp_last = Some(d.tp);
                }
            }
            EV_SYN if d.ptr && code == 0 => self.flush_rel(d),
            EV_KEY => {
                if (d.ptr || d.touchpad) && (BTN_LEFT..=BTN_BACK).contains(&code) {
                    self.flush_rel(d);
                    self.button(code, value != 0);
                } else if d.touchpad && (code == BTN_TOOL_FINGER || code == BTN_TOUCH) {
                    d.tp_touch = value != 0;
                    d.tp_last = None;
                } else if d.kbd && code < 0x100 && value != 2 {
                    let down = value != 0;
                    let m = mod_bit(code);
                    if m != 0 {
                        if down {
                            self.mods |= m
                        } else {
                            self.mods &= !m
                        }
                    }
                    if down {
                        if let Some((id, _, _)) = self.hotkeys.iter().find(|(_, c, mods)| *c == code && *mods == self.mods) {
                            self.h.on_event(CapEvent::Hotkey { id: *id }, self.cursor());
                        }
                    }
                    if KB_FORWARDING.load(Ordering::Relaxed) {
                        self.h.on_event(CapEvent::Key { code, down }, (0, 0));
                    }
                }
            }
            _ => {}
        }
    }
}

pub fn start_capture(handler: Arc<dyn CaptureHandler>, opts: CaptureOptions) -> io::Result<Arc<dyn Capture>> {
    let devs: Devs = Arc::new(Mutex::new(vec![]));
    scan(&devs);
    if devs.lock().unwrap().is_empty() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "no readable input devices in /dev/input"));
    }
    let hotkeys = opts
        .hotkeys
        .iter()
        .filter_map(|h| {
            let code = crate::keymap::name_to_evdev(&h.key)?;
            Some((h.id, code, (h.ctrl as u8) | (h.alt as u8) << 1 | (h.shift as u8) << 2 | (h.meta as u8) << 3))
        })
        .collect();
    let gain = std::env::var("SWITCHBOARD_EVDEV_GAIN").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let mut rd = Reader { h: handler, gain, frac: (0.0, 0.0), mods: 0, hotkeys };
    let cap = Arc::new(WlCapture { devs: devs.clone() });
    std::thread::Builder::new().name("sb-evdev".into()).spawn(move || {
        let mut last_scan = Instant::now();
        let mut buf = vec![0u8; 24 * 128];
        loop {
            let mut pfds: Vec<libc::pollfd> = devs.lock().unwrap().iter().map(|d| libc::pollfd { fd: d.f.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
            let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 1000) };
            if n > 0 {
                let mut list = devs.lock().unwrap();
                let mut dead = vec![];
                // Devices only change in this thread, so indices match the pollfd snapshot.
                for (i, p) in pfds.iter().enumerate() {
                    if p.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                        dead.push(i);
                        continue;
                    }
                    if p.revents & libc::POLLIN == 0 {
                        continue;
                    }
                    let d = &mut list[i];
                    loop {
                        match d.f.read(&mut buf) {
                            Ok(0) => break,
                            Ok(len) => {
                                for e in buf[..len].chunks_exact(24) {
                                    let t = u16::from_ne_bytes([e[16], e[17]]);
                                    let c = u16::from_ne_bytes([e[18], e[19]]);
                                    let v = i32::from_ne_bytes([e[20], e[21], e[22], e[23]]);
                                    rd.event(d, t, c, v);
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(_) => {
                                dead.push(i);
                                break;
                            }
                        }
                    }
                }
                for i in dead.into_iter().rev() {
                    let d = list.remove(i);
                    log::info!("evdev: {} removed", d.name);
                }
            }
            if last_scan.elapsed() > Duration::from_secs(2) {
                last_scan = Instant::now();
                scan(&devs);
            }
        }
    })?;
    Ok(cap)
}
