//! Wire protocol.
//!
//! Two channels per peer:
//! * **Control** – TCP, Noise-encrypted frames carrying JSON [`Ctrl`] messages.
//!   Low rate, must be reliable and ordered (enter/leave, pairing, clipboard).
//! * **Input** – UDP, Noise-encrypted (stateless, nonce = sequence number)
//!   datagrams carrying an [`InputState`] snapshot.
//!
//! ## Why snapshots instead of events
//! Every input datagram carries *cumulative* counters (total dx/dy/wheel since
//! the start of the current control epoch), the *current* button bitmask and a
//! ring of the most recent button transitions. Each transition records the
//! motion counters at the instant it happened. Consequences:
//! * A lost datagram loses nothing: the next one carries the totals and any
//!   transitions not yet seen, in order. No retransmission, no head-of-line
//!   blocking (important on Wi-Fi where bursts of loss/latency are common).
//! * Re-ordered/duplicated datagrams are harmless: older sequence numbers are
//!   simply dropped because newer snapshots supersede them.
//! * Clicks land at the exact motion position at which they happened, even if
//!   motion is coalesced.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 47801; // TCP control + UDP input
pub const DISCOVERY_PORT: u16 = 47802; // UDP broadcast beacons
pub const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Mouse buttons, as bit positions in [`InputState::buttons`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Button {
    Left = 0,
    Right = 1,
    Middle = 2,
    Back = 3,    // X1 / BTN_SIDE
    Forward = 4, // X2 / BTN_EXTRA
}

impl Button {
    pub const ALL: [Button; 5] = [Button::Left, Button::Right, Button::Middle, Button::Back, Button::Forward];
    pub fn from_u8(v: u8) -> Option<Button> {
        Button::ALL.get(v as usize).copied()
    }
    pub fn bit(self) -> u16 {
        1 << (self as u8)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ButtonEvent {
    /// Monotonic per-epoch transition counter (starts at 1).
    pub seq: u32,
    pub button: u8,
    pub down: bool,
    /// Cumulative motion at the instant of the transition.
    pub cum_dx: i32,
    pub cum_dy: i32,
}

pub const EVENT_RING: usize = 16;

/// Snapshot sent source -> sink on every input change (and as keepalive).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct InputState {
    /// Control epoch; changes every time control is handed to the sink.
    pub epoch: u32,
    /// Source timestamp (µs, source monotonic clock) – used for RTT/jitter.
    pub t_us: u64,
    pub cum_dx: i32,
    pub cum_dy: i32,
    /// Wheel in 1/120 notch units (Windows WHEEL_DELTA / Linux hi-res units).
    pub cum_wheel_v: i32,
    pub cum_wheel_h: i32,
    pub buttons: u16,
    /// Most recent transitions (oldest first). At most EVENT_RING.
    pub events: Vec<ButtonEvent>,
}

const KIND_INPUT: u8 = 1;
const KIND_ACK: u8 = 2;

impl InputState {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.push(KIND_INPUT);
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.t_us.to_le_bytes());
        out.extend_from_slice(&self.cum_dx.to_le_bytes());
        out.extend_from_slice(&self.cum_dy.to_le_bytes());
        out.extend_from_slice(&self.cum_wheel_v.to_le_bytes());
        out.extend_from_slice(&self.cum_wheel_h.to_le_bytes());
        out.extend_from_slice(&self.buttons.to_le_bytes());
        let n = self.events.len().min(EVENT_RING);
        out.push(n as u8);
        for e in &self.events[self.events.len() - n..] {
            out.extend_from_slice(&e.seq.to_le_bytes());
            out.push(e.button | if e.down { 0x80 } else { 0 });
            out.extend_from_slice(&e.cum_dx.to_le_bytes());
            out.extend_from_slice(&e.cum_dy.to_le_bytes());
        }
    }

    pub fn decode(b: &[u8]) -> Option<InputState> {
        let mut r = Reader { b, p: 0 };
        if r.u8()? != KIND_INPUT {
            return None;
        }
        let mut s = InputState {
            epoch: r.u32()?,
            t_us: r.u64()?,
            cum_dx: r.i32()?,
            cum_dy: r.i32()?,
            cum_wheel_v: r.i32()?,
            cum_wheel_h: r.i32()?,
            buttons: r.u16()?,
            events: Vec::new(),
        };
        let n = r.u8()? as usize;
        if n > EVENT_RING {
            return None;
        }
        for _ in 0..n {
            let seq = r.u32()?;
            let bd = r.u8()?;
            s.events.push(ButtonEvent { seq, button: bd & 0x7f, down: bd & 0x80 != 0, cum_dx: r.i32()?, cum_dy: r.i32()? });
        }
        Some(s)
    }
}

/// Sink -> source acknowledgement (liveness + latency measurement).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct InputAck {
    pub epoch: u32,
    /// Highest input sequence number received.
    pub seq: u64,
    /// Echo of `t_us` from that datagram.
    pub echo_t_us: u64,
    /// Time the sink held the datagram before acking, µs (receive -> inject done).
    pub hold_us: u32,
    /// Sink-side median inject duration since last ack, µs.
    pub inject_us: u32,
    pub received: u32,
}

