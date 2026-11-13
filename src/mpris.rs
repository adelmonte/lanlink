//! Lets the phone see and control desktop media players over MPRIS (D-Bus).

use crate::daemon::Daemon;
use crate::protocol::{self, Packet};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use futures_util::StreamExt;
use tokio::sync::{Notify, OnceCell};
use zbus::{Connection, OwnedVariant, Variant};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const PROPS: &str = "org.freedesktop.DBus.Properties";

/// Devices that have asked about media, so get pushed updates.
static SUBSCRIBED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static BUS: OnceCell<Option<Connection>> = OnceCell::const_new();

async fn bus() -> Option<&'static Connection> {
    BUS.get_or_init(|| async { Connection::session().await.ok() }).await.as_ref()
}

fn subscribe(id: &str) {
    let mut s = SUBSCRIBED.lock().unwrap_or_else(|e| e.into_inner());
    s.get_or_insert_with(HashSet::new).insert(id.to_string());
}

fn subscribers() -> Vec<String> {
    let s = SUBSCRIBED.lock().unwrap_or_else(|e| e.into_inner());
    s.as_ref().map(|s| s.iter().cloned().collect()).unwrap_or_default()
}

struct Player {
    bus: String,
    name: String,
}

fn inner<'b, 'a>(v: &'b Variant<'a>) -> &'b Variant<'a> {
    match v {
        Variant::Variant(b) => inner(b),
        o => o,
    }
}

fn v_str(v: &Variant) -> Option<String> {
    match inner(v) {
        Variant::Str(s) => Some(s.to_string()),
        Variant::ObjectPath(p) => Some(p.to_string()),
        _ => None,
    }
}

fn v_i64(v: &Variant) -> Option<i64> {
    Some(match *inner(v) {
        Variant::I64(x) => x,
        Variant::U64(x) => x as i64,
        Variant::I32(x) => x as i64,
        Variant::U32(x) => x as i64,
        Variant::F64(x) => x as i64,
        _ => return None,
    })
}

fn v_f64(v: &Variant) -> Option<f64> {
    match *inner(v) {
        Variant::F64(x) => Some(x),
        _ => v_i64(v).map(|x| x as f64),
    }
}

fn v_bool(v: &Variant) -> Option<bool> {
    match *inner(v) {
        Variant::Bool(b) => Some(b),
        _ => None,
    }
}

fn v_strs(v: &Variant) -> Vec<String> {
    match inner(v) {
        Variant::Array(a) => a.inner().iter().filter_map(v_str).collect(),
        other => v_str(other).into_iter().collect(),
    }
}

async fn call<B>(c: &Connection, dest: &str, path: &str, iface: &str, method: &str, body: &B) -> Result<zbus::Message>
where
    B: serde::Serialize + zbus::Type,
{
    Ok(c.call_method(Some(dest), path, Some(iface), method, body).await?)
}

async fn players(c: &Connection) -> Result<Vec<Player>> {
    let msg = call(c, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "ListNames", &()).await?;
    let mut names: Vec<String> = msg.body().deserialize::<Vec<String>>()?;
    // playerctld is a proxy for the other players, not one itself.
    names.retain(|n| n.starts_with(PREFIX) && !n.ends_with(".playerctld"));
    names.sort();
    let mut out: Vec<Player> = vec![];
    for bus in names {
        let ident = match call(c, &bus, PATH, PROPS, "Get", &("org.mpris.MediaPlayer2", "Identity")).await {
            Ok(m) => m.body().deserialize::<OwnedVariant>().ok().and_then(|v| v_str(&v)),
            Err(_) => None,
        };
        let base = ident.unwrap_or_else(|| bus[PREFIX.len()..].split('.').next().unwrap_or("Player").to_string());
        let mut name = base.clone();
        let mut i = 2;
        while out.iter().any(|p| p.name == name) {
            name = format!("{base} {i}");
            i += 1;
        }
        out.push(Player { bus, name });
    }
    Ok(out)
}

async fn props(c: &Connection, bus: &str) -> Result<HashMap<String, OwnedVariant>> {
    let m = call(c, bus, PATH, PROPS, "GetAll", &(PLAYER,)).await?;
    Ok(m.body().deserialize()?)
}

