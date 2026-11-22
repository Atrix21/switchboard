//! Clipboard synchronization and file transfer.
//!
//! Policy
//! * Text and images are pushed to every paired peer as soon as they are
//!   copied (small, and the next paste is probably on the other machine).
//! * Files/folders are *offered* on copy and transferred when the pointer
//!   moves to that peer (that is when a paste can happen), up to a size cap.
//!   They are written to a fresh `Downloads/Switchboard/<time>/` folder (never
//!   overwriting anything) which is then placed on the local clipboard.
//! * Loop prevention: content we place on the clipboard from a peer is hashed;
//!   the resulting local change notification is recognized and ignored. The
//!   same content is never sent twice to the same peer.
//!
//! Wire: `Ctrl::ClipData { id, kind, total }` followed by binary
//! `Frame::Data` chunks (≤ 60 KiB so control messages interleave).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use blake2::{Blake2s256, Digest};
use serde::Serialize;

use crate::engine::{Engine, Peer};
use crate::proto::{ClipKind, Ctrl, Frame};

#[derive(Clone, Debug, PartialEq)]
pub enum Clip {
    Text(String),
    /// PNG-encoded image.
    Png(Vec<u8>),
    Files(Vec<PathBuf>),
}

pub trait Backend: Send + Sync {
    /// Increments whenever the system clipboard changes.
    fn change_count(&self) -> u64;
    /// Best available content: files > image > text.
    fn read(&self) -> Option<Clip>;
    fn write(&self, c: &Clip) -> io::Result<()>;
}

const CHUNK: usize = 60 * 1024;
const MAX_TEXT: usize = 16 << 20;
const MAX_IMAGE: usize = 64 << 20;

type Hash = [u8; 32];

fn hash_clip(c: &Clip) -> Hash {
    let mut h = Blake2s256::new();
    match c {
        Clip::Text(t) => {
            h.update(b"t");
            h.update(t.replace("\r\n", "\n").as_bytes());
        }
        Clip::Png(p) => {
            h.update(b"p");
            h.update(p);
        }
        Clip::Files(fs) => {
            h.update(b"f");
            for f in fs {
                h.update(f.to_string_lossy().as_bytes());
                if let Ok(m) = std::fs::metadata(f) {
                    h.update(m.len().to_le_bytes());
                    if let Ok(t) = m.modified() {
                        h.update(format!("{t:?}").as_bytes());
                    }
                }
            }
        }
    }
    h.finalize().into()
}

#[derive(Clone, Debug, Serialize)]
pub struct Transfer {
    pub id: u64,
    pub peer: String,
    pub outgoing: bool,
    pub kind: ClipKind,
    pub bytes: u64,
    pub total: u64,
    pub done: bool,
    pub error: Option<String>,
    pub dest: Option<String>,
    #[serde(skip)]
    pub started: Option<Instant>,
}

enum Incoming {
    Buf { kind: ClipKind, buf: Vec<u8> },
    Files { rx: ArchiveReader },
}

#[derive(Default)]
struct State {
    last_count: u64,
    /// Hash of what we last wrote from a peer (its change notification is ignored).
    ignore: Option<Hash>,
    current: Option<(Clip, Hash)>,
    sent: HashMap<String, Hash>,
    incoming: HashMap<(String, u64), (Incoming, usize)>,
    transfers: Vec<Transfer>,
    next_id: u64,
}

pub struct ClipSync {
    engine: Weak<Engine>,
    be: Option<Arc<dyn Backend>>,
    st: Mutex<State>,
}

impl ClipSync {
    pub fn start(engine: &Arc<Engine>) -> Arc<ClipSync> {
        let be = match crate::platform::clipboard() {
            Ok(b) => Some(b),
            Err(e) => {
                log::warn!("clipboard unavailable: {e}");
                None
            }
        };
        let c = Arc::new(ClipSync { engine: Arc::downgrade(engine), be, st: Mutex::new(State { next_id: crate::net::random_u32() as u64, ..Default::default() }) });
        if let Some(be) = &c.be {
            c.st.lock().unwrap().last_count = be.change_count();
            let w = Arc::downgrade(&c);
            std::thread::Builder::new()
                .name("sb-clip".into())
                .spawn(move || loop {
                    std::thread::sleep(Duration::from_millis(200));
                    let Some(c) = w.upgrade() else { return };
                    c.poll();
                })
                .expect("spawn clipboard thread");
        }
        c
    }

