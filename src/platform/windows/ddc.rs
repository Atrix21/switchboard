//! Native DDC/CI on Windows via dxva2 (Monitor Configuration API).
//!
//! Measured on the AW2726DM over DisplayPort: SetVCPFeature(0x60) ≈ 60 ms,
//! and it keeps working while the monitor shows another input (no DP
//! hot-unplug occurs on input change), so Windows can switch the monitor back.

use std::io;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Devices::Display::*;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;

use crate::ddc::DdcMonitor;

fn desc_of(pm: &PHYSICAL_MONITOR) -> String {
    let d = pm.szPhysicalMonitorDescription;
    let n = d.iter().position(|&c| c == 0).unwrap_or(d.len());
    String::from_utf16_lossy(&d[..n])
}

/// Calls `f` with every physical monitor (handles are destroyed afterwards).
fn with_monitors<R>(mut f: impl FnMut(HMONITOR, &PHYSICAL_MONITOR, &str) -> Option<R>) -> Option<R> {
    unsafe extern "system" fn cb(hm: HMONITOR, _: HDC, _: *mut RECT, d: LPARAM) -> BOOL {
        (*(d as *mut Vec<HMONITOR>)).push(hm);
        1
    }
    let mut hms: Vec<HMONITOR> = Vec::new();
    unsafe { EnumDisplayMonitors(null_mut(), null(), Some(cb), &mut hms as *mut _ as LPARAM) };
    for hm in hms {
        let mut n = 0u32;
        if unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(hm, &mut n) } == 0 || n == 0 {
            continue;
        }
        let mut arr: Vec<PHYSICAL_MONITOR> = vec![unsafe { zeroed() }; n as usize];
        if unsafe { GetPhysicalMonitorsFromHMONITOR(hm, n, arr.as_mut_ptr()) } == 0 {
            continue;
        }
        let mut out = None;
        for pm in &arr {
            let d = desc_of(pm);
            if out.is_none() {
                out = f(hm, pm, &d);
            }
        }
        unsafe { DestroyPhysicalMonitors(n, arr.as_ptr()) };
        if out.is_some() {
            return out;
        }
    }
    None
}

pub fn physical_description(hm: HMONITOR) -> Option<String> {
    let mut n = 0u32;
    unsafe {
        if GetNumberOfPhysicalMonitorsFromHMONITOR(hm, &mut n) == 0 || n == 0 {
            return None;
        }
        let mut arr: Vec<PHYSICAL_MONITOR> = vec![zeroed(); n as usize];
        if GetPhysicalMonitorsFromHMONITOR(hm, n, arr.as_mut_ptr()) == 0 {
            return None;
        }
        let d = desc_of(&arr[0]);
        DestroyPhysicalMonitors(n, arr.as_ptr());
        Some(d)
    }
}

fn monitor_rect(hm: HMONITOR) -> (i32, i32, i32, i32) {
    let mut mi: MONITORINFO = unsafe { zeroed() };
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    unsafe { GetMonitorInfoW(hm, &mut mi) };
    let r = mi.rcMonitor;
    (r.left, r.top, r.right - r.left, r.bottom - r.top)
}

pub fn list() -> Vec<DdcMonitor> {
    let mut v = Vec::new();
    let _: Option<()> = with_monitors(|hm, pm, d| {
        let (mut cur, mut max) = (0u32, 0u32);
        let ok = unsafe { GetVCPFeatureAndVCPFeatureReply(pm.hPhysicalMonitor, 0x60, null_mut(), &mut cur, &mut max) } != 0;
        let (x, y, w, h) = monitor_rect(hm);
        // Connector hint: Windows appends e.g. "(DisplayPort)" / "(HDMI)" to the description.
        let connector = if d.contains("DisplayPort") {
            Some("DP".to_string())
        } else if d.contains("HDMI") {
            Some("HDMI".to_string())
        } else {
            None
        };
        v.push(DdcMonitor {
            id: format!("{d}@{x},{y} {w}x{h}"),
            model: d.to_string(),
            connector,
            input: ok.then_some((cur & 0xff) as u16),
            ddc_ok: ok,
        });
        None
    });
    v
}

pub fn get_input(model: &str) -> io::Result<u16> {
    with_monitors(|_, pm, d| {
        if !d.contains(model) {
            return None;
        }
        let (mut cur, mut max) = (0u32, 0u32);
        Some(if unsafe { GetVCPFeatureAndVCPFeatureReply(pm.hPhysicalMonitor, 0x60, null_mut(), &mut cur, &mut max) } != 0 {
            // Some Dell firmware mirrors the previous value into the high byte (e.g. 0x0F11).
            Ok((cur & 0xff) as u16)
        } else {
            Err(io::Error::last_os_error())
        })
    })
    .unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::NotFound, format!("no monitor matching {model:?}"))))
}

pub fn set_input(model: &str, input: u16) -> io::Result<()> {
    with_monitors(|_, pm, d| {
        if !d.contains(model) {
            return None;
        }
        Some(if unsafe { SetVCPFeature(pm.hPhysicalMonitor, 0x60, input as u32) } != 0 { Ok(()) } else { Err(io::Error::last_os_error()) })
    })
    .unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::NotFound, format!("no monitor matching {model:?}"))))
}
