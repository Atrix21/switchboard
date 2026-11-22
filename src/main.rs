//! Switchboard – one mouse, every computer on your desk.

mod bench;
mod clipboard;
mod config;
mod ddc;
mod discovery;
mod engine;
mod geom;
mod ipc;
mod keymap;
mod logger;
mod net;
mod platform;
mod proto;
mod stats;

use std::net::SocketAddr;
use std::time::Duration;

use serde_json::json;

const HELP: &str = "\
Switchboard — share one mouse across the computers on your desk

USAGE:
  switchboard run                    Start the background service (foreground process)
  switchboard status                 Show connection state and statistics
  switchboard pair <address>         Pair with another computer running Switchboard
  switchboard confirm [code]         Confirm the 6-digit pairing code shown on both computers
  switchboard reject                 Reject a pending pairing
  switchboard link <edge> <peer> [screen]
                                     Send the pointer to <peer> across <edge> (left|right|top|bottom)
  switchboard switch [peer|local]    Move the pointer to a peer, or bring it back
  switchboard enable | disable       Resume / pause forwarding
  switchboard forget <peer>          Remove a paired computer
  switchboard monitor [list|get|set <input>]
                                     Inspect or switch the shared monitor (input: dp, hdmi, 0x0f…)
  switchboard screens                List displays and their coordinates
  switchboard bench echo             Run the benchmark responder
  switchboard bench net <host> [--rate N] [--secs N] [--proto udp|tcp|both]
  switchboard bench inject [N]       Measure input-injection cost
  switchboard bench crypto           Measure datagram encryption cost
  switchboard quit                   Stop the background service

Config: {CONFIG}
";

fn main() {
    #[cfg(windows)]
    platform::os::init_process();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = match a.as_slice() {
        [] | ["help"] | ["--help"] | ["-h"] => {
            print!("{}", HELP.replace("{CONFIG}", &config::config_path().display().to_string()));
            0
        }
        ["--version"] | ["version"] => {
            println!("switchboard {}", env!("CARGO_PKG_VERSION"));
            0
        }
        ["run", rest @ ..] => run(rest.contains(&"--log-file")),
        ["status"] => print_status(),
        ["pair", addr] => ipc_print(json!({"cmd": "pair", "address": addr})),
        ["confirm"] => ipc_print(json!({"cmd": "confirm"})),
        ["confirm", code @ ..] => ipc_print(json!({"cmd": "confirm", "code": code.join("")})),
        ["reject"] => ipc_print(json!({"cmd": "confirm", "accept": false})),
        ["link", edge, peer] => ipc_print(json!({"cmd": "link", "edge": edge, "peer": peer})),
        ["link", edge, peer, screen] => ipc_print(json!({"cmd": "link", "edge": edge, "peer": peer, "screen": screen})),
        ["switch"] => ipc_print(json!({"cmd": "switch", "to": "local"})),
        ["switch", to] => ipc_print(json!({"cmd": "switch", "to": to})),
        ["enable"] => ipc_print(json!({"cmd": "enable"})),
        ["disable"] => ipc_print(json!({"cmd": "disable"})),
        ["forget", p] => ipc_print(json!({"cmd": "forget", "peer": p})),
        ["quit"] => ipc_print(json!({"cmd": "quit"})),
        ["monitor"] | ["monitor", "list"] => {
            for m in ddc::list() {
                println!("{:<48} ddc={:<5} input={}", m.id, m.ddc_ok, m.input.map(config::input_name).unwrap_or("-".into()));
            }
            0
        }
        ["monitor", "get", rest @ ..] => {
            let model = rest.first().map(|s| s.to_string()).unwrap_or_else(|| config::Config::load().monitor.model);
            let model = if model.is_empty() { ddc::detect("").map(|m| m.model).unwrap_or_default() } else { model };
            match ddc::get_input(&model) {
                Ok(v) => {
                    println!("{model}: 0x{v:02X} ({})", config::input_name(v));
                    0
                }
                Err(e) => {
                    eprintln!("{model}: {e}");
                    1
                }
            }
        }
        ["monitor", "set", input, rest @ ..] => {
            let Some(v) = config::parse_input(input) else {
                eprintln!("bad input {input:?}");
                std::process::exit(2)
            };
            let model = rest.first().map(|s| s.to_string()).unwrap_or_else(|| config::Config::load().monitor.model);
            let model = if model.is_empty() { ddc::detect("").map(|m| m.model).unwrap_or_default() } else { model };
            let t = std::time::Instant::now();
            match ddc::set_input(&model, v) {
                Ok(()) => {
                    println!("{model}: set 0x{v:02X} ({}) in {} ms", config::input_name(v), t.elapsed().as_millis());
                    0
                }
                Err(e) => {
                    eprintln!("{model}: {e}");
                    1
                }
            }
        }
        ["screens"] => {
            for s in platform::screens() {
                println!("{:<56} x={:<6} y={:<6} {}x{}{}", s.name, s.rect.x, s.rect.y, s.rect.w, s.rect.h, if s.primary { "  (primary)" } else { "" });
            }
            0
        }
        ["bench", "echo"] => bench::echo(bench::BENCH_PORT).map(|_| 0).unwrap_or_else(|e| {
            eprintln!("{e}");
            1
        }),
        ["bench", "net", host, rest @ ..] => bench_net(host, rest),
        ["bench", "inject", rest @ ..] => {
            let n = rest.first().and_then(|s| s.parse().ok()).unwrap_or(5000);
            bench::inject(n).map(|_| 0).unwrap_or_else(|e| {
                eprintln!("{e}");
                1
            })
        }
        ["bench", "crypto"] => {
            bench::crypto(100_000);
            0
        }
        _ => {
            eprintln!("unknown command; see `switchboard help`");
            2
        }
    };
    std::process::exit(code);
}

