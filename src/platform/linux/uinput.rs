//! uinput virtual pointer: a real kernel input device, so libinput applies
//! the user's own acceleration profile, high-resolution scrolling works, and
//! the same code path will serve Wayland compositors.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;

use super::Injector;
use crate::proto::Button;

const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
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

#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

pub struct UinputInjector {
    f: File,
    buf: Vec<u8>,
    x: super::XTestInjector, // for warp / cursor position
    wheel: (i32, i32),
    dirty: bool,
}

impl UinputInjector {
    pub fn new() -> io::Result<Self> {
        let f = OpenOptions::new().write(true).open("/dev/uinput")?;
        let fd = f.as_raw_fd();
        let io = |r: libc::c_int| if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) };
        unsafe {
            io(libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int))?;
            io(libc::ioctl(fd, UI_SET_EVBIT, EV_REL as libc::c_int))?;
            for b in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                io(libc::ioctl(fd, UI_SET_KEYBIT, b as libc::c_int))?;
            }
            for r in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
                io(libc::ioctl(fd, UI_SET_RELBIT, r as libc::c_int))?;
            }
            let mut s = UinputSetup { bustype: 0x06 /* BUS_VIRTUAL */, vendor: 0x5357, product: 0x4244, version: 1, name: [0; 80], ff_effects_max: 0 };
            let n = b"Switchboard Virtual Pointer";
            s.name[..n.len()].copy_from_slice(n);
            io(libc::ioctl(fd, UI_DEV_SETUP, &s))?;
            io(libc::ioctl(fd, UI_DEV_CREATE))?;
        }
        let x = super::XTestInjector::new()?;
        // Forwarded motion already carries the source machine's pointer
        // ballistics, so this device must not be accelerated again.
        std::thread::spawn(|| {
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                match set_flat_profile() {
                    Ok(true) => {
                        log::info!("virtual pointer: flat acceleration profile");
                        return;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        log::warn!("could not set flat acceleration on virtual pointer: {e}");
                        return;
                    }
                }
            }
        });
        Ok(UinputInjector { f, buf: Vec::with_capacity(24 * 16), x, wheel: (0, 0), dirty: false })
    }

    fn ev(&mut self, t: u16, code: u16, value: i32) {
        let mut e = [0u8; 24]; // struct input_event on 64-bit: timeval(16) + type + code + value
        e[16..18].copy_from_slice(&t.to_ne_bytes());
        e[18..20].copy_from_slice(&code.to_ne_bytes());
        e[20..24].copy_from_slice(&value.to_ne_bytes());
        self.buf.extend_from_slice(&e);
        self.dirty = t != EV_SYN;
    }
}

impl Drop for UinputInjector {
    fn drop(&mut self) {
        unsafe { libc::ioctl(self.f.as_raw_fd(), UI_DEV_DESTROY) };
    }
}

impl Injector for UinputInjector {
    fn name(&self) -> &'static str {
        "uinput"
    }
    fn motion(&mut self, dx: i32, dy: i32) -> io::Result<()> {
        if dx != 0 {
            self.ev(EV_REL, REL_X, dx);
        }
        if dy != 0 {
            self.ev(EV_REL, REL_Y, dy);
        }
        Ok(())
    }
    fn button(&mut self, b: Button, down: bool) -> io::Result<()> {
        let code = match b {
            Button::Left => BTN_LEFT,
            Button::Right => BTN_RIGHT,
            Button::Middle => BTN_MIDDLE,
            Button::Back => BTN_SIDE,
            Button::Forward => BTN_EXTRA,
        };
        // A button change must be its own frame so it lands after preceding motion.
        if self.dirty {
            self.ev(EV_SYN, 0, 0);
        }
        self.ev(EV_KEY, code, down as i32);
        self.ev(EV_SYN, 0, 0);
        Ok(())
    }
    fn wheel(&mut self, v: i32, h: i32) -> io::Result<()> {
        if v != 0 {
            self.ev(EV_REL, REL_WHEEL_HI_RES, v);
            self.wheel.0 += v;
            let n = self.wheel.0 / 120;
            if n != 0 {
                self.ev(EV_REL, REL_WHEEL, n);
                self.wheel.0 -= n * 120;
            }
        }
        if h != 0 {
            self.ev(EV_REL, REL_HWHEEL_HI_RES, h);
            self.wheel.1 += h;
            let n = self.wheel.1 / 120;
            if n != 0 {
                self.ev(EV_REL, REL_HWHEEL, n);
                self.wheel.1 -= n * 120;
            }
        }
        Ok(())
    }
    fn warp(&mut self, x: i32, y: i32) -> io::Result<()> {
        self.flush()?;
        self.x.warp(x, y)
    }
    fn cursor_pos(&mut self) -> Option<(i32, i32)> {
        self.x.cursor_pos()
    }
    fn set_cursor_visible(&mut self, visible: bool) {
        self.x.set_cursor_visible(visible);
    }
    fn key(&mut self, code: u16, down: bool) -> io::Result<()> {
        self.flush()?;
        self.x.key(code, down)?;
        self.x.flush()
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        if self.dirty {
            self.ev(EV_SYN, 0, 0);
        }
        let r = self.f.write_all(&self.buf);
        self.buf.clear();
        r
    }
}

/// Sets "libinput Accel Profile Enabled" = flat on our device. Ok(false) if the
/// device has not appeared in X yet.
fn set_flat_profile() -> io::Result<bool> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xinput::{self, ConnectionExt as _};
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
    let e = |x: &dyn std::fmt::Display| io::Error::other(x.to_string());
    let (c, _) = x11rb::connect(None).map_err(|x| e(&x))?;
    let devs = c.xinput_xi_query_device(xinput::Device::ALL).map_err(|x| e(&x))?.reply().map_err(|x| e(&x))?;
    let Some(dev) = devs.infos.iter().find(|d| d.name == b"Switchboard Virtual Pointer" && d.type_ == xinput::DeviceType::SLAVE_POINTER) else {
        return Ok(false);
    };
    let prop = c.intern_atom(true, b"libinput Accel Profile Enabled").map_err(|x| e(&x))?.reply().map_err(|x| e(&x))?.atom;
    if prop == 0 {
        return Err(e(&"libinput driver not in use"));
    }
    let cur = c.xinput_xi_get_property(dev.deviceid, false, prop, u32::from(AtomEnum::ANY), 0, 16).map_err(|x| e(&x))?.reply().map_err(|x| e(&x))?;
    let n = cur.num_items as usize;
    if n < 2 {
        return Ok(false);
    }
    let mut v = vec![0u8; n];
    v[1] = 1; // [adaptive, flat, (custom)]
    c.xinput_xi_change_property(dev.deviceid, x11rb::protocol::xproto::PropMode::REPLACE, prop, cur.type_, n as u32, &xinput::XIChangePropertyAux::Data8(v)).map_err(|x| e(&x))?;
    c.flush().map_err(|x| e(&x))?;
    Ok(true)
}
