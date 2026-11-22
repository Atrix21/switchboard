//! X11 CLIPBOARD: change detection via XFixes, reading via ConvertSelection
//! (with INCR), and an owner thread that serves our content (with INCR for
//! large payloads). Text, PNG images and file lists (text/uri-list and
//! x-special/gnome-copied-files, which Nautilus uses for paste).

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, Atom, AtomEnum, ConnectionExt as _, EventMask, PropMode, Window};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::clipboard::{Backend, Clip};

fn xerr<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("X11 clipboard: {e}"))
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    incr: Atom,
    utf8: Atom,
    text_plain_utf8: Atom,
    text_plain: Atom,
    text: Atom,
    png: Atom,
    uri_list: Atom,
    gnome_files: Atom,
    prop: Atom,
}

impl Atoms {
    fn new(c: &RustConnection) -> io::Result<Atoms> {
        let a = |n: &str| -> io::Result<Atom> { Ok(c.intern_atom(false, n.as_bytes()).map_err(xerr)?.reply().map_err(xerr)?.atom) };
        Ok(Atoms {
            clipboard: a("CLIPBOARD")?,
            targets: a("TARGETS")?,
            timestamp: a("TIMESTAMP")?,
            incr: a("INCR")?,
            utf8: a("UTF8_STRING")?,
            text_plain_utf8: a("text/plain;charset=utf-8")?,
            text_plain: a("text/plain")?,
            text: a("TEXT")?,
            png: a("image/png")?,
            uri_list: a("text/uri-list")?,
            gnome_files: a("x-special/gnome-copied-files")?,
            prop: a("SWITCHBOARD_SEL")?,
        })
    }
}

fn make_window(c: &RustConnection) -> io::Result<Window> {
    let screen = &c.setup().roots[0];
    let w = c.generate_id().map_err(xerr)?;
    c.create_window(0, w, screen.root, 0, 0, 1, 1, 0, xproto::WindowClass::INPUT_ONLY, 0, &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE))
        .map_err(xerr)?;
    Ok(w)
}

struct Incr {
    data: Arc<Vec<u8>>,
    pos: usize,
    target: Atom,
    started: Instant,
}

pub struct XClipboard {
    c: Arc<RustConnection>,
    win: Window,
    atoms: Arc<Atoms>,
    served: Arc<Mutex<Option<Clip>>>,
    count: Arc<AtomicU64>,
    reader: Mutex<(RustConnection, Window, Atoms)>,
}

pub fn new() -> io::Result<Arc<dyn Backend>> {
    let (c, _) = x11rb::connect(None).map_err(xerr)?;
    c.extension_information(xfixes::X11_EXTENSION_NAME).map_err(xerr)?.ok_or_else(|| xerr("XFIXES missing"))?;
    c.xfixes_query_version(5, 0).map_err(xerr)?.reply().map_err(xerr)?;
    let atoms = Arc::new(Atoms::new(&c)?);
    let win = make_window(&c)?;
    c.xfixes_select_selection_input(win, atoms.clipboard, xfixes::SelectionEventMask::SET_SELECTION_OWNER | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE)
        .map_err(xerr)?;
    c.flush().map_err(xerr)?;
    let (rc, _) = x11rb::connect(None).map_err(xerr)?;
    let rwin = make_window(&rc)?;
    let ratoms = Atoms::new(&rc)?;
    rc.flush().map_err(xerr)?;
    let me = Arc::new(XClipboard { c: Arc::new(c), win, atoms, served: Arc::new(Mutex::new(None)), count: Arc::new(AtomicU64::new(0)), reader: Mutex::new((rc, rwin, ratoms)) });
    let t = me.clone();
    std::thread::Builder::new().name("sb-xclip".into()).spawn(move || t.serve_loop())?;
    Ok(me)
}

fn file_uri(p: &std::path::Path) -> String {
    let mut s = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn parse_uris(s: &str) -> Vec<PathBuf> {
    s.lines()
        .filter_map(|l| l.trim().strip_prefix("file://"))
        .map(|p| {
            let p = p.trim_start_matches(|c| c != '/'); // drop optional host
            let b = p.as_bytes();
            let mut out = Vec::with_capacity(b.len());
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' && i + 2 < b.len() {
                    if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                }
                out.push(b[i]);
                i += 1;
            }
            PathBuf::from(String::from_utf8_lossy(&out).to_string())
        })
        .collect()
}

