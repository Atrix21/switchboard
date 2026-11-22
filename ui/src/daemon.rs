//! Talking to (and starting) the background service; app-level OS integration.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;
use tao::event_loop::EventLoopProxy;

const IPC_PORT: u16 = 47803;
const APP_PORT: u16 = 47804;

fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("SWITCHBOARD_HOME") {
        return PathBuf::from(d);
    }
    if cfg!(windows) {
        PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("Switchboard")
    } else {
        std::env::var("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")).join("switchboard")
    }
}

pub fn request(req: Value) -> Result<Value, String> {
    let s = TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], IPC_PORT)), Duration::from_millis(500)).map_err(|_| "Switchboard isn't running".to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let tok = std::fs::read_to_string(config_dir().join("ipc.token")).map_err(|e| format!("reading IPC token: {e}"))?;
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{}", tok.trim()).map_err(|e| e.to_string())?;
    writeln!(w, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| e.to_string())
}

/// Holds the single-instance port; later launches connect to it to say "show".
pub struct Guard(TcpListener);

pub fn single_instance() -> Option<Guard> {
    TcpListener::bind(("127.0.0.1", APP_PORT)).ok().map(Guard)
}

pub fn poke_existing() {
    if let Ok(mut s) = TcpStream::connect(("127.0.0.1", APP_PORT)) {
        let _ = s.write_all(b"show\n");
    }
}

pub fn listen_for_show<E: Send + 'static>(guard: Guard, proxy: EventLoopProxy<E>, make: fn() -> E) {
    std::thread::spawn(move || {
        for s in guard.0.incoming().flatten() {
            drop(s);
            if proxy.send_event(make()).is_err() {
                return;
            }
        }
    });
}

fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf())).unwrap_or_default()
}

fn daemon_exe() -> PathBuf {
    exe_dir().join(if cfg!(windows) { "switchboard.exe" } else { "switchboard" })
}

/// Start the background service if it is not already answering.
pub fn ensure_running() {
    if request(serde_json::json!({"cmd": "status"})).is_ok() {
        return;
    }
    let mut c = Command::new(daemon_exe());
    c.args(["run", "--log-file"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000 | 0x0000_0008); // CREATE_NO_WINDOW | DETACHED_PROCESS
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    if let Err(e) = c.spawn() {
        eprintln!("could not start the Switchboard service ({}): {e}", daemon_exe().display());
        return;
    }
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100));
        if request(serde_json::json!({"cmd": "status"})).is_ok() {
            return;
        }
    }
}

pub fn downloads_dir() -> PathBuf {
    let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default();
    home.join("Downloads").join("Switchboard")
}

pub fn open_path(p: &std::path::Path) {
    let _ = std::fs::create_dir_all(p);
    #[cfg(windows)]
    let _ = Command::new("explorer").arg(p).spawn();
    #[cfg(not(windows))]
    let _ = Command::new("xdg-open").arg(p).spawn();
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
fn reg(args: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    Command::new("reg").args(args).creation_flags(0x0800_0000).output()
}

pub fn autostart_enabled() -> bool {
    #[cfg(windows)]
    {
        reg(&["query", RUN_KEY, "/v", "Switchboard"]).map(|o| o.status.success()).unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        autostart_file().exists()
    }
}

#[cfg(not(windows))]
fn autostart_file() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")).join("autostart").join("switchboard.desktop")
}

pub fn set_autostart(on: bool) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    #[cfg(windows)]
    {
        let o = if on { reg(&["add", RUN_KEY, "/v", "Switchboard", "/t", "REG_SZ", "/d", &format!("\"{}\" --background", exe.display()), "/f"])? } else { reg(&["delete", RUN_KEY, "/v", "Switchboard", "/f"])? };
        if !o.status.success() && on {
            return Err(std::io::Error::other(String::from_utf8_lossy(&o.stderr).to_string()));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let f = autostart_file();
        if on {
            std::fs::create_dir_all(f.parent().unwrap())?;
            std::fs::write(&f, format!("[Desktop Entry]\nType=Application\nName=Switchboard\nComment=Share one mouse and keyboard across your computers\nExec=\"{}\" --background\nIcon=switchboard\nX-GNOME-Autostart-enabled=true\nNoDisplay=false\n", exe.display()))
        } else {
            match std::fs::remove_file(&f) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }
}
