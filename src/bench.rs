//! Benchmark harness.
//!
//! * `bench echo` – UDP + TCP echo responder (run on the other machine).
//! * `bench net <host>` – send timestamped probes at a mouse-like rate over UDP
//!   and/or TCP(NODELAY); report RTT percentiles, jitter, loss, reordering.
//! * `bench inject` – cost of one injection call on this OS.
//! * `bench crypto` – seal+open cost of one input datagram.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::stats::{jitter, percentile};

pub const BENCH_PORT: u16 = 47810;

pub fn echo(port: u16) -> std::io::Result<()> {
    let u = UdpSocket::bind(("0.0.0.0", port))?;
    let t = TcpListener::bind(("0.0.0.0", port))?;
    println!("echo responder on UDP+TCP {port} (Ctrl+C to stop)");
    std::thread::spawn(move || {
        let mut b = [0u8; 2048];
        loop {
            if let Ok((n, from)) = u.recv_from(&mut b) {
                if crate::engine::is_local_net(from.ip()) {
                    let _ = u.send_to(&b[..n], from);
                }
            }
        }
    });
    for s in t.incoming().flatten() {
        if !s.peer_addr().map(|a| crate::engine::is_local_net(a.ip())).unwrap_or(false) {
            continue;
        }
        std::thread::spawn(move || {
            let mut s = s;
            let _ = s.set_nodelay(true);
            let mut b = [0u8; 4096];
            while let Ok(n) = s.read(&mut b) {
                if n == 0 || s.write_all(&b[..n]).is_err() {
                    break;
                }
            }
        });
    }
    Ok(())
}

pub struct NetResult {
    pub proto: &'static str,
    pub sent: u64,
    pub recv: u64,
    pub reordered: u64,
    pub rtt_us: Vec<u64>,
}

impl NetResult {
    pub fn print(&self) {
        let mut v = self.rtt_us.clone();
        let loss = if self.sent > 0 { 100.0 * (self.sent - self.recv.min(self.sent)) as f64 / self.sent as f64 } else { 0.0 };
        let jit = jitter(&self.rtt_us);
        let ms = |x: u64| x as f64 / 1000.0;
        println!(
            "{:<4} sent={:<6} recv={:<6} loss={:>5.2}% reorder={:<4} RTT ms: min={:.2} p50={:.2} p90={:.2} p99={:.2} p99.9={:.2} max={:.2} jitter={:.2}",
            self.proto,
            self.sent,
            self.recv,
            loss,
            self.reordered,
            ms(*v.iter().min().unwrap_or(&0)),
            ms(percentile(&mut v, 50.0)),
            ms(percentile(&mut v, 90.0)),
            ms(percentile(&mut v, 99.0)),
            ms(percentile(&mut v, 99.9)),
            ms(*v.iter().max().unwrap_or(&0)),
            jit / 1000.0
        );
    }
}

fn pkt(seq: u64, t0: Instant, size: usize) -> Vec<u8> {
    let mut p = vec![0u8; size.max(16)];
    p[..8].copy_from_slice(&seq.to_le_bytes());
    p[8..16].copy_from_slice(&(t0.elapsed().as_micros() as u64).to_le_bytes());
    p
}

/// Busy-ish pacing at `rate` Hz without spinning a full core.
fn pace(next: &mut Instant, period: Duration) {
    *next += period;
    let now = Instant::now();
    if *next > now {
        let d = *next - now;
        if d > Duration::from_micros(1500) {
            std::thread::sleep(d - Duration::from_micros(1000));
        }
        while Instant::now() < *next {
            std::hint::spin_loop();
        }
    } else if now - *next > period * 10 {
        *next = now;
    }
}

pub fn net_udp(host: SocketAddr, rate: u32, secs: u32, size: usize) -> std::io::Result<NetResult> {
    let s = UdpSocket::bind("0.0.0.0:0")?;
    s.connect(host)?;
    s.set_read_timeout(Some(Duration::from_millis(200)))?;
    let r = s.try_clone()?;
    let t0 = Instant::now();
    let done = Arc::new(AtomicBool::new(false));
    let res = Arc::new(Mutex::new((0u64, 0u64, Vec::<u64>::new())));
    let rx = {
        let done = done.clone();
        let res = res.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; 2048];
            let mut high = 0u64;
            loop {
                match r.recv(&mut b) {
                    Ok(n) if n >= 16 => {
                        let seq = u64::from_le_bytes(b[..8].try_into().unwrap());
                        let ts = u64::from_le_bytes(b[8..16].try_into().unwrap());
                        let now = t0.elapsed().as_micros() as u64;
                        let mut g = res.lock().unwrap();
                        g.0 += 1;
                        if seq < high {
                            g.1 += 1;
                        }
                        high = high.max(seq);
                        g.2.push(now - ts);
                    }
                    _ => {
                        if done.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                }
            }
        })
    };
    let period = Duration::from_secs_f64(1.0 / rate as f64);
    let n = rate as u64 * secs as u64;
    let mut next = Instant::now();
    for seq in 1..=n {
        let _ = s.send(&pkt(seq, t0, size));
        pace(&mut next, period);
    }
    std::thread::sleep(Duration::from_millis(500));
    done.store(true, Ordering::Relaxed);
    let _ = rx.join();
    let g = res.lock().unwrap();
    Ok(NetResult { proto: "udp", sent: n, recv: g.0, reordered: g.1, rtt_us: g.2.clone() })
}

