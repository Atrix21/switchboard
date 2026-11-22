//! The Switchboard engine: peers, pointer ownership state machine, input
//! forwarding (source) and injection (sink), watchdogs, pairing.
//!
//! Threads:
//! * capture thread (platform) → [`Engine::on_capture`]: edge detection and,
//!   while forwarding, encodes + sends input datagrams inline (no thread hop).
//! * UDP thread → [`Engine::on_datagram`]: sink-side decode + inject inline;
//!   source-side ack processing.
//! * one reader thread per peer control channel → events to the engine thread.
//! * engine thread: control messages, timers (keepalive, watchdogs, pings,
//!   reconnects), IPC requests.
//!
//! Lock order: `st` → `inj`. Never call into the network while holding `peers`.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::{Config, Link, PeerConfig};
use crate::ddc::Coordinator;
use crate::geom::{self, Push};
use crate::keymap::KeySet;
use crate::net::{self, Identity, Session};
use crate::platform::{self, CapEvent, Capture, CaptureHandler, CaptureOptions, Hotkey, Injector};
use crate::proto::*;
use crate::stats::{Histogram, RateMeter};

pub const HOTKEY_EMERGENCY: u32 = 1;
pub const HOTKEY_TOGGLE: u32 = 2;

const KEEPALIVE: Duration = Duration::from_millis(50);
const ACK_INTERVAL: Duration = Duration::from_millis(20);
const SINK_IDLE_RELEASE: Duration = Duration::from_millis(1000);
const PING_INTERVAL: Duration = Duration::from_secs(1);
const PEER_DEAD: Duration = Duration::from_secs(5);
const PAIR_TIMEOUT: Duration = Duration::from_secs(180);

pub fn now_us() -> u64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[derive(Default)]
pub struct PeerInfo {
    pub name: String,
    pub os: String,
    pub udp_addr: Option<SocketAddr>,
    pub send_id: u32,
    pub screens: Vec<ScreenInfo>,
    pub monitor_model: String,
    pub monitor_input: Option<u16>,
    pub rtt: Histogram,
    pub hello: bool,
    pub links: Vec<crate::config::Link>,
}

pub struct Peer {
    pub sess: Session,
    pub key_hex: String,
    pub fp: String,
    /// Id the peer puts in datagrams addressed to us.
    pub recv_id: u32,
    pub info: Mutex<PeerInfo>,
    pub trusted: AtomicBool,
    pair_local: AtomicBool,
    pair_remote: AtomicBool,
    last_rx: Mutex<Instant>,
    alive: AtomicBool,
    connected_at: Instant,
}

impl Peer {
    pub fn send(&self, m: Ctrl) {
        if let Err(e) = self.sess.send(&Frame::Ctrl(m)) {
            log::debug!("send to {}: {e}", self.fp);
            self.alive.store(false, Ordering::Relaxed);
            self.sess.shutdown();
        }
    }
    pub fn name(&self) -> String {
        let n = self.info.lock().unwrap().name.clone();
        if n.is_empty() {
            self.fp.clone()
        } else {
            n
        }
    }
}

#[derive(Clone)]
enum Mode {
    Local,
    /// We are the source; our physical pointer drives `peer`.
    Remote { peer: Arc<Peer>, epoch: u32, exit_edge: Edge, exit_seg: Rect },
    /// We are the sink; `peer`'s pointer drives our cursor.
    Hosted { peer: Arc<Peer>, epoch: u32 },
}

impl Mode {
    fn label(&self) -> String {
        match self {
            Mode::Local => "local".into(),
            Mode::Remote { peer, .. } => format!("controlling {}", peer.name()),
            Mode::Hosted { peer, .. } => format!("controlled by {}", peer.name()),
        }
    }
}

#[derive(Default)]
struct SinkState {
    cum_dx: i32,
    cum_dy: i32,
    wv: i32,
    wh: i32,
    ev_seq: u32,
    pressed: u16,
    last_rx: Option<Instant>,
    last_seq: u64,
    last_echo: u64,
    /// (edge, segment, landing target on the source) for every link back to the source.
    /// (edge, segment, landing target, dwell ms, link) for every link back to the source.
    returns: Vec<(Edge, Rect, Target, u64, Link)>,
    push: Vec<Push>,
    last_ack: Option<Instant>,
    received: u32,
    inject: Histogram,
}

/// Keyboard forwarding: this machine's keyboard -> focus owner.
#[derive(Default)]
struct KbSource {
    target: Option<Arc<Peer>>,
    st: KeyState,
    keys: KeySet,
    seq: u32,
    acked: u32,
    last_send: Option<Instant>,
    /// Keys consumed by a local hotkey (their release is swallowed too).
    swallowed: KeySet,
    unacked_since: Option<Instant>,
    enc: Vec<u8>,
    dgram: Vec<u8>,
}

/// Keys received from a peer's keyboard.
#[derive(Default)]
struct KbSink {
    from: Option<Arc<Peer>>,
    epoch: u32,
    seq: u32,
    pressed: KeySet,
    last_rx: Option<Instant>,
}

struct State {
    mode: Mode,
    /// Who has the pointer (None = this machine). Keystrokes follow it.
    focus: Option<Arc<Peer>>,
    focus_announced: bool,
    kb: KbSource,
    ksink: KbSink,
    hotkeys: Vec<(u32, Hotkey, u16)>,
    next_epoch: u32,
    disabled: bool,
    // ---- source
    src: InputState,
    ev_seq: u32,
    phys_buttons: u16,
    push: HashMap<usize, Push>,
    last_ack: Instant,
    last_send: Instant,
    sent: u64,
    chord_since: Option<Instant>,
    last_phys_motion: Option<Instant>,
    enc: Vec<u8>,
    dgram: Vec<u8>,
    // ---- sink
    sink: SinkState,
    // ---- cached geometry
    links: Vec<(Link, Rect)>,
}

#[derive(Default)]
pub struct Stats {
    pub capture_rate: RateMeter,
    pub send_rate: RateMeter,
    pub capture_cost: Histogram,
    pub udp_rx: u64,
    pub udp_bad: u64,
    pub switches: u64,
    pub last_switch_reason: String,
    pub acks: u64,
    pub sink_lost_seq: u64,
}

/// Optional explicit landing place on the other machine.
#[derive(Clone, Debug, Default)]
pub struct Target {
    pub screen: Option<String>,
    pub edge: Option<Edge>,
    pub switch_monitor: Option<bool>,
}

pub enum Ev {
    Connected,
    Frame(Arc<Peer>, Frame),
    Closed(Arc<Peer>),
    SendEnter(Arc<Peer>, u32, Edge, f32, Target),
    SendLeave(Arc<Peer>, u32, Edge, f32, Target),
    Hotkey(u32),
    Ipc(Value, Sender<Value>),
    Discovered { key_hex: String, addr: SocketAddr, name: String },
    MonitorManual(u16),
}

pub struct Engine {
    pub cfg: RwLock<Config>,
    pub id: Identity,
    pub fp: String,
    udp: UdpSocket,
    pub udp_port: u16,
    peers: RwLock<Vec<Arc<Peer>>>,
    st: Mutex<State>,
    capture: OnceLock<Arc<dyn Capture>>,
    inj: Mutex<Box<dyn Injector>>,
    screens: RwLock<Vec<ScreenInfo>>,
    pub monitor: Coordinator,
    pub tx: Sender<Ev>,
    pub stats: Mutex<Stats>,
    started: Instant,
    discovered: Mutex<HashMap<String, (SocketAddr, String, Instant)>>,
    dialing: Mutex<HashMap<String, Instant>>,
    pub clip: OnceLock<Arc<crate::clipboard::ClipSync>>,
    shutting_down: AtomicBool,
    /// Events that reached `on_capture` (paired with platform swallowed count
    /// to detect input being blocked locally but not forwarded).
    captured: AtomicU64,
    dead_check: Mutex<(Instant, u64, u64)>,
    seq_drop: AtomicU64,
}

struct Handler(Arc<Engine>);
impl CaptureHandler for Handler {
    fn on_event(&self, ev: CapEvent, cursor: (i32, i32)) {
        self.0.on_capture(ev, cursor);
    }
}

