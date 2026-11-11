//! `lanlink` with no arguments: a small TUI over the daemon's IPC socket.

use crate::ipc::{self, Request};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub fn state_text(d: &Value) -> String {
    let b = |k: &str| d[k].as_bool().unwrap_or(false);
    let mut s = match (b("paired"), b("online"), d["pair"].as_str().unwrap_or("none")) {
        (_, _, "incoming") => "wants to pair".to_string(),
        (_, _, "requested") => "pairing...".to_string(),
        (true, true, _) => "connected".to_string(),
        (true, false, _) => "offline".to_string(),
        (false, _, _) => "not paired".to_string(),
    };
    if d["mount"].is_string() {
        s.push_str(", mounted");
    }
    if b("remote") {
        s.push_str(", controlling");
    }
    s
}

enum Mode {
    Normal,
    Help,
    ConfirmUnpair(String, String),
    Connect(String),
    Picker(Picker),
}

struct Picker {
    device: String,
    dir: PathBuf,
    entries: Vec<(String, bool)>,
    state: ListState,
    hidden: bool,
}

impl Picker {
    fn new(device: String) -> Self {
        let mut p = Picker {
            device,
            dir: dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
            entries: vec![],
            state: ListState::default(),
            hidden: false,
        };
        p.load();
        p
    }

    fn load(&mut self) {
        let mut e: Vec<(String, bool)> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if !self.hidden && name.starts_with('.') {
                            return None;
                        }
                        let is_dir = e.path().is_dir();
                        Some((name, is_dir))
                    })
                    .collect()
            })
            .unwrap_or_default();
        e.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        if self.dir.parent().is_some() {
            e.insert(0, ("..".into(), true));
        }
        self.entries = e;
        self.state.select(if self.entries.is_empty() { None } else { Some(0) });
    }
}

struct App {
    status: Value,
    error: Option<String>,
    list: ListState,
    mode: Mode,
    msg: Option<(String, Instant)>,
    quit: bool,
}

pub fn run() -> Result<()> {
    let mut term = ratatui::init();
    let res = App::new().main_loop(&mut term);
    ratatui::restore();
    res
}

impl App {
    fn new() -> Self {
        App {
            status: Value::Null,
            error: None,
            list: ListState::default().with_selected(Some(0)),
            mode: Mode::Normal,
            msg: None,
            quit: false,
        }
    }

    fn refresh(&mut self) {
        match ipc::call(&Request::Status) {
            Ok(s) => {
                self.status = s;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        let n = self.devices().len();
        match self.list.selected() {
            _ if n == 0 => self.list.select(None),
            None => self.list.select(Some(0)),
            Some(i) if i >= n => self.list.select(Some(n - 1)),
            _ => {}
        }
    }

    fn devices(&self) -> Vec<Value> {
        self.status["devices"].as_array().cloned().unwrap_or_default()
    }

    fn selected(&self) -> Option<Value> {
        self.list.selected().and_then(|i| self.devices().get(i).cloned())
    }

    fn say(&mut self, m: impl Into<String>) {
        self.msg = Some((m.into(), Instant::now()));
    }

    fn act(&mut self, req: Request, ok: &str) {
        match ipc::call(&req) {
            Ok(v) => {
                let code = v["code"].as_str().map(|c| format!(" Code: {c}")).unwrap_or_default();
                self.say(format!("{ok}{code}"));
            }
            Err(e) => self.say(format!("{e:#}")),
        }
        self.refresh();
    }

    fn main_loop(&mut self, term: &mut DefaultTerminal) -> Result<()> {
        let mut last = Instant::now() - Duration::from_secs(10);
        while !self.quit {
            if last.elapsed() >= Duration::from_millis(500) {
                self.refresh();
                last = Instant::now();
            }
            term.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(200))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                self.key(k);
            }
        }
        Ok(())
    }

