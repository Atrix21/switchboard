//! Minimal logger: stderr + optional file, with timestamps relative to start.

use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;

struct Logger {
    start: Instant,
    file: Option<Mutex<std::fs::File>>,
    level: log::LevelFilter,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.level && m.target().starts_with("switchboard")
    }
    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let t = self.start.elapsed().as_secs_f64();
        let line = format!("{t:10.3} {:<5} {}\n", r.level(), r.args());
        let _ = std::io::stderr().write_all(line.as_bytes());
        if let Some(f) = &self.file {
            let _ = f.lock().unwrap().write_all(line.as_bytes());
        }
    }
    fn flush(&self) {}
}

pub fn init(to_file: bool) {
    let level = match std::env::var("SWITCHBOARD_LOG").as_deref() {
        Ok("debug") => log::LevelFilter::Debug,
        Ok("trace") => log::LevelFilter::Trace,
        Ok("warn") => log::LevelFilter::Warn,
        _ => log::LevelFilter::Info,
    };
    let file = if to_file {
        let p = crate::config::log_path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        // Keep one previous log.
        if std::fs::metadata(&p).map(|m| m.len() > 4 << 20).unwrap_or(false) {
            let _ = std::fs::rename(&p, p.with_extension("log.1"));
        }
        std::fs::OpenOptions::new().create(true).append(true).open(p).ok().map(Mutex::new)
    } else {
        None
    };
    let l = Box::leak(Box::new(Logger { start: Instant::now(), file, level }));
    let _ = log::set_logger(l);
    log::set_max_level(level);
}