impl Engine {
    pub fn start(cfg: Config, id: Identity) -> std::io::Result<(Arc<Engine>, Receiver<Ev>)> {
        let (tx, rx) = channel();
        let udp = net::bind_udp(cfg.port)?;
        let udp_port = udp.local_addr()?.port();
        let screens = platform::screens();
        let inj = platform::new_injector()?;
        log::info!("injector: {}  screens: {}", inj.name(), screens.iter().map(|s| format!("{} {:?}", s.name, s.rect)).collect::<Vec<_>>().join(", "));

        // Shared monitor: resolve model + this machine's input.
        let mut model = cfg.monitor.model.clone();
        let mut local_input = cfg.monitor.local_input;
        if cfg.monitor.enabled {
            if let Some(m) = crate::ddc::detect(&model) {
                if model.is_empty() {
                    model = m.model.clone();
                }
                if local_input.is_none() {
                    local_input = m.connector.as_deref().and_then(crate::ddc::guess_input);
                }
                log::info!("shared monitor: {} (ddc {}), this machine on {:?}", m.model, if m.ddc_ok { "ok" } else { "unavailable" }, local_input.map(crate::config::input_name));
            } else {
                log::info!("no DDC/CI monitor found{}", if model.is_empty() { String::new() } else { format!(" matching {model:?}") });
            }
        }
        let monitor = Coordinator::new(if cfg.monitor.enabled { model } else { String::new() }, local_input);
        let fp = id.fingerprint();
        let e = Arc::new(Engine {
            cfg: RwLock::new(cfg),
            id,
            fp,
            udp,
            udp_port,
            peers: RwLock::new(vec![]),
            st: Mutex::new(State {
                mode: Mode::Local,
                focus: None,
                focus_announced: false,
                kb: KbSource::default(),
                ksink: KbSink::default(),
                hotkeys: vec![],
                next_epoch: net::random_u32() & 0xffff,
                disabled: false,
                src: InputState::default(),
                ev_seq: 0,
                phys_buttons: 0,
                push: HashMap::new(),
                last_ack: Instant::now(),
                last_send: Instant::now(),
                sent: 0,
                chord_since: None,
                last_phys_motion: None,
                enc: Vec::with_capacity(512),
                dgram: Vec::with_capacity(600),
                sink: SinkState::default(),
                links: vec![],
            }),
            capture: OnceLock::new(),
            inj: Mutex::new(inj),
            screens: RwLock::new(screens),
            monitor,
            tx,
            stats: Mutex::new(Stats::default()),
            started: Instant::now(),
            discovered: Mutex::new(HashMap::new()),
            dialing: Mutex::new(HashMap::new()),
            clip: OnceLock::new(),
            shutting_down: AtomicBool::new(false),
            captured: AtomicU64::new(0),
            dead_check: Mutex::new((Instant::now(), 0, 0)),
            seq_drop: AtomicU64::new(0),
        });
        e.refresh_links();

        // Monitor fallback through a peer.
        {
            let w = Arc::downgrade(&e);
            e.monitor.set_remote_fallback(Some(Box::new(move |input| {
                let Some(e) = w.upgrade() else { return false };
                let peers = e.peers.read().unwrap().clone();
                if let Some(p) = peers.iter().find(|p| p.trusted.load(Ordering::Relaxed) && p.alive.load(Ordering::Relaxed)) {
                    p.send(Ctrl::MonitorSet { input, req: 0 });
                    return true;
                }
                false
            })));
        }

        // Tell peers whenever we switch the shared monitor, so edge conditions
        // ("only while it shows the other computer") are correct instantly.
        {
            let w = Arc::downgrade(&e);
            e.monitor.set_on_switched(Some(Box::new(move |input| {
                if let Some(e) = w.upgrade() {
                    for p in e.trusted_peers() {
                        std::thread::spawn(move || p.send(Ctrl::MonitorShows { input }));
                    }
                }
            })));
        }

        // Monitor switched with its own buttons -> follow it.
        if e.cfg.read().unwrap().monitor.follow_manual_switch {
            let w = Arc::downgrade(&e);
            e.monitor.set_on_manual(Some(Box::new(move |input| {
                if let Some(e) = w.upgrade() {
                    let _ = e.tx.send(Ev::MonitorManual(input));
                }
            })));
        }

        // Capture
        let hk: Vec<Hotkey> = {
            let c = e.cfg.read().unwrap();
            [Hotkey::parse(HOTKEY_EMERGENCY, &c.hotkeys.emergency), Hotkey::parse(HOTKEY_TOGGLE, &c.hotkeys.toggle)].into_iter().flatten().collect()
        };
        e.st.lock().unwrap().hotkeys = hk.iter().filter_map(|h| crate::keymap::name_to_evdev(&h.key).map(|c| (h.id, h.clone(), c))).collect();
        match platform::start_capture(Arc::new(Handler(e.clone())), CaptureOptions { hotkeys: hk }) {
            Ok(c) => {
                let _ = e.capture.set(c);
            }
            Err(err) => log::error!("capture unavailable: {err} (this machine can receive input but not send it)"),
        }

        // Listener
        let l = TcpListener::bind(("0.0.0.0", e.cfg.read().unwrap().port))?;
        {
            let e = e.clone();
            std::thread::Builder::new().name("sb-listen".into()).spawn(move || {
                for s in l.incoming() {
                    match s {
                        Ok(s) => {
                            let e = e.clone();
                            std::thread::spawn(move || e.establish(s, false));
                        }
                        Err(err) => log::warn!("accept: {err}"),
                    }
                }
            })?;
        }
        // UDP receiver
        {
            let e = e.clone();
            std::thread::Builder::new().name("sb-udp".into()).spawn(move || e.udp_loop())?;
        }
        Ok((e, rx))
    }

    fn refresh_links(&self) {
        let screens = self.screens.read().unwrap().clone();
        let cfg = self.cfg.read().unwrap();
        let links = cfg.links.iter().filter_map(|l| geom::segment(&screens, l.edge, l.screen.as_deref()).map(|r| (l.clone(), r))).collect();
        self.st.lock().unwrap().links = links;
    }

    pub fn rescan_screens(&self) {
        let s = platform::screens();
        let changed = *self.screens.read().unwrap() != s;
        if changed {
            log::info!("display layout changed: {}", s.iter().map(|s| format!("{} {:?}", s.name, s.rect)).collect::<Vec<_>>().join(", "));
            *self.screens.write().unwrap() = s.clone();
            self.refresh_links();
            self.announce();
        }
    }

    fn hello(&self) -> Ctrl {
        let cfg = self.cfg.read().unwrap();
        let ms = self.monitor.status();
        Ctrl::Hello {
            version: PROTOCOL_VERSION,
            name: cfg.name.clone(),
            os: std::env::consts::OS.into(),
            udp_port: self.udp_port,
            udp_session: 0, // patched per peer
            screens: self.screens.read().unwrap().clone(),
            monitor_model: ms.model,
            monitor_input: ms.local_input,
            links: cfg.links.clone(),
        }
    }

    /// (Re)send our Hello to every paired peer (screens, links or monitor changed).
    fn announce(&self) {
        for p in self.trusted_peers() {
            let mut h = self.hello();
            if let Ctrl::Hello { udp_session, .. } = &mut h {
                *udp_session = p.recv_id;
            }
            std::thread::spawn(move || p.send(h));
        }
    }

    /// Model-name core of the shared monitor ("AW2726DM"), if one is configured.
    fn shared_core(&self) -> Option<String> {
        let m = self.monitor.status().model;
        (!m.is_empty()).then(|| model_core(&m))
    }

    /// Is the shared monitor currently showing another computer?
    fn shared_shows_other(&self) -> bool {
        let ms = self.monitor.status();
        matches!((ms.observed, ms.local_input), (Some(o), Some(l)) if o & 0xff != l & 0xff)
    }

    /// Screens to test an edge against: for conditional links while the shared
    /// monitor shows someone else, its (invisible) screen does not block.
    fn edge_screens(&self, link: &Link, all: &[ScreenInfo], shows_other: bool, core: &Option<String>) -> Option<Vec<ScreenInfo>> {
        if link.when.as_deref() != Some("shared_shows_other") {
            return Some(all.to_vec());
        }
        if !shows_other {
            return None;
        }
        Some(all.iter().filter(|s| core.as_ref().is_none_or(|c| !s.name.contains(c.as_str()))).cloned().collect())
    }

    // ------------------------------------------------------------ connections