impl XClipboard {
    fn targets_for(&self, c: &Clip) -> Vec<Atom> {
        let a = &self.atoms;
        let mut v = vec![a.targets, a.timestamp];
        match c {
            Clip::Text(_) => v.extend([a.utf8, a.text_plain_utf8, a.text_plain, a.text, AtomEnum::STRING.into()]),
            Clip::Png(_) => v.push(a.png),
            Clip::Files(_) => v.extend([a.gnome_files, a.uri_list, a.utf8, a.text_plain_utf8]),
        }
        v
    }

    fn data_for(&self, c: &Clip, target: Atom) -> Option<Vec<u8>> {
        let a = &self.atoms;
        match c {
            Clip::Text(t) if [a.utf8, a.text_plain_utf8, a.text_plain, a.text, AtomEnum::STRING.into()].contains(&target) => Some(t.as_bytes().to_vec()),
            Clip::Png(p) if target == a.png => Some(p.clone()),
            Clip::Files(fs) if target == a.uri_list => Some(fs.iter().map(|f| file_uri(f) + "\r\n").collect::<String>().into_bytes()),
            Clip::Files(fs) if target == a.gnome_files => Some(format!("copy\n{}", fs.iter().map(|f| file_uri(f)).collect::<Vec<_>>().join("\n")).into_bytes()),
            Clip::Files(fs) if target == a.utf8 || target == a.text_plain_utf8 => Some(fs.iter().map(|f| f.to_string_lossy().to_string()).collect::<Vec<_>>().join("\n").into_bytes()),
            _ => None,
        }
    }

    fn serve_loop(self: Arc<Self>) {
        let c = &*self.c;
        let max = (c.maximum_request_bytes() - 1024).min(256 * 1024);
        let mut incr: HashMap<(Window, Atom), Incr> = HashMap::new();
        loop {
            let ev = match c.wait_for_event() {
                Ok(e) => e,
                Err(e) => {
                    log::error!("X clipboard connection lost: {e}");
                    return;
                }
            };
            match ev {
                Event::XfixesSelectionNotify(e) if e.selection == self.atoms.clipboard => {
                    if e.owner != self.win {
                        self.count.fetch_add(1, Ordering::SeqCst);
                    }
                }
                Event::SelectionClear(e) if e.selection == self.atoms.clipboard => {
                    *self.served.lock().unwrap() = None;
                }
                Event::SelectionRequest(r) => {
                    let served = self.served.lock().unwrap().clone();
                    let prop = if r.property == x11rb::NONE { r.target } else { r.property };
                    let mut ok = false;
                    if let Some(clip) = served {
                        if r.target == self.atoms.targets {
                            let t = self.targets_for(&clip);
                            ok = c.change_property32(PropMode::REPLACE, r.requestor, prop, AtomEnum::ATOM, &t).is_ok();
                        } else if r.target == self.atoms.timestamp {
                            ok = c.change_property32(PropMode::REPLACE, r.requestor, prop, AtomEnum::INTEGER, &[r.time]).is_ok();
                        } else if let Some(d) = self.data_for(&clip, r.target) {
                            if d.len() <= max {
                                ok = c.change_property8(PropMode::REPLACE, r.requestor, prop, r.target, &d).is_ok();
                            } else {
                                // INCR: announce size, then send chunks as the requestor deletes the property.
                                let _ = c.change_window_attributes(r.requestor, &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE));
                                ok = c.change_property32(PropMode::REPLACE, r.requestor, prop, self.atoms.incr, &[d.len() as u32]).is_ok();
                                incr.insert((r.requestor, prop), Incr { data: Arc::new(d), pos: 0, target: r.target, started: Instant::now() });
                            }
                        }
                    }
                    let n = xproto::SelectionNotifyEvent { response_type: xproto::SELECTION_NOTIFY_EVENT, sequence: 0, time: r.time, requestor: r.requestor, selection: r.selection, target: r.target, property: if ok { prop } else { x11rb::NONE } };
                    let _ = c.send_event(false, r.requestor, EventMask::NO_EVENT, n);
                    let _ = c.flush();
                }
                Event::PropertyNotify(p) if p.state == xproto::Property::DELETE => {
                    if let Some(st) = incr.get_mut(&(p.window, p.atom)) {
                        let end = (st.pos + max).min(st.data.len());
                        let chunk = &st.data[st.pos..end];
                        let _ = c.change_property8(PropMode::REPLACE, p.window, p.atom, st.target, chunk);
                        let done = chunk.is_empty();
                        st.pos = end;
                        if done {
                            incr.remove(&(p.window, p.atom));
                        }
                        let _ = c.flush();
                    }
                }
                _ => {}
            }
            incr.retain(|_, s| s.started.elapsed() < Duration::from_secs(30));
        }
    }
}