    fn cfg(&self) -> Option<crate::config::ClipboardConfig> {
        Some(self.engine.upgrade()?.cfg.read().unwrap().clipboard.clone())
    }

    fn poll(&self) {
        let Some(be) = &self.be else { return };
        let n = be.change_count();
        {
            let mut st = self.st.lock().unwrap();
            if n == st.last_count {
                return;
            }
            st.last_count = n;
        }
        let Some(cfg) = self.cfg() else { return };
        if !cfg.enabled {
            return;
        }
        let Some(clip) = be.read() else { return };
        let h = hash_clip(&clip);
        {
            let mut st = self.st.lock().unwrap();
            if st.ignore == Some(h) {
                return; // our own write
            }
            st.current = Some((clip.clone(), h));
        }
        let Some(e) = self.engine.upgrade() else { return };
        match &clip {
            Clip::Text(t) if cfg.text && t.len() <= MAX_TEXT => {
                log::info!("clipboard: text ({} chars) -> peers", t.chars().count());
                for p in e.trusted_peers() {
                    self.push(&p);
                }
            }
            Clip::Png(img) if cfg.images && img.len() <= MAX_IMAGE => {
                log::info!("clipboard: image ({} KiB) -> peers", img.len() / 1024);
                for p in e.trusted_peers() {
                    self.push(&p);
                }
            }
            Clip::Files(fs) if cfg.files => {
                let (n, bytes) = tree_size(fs);
                log::info!("clipboard: {} item(s), {n} file(s), {} MiB – offered; sent when the pointer moves", fs.len(), bytes >> 20);
                let summary = format!("{n} file(s), {} MiB", bytes >> 20);
                for p in e.trusted_peers() {
                    let id = self.next_id();
                    let s = summary.clone();
                    std::thread::spawn(move || p.send(Ctrl::ClipOffer { id, kinds: vec![ClipKind::Files], summary: s }));
                }
            }
            _ => {}
        }
    }

    fn next_id(&self) -> u64 {
        let mut st = self.st.lock().unwrap();
        st.next_id += 1;
        st.next_id
    }

    /// Pointer moved to `p`: make sure it has our current clipboard.
    pub fn on_focus(self: &Arc<Self>, p: &Arc<Peer>) {
        let me = self.clone();
        let p = p.clone();
        std::thread::spawn(move || me.push(&p));
    }

    /// Paired peer (re)connected.
    pub fn on_peer_ready(self: &Arc<Self>, p: &Arc<Peer>) {
        // Only small content proactively; files wait for the pointer.
        let is_small = matches!(self.st.lock().unwrap().current, Some((Clip::Text(_), _)) | Some((Clip::Png(_), _)));
        if is_small {
            self.on_focus(p);
        }
    }

    /// Send the current clipboard to `p` unless it already has it.
    fn push(&self, p: &Arc<Peer>) {
        let Some(cfg) = self.cfg() else { return };
        let (clip, h) = {
            let st = self.st.lock().unwrap();
            match &st.current {
                Some((c, h)) if st.sent.get(&p.key_hex) != Some(h) => (c.clone(), *h),
                _ => return,
            }
        };
        self.st.lock().unwrap().sent.insert(p.key_hex.clone(), h);
        let id = self.next_id();
        let r = match clip {
            Clip::Text(t) => self.send_buf(p, id, ClipKind::Text, t.into_bytes()),
            Clip::Png(b) => self.send_buf(p, id, ClipKind::Png, b),
            Clip::Files(fs) => {
                let (_, bytes) = tree_size(&fs);
                if bytes > cfg.max_files_mib << 20 {
                    log::warn!("clipboard files ({} MiB) exceed the {} MiB limit; not sent", bytes >> 20, cfg.max_files_mib);
                    return;
                }
                self.send_files(p, id, &fs, bytes)
            }
        };
        if let Err(e) = r {
            log::warn!("clipboard send to {}: {e}", p.name());
            self.st.lock().unwrap().sent.remove(&p.key_hex);
        }
    }

