//! Screen-edge geometry: where links live, when the pointer is "pushing
//! through" an edge, and where it should appear on the other side.

use crate::proto::{Edge, Rect, ScreenInfo};

/// Bounding box of the screens that form the outermost boundary on `edge`.
/// With `screen` given, the matching display's rect instead.
pub fn segment(screens: &[ScreenInfo], edge: Edge, screen: Option<&str>) -> Option<Rect> {
    if let Some(name) = screen {
        return screens.iter().find(|s| s.name.contains(name)).map(|s| s.rect);
    }
    if screens.is_empty() {
        return None;
    }
    let key = |r: &Rect| match edge {
        Edge::Left => -r.x,
        Edge::Right => r.right(),
        Edge::Top => -r.y,
        Edge::Bottom => r.bottom(),
    };
    let best = screens.iter().map(|s| key(&s.rect)).max()?;
    let outer: Vec<Rect> = screens.iter().map(|s| s.rect).filter(|r| key(r) == best).collect();
    Some(bbox(&outer))
}

pub fn bbox(rs: &[Rect]) -> Rect {
    let x0 = rs.iter().map(|r| r.x).min().unwrap_or(0);
    let y0 = rs.iter().map(|r| r.y).min().unwrap_or(0);
    let x1 = rs.iter().map(|r| r.right()).max().unwrap_or(0);
    let y1 = rs.iter().map(|r| r.bottom()).max().unwrap_or(0);
    Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

pub fn desktop(screens: &[ScreenInfo]) -> Rect {
    bbox(&screens.iter().map(|s| s.rect).collect::<Vec<_>>())
}

/// Outward component of a motion relative to `edge` (positive = pushing out).
pub fn outward(edge: Edge, dx: i32, dy: i32) -> i32 {
    match edge {
        Edge::Left => -dx,
        Edge::Right => dx,
        Edge::Top => -dy,
        Edge::Bottom => dy,
    }
}

/// If the cursor sits on `edge` of `seg` (outside the corner dead zones, and
/// with no other screen beyond it), returns the 0..1 position along the edge.
pub fn on_edge(screens: &[ScreenInfo], seg: &Rect, edge: Edge, x: i32, y: i32, deadzone: i32) -> Option<f32> {
    let (at, along, lo, len, beyond) = match edge {
        Edge::Right => (x >= seg.right() - 1, y, seg.y, seg.h, (x + 1, y)),
        Edge::Left => (x <= seg.x, y, seg.y, seg.h, (x - 1, y)),
        Edge::Top => (y <= seg.y, x, seg.x, seg.w, (x, y - 1)),
        Edge::Bottom => (y >= seg.bottom() - 1, x, seg.x, seg.w, (x, y + 1)),
    };
    if !at || len <= 0 {
        return None;
    }
    let dz = deadzone.min(len / 4);
    if along < lo + dz || along >= lo + len - dz {
        return None;
    }
    if screens.iter().any(|s| s.rect.contains(beyond.0, beyond.1)) {
        return None;
    }
    Some(((along - lo) as f32 / len as f32).clamp(0.0, 1.0))
}

/// Where the pointer appears when entering through `edge` of `seg` at `pos`.
/// One pixel inside so that it is visible and not immediately "on" the edge.
pub fn entry_point(seg: &Rect, edge: Edge, pos: f32) -> (i32, i32) {
    let pos = pos.clamp(0.0, 1.0);
    let ax = seg.x + ((seg.w - 1) as f32 * pos).round() as i32;
    let ay = seg.y + ((seg.h - 1) as f32 * pos).round() as i32;
    match edge {
        Edge::Left => (seg.x + 1, ay),
        Edge::Right => (seg.right() - 2, ay),
        Edge::Top => (ax, seg.y + 1),
        Edge::Bottom => (ax, seg.bottom() - 2),
    }
}

/// Edge-push accumulator implementing configurable resistance.
///
/// A disarmed edge ignores pushes until the pointer has been off it once:
/// after arriving *on* an edge (e.g. a monitor "flip" gesture) a continued
/// push must not bounce the pointer straight back.
#[derive(Debug)]
pub struct Push {
    acc: i32,
    armed: bool,
}

impl Default for Push {
    fn default() -> Self {
        Push { acc: 0, armed: true }
    }
}

impl Push {
    pub fn disarm(&mut self) {
        self.acc = 0;
        self.armed = false;
    }
    /// Feed one motion sample. Returns true when the push exceeds `resistance`.
    pub fn feed(&mut self, on_edge: bool, out: i32, resistance: i32) -> bool {
        if !on_edge {
            self.armed = true;
        }
        if !self.armed {
            return false;
        }
        if !on_edge || out <= 0 {
            if out < 0 || !on_edge {
                self.acc = 0;
            }
            return false;
        }
        self.acc += out;
        if self.acc > resistance {
            self.acc = 0;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win() -> Vec<ScreenInfo> {
        // The actual Windows dev machine layout.
        vec![
            ScreenInfo { name: "Alienware_AW2726DM(DisplayPort)".into(), rect: Rect { x: 0, y: 0, w: 2560, h: 1440 }, primary: true },
            ScreenInfo { name: "Generic PnP Monitor".into(), rect: Rect { x: -1920, y: 195, w: 1920, h: 1080 }, primary: false },
        ]
    }

    #[test]
    fn segments() {
        let s = win();
        assert_eq!(segment(&s, Edge::Right, None), Some(Rect { x: 0, y: 0, w: 2560, h: 1440 }));
        assert_eq!(segment(&s, Edge::Left, None), Some(Rect { x: -1920, y: 195, w: 1920, h: 1080 }));
        assert_eq!(segment(&s, Edge::Top, Some("AW2726")), Some(Rect { x: 0, y: 0, w: 2560, h: 1440 }));
    }

    #[test]
    fn edges() {
        let s = win();
        let seg = segment(&s, Edge::Right, None).unwrap();
        assert!(on_edge(&s, &seg, Edge::Right, 2559, 720, 8).is_some());
        assert!(on_edge(&s, &seg, Edge::Right, 2558, 720, 8).is_none());
        assert!(on_edge(&s, &seg, Edge::Right, 2559, 3, 8).is_none(), "corner deadzone");
        // Left edge of the Alienware is not an exit: the other monitor is there.
        let aw = s[0].rect;
        assert!(on_edge(&s, &aw, Edge::Left, 0, 500, 8).is_none());
        // ...but the part above the Generic monitor (y<195) is open.
        assert!(on_edge(&s, &aw, Edge::Left, 0, 100, 8).is_some());
        let p = on_edge(&s, &seg, Edge::Right, 2559, 720, 0).unwrap();
        assert!((p - 0.5).abs() < 0.01);
    }

    #[test]
    fn entry() {
        let r = Rect { x: 0, y: 0, w: 1920, h: 1080 };
        assert_eq!(entry_point(&r, Edge::Left, 0.5), (1, 540));
        assert_eq!(entry_point(&r, Edge::Right, 0.0), (1918, 0));
        assert_eq!(entry_point(&r, Edge::Bottom, 1.0), (1919, 1078));
    }

    #[test]
    fn push() {
        let mut p = Push::default();
        assert!(p.feed(true, 3, 0));
        assert!(!p.feed(true, 3, 10));
        assert!(!p.feed(true, 3, 10));
        assert!(!p.feed(true, 3, 10));
        assert!(p.feed(true, 3, 10));
        assert!(!p.feed(true, 5, 10));
        assert!(!p.feed(false, 0, 10));
        assert!(!p.feed(true, 8, 10), "reset after leaving edge");
        let mut p = Push::default();
        p.disarm();
        assert!(!p.feed(true, 50, 0), "disarmed on arrival");
        assert!(!p.feed(false, -1, 0));
        assert!(p.feed(true, 1, 0), "re-armed after leaving the edge");
    }
}
