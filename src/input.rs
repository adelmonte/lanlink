//! Receiving remote input (kdeconnect.mousepad.request): inject it through
//! virtual uinput devices, so it works under any Wayland compositor or X11.
//! Also holds the key tables shared with the sending side (capture.rs).

use crate::daemon::Daemon;
use crate::notify;
use crate::protocol::{self, Packet};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode};
use std::sync::atomic::{AtomicBool, Ordering};

/// KDE Connect's `specialKey` numbers.
pub const SPECIAL: &[(i64, KeyCode)] = &[
    (1, KeyCode::KEY_BACKSPACE),
    (2, KeyCode::KEY_TAB),
    (3, KeyCode::KEY_ENTER),
    (4, KeyCode::KEY_LEFT),
    (5, KeyCode::KEY_UP),
    (6, KeyCode::KEY_RIGHT),
    (7, KeyCode::KEY_DOWN),
    (8, KeyCode::KEY_PAGEUP),
    (9, KeyCode::KEY_PAGEDOWN),
    (10, KeyCode::KEY_HOME),
    (11, KeyCode::KEY_END),
    (12, KeyCode::KEY_ENTER),
    (13, KeyCode::KEY_DELETE),
    (14, KeyCode::KEY_ESC),
    (15, KeyCode::KEY_SYSRQ),
    (16, KeyCode::KEY_SCROLLLOCK),
    (21, KeyCode::KEY_F1),
    (22, KeyCode::KEY_F2),
    (23, KeyCode::KEY_F3),
    (24, KeyCode::KEY_F4),
    (25, KeyCode::KEY_F5),
    (26, KeyCode::KEY_F6),
    (27, KeyCode::KEY_F7),
    (28, KeyCode::KEY_F8),
    (29, KeyCode::KEY_F9),
    (30, KeyCode::KEY_F10),
    (31, KeyCode::KEY_F11),
    (32, KeyCode::KEY_F12),
];

/// Character -> (key, needs shift) on a US layout.
pub fn char_key(c: char) -> Option<(KeyCode, bool)> {
    const ROWS: [(&str, u16); 3] = [("qwertyuiop", 16), ("asdfghjkl", 30), ("zxcvbnm", 44)];
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        for (row, start) in ROWS {
            if let Some(i) = row.find(lower) {
                return Some((KeyCode(start + i as u16), c.is_ascii_uppercase()));
            }
        }
    }
    let k = |code: KeyCode, shift: bool| Some((code, shift));
    use KeyCode as K;
    match c {
        '1'..='9' => k(KeyCode(2 + (c as u16 - '1' as u16)), false),
        '0' => k(K::KEY_0, false),
        '!' => k(K::KEY_1, true),
        '@' => k(K::KEY_2, true),
        '#' => k(K::KEY_3, true),
        '$' => k(K::KEY_4, true),
        '%' => k(K::KEY_5, true),
        '^' => k(K::KEY_6, true),
        '&' => k(K::KEY_7, true),
        '*' => k(K::KEY_8, true),
        '(' => k(K::KEY_9, true),
        ')' => k(K::KEY_0, true),
        ' ' => k(K::KEY_SPACE, false),
        '\n' => k(K::KEY_ENTER, false),
        '\t' => k(K::KEY_TAB, false),
        '-' => k(K::KEY_MINUS, false),
        '_' => k(K::KEY_MINUS, true),
        '=' => k(K::KEY_EQUAL, false),
        '+' => k(K::KEY_EQUAL, true),
        '[' => k(K::KEY_LEFTBRACE, false),
        '{' => k(K::KEY_LEFTBRACE, true),
        ']' => k(K::KEY_RIGHTBRACE, false),
        '}' => k(K::KEY_RIGHTBRACE, true),
        '\\' => k(K::KEY_BACKSLASH, false),
        '|' => k(K::KEY_BACKSLASH, true),
        ';' => k(K::KEY_SEMICOLON, false),
        ':' => k(K::KEY_SEMICOLON, true),
        '\'' => k(K::KEY_APOSTROPHE, false),
        '"' => k(K::KEY_APOSTROPHE, true),
        '`' => k(K::KEY_GRAVE, false),
        '~' => k(K::KEY_GRAVE, true),
        ',' => k(K::KEY_COMMA, false),
        '<' => k(K::KEY_COMMA, true),
        '.' => k(K::KEY_DOT, false),
        '>' => k(K::KEY_DOT, true),
        '/' => k(K::KEY_SLASH, false),
        '?' => k(K::KEY_SLASH, true),
        _ => None,
    }
}