    fn track(&self, t: Transfer) {
        let mut st = self.st.lock().unwrap();
        st.transfers.retain(|x| !(x.done && x.started.is_some_and(|s| s.elapsed() > Duration::from_secs(600))));
        if let Some(x) = st.transfers.iter_mut().find(|x| x.id == t.id && x.outgoing == t.outgoing) {
            *x = t;
        } else {
            st.transfers.push(t);
        }
    }

    fn send_buf(&self, p: &Arc<Peer>, id: u64, kind: ClipKind, b: Vec<u8>) -> io::Result<()> {
        p.sess.send(&Frame::Ctrl(Ctrl::ClipData { id, kind, total: b.len() as u64 }))?;
        let mut chunks = b.chunks(CHUNK).peekable();
        if chunks.peek().is_none() {
            p.sess.send(&Frame::Data { id, last: true, bytes: vec![] })?;
        }
        while let Some(c) = chunks.next() {
            p.sess.send(&Frame::Data { id, last: chunks.peek().is_none(), bytes: c.to_vec() })?;
        }
        Ok(())
    }

    fn send_files(&self, p: &Arc<Peer>, id: u64, fs: &[PathBuf], total: u64) -> io::Result<()> {
        log::info!("sending {} MiB of files to {}", total >> 20, p.name());
        let mut t = Transfer { id, peer: p.name(), outgoing: true, kind: ClipKind::Files, bytes: 0, total, done: false, error: None, dest: None, started: Some(Instant::now()) };
        self.track(t.clone());
        p.sess.send(&Frame::Ctrl(Ctrl::ClipData { id, kind: ClipKind::Files, total }))?;
        let mut w = ChunkWriter { p: p.clone(), id, buf: Vec::with_capacity(CHUNK), sent: 0 };
        let mut last_report = Instant::now();
        let mut res = Ok(());
        for f in fs {
            let base = f.parent().unwrap_or(Path::new(""));
            if let Err(e) = write_tree(&mut w, base, f, &mut |n| {
                t.bytes += n;
                if last_report.elapsed() > Duration::from_millis(250) {
                    last_report = Instant::now();
                    self.track(t.clone());
                }
            }) {
                res = Err(e);
                break;
            }
        }
        if res.is_ok() {
            w.buf.push(0); // end marker
            res = w.finish();
        }
        t.done = true;
        t.error = res.as_ref().err().map(|e| e.to_string());
        if let Some(s) = t.started {
            log::info!("files to {}: {} MiB in {:.1}s{}", p.name(), t.bytes >> 20, s.elapsed().as_secs_f64(), t.error.as_deref().map(|e| format!(" – error {e}")).unwrap_or_default());
        }
        self.track(t);
        res
    }

    pub fn on_ctrl(&self, p: &Arc<Peer>, m: Ctrl) {
        match m {
            Ctrl::ClipOffer { summary, .. } => log::info!("{} copied {summary}; move the pointer there… or here to receive", p.name()),
            Ctrl::ClipData { id, kind, total } => {
                let inc = match kind {
                    ClipKind::Text if total as usize <= MAX_TEXT => Incoming::Buf { kind, buf: Vec::with_capacity(total as usize) },
                    ClipKind::Png if total as usize <= MAX_IMAGE => Incoming::Buf { kind, buf: Vec::with_capacity(total as usize) },
                    ClipKind::Files => match ArchiveReader::new(&download_dir()) {
                        Ok(rx) => {
                            self.track(Transfer { id, peer: p.name(), outgoing: false, kind, bytes: 0, total, done: false, error: None, dest: Some(rx.root.display().to_string()), started: Some(Instant::now()) });
                            Incoming::Files { rx }
                        }
                        Err(e) => {
                            log::error!("cannot create download folder: {e}");
                            return;
                        }
                    },
                    _ => return,
                };
                self.st.lock().unwrap().incoming.insert((p.key_hex.clone(), id), (inc, 0));
            }
            Ctrl::ClipError { detail, .. } => log::warn!("{}: clipboard error: {detail}", p.name()),
            _ => {}
        }
    }

