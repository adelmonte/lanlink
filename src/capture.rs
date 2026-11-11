//! `lanlink remote <device>`: grab this machine's touchpad, mouse and keyboard
//! and forward everything to another device as remote-input packets.
//! Ctrl+Alt+Esc releases. A notification and a screen border show it's active.

use crate::daemon::Daemon;
use crate::input::{self, SPECIAL};
use crate::notify;
use crate::protocol::{self, Packet};
use anyhow::{Result, bail};
use evdev::{AbsoluteAxisCode, EventSummary, InputEvent, KeyCode, RelativeAxisCode, SynchronizationCode};
use notify_rust::NotificationHandle;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Two-finger scroll distance (screen px) per wheel notch sent.
const SCROLL_STEP: f64 = 30.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Keyboard,
    Touchpad,
    Mouse,
}

struct Note {
    handle: Mutex<Option<NotificationHandle>>,
    stopped: AtomicBool,
}

pub struct Capture {
    pub device: String,
    tasks: Vec<JoinHandle<()>>,
    border: Option<std::process::Child>,
    note: Arc<Note>,
}

fn classify(dev: &evdev::Device) -> Option<Kind> {
    if dev.name().is_some_and(|n| n.starts_with("lanlink")) {
        return None;
    }
    let keys = dev.supported_keys();
    let has = |k: KeyCode| keys.is_some_and(|s| s.contains(k));
    let abs = dev.supported_absolute_axes().is_some_and(|a| a.contains(AbsoluteAxisCode::ABS_X));
    let rel = dev.supported_relative_axes().is_some_and(|r| r.contains(RelativeAxisCode::REL_X));
    let direct = dev.properties().contains(evdev::PropType::DIRECT); // touchscreens
    if has(KeyCode::BTN_TOOL_FINGER) && abs && !direct {
        Some(Kind::Touchpad)
    } else if rel && has(KeyCode::BTN_LEFT) {
        Some(Kind::Mouse)
    } else if has(KeyCode::KEY_A) && has(KeyCode::KEY_Z) && has(KeyCode::KEY_ENTER) {
        Some(Kind::Keyboard)
    } else {
        None
    }
}

fn find_devices() -> Vec<(evdev::Device, Kind)> {
    evdev::enumerate().filter_map(|(_, d)| classify(&d).map(|k| (d, k))).collect()
}