fn run(log_file: bool) -> i32 {
    logger::init(log_file || std::env::var("SWITCHBOARD_LOG_FILE").is_ok());
    let cfg = config::Config::load();
    let id = match net::Identity::load_or_create(&config::identity_path()) {
        Ok(i) => i,
        Err(e) => {
            log::error!("identity: {e}");
            return 1;
        }
    };
    log::info!("Switchboard {} \"{}\" fingerprint {}", env!("CARGO_PKG_VERSION"), cfg.name, id.fingerprint());
    if !config::config_path().exists() {
        let _ = cfg.save();
    }
    let (e, rx) = match engine::Engine::start(cfg, id) {
        Ok(x) => x,
        Err(err) => {
            log::error!("start failed: {err} (is another instance running?)");
            return 1;
        }
    };
    let _ = e.clip.set(std::sync::Arc::new(clipboard::ClipSync));
    if let Err(err) = ipc::serve(e.clone()) {
        log::error!("IPC unavailable: {err}");
    }
    if e.cfg.read().unwrap().discovery {
        discovery::start(e.clone());
    }
    {
        let e = e.clone();
        let _ = ctrlc_handler(move || {
            e.shutdown();
            std::thread::sleep(Duration::from_millis(150));
            std::process::exit(0);
        });
    }
    e.run(rx);
    0
}

#[cfg(windows)]
fn ctrlc_handler(f: impl Fn() + Send + Sync + 'static) -> bool {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    static F: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
    let _ = F.set(Box::new(f));
    unsafe extern "system" fn h(_: u32) -> i32 {
        if let Some(f) = F.get() {
            f();
        }
        1
    }
    unsafe { SetConsoleCtrlHandler(Some(h), 1) != 0 }
}

#[cfg(target_os = "linux")]
fn ctrlc_handler(f: impl Fn() + Send + Sync + 'static) -> bool {
    platform::os::on_terminate(Box::new(f));
    true
}

