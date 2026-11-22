//! Native DDC/CI over Linux i2c-dev, with ddcutil as a fallback.
//!
//! Only buses whose adapter name identifies them as display buses are ever
//! opened (same heuristic ddcutil uses), so SMBus/SPD/touchpad buses are never
//! touched. Requires rw access to /dev/i2c-N: Ubuntu's ddcutil package grants
//! the logged-in user an ACL via udev (`uaccess`), otherwise add the user to
//! the `i2c` group.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::time::Duration;

use crate::ddc::DdcMonitor;

const I2C_SLAVE: libc::c_ulong = 0x0703;
const DDC_ADDR: libc::c_ulong = 0x37;
const EDID_ADDR: libc::c_ulong = 0x50;

fn is_display_bus(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["nvidia i2c adapter", "i915 gmbus", "aux ", "amdgpu", "radeon", "nouveau", "dmub", "dp-aux", "dpmst"].iter().any(|k| n.contains(k))
}

fn display_buses() -> Vec<(u32, String)> {
    let mut v = vec![];
    if let Ok(rd) = std::fs::read_dir("/sys/bus/i2c/devices") {
        for e in rd.flatten() {
            let f = e.file_name().to_string_lossy().to_string();
            let Some(n) = f.strip_prefix("i2c-").and_then(|n| n.parse::<u32>().ok()) else { continue };
            let name = std::fs::read_to_string(e.path().join("name")).unwrap_or_default().trim().to_string();
            if is_display_bus(&name) {
                v.push((n, name));
            }
        }
    }
    v.sort();
    v
}

fn open(bus: u32, addr: libc::c_ulong) -> io::Result<File> {
    let f = OpenOptions::new().read(true).write(true).open(format!("/dev/i2c-{bus}"))?;
    if unsafe { libc::ioctl(f.as_raw_fd(), I2C_SLAVE, addr) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(f)
}

/// Read the 128-byte base EDID block. Returns (manufacturer, model name).
fn read_edid(bus: u32) -> io::Result<(String, String)> {
    let mut f = open(bus, EDID_ADDR)?;
    f.write_all(&[0])?;
    let mut e = [0u8; 128];
    f.read_exact(&mut e)?;
    if e[..8] != [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0] {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "no EDID header"));
    }
    if e.iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "EDID checksum"));
    }
    Ok(parse_edid(&e))
}

pub fn parse_edid(e: &[u8]) -> (String, String) {
    let m = u16::from_be_bytes([e[8], e[9]]);
    let mfg: String = [(m >> 10) & 31, (m >> 5) & 31, m & 31].iter().map(|c| (b'A' - 1 + *c as u8) as char).collect();
    let mut model = String::new();
    for d in 0..4 {
        let o = 54 + d * 18;
        if e[o] == 0 && e[o + 1] == 0 && e[o + 3] == 0xFC {
            model = e[o + 5..o + 18].iter().take_while(|&&c| c != 0x0a).map(|&c| c as char).collect::<String>().trim().to_string();
        }
    }
    (mfg, model)
}

fn checksum(dest: u8, b: &[u8]) -> u8 {
    b.iter().fold(dest, |a, x| a ^ x)
}

// The DDC/CI spec requires >= 40-50 ms between transactions.
static BUS_LOCK: Mutex<()> = Mutex::new(());