    fn key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.key_normal(k),
            Mode::Help => {}
            Mode::ConfirmUnpair(id, name) => {
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Enter) {
                    self.act(Request::Unpair { device: id }, &format!("Unpaired {name}."));
                }
            }
            Mode::Connect(mut s) => match k.code {
                KeyCode::Enter if !s.is_empty() => self.act(Request::Connect { addr: s }, "Connecting..."),
                KeyCode::Esc => {}
                KeyCode::Backspace => {
                    s.pop();
                    self.mode = Mode::Connect(s);
                }
                KeyCode::Char(c) => {
                    s.push(c);
                    self.mode = Mode::Connect(s);
                }
                _ => self.mode = Mode::Connect(s),
            },
            Mode::Picker(p) => self.key_picker(p, k),
        }
    }

    fn key_normal(&mut self, k: KeyEvent) {
        let n = self.devices().len();
        let sel = self.selected();
        let id = sel.as_ref().and_then(|d| d["id"].as_str()).map(String::from);
        let name = sel.as_ref().and_then(|d| d["name"].as_str()).unwrap_or("").to_string();
        let b = |k: &str| sel.as_ref().and_then(|d| d[k].as_bool()).unwrap_or(false);
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Down | KeyCode::Char('j') if n > 0 => {
                self.list.select(Some((self.list.selected().unwrap_or(0) + 1).min(n - 1)))
            }
            KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                self.list.select(Some(self.list.selected().unwrap_or(0).saturating_sub(1)))
            }
            KeyCode::Char('c') => self.mode = Mode::Connect(String::new()),
            KeyCode::Char('R') => self.act(Request::Release, "Released."),
            _ => {
                let Some(id) = id else { return };
                match k.code {
                    KeyCode::Char('p') => self.act(Request::Pair { device: id }, "Pair request sent."),
                    KeyCode::Char('a') => self.act(Request::Accept { device: id }, &format!("Paired with {name}.")),
                    KeyCode::Char('x') => self.act(Request::Reject { device: id }, "Rejected."),
                    KeyCode::Char('u') if b("paired") => self.mode = Mode::ConfirmUnpair(id, name),
                    KeyCode::Char('m') if sel.as_ref().is_some_and(|d| d["mount"].is_string()) => {
                        self.act(Request::Unmount { device: id }, "Unmounted.")
                    }
                    KeyCode::Char('m') => self.act(Request::Mount { device: id }, "Asked the phone to share its files..."),
                    KeyCode::Char('o') => {
                        if let Some(m) = sel.as_ref().and_then(|d| d["mount"].as_str()) {
                            let _ = std::process::Command::new("xdg-open")
                                .arg(m)
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn();
                        } else {
                            self.say("Not mounted; press m first.");
                        }
                    }
                    KeyCode::Char('s') => self.mode = Mode::Picker(Picker::new(id)),
                    KeyCode::Char('r') if b("remote") => self.act(Request::Release, "Released."),
                    KeyCode::Char('r') => {
                        self.act(Request::Remote { device: id }, "Remote control on. Ctrl+Alt+Esc to release.")
                    }
                    KeyCode::Char('P') => self.act(Request::Ping { device: id }, "Pinged."),
                    _ => {}
                }
            }
        }
    }

    fn key_picker(&mut self, mut p: Picker, k: KeyEvent) {
        let n = p.entries.len();
        let cur = p.state.selected().and_then(|i| p.entries.get(i).cloned());
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return,
            KeyCode::Down | KeyCode::Char('j') if n > 0 => p.state.select(Some((p.state.selected().unwrap_or(0) + 1).min(n - 1))),
            KeyCode::Up | KeyCode::Char('k') => p.state.select(Some(p.state.selected().unwrap_or(0).saturating_sub(1))),
            KeyCode::Char('.') => {
                p.hidden = !p.hidden;
                p.load();
            }
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                if let Some(parent) = p.dir.parent() {
                    p.dir = parent.to_path_buf();
                    p.load();
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some((name, is_dir)) = cur {
                    let path = if name == ".." { p.dir.parent().unwrap_or(&p.dir).to_path_buf() } else { p.dir.join(&name) };
                    if is_dir {
                        p.dir = path;
                        p.load();
                    } else {
                        let items = vec![path.to_string_lossy().into_owned()];
                        self.act(Request::Send { device: p.device, items }, &format!("Sending {name}..."));
                        return;
                    }
                }
            }
            _ => {}
        }
        self.mode = Mode::Picker(p);
    }

    // ---- drawing -----------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let [head, body, transfers, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(7),
            Constraint::Length(1),
        ])
        .areas(f.area());

        let me = &self.status["self"];
        f.render_widget(
            Line::from(vec![
                " lanlink ".bold().black().on_cyan(),
                format!("  {} ({})", me["name"].as_str().unwrap_or("?"), me["type"].as_str().unwrap_or("")).into(),
            ]),
            head,
        );

        if let Some(e) = &self.error {
            let p = Paragraph::new(vec![
                Line::from("Can't reach the daemon.".red().bold()),
                Line::from(""),
                Line::from(e.as_str()),
                Line::from(""),
                Line::from("Start it with:  lanlink daemon"),
            ])
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Devices "));
            f.render_widget(p, body);
        } else {
            self.draw_body(f, body);
        }
        self.draw_transfers(f, transfers);

        let help = "j/k move  p pair  s send  m mount  o open  r remote  u unpair  c connect  ? help  q quit";
        let line = match &self.msg {
            Some((m, t)) if t.elapsed() < Duration::from_secs(6) => Line::from(format!(" {m}")).yellow(),
            _ => Line::from(format!(" {help}")).fg(Color::Indexed(248)),
        };
        f.render_widget(line, foot);

        self.draw_pair_popup(f);
        match &mut self.mode {
            Mode::Normal => {}
            Mode::Help => draw_help(f),
            Mode::ConfirmUnpair(_, name) => {
                let area = popup(f.area(), 44, 5);
                f.render_widget(Clear, area);
                f.render_widget(
                    Paragraph::new(format!("Unpair {name}?\n\ny = yes, anything else = cancel"))
                        .block(Block::bordered().title(" Unpair ")),
                    area,
                );
            }
            Mode::Connect(s) => {
                let area = popup(f.area(), 50, 5);
                f.render_widget(Clear, area);
                f.render_widget(
                    Paragraph::new(format!("IP address (port optional):\n\n> {s}_"))
                        .block(Block::bordered().title(" Connect ")),
                    area,
                );
            }
            Mode::Picker(p) => {
                let area = popup(f.area(), 70, 22);
                f.render_widget(Clear, area);
                let items: Vec<ListItem> = p
                    .entries
                    .iter()
                    .map(|(n, d)| if *d { ListItem::new(format!("{n}/")).blue() } else { ListItem::new(n.as_str()) })
                    .collect();
                let list = List::new(items)
                    .block(
                        Block::bordered()
                            .title(format!(" Send: {} ", p.dir.display()))
                            .title_bottom(" enter open/send  backspace up  . hidden  esc cancel "),
                    )
                    .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
                f.render_stateful_widget(list, area, &mut p.state);
            }
        }
    }

    fn draw_body(&mut self, f: &mut Frame, area: Rect) {
        let [left, right] = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(area);
        let devs = self.devices();
        let items: Vec<ListItem> = devs
            .iter()
            .map(|d| {
                let b = |k: &str| d[k].as_bool().unwrap_or(false);
                let (dot, color) = match (b("paired"), b("online")) {
                    (true, true) => ("●", Color::Green),
                    (true, false) => ("○", Color::Indexed(248)),
                    (false, _) => ("◌", Color::Yellow),
                };
                let mut spans = vec![
                    Span::styled(format!(" {dot} "), Style::new().fg(color)),
                    Span::raw(d["name"].as_str().unwrap_or("?").to_string()),
                ];
                if d["mount"].is_string() {
                    spans.push(" [mounted]".cyan());
                }
                if b("remote") {
                    spans.push(" [remote]".magenta());
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let title = if devs.is_empty() { " Devices (none yet) " } else { " Devices " };
        let list = List::new(items)
            .block(Block::bordered().title(title))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, left, &mut self.list);

        let lines = match self.selected() {
            None => vec![
                Line::from("Nothing found yet."),
                Line::from(""),
                Line::from("Open the lanlink app on your phone (same Wi-Fi),"),
                Line::from("or press c to connect by IP."),
            ],
            Some(d) => detail_lines(&d),
        };
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::bordered().title(" Details ")), right);
    }

    fn draw_transfers(&self, f: &mut Frame, area: Rect) {
        let ts = self.status["transfers"].as_array().cloned().unwrap_or_default();
        let lines: Vec<Line> = ts
            .iter()
            .take(area.height.saturating_sub(2) as usize)
            .map(|t| {
                let arrow = if t["incoming"].as_bool().unwrap_or(false) { "↓ from" } else { "↑ to" };
                let (done, total) = (t["done"].as_i64().unwrap_or(0), t["total"].as_i64().unwrap_or(-1));
                let status = t["status"].as_str().unwrap_or("");
                let progress = match status {
                    "active" if total > 0 => format!("{}%  {} / {}", done * 100 / total, human(done), human(total)),
                    "active" => human(done),
                    s => s.to_string(),
                };
                let style = match status {
                    "done" => Style::new().fg(Color::Green),
                    "active" => Style::new().fg(Color::Yellow),
                    _ => Style::new().fg(Color::Red),
                };
                Line::from(vec![
                    Span::raw(format!(" {arrow} {}: ", t["device"].as_str().unwrap_or(""))),
                    Span::raw(t["name"].as_str().unwrap_or("").to_string()).bold(),
                    Span::raw("  "),
                    Span::styled(progress, style),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" Transfers ")), area);
    }

    fn draw_pair_popup(&self, f: &mut Frame) {
        let Some(d) = self.devices().into_iter().find(|d| d["pair"] == "incoming") else { return };
        let area = popup(f.area(), 52, 8);
        f.render_widget(Clear, area);
        let text = vec![
            Line::from(format!("{} wants to pair.", d["name"].as_str().unwrap_or("?"))),
            Line::from(""),
            Line::from(vec!["Code: ".into(), d["code"].as_str().unwrap_or("?").to_string().bold().yellow()]),
            Line::from("Make sure the phone shows the same code."),
            Line::from(""),
            Line::from("select it and press a = accept, x = reject").fg(Color::Indexed(248)),
        ];
        f.render_widget(Paragraph::new(text).block(Block::bordered().title(" Pair request ").yellow()), area);
    }
}