fn ipc_print(req: serde_json::Value) -> i32 {
    match ipc::request(req) {
        Ok(v) => {
            if let Some(m) = v["message"].as_str() {
                println!("{m}");
            } else if v["ok"] == true {
                println!("ok");
            } else if let Some(e) = v["error"].as_str() {
                eprintln!("error: {e}");
                return 1;
            } else {
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn print_status() -> i32 {
    let v = match ipc::request(json!({"cmd": "status"})) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if std::env::var("SWITCHBOARD_JSON").is_ok() {
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        return 0;
    }
    println!("{}  ({})  {}", v["name"].as_str().unwrap_or(""), v["fingerprint"].as_str().unwrap_or(""), if v["disabled"] == true { "DISABLED" } else { "" });
    println!("pointer: {}", v["mode"].as_str().unwrap_or("?"));
    for p in v["peers"].as_array().into_iter().flatten() {
        if p["trusted"] == true {
            println!("  ● {} {} {}  rtt p50 {:.1} ms p99 {:.1} ms", p["name"].as_str().unwrap_or(""), p["address"].as_str().unwrap_or(""), p["os"].as_str().unwrap_or(""), p["rtt_ms"]["p50"].as_f64().unwrap_or(0.0), p["rtt_ms"]["p99"].as_f64().unwrap_or(0.0));
        } else {
            println!(
                "  ◌ pairing with {} ({}) — code {}{}",
                p["name"].as_str().unwrap_or(""),
                p["address"].as_str().unwrap_or(""),
                p["pairing_code"].as_str().unwrap_or(""),
                if p["confirmed_here"] == true { " (confirmed here)" } else { "  → run `switchboard confirm` if it matches" }
            );
        }
    }
    for d in v["discovered"].as_array().into_iter().flatten() {
        if d["paired"] != true {
            println!("  ○ nearby: {} at {}  → `switchboard pair {}`", d["name"].as_str().unwrap_or(""), d["address"].as_str().unwrap_or(""), d["address"].as_str().unwrap_or("").split(':').next().unwrap_or(""));
        }
    }
    let m = &v["monitor"];
    if m["model"].as_str().is_some_and(|s| !s.is_empty()) {
        println!("monitor: {}  this computer on {}  last switch: {}", m["model"].as_str().unwrap_or(""), m["local_input"].as_u64().map(|i| config::input_name(i as u16)).unwrap_or("?".into()), m["last_ms"].as_u64().map(|x| format!("{x} ms")).unwrap_or("-".into()));
    }
    let s = &v["stats"];
    println!(
        "input: capture {:.0}/s  sent {:.0}/s  handler p50 {} µs  inject p50 {} µs  cpu {:.2}s  mem {:.1} MiB",
        s["capture_events_per_s"].as_f64().unwrap_or(0.0),
        s["datagrams_sent_per_s"].as_f64().unwrap_or(0.0),
        s["capture_handler_us_p50"],
        s["sink_inject_us_p50"],
        s["cpu_s"].as_f64().unwrap_or(0.0),
        s["rss_bytes"].as_f64().unwrap_or(0.0) / 1048576.0
    );
    0
}

fn bench_net(host: &str, rest: &[&str]) -> i32 {
    let mut rate = 1000;
    let mut secs = 10;
    let mut proto = "both";
    let mut size = 64;
    let mut it = rest.iter();
    while let Some(k) = it.next() {
        let v = it.next().copied().unwrap_or("");
        match *k {
            "--rate" => rate = v.parse().unwrap_or(rate),
            "--secs" => secs = v.parse().unwrap_or(secs),
            "--proto" => proto = v,
            "--size" => size = v.parse().unwrap_or(size),
            _ => {}
        }
    }
    let Some(addr) = engine::parse_addr(host, bench::BENCH_PORT) else {
        eprintln!("bad host");
        return 2;
    };
    let addr: SocketAddr = addr;
    println!("probing {addr} at {rate} Hz for {secs} s, {size}-byte payloads");
    if proto == "udp" || proto == "both" {
        match bench::net_udp(addr, rate, secs, size) {
            Ok(r) => r.print(),
            Err(e) => eprintln!("udp: {e}"),
        }
    }
    if proto == "tcp" || proto == "both" {
        match bench::net_tcp(addr, rate, secs, size) {
            Ok(r) => r.print(),
            Err(e) => eprintln!("tcp: {e}"),
        }
    }
    0
}
