//! Monitor input selection (MCCS VCP 0x60) – platform-independent facade and
//! the switching coordinator.
//!
//! Switching is slow (tens of ms for the DDC write, then 1–3 s for the monitor
//! to re-sync), so it runs on its own thread and never blocks input. Requests
//! are coalesced: only the most recent desired input is applied.

use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct DdcMonitor {
    pub id: String,
    pub model: String,
    /// "DP" / "HDMI" if known – used to guess this machine's input value.
    pub connector: Option<String>,
    pub input: Option<u16>,
    pub ddc_ok: bool,
}

pub fn list() -> Vec<DdcMonitor> {
    crate::platform::os::ddc::list()
}

pub fn get_input(model: &str) -> io::Result<u16> {
    crate::platform::os::ddc::get_input(model)
}

pub fn set_input(model: &str, input: u16) -> io::Result<()> {
    crate::platform::os::ddc::set_input(model, input)
}

/// MCCS default input value for a connector type.
pub fn guess_input(connector: &str) -> Option<u16> {
    let c = connector.to_ascii_uppercase();
    if c.starts_with("DP") || c.contains("DISPLAYPORT") {
        Some(0x0F)
    } else if c.starts_with("HDMI") {
        Some(0x11)
    } else if c.starts_with("DVI") {
        Some(0x03)
    } else {
        None
    }
}

/// Find the shared monitor: explicit model substring, else the first DDC-capable one.
pub fn detect(model_hint: &str) -> Option<DdcMonitor> {
    let all = list();
    if !model_hint.is_empty() {
        return all.into_iter().find(|m| m.model.contains(model_hint));
    }
    all.into_iter().find(|m| m.ddc_ok)
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct MonitorStatus {
    pub model: String,
    pub local_input: Option<u16>,
    pub last_target: Option<u16>,
    pub last_ok: Option<bool>,
    pub last_ms: Option<u64>,
    pub last_error: Option<String>,
    pub switches: u64,
    pub failures: u64,
    /// Last input observed by polling (VCP 0x60).
    pub observed: Option<u16>,
    pub manual_switches: u64,
}

type RemoteFn = Box<dyn Fn(u16) -> bool + Send>;
type ManualFn = Box<dyn Fn(u16) + Send>;

struct Inner {
    desired: Option<u16>,
    status: MonitorStatus,
    remote: Option<RemoteFn>,
    /// Called when the input changed without us (e.g. the monitor's own buttons).
    on_manual: Option<ManualFn>,
    on_switched: Option<ManualFn>,
    last_switch: Option<Instant>,
}

/// Background monitor-switching worker.
#[derive(Clone)]
pub struct Coordinator {
    inner: Arc<(Mutex<Inner>, Condvar)>,
}

impl Coordinator {
    pub fn new(model: String, local_input: Option<u16>) -> Coordinator {
        let c = Coordinator {
            inner: Arc::new((Mutex::new(Inner { desired: None, status: MonitorStatus { model, local_input, ..Default::default() }, remote: None, on_manual: None, on_switched: None, last_switch: None }), Condvar::new())),
        };
        let c2 = c.clone();
        std::thread::Builder::new().name("sb-monitor".into()).spawn(move || c2.run()).expect("spawn monitor thread");
        c
    }

    /// Fallback used when local DDC fails: ask a peer to switch.
    pub fn set_remote_fallback(&self, f: Option<RemoteFn>) {
        self.inner.0.lock().unwrap().remote = f;
    }

    /// Watching costs a DDC read every 1.5 s; only do it when someone listens.
    pub fn watching(&self) -> bool {
        self.inner.0.lock().unwrap().on_manual.is_some()
    }

    pub fn set_on_switched(&self, f: Option<ManualFn>) {
        self.inner.0.lock().unwrap().on_switched = f;
    }

    /// A peer switched the monitor: record it so the watcher does not mistake
    /// it for a manual change.
    pub fn set_observed(&self, input: u16) {
        let mut g = self.inner.0.lock().unwrap();
        g.status.observed = Some(input & 0xff);
        g.last_switch = Some(Instant::now());
    }

    pub fn set_on_manual(&self, f: Option<ManualFn>) {
        self.inner.0.lock().unwrap().on_manual = f;
    }

    pub fn request(&self, input: u16) {
        let (m, cv) = &*self.inner;
        m.lock().unwrap().desired = Some(input);
        cv.notify_one();
    }

    pub fn status(&self) -> MonitorStatus {
        self.inner.0.lock().unwrap().status.clone()
    }

    /// Poll the active input; report changes we did not make.
    fn watch(&self, model: &str) {
        let Ok(v) = get_input(model) else { return };
        let (m, _) = &*self.inner;
        let mut g = m.lock().unwrap();
        let prev = g.status.observed.replace(v);
        if prev.is_some() && prev != Some(v) && g.desired.is_none() {
            g.status.manual_switches += 1;
            log::info!("monitor input changed to {} outside Switchboard", crate::config::input_name(v));
            if let Some(f) = &g.on_manual {
                f(v);
            }
        }
    }

    fn run(&self) {
        let (m, cv) = &*self.inner;
        loop {
            let (target, model) = {
                let mut g = m.lock().unwrap();
                if g.desired.is_none() {
                    g = cv.wait_timeout(g, Duration::from_millis(1500)).unwrap().0;
                }
                match g.desired.take() {
                    Some(t) => (t, g.status.model.clone()),
                    None => {
                        let model = g.status.model.clone();
                        let quiet = g.last_switch.is_none_or(|t| t.elapsed() > Duration::from_secs(5));
                        drop(g);
                        if !model.is_empty() && quiet && self.watching() {
                            self.watch(&model);
                        }
                        continue;
                    }
                }
            };
            if model.is_empty() {
                continue;
            }
            if m.lock().unwrap().status.observed == Some(target) {
                continue; // already showing it
            }
            let t0 = Instant::now();
            let mut res = set_input(&model, target).map_err(|e| e.to_string());
            if res.is_err() {
                // One retry: DDC/CI on some monitors NAKs while busy.
                std::thread::sleep(Duration::from_millis(80));
                res = set_input(&model, target).map_err(|e| e.to_string());
            }
            if let Err(e) = &res {
                log::warn!("local DDC switch to 0x{target:02X} failed: {e}; trying peer");
                let remote_ok = m.lock().unwrap().remote.as_ref().map(|f| f(target)).unwrap_or(false);
                if remote_ok {
                    res = Ok(());
                }
            }
            let ms = t0.elapsed().as_millis() as u64;
            let mut g = m.lock().unwrap();
            g.last_switch = Some(Instant::now());
            if res.is_ok() {
                g.status.observed = Some(target);
                if let Some(f) = &g.on_switched {
                    f(target);
                }
            }
            g.status.last_target = Some(target);
            g.status.last_ms = Some(ms);
            g.status.last_ok = Some(res.is_ok());
            g.status.switches += 1;
            match res {
                Ok(()) => {
                    g.status.last_error = None;
                    log::info!("monitor -> {} in {ms} ms", crate::config::input_name(target));
                }
                Err(e) => {
                    g.status.failures += 1;
                    g.status.last_error = Some(e.clone());
                    log::error!("monitor switch to 0x{target:02X} failed: {e}");
                }
            }
        }
    }
}