fn detail_lines(d: &Value) -> Vec<Line<'static>> {
    let s = |k: &str| d[k].as_str().unwrap_or("").to_string();
    let b = |k: &str| d[k].as_bool().unwrap_or(false);
    let row = |label: &str, value: String| Line::from(vec![format!("{label:<12}").fg(Color::Indexed(248)), Span::raw(value)]);
    let mut out = vec![
        Line::from(s("name")).bold(),
        Line::from(""),
        row("Status", state_text(d)),
        row("Type", s("type")),
    ];
    if let Some(ip) = d["ip"].as_str() {
        out.push(row("Address", ip.to_string()));
    }
    if let Some(bat) = d["battery"].as_i64() {
        let ch = if b("charging") { " (charging)" } else { "" };
        out.push(row("Battery", format!("{bat}%{ch}")));
    }
    if let Some(m) = d["mount"].as_str() {
        out.push(row("Mounted at", m.to_string()));
    }
    if let Some(c) = d["code"].as_str() {
        out.push(row("Pair code", c.to_string()));
    }
    out.push(row("Fingerprint", s("fingerprint")));
    out.push(row("ID", s("id")));
    out.push(Line::from(""));

    let mut keys: Vec<&str> = vec![];
    match (b("paired"), b("online"), d["pair"].as_str().unwrap_or("")) {
        (_, _, "incoming") => keys.extend(["a accept", "x reject"]),
        (false, true, _) => keys.push("p pair"),
        (true, true, _) => {
            keys.extend(["s send file", if d["mount"].is_string() { "m unmount" } else { "m browse" }]);
            if d["mount"].is_string() {
                keys.push("o open");
            }
            if b("can_remote") {
                keys.push(if b("remote") { "r release" } else { "r remote control" });
            }
            keys.extend(["P ping", "u unpair"]);
        }
        (true, false, _) => keys.push("u unpair"),
        _ => {}
    }
    out.push(Line::from(keys.join("   ")).cyan());
    out
}