    pub fn on_data(&self, p: &Arc<Peer>, id: u64, last: bool, bytes: Vec<u8>) {
        let key = (p.key_hex.clone(), id);
        let mut st = self.st.lock().unwrap();
        let Some((inc, got)) = st.incoming.get_mut(&key) else { return };
        *got += bytes.len();
        let got = *got;
        let r = match inc {
            Incoming::Buf { buf, .. } => {
                buf.extend_from_slice(&bytes);
                Ok(())
            }
            Incoming::Files { rx } => rx.feed(&bytes),
        };
        if let Err(e) = r {
            log::error!("receiving files from {}: {e}", p.name());
            st.incoming.remove(&key);
            return;
        }
        if let Some(t) = st.transfers.iter_mut().find(|t| t.id == id && !t.outgoing) {
            t.bytes = got as u64;
        }
        if !last {
            return;
        }
        let (inc, _) = st.incoming.remove(&key).unwrap();
        let clip = match inc {
            Incoming::Buf { kind: ClipKind::Text, buf } => String::from_utf8(buf).ok().map(Clip::Text),
            Incoming::Buf { kind: ClipKind::Png, buf } => Some(Clip::Png(buf)),
            Incoming::Buf { .. } => None,
            Incoming::Files { rx } => {
                let top = rx.top_level();
                if let Some(t) = st.transfers.iter_mut().find(|t| t.id == id && !t.outgoing) {
                    t.done = true;
                }
                log::info!("received {} item(s) from {} into {}", top.len(), p.name(), rx.root.display());
                Some(Clip::Files(top))
            }
        };
        let Some(clip) = clip else { return };
        match &clip {
            Clip::Text(t) => log::info!("clipboard from {}: text ({} chars)", p.name(), t.chars().count()),
            Clip::Png(b) => log::info!("clipboard from {}: image ({} KiB)", p.name(), b.len() / 1024),
            Clip::Files(_) => {}
        }
        let h = hash_clip(&clip);
        st.ignore = Some(h);
        // The sender (and we) already have this content.
        st.sent.insert(p.key_hex.clone(), h);
        st.current = Some((clip.clone(), h));
        drop(st);
        if let Some(be) = &self.be {
            if let Err(e) = be.write(&clip) {
                log::warn!("setting clipboard: {e}");
            }
        }
    }

    pub fn transfers(&self) -> Vec<Transfer> {
        self.st.lock().unwrap().transfers.clone()
    }
}

struct ChunkWriter {
    p: Arc<Peer>,
    id: u64,
    buf: Vec<u8>,
    sent: u64,
}

impl ChunkWriter {
    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        let mut b = b;
        while !b.is_empty() {
            let n = (CHUNK - self.buf.len()).min(b.len());
            self.buf.extend_from_slice(&b[..n]);
            b = &b[n..];
            if self.buf.len() == CHUNK {
                self.p.sess.send(&Frame::Data { id: self.id, last: false, bytes: std::mem::take(&mut self.buf) })?;
                self.sent += CHUNK as u64;
                self.buf.reserve(CHUNK);
            }
        }
        Ok(())
    }
    fn finish(mut self) -> io::Result<()> {
        self.p.sess.send(&Frame::Data { id: self.id, last: true, bytes: std::mem::take(&mut self.buf) })
    }
}

// ---------------------------------------------------------------- archive
//
// entry := type:u8 (0 = end, 1 = dir, 2 = file) name_len:u16 name:utf8 [size:u64 data]
// Names are relative, '/'-separated.

