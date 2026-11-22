//! User configuration (`switchboard.toml`) and on-disk state locations.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::proto::{Edge, DEFAULT_PORT};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Name shown to peers.
    pub name: String,
    pub port: u16,
    /// Broadcast discovery beacons on the LAN.
    pub discovery: bool,
    pub peers: Vec<PeerConfig>,
    pub links: Vec<Link>,
    pub switching: Switching,
    pub monitor: MonitorConfig,
    pub hotkeys: Hotkeys,
    pub clipboard: ClipboardConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct PeerConfig {
    pub name: String,
    /// Hex-encoded X25519 static public key (set by pairing).
    pub key: String,
    /// Optional fixed address (used in addition to discovery).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// "When the pointer leaves `screen`'s `edge`, it goes to `peer`."
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Link {
    pub edge: Edge,
    pub peer: String,
    /// Restrict to one display (substring of its name). None = whole desktop edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Switching {
    /// Distance (raw mouse counts) the pointer must be pushed past the edge
    /// before switching. Prevents accidental switches; 0 = immediate.
    pub edge_resistance: i32,
    /// Pixels at each end of an edge that never trigger a switch (keeps screen
    /// corners usable for hot corners / close buttons).
    pub corner_deadzone: i32,
    /// Refuse to switch while a mouse button is held (avoids stuck drags).
    pub lock_while_button_held: bool,
    /// Source returns to local control if the peer stops acknowledging for this long.
    pub peer_timeout_ms: u64,
    /// Emergency mouse chord: hold left+right+middle this long to force local control. 0 = off.
    pub chord_release_ms: u64,
}

impl Default for Switching {
    fn default() -> Self {
        Switching { edge_resistance: 0, corner_deadzone: 8, lock_while_button_held: true, peer_timeout_ms: 750, chord_release_ms: 1200 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    /// Switch the shared monitor's input when the pointer moves between machines.
    pub enabled: bool,
    /// Substring identifying the shared monitor (model name).
    pub model: String,
    /// The monitor input (VCP 0x60 value) this machine is connected to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_input: Option<u16>,
    /// Linux: I2C bus number override (e.g. 5 for /dev/i2c-5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub i2c_bus: Option<u32>,
    /// "auto" | "native" | "ddcutil"
    pub backend: String,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        MonitorConfig { enabled: true, model: String::new(), local_input: None, i2c_bus: None, backend: "auto".into() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    /// Always returns the pointer to this machine and disables forwarding until re-enabled.
    pub emergency: String,
    /// Send the pointer to the next peer / bring it back.
    pub toggle: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys { emergency: "ctrl+alt+shift+escape".into(), toggle: "ctrl+alt+shift+s".into() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    pub enabled: bool,
    pub text: bool,
    pub images: bool,
    pub files: bool,
    /// Largest file bundle transferred automatically, MiB.
    pub max_files_mib: u64,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        ClipboardConfig { enabled: true, text: true, images: true, files: true, max_files_mib: 2048 }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: hostname(),
            port: DEFAULT_PORT,
            discovery: true,
            peers: vec![],
            links: vec![],
            switching: Switching::default(),
            monitor: MonitorConfig::default(),
            hotkeys: Hotkeys::default(),
            clipboard: ClipboardConfig::default(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("SWITCHBOARD_HOME") {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| ".".into())).join("Switchboard")
    }
    #[cfg(not(windows))]
    {
        let base = std::env::var("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config"));
        base.join("switchboard")
    }
}

pub fn config_path() -> PathBuf {
    config_dir().join("switchboard.toml")
}
pub fn identity_path() -> PathBuf {
    config_dir().join("identity.key")
}
pub fn ipc_token_path() -> PathBuf {
    config_dir().join("ipc.token")
}
pub fn log_path() -> PathBuf {
    config_dir().join("switchboard.log")
}

impl Config {
    pub fn load() -> Config {
        Self::load_from(&config_path())
    }
    pub fn load_from(p: &Path) -> Config {
        match std::fs::read_to_string(p) {
            Ok(s) => match toml::from_str(&s) {
                Ok(c) => c,
                Err(e) => {
                    log::error!("config {}: {e}; using defaults", p.display());
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }
    pub fn save(&self) -> io::Result<()> {
        let p = config_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let s = toml::to_string_pretty(self).map_err(io::Error::other)?;
        let tmp = p.with_extension("toml.tmp");
        std::fs::write(&tmp, s)?;
        std::fs::rename(tmp, p)
    }
    pub fn peer_by_key(&self, key_hex: &str) -> Option<&PeerConfig> {
        self.peers.iter().find(|p| p.key == key_hex)
    }
}

pub fn hostname() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "windows".into())
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "linux".into())
    }
}

/// Write a file readable only by the current user.
pub fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    {
        // %APPDATA% is already per-user ACL'd on Windows.
        std::fs::write(path, data)
    }
}

pub fn parse_input(s: &str) -> Option<u16> {
    let s = s.trim();
    match s.to_ascii_lowercase().as_str() {
        "dp" | "dp1" | "displayport" | "displayport1" => Some(0x0F),
        "dp2" | "displayport2" => Some(0x10),
        "hdmi" | "hdmi1" => Some(0x11),
        "hdmi2" => Some(0x12),
        _ => {
            if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                u16::from_str_radix(h, 16).ok()
            } else {
                s.parse().ok()
            }
        }
    }
}

pub fn input_name(v: u16) -> String {
    match v & 0xff {
        0x01 => "VGA-1".into(),
        0x03 => "DVI-1".into(),
        0x0F => "DisplayPort-1".into(),
        0x10 => "DisplayPort-2".into(),
        0x11 => "HDMI-1".into(),
        0x12 => "HDMI-2".into(),
        0x1B => "USB-C".into(),
        x => format!("0x{x:02X}"),
    }
}