/// Reverse of `char_key`, for the sending side. Space/Enter/Tab are left to
/// special keys (Enter/Tab) or sent as " ".
pub fn key_char(code: KeyCode, shift: bool) -> Option<char> {
    (32u8..127)
        .map(char::from)
        .find(|&c| char_key(c) == Some((code, shift)))
}

#[derive(Default, Clone, Copy)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

pub struct Injector {
    mouse: VirtualDevice,
    kbd: VirtualDevice,
    acc: (f64, f64),
}

fn ev(kind: EventType, code: u16, value: i32) -> InputEvent {
    InputEvent::new(kind.0, code, value)
}

impl Injector {
    pub fn new() -> std::io::Result<Self> {
        let mut buttons = AttributeSet::<KeyCode>::new();
        for b in [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE] {
            buttons.insert(b);
        }
        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        for a in [RelativeAxisCode::REL_X, RelativeAxisCode::REL_Y, RelativeAxisCode::REL_WHEEL, RelativeAxisCode::REL_HWHEEL] {
            axes.insert(a);
        }
        let mouse = VirtualDevice::builder()?
            .name("lanlink virtual pointer")
            .with_keys(&buttons)?
            .with_relative_axes(&axes)?
            .build()?;
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=248u16 {
            keys.insert(KeyCode(code));
        }
        let kbd = VirtualDevice::builder()?.name("lanlink virtual keyboard").with_keys(&keys)?.build()?;
        Ok(Injector { mouse, kbd, acc: (0.0, 0.0) })
    }

    fn button(&mut self, b: KeyCode, down: bool) {
        let _ = self.mouse.emit(&[ev(EventType::KEY, b.0, down as i32)]);
    }

    fn click(&mut self, b: KeyCode) {
        self.button(b, true);
        self.button(b, false);
    }

    fn move_by(&mut self, dx: f64, dy: f64) {
        self.acc.0 += dx;
        self.acc.1 += dy;
        let (x, y) = (self.acc.0.trunc(), self.acc.1.trunc());
        self.acc.0 -= x;
        self.acc.1 -= y;
        if x != 0.0 || y != 0.0 {
            let _ = self.mouse.emit(&[
                ev(EventType::RELATIVE, RelativeAxisCode::REL_X.0, x as i32),
                ev(EventType::RELATIVE, RelativeAxisCode::REL_Y.0, y as i32),
            ]);
        }
    }

    /// One wheel notch per packet, like KDE Connect's X11 backend.
    fn scroll(&mut self, dx: f64, dy: f64, notches: i32) {
        if dy != 0.0 {
            let v = if dy > 0.0 { notches } else { -notches };
            let _ = self.mouse.emit(&[ev(EventType::RELATIVE, RelativeAxisCode::REL_WHEEL.0, v)]);
        } else if dx != 0.0 {
            let v = if dx > 0.0 { notches } else { -notches };
            let _ = self.mouse.emit(&[ev(EventType::RELATIVE, RelativeAxisCode::REL_HWHEEL.0, v)]);
        }
    }

