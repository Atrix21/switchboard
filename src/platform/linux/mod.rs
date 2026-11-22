//! Linux/X11 backend.
//!
//! * Screens: RandR 1.5 monitors, named "<EDID model> [<output>]".
//! * Injection: XTest (works for any X11 user, no setup) or uinput (kernel
//!   virtual device; needs write access to /dev/uinput; future Wayland path,
//!   high-resolution wheel). Selected with SWITCHBOARD_INJECTOR=xtest|uinput,
//!   default: uinput when accessible, else XTest.
//! * Capture: XInput2 raw events on the root window (device-level deltas,
//!   unaccelerated), plus an active pointer grab with an invisible cursor while
//!   forwarding so local applications receive nothing.
//! * Hotkeys: XGrabKey on the root window.

pub mod clipboard;
pub mod ddc;
mod uinput;

use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask, GrabMode, ModMask};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::NONE;

use super::{CapEvent, Capture, CaptureHandler, CaptureOptions, Hotkey, Injector};
use crate::proto::{Button, Rect, ScreenInfo};

fn xerr<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("X11: {e}"))
}

fn connect() -> io::Result<(RustConnection, xproto::Window)> {
    let (c, n) = x11rb::connect(None).map_err(xerr)?;
    let root = c.setup().roots[n].root;
    Ok((c, root))
}

// ---------------------------------------------------------------- screens

pub fn screens() -> Vec<ScreenInfo> {
    screens_inner().unwrap_or_else(|e| {
        log::warn!("RandR: {e}");
        vec![]
    })
}

fn atom_name(c: &RustConnection, a: u32) -> String {
    c.get_atom_name(a).ok().and_then(|r| r.reply().ok()).map(|r| String::from_utf8_lossy(&r.name).to_string()).unwrap_or_default()
}

fn screens_inner() -> io::Result<Vec<ScreenInfo>> {
    let (c, root) = connect()?;
    let mons = c.randr_get_monitors(root, true).map_err(xerr)?.reply().map_err(xerr)?;
    let edid_atom = c.intern_atom(true, b"EDID").map_err(xerr)?.reply().map_err(xerr)?.atom;
    let mut v = vec![];
    for m in mons.monitors {
        let out_name = atom_name(&c, m.name);
        let mut model = String::new();
        if edid_atom != NONE {
            for o in &m.outputs {
                if let Ok(Ok(p)) = c.randr_get_output_property(*o, edid_atom, xproto::AtomEnum::ANY, 0, 64, false, false).map(|r| r.reply()) {
                    if p.data.len() >= 128 {
                        model = ddc::parse_edid(&p.data).1;
                    }
                }
            }
        }
        v.push(ScreenInfo {
            name: if model.is_empty() { out_name.clone() } else { format!("{model} [{out_name}]") },
            rect: Rect { x: m.x as i32, y: m.y as i32, w: m.width as i32, h: m.height as i32 },
            primary: m.primary,
        });
    }
    Ok(v)
}

// ---------------------------------------------------------------- injection

/// Accumulates hi-res wheel units into whole notches for button-based wheels.
#[derive(Default)]
struct WheelAcc {
    v: i32,
    h: i32,
}

impl WheelAcc {
    fn take(&mut self, v: i32, h: i32) -> (i32, i32) {
        self.v += v;
        self.h += h;
        let (nv, nh) = (self.v / 120, self.h / 120);
        self.v -= nv * 120;
        self.h -= nh * 120;
        (nv, nh)
    }
}

pub struct XTestInjector {
    c: RustConnection,
    root: xproto::Window,
    wheel: WheelAcc,
    hidden: bool,
}

fn x_button(b: Button) -> u8 {
    match b {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 3,
        Button::Back => 8,
        Button::Forward => 9,
    }
}

impl XTestInjector {
    pub fn new() -> io::Result<Self> {
        let (c, root) = connect()?;
        c.extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME).map_err(xerr)?.ok_or_else(|| xerr("XTEST extension missing"))?;
        Ok(XTestInjector { c, root, wheel: WheelAcc::default(), hidden: false })
    }
    fn click(&self, btn: u8) -> io::Result<()> {
        self.c.xtest_fake_input(xproto::BUTTON_PRESS_EVENT, btn, 0, NONE, 0, 0, 0).map_err(xerr)?;
        self.c.xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, btn, 0, NONE, 0, 0, 0).map_err(xerr)?;
        Ok(())
    }
}

