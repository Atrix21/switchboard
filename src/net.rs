//! Identity, Noise handshake and encrypted framing for TCP (control) and UDP (input).
//!
//! * Handshake: `Noise_XX_25519_ChaChaPoly_BLAKE2s` over the TCP stream,
//!   2-byte big-endian length prefix per message. Mutual authentication via
//!   long-term X25519 static keys; trust is established once by pairing.
//! * After the handshake we use snow's *stateless* transport (explicit nonces)
//!   so that send and receive can happen from different threads and so the same
//!   per-direction keys can serve both channels with disjoint nonce spaces:
//!   TCP nonces count up from 0, UDP nonces have the top bit set. A nonce is
//!   never reused under a key.
//! * Pairing SAS: 6 digits derived from the Noise handshake hash, which binds
//!   both static and ephemeral keys. A man-in-the-middle cannot make both sides
//!   compute the same hash, so matching codes prove a direct connection.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use blake2::{Blake2s256, Digest};
use snow::{Builder, StatelessTransportState};

use crate::proto::{Frame, NOISE_PATTERN};

pub type Key = [u8; 32];

#[derive(Clone)]
pub struct Identity {
    pub private: Key,
    pub public: Key,
}

impl Identity {
    pub fn load_or_create(path: &Path) -> io::Result<Identity> {
        if let Ok(b) = std::fs::read(path) {
            if b.len() == 64 {
                return Ok(Identity { private: b[..32].try_into().unwrap(), public: b[32..].try_into().unwrap() });
            }
        }
        let kp = Builder::new(NOISE_PATTERN.parse().unwrap()).generate_keypair().map_err(io::Error::other)?;
        let id = Identity { private: kp.private.try_into().unwrap(), public: kp.public.try_into().unwrap() };
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut v = id.private.to_vec();
        v.extend_from_slice(&id.public);
        crate::config::write_private(path, &v)?;
        Ok(id)
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public)
    }
}

/// Short human-readable fingerprint of a public key, e.g. `K7QF-M2XA-93JD`.
pub fn fingerprint(pk: &Key) -> String {
    const A: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"; // no 0/O/1/I
    let h = Blake2s256::digest(pk);
    let mut bits: u64 = u64::from_le_bytes(h[..8].try_into().unwrap());
    let mut s = String::new();
    for i in 0..12 {
        if i > 0 && i % 4 == 0 {
            s.push('-');
        }
        s.push(A[(bits & 31) as usize] as char);
        bits >>= 5;
    }
    s
}