fn status(name: &str, p: &HashMap<String, OwnedVariant>) -> Value {
    let get = |k: &str| p.get(k);
    let mut meta: HashMap<String, &Variant> = HashMap::new();
    if let Some(m) = get("Metadata")
        && let Variant::Dict(d) = inner(m)
    {
        for (k, v) in d.iter() {
            if let Some(k) = v_str(k) {
                meta.insert(k, v);
            }
        }
    }
    let ms = |k: &str| meta.get(k).and_then(|v| v_str(v)).unwrap_or_default();
    let title = ms("xesam:title");
    let artist = meta.get("xesam:artist").map(|v| v_strs(v).join(", ")).unwrap_or_default();
    let length = meta.get("mpris:length").and_then(|v| v_i64(v)).unwrap_or(0) / 1000;
    let b = |k: &str| get(k).and_then(|v| v_bool(v)).unwrap_or(false);
    let now_playing = match (artist.is_empty(), title.is_empty()) {
        (false, false) => format!("{artist} - {title}"),
        (true, false) => title.clone(),
        _ => String::new(),
    };
    json!({
        "player": name,
        "title": title,
        "artist": artist,
        "album": ms("xesam:album"),
        "albumArtUrl": ms("mpris:artUrl"),
        "nowPlaying": now_playing,
        "length": length,
        "pos": get("Position").and_then(|v| v_i64(v)).unwrap_or(0) / 1000,
        "isPlaying": get("PlaybackStatus").and_then(|v| v_str(v)).as_deref() == Some("Playing"),
        "canPause": b("CanPause"),
        "canPlay": b("CanPlay"),
        "canGoNext": b("CanGoNext"),
        "canGoPrevious": b("CanGoPrevious"),
        "canSeek": b("CanSeek"),
        "volume": (get("Volume").and_then(|v| v_f64(v)).unwrap_or(1.0) * 100.0).round() as i64,
        "loopStatus": get("LoopStatus").and_then(|v| v_str(v)).unwrap_or_else(|| "None".into()),
        "shuffle": b("Shuffle"),
    })
}

fn list_packet(players: &[Player]) -> Packet {
    let names: Vec<&str> = players.iter().map(|p| p.name.as_str()).collect();
    Packet::new(protocol::MPRIS, json!({"playerList": names, "supportAlbumArtPayload": true}))
}

pub async fn handle_request(d: Arc<Daemon>, id: &str, p: Packet) {
    if let Err(e) = handle_inner(&d, id, &p).await {
        eprintln!("lanlink: mpris: {e:#}");
    }
}

async fn handle_inner(d: &Daemon, id: &str, p: &Packet) -> Result<()> {
    subscribe(id);
    let c = bus().await.ok_or_else(|| anyhow!("no session bus"))?;
    let list = players(c).await?;
    if p.bool("requestPlayerList") {
        d.send(id, list_packet(&list))?;
    }
    let Some(name) = p.str("player") else { return Ok(()) };
    let Some(pl) = list.iter().find(|x| x.name == name) else { return Ok(()) };
    let bus = pl.bus.as_str();

    // The phone wants the cover. Only ever send the file the player itself
    // names as its current art, never an arbitrary path from the request.
    if let Some(url) = p.str("albumArtUrl") {
        let current = status(&pl.name, &props(c, bus).await?);
        if current["albumArtUrl"].as_str() == Some(url)
            && let Some(path) = file_url_path(url)
        {
            let pkt = Packet::new(protocol::MPRIS, json!({"transferringAlbumArt": true, "player": pl.name, "albumArtUrl": url}));
            crate::share::send_payload(d, id, pkt, &path).await?;
        }
        return Ok(());
    }

    // A command's result reaches the phone through the player's own change
    // signal (see push_loop), so only plain status requests are answered here.
    let commands = ["action", "setVolume", "Seek", "SetPosition", "setLoopStatus", "setShuffle"];
    let is_command = commands.iter().any(|k| p.body.get(*k).is_some());
    if let Some(action) = p.str("action")
        && ["PlayPause", "Play", "Pause", "Next", "Previous", "Stop"].contains(&action)
    {
        call(c, bus, PATH, PLAYER, action, &()).await?;
    }
    if let Some(vol) = p.i64("setVolume") {
        let v = Variant::F64((vol as f64 / 100.0).clamp(0.0, 1.0));
        call(c, bus, PATH, PROPS, "Set", &(PLAYER, "Volume", v)).await?;
    }
    if let Some(off) = p.i64("Seek") {
        call(c, bus, PATH, PLAYER, "Seek", &(off,)).await?;
    }
    if let Some(pos) = p.i64("SetPosition") {
        let pr = props(c, bus).await?;
        let track = pr.get("Metadata").and_then(|m| match inner(m) {
            Variant::Dict(dict) => dict.iter().find(|(k, _)| v_str(k).as_deref() == Some("mpris:trackid")).and_then(|(_, v)| v_str(v)),
            _ => None,
        });
        if let Some(t) = track
            && let Ok(path) = zbus::ObjectPath::try_from(t.as_str())
        {
            call(c, bus, PATH, PLAYER, "SetPosition", &(path, pos * 1000)).await?;
        }
    }
    if let Some(ls) = p.str("setLoopStatus") {
        call(c, bus, PATH, PROPS, "Set", &(PLAYER, "LoopStatus", Variant::from(ls))).await?;
    }
    if let Some(sh) = p.body.get("setShuffle").and_then(Value::as_bool) {
        call(c, bus, PATH, PROPS, "Set", &(PLAYER, "Shuffle", Variant::Bool(sh))).await?;
    }
    if is_command {
        return Ok(());
    }
    let st = status(&pl.name, &props(c, bus).await?);
    d.send(id, Packet::new(protocol::MPRIS, st))?;
    Ok(())
}