impl Injector for XTestInjector {
    fn name(&self) -> &'static str {
        "XTest"
    }
    fn motion(&mut self, dx: i32, dy: i32) -> io::Result<()> {
        if dx != 0 || dy != 0 {
            // detail=1: relative motion.
            self.c.xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, 1, 0, NONE, dx.clamp(-32768, 32767) as i16, dy.clamp(-32768, 32767) as i16, 0).map_err(xerr)?;
        }
        Ok(())
    }
    fn button(&mut self, b: Button, down: bool) -> io::Result<()> {
        let t = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.c.xtest_fake_input(t, x_button(b), 0, NONE, 0, 0, 0).map_err(xerr)?;
        Ok(())
    }
    fn wheel(&mut self, v: i32, h: i32) -> io::Result<()> {
        let (nv, nh) = self.wheel.take(v, h);
        for _ in 0..nv.unsigned_abs() {
            self.click(if nv > 0 { 4 } else { 5 })?;
        }
        for _ in 0..nh.unsigned_abs() {
            self.click(if nh > 0 { 7 } else { 6 })?;
        }
        Ok(())
    }
    fn warp(&mut self, x: i32, y: i32) -> io::Result<()> {
        self.c.warp_pointer(NONE, self.root, 0, 0, 0, 0, x as i16, y as i16).map_err(xerr)?;
        self.c.flush().map_err(xerr)
    }
    fn cursor_pos(&mut self) -> Option<(i32, i32)> {
        let r = self.c.query_pointer(self.root).ok()?.reply().ok()?;
        Some((r.root_x as i32, r.root_y as i32))
    }
    fn set_cursor_visible(&mut self, visible: bool) {
        use x11rb::protocol::xfixes::ConnectionExt as _;
        if visible == !self.hidden {
            return;
        }
        // XFixes hide/show is per client and undone automatically if we exit.
        if self.c.xfixes_query_version(5, 0).ok().and_then(|c| c.reply().ok()).is_none() {
            return;
        }
        let r = if visible { self.c.xfixes_show_cursor(self.root) } else { self.c.xfixes_hide_cursor(self.root) };
        if r.is_ok() {
            self.hidden = !visible;
            let _ = self.c.flush();
        }
    }
    fn key(&mut self, code: u16, down: bool) -> io::Result<()> {
        if code > 247 {
            return Ok(());
        }
        let t = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        self.c.xtest_fake_input(t, (code + 8) as u8, 0, NONE, 0, 0, 0).map_err(xerr)?;
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.c.flush().map_err(xerr)
    }
}

pub fn new_injector() -> io::Result<Box<dyn Injector>> {
    let pref = std::env::var("SWITCHBOARD_INJECTOR").unwrap_or_default();
    if pref != "xtest" {
        match uinput::UinputInjector::new() {
            Ok(u) => return Ok(Box::new(u)),
            Err(e) if pref == "uinput" => return Err(e),
            Err(e) => log::info!("uinput unavailable ({e}); using XTest"),
        }
    }
    Ok(Box::new(XTestInjector::new()?))
}

// ---------------------------------------------------------------- capture

static FORWARDING: AtomicBool = AtomicBool::new(false);
static KB_FORWARDING: AtomicBool = AtomicBool::new(false);
static PARK_X: AtomicI32 = AtomicI32::new(0);
static PARK_Y: AtomicI32 = AtomicI32::new(0);

struct XCapture {
    c: Arc<RustConnection>,
    root: xproto::Window,
    cursor: xproto::Cursor,
    lock: Mutex<()>,
}