pub fn hex(k: &[u8]) -> String {
    k.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn unhex(s: &str) -> Option<Key> {
    if s.len() != 64 {
        return None;
    }
    let mut k = [0u8; 32];
    for i in 0..32 {
        k[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(k)
}

const UDP_NONCE_BIT: u64 = 1 << 63;
const MAX_FRAME: usize = 65535;
const TAG: usize = 16;
pub const UDP_MAGIC: u8 = 0x5B;
/// UDP datagram header: magic(1) + session id(4) + nonce(8).
pub const UDP_HDR: usize = 13;

/// An authenticated, encrypted session with a peer.
pub struct Session {
    pub remote_static: Key,
    pub sas: String,
    pub initiator: bool,
    transport: Arc<StatelessTransportState>,
    writer: Mutex<(TcpStream, u64)>,
    reader: Mutex<(TcpStream, u64)>,
    ctl: TcpStream,
    udp_send_nonce: AtomicU64,
    udp_recv_high: AtomicU64,
    pub peer_addr: SocketAddr,
}

fn write_msg(s: &mut TcpStream, b: &[u8]) -> io::Result<()> {
    let mut v = Vec::with_capacity(2 + b.len());
    v.extend_from_slice(&(b.len() as u16).to_be_bytes());
    v.extend_from_slice(b);
    s.write_all(&v)
}
fn read_msg(s: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<()> {
    let mut l = [0u8; 2];
    s.read_exact(&mut l)?;
    let n = u16::from_be_bytes(l) as usize;
    buf.resize(n, 0);
    s.read_exact(buf)
}

impl Session {
    /// Run the Noise XX handshake on a freshly connected stream.
    pub fn handshake(mut stream: TcpStream, initiator: bool, id: &Identity) -> io::Result<Session> {
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let b = Builder::new(NOISE_PATTERN.parse().unwrap()).local_private_key(&id.private);
        let mut hs = if initiator { b.build_initiator() } else { b.build_responder() }.map_err(io::Error::other)?;
        let mut buf = vec![0u8; MAX_FRAME];
        let mut msg = Vec::new();
        // XX: -> e ; <- e, ee, s, es ; -> s, se
        let mut my_turn = initiator;
        while !hs.is_handshake_finished() {
            if my_turn {
                let n = hs.write_message(&[], &mut buf).map_err(io::Error::other)?;
                write_msg(&mut stream, &buf[..n])?;
            } else {
                read_msg(&mut stream, &mut msg)?;
                hs.read_message(&msg, &mut buf).map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
            }
            my_turn = !my_turn;
        }
        let remote_static: Key = hs.get_remote_static().ok_or_else(|| io::Error::other("no remote static"))?.try_into().unwrap();
        let hh = hs.get_handshake_hash();
        let sas = sas_from_hash(hh);
        let transport = hs.into_stateless_transport_mode().map_err(io::Error::other)?;
        stream.set_read_timeout(Some(Duration::from_secs(4)))?;
        let peer_addr = stream.peer_addr()?;
        let r = stream.try_clone()?;
        let ctl = stream.try_clone()?;
        Ok(Session {
            remote_static,
            sas,
            initiator,
            transport: Arc::new(transport),
            writer: Mutex::new((stream, 0)),
            reader: Mutex::new((r, 0)),
            ctl,
            udp_send_nonce: AtomicU64::new(UDP_NONCE_BIT),
            udp_recv_high: AtomicU64::new(0),
            peer_addr,
        })
    }

    pub fn send(&self, f: &Frame) -> io::Result<()> {
        let plain = f.encode();
        let mut w = self.writer.lock().unwrap();
        for (i, chunk) in plain.chunks(MAX_FRAME - TAG - 1).enumerate() {
            // Continuation flag: first byte 1 = more chunks follow.
            let more = (i + 1) * (MAX_FRAME - TAG - 1) < plain.len();
            let mut p = Vec::with_capacity(chunk.len() + 1);
            p.push(more as u8);
            p.extend_from_slice(chunk);
            let mut out = vec![0u8; p.len() + TAG];
            let nonce = w.1;
            w.1 += 1;
            let n = self.transport.write_message(nonce, &p, &mut out).map_err(io::Error::other)?;
            write_msg(&mut w.0, &out[..n])?;
        }
        Ok(())
    }

    /// Blocking receive of one frame. Returns `TimedOut`/`WouldBlock` errors on
    /// read timeout (the caller decides about liveness).
    pub fn recv(&self) -> io::Result<Frame> {
        let mut r = self.reader.lock().unwrap();
        let mut whole = Vec::new();
        let mut msg = Vec::new();
        loop {
            read_msg(&mut r.0, &mut msg)?;
            let mut out = vec![0u8; msg.len()];
            let nonce = r.1;
            let n = self.transport.read_message(nonce, &msg, &mut out).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            r.1 += 1;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "empty frame"));
            }
            whole.extend_from_slice(&out[1..n]);
            if out[0] == 0 {
                break;
            }
            if whole.len() > 64 * 1024 * 1024 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
            }
        }
        Frame::decode(&whole).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad frame"))
    }

    pub fn shutdown(&self) {
        let _ = self.ctl.shutdown(std::net::Shutdown::Both);
    }

    /// Encrypt an input datagram. `session_id` is the *peer's* receive id.
    pub fn seal_udp(&self, session_id: u32, plain: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        let nonce = self.udp_send_nonce.fetch_add(1, Ordering::Relaxed);
        out.clear();
        out.push(UDP_MAGIC);
        out.extend_from_slice(&session_id.to_le_bytes());
        out.extend_from_slice(&nonce.to_le_bytes());
        out.resize(UDP_HDR + plain.len() + TAG, 0);
        let n = self.transport.write_message(nonce, plain, &mut out[UDP_HDR..]).map_err(io::Error::other)?;
        out.truncate(UDP_HDR + n);
        Ok(())
    }

    /// Decrypt a datagram (header already validated by caller). Rejects replays
    /// and stale datagrams: snapshots are cumulative, so anything older than the
    /// newest one already processed is useless. Returns the nonce (sequence).
    pub fn open_udp(&self, dgram: &[u8], out: &mut Vec<u8>) -> Option<u64> {
        if dgram.len() < UDP_HDR + TAG || dgram[0] != UDP_MAGIC {
            return None;
        }
        let nonce = u64::from_le_bytes(dgram[5..13].try_into().ok()?);
        if nonce & UDP_NONCE_BIT == 0 || nonce <= self.udp_recv_high.load(Ordering::Relaxed) {
            return None;
        }
        out.resize(dgram.len() - UDP_HDR, 0);
        let n = self.transport.read_message(nonce, &dgram[UDP_HDR..], out).ok()?;
        out.truncate(n);
        self.udp_recv_high.fetch_max(nonce, Ordering::Relaxed);
        Some(nonce & !UDP_NONCE_BIT)
    }
}

