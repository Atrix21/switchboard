//! Local control API for the CLI and the desktop UI.
//!
//! Newline-delimited JSON over TCP bound to 127.0.0.1 only. The first line of
//! every connection must be the secret from `ipc.token` (a per-user file), so
//! other local users cannot drive the daemon.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::engine::{parse_addr, Engine, Ev};

pub const IPC_PORT: u16 = 47803;

fn token() -> String {
    let p = crate::config::ipc_token_path();
    if let Ok(t) = std::fs::read_to_string(&p) {
        if t.trim().len() >= 32 {
            return t.trim().to_string();
        }
    }
    let mut b = [0u8; 24];
    getrandom::getrandom(&mut b).expect("rng");
    let t = crate::net::hex(&b);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let _ = crate::config::write_private(&p, t.as_bytes());
    t
}

pub fn serve(e: Arc<Engine>) -> std::io::Result<()> {
    let tok = token();
    let port = std::env::var("SWITCHBOARD_IPC_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(IPC_PORT);
    let l = TcpListener::bind(("127.0.0.1", port))?;
    std::thread::Builder::new().name("sb-ipc".into()).spawn(move || {
        for s in l.incoming().flatten() {
            let e = e.clone();
            let tok = tok.clone();
            std::thread::spawn(move || {
                let _ = client(e, s, &tok);
            });
        }
    })?;
    Ok(())
}

fn client(e: Arc<Engine>, s: TcpStream, tok: &str) -> std::io::Result<()> {
    let mut w = s.try_clone()?;
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line)?;
    if line.trim() != tok {
        w.write_all(b"{\"error\":\"unauthorized\"}\n")?;
        return Ok(());
    }
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(err) => json!({"cmd": "_bad", "error": err.to_string()}),
        };
        let resp = if req["cmd"] == "status" {
            e.status() // read-only; answer without hopping to the engine thread
        } else {
            let (tx, rx) = channel();
            let _ = e.tx.send(Ev::Ipc(req, tx));
            rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| json!({"error": "timeout"}))
        };
        w.write_all(serde_json::to_string(&resp).unwrap().as_bytes())?;
        w.write_all(b"\n")?;
    }
}