pub fn net_tcp(host: SocketAddr, rate: u32, secs: u32, size: usize) -> std::io::Result<NetResult> {
    let mut s = TcpStream::connect_timeout(&host, Duration::from_secs(3))?;
    s.set_nodelay(true)?;
    let mut r = s.try_clone()?;
    let size = size.max(16);
    let t0 = Instant::now();
    let n = rate as u64 * secs as u64;
    let rx = std::thread::spawn(move || {
        let mut v = Vec::with_capacity(n as usize);
        let mut b = vec![0u8; size];
        for _ in 0..n {
            if r.read_exact(&mut b).is_err() {
                break;
            }
            let ts = u64::from_le_bytes(b[8..16].try_into().unwrap());
            v.push(t0.elapsed().as_micros() as u64 - ts);
        }
        v
    });
    let period = Duration::from_secs_f64(1.0 / rate as f64);
    let mut next = Instant::now();
    for seq in 1..=n {
        s.write_all(&pkt(seq, t0, size))?;
        pace(&mut next, period);
    }
    let v = rx.join().unwrap_or_default();
    Ok(NetResult { proto: "tcp", sent: n, recv: v.len() as u64, reordered: 0, rtt_us: v })
}

pub fn inject(n: u32) -> std::io::Result<()> {
    let mut inj = crate::platform::new_injector()?;
    println!("injector: {}", inj.name());
    let mut v = Vec::with_capacity(n as usize);
    for i in 0..n {
        let d = if i % 2 == 0 { 1 } else { -1 };
        let t = Instant::now();
        inj.motion(d, 0)?;
        inj.flush()?;
        v.push(t.elapsed().as_nanos() as u64);
    }
    let mut q = Vec::with_capacity(200);
    for _ in 0..200 {
        let t = Instant::now();
        let _ = inj.cursor_pos();
        q.push(t.elapsed().as_nanos() as u64);
    }
    println!(
        "motion+flush µs: p50={:.1} p90={:.1} p99={:.1} max={:.1}   cursor_pos µs: p50={:.1} p99={:.1}",
        percentile(&mut v, 50.0) as f64 / 1000.0,
        percentile(&mut v, 90.0) as f64 / 1000.0,
        percentile(&mut v, 99.0) as f64 / 1000.0,
        *v.iter().max().unwrap() as f64 / 1000.0,
        percentile(&mut q, 50.0) as f64 / 1000.0,
        percentile(&mut q, 99.0) as f64 / 1000.0
    );
    Ok(())
}

pub fn crypto(n: u32) {
    use crate::proto::{ButtonEvent, InputState};
    let id_a = crate::net::Identity::load_or_create(&std::env::temp_dir().join("sb-bench-a.key")).unwrap();
    let id_b = crate::net::Identity::load_or_create(&std::env::temp_dir().join("sb-bench-b.key")).unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    let t = std::thread::spawn(move || crate::net::Session::handshake(l.accept().unwrap().0, false, &id_b).unwrap());
    let sa = crate::net::Session::handshake(TcpStream::connect(a).unwrap(), true, &id_a).unwrap();
    let sb = t.join().unwrap();
    let mut s = InputState { epoch: 1, ..Default::default() };
    for i in 0..4 {
        s.events.push(ButtonEvent { seq: i, button: 0, down: i % 2 == 0, cum_dx: 0, cum_dy: 0 });
    }
    let (mut enc, mut d, mut o) = (Vec::new(), Vec::new(), Vec::new());
    let mut v = Vec::with_capacity(n as usize);
    for i in 0..n {
        s.cum_dx = i as i32;
        let t = Instant::now();
        s.encode(&mut enc);
        sa.seal_udp(1, &enc, &mut d).unwrap();
        sb.open_udp(&d, &mut o).unwrap();
        InputState::decode(&o).unwrap();
        v.push(t.elapsed().as_nanos() as u64);
    }
    println!("datagram {} bytes; encode+seal+open+decode µs: p50={:.2} p99={:.2}", d.len(), percentile(&mut v, 50.0) as f64 / 1000.0, percentile(&mut v, 99.0) as f64 / 1000.0);
}
