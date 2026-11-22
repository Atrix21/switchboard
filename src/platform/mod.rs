//! Platform backends. Each OS module provides:
//! * `screens()` – display geometry in the OS's global pointer coordinate space.
//! * `new_injector()` – synthesizes pointer input.
//! * `start_capture()` – reads physical pointer input and, while forwarding,
//!   suppresses it locally.
//! * `ddc` – monitor input (VCP 0x60) control.
//! * `clipboard` – read/write/watch the system clipboard.

use std::io;
use std::sync::Arc;

use crate::proto::{Button, ScreenInfo};

#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use self::windows as os;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub use self::linux as os;

pub mod null;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CapEvent {
    /// Raw (unaccelerated where the OS allows) relative motion.
    Motion { dx: i32, dy: i32 },
    Button { button: Button, down: bool },
    /// 1/120-notch units. Positive v = away from user, positive h = right.
    Wheel { v: i32, h: i32 },
    Hotkey { id: u32 },
    /// Physical key (evdev code) – only reported while keyboard forwarding is on.
    Key { code: u16, down: bool },
}

/// Receives physical input on the capture thread. Must be fast: it runs on
/// the input path.
pub trait CaptureHandler: Send + Sync + 'static {
    /// `cursor` is the local cursor position when the event was read.
    fn on_event(&self, ev: CapEvent, cursor: (i32, i32));
}

pub trait Injector: Send {
    fn name(&self) -> &'static str;
    fn motion(&mut self, dx: i32, dy: i32) -> io::Result<()>;
    fn button(&mut self, b: Button, down: bool) -> io::Result<()>;
    /// 1/120-notch units.
    fn wheel(&mut self, v: i32, h: i32) -> io::Result<()>;
    /// Move the cursor to absolute global coordinates.
    fn warp(&mut self, x: i32, y: i32) -> io::Result<()>;
    fn cursor_pos(&mut self) -> Option<(i32, i32)>;
    /// Press/release a physical key (evdev code).
    fn key(&mut self, code: u16, down: bool) -> io::Result<()>;
    /// Flush any batched input to the OS.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Global hotkey description parsed from e.g. "ctrl+alt+shift+escape".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub id: u32,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    /// Lower-case key name: "a".."z", "0".."9", "f1".."f24", "escape", "left", ...
    pub key: String,
}

impl Hotkey {
    pub fn parse(id: u32, s: &str) -> Option<Hotkey> {
        let mut h = Hotkey { id, ctrl: false, alt: false, shift: false, meta: false, key: String::new() };
        for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
            match part.as_str() {
                "ctrl" | "control" => h.ctrl = true,
                "alt" => h.alt = true,
                "shift" => h.shift = true,
                "meta" | "super" | "win" | "cmd" => h.meta = true,
                "" => return None,
                k => {
                    if !h.key.is_empty() {
                        return None;
                    }
                    h.key = match k {
                        "esc" => "escape".into(),
                        other => other.into(),
                    }
                }
            }
        }
        (!h.key.is_empty()).then_some(h)
    }
}

pub struct CaptureOptions {
    pub hotkeys: Vec<Hotkey>,
}

/// Handle to the running capture backend.
pub trait Capture: Send + Sync {
    /// Enter/leave forwarding mode. While forwarding, physical pointer input
    /// must not affect the local desktop. `park` = where to hold the cursor.
    fn set_forwarding(&self, on: bool, park: (i32, i32));
    /// Divert the physical keyboard to the engine (as `CapEvent::Key`) instead
    /// of the local desktop.
    fn set_keyboard_forwarding(&self, on: bool);
}

/// Shared helper: injector selection by env var for tests/benchmarks.
pub fn new_injector() -> io::Result<Box<dyn Injector>> {
    match std::env::var("SWITCHBOARD_INJECT").as_deref() {
        Ok("null") => Ok(Box::new(null::RecordingInjector::default())),
        _ => os::new_injector(),
    }
}

pub fn screens() -> Vec<ScreenInfo> {
    if std::env::var("SWITCHBOARD_INJECT").as_deref() == Ok("null") {
        return null::screens();
    }
    os::screens()
}

pub fn clipboard() -> io::Result<Arc<dyn crate::clipboard::Backend>> {
    if std::env::var("SWITCHBOARD_INJECT").as_deref() == Ok("null") {
        return Ok(Arc::new(null::MemClipboard::default()));
    }
    os::clipboard::new()
}

pub fn start_capture(handler: Arc<dyn CaptureHandler>, opts: CaptureOptions) -> io::Result<Arc<dyn Capture>> {
    if std::env::var("SWITCHBOARD_CAPTURE").as_deref() == Ok("none") {
        return Ok(Arc::new(null::NoCapture));
    }
    os::start_capture(handler, opts)
}
