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
}

pub struct NoCapture;
impl Capture for NoCapture {
    fn set_forwarding(&self, _on: bool, _park: (i32, i32)) {}
}
