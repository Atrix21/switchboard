//! Keyboard codes. The wire format is the Linux evdev keycode (KEY_*): a
//! layout-independent physical key position. Each OS applies its own layout.
//!
//! Windows: PC/AT scan code set 1 (+ E0 "extended" flag) <-> evdev. For the
//! main block the two are numerically identical; extended keys use a table.
//! X11: keycode = evdev + 8.
#![cfg_attr(not(windows), allow(dead_code))]

/// (evdev, set-1 scancode, extended)
const EXT: &[(u16, u16)] = &[
    (96, 0x1C),  // KP_ENTER
    (97, 0x1D),  // RIGHTCTRL
    (98, 0x35),  // KP_SLASH
    (99, 0x37),  // SYSRQ / PrintScreen
    (100, 0x38), // RIGHTALT
    (102, 0x47), // HOME
    (103, 0x48), // UP
    (104, 0x49), // PAGEUP
    (105, 0x4B), // LEFT
    (106, 0x4D), // RIGHT
    (107, 0x4F), // END
    (108, 0x50), // DOWN
    (109, 0x51), // PAGEDOWN
    (110, 0x52), // INSERT
    (111, 0x53), // DELETE
    (125, 0x5B), // LEFTMETA
    (126, 0x5C), // RIGHTMETA
    (127, 0x5D), // COMPOSE / Menu
    (113, 0x20), // MUTE
    (114, 0x2E), // VOLUMEDOWN
    (115, 0x30), // VOLUMEUP
    (164, 0x22), // PLAYPAUSE
    (166, 0x24), // STOPCD
    (165, 0x10), // PREVIOUSSONG
    (163, 0x19), // NEXTSONG
    (116, 0x5E), // POWER
    (142, 0x5F), // SLEEP
    (69, 0x45),  // NUMLOCK (extended in LL hooks)
];

/// Non-extended scancodes that are not numerically equal to evdev.
const PLAIN: &[(u16, u16)] = &[
    (183, 0x64), (184, 0x65), (185, 0x66), (186, 0x67), (187, 0x68), (188, 0x69), // F13-F18
    (189, 0x6A), (190, 0x6B), (191, 0x6C), (192, 0x6D), (193, 0x6E), (194, 0x76), // F19-F24
    (93, 0x70),  // KATAKANAHIRAGANA
    (89, 0x73),  // RO
    (92, 0x79),  // HENKAN
    (94, 0x7B),  // MUHENKAN
    (124, 0x7D), // YEN
];

pub const KEY_PAUSE: u16 = 119;

/// Windows scancode -> evdev. `pause` is true when the VK is VK_PAUSE
/// (Pause shares scancode 0x45 with NumLock).
pub fn win_to_evdev(scan: u16, extended: bool, pause: bool) -> Option<u16> {
    if pause {
        return Some(KEY_PAUSE);
    }
    if extended {
        return EXT.iter().find(|(_, s)| *s == scan).map(|(e, _)| *e);
    }
    if let Some((e, _)) = PLAIN.iter().find(|(_, s)| *s == scan) {
        return Some(*e);
    }
    if (1..=88).contains(&scan) {
        return Some(scan);
    }
    None
}

/// evdev -> (scancode, extended). None for Pause (inject via VK) or unknown.
pub fn evdev_to_win(code: u16) -> Option<(u16, bool)> {
    if let Some((_, s)) = EXT.iter().find(|(e, _)| *e == code) {
        return Some((*s, true));
    }
    if let Some((_, s)) = PLAIN.iter().find(|(e, _)| *e == code) {
        return Some((*s, false));
    }
    if (1..=88).contains(&code) && code != 69 {
        return Some((code, false));
    }
    None
}

pub const LEFTCTRL: u16 = 29;
pub const RIGHTCTRL: u16 = 97;
pub const LEFTSHIFT: u16 = 42;
pub const RIGHTSHIFT: u16 = 54;
pub const LEFTALT: u16 = 56;
pub const RIGHTALT: u16 = 100;
pub const LEFTMETA: u16 = 125;
pub const RIGHTMETA: u16 = 126;

/// Hotkey key name (as in the config) -> evdev code.
pub fn name_to_evdev(k: &str) -> Option<u16> {
    const LETTERS: [u16; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
    let b = k.as_bytes();
    if b.len() == 1 && b[0].is_ascii_lowercase() {
        return Some(LETTERS[(b[0] - b'a') as usize]);
    }
    if b.len() == 1 && b[0].is_ascii_digit() {
        return Some(if b[0] == b'0' { 11 } else { (b[0] - b'1') as u16 + 2 });
    }
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()) {
        return match n {
            1..=10 => Some(58 + n),
            11 => Some(87),
            12 => Some(88),
            13..=24 => Some(170 + n),
            _ => None,
        };
    }
    Some(match k {
        "escape" => 1,
        "space" => 57,
        "tab" => 15,
        "enter" | "return" => 28,
        "backspace" => 14,
        "left" => 105,
        "right" => 106,
        "up" => 103,
        "down" => 108,
        "home" => 102,
        "end" => 107,
        "pageup" => 104,
        "pagedown" => 109,
        "insert" => 110,
        "delete" => 111,
        "pause" => KEY_PAUSE,
        "scrolllock" => 70,
        _ => return None,
    })
}

/// 256-bit pressed-key bitmap.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct KeySet(pub [u8; 32]);

impl KeySet {
    pub fn set(&mut self, code: u16, down: bool) {
        if code < 256 {
            if down {
                self.0[code as usize / 8] |= 1 << (code % 8);
            } else {
                self.0[code as usize / 8] &= !(1 << (code % 8));
            }
        }
    }
    pub fn get(&self, code: u16) -> bool {
        code < 256 && self.0[code as usize / 8] & (1 << (code % 8)) != 0
    }
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|b| *b == 0)
    }
    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        (0..256u16).filter(|c| self.get(*c))
    }
    pub fn ctrl(&self) -> bool {
        self.get(LEFTCTRL) || self.get(RIGHTCTRL)
    }
    pub fn shift(&self) -> bool {
        self.get(LEFTSHIFT) || self.get(RIGHTSHIFT)
    }
    pub fn alt(&self) -> bool {
        self.get(LEFTALT) || self.get(RIGHTALT)
    }
    pub fn meta(&self) -> bool {
        self.get(LEFTMETA) || self.get(RIGHTMETA)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        for code in 1..200u16 {
            if let Some((s, e)) = evdev_to_win(code) {
                assert_eq!(win_to_evdev(s, e, false), Some(code), "code {code}");
            }
        }
        assert_eq!(win_to_evdev(0x45, false, true), Some(KEY_PAUSE));
        assert_eq!(win_to_evdev(0x45, true, false), Some(69));
        assert_eq!(name_to_evdev("s"), Some(31));
        assert_eq!(name_to_evdev("escape"), Some(1));
        assert_eq!(name_to_evdev("f12"), Some(88));
        let mut k = KeySet::default();
        k.set(29, true);
        k.set(255, true);
        assert!(k.ctrl() && k.get(255));
        assert_eq!(k.iter().collect::<Vec<_>>(), vec![29, 255]);
    }
}