impl InputAck {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.push(KIND_ACK);
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.echo_t_us.to_le_bytes());
        out.extend_from_slice(&self.hold_us.to_le_bytes());
        out.extend_from_slice(&self.inject_us.to_le_bytes());
        out.extend_from_slice(&self.received.to_le_bytes());
    }
    pub fn decode(b: &[u8]) -> Option<InputAck> {
        let mut r = Reader { b, p: 0 };
        if r.u8()? != KIND_ACK {
            return None;
        }
        Some(InputAck { epoch: r.u32()?, seq: r.u64()?, echo_t_us: r.u64()?, hold_us: r.u32()?, inject_us: r.u32()?, received: r.u32()? })
    }
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let s = self.b.get(self.p..self.p + N)?;
        self.p += N;
        s.try_into().ok()
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take()?))
    }
}

/// Which side of a screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScreenInfo {
    pub name: String,
    pub rect: Rect,
    pub primary: bool,
}

/// Control-channel messages (JSON inside Noise frames).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Ctrl {
    /// First message on every session. `udp_session` is the id the sender
    /// expects in the header of datagrams addressed to it.
    Hello {
        version: u32,
        name: String,
        os: String,
        udp_port: u16,
        udp_session: u32,
        screens: Vec<ScreenInfo>,
        /// Shared-monitor model substring and the input this machine is on.
        monitor_model: String,
        monitor_input: Option<u16>,
    },
    /// Sent when the peer is not yet trusted; carries nothing – the SAS is
    /// derived independently on both sides from the handshake hash.
    PairPending,
    PairConfirm { accept: bool },
    Ping { t_us: u64 },
    Pong { t_us: u64 },
    /// Source -> sink: you now own the pointer. `pos` is 0..1 along `edge` of
    /// the sink's virtual desktop where the pointer should appear.
    Enter { epoch: u32, edge: Edge, pos: f32 },
    /// Sink -> source: pointer crossed back out of `edge` at `pos`.
    Leave { epoch: u32, edge: Edge, pos: f32 },
    /// Source -> sink: control revoked (manual switch / emergency / shutdown).
    /// Sink must release all buttons.
    Revoke { epoch: u32 },
    /// Either direction: ask the peer to take/return control
    /// (e.g. hotkey pressed on the machine the keyboard is connected to).
    RequestFocus { target: Focus },
    /// Ask the peer to switch the shared monitor (fallback when local DDC fails).
    MonitorSet { input: u16, req: u32 },
    MonitorResult { req: u32, ok: bool, detail: String },
    /// Clipboard: offer metadata only; content is pulled on demand.
    ClipOffer { id: u64, kinds: Vec<ClipKind>, summary: String },
    ClipRequest { id: u64, kind: ClipKind },
    /// Header for clipboard/file payload; followed by binary data frames
    /// (see [`Frame::Data`]) carrying `total` bytes.
    ClipData { id: u64, kind: ClipKind, total: u64 },
    ClipError { id: u64, detail: String },
    Bye,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Focus {
    /// The machine sending the message.
    Me,
    /// The machine receiving the message.
    You,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ClipKind {
    Text,
    /// PNG-encoded image.
    Png,
    /// A file bundle (tar-like Switchboard archive of files and folders).
    Files,
}

/// A decrypted control-channel frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Ctrl(Ctrl),
    /// Bulk payload chunk for transfer `id`.
    Data { id: u64, last: bool, bytes: Vec<u8> },
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Frame::Ctrl(m) => {
                let mut v = vec![0u8];
                serde_json::to_writer(&mut v, m).expect("ctrl serialize");
                v
            }
            Frame::Data { id, last, bytes } => {
                let mut v = Vec::with_capacity(10 + bytes.len());
                v.push(1);
                v.extend_from_slice(&id.to_le_bytes());
                v.push(*last as u8);
                v.extend_from_slice(bytes);
                v
            }
        }
    }
    pub fn decode(b: &[u8]) -> Option<Frame> {
        match *b.first()? {
            0 => serde_json::from_slice(&b[1..]).ok().map(Frame::Ctrl),
            1 if b.len() >= 10 => Some(Frame::Data { id: u64::from_le_bytes(b[1..9].try_into().ok()?), last: b[9] != 0, bytes: b[10..].to_vec() }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_roundtrip() {
        let s = InputState {
            epoch: 7,
            t_us: 123456789,
            cum_dx: -5,
            cum_dy: 99999,
            cum_wheel_v: -240,
            cum_wheel_h: 120,
            buttons: 0b101,
            events: vec![ButtonEvent { seq: 1, button: 0, down: true, cum_dx: -3, cum_dy: 4 }, ButtonEvent { seq: 2, button: 2, down: false, cum_dx: -5, cum_dy: 99999 }],
        };
        let mut b = Vec::new();
        s.encode(&mut b);
        assert_eq!(InputState::decode(&b).unwrap(), s);
        assert!(InputState::decode(&b[..b.len() - 1]).is_none());
    }

    #[test]
    fn ack_roundtrip() {
        let a = InputAck { epoch: 3, seq: 77, echo_t_us: 5, hold_us: 9, inject_us: 11, received: 1000 };
        let mut b = Vec::new();
        a.encode(&mut b);
        assert_eq!(InputAck::decode(&b).unwrap(), a);
    }

    #[test]
    fn ctrl_roundtrip() {
        let m = Frame::Data { id: 4, last: true, bytes: vec![0, 1, 2, 255] };
        assert_eq!(Frame::decode(&m.encode()).unwrap(), m);
        let m = Frame::Ctrl(Ctrl::Enter { epoch: 2, edge: Edge::Right, pos: 0.5 });
        assert_eq!(Frame::decode(&m.encode()).unwrap(), m);
    }
}