fn tree_size(fs: &[PathBuf]) -> (u64, u64) {
    fn walk(p: &Path, n: &mut u64, b: &mut u64) {
        let Ok(m) = std::fs::symlink_metadata(p) else { return };
        if m.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    walk(&e.path(), n, b);
                }
            }
        } else if m.is_file() {
            *n += 1;
            *b += m.len();
        }
    }
    let (mut n, mut b) = (0, 0);
    for f in fs {
        walk(f, &mut n, &mut b);
    }
    (n, b)
}

fn rel_name(base: &Path, p: &Path) -> String {
    p.strip_prefix(base).unwrap_or(p).components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect::<Vec<_>>().join("/")
}

fn write_tree(w: &mut ChunkWriter, base: &Path, p: &Path, progress: &mut dyn FnMut(u64)) -> io::Result<()> {
    let m = std::fs::symlink_metadata(p)?;
    let name = rel_name(base, p);
    if name.len() > u16::MAX as usize {
        return Ok(());
    }
    if m.is_dir() {
        w.put(&[1])?;
        w.put(&(name.len() as u16).to_le_bytes())?;
        w.put(name.as_bytes())?;
        let mut entries: Vec<_> = std::fs::read_dir(p)?.flatten().map(|e| e.path()).collect();
        entries.sort();
        for e in entries {
            write_tree(w, base, &e, progress)?;
        }
    } else if m.is_file() {
        let mut f = File::open(p)?;
        let size = m.len();
        w.put(&[2])?;
        w.put(&(name.len() as u16).to_le_bytes())?;
        w.put(name.as_bytes())?;
        w.put(&size.to_le_bytes())?;
        let mut left = size;
        let mut buf = vec![0u8; CHUNK];
        while left > 0 {
            let n = f.read(&mut buf[..(left.min(CHUNK as u64) as usize)])?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{} shrank while sending", p.display())));
            }
            w.put(&buf[..n])?;
            left -= n as u64;
            progress(n as u64);
        }
    }
    Ok(())
}

pub fn download_dir() -> PathBuf {
    let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    home.join("Downloads").join("Switchboard")
}

/// Make a received path component safe on this OS (no traversal, no
/// reserved characters).
fn sanitize(name: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        let clean: String = part.chars().map(|c| if c.is_control() || "<>:\"\\|?*".contains(c) { '_' } else { c }).collect();
        let clean = clean.trim_end_matches([' ', '.']).to_string();
        if clean.is_empty() {
            return None;
        }
        out.push(clean);
    }
    Some(out)
}

/// Incremental archive parser writing into a fresh timestamped folder.
pub struct ArchiveReader {
    pub root: PathBuf,
    buf: Vec<u8>,
    file: Option<(File, u64)>,
    top: Vec<PathBuf>,
    ended: bool,
}

impl ArchiveReader {
    pub fn new(base: &Path) -> io::Result<ArchiveReader> {
        let stamp = {
            let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
            format!("{s}")
        };
        let mut root = base.join(&stamp);
        let mut i = 1;
        while root.exists() {
            root = base.join(format!("{stamp}-{i}"));
            i += 1;
        }
        std::fs::create_dir_all(&root)?;
        Ok(ArchiveReader { root, buf: Vec::new(), file: None, top: vec![], ended: false })
    }