    fn key(&mut self, code: KeyCode, m: Mods) {
        let mods: Vec<KeyCode> = [
            (m.ctrl, KeyCode::KEY_LEFTCTRL),
            (m.alt, KeyCode::KEY_LEFTALT),
            (m.shift, KeyCode::KEY_LEFTSHIFT),
            (m.meta, KeyCode::KEY_LEFTMETA),
        ]
        .into_iter()
        .filter_map(|(on, k)| on.then_some(k))
        .collect();
        for k in &mods {
            let _ = self.kbd.emit(&[ev(EventType::KEY, k.0, 1)]);
        }
        let _ = self.kbd.emit(&[ev(EventType::KEY, code.0, 1)]);
        let _ = self.kbd.emit(&[ev(EventType::KEY, code.0, 0)]);
        for k in mods.iter().rev() {
            let _ = self.kbd.emit(&[ev(EventType::KEY, k.0, 0)]);
        }
    }
}

static WARNED: AtomicBool = AtomicBool::new(false);

pub fn handle(d: &Daemon, id: &str, p: &Packet) {
    let mut guard = d.input.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        match Injector::new() {
            Ok(i) => *guard = Some(i),
            Err(e) => {
                if !WARNED.swap(true, Ordering::Relaxed) {
                    notify::show("Remote input unavailable", format!("Can't open /dev/uinput: {e}. See the README for the udev rule."));
                }
                return;
            }
        }
    }
    let inj = guard.as_mut().expect("just set");
    let mods = Mods { shift: p.bool("shift"), ctrl: p.bool("ctrl"), alt: p.bool("alt"), meta: p.bool("super") };
    let speed = d.cfg.pointer_speed;

    if p.bool("singleclick") {
        inj.click(KeyCode::BTN_LEFT);
    } else if p.bool("doubleclick") {
        inj.click(KeyCode::BTN_LEFT);
        inj.click(KeyCode::BTN_LEFT);
    } else if p.bool("middleclick") {
        inj.click(KeyCode::BTN_MIDDLE);
    } else if p.bool("rightclick") {
        inj.click(KeyCode::BTN_RIGHT);
    } else if p.bool("singlehold") {
        inj.button(KeyCode::BTN_LEFT, true);
    } else if p.bool("singlerelease") {
        inj.button(KeyCode::BTN_LEFT, false);
    } else if p.bool("scroll") {
        let sign = if d.cfg.invert_scroll { -1.0 } else { 1.0 };
        let notches = d.cfg.scroll_speed.round().max(1.0) as i32;
        inj.scroll(p.f64("dx").unwrap_or(0.0) * sign, p.f64("dy").unwrap_or(0.0) * sign, notches);
    } else if let Some(sk) = p.i64("specialKey").filter(|k| *k != 0) {
        if let Some((_, code)) = SPECIAL.iter().find(|(n, _)| *n == sk) {
            inj.key(*code, mods);
        }
    } else if let Some(text) = p.str("key").filter(|k| !k.is_empty()) {
        for c in text.chars() {
            if let Some((code, shift)) = char_key(c) {
                inj.key(code, Mods { shift: mods.shift || shift, ..mods });
            }
        }
    } else {
        inj.move_by(p.f64("dx").unwrap_or(0.0) * speed, p.f64("dy").unwrap_or(0.0) * speed);
    }
    drop(guard);

    if p.bool("sendAck") {
        let mut body = p.body.clone();
        body["isAck"] = serde_json::json!(true);
        let _ = d.send(id, Packet::new(protocol::MOUSEPAD_ECHO, body));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_roundtrip() {
        for c in (32u8..127).map(char::from) {
            let (code, shift) = char_key(c).unwrap_or_else(|| panic!("no key for {c:?}"));
            assert_eq!(key_char(code, shift), Some(c), "{c:?}");
        }
        assert_eq!(char_key('A'), Some((KeyCode::KEY_A, true)));
        assert_eq!(char_key('m'), Some((KeyCode::KEY_M, false)));
        assert_eq!(char_key('1'), Some((KeyCode::KEY_1, false)));
    }
}