    pub fn dial(self: &Arc<Self>, addr: SocketAddr) {
        let e = self.clone();
        std::thread::spawn(move || match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            Ok(s) => e.establish(s, true),
            Err(err) => log::debug!("dial {addr}: {err}"),
        });
    }

    fn establish(self: &Arc<Self>, s: TcpStream, initiator: bool) {
        let addr = s.peer_addr().ok();
        // Never expose the service beyond the local network.
        if let Some(a) = addr {
            if !is_local_net(a.ip()) {
                log::warn!("rejecting non-local connection from {a}");
                return;
            }
        }
        let sess = match Session::handshake(s, initiator, &self.id) {
            Ok(x) => x,
            Err(err) => {
                log::warn!("handshake with {addr:?} failed: {err}");
                return;
            }
        };
        let key_hex = net::hex(&sess.remote_static);
        if sess.remote_static == self.id.public {
            log::debug!("connected to self; dropping");
            return;
        }
        let trusted = self.cfg.read().unwrap().peer_by_key(&key_hex).is_some();
        let peer = Arc::new(Peer {
            fp: net::fingerprint(&sess.remote_static),
            sess,
            key_hex,
            recv_id: net::random_u32(),
            info: Mutex::new(PeerInfo::default()),
            trusted: AtomicBool::new(trusted),
            pair_local: AtomicBool::new(false),
            pair_remote: AtomicBool::new(false),
            last_rx: Mutex::new(Instant::now()),
            alive: AtomicBool::new(true),
            connected_at: Instant::now(),
        });
        if let Some(n) = self.cfg.read().unwrap().peer_by_key(&peer.key_hex) {
            peer.info.lock().unwrap().name = n.name.clone();
        }
        // De-duplicate simultaneous connections deterministically: keep the
        // session initiated by the side with the smaller fingerprint.
        {
            let mut peers = self.peers.write().unwrap();
            if let Some(i) = peers.iter().position(|p| p.key_hex == peer.key_hex && p.alive.load(Ordering::Relaxed)) {
                let preferred_initiator_is_me = self.fp < peer.fp;
                let new_ok = peer.sess.initiator == preferred_initiator_is_me;
                let old_ok = peers[i].sess.initiator == preferred_initiator_is_me;
                if old_ok || !new_ok {
                    log::debug!("duplicate connection to {}; keeping existing", peer.fp);
                    peer.sess.shutdown();
                    return;
                }
                peers[i].alive.store(false, Ordering::Relaxed);
                peers[i].sess.shutdown();
                peers.remove(i);
            }
            peers.retain(|p| p.alive.load(Ordering::Relaxed));
            peers.push(peer.clone());
        }
        log::info!("connected {} {} ({})", if trusted { "peer" } else { "UNPAIRED device" }, peer.fp, peer.sess.peer_addr);
        let mut h = self.hello();
        if let Ctrl::Hello { udp_session, .. } = &mut h {
            *udp_session = peer.recv_id;
        }
        peer.send(h);
        if !trusted {
            peer.send(Ctrl::PairPending);
        }
        let _ = self.tx.send(Ev::Connected);
        // Reader loop
        let tx = self.tx.clone();
        loop {
            match peer.sess.recv() {
                Ok(f) => {
                    *peer.last_rx.lock().unwrap() = Instant::now();
                    if tx.send(Ev::Frame(peer.clone(), f)).is_err() {
                        break;
                    }
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {
                    if !peer.alive.load(Ordering::Relaxed) || peer.last_rx.lock().unwrap().elapsed() > PEER_DEAD {
                        log::warn!("peer {} timed out", peer.fp);
                        break;
                    }
                }
                Err(e) => {
                    log::info!("peer {} disconnected: {e}", peer.fp);
                    break;
                }
            }
        }
        peer.alive.store(false, Ordering::Relaxed);
        peer.sess.shutdown();
        let _ = tx.send(Ev::Closed(peer));
    }

    pub fn trusted_peers(&self) -> Vec<Arc<Peer>> {
        self.peers.read().unwrap().iter().filter(|p| p.trusted.load(Ordering::Relaxed) && p.alive.load(Ordering::Relaxed)).cloned().collect()
    }

    fn peer_by_name(&self, name: &str) -> Option<Arc<Peer>> {
        self.trusted_peers().into_iter().find(|p| p.name() == name || p.fp == name)
    }

    // ------------------------------------------------------------ source side

    pub fn on_capture(&self, ev: CapEvent, cursor: (i32, i32)) {
        let t0 = Instant::now();
        self.captured.fetch_add(1, Ordering::Relaxed);
        if let CapEvent::Hotkey { id } = ev {
            let _ = self.tx.send(Ev::Hotkey(id));
            return;
        }
        let mut st = self.st.lock().unwrap();
        let st = &mut *st;
        if let CapEvent::Key { code, down } = ev {
            self.on_key(st, code, down);
            return;
        }
        if matches!(st.mode, Mode::Local) && matches!(ev, CapEvent::Motion { .. }) && (st.focus.is_some() || !st.focus_announced) {
            // The user is using this machine's own pointer: claim focus so a
            // keyboard connected elsewhere types here.
            self.set_focus(st, None);
            st.focus_announced = true;
            for p in self.trusted_peers() {
                std::thread::spawn(move || p.send(Ctrl::Focus { owner: Focus::Me }));
            }
        }
        match ev {
            CapEvent::Button { button, down } => {
                if down {
                    st.phys_buttons |= button.bit();
                } else {
                    st.phys_buttons &= !button.bit();
                }
            }
            CapEvent::Motion { .. } => st.last_phys_motion = Some(Instant::now()),
            _ => {}
        }
        match st.mode.clone() {
            Mode::Local => {
                if let CapEvent::Motion { dx, dy } = ev {
                    if st.disabled || st.links.is_empty() {
                        return;
                    }
                    self.check_edges(st, dx, dy, cursor);
                }
            }
            Mode::Hosted { .. } => {}
            Mode::Remote { peer, .. } => {
                match ev {
                    CapEvent::Motion { dx, dy } => {
                        st.src.cum_dx = st.src.cum_dx.wrapping_add(dx);
                        st.src.cum_dy = st.src.cum_dy.wrapping_add(dy);
                    }
                    CapEvent::Button { button, down } => {
                        st.ev_seq += 1;
                        let e = ButtonEvent { seq: st.ev_seq, button: button as u8, down, cum_dx: st.src.cum_dx, cum_dy: st.src.cum_dy };
                        if st.src.events.len() == EVENT_RING {
                            st.src.events.remove(0);
                        }
                        st.src.events.push(e);
                        if down {
                            st.src.buttons |= button.bit();
                        } else {
                            st.src.buttons &= !button.bit();
                        }
                    }
                    CapEvent::Wheel { v, h } => {
                        st.src.cum_wheel_v = st.src.cum_wheel_v.wrapping_add(v);
                        st.src.cum_wheel_h = st.src.cum_wheel_h.wrapping_add(h);
                    }
                    CapEvent::Hotkey { .. } | CapEvent::Key { .. } => {}
                }
                self.send_input(st, &peer);
                // Emergency chord: left+right+middle held.
                let chord = Button::Left.bit() | Button::Right.bit() | Button::Middle.bit();
                let chord_ms = self.cfg.read().unwrap().switching.chord_release_ms;
                if chord_ms > 0 && st.phys_buttons & chord == chord {
                    let since = *st.chord_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_millis(chord_ms) {
                        st.chord_since = None;
                        self.end_remote(st, "emergency mouse chord", None);
                    }
                } else {
                    st.chord_since = None;
                }
            }
        }
        let mut s = self.stats.lock().unwrap();
        s.capture_rate.tick();
        s.capture_cost.add(t0.elapsed().as_micros() as u64);
    }

    fn check_edges(&self, st: &mut State, dx: i32, dy: i32, cursor: (i32, i32)) {
        let (resistance, deadzone, lock) = {
            let c = self.cfg.read().unwrap();
            (c.switching.edge_resistance, c.switching.corner_deadzone, c.switching.lock_while_button_held)
        };
        let screens = self.screens.read().unwrap().clone();
        let shows_other = self.shared_shows_other();
        let core = self.shared_core();
        let mut hit = None;
        for (i, (link, seg)) in st.links.iter().enumerate() {
            let Some(scr) = self.edge_screens(link, &screens, shows_other, &core) else {
                st.push.entry(i).or_default().feed(false, 0, 0);
                continue;
            };
            let pos = geom::on_edge(&scr, seg, link.edge, cursor.0, cursor.1, deadzone);
            let out = geom::outward(link.edge, dx, dy);
            let push = st.push.entry(i).or_default();
            if push.feed_dwell(pos.is_some(), out, resistance, Duration::from_millis(link.dwell_ms.unwrap_or(0))) {
                hit = Some((link.clone(), *seg, pos.unwrap()));
                break;
            }
        }
        drop(screens);
        let Some((link, seg, pos)) = hit else { return };
        if lock && st.phys_buttons != 0 {
            return;
        }
        #[cfg(windows)]
        if platform::os::cursor_clipped() {
            return; // fullscreen game or app has confined the cursor
        }
        let Some(peer) = self.peer_by_name(&link.peer) else { return };
        let to = Target { screen: link.to_screen.clone(), edge: link.to_edge, switch_monitor: link.switch_monitor };
        self.begin_remote(st, peer, link.edge, seg, pos, cursor, to, "edge");
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_remote(&self, st: &mut State, peer: Arc<Peer>, edge: Edge, seg: Rect, pos: f32, park: (i32, i32), to: Target, reason: &str) {
        if peer.info.lock().unwrap().udp_addr.is_none() {
            log::warn!("cannot switch to {}: no hello yet", peer.name());
            return;
        }
        let epoch = st.next_epoch;
        st.next_epoch = st.next_epoch.wrapping_add(1);
        st.src = InputState { epoch, ..Default::default() };
        st.ev_seq = 0;
        st.last_ack = Instant::now();
        st.mode = Mode::Remote { peer: peer.clone(), epoch, exit_edge: edge, exit_seg: seg };
        if let Some(c) = self.capture.get() {
            c.set_forwarding(true, park);
        }
        log::info!("→ {} ({reason}, epoch {epoch})", peer.name());
        {
            let mut s = self.stats.lock().unwrap();
            s.switches += 1;
            s.last_switch_reason = reason.into();
        }
        let switch_monitor = to.switch_monitor != Some(false);
        let _ = self.tx.send(Ev::SendEnter(peer.clone(), epoch, edge, pos, to));
        self.set_focus(st, Some(peer.clone()));
        if switch_monitor {
            self.switch_monitor_to(Some(&peer));
        }
        self.send_input(st, &peer);
    }

    /// Return pointer control to this machine.
    fn end_remote(&self, st: &mut State, reason: &str, leave: Option<(f32, Target)>) {
        let Mode::Remote { peer, epoch, exit_edge, exit_seg } = st.mode.clone() else { return };
        let switch_monitor = leave.as_ref().is_none_or(|(_, t)| t.switch_monitor != Some(false));
        st.mode = Mode::Local;
        st.push.values_mut().for_each(|p| p.disarm());
        for i in 0..st.links.len() {
            st.push.entry(i).or_default().disarm();
        }
        let target = leave.as_ref().map(|(pos, to)| {
            let screens = self.screens.read().unwrap();
            let seg = to.screen.as_deref().and_then(|n| geom::segment(&screens, exit_edge, Some(n))).unwrap_or(exit_seg);
            geom::entry_point(&seg, to.edge.unwrap_or(exit_edge), *pos)
        });
        if let Some(c) = self.capture.get() {
            c.set_forwarding(false, target.unwrap_or((0, 0)));
        }
        if let Some((x, y)) = target {
            let _ = self.inj.lock().unwrap().warp(x, y);
        }
        if leave.is_none() {
            // Tell the sink to release everything (best effort, also over UDP via buttons=0).
            st.src.buttons = 0;
            self.send_input(st, &peer);
            let p = peer.clone();
            std::thread::spawn(move || p.send(Ctrl::Revoke { epoch }));
        }
        self.set_focus(st, None);
        log::info!("← local ({reason})");
        {
            let mut s = self.stats.lock().unwrap();
            s.switches += 1;
            s.last_switch_reason = reason.into();
        }
        if switch_monitor {
            self.switch_monitor_to(None);
        }
    }

    fn send_input(&self, st: &mut State, peer: &Peer) {
        let (addr, send_id) = {
            let i = peer.info.lock().unwrap();
            match i.udp_addr {
                Some(a) => (a, i.send_id),
                None => return,
            }
        };
        st.src.t_us = now_us();
        st.src.encode(&mut st.enc);
        if peer.sess.seal_udp(send_id, &st.enc, &mut st.dgram).is_ok() {
            if self.test_drop() {
                st.last_send = Instant::now();
                return;
            }
            let _ = self.udp.send_to(&st.dgram, addr);
            st.sent += 1;
            st.last_send = Instant::now();
            self.stats.lock().unwrap().send_rate.tick();
        }
    }

    /// Test hook (SWITCHBOARD_TEST_DROP=n): deterministically drop every n-th datagram.
    fn test_drop(&self) -> bool {
        static N: OnceLock<u64> = OnceLock::new();
        let n = *N.get_or_init(|| std::env::var("SWITCHBOARD_TEST_DROP").ok().and_then(|v| v.parse().ok()).unwrap_or(0));
        n > 0 && self.seq_drop.fetch_add(1, Ordering::Relaxed) % n == 0
    }

    fn switch_monitor_to(&self, peer: Option<&Peer>) {
        if !self.cfg.read().unwrap().monitor.enabled {
            return;
        }
        let ms = self.monitor.status();
        if ms.model.is_empty() {
            return;
        }
        let target = match peer {
            None => ms.local_input,
            Some(p) => {
                let i = p.info.lock().unwrap();
                let same = !i.monitor_model.is_empty() && (i.monitor_model.contains(&ms.model) || ms.model.contains(&i.monitor_model) || model_core(&i.monitor_model) == model_core(&ms.model));
                if same {
                    i.monitor_input
                } else {
                    None
                }
            }
        };
        if let Some(t) = target {
            self.monitor.request(t);
        }
    }

    // ------------------------------------------------------------ keyboard

    /// Change who has the pointer; keyboard forwarding follows.
    fn set_focus(&self, st: &mut State, new: Option<Arc<Peer>>) {
        let same = match (&st.focus, &new) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        if same {
            return;
        }
        st.focus = new.clone();
        let follow = self.cfg.read().unwrap().keyboard.follow_pointer;
        // Release everything at the old target.
        if st.kb.target.is_some() {
            st.kb.keys = KeySet::default();
            st.kb.st.events.clear();
            self.send_keys(st);
            st.kb.target = None;
        }
        match new {
            Some(p) if !follow => {
                if let Some(c) = self.clip.get() {
                    c.on_focus(&p);
                }
            }
            Some(p) if p.trusted.load(Ordering::Relaxed) => {
                st.kb.target = Some(p.clone());
                st.kb.st = KeyState { epoch: st.next_epoch, ..Default::default() };
                st.next_epoch = st.next_epoch.wrapping_add(1);
                st.kb.keys = KeySet::default();
                st.kb.swallowed = KeySet::default();
                st.kb.seq = 0;
                st.kb.acked = 0;
                if let Some(c) = self.capture.get() {
                    c.set_keyboard_forwarding(true);
                }
                log::info!("keyboard -> {}", p.name());
                if let Some(c) = self.clip.get() {
                    c.on_focus(&p);
                }
            }
            _ => {
                if let Some(c) = self.capture.get() {
                    c.set_keyboard_forwarding(false);
                }
            }
        }
    }

    fn on_key(&self, st: &mut State, code: u16, down: bool) {
        if st.kb.target.is_none() {
            return;
        }
        // Local hotkeys still work while the keyboard is diverted.
        if down {
            let m = st.kb.keys;
            let hit = st.hotkeys.iter().find(|(_, h, c)| *c == code && h.ctrl == m.ctrl() && h.alt == m.alt() && h.shift == m.shift() && h.meta == m.meta()).map(|(id, _, _)| *id);
            if let Some(id) = hit {
                st.kb.swallowed.set(code, true);
                let _ = self.tx.send(Ev::Hotkey(id));
                return;
            }
        } else if st.kb.swallowed.get(code) {
            st.kb.swallowed.set(code, false);
            return;
        }
        if down && st.kb.keys.get(code) {
            // Autorepeat from the source OS: the target generates its own repeats.
            return;
        }
        st.kb.keys.set(code, down);
        st.kb.seq += 1;
        let seq = st.kb.seq;
        if st.kb.st.events.len() == KEY_RING {
            st.kb.st.events.remove(0);
        }
        st.kb.st.events.push(KeyEvent { seq, code, down });
        self.send_keys(st);
    }

    fn send_keys(&self, st: &mut State) {
        let Some(peer) = st.kb.target.clone() else { return };
        let (addr, send_id) = {
            let i = peer.info.lock().unwrap();
            match i.udp_addr {
                Some(a) => (a, i.send_id),
                None => return,
            }
        };
        st.kb.st.pressed = st.kb.keys.0;
        let kb = &mut st.kb;
        kb.st.encode(&mut kb.enc);
        if peer.sess.seal_udp(send_id, &kb.enc, &mut kb.dgram).is_ok() {
            kb.last_send = Some(Instant::now());
            if !self.test_drop() {
                let _ = self.udp.send_to(&kb.dgram, addr);
            }
        }
    }

    fn on_keys(&self, peer: &Arc<Peer>, k: KeyState) {
        let mut st = self.st.lock().unwrap();
        let st = &mut *st;
        let same_src = st.ksink.from.as_ref().is_some_and(|f| Arc::ptr_eq(f, peer));
        if !same_src || st.ksink.epoch != k.epoch {
            if !same_src {
                self.release_keys(st);
            }
            st.ksink.from = Some(peer.clone());
            st.ksink.epoch = k.epoch;
            st.ksink.seq = 0;
        }
        st.ksink.last_rx = Some(Instant::now());
        {
            let mut inj = self.inj.lock().unwrap();
            let ks = &mut st.ksink;
            let seen = ks.seq;
            for e in k.events.iter().filter(|e| e.seq > seen) {
                if ks.pressed.get(e.code) != e.down {
                    let _ = inj.key(e.code, e.down);
                    ks.pressed.set(e.code, e.down);
                }
                ks.seq = e.seq;
            }
            let want = KeySet(k.pressed);
            if ks.pressed != want {
                for c in 0..256u16 {
                    if ks.pressed.get(c) != want.get(c) {
                        let _ = inj.key(c, want.get(c));
                    }
                }
                ks.pressed = want;
            }
            let _ = inj.flush();
        }
        let a = KeyAck { epoch: k.epoch, seq: st.ksink.seq };
        let (addr, send_id) = {
            let i = peer.info.lock().unwrap();
            match i.udp_addr {
                Some(x) => (x, i.send_id),
                None => return,
            }
        };
        let mut p = Vec::with_capacity(16);
        a.encode(&mut p);
        let mut d = Vec::with_capacity(48);
        if peer.sess.seal_udp(send_id, &p, &mut d).is_ok() {
            let _ = self.udp.send_to(&d, addr);
        }
    }

    fn release_keys(&self, st: &mut State) {
        let pressed = std::mem::take(&mut st.ksink.pressed);
        if !pressed.is_empty() {
            let mut inj = self.inj.lock().unwrap();
            for c in pressed.iter() {
                let _ = inj.key(c, false);
            }
            let _ = inj.flush();
        }
    }

    // ------------------------------------------------------------ UDP

    fn udp_loop(self: Arc<Self>) {
        let mut buf = vec![0u8; 2048];
        let mut plain = Vec::with_capacity(2048);
        loop {
            let (n, from) = match self.udp.recv_from(&mut buf) {
                Ok(x) => x,
                Err(e) => {
                    // Windows reports ICMP port-unreachable as ConnectionReset on UDP sockets.
                    if e.kind() != std::io::ErrorKind::ConnectionReset {
                        log::debug!("udp recv: {e}");
                    }
                    continue;
                }
            };
            self.on_datagram(&buf[..n], from, &mut plain);
        }
    }

    fn on_datagram(&self, d: &[u8], from: SocketAddr, plain: &mut Vec<u8>) {
        let rx_at = Instant::now();
        let Some(sid) = net::udp_session_id(d) else { return };
        let peer = self.peers.read().unwrap().iter().find(|p| p.recv_id == sid).cloned();
        let Some(peer) = peer else { return };
        let Some(seq) = peer.sess.open_udp(d, plain) else {
            self.stats.lock().unwrap().udp_bad += 1;
            return;
        };
        self.stats.lock().unwrap().udp_rx += 1;
        if !peer.trusted.load(Ordering::Relaxed) {
            return;
        }
        // Roaming: authenticated datagram from a new address (DHCP renewal etc).
        {
            let mut i = peer.info.lock().unwrap();
            if let Some(a) = i.udp_addr {
                if a.ip() != from.ip() {
                    log::info!("peer {} moved to {}", peer.fp, from.ip());
                    i.udp_addr = Some(SocketAddr::new(from.ip(), a.port()));
                }
            }
        }
        match plain.first() {
            Some(1) => {
                if let Some(s) = InputState::decode(plain) {
                    self.on_input(&peer, s, seq, rx_at);
                }
            }
            Some(2) => {
                if let Some(a) = InputAck::decode(plain) {
                    self.on_ack(&peer, a);
                }
            }
            Some(3) => {
                if let Some(k) = KeyState::decode(plain) {
                    self.on_keys(&peer, k);
                }
            }
            Some(4) => {
                if let Some(a) = KeyAck::decode(plain) {
                    let mut st = self.st.lock().unwrap();
                    if st.kb.target.as_ref().is_some_and(|t| Arc::ptr_eq(t, &peer)) && a.epoch == st.kb.st.epoch {
                        st.kb.acked = st.kb.acked.max(a.seq);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_ack(&self, peer: &Arc<Peer>, a: InputAck) {
        let rtt = now_us().saturating_sub(a.echo_t_us);
        peer.info.lock().unwrap().rtt.add(rtt);
        let mut st = self.st.lock().unwrap();
        if let Mode::Remote { peer: p, epoch, .. } = &st.mode {
            if Arc::ptr_eq(p, peer) && *epoch == a.epoch {
                st.last_ack = Instant::now();
            }
        }
        drop(st);
        self.stats.lock().unwrap().acks += 1;
    }

    fn on_input(&self, peer: &Arc<Peer>, s: InputState, seq: u64, rx_at: Instant) {
        let mut st = self.st.lock().unwrap();
        let st = &mut *st;
        let Mode::Hosted { peer: p, epoch } = &st.mode else { return };
        if !Arc::ptr_eq(p, peer) || *epoch != s.epoch {
            return;
        }
        let deadzone;
        let resistance;
        {
            let c = self.cfg.read().unwrap();
            deadzone = c.switching.corner_deadzone;
            resistance = c.switching.edge_resistance;
        }
        let sk = &mut st.sink;
        if sk.last_seq != 0 && seq > sk.last_seq + 1 {
            self.stats.lock().unwrap().sink_lost_seq += seq - sk.last_seq - 1;
        }
        sk.last_seq = seq;
        sk.last_echo = s.t_us;
        sk.last_rx = Some(rx_at);
        sk.received += 1;
        let t_inj = Instant::now();
        let mut inj = self.inj.lock().unwrap();
        let mut moved = (0, 0);
        // 1. Button transitions in order, each at its recorded motion position.
        let seen = sk.ev_seq;
        for e in s.events.iter().filter(|e| e.seq > seen) {
            let (dx, dy) = (e.cum_dx.wrapping_sub(sk.cum_dx), e.cum_dy.wrapping_sub(sk.cum_dy));
            if dx != 0 || dy != 0 {
                let _ = inj.motion(dx, dy);
                moved.0 += dx;
                moved.1 += dy;
            }
            sk.cum_dx = e.cum_dx;
            sk.cum_dy = e.cum_dy;
            if let Some(b) = Button::from_u8(e.button) {
                let _ = inj.button(b, e.down);
                if e.down {
                    sk.pressed |= b.bit();
                } else {
                    sk.pressed &= !b.bit();
                }
            }
            sk.ev_seq = e.seq;
        }
        // 2. Remaining motion and wheel.
        let (dx, dy) = (s.cum_dx.wrapping_sub(sk.cum_dx), s.cum_dy.wrapping_sub(sk.cum_dy));
        if dx != 0 || dy != 0 {
            let _ = inj.motion(dx, dy);
            moved.0 += dx;
            moved.1 += dy;
        }
        sk.cum_dx = s.cum_dx;
        sk.cum_dy = s.cum_dy;
        let (wv, wh) = (s.cum_wheel_v.wrapping_sub(sk.wv), s.cum_wheel_h.wrapping_sub(sk.wh));
        if wv != 0 || wh != 0 {
            let _ = inj.wheel(wv, wh);
        }
        sk.wv = s.cum_wheel_v;
        sk.wh = s.cum_wheel_h;
        // 3. Reconcile with authoritative button state (covers ring overflow).
        if sk.pressed != s.buttons {
            for b in Button::ALL {
                let want = s.buttons & b.bit() != 0;
                if (sk.pressed & b.bit() != 0) != want {
                    let _ = inj.button(b, want);
                }
            }
            sk.pressed = s.buttons;
        }
        let flush = inj.flush();
        if let Err(e) = flush {
            log::debug!("inject: {e}");
        }
        sk.inject.add(t_inj.elapsed().as_micros() as u64);
        // 4. Return edge?
        let mut leave = None;
        if moved != (0, 0) && !sk.returns.is_empty() {
            if let Some((x, y)) = inj.cursor_pos() {
                let screens = self.screens.read().unwrap().clone();
                let shows_other = self.shared_shows_other();
                let core = self.shared_core();
                if sk.push.len() != sk.returns.len() {
                    sk.push = (0..sk.returns.len()).map(|_| Push::default()).collect();
                }
                for (i, (edge, seg, to, dwell, link)) in sk.returns.iter().enumerate() {
                    let Some(scr) = self.edge_screens(link, &screens, shows_other, &core) else { continue };
                    let pos = geom::on_edge(&scr, seg, *edge, x, y, deadzone);
                    let out = geom::outward(*edge, moved.0, moved.1);
                    if sk.push[i].feed_dwell(pos.is_some(), out, resistance, Duration::from_millis(*dwell)) && sk.pressed == 0 {
                        leave = Some((*edge, pos.unwrap(), to.clone()));
                        break;
                    }
                }
            }
        }
        drop(inj);
        let epoch = s.epoch;
        let left = leave.is_some();
        if let Some((edge, pos, to)) = leave {
            st.mode = Mode::Local;
            self.set_focus(st, Some(peer.clone()));
            let _ = self.tx.send(Ev::SendLeave(peer.clone(), epoch, edge, pos, to));
        }
        // 5. Ack (throttled).
        let sk = &mut st.sink;
        if sk.last_ack.is_none_or(|t| t.elapsed() >= ACK_INTERVAL) || left {
            sk.last_ack = Some(Instant::now());
            let a = InputAck {
                epoch,
                seq,
                echo_t_us: sk.last_echo,
                hold_us: rx_at.elapsed().as_micros() as u32,
                inject_us: sk.inject.percentile(50.0) as u32,
                received: sk.received,
            };
            self.send_ack(peer, &a);
        }
    }

    fn send_ack(&self, peer: &Peer, a: &InputAck) {
        let (addr, send_id) = {
            let i = peer.info.lock().unwrap();
            match i.udp_addr {
                Some(x) => (x, i.send_id),
                None => return,
            }
        };
        let mut p = Vec::with_capacity(64);
        a.encode(&mut p);
        let mut d = Vec::with_capacity(96);
        if peer.sess.seal_udp(send_id, &p, &mut d).is_ok() {
            let _ = self.udp.send_to(&d, addr);
        }
    }

    fn release_all_injected(&self, st: &mut State) {
        let pressed = std::mem::take(&mut st.sink.pressed);
        if pressed != 0 {
            let mut inj = self.inj.lock().unwrap();
            for b in Button::ALL {
                if pressed & b.bit() != 0 {
                    let _ = inj.button(b, false);
                }
            }
            let _ = inj.flush();
            log::info!("released stuck buttons {pressed:#b}");
        }
    }

    // ------------------------------------------------------------ engine thread

    pub fn run(self: &Arc<Self>, rx: Receiver<Ev>) {
        let mut last_ping = Instant::now();
        let mut last_dial = Instant::now() - Duration::from_secs(10);
        let mut last_screen_scan = Instant::now();
        loop {
            // Fast ticks only while something time-critical is going on.
            let busy = {
                let st = self.st.lock().unwrap();
                !matches!(st.mode, Mode::Local) || st.kb.target.is_some() || !st.ksink.pressed.is_empty()
            };
            match rx.recv_timeout(Duration::from_millis(if busy { 10 } else { 100 })) {
                Ok(ev) => self.handle(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if self.shutting_down.load(Ordering::Relaxed) {
                return;
            }
            self.tick();
            if last_ping.elapsed() >= PING_INTERVAL {
                last_ping = Instant::now();
                for p in self.peers.read().unwrap().clone() {
                    if p.alive.load(Ordering::Relaxed) {
                        let p2 = p.clone();
                        std::thread::spawn(move || p2.send(Ctrl::Ping { t_us: now_us() }));
                        if !p.trusted.load(Ordering::Relaxed) && p.connected_at.elapsed() > PAIR_TIMEOUT {
                            log::info!("pairing with {} timed out", p.fp);
                            p.alive.store(false, Ordering::Relaxed);
                            p.sess.shutdown();
                        }
                    }
                }
            }
            if last_dial.elapsed() >= Duration::from_secs(2) {
                last_dial = Instant::now();
                self.reconnect();
            }
            if last_screen_scan.elapsed() >= Duration::from_secs(3) {
                last_screen_scan = Instant::now();
                self.rescan_screens();
            }
        }
    }

    fn tick(&self) {
        let timeout = Duration::from_millis(self.cfg.read().unwrap().switching.peer_timeout_ms);
        let mut st = self.st.lock().unwrap();
        let st = &mut *st;
        // Keyboard safety: if the target stops acknowledging keys, give the
        // keyboard back to this machine instead of typing into the void.
        if st.kb.target.is_some() && st.kb.acked < st.kb.seq && st.kb.last_send.is_some() {
            let first_unacked_age = st.kb.unacked_since.get_or_insert_with(Instant::now).elapsed();
            if first_unacked_age > Duration::from_millis(1200) {
                log::error!("keyboard target stopped acknowledging; keyboard returned to this computer");
                if matches!(st.mode, Mode::Remote { .. }) {
                    self.end_remote(st, "keyboard target unresponsive", None);
                } else {
                    self.set_focus(st, None);
                }
            }
        } else {
            st.kb.unacked_since = None;
        }
        // Keyboard: resend until acknowledged; keepalive while keys are held.
        if st.kb.target.is_some() {
            let since = st.kb.last_send.map(|t| t.elapsed()).unwrap_or(Duration::MAX);
            if (st.kb.acked < st.kb.seq && since >= Duration::from_millis(20)) || (!st.kb.keys.is_empty() && since >= Duration::from_millis(250)) {
                self.send_keys(st);
            }
        }
        if !st.ksink.pressed.is_empty() && st.ksink.last_rx.is_some_and(|t| t.elapsed() > SINK_IDLE_RELEASE) {
            log::info!("keyboard source went quiet; releasing held keys");
            self.release_keys(st);
        }
        match st.mode.clone() {
            Mode::Remote { peer, .. } => {
                let dead = {
                    let mut d = self.dead_check.lock().unwrap();
                    let (sw, cap) = (platform::os::swallowed_count(), self.captured.load(Ordering::Relaxed));
                    if d.0.elapsed() >= Duration::from_millis(400) {
                        let dead = sw.saturating_sub(d.1) > 40 && cap == d.2;
                        *d = (Instant::now(), sw, cap);
                        dead
                    } else {
                        false
                    }
                };
                if dead {
                    log::error!("FAIL-SAFE: input was being blocked locally but not forwarded; returning control");
                    self.end_remote(st, "fail-safe (capture stalled)", None);
                } else if platform::os::take_failsafe() {
                    log::error!("FAIL-SAFE: local mouse was being blocked but raw input was not arriving; returned control");
                    self.end_remote(st, "fail-safe (raw input missing)", None);
                } else if !peer.alive.load(Ordering::Relaxed) {
                    self.end_remote(st, "peer disconnected", None);
                } else if st.last_ack.elapsed() > timeout {
                    self.end_remote(st, "peer stopped responding", None);
                } else if st.last_send.elapsed() >= KEEPALIVE {
                    self.send_input(st, &peer);
                }
            }
            Mode::Hosted { peer, .. } => {
                if !peer.alive.load(Ordering::Relaxed) {
                    self.release_all_injected(st);
                    st.mode = Mode::Local;
                    log::info!("controller {} disconnected; local control", peer.fp);
                } else if st.sink.last_rx.is_some_and(|t| t.elapsed() > SINK_IDLE_RELEASE) {
                    self.release_all_injected(st);
                }
            }
            Mode::Local => {}
        }
    }

    fn reconnect(self: &Arc<Self>) {
        let cfg = self.cfg.read().unwrap().clone();
        let connected: Vec<String> = self.peers.read().unwrap().iter().filter(|p| p.alive.load(Ordering::Relaxed)).map(|p| p.key_hex.clone()).collect();
        let disc = self.discovered.lock().unwrap().clone();
        for pc in &cfg.peers {
            if connected.contains(&pc.key) {
                continue;
            }
            let mut addrs: Vec<SocketAddr> = vec![];
            if let Some((a, _, seen)) = disc.get(&pc.key) {
                if seen.elapsed() < Duration::from_secs(30) {
                    addrs.push(*a);
                }
            }
            if let Some(a) = &pc.address {
                if let Some(sa) = parse_addr(a, cfg.port) {
                    if !addrs.contains(&sa) {
                        addrs.push(sa);
                    }
                }
            }
            for a in addrs {
                let k = format!("{}@{}", pc.key, a);
                let mut d = self.dialing.lock().unwrap();
                if d.get(&k).is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    continue;
                }
                d.insert(k, Instant::now());
                drop(d);
                self.dial(a);
            }
        }
    }

    fn handle(self: &Arc<Self>, ev: Ev) {
        match ev {
            Ev::Connected => {}
            Ev::Closed(p) => {
                self.peers.write().unwrap().retain(|q| !Arc::ptr_eq(q, &p));
                {
                    let mut st = self.st.lock().unwrap();
                    if st.focus.as_ref().is_some_and(|f| Arc::ptr_eq(f, &p)) {
                        self.set_focus(&mut st, None);
                    }
                    if st.ksink.from.as_ref().is_some_and(|f| Arc::ptr_eq(f, &p)) {
                        self.release_keys(&mut st);
                    }
                }
                self.tick();
            }
            Ev::SendEnter(p, epoch, edge, pos, to) => p.send(Ctrl::Enter { epoch, edge, pos, to_screen: to.screen, to_edge: to.edge }),
            Ev::SendLeave(p, epoch, edge, pos, to) => {
                {
                    let mut st = self.st.lock().unwrap();
                    self.release_all_injected(&mut st);
                }
                p.send(Ctrl::Leave { epoch, edge, pos, to_screen: to.screen, to_edge: to.edge, switch_monitor: to.switch_monitor });
                log::info!("pointer returned to {}", p.name());
            }
            Ev::Hotkey(id) => self.on_hotkey(id),
            Ev::MonitorManual(input) => self.on_monitor_manual(input),
            Ev::Ipc(req, reply) => {
                let r = crate::ipc::handle(self, req);
                let _ = reply.send(r);
            }
            Ev::Discovered { key_hex, addr, name } => {
                let mut d = self.discovered.lock().unwrap();
                let new = !d.contains_key(&key_hex);
                d.insert(key_hex.clone(), (addr, name.clone(), Instant::now()));
                drop(d);
                if new {
                    log::info!("discovered {name} at {addr}");
                    if self.cfg.read().unwrap().peer_by_key(&key_hex).is_some() {
                        self.reconnect();
                    }
                }
            }
            Ev::Frame(p, f) => match f {
                Frame::Ctrl(m) => self.on_ctrl(&p, m),
                Frame::Data { id, last, bytes } => {
                    if p.trusted.load(Ordering::Relaxed) {
                        if let Some(c) = self.clip.get() {
                            c.on_data(&p, id, last, bytes);
                        }
                    }
                }
            },
        }
    }

    /// The monitor now shows `input` and we did not switch it: move the
    /// pointer to the computer on screen.
    fn on_monitor_manual(self: &Arc<Self>, input: u16) {
        if !self.cfg.read().unwrap().monitor.follow_manual_switch {
            return;
        }
        let mine = self.monitor.status().local_input == Some(input);
        let shown_peer = self.trusted_peers().into_iter().find(|p| p.info.lock().unwrap().monitor_input == Some(input));
        let mut st = self.st.lock().unwrap();
        let st = &mut *st;
        if st.disabled {
            return;
        }
        match (mine, st.mode.clone(), shown_peer) {
            // The monitor now shows this computer.
            (true, Mode::Remote { .. }, _) => self.end_remote(st, "monitor button", Some((0.5, Target::default()))),
            (true, Mode::Local, _) => {
                if let Some(p) = st.focus.clone() {
                    std::thread::spawn(move || p.send(Ctrl::RequestFocus { target: Focus::Me }));
                }
            }
            // The monitor now shows a peer.
            (false, Mode::Local, Some(p)) => {
                let has_pointer = st.last_phys_motion.is_some_and(|t| t.elapsed() < Duration::from_secs(120));
                if has_pointer && st.focus.is_none() {
                    let screens = self.screens.read().unwrap().clone();
                    let (edge, seg) = st.links.iter().find(|(l, _)| l.peer == p.name()).map(|(l, r)| (l.edge, *r)).unwrap_or((Edge::Right, geom::desktop(&screens)));
                    let park = geom::entry_point(&seg, edge, 0.5);
                    self.begin_remote(st, p, edge, seg, 0.5, park, Target::default(), "monitor button");
                }
            }
            (false, Mode::Hosted { peer, .. }, Some(p)) if Arc::ptr_eq(&peer, &p) => {}
            (false, Mode::Hosted { peer, .. }, _) => {
                std::thread::spawn(move || peer.send(Ctrl::RequestFocus { target: Focus::You }));
            }
            _ => {}
        }
    }

    fn on_hotkey(self: &Arc<Self>, id: u32) {
        let mut st = self.st.lock().unwrap();
        let st = &mut *st;
        match id {
            HOTKEY_EMERGENCY => {
                match st.mode.clone() {
                    Mode::Remote { .. } => self.end_remote(st, "emergency hotkey", None),
                    Mode::Hosted { peer, .. } => {
                        self.release_all_injected(st);
                        st.mode = Mode::Local;
                        let p = peer.clone();
                        std::thread::spawn(move || p.send(Ctrl::RequestFocus { target: Focus::You }));
                    }
                    Mode::Local => {}
                }
                st.disabled = true;
                log::warn!("EMERGENCY: forwarding disabled. Press the toggle hotkey or use the app to re-enable.");
            }
            HOTKEY_TOGGLE => {
                if st.disabled {
                    st.disabled = false;
                    log::info!("forwarding re-enabled");
                }
                match st.mode.clone() {
                    Mode::Remote { .. } => self.end_remote(st, "hotkey", Some((0.5, Target::default()))),
                    Mode::Hosted { peer, .. } => {
                        let p = peer.clone();
                        std::thread::spawn(move || p.send(Ctrl::RequestFocus { target: Focus::You }));
                    }
                    Mode::Local => {
                        let Some(peer) = self.trusted_peers().into_iter().next() else { return };
                        let has_pointer = st.last_phys_motion.is_some_and(|t| t.elapsed() < Duration::from_secs(60));
                        if has_pointer {
                            let screens = self.screens.read().unwrap().clone();
                            let (edge, seg) = st.links.iter().find(|(l, _)| l.peer == peer.name()).map(|(l, r)| (l.edge, *r)).unwrap_or((Edge::Right, geom::desktop(&screens)));
                            let park = geom::entry_point(&seg, edge, 0.5);
                            self.begin_remote(st, peer, edge, seg, 0.5, park, Target::default(), "hotkey");
                        } else {
                            let p = peer.clone();
                            std::thread::spawn(move || p.send(Ctrl::RequestFocus { target: Focus::Me }));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn on_ctrl(self: &Arc<Self>, p: &Arc<Peer>, m: Ctrl) {
        let trusted = p.trusted.load(Ordering::Relaxed);
        match m {
            Ctrl::Hello { version, name, os, udp_port, udp_session, screens, monitor_model, monitor_input, links } => {
                if version != PROTOCOL_VERSION {
                    log::warn!("peer {} speaks protocol v{version}, we speak v{PROTOCOL_VERSION}", p.fp);
                }
                let mut i = p.info.lock().unwrap();
                if !trusted || i.name.is_empty() {
                    i.name = name;
                }
                i.os = os;
                i.udp_addr = Some(SocketAddr::new(p.sess.peer_addr.ip(), udp_port));
                i.send_id = udp_session;
                i.screens = screens;
                i.monitor_model = monitor_model;
                i.monitor_input = monitor_input;
                i.links = links;
                i.hello = true;
                log::info!("hello from {} ({}), monitor {:?} input {:?}", i.name, i.os, i.monitor_model, i.monitor_input.map(crate::config::input_name));
                drop(i);
                if trusted {
                    if let Some(c) = self.clip.get() {
                        c.on_peer_ready(p);
                    }
                }
            }
            Ctrl::Ping { t_us } => {
                let p = p.clone();
                std::thread::spawn(move || p.send(Ctrl::Pong { t_us }));
            }
            Ctrl::Pong { t_us } => {
                p.info.lock().unwrap().rtt.add(now_us().saturating_sub(t_us));
            }
            Ctrl::PairPending => {}
            Ctrl::PairConfirm { accept } => {
                if !accept {
                    log::info!("{} rejected pairing", p.fp);
                    p.alive.store(false, Ordering::Relaxed);
                    p.sess.shutdown();
                    return;
                }
                p.pair_remote.store(true, Ordering::Relaxed);
                self.maybe_finish_pairing(p);
            }
            Ctrl::Bye => {
                p.alive.store(false, Ordering::Relaxed);
                p.sess.shutdown();
            }
            _ if !trusted => log::warn!("ignoring message from unpaired device {}", p.fp),
            Ctrl::Enter { epoch, edge, pos, to_screen, to_edge } => {
                let mut st = self.st.lock().unwrap();
                let st = &mut *st;
                if st.disabled {
                    let p = p.clone();
                    std::thread::spawn(move || p.send(Ctrl::Leave { epoch, edge: edge.opposite(), pos, to_screen: None, to_edge: None, switch_monitor: Some(false) }));
                    return;
                }
                if let Mode::Remote { peer, .. } = &st.mode {
                    // Both sides crossed at once: the smaller fingerprint stays source.
                    if Arc::ptr_eq(peer, p) && self.fp < p.fp {
                        let p = p.clone();
                        std::thread::spawn(move || p.send(Ctrl::Leave { epoch, edge: edge.opposite(), pos, to_screen: None, to_edge: None, switch_monitor: Some(false) }));
                        return;
                    }
                    self.end_remote(st, "peer took control", None);
                }
                let screens = self.screens.read().unwrap().clone();
                let returns: Vec<(Edge, Rect, Target, u64, Link)> = st.links.iter().filter(|(l, _)| l.peer == p.name()).map(|(l, r)| (l.edge, *r, Target { screen: l.to_screen.clone(), edge: l.to_edge, switch_monitor: l.switch_monitor }, l.dwell_ms.unwrap_or(0), l.clone())).collect();
                // Landing place: explicit target from the source, else our first link to it, else the opposite edge of the desktop.
                let land_edge = to_edge.unwrap_or_else(|| returns.first().map(|r| r.0).unwrap_or(edge.opposite()));
                let land_seg = to_screen.as_deref().and_then(|n| geom::segment(&screens, land_edge, Some(n))).or_else(|| returns.first().map(|r| r.1)).unwrap_or_else(|| geom::desktop(&screens));
                let returns = if returns.is_empty() {
                    let l = Link { edge: land_edge, peer: p.name(), screen: None, to_screen: None, to_edge: None, dwell_ms: None, switch_monitor: None, when: None };
                    vec![(land_edge, land_seg, Target::default(), 0, l)]
                } else {
                    returns
                };
                let push = returns.iter().map(|_| { let mut p = Push::default(); p.disarm(); p }).collect();
                st.sink = SinkState { returns, push, ..Default::default() };
                st.mode = Mode::Hosted { peer: p.clone(), epoch };
                self.set_focus(st, None);
                let (x, y) = geom::entry_point(&land_seg, land_edge, pos);
                let _ = self.inj.lock().unwrap().warp(x, y);
                let ret_edge = land_edge;
                log::info!("pointer from {} entered at ({x},{y}) via {ret_edge:?}", p.name());
                self.stats.lock().unwrap().switches += 1;
            }
            Ctrl::Leave { epoch, edge: _, pos, to_screen, to_edge, switch_monitor } => {
                let mut st = self.st.lock().unwrap();
                if let Mode::Remote { peer, epoch: e, .. } = &st.mode {
                    if Arc::ptr_eq(peer, p) && *e == epoch {
                        self.end_remote(&mut st, "edge", Some((pos, Target { screen: to_screen, edge: to_edge, switch_monitor })));
                    }
                }
            }
            Ctrl::Revoke { epoch } => {
                let mut st = self.st.lock().unwrap();
                if let Mode::Hosted { peer, epoch: e } = &st.mode {
                    if Arc::ptr_eq(peer, p) && *e == epoch {
                        self.release_all_injected(&mut st);
                        st.mode = Mode::Local;
                        self.set_focus(&mut st, Some(p.clone()));
                        log::info!("{} took the pointer back", p.name());
                    }
                }
            }
            Ctrl::RequestFocus { target } => {
                let mut st = self.st.lock().unwrap();
                let st = &mut *st;
                match (target, st.mode.clone()) {
                    // Peer wants the pointer back from us (we control it): stop.
                    (Focus::You, Mode::Remote { peer, .. }) if Arc::ptr_eq(&peer, p) => self.end_remote(st, "peer requested", Some((0.5, Target::default()))),
                    // Peer wants our pointer on their machine.
                    (Focus::Me, Mode::Local) if !st.disabled => {
                        let screens = self.screens.read().unwrap().clone();
                        let (edge, seg) = st.links.iter().find(|(l, _)| l.peer == p.name()).map(|(l, r)| (l.edge, *r)).unwrap_or((Edge::Right, geom::desktop(&screens)));
                        let park = geom::entry_point(&seg, edge, 0.5);
                        self.begin_remote(st, p.clone(), edge, seg, 0.5, park, Target::default(), "peer hotkey");
                    }
                    _ => {}
                }
            }
            Ctrl::Focus { owner } => {
                let mut st = self.st.lock().unwrap();
                if matches!(st.mode, Mode::Local) {
                    match owner {
                        Focus::Me => self.set_focus(&mut st, Some(p.clone())),
                        Focus::You => self.set_focus(&mut st, None),
                    }
                }
            }
            Ctrl::MonitorShows { input } => {
                self.monitor.set_observed(input);
            }
            Ctrl::SetLinks { links } => {
                let me_for_peer = p.name();
                {
                    let mut cfg = self.cfg.write().unwrap();
                    cfg.links.retain(|l| l.peer != me_for_peer);
                    for mut l in links.into_iter().take(16) {
                        l.peer = me_for_peer.clone();
                        cfg.links.push(l);
                    }
                    if let Err(e) = cfg.save() {
                        log::error!("saving links from {}: {e}", me_for_peer);
                    }
                }
                self.refresh_links();
                self.announce();
                log::info!("layout updated from {}", me_for_peer);
            }
            Ctrl::MonitorSet { input, req } => {
                let model = self.monitor.status().model;
                let p = p.clone();
                std::thread::spawn(move || {
                    let r = crate::ddc::set_input(&model, input);
                    p.send(Ctrl::MonitorResult { req, ok: r.is_ok(), detail: r.err().map(|e| e.to_string()).unwrap_or_default() });
                });
            }
            Ctrl::MonitorResult { ok, detail, .. } => {
                if !ok {
                    log::warn!("peer could not switch monitor: {detail}");
                }
            }
            m @ (Ctrl::ClipOffer { .. } | Ctrl::ClipRequest { .. } | Ctrl::ClipData { .. } | Ctrl::ClipError { .. }) => {
                if let Some(c) = self.clip.get() {
                    c.on_ctrl(p, m);
                }
            }
        }
    }

    // ------------------------------------------------------------ pairing

    pub fn confirm_pairing(&self, fp: Option<&str>, accept: bool) -> Result<String, String> {
        let peers = self.peers.read().unwrap().clone();
        let cand: Vec<_> = peers.iter().filter(|p| !p.trusted.load(Ordering::Relaxed) && p.alive.load(Ordering::Relaxed) && fp.is_none_or(|f| p.fp == f || p.sess.sas.replace(' ', "") == f.replace(' ', ""))).collect();
        let p = match cand.len() {
            0 => return Err("no pending pairing".into()),
            1 => cand[0].clone(),
            _ => return Err("several pending pairings; specify the code".into()),
        };
        p.send(Ctrl::PairConfirm { accept });
        if !accept {
            p.alive.store(false, Ordering::Relaxed);
            p.sess.shutdown();
            return Ok("rejected".into());
        }
        p.pair_local.store(true, Ordering::Relaxed);
        self.maybe_finish_pairing(&p);
        Ok(if p.trusted.load(Ordering::Relaxed) { format!("paired with {}", p.name()) } else { "confirmed here; waiting for the other computer".into() })
    }

    fn maybe_finish_pairing(&self, p: &Arc<Peer>) {
        if !(p.pair_local.load(Ordering::Relaxed) && p.pair_remote.load(Ordering::Relaxed)) || p.trusted.load(Ordering::Relaxed) {
            return;
        }
        let name = p.name();
        {
            let mut cfg = self.cfg.write().unwrap();
            cfg.peers.retain(|q| q.key != p.key_hex);
            let address = Some(p.sess.peer_addr.ip().to_string());
            cfg.peers.push(PeerConfig { name: name.clone(), key: p.key_hex.clone(), address });
            if let Err(e) = cfg.save() {
                log::error!("saving config: {e}");
            }
        }
        p.trusted.store(true, Ordering::Relaxed);
        log::info!("paired with {name} ({})", p.fp);
        // Re-send hello now that we trust each other (name, udp etc. were already exchanged).
        let mut h = self.hello();
        if let Ctrl::Hello { udp_session, .. } = &mut h {
            *udp_session = p.recv_id;
        }
        p.send(h);
    }

    // ------------------------------------------------------------ status / IPC helpers

    pub fn status(&self) -> Value {
        let st = self.st.lock().unwrap();
        let stats = self.stats.lock().unwrap();
        let (cpu, mem) = crate::stats::process_stats();
        let peers: Vec<Value> = self
            .peers
            .read()
            .unwrap()
            .iter()
            .filter(|p| p.alive.load(Ordering::Relaxed))
            .map(|p| {
                let i = p.info.lock().unwrap();
                json!({
                    "name": if i.name.is_empty() { p.fp.clone() } else { i.name.clone() },
                    "fingerprint": p.fp,
                    "address": p.sess.peer_addr.to_string(),
                    "os": i.os,
                    "trusted": p.trusted.load(Ordering::Relaxed),
                    "pairing_code": (!p.trusted.load(Ordering::Relaxed)).then(|| p.sess.sas.clone()),
                    "confirmed_here": p.pair_local.load(Ordering::Relaxed),
                    "rtt_ms": { "p50": i.rtt.percentile(50.0) as f64 / 1000.0, "p95": i.rtt.percentile(95.0) as f64 / 1000.0, "p99": i.rtt.percentile(99.0) as f64 / 1000.0, "samples": i.rtt.len() },
                    "monitor_input": i.monitor_input.map(crate::config::input_name),
                    "links": i.links,
                    "screens": i.screens,
                    "connected_s": p.connected_at.elapsed().as_secs(),
                })
            })
            .collect();
        let disc: Vec<Value> = self.discovered.lock().unwrap().iter().filter(|(_, v)| v.2.elapsed() < Duration::from_secs(30)).map(|(k, v)| json!({"name": v.1, "address": v.0.to_string(), "fingerprint": net::unhex(k).map(|k| net::fingerprint(&k)), "paired": self.cfg.read().unwrap().peer_by_key(k).is_some()})).collect();
        let cfg = self.cfg.read().unwrap();
        json!({
            "name": cfg.name,
            "fingerprint": self.fp,
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "mode": st.mode.label(),
            "focus": st.focus.as_ref().map(|p| p.name()),
            "keyboard_to": st.kb.target.as_ref().map(|p| p.name()),
            "disabled": st.disabled,
            "uptime_s": self.started.elapsed().as_secs(),
            "screens": *self.screens.read().unwrap(),
            "links": cfg.links,
            "peers": peers,
            "discovered": disc,
            "monitor": self.monitor.status(),
            "transfers": self.clip.get().map(|c| c.transfers()).unwrap_or_default(),
            "capture": self.capture.get().is_some(),
            "injector": self.inj.lock().unwrap().name(),
            "stats": {
                "capture_events_per_s": stats.capture_rate.rate(),
                "datagrams_sent_per_s": stats.send_rate.rate(),
                "capture_handler_us_p50": stats.capture_cost.percentile(50.0),
                "capture_handler_us_p99": stats.capture_cost.percentile(99.0),
                "sink_inject_us_p50": st.sink.inject.percentile(50.0),
                "sink_inject_us_p99": st.sink.inject.percentile(99.0),
                "sink_received": st.sink.received,
                "sink_gaps": stats.sink_lost_seq,
                "udp_rx": stats.udp_rx,
                "udp_rejected": stats.udp_bad,
                "acks": stats.acks,
                "switches": stats.switches,
                "last_switch_reason": stats.last_switch_reason,
                "hook_swallowed": hook_counters().0,
                "raw_while_forwarding": hook_counters().1,
                "hook_callback_max_us": hook_counters().2,
                "cpu_s": cpu,
                "rss_bytes": mem,
            }
        })
    }

    pub fn set_disabled(&self, d: bool) {
        let mut st = self.st.lock().unwrap();
        if d {
            if matches!(st.mode, Mode::Remote { .. }) {
                self.end_remote(&mut st, "disabled", None);
            }
        }
        st.disabled = d;
    }

    pub fn switch_to(self: &Arc<Self>, target: &str) -> Result<String, String> {
        let mut st = self.st.lock().unwrap();
        let st = &mut *st;
        if target == "local" || target == self.cfg.read().unwrap().name {
            match st.mode.clone() {
                Mode::Remote { .. } => self.end_remote(st, "manual", Some((0.5, Target::default()))),
                Mode::Hosted { peer, .. } => {
                    let p = peer.clone();
                    std::thread::spawn(move || p.send(Ctrl::RequestFocus { target: Focus::You }));
                }
                Mode::Local => {}
            }
            return Ok("local".into());
        }
        let peer = self.peer_by_name(target).ok_or_else(|| format!("no connected peer named {target:?}"))?;
        if let Mode::Remote { .. } = st.mode {
            self.end_remote(st, "manual", None);
        }
        let screens = self.screens.read().unwrap().clone();
        let (edge, seg) = st.links.iter().find(|(l, _)| l.peer == peer.name()).map(|(l, r)| (l.edge, *r)).unwrap_or((Edge::Right, geom::desktop(&screens)));
        let park = geom::entry_point(&seg, edge, 0.5);
        self.begin_remote(st, peer.clone(), edge, seg, 0.5, park, Target::default(), "manual");
        Ok(format!("controlling {}", peer.name()))
    }

    /// Save this computer's links and send each peer its own.
    pub fn apply_layout(&self, links: Vec<crate::config::Link>, peers: Vec<(String, Vec<crate::config::Link>)>) -> Result<(), String> {
        for (name, _) in &peers {
            if self.peer_by_name(name).is_none() {
                return Err(format!("{name} isn't connected; connect it before changing the arrangement"));
            }
        }
        {
            let mut cfg = self.cfg.write().unwrap();
            let peer_names: Vec<String> = peers.iter().map(|(n, _)| n.clone()).collect();
            cfg.links.retain(|l| !peer_names.contains(&l.peer));
            cfg.links.extend(links);
            cfg.save().map_err(|e| e.to_string())?;
        }
        self.refresh_links();
        self.announce();
        let my_name = self.cfg.read().unwrap().name.clone();
        for (name, ls) in peers {
            if let Some(p) = self.peer_by_name(&name) {
                let ls = ls.into_iter().map(|mut l| {
                    l.peer = my_name.clone();
                    l
                }).collect();
                std::thread::spawn(move || p.send(Ctrl::SetLinks { links: ls }));
            }
        }
        Ok(())
    }

    pub fn reload_config(&self) {
        let c = Config::load();
        *self.cfg.write().unwrap() = c;
        self.refresh_links();
        self.announce();
    }

    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        let mut st = self.st.lock().unwrap();
        self.set_focus(&mut st, None);
        self.release_keys(&mut st);
        match st.mode.clone() {
            Mode::Remote { .. } => self.end_remote(&mut st, "shutdown", None),
            Mode::Hosted { .. } => {
                self.release_all_injected(&mut st);
                st.mode = Mode::Local;
            }
            Mode::Local => {}
        }
        drop(st);
        for p in self.peers.read().unwrap().iter() {
            p.send(Ctrl::Bye);
        }
    }

    pub fn discovered_peer(&self, key_hex: String, addr: SocketAddr, name: String) {
        let _ = self.tx.send(Ev::Discovered { key_hex, addr, name });
    }
}

fn hook_counters() -> (u32, u32, u32) {
    #[cfg(windows)]
    {
        (platform::os::HOOK_SWALLOWED.load(Ordering::Relaxed), platform::os::RAW_WHILE_FORWARDING.load(Ordering::Relaxed), platform::os::HOOK_MAX_US.load(Ordering::Relaxed))
    }
    #[cfg(not(windows))]
    {
        (0, 0, 0)
    }
}

fn model_core(s: &str) -> String {
    // "Alienware_AW2726DM(DisplayPort)" / "AW2726DM" -> "AW2726DM"
    s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| w.len() >= 5 && w.chars().any(|c| c.is_ascii_digit())).last().unwrap_or(s).to_string()
}

pub fn parse_addr(a: &str, default_port: u16) -> Option<SocketAddr> {
    a.parse::<SocketAddr>().ok().or_else(|| a.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, default_port)))
}

/// Private/link-local/loopback only — the control service is never meant to
/// be reachable from the internet.
pub fn is_local_net(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.is_link_local(),
        IpAddr::V6(v) => v.is_loopback() || (v.segments()[0] & 0xffc0) == 0xfe80 || (v.segments()[0] & 0xfe00) == 0xfc00 || v.to_ipv4_mapped().is_some_and(|v4| v4.is_private() || v4.is_loopback()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_matching() {
        assert_eq!(model_core("Alienware_AW2726DM(DisplayPort)"), "AW2726DM");
        assert_eq!(model_core("AW2726DM"), "AW2726DM");
        assert!(is_local_net("192.168.1.20".parse().unwrap()));
        assert!(!is_local_net("8.8.8.8".parse().unwrap()));
    }
}
