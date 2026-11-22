//! Lightweight measurement primitives (no allocation on the hot path).

use std::time::Instant;

/// Ring buffer of the last N samples with percentile queries.
pub struct Histogram {
    buf: Vec<u64>,
    pos: usize,
    full: bool,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram { buf: vec![0; 1024], pos: 0, full: false }
    }
}

impl Histogram {
    pub fn add(&mut self, v: u64) {
        self.buf[self.pos] = v;
        self.pos += 1;
        if self.pos == self.buf.len() {
            self.pos = 0;
            self.full = true;
        }
    }
    pub fn len(&self) -> usize {
        if self.full {
            self.buf.len()
        } else {
            self.pos
        }
    }
    pub fn samples(&self) -> Vec<u64> {
        self.buf[..self.len()].to_vec()
    }
    pub fn percentile(&self, p: f64) -> u64 {
        let mut v = self.samples();
        percentile(&mut v, p)
    }
}

pub fn percentile(v: &mut [u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let i = ((p / 100.0) * (v.len() - 1) as f64).round() as usize;
    v[i.min(v.len() - 1)]
}

/// Mean absolute difference between consecutive samples (RFC 3550-style jitter, simplified).
pub fn jitter(v: &[u64]) -> f64 {
    if v.len() < 2 {
        return 0.0;
    }
    v.windows(2).map(|w| (w[1] as f64 - w[0] as f64).abs()).sum::<f64>() / (v.len() - 1) as f64
}

/// Events per second over a sliding one-second window.
pub struct RateMeter {
    window_start: Instant,
    count: u64,
    last_rate: f64,
}

impl Default for RateMeter {
    fn default() -> Self {
        RateMeter { window_start: Instant::now(), count: 0, last_rate: 0.0 }
    }
}

impl RateMeter {
    pub fn tick(&mut self) {
        self.count += 1;
        let e = self.window_start.elapsed().as_secs_f64();
        if e >= 1.0 {
            self.last_rate = self.count as f64 / e;
            self.count = 0;
            self.window_start = Instant::now();
        }
    }
    pub fn rate(&self) -> f64 {
        if self.window_start.elapsed().as_secs_f64() > 2.0 {
            0.0
        } else {
            self.last_rate
        }
    }
}

/// (total CPU seconds, resident bytes) for this process.
pub fn process_stats() -> (f64, u64) {
    #[cfg(windows)]
    {
        crate::platform::os::process_stats()
    }
    #[cfg(target_os = "linux")]
    {
        let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
        let after = stat.rsplit(')').next().unwrap_or("");
        let f: Vec<&str> = after.split_whitespace().collect();
        let cpu = f.get(11).and_then(|u| u.parse::<f64>().ok()).unwrap_or(0.0) + f.get(12).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let rss = std::fs::read_to_string("/proc/self/statm").ok().and_then(|s| s.split_whitespace().nth(1).and_then(|r| r.parse::<u64>().ok())).unwrap_or(0);
        (cpu / tck, rss * page)
    }
}
