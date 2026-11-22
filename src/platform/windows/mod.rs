//! Windows backend: Raw Input + on-demand WH_MOUSE_LL capture, SendInput
//! injection, dxva2 DDC/CI.

pub mod clipboard;
pub mod ddc;

use std::ffi::c_void;
use std::io;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{mpsc, Arc};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Input::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::{CapEvent, Capture, CaptureHandler, CaptureOptions, Hotkey, Injector};
use crate::proto::{Button, Rect, ScreenInfo};

pub fn init_process() {
    // Per-monitor DPI awareness so all coordinates are physical pixels.
    #[link(name = "user32")]
    extern "system" {
        fn SetProcessDpiAwarenessContext(v: isize) -> i32;
    }
    unsafe {
        SetProcessDpiAwarenessContext(-4); // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
fn from_wide(w: &[u16]) -> String {
    let n = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..n])
}

// ---------------------------------------------------------------- screens

pub fn screens() -> Vec<ScreenInfo> {
    unsafe extern "system" fn cb(hm: HMONITOR, _: HDC, _: *mut RECT, d: LPARAM) -> BOOL {
        let v = &mut *(d as *mut Vec<ScreenInfo>);
        let mut mi: MONITORINFOEXW = zeroed();
        mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hm, &mut mi as *mut _ as *mut MONITORINFO) != 0 {
            let r = mi.monitorInfo.rcMonitor;
            let desc = ddc::physical_description(hm).unwrap_or_default();
            let dev = from_wide(&mi.szDevice);
            v.push(ScreenInfo {
                name: if desc.is_empty() { dev } else { format!("{desc} [{dev}]") },
                rect: Rect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top },
                primary: mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        1
    }
    let mut v: Vec<ScreenInfo> = Vec::new();
    unsafe {
        EnumDisplayMonitors(null_mut(), null(), Some(cb), &mut v as *mut _ as LPARAM);
    }
    v
}

// ---------------------------------------------------------------- injection

pub struct SendInputInjector {
    batch: Vec<INPUT>,
}

pub fn new_injector() -> io::Result<Box<dyn Injector>> {
    Ok(Box::new(SendInputInjector { batch: Vec::with_capacity(16) }))
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: SB_EXTRA_INFO } },
    }
}

/// Marker placed in dwExtraInfo of everything we inject.
pub const SB_EXTRA_INFO: usize = 0x5357_4244; // "SWBD"

