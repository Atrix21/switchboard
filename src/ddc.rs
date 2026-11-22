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
}

type RemoteFn = Box<dyn Fn(u16) -> bool + Send>;

struct Inner {
    desired: Option<u16>,
    status: MonitorStatus,
    remote: Option<RemoteFn>,
}

/// Background monitor-switching worker.
#[derive(Clone)]
pub struct Coordinator {
    inner: Arc<(Mutex<Inner>, Condvar)>,
}

impl Coordinator {
    pub fn new(model: String, local_input: Option<u16>) -> Coordinator {
        let c = Coordinator {
            inner: Arc::new((Mutex::new(Inner { desired: None, status: MonitorStatus { model, local_input, ..Default::default() }, remote: None }), Condvar::new())),
        };
        let c2 = c.clone();
        std::thread::Builder::new().name("sb-monitor".into()).spawn(move || c2.run()).expect("spawn monitor thread");
        c
    }

    /// Fallback used when local DDC fails: ask a peer to switch.
    pub fn set_remote_fallback(&self, f: Option<RemoteFn>) {
        self.inner.0.lock().unwrap().remote = f;
    }

    pub fn request(&self, input: u16) {
        let (m, cv) = &*self.inner;
        m.lock().unwrap().desired = Some(input);
        cv.notify_one();
    }

    pub fn status(&self) -> MonitorStatus {
        self.inner.0.lock().unwrap().status.clone()
    }

    fn run(&self) {
        let (m, cv) = &*self.inner;
        loop {
            let (target, model) = {
                let mut g = m.lock().unwrap();
                while g.desired.is_none() {
                    g = cv.wait(g).unwrap();
                }
                (g.desired.take().unwrap(), g.status.model.clone())
            };
            if model.is_empty() {
                continue;
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
