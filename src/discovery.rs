//! LAN discovery: small UDP broadcast beacons every 2 s.
//!
//! Beacons only advertise presence (name, public key, port). They carry no
//! secrets and grant nothing: every connection is still mutually
//! authenticated by Noise and must belong to a paired key to be used.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::proto::DISCOVERY_PORT;

#[derive(Serialize, Deserialize)]
struct Beacon {
    sb: u32,
    key: String,
    name: String,
    port: u16,
}

pub fn start(e: Arc<Engine>) {
    let sock = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) {
        Ok(s) => s,
        Err(err) => {
            log::warn!("discovery disabled: cannot bind UDP {DISCOVERY_PORT}: {err}");
            return;
        }
    };
    let _ = sock.set_broadcast(true);
    let rx = sock.try_clone().expect("clone socket");
    let me = crate::net::hex(&e.id.public);
    {
        let e = e.clone();
        let me = me.clone();
        std::thread::Builder::new()
            .name("sb-disc-rx".into())
            .spawn(move || {
                let mut buf = [0u8; 1024];
                loop {
                    let Ok((n, from)) = rx.recv_from(&mut buf) else { continue };
                    if !crate::engine::is_local_net(from.ip()) {
                        continue;
                    }
                    let Ok(b) = serde_json::from_slice::<Beacon>(&buf[..n]) else { continue };
                    if b.sb != 1 || b.key == me || crate::net::unhex(&b.key).is_none() {
                        continue;
                    }
                    e.discovered_peer(b.key, SocketAddr::new(from.ip(), b.port), b.name.chars().take(64).collect());
                }
            })
            .expect("spawn discovery");
    }
    std::thread::Builder::new()
        .name("sb-disc-tx".into())
        .spawn(move || loop {
            let (name, port, on) = {
                let c = e.cfg.read().unwrap();
                (c.name.clone(), c.port, c.discovery)
            };
            if on {
                let b = serde_json::to_vec(&Beacon { sb: 1, key: me.clone(), name, port }).unwrap();
                let _ = sock.send_to(&b, SocketAddrV4::new(Ipv4Addr::BROADCAST, DISCOVERY_PORT));
                for bc in subnet_broadcasts() {
                    let _ = sock.send_to(&b, SocketAddrV4::new(bc, DISCOVERY_PORT));
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        })
        .expect("spawn discovery");
}

/// Directed broadcast addresses of local /24s (covers typical home networks;
/// limited broadcast alone is filtered by some Wi-Fi drivers on Windows).
fn subnet_broadcasts() -> Vec<Ipv4Addr> {
    // Learn our primary LAN address by "connecting" a UDP socket (no packets sent).
    let mut v = vec![];
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("192.168.0.1:9").is_ok() || s.connect("10.0.0.1:9").is_ok() {
            if let Ok(SocketAddr::V4(a)) = s.local_addr() {
                let o = a.ip().octets();
                if a.ip().is_private() {
                    v.push(Ipv4Addr::new(o[0], o[1], o[2], 255));
                }
            }
        }
    }
    v
}