impl Capture for XCapture {
    fn set_keyboard_forwarding(&self, on: bool) {
        let was = KB_FORWARDING.swap(on, Ordering::SeqCst);
        if was == on {
            return;
        }
        let _g = self.lock.lock().unwrap();
        let c = &*self.c;
        if on {
            // Retry briefly: a menu or another app may hold a transient grab.
            for _ in 0..20 {
                match c.grab_keyboard(false, self.root, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC).map(|r| r.reply()) {
                    Ok(Ok(r)) if r.status == xproto::GrabStatus::SUCCESS => return,
                    _ => std::thread::sleep(std::time::Duration::from_millis(25)),
                }
            }
            log::warn!("keyboard grab failed; keys typed here will also reach local apps");
        } else {
            let _ = c.ungrab_keyboard(x11rb::CURRENT_TIME);
            let _ = c.flush();
        }
    }

    fn set_forwarding(&self, on: bool, park: (i32, i32)) {
        PARK_X.store(park.0, Ordering::Relaxed);
        PARK_Y.store(park.1, Ordering::Relaxed);
        let was = FORWARDING.swap(on, Ordering::SeqCst);
        let _g = self.lock.lock().unwrap();
        let c = &*self.c;
        if on {
            let _ = c.warp_pointer(NONE, self.root, 0, 0, 0, 0, park.0 as i16, park.1 as i16);
            if !was {
                // Empty event mask: we read input via XI2 raw events on the
                // capture connection; this grab only diverts it from apps.
                match c.grab_pointer(false, self.root, EventMask::NO_EVENT, GrabMode::ASYNC, GrabMode::ASYNC, NONE, self.cursor, x11rb::CURRENT_TIME).map(|r| r.reply()) {
                    Ok(Ok(r)) if r.status == xproto::GrabStatus::SUCCESS => {}
                    other => log::warn!("pointer grab failed: {other:?} (another app holds a grab?)"),
                }
            }
        } else if was {
            let _ = c.ungrab_pointer(x11rb::CURRENT_TIME);
        }
        let _ = c.flush();
    }
}

impl Drop for XCapture {
    fn drop(&mut self) {
        let _ = self.c.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = self.c.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.c.flush();
    }
}

fn invisible_cursor(c: &RustConnection, root: xproto::Window) -> io::Result<xproto::Cursor> {
    let pm = c.generate_id().map_err(xerr)?;
    c.create_pixmap(1, pm, root, 1, 1).map_err(xerr)?;
    let cur = c.generate_id().map_err(xerr)?;
    c.create_cursor(cur, pm, pm, 0, 0, 0, 0, 0, 0, 0, 0).map_err(xerr)?;
    c.free_pixmap(pm).map_err(xerr)?;
    Ok(cur)
}

fn keysym_for(key: &str) -> Option<u32> {
    let k = key.as_bytes();
    if k.len() == 1 && (k[0].is_ascii_lowercase() || k[0].is_ascii_digit()) {
        return Some(k[0] as u32);
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0xffbe + n - 1);
        }
    }
    Some(match key {
        "escape" => 0xff1b,
        "space" => 0x20,
        "tab" => 0xff09,
        "enter" | "return" => 0xff0d,
        "backspace" => 0xff08,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" => 0xff55,
        "pagedown" => 0xff56,
        "insert" => 0xff63,
        "delete" => 0xffff,
        "pause" => 0xff13,
        "scrolllock" => 0xff14,
        _ => return None,
    })
}

fn keycode_for(c: &RustConnection, keysym: u32) -> Option<u8> {
    let s = c.setup();
    let (min, max) = (s.min_keycode, s.max_keycode);
    let m = c.get_keyboard_mapping(min, max - min + 1).ok()?.reply().ok()?;
    let per = m.keysyms_per_keycode as usize;
    m.keysyms.chunks(per).position(|ks| ks.contains(&keysym)).map(|i| min + i as u8)
}

fn hk_mods(h: &Hotkey) -> u16 {
    let mut m = 0u16;
    if h.shift {
        m |= u16::from(ModMask::SHIFT);
    }
    if h.ctrl {
        m |= u16::from(ModMask::CONTROL);
    }
    if h.alt {
        m |= u16::from(ModMask::M1);
    }
    if h.meta {
        m |= u16::from(ModMask::M4);
    }
    m
}

const IGNORED_MODS: [u16; 4] = [0, 2 /* Lock */, 16 /* Mod2=NumLock */, 18];