fn get_vcp(bus: u32, code: u8) -> io::Result<(u16, u16)> {
    let _g = BUS_LOCK.lock().unwrap();
    let mut f = open(bus, DDC_ADDR)?;
    let mut req = vec![0x51, 0x82, 0x01, code];
    req.push(checksum(0x6E, &req));
    let mut last_err = io::Error::other("no reply");
    for _attempt in 0..3 {
        f.write_all(&req)?;
        std::thread::sleep(Duration::from_millis(40));
        let mut r = [0u8; 11];
        if let Err(e) = f.read_exact(&mut r) {
            last_err = e;
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        // r: 6E 88 02 result code type maxH maxL curH curL chk
        let chk = checksum(0x50, &r[..10]);
        if r[1] != 0x88 || r[2] != 0x02 || r[4] != code || chk != r[10] {
            last_err = io::Error::new(io::ErrorKind::InvalidData, format!("bad reply {r:02x?}"));
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if r[3] != 0 {
            return Err(io::Error::new(io::ErrorKind::Unsupported, format!("VCP 0x{code:02x} unsupported")));
        }
        return Ok((u16::from_be_bytes([r[8], r[9]]), u16::from_be_bytes([r[6], r[7]])));
    }
    Err(last_err)
}

fn set_vcp(bus: u32, code: u8, value: u16) -> io::Result<()> {
    let _g = BUS_LOCK.lock().unwrap();
    let mut f = open(bus, DDC_ADDR)?;
    let mut req = vec![0x51, 0x84, 0x03, code, (value >> 8) as u8, value as u8];
    req.push(checksum(0x6E, &req));
    f.write_all(&req)?;
    std::thread::sleep(Duration::from_millis(50));
    Ok(())
}

struct Found {
    bus: u32,
}

static CACHE: Mutex<Vec<(u32, String, String)>> = Mutex::new(Vec::new());

/// (bus, mfg, model) of every display bus with a readable EDID.
fn scan() -> Vec<(u32, String, String)> {
    let mut c = CACHE.lock().unwrap();
    if c.is_empty() {
        for (bus, _) in display_buses() {
            if let Ok((mfg, model)) = read_edid(bus) {
                c.push((bus, mfg, model));
            }
        }
    }
    c.clone()
}

fn find(model: &str) -> io::Result<Found> {
    if let Some(b) = std::env::var("SWITCHBOARD_I2C_BUS").ok().and_then(|b| b.parse().ok()) {
        return Ok(Found { bus: b });
    }
    scan()
        .into_iter()
        .find(|(_, mfg, m)| model.is_empty() || m.contains(model) || model.contains(m.as_str()) && !m.is_empty() || format!("{mfg} {m}").contains(model))
        .map(|(bus, _, _)| Found { bus })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no DDC monitor matching {model:?} (no access to /dev/i2c-*?)")))
}

fn connector_for(model: &str) -> Option<String> {
    // Match the RandR output whose EDID model matches; its name (HDMI-1-0, DP-2...) gives the connector type.
    super::screens().into_iter().find(|s| s.name.contains(model)).and_then(|s| s.name.split('[').nth(1).map(|c| c.trim_end_matches(']').to_string()))
}

pub fn list() -> Vec<DdcMonitor> {
    scan()
        .into_iter()
        .map(|(bus, mfg, model)| {
            let input = get_vcp(bus, 0x60).ok().map(|(c, _)| c & 0xff);
            DdcMonitor { id: format!("/dev/i2c-{bus} {mfg} {model}"), connector: connector_for(&model), model, input, ddc_ok: input.is_some() }
        })
        .collect()
}

pub fn get_input(model: &str) -> io::Result<u16> {
    let f = find(model)?;
    match get_vcp(f.bus, 0x60) {
        Ok((c, _)) => Ok(c & 0xff),
        Err(e) => ddcutil_get(f.bus).map_err(|_| e),
    }
}

pub fn set_input(model: &str, input: u16) -> io::Result<()> {
    let f = find(model)?;
    match set_vcp(f.bus, 0x60, input) {
        Ok(()) => Ok(()),
        Err(e) => {
            log::warn!("native DDC on i2c-{} failed ({e}); trying ddcutil", f.bus);
            ddcutil_set(f.bus, input)
        }
    }
}

fn ddcutil_get(bus: u32) -> io::Result<u16> {
    let o = std::process::Command::new("ddcutil").args(["--bus", &bus.to_string(), "getvcp", "0x60", "--terse"]).output()?;
    let s = String::from_utf8_lossy(&o.stdout);
    // "VCP 60 SNC x0f"
    s.split_whitespace().last().and_then(|v| u16::from_str_radix(v.trim_start_matches('x'), 16).ok()).ok_or_else(|| io::Error::other(format!("ddcutil: {s}")))
}

fn ddcutil_set(bus: u32, input: u16) -> io::Result<()> {
    let o = std::process::Command::new("ddcutil").args(["--bus", &bus.to_string(), "--noverify", "setvcp", "0x60", &format!("0x{input:02x}")]).output()?;
    if o.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(String::from_utf8_lossy(&o.stderr).trim().to_string()))
    }
}