/// Wait until no keys are held, so the compositor sees the release of
/// whatever keybind launched us (otherwise Super gets stuck down).
fn wait_released(devs: &[(evdev::Device, Kind)]) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let held = devs
            .iter()
            .filter(|(_, k)| *k == Kind::Keyboard)
            .any(|(d, _)| d.get_key_state().map(|s| s.iter().next().is_some()).unwrap_or(false));
        if !held {
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

pub async fn start(d: Arc<Daemon>, id: String) -> Result<()> {
    {
        let st = d.lock();
        let Some(dev) = st.devices.get(&id) else { bail!("unknown device") };
        if !dev.accepts(protocol::MOUSEPAD) {
            bail!("{} doesn't accept remote input", dev.name);
        }
    }
    if d.capture_target().is_some() {
        stop(&d);
    }
    let devs = tokio::task::spawn_blocking(|| {
        let devs = find_devices();
        wait_released(&devs);
        devs
    })
    .await?;
    if !devs.iter().any(|(_, k)| *k == Kind::Keyboard) {
        bail!("no keyboard found; is your user in the `input` group?");
    }

    let (tx, rx) = mpsc::unbounded_channel::<(usize, InputEvent)>();
    let mut tasks = vec![];
    let mut pads = vec![];
    for (i, (mut dev, kind)) in devs.into_iter().enumerate() {
        if dev.grab().is_err() {
            continue;
        }
        let pad = if kind == Kind::Touchpad { Some(Pad::new(&dev)) } else { None };
        pads.push((kind, pad));
        let Ok(mut stream) = dev.into_event_stream() else { continue };
        let tx = tx.clone();
        tasks.push(tokio::spawn(async move {
            while let Ok(ev) = stream.next_event().await {
                if tx.send((i, ev)).is_err() {
                    break;
                }
            }
        }));
    }
    drop(tx);

    let name = d.name_of(&id);
    let note = Arc::new(Note { handle: Mutex::new(None), stopped: AtomicBool::new(false) });
    {
        let note = note.clone();
        let body = format!("Keyboard and touchpad go to {name}.\nCtrl+Alt+Esc to release.");
        std::thread::spawn(move || {
            let h = notify::persistent("Remote control active", &body);
            if note.stopped.load(Ordering::SeqCst) {
                if let Some(h) = h {
                    h.close();
                }
            } else {
                *note.handle.lock().unwrap() = h;
            }
        });
    }
    let border = std::env::current_exe().ok().and_then(|exe| {
        std::process::Command::new(exe)
            .arg("border")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()
    });

    let fwd = Forwarder {
        d: d.clone(),
        id: id.clone(),
        mods: input::Mods::default(),
        pads,
        mouse: (0, 0),
        speed: d.cfg.pointer_speed,
        natural: d.cfg.natural_scroll,
    };
    tasks.push(tokio::spawn(fwd.run(rx)));
    *d.capture.lock().unwrap() = Some(Capture { device: id, tasks, border, note });
    eprintln!("lanlink: remote control of {name} started");
    Ok(())
}

pub fn stop(d: &Daemon) {
    let Some(mut c) = d.capture.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    // Aborting drops the event streams, which closes the fds and releases the grabs.
    for t in &c.tasks {
        t.abort();
    }
    if let Some(mut b) = c.border.take() {
        let _ = b.kill();
        std::thread::spawn(move || {
            let _ = b.wait();
        });
    }
    c.note.stopped.store(true, Ordering::SeqCst);
    if let Some(h) = c.note.handle.lock().unwrap().take() {
        std::thread::spawn(move || h.close());
    }
    eprintln!("lanlink: remote control released");
}

/// Minimal touchpad driver: one finger moves, two scroll, taps click.
struct Pad {
    res: (f64, f64),
    cur: (i32, i32),
    prev: Option<(i32, i32)>,
    fingers: u8,
    prev_fingers: u8,
    max_fingers: u8,
    touching: bool,
    was_touching: bool,
    started: Instant,
    moved_mm: f64,
    clicked: bool,
    scroll_acc: f64,
    button: Option<&'static str>,
}

impl Pad {
    fn new(dev: &evdev::Device) -> Self {
        let mut res = (0.0, 0.0);
        if let Ok(info) = dev.get_absinfo() {
            for (axis, i) in info {
                let r = if i.resolution() > 0 { i.resolution() as f64 } else { (i.maximum() - i.minimum()).max(1) as f64 / 100.0 };
                if axis == AbsoluteAxisCode::ABS_X {
                    res.0 = r;
                } else if axis == AbsoluteAxisCode::ABS_Y {
                    res.1 = r;
                }
            }
        }
        Pad {
            res: (res.0.max(1.0), res.1.max(1.0)),
            cur: (0, 0),
            prev: None,
            fingers: 0,
            prev_fingers: 0,
            max_fingers: 0,
            touching: false,
            was_touching: false,
            started: Instant::now(),
            moved_mm: 0.0,
            clicked: false,
            scroll_acc: 0.0,
            button: None,
        }
    }
}

struct Forwarder {
    d: Arc<Daemon>,
    id: String,
    mods: input::Mods,
    pads: Vec<(Kind, Option<Pad>)>,
    mouse: (i32, i32),
    speed: f64,
    natural: bool,
}

impl Forwarder {
    fn send(&self, body: Value) {
        let _ = self.d.send(&self.id, Packet::new(protocol::MOUSEPAD, body));
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<(usize, InputEvent)>) {
        while let Some((i, ev)) = rx.recv().await {
            let kind = self.pads.get(i).map(|p| p.0);
            let release = match kind {
                Some(Kind::Keyboard) => self.keyboard(ev),
                Some(Kind::Touchpad) => {
                    self.touchpad(i, ev);
                    false
                }
                Some(Kind::Mouse) => {
                    self.mouse(ev);
                    false
                }
                None => false,
            };
            if release {
                stop(&self.d);
                return;
            }
        }
        // Every device vanished (unplugged, or we were stopped).
        stop(&self.d);
    }

    /// Returns true on the release combo.
    fn keyboard(&mut self, ev: InputEvent) -> bool {
        let EventSummary::Key(_, code, value) = ev.destructure() else { return false };
        let down = value != 0;
        use KeyCode as K;
        match code {
            K::KEY_LEFTSHIFT | K::KEY_RIGHTSHIFT => self.mods.shift = down,
            K::KEY_LEFTCTRL | K::KEY_RIGHTCTRL => self.mods.ctrl = down,
            K::KEY_LEFTALT | K::KEY_RIGHTALT => self.mods.alt = down,
            K::KEY_LEFTMETA | K::KEY_RIGHTMETA => self.mods.meta = down,
            _ if !down => {}
            K::KEY_ESC if self.mods.ctrl && self.mods.alt => return true,
            _ => {
                let m = self.mods;
                let combo = m.ctrl || m.alt || m.meta;
                if let Some((n, _)) = SPECIAL.iter().find(|(_, k)| *k == code) {
                    self.send(json!({"specialKey": n, "shift": m.shift, "ctrl": m.ctrl, "alt": m.alt, "super": m.meta}));
                } else if let Some(c) = input::key_char(code, m.shift) {
                    // Plain typing sends the already-shifted character; combos also flag shift.
                    self.send(json!({"key": c.to_string(), "shift": combo && m.shift, "ctrl": m.ctrl, "alt": m.alt, "super": m.meta}));
                }
            }
        }
        false
    }

    fn mouse(&mut self, ev: InputEvent) {
        match ev.destructure() {
            EventSummary::RelativeAxis(_, RelativeAxisCode::REL_X, v) => self.mouse.0 += v,
            EventSummary::RelativeAxis(_, RelativeAxisCode::REL_Y, v) => self.mouse.1 += v,
            EventSummary::RelativeAxis(_, RelativeAxisCode::REL_WHEEL, v) if v != 0 => {
                for _ in 0..v.unsigned_abs() {
                    self.send(json!({"scroll": true, "dx": 0, "dy": if v > 0 { 10 } else { -10 }}));
                }
            }
            EventSummary::Key(_, KeyCode::BTN_LEFT, 1) => self.send(json!({"singlehold": true})),
            EventSummary::Key(_, KeyCode::BTN_LEFT, 0) => self.send(json!({"singlerelease": true})),
            EventSummary::Key(_, KeyCode::BTN_RIGHT, 1) => self.send(json!({"rightclick": true})),
            EventSummary::Key(_, KeyCode::BTN_MIDDLE, 1) => self.send(json!({"middleclick": true})),
            EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
                let (x, y) = std::mem::take(&mut self.mouse);
                if x != 0 || y != 0 {
                    self.send(json!({"dx": x as f64 * self.speed, "dy": y as f64 * self.speed}));
                }
            }
            _ => {}
        }
    }

    fn touchpad(&mut self, i: usize, ev: InputEvent) {
        let (speed, natural) = (self.speed, self.natural);
        let Some((_, Some(p))) = self.pads.get_mut(i) else { return };
        let mut out: Vec<Value> = vec![];
        match ev.destructure() {
            EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_X, v) => p.cur.0 = v,
            EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_Y, v) => p.cur.1 = v,
            EventSummary::Key(_, KeyCode::BTN_TOUCH, v) => p.touching = v != 0,
            EventSummary::Key(_, code, v) if tool_fingers(code).is_some() => {
                let n = tool_fingers(code).unwrap();
                if v != 0 {
                    p.fingers = n;
                } else if p.fingers == n {
                    p.fingers = 0;
                }
            }
            EventSummary::Key(_, KeyCode::BTN_LEFT | KeyCode::BTN_RIGHT, v) => {
                // Clickpad press: two fingers down (or the right button) means right click.
                p.clicked = true;
                if v == 1 {
                    if p.fingers >= 2 || ev.code() == KeyCode::BTN_RIGHT.0 {
                        out.push(json!({"rightclick": true}));
                        p.button = Some("right");
                    } else {
                        out.push(json!({"singlehold": true}));
                        p.button = Some("left");
                    }
                } else if v == 0 && p.button.take() == Some("left") {
                    out.push(json!({"singlerelease": true}));
                }
            }
            EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
                if p.touching {
                    if !p.was_touching {
                        p.started = Instant::now();
                        p.moved_mm = 0.0;
                        p.max_fingers = 0;
                        p.clicked = false;
                        p.scroll_acc = 0.0;
                        p.prev = None;
                    }
                    let fingers = p.fingers.max(1);
                    p.max_fingers = p.max_fingers.max(fingers);
                    if fingers != p.prev_fingers {
                        p.prev = None; // the reported position jumps when fingers change
                    }
                    if let Some(prev) = p.prev {
                        let mx = (p.cur.0 - prev.0) as f64 / p.res.0;
                        let my = (p.cur.1 - prev.1) as f64 / p.res.1;
                        let dist = (mx * mx + my * my).sqrt();
                        p.moved_mm += dist;
                        // Simple acceleration: 4 px/mm slow, up to 12 px/mm fast.
                        let gain = (4.0 + 8.0 * (dist / 1.5).min(1.0)) * speed;
                        match fingers {
                            1 => {
                                if mx != 0.0 || my != 0.0 {
                                    out.push(json!({"dx": mx * gain, "dy": my * gain}));
                                }
                            }
                            2 => {
                                p.scroll_acc += my * 8.0 * speed;
                                while p.scroll_acc.abs() >= SCROLL_STEP {
                                    let s = p.scroll_acc.signum();
                                    p.scroll_acc -= s * SCROLL_STEP;
                                    // Finger down = wheel up unless natural scrolling.
                                    let up = (s > 0.0) != natural;
                                    out.push(json!({"scroll": true, "dx": 0, "dy": if up { 10 } else { -10 }}));
                                }
                            }
                            _ => {}
                        }
                    }
                    p.prev = Some(p.cur);
                    p.prev_fingers = fingers;
                } else if p.was_touching {
                    if !p.clicked && p.started.elapsed() < Duration::from_millis(200) && p.moved_mm < 2.0 {
                        match p.max_fingers {
                            1 => out.push(json!({"singleclick": true})),
                            2 => out.push(json!({"rightclick": true})),
                            3 => out.push(json!({"middleclick": true})),
                            _ => {}
                        }
                    }
                    p.prev = None;
                    p.prev_fingers = 0;
                }
                p.was_touching = p.touching;
            }
            _ => {}
        }
        for b in out {
            self.send(b);
        }
    }
}

fn tool_fingers(code: KeyCode) -> Option<u8> {
    match code {
        KeyCode::BTN_TOOL_FINGER => Some(1),
        KeyCode::BTN_TOOL_DOUBLETAP => Some(2),
        KeyCode::BTN_TOOL_TRIPLETAP => Some(3),
        KeyCode::BTN_TOOL_QUADTAP => Some(4),
        KeyCode::BTN_TOOL_QUINTTAP => Some(5),
        _ => None,
    }
}
