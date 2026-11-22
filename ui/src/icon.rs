//! Tray/window icons drawn in code: two patch jacks joined by a cable.
//! Colour carries state; the shape never changes, so it stays recognisable.

use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    /// Service not reachable.
    Offline,
    /// Running, no paired computer connected.
    Idle,
    /// Connected; pointer here.
    Connected,
    /// Pointer is on (or from) another computer.
    Away,
    Paused,
}

pub fn state_of(s: Option<&Value>) -> State {
    let Some(s) = s else { return State::Offline };
    if s["disabled"] == true {
        return State::Paused;
    }
    if s["mode"].as_str().is_some_and(|m| m != "local") {
        return State::Away;
    }
    if s["peers"].as_array().is_some_and(|a| a.iter().any(|p| p["trusted"] == true)) {
        State::Connected
    } else {
        State::Idle
    }
}

pub fn tooltip(s: Option<&Value>) -> String {
    let Some(s) = s else { return "Switchboard isn't running".into() };
    if s["disabled"] == true {
        return "Switchboard is paused".into();
    }
    let mode = s["mode"].as_str().unwrap_or("local");
    let peer = s["peers"].as_array().and_then(|a| a.iter().find(|p| p["trusted"] == true));
    match (mode, peer) {
        ("local", Some(p)) => format!("Connected to {} · {:.0} ms", p["name"].as_str().unwrap_or("?"), p["rtt_ms"]["p50"].as_f64().unwrap_or(0.0).max(1.0)),
        ("local", None) => "Waiting for your other computer".into(),
        (m, _) => {
            let mut c = m.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }
    }
}

fn color(s: State) -> [u8; 3] {
    match s {
        State::Offline => [0xE0, 0x64, 0x5A],
        State::Idle | State::Paused => [0x8A, 0x94, 0xA0],
        State::Connected => [0x3D, 0xB2, 0xA4],
        State::Away => [0xC2, 0x9A, 0x4A],
    }
}

/// Signed-distance drawing at 4x, box-filtered down for anti-aliasing.
fn render(size: u32, s: State) -> Vec<u8> {
    const SS: u32 = 4;
    let n = size * SS;
    let c = color(s);
    let f = n as f32;
    let mut cov = vec![0f32; (size * size) as usize];
    let jack_r = f * 0.17;
    let ring = f * 0.065;
    let (ax, ay) = (f * 0.27, f * 0.62);
    let (bx, by) = (f * 0.73, f * 0.38);
    for y in 0..n {
        for x in 0..n {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let d_ring = |cx: f32, cy: f32| (((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - jack_r).abs() - ring / 2.0;
            // Cable: quadratic-ish arc approximated by distance to the segment, bowed.
            let (dx, dy) = (bx - ax, by - ay);
            let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
            let bow = (t * (1.0 - t)) * f * 0.28;
            let (qx, qy) = (ax + dx * t, ay + dy * t + bow);
            let d_cable = ((px - qx).powi(2) + (py - qy).powi(2)).sqrt() - ring / 2.0;
            let in_jack = |cx: f32, cy: f32| ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() < jack_r - ring;
            let mut d = d_ring(ax, ay).min(d_ring(bx, by));
            if !in_jack(ax, ay) && !in_jack(bx, by) {
                d = d.min(d_cable);
            }
            // Jack centres (pin dots).
            let pin = |cx: f32, cy: f32| ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - jack_r * 0.32;
            d = d.min(pin(ax, ay)).min(pin(bx, by));
            if s == State::Paused {
                // Diagonal slash.
                let ds = ((px - py) / std::f32::consts::SQRT_2).abs() - ring / 2.0;
                d = d.min(ds);
            }
            if d < 0.0 {
                cov[((y / SS) * size + x / SS) as usize] += 1.0 / (SS * SS) as f32;
            }
        }
    }
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for a in cov {
        out.extend_from_slice(&[c[0], c[1], c[2], (a.min(1.0) * 255.0) as u8]);
    }
    out
}

pub fn tray(s: State) -> tray_icon::Icon {
    tray_icon::Icon::from_rgba(render(32, s), 32, 32).expect("icon")
}

pub fn window_icon() -> tao::window::Icon {
    tao::window::Icon::from_rgba(render(64, State::Connected), 64, 64).expect("icon")
}