    fn note_top(&mut self, rel: &Path) {
        if let Some(first) = rel.components().next() {
            let t = self.root.join(first.as_os_str());
            if !self.top.contains(&t) {
                self.top.push(t);
            }
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> io::Result<()> {
        self.buf.extend_from_slice(data);
        let mut pos = 0;
        let res = self.parse(&mut pos);
        self.buf.drain(..pos);
        res
    }

    fn parse(&mut self, pos: &mut usize) -> io::Result<()> {
        loop {
            if let Some((f, left)) = &mut self.file {
                let n = (*left).min((self.buf.len() - *pos) as u64) as usize;
                f.write_all(&self.buf[*pos..*pos + n])?;
                *left -= n as u64;
                *pos += n;
                if *left == 0 {
                    self.file = None;
                }
                if *pos == self.buf.len() {
                    return Ok(());
                }
                continue;
            }
            let b = &self.buf[*pos..];
            if self.ended || b.is_empty() {
                *pos = self.buf.len();
                return Ok(());
            }
            if b[0] == 0 {
                self.ended = true;
                continue;
            }
            if b.len() < 3 {
                return Ok(());
            }
            let nl = u16::from_le_bytes([b[1], b[2]]) as usize;
            let hdr = 3 + nl + if b[0] == 2 { 8 } else { 0 };
            if b.len() < hdr {
                return Ok(());
            }
            let kind = b[0];
            let name = String::from_utf8_lossy(&b[3..3 + nl]).to_string();
            let size = if kind == 2 { u64::from_le_bytes(b[3 + nl..hdr].try_into().unwrap()) } else { 0 };
            let rel = sanitize(&name).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("unsafe path {name:?}")))?;
            let path = self.root.join(&rel);
            match kind {
                1 => std::fs::create_dir_all(&path)?,
                2 => {
                    if let Some(d) = path.parent() {
                        std::fs::create_dir_all(d)?;
                    }
                    let f = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
                    if size > 0 {
                        self.file = Some((f, size));
                    }
                }
                t => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad entry type {t}"))),
            }
            self.note_top(&rel);
            *pos += hdr;
        }
    }

    pub fn top_level(&self) -> Vec<PathBuf> {
        self.top.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_paths() {
        assert_eq!(sanitize("a/b.txt"), Some(PathBuf::from("a").join("b.txt")));
        assert_eq!(sanitize("../etc/passwd"), None);
        assert_eq!(sanitize("/abs"), None);
        assert_eq!(sanitize("a/./b"), None);
        assert_eq!(sanitize("c:x?.txt"), Some(PathBuf::from("c_x_.txt")));
    }

    #[test]
    fn archive_roundtrip_any_chunking() {
        // Build an archive by hand: dir "d", file "d/x" (5 bytes), file "y" (0 bytes).
        let mut a = vec![1u8];
        a.extend_from_slice(&1u16.to_le_bytes());
        a.extend_from_slice(b"d");
        a.push(2);
        a.extend_from_slice(&3u16.to_le_bytes());
        a.extend_from_slice(b"d/x");
        a.extend_from_slice(&5u64.to_le_bytes());
        a.extend_from_slice(b"hello");
        a.push(2);
        a.extend_from_slice(&1u16.to_le_bytes());
        a.extend_from_slice(b"y");
        a.extend_from_slice(&0u64.to_le_bytes());
        a.push(0);
        for chunk in [1usize, 2, 3, 7, 1000] {
            let base = std::env::temp_dir().join(format!("sb-arch-test-{}-{chunk}", std::process::id()));
            let mut r = ArchiveReader::new(&base).unwrap();
            for c in a.chunks(chunk) {
                r.feed(c).unwrap();
            }
            assert!(r.ended, "chunk {chunk}");
            assert_eq!(std::fs::read(r.root.join("d").join("x")).unwrap(), b"hello");
            assert_eq!(std::fs::read(r.root.join("y")).unwrap(), b"");
            assert_eq!(r.top_level().len(), 2);
            let _ = std::fs::remove_dir_all(base);
        }
    }

    #[test]
    fn archive_rejects_traversal() {
        let mut a = vec![2u8];
        a.extend_from_slice(&5u16.to_le_bytes());
        a.extend_from_slice(b"../zz");
        a.extend_from_slice(&0u64.to_le_bytes());
        let base = std::env::temp_dir().join(format!("sb-arch-bad-{}", std::process::id()));
        let mut r = ArchiveReader::new(&base).unwrap();
        assert!(r.feed(&a).is_err());
        let _ = std::fs::remove_dir_all(base);
    }
}