struct Sent {
    without_pos: Value,
    pos: i64,
    at: Instant,
    playing: bool,
}

/// Players announce their own changes on the bus: a player appearing or
/// leaving, a property changing (track, play state, volume) or a seek.
const SIGNALS: &[&str] = &[
    "type='signal',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',path='/org/mpris/MediaPlayer2'",
    "type='signal',interface='org.mpris.MediaPlayer2.Player',member='Seeked',path='/org/mpris/MediaPlayer2'",
    "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0namespace='org.mpris.MediaPlayer2'",
];

/// Wakes `changed` whenever a player signals a change.
async fn watch(c: &'static Connection, changed: Arc<Notify>) {
    for rule in SIGNALS {
        let Ok(mut stream) = zbus::MessageStream::for_match_rule(*rule, c, None).await else {
            eprintln!("lanlink: mpris: can't watch for player changes; checking every few seconds instead");
            continue;
        };
        let changed = changed.clone();
        tokio::spawn(async move {
            while stream.next().await.is_some() {
                changed.notify_one();
            }
        });
    }
}

/// Push player changes to subscribed devices as the players announce them.
/// A slow check catches players that don't signal. Position is only sent
/// when it jumps; the phone extrapolates it while playing.
pub async fn push_loop(d: Arc<Daemon>) {
    let mut last_lists: HashMap<String, Vec<String>> = HashMap::new();
    let mut last: HashMap<(String, String), Sent> = HashMap::new();
    let changed = Arc::new(Notify::new());
    if let Some(c) = bus().await {
        watch(c, changed.clone()).await;
    }
    loop {
        tokio::select! {
            _ = changed.notified() => {
                // A change usually comes as a burst of signals; send it once.
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        let subs: Vec<String> = {
            let st = d.lock();
            subscribers().into_iter().filter(|id| st.devices.get(id).is_some_and(|x| x.online())).collect()
        };
        if subs.is_empty() {
            last_lists.clear();
            last.clear();
            continue;
        }
        let Some(c) = bus().await else { continue };
        let Ok(list) = players(c).await else { continue };
        let mut statuses = vec![];
        for pl in &list {
            if let Ok(pr) = props(c, &pl.bus).await {
                statuses.push(status(&pl.name, &pr));
            }
        }
        let names: Vec<String> = list.iter().map(|p| p.name.clone()).collect();
        for id in subs {
            if last_lists.get(&id) != Some(&names) {
                let _ = d.send(&id, list_packet(&list));
                last_lists.insert(id.clone(), names.clone());
            }
            for s in &statuses {
                let player = s["player"].as_str().unwrap_or_default().to_string();
                let pos = s["pos"].as_i64().unwrap_or(0);
                let playing = s["isPlaying"].as_bool().unwrap_or(false);
                let mut wp = s.clone();
                wp["pos"] = json!(0);
                let key = (id.clone(), player);
                let send = match last.get(&key) {
                    None => true,
                    Some(prev) => {
                        let expect = prev.pos + if prev.playing { prev.at.elapsed().as_millis() as i64 } else { 0 };
                        prev.without_pos != wp || (pos - expect).abs() > 2000
                    }
                };
                if send {
                    let _ = d.send(&id, Packet::new(protocol::MPRIS, s.clone()));
                    last.insert(key, Sent { without_pos: wp, pos, at: Instant::now(), playing });
                }
            }
        }
    }
}

/// `file:///a%20b.jpg` -> `/a b.jpg`.
fn file_url_path(url: &str) -> Option<std::path::PathBuf> {
    let raw = url.strip_prefix("file://")?.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && let Some(hex) = raw.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    let p = std::path::PathBuf::from(String::from_utf8(out).ok()?);
    p.is_absolute().then_some(p)
}