fn wait_event<F: FnMut(&Event) -> bool>(c: &RustConnection, timeout: Duration, mut f: F) -> Option<Event> {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        match c.poll_for_event() {
            Ok(Some(e)) => {
                if f(&e) {
                    return Some(e);
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(_) => return None,
        }
    }
    None
}

fn read_target(c: &RustConnection, win: Window, a: &Atoms, target: Atom) -> Option<Vec<u8>> {
    c.convert_selection(win, a.clipboard, target, a.prop, x11rb::CURRENT_TIME).ok()?;
    c.flush().ok()?;
    let ev = wait_event(c, Duration::from_millis(1500), |e| matches!(e, Event::SelectionNotify(n) if n.requestor == win))?;
    let Event::SelectionNotify(n) = ev else { return None };
    if n.property == x11rb::NONE {
        return None;
    }
    let r = c.get_property(true, win, a.prop, AtomEnum::ANY, 0, u32::MAX / 4).ok()?.reply().ok()?;
    if r.type_ != a.incr {
        return Some(r.value);
    }
    // INCR transfer: each deletion (done by get_property above) prompts the next chunk.
    let mut out = Vec::new();
    loop {
        wait_event(c, Duration::from_secs(2), |e| matches!(e, Event::PropertyNotify(p) if p.window == win && p.atom == a.prop && p.state == xproto::Property::NEW_VALUE))?;
        let r = c.get_property(true, win, a.prop, AtomEnum::ANY, 0, u32::MAX / 4).ok()?.reply().ok()?;
        if r.value.is_empty() {
            return Some(out);
        }
        out.extend_from_slice(&r.value);
        if out.len() > 256 << 20 {
            return None;
        }
    }
}

impl Backend for XClipboard {
    fn change_count(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }

    fn read(&self) -> Option<Clip> {
        let g = self.reader.lock().unwrap();
        let (c, win, a) = (&g.0, g.1, &g.2);
        let owner = c.get_selection_owner(a.clipboard).ok()?.reply().ok()?.owner;
        if owner == x11rb::NONE || owner == self.win {
            return None;
        }
        let t = read_target(c, win, a, a.targets)?;
        let targets: Vec<Atom> = t.chunks_exact(4).map(|b| u32::from_ne_bytes(b.try_into().unwrap())).collect();
        if targets.contains(&a.gnome_files) || targets.contains(&a.uri_list) {
            let tgt = if targets.contains(&a.uri_list) { a.uri_list } else { a.gnome_files };
            if let Some(d) = read_target(c, win, a, tgt) {
                let fs = parse_uris(&String::from_utf8_lossy(&d));
                if !fs.is_empty() {
                    return Some(Clip::Files(fs));
                }
            }
        }
        if targets.contains(&a.png) {
            if let Some(d) = read_target(c, win, a, a.png) {
                return Some(Clip::Png(d));
            }
        }
        for tgt in [a.utf8, a.text_plain_utf8, AtomEnum::STRING.into()] {
            if targets.contains(&tgt) {
                if let Some(d) = read_target(c, win, a, tgt) {
                    return Some(Clip::Text(String::from_utf8_lossy(&d).to_string()));
                }
            }
        }
        None
    }

    fn write(&self, clip: &Clip) -> io::Result<()> {
        *self.served.lock().unwrap() = Some(clip.clone());
        self.c.set_selection_owner(self.win, self.atoms.clipboard, x11rb::CURRENT_TIME).map_err(xerr)?;
        self.c.flush().map_err(xerr)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn uris() {
        let p = std::path::Path::new("/home/a b/ü.txt");
        let u = super::file_uri(p);
        assert_eq!(u, "file:///home/a%20b/%C3%BC.txt");
        assert_eq!(super::parse_uris(&format!("{u}\r\n# comment\nfile://host/x")), vec![p.to_path_buf(), std::path::PathBuf::from("/x")]);
    }
}
