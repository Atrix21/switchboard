//! Test doubles: a recording injector with a simulated screen, and a no-op capture.

use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use super::{Capture, Injector};
use crate::proto::{Button, Rect, ScreenInfo};

pub fn screens() -> Vec<ScreenInfo> {
    vec![ScreenInfo { name: "SIM-1".into(), rect: Rect { x: 0, y: 0, w: 1920, h: 1080 }, primary: true }]
}

/// Everything injected, for assertions in tests.
#[derive(Default, Debug, Clone)]
pub struct Recorded {
    pub x: i32,
    pub y: i32,
    pub buttons: u16,
    pub log: Vec<String>,
    pub wheel_v: i32,
    pub wheel_h: i32,
    pub keys: crate::keymap::KeySet,
}

pub fn recorded() -> Arc<Mutex<Recorded>> {
    static R: OnceLock<Arc<Mutex<Recorded>>> = OnceLock::new();
    R.get_or_init(|| Arc::new(Mutex::new(Recorded { x: 960, y: 540, ..Default::default() }))).clone()
}

#[derive(Default)]
pub struct RecordingInjector;

impl Injector for RecordingInjector {
    fn name(&self) -> &'static str {
        "null"
    }
    fn motion(&mut self, dx: i32, dy: i32) -> io::Result<()> {
        let r = recorded();
        let mut r = r.lock().unwrap();
        r.x = (r.x + dx).clamp(0, 1919);
        r.y = (r.y + dy).clamp(0, 1079);
        Ok(())
    }
    fn button(&mut self, b: Button, down: bool) -> io::Result<()> {
        let r = recorded();
        let mut r = r.lock().unwrap();
        if down {
            r.buttons |= b.bit();
        } else {
            r.buttons &= !b.bit();
        }
        let (x, y) = (r.x, r.y);
        r.log.push(format!("{b:?} {} @{x},{y}", if down { "down" } else { "up" }));
        Ok(())
    }
    fn wheel(&mut self, v: i32, h: i32) -> io::Result<()> {
        let r = recorded();
        let mut r = r.lock().unwrap();
        r.wheel_v += v;
        r.wheel_h += h;
        Ok(())
    }
    fn warp(&mut self, x: i32, y: i32) -> io::Result<()> {
        let r = recorded();
        let mut r = r.lock().unwrap();
        r.x = x;
        r.y = y;
        Ok(())
    }
    fn cursor_pos(&mut self) -> Option<(i32, i32)> {
        let r = recorded();
        let r = r.lock().unwrap();
        Some((r.x, r.y))
    }
    fn key(&mut self, code: u16, down: bool) -> io::Result<()> {
        let r = recorded();
        let mut r = r.lock().unwrap();
        r.keys.set(code, down);
        r.log.push(format!("key {code} {}", if down { "down" } else { "up" }));
        Ok(())
    }
}

/// In-memory clipboard for tests.
#[derive(Default)]
pub struct MemClipboard {
    pub inner: Mutex<(u64, Option<crate::clipboard::Clip>)>,
}

pub fn mem_clipboard() -> &'static MemClipboard {
    static C: OnceLock<MemClipboard> = OnceLock::new();
    C.get_or_init(MemClipboard::default)
}

impl crate::clipboard::Backend for MemClipboard {
    fn change_count(&self) -> u64 {
        mem_clipboard().inner.lock().unwrap().0
    }
    fn read(&self) -> Option<crate::clipboard::Clip> {
        mem_clipboard().inner.lock().unwrap().1.clone()
    }
    fn write(&self, c: &crate::clipboard::Clip) -> io::Result<()> {
        let mut g = mem_clipboard().inner.lock().unwrap();
        g.0 += 1;
        g.1 = Some(c.clone());
        Ok(())
    }
}

pub struct NoCapture;
impl Capture for NoCapture {
    fn set_forwarding(&self, _on: bool, _park: (i32, i32)) {}
    fn set_keyboard_forwarding(&self, _on: bool) {}
}