pub fn udp_session_id(dgram: &[u8]) -> Option<u32> {
    if dgram.len() < UDP_HDR || dgram[0] != UDP_MAGIC {
        return None;
    }
    Some(u32::from_le_bytes(dgram[1..5].try_into().ok()?))
}

fn sas_from_hash(h: &[u8]) -> String {
    let d = Blake2s256::new_with_prefix(b"switchboard-sas-v1").chain_update(h).finalize();
    let n = u32::from_le_bytes(d[..4].try_into().unwrap()) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

pub fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    getrandom::getrandom(&mut b).expect("rng");
    u32::from_le_bytes(b)
}

/// Bind the UDP input socket with sensible buffer sizes.
pub fn bind_udp(port: u16) -> io::Result<UdpSocket> {
    let s = UdpSocket::bind(("0.0.0.0", port))?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Ctrl;
    use std::net::TcpListener;

    fn ident() -> Identity {
        let kp = Builder::new(NOISE_PATTERN.parse().unwrap()).generate_keypair().unwrap();
        Identity { private: kp.private.try_into().unwrap(), public: kp.public.try_into().unwrap() }
    }

    #[test]
    fn handshake_frames_and_udp() {
        let (a, b) = (ident(), ident());
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let b2 = b.clone();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            Session::handshake(s, false, &b2).unwrap()
        });
        let sa = Session::handshake(TcpStream::connect(addr).unwrap(), true, &a).unwrap();
        let sb = t.join().unwrap();
        assert_eq!(sa.sas, sb.sas);
        assert_eq!(sa.remote_static, b.public);
        assert_eq!(sb.remote_static, a.public);

        let big = vec![7u8; 200_000];
        let sa = std::sync::Arc::new(sa);
        let sa2 = sa.clone();
        let big2 = big.clone();
        let w = std::thread::spawn(move || {
            sa2.send(&Frame::Ctrl(Ctrl::Ping { t_us: 42 })).unwrap();
            sa2.send(&Frame::Data { id: 1, last: true, bytes: big2 }).unwrap();
        });
        assert_eq!(sb.recv().unwrap(), Frame::Ctrl(Ctrl::Ping { t_us: 42 }));
        assert_eq!(sb.recv().unwrap(), Frame::Data { id: 1, last: true, bytes: big });
        w.join().unwrap();

        let mut d = Vec::new();
        let mut o = Vec::new();
        sa.seal_udp(9, b"hello", &mut d).unwrap();
        assert_eq!(udp_session_id(&d), Some(9));
        assert_eq!(sb.open_udp(&d, &mut o), Some(0));
        assert_eq!(&o, b"hello");
        assert_eq!(sb.open_udp(&d, &mut o), None, "replay rejected");
        sa.seal_udp(9, b"again", &mut d).unwrap();
        let mut t = d.clone();
        let last = t.len() - 1;
        t[last] ^= 1;
        assert_eq!(sb.open_udp(&t, &mut o), None, "tamper rejected");
        assert_eq!(sb.open_udp(&d, &mut o), Some(1));
    }
}