impl Injector for SendInputInjector {
    fn name(&self) -> &'static str {
        "SendInput"
    }
    fn motion(&mut self, dx: i32, dy: i32) -> io::Result<()> {
        if dx != 0 || dy != 0 {
            self.batch.push(mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE));
        }
        Ok(())
    }
    fn button(&mut self, b: Button, down: bool) -> io::Result<()> {
        let (flag, data) = match (b, down) {
            (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Button::Back, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 1), // XBUTTON1
            (Button::Forward, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 2), // XBUTTON2
        };
        self.batch.push(mouse_input(0, 0, data, flag));
        Ok(())
    }
    fn wheel(&mut self, v: i32, h: i32) -> io::Result<()> {
        if v != 0 {
            self.batch.push(mouse_input(0, 0, v, MOUSEEVENTF_WHEEL));
        }
        if h != 0 {
            self.batch.push(mouse_input(0, 0, h, MOUSEEVENTF_HWHEEL));
        }
        Ok(())
    }
    fn warp(&mut self, x: i32, y: i32) -> io::Result<()> {
        self.flush()?;
        if unsafe { SetCursorPos(x, y) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn cursor_pos(&mut self) -> Option<(i32, i32)> {
        let mut p = POINT { x: 0, y: 0 };
        (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
    }
    fn key(&mut self, code: u16, down: bool) -> io::Result<()> {
        let up = if down { 0 } else { KEYEVENTF_KEYUP };
        let ki = if code == crate::keymap::KEY_PAUSE {
            KEYBDINPUT { wVk: VK_PAUSE, wScan: 0, dwFlags: up, time: 0, dwExtraInfo: SB_EXTRA_INFO }
        } else {
            let Some((scan, ext)) = crate::keymap::evdev_to_win(code) else { return Ok(()) };
            KEYBDINPUT { wVk: 0, wScan: scan, dwFlags: KEYEVENTF_SCANCODE | up | if ext { KEYEVENTF_EXTENDEDKEY } else { 0 }, time: 0, dwExtraInfo: SB_EXTRA_INFO }
        };
        self.batch.push(INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } });
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let n = unsafe { SendInput(self.batch.len() as u32, self.batch.as_ptr(), size_of::<INPUT>() as i32) };
        let want = self.batch.len();
        self.batch.clear();
        if n as usize != want {
            // Typically UIPI: the foreground window belongs to an elevated process.
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "SendInput blocked (elevated foreground window or secure desktop)"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- capture

static FORWARDING: AtomicBool = AtomicBool::new(false);
static PARK_X: AtomicI32 = AtomicI32::new(0);
static PARK_Y: AtomicI32 = AtomicI32::new(0);
/// Events swallowed by the hook (diagnostics).
pub static HOOK_SWALLOWED: AtomicU32 = AtomicU32::new(0);
/// Raw input events seen while forwarding (diagnostics only: Windows 11
/// throttles background raw input, so the hook is the data source).
pub static RAW_WHILE_FORWARDING: AtomicU32 = AtomicU32::new(0);

/// Hook callbacks never call into the engine directly. While a low-level hook
/// is installed, Windows waits for the callback before delivering ANY input to
/// the system, so a callback that blocks (e.g. on an engine lock) freezes the
/// whole desktop's mouse and keyboard. The callbacks only decide swallow/pass
/// from atomics and enqueue the event; a worker thread runs the handler.
///
/// Invariant: an event is swallowed only if it was handed to the queue.
static HOOK_QUEUE: std::sync::OnceLock<mpsc::Sender<(CapEvent, (i32, i32))>> = std::sync::OnceLock::new();
/// Slowest hook callback observed, µs (diagnostics).
pub static HOOK_MAX_US: AtomicU32 = AtomicU32::new(0);

/// Hand an event to the worker. Returns false if there is no consumer, in
/// which case the caller must let the event through to Windows.
#[inline]
fn enqueue(ev: CapEvent, pos: (i32, i32)) -> bool {
    HOOK_QUEUE.get().is_some_and(|q| q.send((ev, pos)).is_ok())
}

struct HookTimer(std::time::Instant);
impl Drop for HookTimer {
    fn drop(&mut self) {
        HOOK_MAX_US.fetch_max(self.0.elapsed().as_micros() as u32, Ordering::Relaxed);
    }
}

const WM_APP_HOOK_ON: u32 = WM_APP + 1;
const WM_APP_HOOK_OFF: u32 = WM_APP + 2;
const WM_APP_KB_ON: u32 = WM_APP + 3;
const WM_APP_KB_OFF: u32 = WM_APP + 4;

static KB_FORWARDING: AtomicBool = AtomicBool::new(false);
/// Self-test only: treat our own injected events as physical input.
static SELFTEST: AtomicBool = AtomicBool::new(false);
#[inline]
fn is_physical(flags_injected: bool, extra: usize) -> bool {
    !flags_injected || (SELFTEST.load(Ordering::Relaxed) && extra == SELFTEST_TAG)
}
const SELFTEST_TAG: usize = 0x5342_5354; // "SBST"
/// VKs that were already down when keyboard forwarding started: their
/// release must reach the local desktop or the key would stay stuck here.
static KB_HELD_AT_GRAB: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];

unsafe extern "system" fn ll_kbd_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let _t = HookTimer(std::time::Instant::now());
    if code == HC_ACTION as i32 && KB_FORWARDING.load(Ordering::Relaxed) {
        let k = &*(lp as *const KBDLLHOOKSTRUCT);
        if is_physical(k.flags & LLKHF_INJECTED != 0, k.dwExtraInfo) {
            let up = k.flags & LLKHF_UP != 0;
            let vk = (k.vkCode & 0xff) as usize;
            if KB_HELD_AT_GRAB[vk].load(Ordering::Relaxed) {
                if up {
                    KB_HELD_AT_GRAB[vk].store(false, Ordering::Relaxed);
                }
                return CallNextHookEx(null_mut(), code, wp, lp);
            }
            let ext = k.flags & LLKHF_EXTENDED != 0;
            let mut scan = k.scanCode as u16;
            if scan == 0 {
                scan = MapVirtualKeyW(k.vkCode, MAPVK_VK_TO_VSC) as u16;
            }
            let handed = match crate::keymap::win_to_evdev(scan, ext, k.vkCode == VK_PAUSE as u32) {
                Some(evdev) => enqueue(CapEvent::Key { code: evdev, down: !up }, (0, 0)),
                None => HOOK_QUEUE.get().is_some(), // unmapped key: drop it, but only if we are really forwarding
            };
            if handed {
                return 1;
            }
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

/// While forwarding, the low-level hook is both the blocker and the data
/// source. Unlike WM_INPUT for background windows (throttled to ~125 Hz on
/// Windows 11), LL hooks run for every input event, so the mouse's full
/// polling rate is preserved. The cursor is parked mid-screen, so for each
/// event `pt - park` is exactly that event's motion after Windows pointer
/// ballistics (the user's pointer speed / "enhance pointer precision").
unsafe extern "system" fn ll_mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let _t = HookTimer(std::time::Instant::now());
    if code == HC_ACTION as i32 && FORWARDING.load(Ordering::Relaxed) {
        let info = &*(lp as *const MSLLHOOKSTRUCT);
        if is_physical(info.flags & LLMHF_INJECTED != 0, info.dwExtraInfo) {
            HOOK_SWALLOWED.fetch_add(1, Ordering::Relaxed);
            if HOOK_QUEUE.get().is_some() {
                let park = (PARK_X.load(Ordering::Relaxed), PARK_Y.load(Ordering::Relaxed));
                let hi = (info.mouseData >> 16) as u16;
                let x = |down| if hi == 1 { Some(CapEvent::Button { button: Button::Back, down }) } else if hi == 2 { Some(CapEvent::Button { button: Button::Forward, down }) } else { None };
                let ev = match wp as u32 {
                    WM_MOUSEMOVE => {
                        let (dx, dy) = (info.pt.x - park.0, info.pt.y - park.1);
                        (dx != 0 || dy != 0).then_some(CapEvent::Motion { dx, dy })
                    }
                    WM_LBUTTONDOWN => Some(CapEvent::Button { button: Button::Left, down: true }),
                    WM_LBUTTONUP => Some(CapEvent::Button { button: Button::Left, down: false }),
                    WM_RBUTTONDOWN => Some(CapEvent::Button { button: Button::Right, down: true }),
                    WM_RBUTTONUP => Some(CapEvent::Button { button: Button::Right, down: false }),
                    WM_MBUTTONDOWN => Some(CapEvent::Button { button: Button::Middle, down: true }),
                    WM_MBUTTONUP => Some(CapEvent::Button { button: Button::Middle, down: false }),
                    WM_XBUTTONDOWN => x(true),
                    WM_XBUTTONUP => x(false),
                    WM_MOUSEWHEEL => Some(CapEvent::Wheel { v: hi as i16 as i32, h: 0 }),
                    WM_MOUSEHWHEEL => Some(CapEvent::Wheel { v: 0, h: hi as i16 as i32 }),
                    _ => None,
                };
                if let Some(ev) = ev {
                    if !enqueue(ev, park) {
                        return CallNextHookEx(null_mut(), code, wp, lp);
                    }
                }
                return 1;
            }
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

/// Centre of the monitor containing `p` – far from any edge so hook
/// coordinates are never clamped.
fn park_point(p: (i32, i32)) -> (i32, i32) {
    unsafe {
        let hm = MonitorFromPoint(POINT { x: p.0, y: p.1 }, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hm, &mut mi) == 0 {
            return p;
        }
        let r = mi.rcMonitor;
        ((r.left + r.right) / 2, (r.top + r.bottom) / 2)
    }
}

struct WinCapture {
    hook_thread: u32,
}

unsafe impl Send for WinCapture {}
unsafe impl Sync for WinCapture {}

impl Capture for WinCapture {
    fn set_keyboard_forwarding(&self, on: bool) {
        if on {
            for vk in 0..256usize {
                let down = unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
                KB_HELD_AT_GRAB[vk].store(down && !(1..=6).contains(&vk), Ordering::Relaxed);
            }
        }
        let was = KB_FORWARDING.swap(on, Ordering::SeqCst);
        if was != on {
            unsafe { PostThreadMessageW(self.hook_thread, if on { WM_APP_KB_ON } else { WM_APP_KB_OFF }, 0, 0) };
        }
    }
    fn set_forwarding(&self, on: bool, park: (i32, i32)) {
        let park = if on { park_point(park) } else { park };
        PARK_X.store(park.0, Ordering::Relaxed);
        PARK_Y.store(park.1, Ordering::Relaxed);
        let was = FORWARDING.swap(on, Ordering::SeqCst);
        if on {
            unsafe { SetCursorPos(park.0, park.1) };
        }
        if was != on {
            unsafe { PostThreadMessageW(self.hook_thread, if on { WM_APP_HOOK_ON } else { WM_APP_HOOK_OFF }, 0, 0) };
        }
    }
}

impl Drop for WinCapture {
    fn drop(&mut self) {
        FORWARDING.store(false, Ordering::SeqCst);
        KB_FORWARDING.store(false, Ordering::SeqCst);
        unsafe {
            PostThreadMessageW(self.hook_thread, WM_APP_HOOK_OFF, 0, 0);
            PostThreadMessageW(self.hook_thread, WM_APP_KB_OFF, 0, 0);
        }
    }
}

/// A tiny, nearly transparent topmost window that sits under the parked
/// cursor while forwarding and gives it an empty shape, so no frozen cursor
/// is left on this screen. Nothing global changes: if the process dies the
/// window goes with it and the cursor is back.
unsafe extern "system" fn cloak_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SETCURSOR => {
            SetCursor(null_mut());
            1
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn create_cloak() -> HWND {
    let hinst = GetModuleHandleW(null());
    let cls = wide("SwitchboardCloak");
    let wc = WNDCLASSW { lpfnWndProc: Some(cloak_proc), hInstance: hinst, lpszClassName: cls.as_ptr(), hCursor: null_mut(), ..zeroed() };
    RegisterClassW(&wc);
    let h = CreateWindowExW(WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE, cls.as_ptr(), cls.as_ptr(), WS_POPUP, 0, 0, 48, 48, null_mut(), null_mut(), hinst, null());
    if !h.is_null() {
        SetLayeredWindowAttributes(h, 0, 1, LWA_ALPHA);
    }
    h
}

unsafe fn show_cloak(h: HWND, on: bool) {
    if h.is_null() {
        return;
    }
    if on {
        let (x, y) = (PARK_X.load(Ordering::Relaxed), PARK_Y.load(Ordering::Relaxed));
        SetWindowPos(h, HWND_TOPMOST, x - 24, y - 24, 48, 48, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        // Nudge the cursor so Windows asks the window under it for a shape now.
        SetCursorPos(x + 1, y);
        SetCursorPos(x, y);
    } else {
        ShowWindow(h, SW_HIDE);
    }
}

/// Is the cursor currently hidden (diagnostics / selftest)?
pub fn cursor_hidden() -> bool {
    unsafe {
        let mut ci: CURSORINFO = zeroed();
        ci.cbSize = size_of::<CURSORINFO>() as u32;
        GetCursorInfo(&mut ci) != 0 && (ci.hCursor.is_null() || ci.flags & CURSOR_SHOWING == 0)
    }
}

fn hook_thread(ready: mpsc::Sender<u32>) {
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        let cloak = create_cloak();
        let mut msg: MSG = zeroed();
        // Force creation of the thread message queue before announcing the id.
        PeekMessageW(&mut msg, null_mut(), WM_USER, WM_USER, PM_NOREMOVE);
        let _ = ready.send(GetCurrentThreadId());
        let mut hook: HHOOK = null_mut();
        let mut kb_hook: HHOOK = null_mut();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            match msg.message {
                WM_APP_KB_ON if kb_hook.is_null() => {
                    kb_hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_kbd_proc), GetModuleHandleW(null()), 0);
                    if kb_hook.is_null() {
                        log::error!("keyboard hook failed: {}", io::Error::last_os_error());
                    }
                }
                WM_APP_KB_OFF if !kb_hook.is_null() => {
                    UnhookWindowsHookEx(kb_hook);
                    kb_hook = null_mut();
                }
                WM_APP_HOOK_ON if hook.is_null() => {
                    hook = SetWindowsHookExW(WH_MOUSE_LL, Some(ll_mouse_proc), GetModuleHandleW(null()), 0);
                    if hook.is_null() {
                        log::error!("SetWindowsHookExW failed: {}", io::Error::last_os_error());
                    } else {
                        show_cloak(cloak, true);
                    }
                }
                WM_APP_HOOK_OFF if !hook.is_null() => {
                    UnhookWindowsHookEx(hook);
                    hook = null_mut();
                    show_cloak(cloak, false);
                }
                _ => {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        if !hook.is_null() {
            UnhookWindowsHookEx(hook);
        }
        if !kb_hook.is_null() {
            UnhookWindowsHookEx(kb_hook);
        }
    }
}

// Raw input button flag bits.
const RI_L_DOWN: u16 = 0x0001;
const RI_L_UP: u16 = 0x0002;
const RI_R_DOWN: u16 = 0x0004;
const RI_R_UP: u16 = 0x0008;
const RI_M_DOWN: u16 = 0x0010;
const RI_M_UP: u16 = 0x0020;
const RI_4_DOWN: u16 = 0x0040;
const RI_4_UP: u16 = 0x0080;
const RI_5_DOWN: u16 = 0x0100;
const RI_5_UP: u16 = 0x0200;
const RI_WHEEL: u16 = 0x0400;
const RI_HWHEEL: u16 = 0x0800;
const MOUSE_MOVE_ABSOLUTE: u16 = 0x01;
const MOUSE_VIRTUAL_DESKTOP: u16 = 0x02;

struct RawCtx {
    handler: Arc<dyn CaptureHandler>,
    last_abs: Option<(i32, i32)>,
    buf: Vec<u8>,
}

thread_local! {
    static RAW_CTX: std::cell::RefCell<Option<RawCtx>> = const { std::cell::RefCell::new(None) };
}

unsafe extern "system" fn raw_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_INPUT => {
            RAW_CTX.with(|c| {
                if let Some(ctx) = c.borrow_mut().as_mut() {
                    handle_raw(ctx, lp as HRAWINPUT);
                }
            });
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_HOTKEY => {
            RAW_CTX.with(|c| {
                if let Some(ctx) = c.borrow().as_ref() {
                    ctx.handler.on_event(CapEvent::Hotkey { id: wp as u32 }, cursor());
                }
            });
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn cursor() -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

unsafe fn handle_raw(ctx: &mut RawCtx, h: HRAWINPUT) {
    let mut size: u32 = 0;
    let hdr = size_of::<RAWINPUTHEADER>() as u32;
    GetRawInputData(h, RID_INPUT, null_mut(), &mut size, hdr);
    if size == 0 {
        return;
    }
    ctx.buf.resize(size as usize, 0);
    if GetRawInputData(h, RID_INPUT, ctx.buf.as_mut_ptr() as *mut c_void, &mut size, hdr) != size {
        return;
    }
    let ri = &*(ctx.buf.as_ptr() as *const RAWINPUT);
    if ri.header.dwType != RIM_TYPEMOUSE {
        return;
    }
    // Injected input (ours or anyone's SendInput) has no device handle.
    if ri.header.hDevice.is_null() {
        return;
    }
    let m = ri.data.mouse;
    if FORWARDING.load(Ordering::Relaxed) {
        RAW_WHILE_FORWARDING.fetch_add(1, Ordering::Relaxed);
        return; // the LL hook delivers forwarded input
    }
    let pos = cursor();
    let (mut dx, mut dy) = (m.lLastX, m.lLastY);
    if m.usFlags & MOUSE_MOVE_ABSOLUTE != 0 {
        // Tablets / remote sessions: convert normalized absolute to deltas.
        let (w, hgt) = if m.usFlags & MOUSE_VIRTUAL_DESKTOP != 0 {
            (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN))
        } else {
            (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
        };
        let ax = (m.lLastX as i64 * w as i64 / 65535) as i32;
        let ay = (m.lLastY as i64 * hgt as i64 / 65535) as i32;
        let (px, py) = ctx.last_abs.unwrap_or((ax, ay));
        ctx.last_abs = Some((ax, ay));
        dx = ax - px;
        dy = ay - py;
    }
    let flags = m.Anonymous.Anonymous.usButtonFlags;
    let data = m.Anonymous.Anonymous.usButtonData as i16 as i32;
    // Motion first, then buttons: within one report, the position is where the click happened.
    if dx != 0 || dy != 0 {
        ctx.handler.on_event(CapEvent::Motion { dx, dy }, pos);
    }
    if flags != 0 {
        for (bit, b, down) in [
            (RI_L_DOWN, Button::Left, true),
            (RI_L_UP, Button::Left, false),
            (RI_R_DOWN, Button::Right, true),
            (RI_R_UP, Button::Right, false),
            (RI_M_DOWN, Button::Middle, true),
            (RI_M_UP, Button::Middle, false),
            (RI_4_DOWN, Button::Back, true),
            (RI_4_UP, Button::Back, false),
            (RI_5_DOWN, Button::Forward, true),
            (RI_5_UP, Button::Forward, false),
        ] {
            if flags & bit != 0 {
                ctx.handler.on_event(CapEvent::Button { button: b, down }, pos);
            }
        }
        if flags & RI_WHEEL != 0 {
            ctx.handler.on_event(CapEvent::Wheel { v: data, h: 0 }, pos);
        }
        if flags & RI_HWHEEL != 0 {
            ctx.handler.on_event(CapEvent::Wheel { v: 0, h: data }, pos);
        }
    }
}

fn vk_for(key: &str) -> Option<u32> {
    let k = key.as_bytes();
    if k.len() == 1 && (k[0].is_ascii_lowercase() || k[0].is_ascii_digit()) {
        return Some(k[0].to_ascii_uppercase() as u32);
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(VK_F1 as u32 + n - 1);
        }
    }
    Some(match key {
        "escape" => VK_ESCAPE,
        "space" => VK_SPACE,
        "tab" => VK_TAB,
        "enter" | "return" => VK_RETURN,
        "backspace" => VK_BACK,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" => VK_PRIOR,
        "pagedown" => VK_NEXT,
        "insert" => VK_INSERT,
        "delete" => VK_DELETE,
        "pause" => VK_PAUSE,
        "scrolllock" => VK_SCROLL,
        _ => return None,
    } as u32)
}

fn raw_thread(handler: Arc<dyn CaptureHandler>, hotkeys: Vec<Hotkey>, ready: mpsc::Sender<io::Result<()>>) {
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        let hinst = GetModuleHandleW(null());
        let cls = wide("SwitchboardRawInput");
        let wc = WNDCLASSW { lpfnWndProc: Some(raw_wndproc), hInstance: hinst, lpszClassName: cls.as_ptr(), ..zeroed() };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(0, cls.as_ptr(), cls.as_ptr(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), hinst, null());
        if hwnd.is_null() {
            let _ = ready.send(Err(io::Error::last_os_error()));
            return;
        }
        let rid = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
        if RegisterRawInputDevices(&rid, 1, size_of::<RAWINPUTDEVICE>() as u32) == 0 {
            let _ = ready.send(Err(io::Error::last_os_error()));
            return;
        }
        for hk in &hotkeys {
            let Some(vk) = vk_for(&hk.key) else {
                log::warn!("unsupported hotkey key {:?}", hk.key);
                continue;
            };
            let mut mods = MOD_NOREPEAT;
            if hk.ctrl {
                mods |= MOD_CONTROL;
            }
            if hk.alt {
                mods |= MOD_ALT;
            }
            if hk.shift {
                mods |= MOD_SHIFT;
            }
            if hk.meta {
                mods |= MOD_WIN;
            }
            if RegisterHotKey(hwnd, hk.id as i32, mods, vk) == 0 {
                log::warn!("RegisterHotKey {:?} failed: {}", hk, io::Error::last_os_error());
            }
        }
        RAW_CTX.with(|c| *c.borrow_mut() = Some(RawCtx { handler, last_abs: None, buf: Vec::with_capacity(64) }));
        let _ = ready.send(Ok(()));
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub fn start_capture(handler: Arc<dyn CaptureHandler>, opts: CaptureOptions) -> io::Result<Arc<dyn Capture>> {
    let (qtx, qrx) = mpsc::channel::<(CapEvent, (i32, i32))>();
    {
        let handler = handler.clone();
        std::thread::Builder::new().name("sb-hookwork".into()).spawn(move || {
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
            while let Ok((ev, pos)) = qrx.recv() {
                handler.on_event(ev, pos);
            }
        })?;
    }
    let _ = HOOK_QUEUE.set(qtx);
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("sb-hook".into()).spawn(move || hook_thread(tx))?;
    let hook_thread = rx.recv().map_err(io::Error::other)?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("sb-rawinput".into()).spawn(move || raw_thread(handler, opts.hotkeys, tx))?;
    rx.recv().map_err(io::Error::other)??;
    Ok(Arc::new(WinCapture { hook_thread }))
}

/// Is the cursor confined (e.g. by a fullscreen game)? Then edge switching is suppressed.
pub fn cursor_clipped() -> bool {
    unsafe {
        let mut r: RECT = zeroed();
        if GetClipCursor(&mut r) == 0 {
            return false;
        }
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        !(r.left <= vx && r.top <= vy && r.right >= vx + vw && r.bottom >= vy + vh)
    }
}

// ---------------------------------------------------------------- process stats

pub fn process_stats() -> (f64, u64) {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    unsafe {
        let h = GetCurrentProcess();
        let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) = (zeroed(), zeroed(), zeroed(), zeroed());
        GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
        let mut pmc: PROCESS_MEMORY_COUNTERS = zeroed();
        pmc.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        GetProcessMemoryInfo(h, &mut pmc, pmc.cb);
        (ft(k) + ft(u), pmc.WorkingSetSize as u64)
    }
}

pub fn take_failsafe() -> bool {
    false
}

/// Events blocked locally by the hooks (for the engine's dead-forwarding check).
pub fn swallowed_count() -> u64 {
    HOOK_SWALLOWED.load(Ordering::Relaxed) as u64
}

/// End-to-end check of the real capture path (hooks + worker queue + handler)
/// using tagged synthetic input. Always restores local input before returning.
pub fn selftest() -> Result<String, String> {
    use std::sync::Mutex;
    struct Rec(Mutex<Vec<CapEvent>>);
    impl CaptureHandler for Rec {
        fn on_event(&self, ev: CapEvent, _: (i32, i32)) {
            self.0.lock().unwrap().push(ev);
        }
    }
    let rec = Arc::new(Rec(Mutex::new(vec![])));
    let cap = start_capture(rec.clone(), CaptureOptions { hotkeys: vec![] }).map_err(|e| e.to_string())?;
    let send = |inputs: &[INPUT]| unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
    let mi = |dx, dy, data: i32, flags| INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: SELFTEST_TAG } } };
    let ki = |scan: u16, up: bool| INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: 0, wScan: scan, dwFlags: KEYEVENTF_SCANCODE | if up { KEYEVENTF_KEYUP } else { 0 }, time: 0, dwExtraInfo: SELFTEST_TAG } } };
    let wait = |n: usize| {
        for _ in 0..100 {
            if rec.0.lock().unwrap().len() >= n {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    let start = cursor();
    SELFTEST.store(true, Ordering::SeqCst);
    let mut report = vec![];
    let res: Result<(), String> = (|| {
        cap.set_forwarding(true, start);
        cap.set_keyboard_forwarding(true);
        std::thread::sleep(std::time::Duration::from_millis(100)); // hooks installed by the hook thread
        let park = cursor();
        // Movement + a middle click (harmless) + F24 key (scancode 0x76, unused by apps).
        send(&[mi(7, 3, 0, MOUSEEVENTF_MOVE), mi(0, 0, 0, MOUSEEVENTF_MIDDLEDOWN), mi(0, 0, 0, MOUSEEVENTF_MIDDLEUP), mi(0, 0, 120, MOUSEEVENTF_WHEEL)]);
        send(&[ki(0x76, false), ki(0x76, true)]);
        wait(6);
        let got = rec.0.lock().unwrap().clone();
        report.push(format!("events received while forwarding: {got:?}"));
        if !got.iter().any(|e| matches!(e, CapEvent::Motion { .. })) {
            return Err("motion not delivered to handler".into());
        }
        if !got.contains(&CapEvent::Button { button: Button::Middle, down: true }) || !got.contains(&CapEvent::Button { button: Button::Middle, down: false }) {
            return Err("buttons not delivered".into());
        }
        if !got.contains(&CapEvent::Wheel { v: 120, h: 0 }) {
            return Err("wheel not delivered".into());
        }
        if !got.contains(&CapEvent::Key { code: 194, down: true }) || !got.contains(&CapEvent::Key { code: 194, down: false }) {
            return Err("keys not delivered".into());
        }
        if cursor() != park {
            return Err(format!("cursor moved locally while forwarding ({park:?} -> {:?})", cursor()));
        }
        report.push("local cursor stayed parked (input blocked locally)".into());
        report.push(format!("cursor hidden while forwarding: {}", cursor_hidden()));
        Ok(())
    })();
    cap.set_keyboard_forwarding(false);
    cap.set_forwarding(false, start);
    std::thread::sleep(std::time::Duration::from_millis(100));
    // After forwarding: input must reach Windows again.
    let before = cursor();
    let n0 = rec.0.lock().unwrap().len();
    send(&[mi(5, 0, 0, MOUSEEVENTF_MOVE)]);
    std::thread::sleep(std::time::Duration::from_millis(50));
    let after = cursor();
    let passthrough = after != before && rec.0.lock().unwrap()[n0..].iter().all(|e| !matches!(e, CapEvent::Key { .. }));
    let _ = unsafe { SetCursorPos(start.0, start.1) };
    SELFTEST.store(false, Ordering::SeqCst);
    res?;
    if !passthrough {
        return Err(format!("input did not reach Windows after forwarding stopped ({before:?} -> {after:?})"));
    }
    report.push("input flows to Windows again after forwarding stops".into());
    report.push(format!("slowest hook callback: {} µs", HOOK_MAX_US.load(Ordering::Relaxed)));
    Ok(report.join("
"))
}