fn draw_help(f: &mut Frame) {
    let area = popup(f.area(), 60, 20);
    f.render_widget(Clear, area);
    let text = "\
j / k, arrows   move
p               pair with selected device
a / x           accept / reject a pair request
u               unpair
s               send a file
m               mount / unmount the phone's storage
o               open the mount in your file manager
r               control the device with this keyboard
                and touchpad (Ctrl+Alt+Esc releases)
R               release remote control
P               ping
c               connect by IP address
q               quit (the daemon keeps running)

Received files go to your Downloads folder.";
    f.render_widget(Paragraph::new(text).block(Block::bordered().title(" Help (any key closes) ")), area);
}

fn popup(area: Rect, w: u16, h: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(h.min(area.height))]).flex(Flex::Center).areas(area);
    let [r] = Layout::horizontal([Constraint::Length(w.min(area.width))]).flex(Flex::Center).areas(v);
    r
}

fn human(n: i64) -> String {
    let n = n.max(0) as f64;
    match n {
        n if n >= 1e9 => format!("{:.1} GB", n / 1e9),
        n if n >= 1e6 => format!("{:.1} MB", n / 1e6),
        n if n >= 1e3 => format!("{:.0} KB", n / 1e3),
        n => format!("{n} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn renders() {
        let mut app = App::new();
        app.status = serde_json::json!({
            "self": {"name": "box", "type": "desktop"},
            "devices": [
                {"id": "a", "name": "Pixel", "type": "phone", "paired": true, "online": true, "pair": "none",
                 "battery": 80, "charging": true, "mount": "/home/u/Phone/Pixel", "can_remote": true, "fingerprint": "AB"},
                {"id": "b", "name": "tv", "type": "tv", "paired": false, "online": true, "pair": "incoming", "code": "1234ABCD"}
            ],
            "transfers": [{"device": "Pixel", "name": "x.jpg", "incoming": true, "total": 100, "done": 50, "status": "active"}]
        });
        app.list.select(Some(0));
        for mode in [Mode::Normal, Mode::Help, Mode::Connect("10.0.0".into()), Mode::Picker(Picker::new("a".into()))] {
            app.mode = mode;
            for (w, h) in [(100, 30), (40, 12), (10, 5)] {
                let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
                t.draw(|f| app.draw(f)).unwrap();
            }
        }
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        app.mode = Mode::Normal;
        t.draw(|f| app.draw(f)).unwrap();
        let text: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Pixel") && text.contains("1234ABCD") && text.contains("50%"));
    }
}