/// Runs on the engine thread.
pub fn handle(e: &Arc<Engine>, req: Value) -> Value {
    let s = |k: &str| req[k].as_str().map(str::to_string);
    let res: Result<Value, String> = match req["cmd"].as_str().unwrap_or("") {
        "pair" => {
            let addr = s("address").unwrap_or_default();
            let port = e.cfg.read().unwrap().port;
            match parse_addr(&addr, port) {
                Some(a) if crate::engine::is_local_net(a.ip()) => {
                    e.dial(a);
                    Ok(json!({"ok": true, "message": format!("connecting to {a}; compare the 6-digit code on both computers")}))
                }
                Some(_) => Err("only local-network addresses are allowed".into()),
                None => Err(format!("bad address {addr:?}")),
            }
        }
        "confirm" => e.confirm_pairing(s("code").as_deref(), req["accept"].as_bool().unwrap_or(true)).map(|m| json!({"ok": true, "message": m})),
        "switch" => e.switch_to(&s("to").unwrap_or_else(|| "local".into())).map(|m| json!({"ok": true, "message": m})),
        "disable" => {
            e.set_disabled(true);
            Ok(json!({"ok": true}))
        }
        "enable" => {
            e.set_disabled(false);
            Ok(json!({"ok": true}))
        }
        "link" => {
            let edge = serde_json::from_value(req["edge"].clone()).map_err(|x| x.to_string());
            match (edge, s("peer")) {
                (Ok(edge), Some(peer)) => {
                    let screen = s("screen");
                    {
                        let mut c = e.cfg.write().unwrap();
                        if req["replace"].as_bool().unwrap_or(true) {
                            c.links.retain(|l| l.peer != peer);
                        }
                        let to_screen = s("to_screen");
                        let to_edge = serde_json::from_value(req["to_edge"].clone()).ok();
                        c.links.push(crate::config::Link { edge, peer: peer.clone(), screen, to_screen, to_edge });
                        let _ = c.save();
                    }
                    e.reload_config();
                    Ok(json!({"ok": true}))
                }
                (Err(x), _) => Err(x),
                _ => Err("peer required".into()),
            }
        }
        "monitor_set" => {
            let input = s("input").and_then(|i| crate::config::parse_input(&i));
            match input {
                Some(i) => {
                    e.monitor.request(i);
                    Ok(json!({"ok": true}))
                }
                None => Err("bad input".into()),
            }
        }
        "reload" => {
            e.reload_config();
            Ok(json!({"ok": true}))
        }
        "forget" => {
            let name = s("peer").unwrap_or_default();
            let mut c = e.cfg.write().unwrap();
            let before = c.peers.len();
            c.peers.retain(|p| p.name != name);
            c.links.retain(|l| l.peer != name);
            let _ = c.save();
            Ok(json!({"ok": true, "removed": before - c.peers.len()}))
        }
        "settings" => {
            // Partial update of the config from the UI (validated by round-trip).
            let mut cur = serde_json::to_value(&*e.cfg.read().unwrap()).unwrap();
            merge(&mut cur, &req["settings"]);
            match serde_json::from_value::<crate::config::Config>(cur) {
                Ok(c) => {
                    let r = c.save().map_err(|x| x.to_string());
                    e.reload_config();
                    r.map(|_| json!({"ok": true}))
                }
                Err(x) => Err(x.to_string()),
            }
        }
        // ---- test-only hooks (SWITCHBOARD_TEST=1): synthetic capture + injector readback
        "synth" if std::env::var("SWITCHBOARD_TEST").is_ok() => {
            use crate::platform::CapEvent;
            use crate::proto::Button;
            let cursor = (req["x"].as_i64().unwrap_or(0) as i32, req["y"].as_i64().unwrap_or(0) as i32);
            for ev in req["events"].as_array().into_iter().flatten() {
                let a: Vec<i64> = ev.as_array().map(|v| v.iter().filter_map(|x| x.as_i64()).collect()).unwrap_or_default();
                let ce = match (ev[0].as_str(), a.as_slice()) {
                    (Some("m"), _) => CapEvent::Motion { dx: ev[1].as_i64().unwrap_or(0) as i32, dy: ev[2].as_i64().unwrap_or(0) as i32 },
                    (Some("d"), _) => CapEvent::Button { button: Button::from_u8(ev[1].as_u64().unwrap_or(0) as u8).unwrap(), down: true },
                    (Some("u"), _) => CapEvent::Button { button: Button::from_u8(ev[1].as_u64().unwrap_or(0) as u8).unwrap(), down: false },
                    (Some("k"), _) => CapEvent::Key { code: ev[1].as_u64().unwrap_or(0) as u16, down: ev[2].as_bool().unwrap_or(true) },
                    (Some("w"), _) => CapEvent::Wheel { v: ev[1].as_i64().unwrap_or(0) as i32, h: ev[2].as_i64().unwrap_or(0) as i32 },
                    _ => continue,
                };
                e.on_capture(ce, cursor);
            }
            Ok(json!({"ok": true}))
        }
        "recorded" if std::env::var("SWITCHBOARD_TEST").is_ok() => {
            let r = crate::platform::null::recorded();
            let r = r.lock().unwrap();
            Ok(json!({"x": r.x, "y": r.y, "buttons": r.buttons, "log": r.log, "wheel_v": r.wheel_v, "wheel_h": r.wheel_h, "keys": r.keys.iter().collect::<Vec<_>>()}))
        }
        "config" => Ok(serde_json::to_value(&*e.cfg.read().unwrap()).unwrap()),
        "quit" => {
            e.shutdown();
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                std::process::exit(0);
            });
            Ok(json!({"ok": true}))
        }
        other => Err(format!("unknown command {other:?}")),
    };
    res.unwrap_or_else(|m| json!({"ok": false, "error": m}))
}

fn merge(a: &mut Value, b: &Value) {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                merge(a.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (a, b) => *a = b.clone(),
    }
}

/// Client side, used by the CLI.
pub fn request(req: Value) -> Result<Value, String> {
    let port = std::env::var("SWITCHBOARD_IPC_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(IPC_PORT);
    let s = TcpStream::connect(("127.0.0.1", port)).map_err(|_| "Switchboard is not running (start it with `switchboard run`)".to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let tok = std::fs::read_to_string(crate::config::ipc_token_path()).map_err(|e| format!("reading IPC token: {e}"))?;
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{}", tok.trim()).map_err(|e| e.to_string())?;
    writeln!(w, "{}", req).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| e.to_string())
}