pub fn start_capture(handler: Arc<dyn CaptureHandler>, opts: CaptureOptions) -> io::Result<Arc<dyn Capture>> {
    let (c, root) = connect()?;
    let xi = c.xinput_xi_query_version(2, 2).map_err(xerr)?.reply().map_err(xerr)?;
    if xi.major_version < 2 {
        return Err(xerr("XInput2 not available"));
    }
    // Devices whose events are synthetic (ours): XTEST slaves and our uinput device.
    let mut ignore: Vec<u16> = vec![];
    for d in c.xinput_xi_query_device(xinput::Device::ALL).map_err(xerr)?.reply().map_err(xerr)?.infos {
        let name = String::from_utf8_lossy(&d.name).to_string();
        if name.contains("XTEST") || name.contains("Switchboard") {
            ignore.push(d.deviceid);
        }
    }
    let mask = xinput::XIEventMask::RAW_MOTION
        | xinput::XIEventMask::RAW_BUTTON_PRESS
        | xinput::XIEventMask::RAW_BUTTON_RELEASE
        | xinput::XIEventMask::RAW_KEY_PRESS
        | xinput::XIEventMask::RAW_KEY_RELEASE
        | xinput::XIEventMask::HIERARCHY;
    c.xinput_xi_select_events(root, &[xinput::EventMask { deviceid: xinput::Device::ALL_MASTER.into(), mask: vec![mask] }]).map_err(xerr)?;
    let mut keys: Vec<(u8, u16, u32)> = vec![];
    for h in &opts.hotkeys {
        let (Some(ks), mods) = (keysym_for(&h.key), hk_mods(h)) else { continue };
        let Some(kc) = keycode_for(&c, ks) else { continue };
        for extra in IGNORED_MODS {
            if let Ok(ck) = c.grab_key(true, root, (mods | extra).into(), kc, GrabMode::ASYNC, GrabMode::ASYNC) {
                if ck.check().is_err() {
                    log::warn!("hotkey {:?} is already taken by another application", h);
                    break;
                }
            }
        }
        keys.push((kc, mods, h.id));
    }
    c.change_window_attributes(root, &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT)).map_err(xerr)?;
    c.flush().map_err(xerr)?;

    let cursor = invisible_cursor(&c, root)?;
    c.flush().map_err(xerr)?;
    let c = Arc::new(c);
    let cap = Arc::new(XCapture { c: c.clone(), root, cursor, lock: Mutex::new(()) });

    let (tx, rx) = mpsc::channel::<()>();
    std::thread::Builder::new().name("sb-xi2".into()).spawn(move || {
        let _ = tx.send(());
        let fp = |v: &xinput::Fp3232| v.integral as f64 + v.frac as f64 / 4294967296.0;
        let mut frac = (0.0f64, 0.0f64);
        loop {
            let ev = match c.wait_for_event() {
                Ok(e) => e,
                Err(e) => {
                    log::error!("X connection lost: {e}");
                    FORWARDING.store(false, Ordering::SeqCst);
                    return;
                }
            };
            let pos = || c.query_pointer(root).ok().and_then(|r| r.reply().ok()).map(|r| (r.root_x as i32, r.root_y as i32)).unwrap_or((0, 0));
            match ev {
                Event::XinputRawMotion(e) => {
                    if ignore.contains(&e.sourceid) {
                        continue;
                    }
                    // Walk the valuator mask: values are packed in mask order.
                    let mut dx = 0.0;
                    let mut dy = 0.0;
                    let mut idx = 0;
                    for (w, word) in e.valuator_mask.iter().enumerate() {
                        for b in 0..32 {
                            if word & (1 << b) != 0 {
                                let axis = w * 32 + b;
                                if let Some(v) = e.axisvalues_raw.get(idx) {
                                    match axis {
                                        0 => dx = fp(v),
                                        1 => dy = fp(v),
                                        _ => {}
                                    }
                                }
                                idx += 1;
                            }
                        }
                    }
                    frac.0 += dx;
                    frac.1 += dy;
                    let (ix, iy) = (frac.0.trunc() as i32, frac.1.trunc() as i32);
                    frac.0 -= ix as f64;
                    frac.1 -= iy as f64;
                    if ix != 0 || iy != 0 {
                        let p = pos();
                        handler.on_event(CapEvent::Motion { dx: ix, dy: iy }, p);
                        if FORWARDING.load(Ordering::Relaxed) {
                            let _ = c.warp_pointer(NONE, root, 0, 0, 0, 0, PARK_X.load(Ordering::Relaxed) as i16, PARK_Y.load(Ordering::Relaxed) as i16);
                            let _ = c.flush();
                        }
                    }
                }
                Event::XinputRawButtonPress(e) | Event::XinputRawButtonRelease(e) => {
                    if ignore.contains(&e.sourceid) {
                        continue;
                    }
                    let down = ev_kind(&e) == Some(true);
                    let p = pos();
                    let ce = match e.detail {
                        1 => Some(CapEvent::Button { button: Button::Left, down }),
                        2 => Some(CapEvent::Button { button: Button::Middle, down }),
                        3 => Some(CapEvent::Button { button: Button::Right, down }),
                        8 => Some(CapEvent::Button { button: Button::Back, down }),
                        9 => Some(CapEvent::Button { button: Button::Forward, down }),
                        4 if down => Some(CapEvent::Wheel { v: 120, h: 0 }),
                        5 if down => Some(CapEvent::Wheel { v: -120, h: 0 }),
                        6 if down => Some(CapEvent::Wheel { v: 0, h: -120 }),
                        7 if down => Some(CapEvent::Wheel { v: 0, h: 120 }),
                        _ => None,
                    };
                    if let Some(ce) = ce {
                        handler.on_event(ce, p);
                    }
                }
                Event::XinputHierarchy(_) => {
                    // Device hot-plug: refresh ignore list.
                    if let Ok(Ok(r)) = c.xinput_xi_query_device(xinput::Device::ALL).map(|r| r.reply()) {
                        ignore = r.infos.iter().filter(|d| { let n = String::from_utf8_lossy(&d.name); n.contains("XTEST") || n.contains("Switchboard") }).map(|d| d.deviceid).collect();
                    }
                }
                Event::XinputRawKeyPress(e) | Event::XinputRawKeyRelease(e) => {
                    if !KB_FORWARDING.load(Ordering::Relaxed) || ignore.contains(&e.sourceid) || e.detail < 8 {
                        continue;
                    }
                    let down = e.event_type == 13; // XI_RawKeyPress (14 = release)
                    handler.on_event(CapEvent::Key { code: (e.detail - 8) as u16, down }, (0, 0));
                }
                Event::KeyPress(_) | Event::KeyRelease(_) if KB_FORWARDING.load(Ordering::Relaxed) => {
                    // Delivered to us by the active keyboard grab; raw events above carry the data.
                }
                Event::KeyPress(k) => {
                    let st = u16::from(k.state) & !(2 | 16);
                    if let Some((_, _, id)) = keys.iter().find(|(kc, m, _)| *kc == k.detail && *m == st) {
                        handler.on_event(CapEvent::Hotkey { id: *id }, pos());
                    }
                }
                _ => {}
            }
        }
    })?;
    let _ = rx.recv();
    Ok(cap)
}

/// x11rb decodes RawButtonPress and RawButtonRelease into the same struct;
/// recover which one it was from the GE event type.
fn ev_kind(e: &xinput::RawButtonPressEvent) -> Option<bool> {
    match e.event_type {
        15 => Some(true),  // XI_RawButtonPress
        16 => Some(false), // XI_RawButtonRelease
        _ => None,
    }
}

// ---------------------------------------------------------------- misc

static TERM: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
static TERM_FLAG: AtomicBool = AtomicBool::new(false);

pub fn on_terminate(f: Box<dyn Fn() + Send + Sync>) {
    let _ = TERM.set(f);
    extern "C" fn h(_: libc::c_int) {
        TERM_FLAG.store(true, Ordering::SeqCst); // async-signal-safe
    }
    unsafe {
        libc::signal(libc::SIGINT, h as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, h as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, h as *const () as libc::sighandler_t);
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if TERM_FLAG.load(Ordering::SeqCst) {
            if let Some(f) = TERM.get() {
                f();
            }
            return;
        }
    });
}

pub fn take_failsafe() -> bool {
    false
}

pub fn swallowed_count() -> u64 {
    0
}
