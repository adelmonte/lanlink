//! The background service: discovery, device links, pairing, and dispatch to
//! the feature modules (share, sftp, mpris, input, capture).

use crate::config::{self, Config, Identity};
use crate::ipc::Request;
use crate::protocol::{self, Packet};
use crate::{capture, input, mpris, notify, sftp, share, tls};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket, UnixListener};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

const PAIR_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LINE: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PairState {
    None,
    /// We asked; waiting for them. Holds the request timestamp.
    Requested(i64),
    /// They asked; waiting for the user.
    Incoming(i64),
}

pub struct Link {
    pub tx: mpsc::UnboundedSender<Packet>,
    pub generation: u64,
    pub ip: IpAddr,
}

pub struct Device {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub incoming: Vec<String>,
    /// Cert presented on the current link.
    pub cert: Option<Vec<u8>>,
    /// Cert pinned at pairing. Some = paired.
    pub trusted: Option<Vec<u8>>,
    pub link: Option<Link>,
    pub pair: PairState,
    pub battery: Option<(i64, bool)>,
    pub mount: Option<PathBuf>,
    /// Where it was last connected from, to reconnect after it drops.
    pub last_ip: Option<IpAddr>,
}

impl Device {
    fn new(id: &str) -> Self {
        Device {
            id: id.into(),
            name: id.into(),
            kind: "unknown".into(),
            incoming: vec![],
            cert: None,
            trusted: None,
            link: None,
            pair: PairState::None,
            battery: None,
            mount: None,
            last_ip: None,
        }
    }
    pub fn paired(&self) -> bool {
        self.trusted.is_some()
    }
    pub fn online(&self) -> bool {
        self.link.is_some()
    }
    pub fn accepts(&self, kind: &str) -> bool {
        self.incoming.iter().any(|c| c == kind)
    }
}

pub struct Transfer {
    pub id: u64,
    pub device: String,
    pub name: String,
    pub incoming: bool,
    pub total: i64,
    pub done: i64,
    pub status: String,
}

#[derive(Default)]
pub struct State {
    pub devices: HashMap<String, Device>,
    pub transfers: Vec<Transfer>,
    connecting: HashSet<String>,
    next_id: u64,
}

impl State {
    pub fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

pub struct Daemon {
    pub cfg: Config,
    pub ident: Identity,
    pub tcp_port: u16,
    server_cfg: Arc<rustls::ServerConfig>,
    client_cfg: Arc<rustls::ClientConfig>,
    state: Mutex<State>,
    pub input: Mutex<Option<input::Injector>>,
    pub capture: Mutex<Option<capture::Capture>>,
    pub rt: tokio::runtime::Handle,
}

pub async fn run() -> Result<()> {
    let cfg = Config::load()?;
    let ident = Identity::load_or_create()?;
    let sock_path = config::socket_path();
    if tokio::net::UnixStream::connect(&sock_path).await.is_ok() {
        bail!("lanlink daemon is already running ({})", sock_path.display());
    }
    let _ = std::fs::remove_file(&sock_path);
    let ipc = UnixListener::bind(&sock_path).with_context(|| format!("binding {}", sock_path.display()))?;

    let (listener, tcp_port) = bind_tcp(cfg.tcp_port).await?;
    let daemon = Arc::new(Daemon {
        server_cfg: tls::server_config(&ident)?,
        client_cfg: tls::client_config(&ident)?,
        state: Mutex::new(State { devices: load_trusted(), ..Default::default() }),
        input: Mutex::new(None),
        capture: Mutex::new(None),
        rt: tokio::runtime::Handle::current(),
        tcp_port,
        cfg,
        ident,
    });
    eprintln!(
        "lanlink: {} ({}) listening on tcp {}",
        daemon.cfg.name, daemon.ident.device_id, tcp_port
    );

    tokio::spawn(daemon.clone().accept_loop(listener));
    if daemon.cfg.discovery {
        tokio::spawn(daemon.clone().discovery());
    }
    tokio::spawn(daemon.clone().reconnect_loop());
    tokio::spawn(mpris::push_loop(daemon.clone()));
    tokio::spawn(daemon.clone().ipc_loop(ipc));

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    daemon.shutdown();
    let _ = std::fs::remove_file(&sock_path);
    Ok(())
}

async fn bind_tcp(preferred: u16) -> Result<(TcpListener, u16)> {
    let ports = std::iter::once(preferred).chain(protocol::TCP_PORTS.filter(move |p| *p != preferred));
    for port in ports {
        if let Ok(l) = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await {
            return Ok((l, port));
        }
    }
    bail!("no free TCP port in 1716-1764")
}

fn load_trusted() -> HashMap<String, Device> {
    let mut out = HashMap::new();
    let Ok(rd) = std::fs::read_dir(config::trusted_dir()) else { return out };
    for e in rd.flatten() {
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let (Some(id), Some(cert)) = (v["id"].as_str(), v["cert"].as_str().and_then(|c| hex::decode(c).ok()))
        else {
            continue;
        };
        let mut d = Device::new(id);
        d.name = v["name"].as_str().unwrap_or(id).into();
        d.kind = v["type"].as_str().unwrap_or("unknown").into();
        d.trusted = Some(cert);
        out.insert(id.to_string(), d);
    }
    out
}

fn save_trusted(d: &Device) {
    let Some(cert) = &d.trusted else { return };
    let dir = config::trusted_dir();
    let _ = std::fs::create_dir_all(&dir);
    let v = json!({"id": d.id, "name": d.name, "type": d.kind, "cert": hex::encode(cert)});
    let _ = std::fs::write(dir.join(format!("{}.json", d.id)), serde_json::to_vec_pretty(&v).unwrap());
}

/// Read one '\n'-terminated line without buffering past it (TLS bytes follow).
async fn read_raw_line<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    loop {
        if r.read(&mut b).await? == 0 {
            bail!("connection closed");
        }
        if b[0] == b'\n' {
            return Ok(out);
        }
        out.push(b[0]);
        if out.len() > 64 * 1024 {
            bail!("identity too long");
        }
    }
}

async fn read_line<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let n = r.take(MAX_LINE).read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(Some(buf))
}

impl Daemon {
    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn identity(&self, with_port: bool) -> Packet {
        protocol::identity(
            &self.ident.device_id,
            &self.cfg.name,
            &self.cfg.device_type,
            with_port.then_some(self.tcp_port),
        )
    }

    pub fn send(&self, id: &str, p: Packet) -> Result<()> {
        let st = self.lock();
        let d = st.devices.get(id).ok_or_else(|| anyhow!("unknown device"))?;
        let link = d.link.as_ref().ok_or_else(|| anyhow!("{} is offline", d.name))?;
        link.tx.send(p).map_err(|_| anyhow!("{} disconnected", d.name))
    }

    pub fn name_of(&self, id: &str) -> String {
        self.lock().devices.get(id).map(|d| d.name.clone()).unwrap_or_else(|| id.into())
    }

    pub fn peer_ip(&self, id: &str) -> Option<IpAddr> {
        self.lock().devices.get(id)?.link.as_ref().map(|l| l.ip)
    }

    pub fn cert_of(&self, id: &str) -> Option<Vec<u8>> {
        self.lock().devices.get(id)?.trusted.clone()
    }

    pub fn tls_acceptor(&self) -> TlsAcceptor {
        TlsAcceptor::from(self.server_cfg.clone())
    }

    pub fn tls_connector(&self) -> TlsConnector {
        TlsConnector::from(self.client_cfg.clone())
    }

    fn shutdown(&self) {
        capture::stop(self);
        let ids: Vec<String> = self.lock().devices.keys().cloned().collect();
        for id in ids {
            sftp::unmount(self, &id);
        }
    }

    // ---- connections -------------------------------------------------------

    async fn accept_loop(self: Arc<Self>, l: TcpListener) {
        loop {
            let Ok((tcp, addr)) = l.accept().await else { continue };
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.incoming(tcp, addr).await {
                    eprintln!("lanlink: incoming {addr}: {e:#}");
                }
            });
        }
    }

    /// They connected to us: read their plaintext identity, then we are the TLS client.
    async fn incoming(self: Arc<Self>, mut tcp: TcpStream, addr: SocketAddr) -> Result<()> {
        keepalive(&tcp);
        let line = timeout(Duration::from_secs(10), read_raw_line(&mut tcp)).await??;
        let p = Packet::parse(&line)?;
        if p.kind != protocol::IDENTITY {
            bail!("expected identity, got {}", p.kind);
        }
        if p.str("deviceId") == Some(self.ident.device_id.as_str()) {
            return Ok(());
        }
        let tls = timeout(Duration::from_secs(10), self.tls_connector().connect(tls::dummy_name(), tcp)).await??;
        self.run_link(TlsStream::Client(tls), addr.ip(), Some(p.body)).await
    }

    /// We connect to them (after seeing their broadcast, or `lanlink connect`):
    /// send our identity, then we are the TLS server.
    pub async fn outgoing(self: Arc<Self>, addr: SocketAddr, pre: Option<Value>) -> Result<()> {
        let mut tcp = timeout(Duration::from_secs(5), TcpStream::connect(addr)).await??;
        keepalive(&tcp);
        tcp.write_all(&self.identity(true).to_line()).await?;
        let tls = timeout(Duration::from_secs(10), self.tls_acceptor().accept(tcp)).await??;
        self.run_link(TlsStream::Server(tls), addr.ip(), pre).await
    }

    async fn run_link(self: Arc<Self>, tls: TlsStream<TcpStream>, ip: IpAddr, pre: Option<Value>) -> Result<()> {
        let cert = tls::peer_cert(&tls)?;
        let version = pre.as_ref().and_then(|b| b["protocolVersion"].as_i64()).unwrap_or(protocol::PROTOCOL_VERSION);
        let (r, mut w) = tokio::io::split(tls);
        let mut reader = BufReader::new(r);

        // v8: identities are re-exchanged inside TLS; the plaintext one is untrusted.
        let body = if version >= 8 {
            w.write_all(&self.identity(false).to_line()).await?;
            let line = timeout(Duration::from_secs(10), read_line(&mut reader))
                .await??
                .ok_or_else(|| anyhow!("closed during identity"))?;
            let p = Packet::parse(&line)?;
            if p.kind != protocol::IDENTITY {
                bail!("expected identity inside TLS, got {}", p.kind);
            }
            p.body
        } else {
            pre.ok_or_else(|| anyhow!("no identity"))?
        };

        let id = body["deviceId"].as_str().unwrap_or_default().to_string();
        if id == self.ident.device_id {
            return Ok(());
        }
        if !protocol::valid_device_id(&id) && version >= 8 {
            bail!("invalid device id {id:?}");
        }
        if version >= 8 && tls::common_name(&cert)? != id {
            bail!("certificate CN does not match device id {id}");
        }

        let (tx, mut rx) = mpsc::unbounded_channel::<Packet>();
        let generation = self.register(&id, &body, cert, ip, tx)?;
        eprintln!("lanlink: connected {} ({ip})", self.name_of(&id));

        let mut writer = tokio::spawn(async move {
            while let Some(p) = rx.recv().await {
                if w.write_all(&p.to_line()).await.is_err() {
                    break;
                }
            }
            let _ = w.shutdown().await;
        });

        self.on_connected(&id);
        loop {
            tokio::select! {
                line = read_line(&mut reader) => match line {
                    Ok(Some(l)) if l.is_empty() => continue,
                    Ok(Some(l)) => match Packet::parse(&l) {
                        Ok(p) => self.clone().handle(&id, p),
                        Err(e) => eprintln!("lanlink: {e}"),
                    },
                    _ => break,
                },
                _ = &mut writer => break,
            }
        }
        writer.abort();
        self.unregister(&id, generation);
        eprintln!("lanlink: disconnected {}", self.name_of(&id));
        Ok(())
    }

    fn register(&self, id: &str, body: &Value, cert: Vec<u8>, ip: IpAddr, tx: mpsc::UnboundedSender<Packet>) -> Result<u64> {
        let mut st = self.lock();
        let generation = st.next_id();
        let d = st.devices.entry(id.to_string()).or_insert_with(|| Device::new(id));
        if let Some(t) = &d.trusted
            && *t != cert
        {
            bail!("{} presented a different certificate than when paired; refusing", d.name);
        }
        let name = body["deviceName"].as_str().map(config::sanitize_name).unwrap_or_else(|| id.into());
        let kind = body["deviceType"].as_str().unwrap_or("unknown").to_string();
        let changed = d.name != name || d.kind != kind;
        d.name = name;
        d.kind = kind;
        d.incoming = body["incomingCapabilities"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        d.cert = Some(cert);
        d.last_ip = Some(ip);
        d.link = Some(Link { tx, generation, ip }); // replaces (and so closes) any older link
        d.pair = PairState::None;
        if changed && d.paired() {
            save_trusted(d);
        }
        Ok(generation)
    }

    fn unregister(&self, id: &str, generation: u64) {
        let mut st = self.lock();
        let Some(d) = st.devices.get_mut(id) else { return };
        if d.link.as_ref().map(|l| l.generation) != Some(generation) {
            return; // a newer link took over
        }
        d.link = None;
        d.pair = PairState::None;
        d.battery = None;
        if !d.paired() {
            st.devices.remove(id);
        }
        drop(st);
        if self.capture_target().as_deref() == Some(id) {
            capture::stop(self);
        }
    }

    fn on_connected(&self, id: &str) {
        let st = self.lock();
        let Some(d) = st.devices.get(id) else { return };
        if d.paired()
            && let Some(l) = &d.link
        {
            let _ = l.tx.send(Packet::new(protocol::MOUSEPAD_KEYBOARD, json!({"state": true})));
        }
    }

    /// A sleeping phone doesn't hear broadcasts, but a unicast connection
    /// wakes it. Keep trying paired devices where they were last seen.
    async fn reconnect_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(20)).await;
            let targets: Vec<(String, IpAddr)> = {
                let mut st = self.lock();
                let t: Vec<_> = st
                    .devices
                    .values()
                    .filter(|d| d.paired() && !d.online())
                    .filter_map(|d| d.last_ip.map(|ip| (d.id.clone(), ip)))
                    .collect();
                t.into_iter().filter(|(id, _)| st.connecting.insert(id.clone())).collect()
            };
            for (id, ip) in targets {
                let me = self.clone();
                tokio::spawn(async move {
                    let _ = me.clone().outgoing(SocketAddr::new(ip, protocol::UDP_PORT), None).await;
                    me.lock().connecting.remove(&id);
                });
            }
        }
    }

    async fn discovery(self: Arc<Self>) {
        let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, protocol::UDP_PORT)).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                eprintln!("lanlink: discovery disabled, can't bind udp {}: {e}", protocol::UDP_PORT);
                return;
            }
        };
        let _ = sock.set_broadcast(true);
        let bsock = sock.clone();
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let pkt = me.identity(true).to_line();
                let _ = bsock.send_to(&pkt, (Ipv4Addr::BROADCAST, protocol::UDP_PORT)).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let Ok((n, src)) = sock.recv_from(&mut buf).await else { continue };
            let Ok(p) = Packet::parse(buf[..n].trim_ascii_end()) else { continue };
            if p.kind != protocol::IDENTITY {
                continue;
            }
            let Some(id) = p.str("deviceId").map(String::from) else { continue };
            let Some(port) = p.i64("tcpPort") else { continue };
            if id == self.ident.device_id {
                continue;
            }
            {
                let mut st = self.lock();
                if st.devices.get(&id).is_some_and(Device::online) || !st.connecting.insert(id.clone()) {
                    continue;
                }
            }
            let me = self.clone();
            tokio::spawn(async move {
                let addr = SocketAddr::new(src.ip(), port as u16);
                let res = me.clone().outgoing(addr, Some(p.body)).await;
                me.lock().connecting.remove(&id);
                if let Err(e) = res {
                    eprintln!("lanlink: connect {addr}: {e:#}");
                }
            });
        }
    }

    // ---- packets -----------------------------------------------------------

    fn handle(self: Arc<Self>, id: &str, p: Packet) {
        if p.kind == protocol::PAIR {
            return self.handle_pair(id, &p);
        }
        if !self.lock().devices.get(id).is_some_and(Device::paired) {
            // Same as KDE: a device that thinks it's paired with us but isn't gets told so.
            let _ = self.send(id, Packet::new(protocol::PAIR, json!({"pair": false})));
            return;
        }
        match p.kind.as_str() {
            protocol::PING => notify::show(self.name_of(id), p.str("message").unwrap_or("Ping!")),
            protocol::BATTERY => {
                if let Some(d) = self.lock().devices.get_mut(id) {
                    d.battery = p.i64("currentCharge").map(|c| (c, p.bool("isCharging")));
                }
            }
            protocol::SHARE => share::receive(self, id, p),
            protocol::SFTP => sftp::on_info(self, id, p),
            protocol::MPRIS_REQUEST => {
                let id = id.to_string();
                tokio::spawn(async move { mpris::handle_request(self, &id, p).await });
            }
            protocol::MOUSEPAD => input::handle(&self, id, &p),
            _ => {}
        }
    }

    fn handle_pair(self: Arc<Self>, id: &str, p: &Packet) {
        let mut st = self.lock();
        let Some(d) = st.devices.get_mut(id) else { return };
        if !p.bool("pair") {
            let was = d.paired();
            d.pair = PairState::None;
            if was {
                drop(st);
                self.forget(id);
                notify::show("Unpaired", format!("{} unpaired from this device", self.name_of(id)));
            }
            return;
        }
        match d.pair {
            PairState::Requested(_) => {
                d.trusted = d.cert.clone();
                d.pair = PairState::None;
                save_trusted(d);
                let name = d.name.clone();
                drop(st);
                self.on_connected(id);
                notify::show("Paired", format!("Paired with {name}"));
            }
            _ if d.paired() => {
                // They lost our pairing but we didn't: just confirm.
                if let Some(l) = &d.link {
                    let _ = l.tx.send(Packet::new(protocol::PAIR, json!({"pair": true})));
                }
            }
            PairState::Incoming(_) => {}
            PairState::None => {
                let ts = p.i64("timestamp").unwrap_or_else(|| protocol::now_ms() / 1000);
                d.pair = PairState::Incoming(ts);
                let name = d.name.clone();
                let code = d
                    .cert
                    .as_ref()
                    .and_then(|c| tls::verification_key(&self.ident.cert, c, ts).ok())
                    .unwrap_or_default();
                drop(st);
                let me = self.clone();
                let dev = id.to_string();
                notify::ask(
                    format!("Pair request from {name}"),
                    format!("Check both screens show {code}"),
                    &[("accept", "Accept"), ("reject", "Reject")],
                    PAIR_TIMEOUT.as_millis() as u32,
                    move |a| {
                        let res = match a.as_deref() {
                            Some("accept") => me.accept(&dev),
                            Some("reject") => me.reject(&dev),
                            _ => Ok(()),
                        };
                        if let Err(e) = res {
                            eprintln!("lanlink: {e:#}");
                        }
                    },
                );
                self.expire_pair(id, PairState::Incoming(ts));
            }
        }
    }

    fn expire_pair(self: &Arc<Self>, id: &str, state: PairState) {
        let me = self.clone();
        let id = id.to_string();
        self.rt.spawn(async move {
            tokio::time::sleep(PAIR_TIMEOUT).await;
            if let Some(d) = me.lock().devices.get_mut(&id)
                && d.pair == state
            {
                d.pair = PairState::None;
            }
        });
    }

    pub fn pair(self: &Arc<Self>, id: &str) -> Result<String> {
        let ts = protocol::now_ms() / 1000;
        let code = {
            let mut st = self.lock();
            let d = st.devices.get_mut(id).ok_or_else(|| anyhow!("unknown device"))?;
            if d.paired() {
                bail!("already paired with {}", d.name);
            }
            let l = d.link.as_ref().ok_or_else(|| anyhow!("{} is offline", d.name))?;
            l.tx.send(Packet::new(protocol::PAIR, json!({"pair": true, "timestamp": ts})))?;
            d.pair = PairState::Requested(ts);
            d.cert.as_ref().and_then(|c| tls::verification_key(&self.ident.cert, c, ts).ok()).unwrap_or_default()
        };
        self.expire_pair(id, PairState::Requested(ts));
        Ok(code)
    }

    pub fn accept(&self, id: &str) -> Result<()> {
        let mut st = self.lock();
        let d = st.devices.get_mut(id).ok_or_else(|| anyhow!("unknown device"))?;
        if !matches!(d.pair, PairState::Incoming(_)) {
            bail!("no pending pair request from {}", d.name);
        }
        let l = d.link.as_ref().ok_or_else(|| anyhow!("{} is offline", d.name))?;
        l.tx.send(Packet::new(protocol::PAIR, json!({"pair": true})))?;
        d.trusted = d.cert.clone();
        d.pair = PairState::None;
        save_trusted(d);
        drop(st);
        self.on_connected(id);
        Ok(())
    }

    pub fn reject(&self, id: &str) -> Result<()> {
        let mut st = self.lock();
        let d = st.devices.get_mut(id).ok_or_else(|| anyhow!("unknown device"))?;
        d.pair = PairState::None;
        if let Some(l) = &d.link {
            let _ = l.tx.send(Packet::new(protocol::PAIR, json!({"pair": false})));
        }
        Ok(())
    }

    pub fn unpair(&self, id: &str) -> Result<()> {
        let _ = self.send(id, Packet::new(protocol::PAIR, json!({"pair": false})));
        self.forget(id);
        Ok(())
    }

    fn forget(&self, id: &str) {
        sftp::unmount(self, id);
        let _ = std::fs::remove_file(config::trusted_dir().join(format!("{id}.json")));
        let mut st = self.lock();
        if let Some(d) = st.devices.get_mut(id) {
            d.trusted = None;
            if !d.online() {
                st.devices.remove(id);
            }
        }
    }

    pub fn capture_target(&self) -> Option<String> {
        self.capture.lock().ok()?.as_ref().map(|c| c.device.clone())
    }

    // ---- ipc ---------------------------------------------------------------

    async fn ipc_loop(self: Arc<Self>, l: UnixListener) {
        loop {
            let Ok((s, _)) = l.accept().await else { continue };
            let me = self.clone();
            tokio::spawn(async move {
                let (r, mut w) = s.into_split();
                let mut r = BufReader::new(r);
                let Ok(Some(line)) = read_line(&mut r).await else { return };
                let resp = match serde_json::from_slice::<Request>(&line) {
                    Ok(req) => match me.request(req).await {
                        Ok(data) => json!({"ok": true, "data": data}),
                        Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
                    },
                    Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
                };
                let mut out = serde_json::to_vec(&resp).unwrap();
                out.push(b'\n');
                let _ = w.write_all(&out).await;
            });
        }
    }

    fn resolve(&self, q: &str) -> Result<String> {
        let st = self.lock();
        if st.devices.contains_key(q) {
            return Ok(q.into());
        }
        let ql = q.to_lowercase();
        let exact: Vec<_> = st.devices.values().filter(|d| d.name.to_lowercase() == ql).collect();
        if exact.len() == 1 {
            return Ok(exact[0].id.clone());
        }
        let fuzzy: Vec<_> = st
            .devices
            .values()
            .filter(|d| d.id.starts_with(q) || d.name.to_lowercase().starts_with(&ql))
            .collect();
        match fuzzy.len() {
            1 => Ok(fuzzy[0].id.clone()),
            0 => bail!("no device matches {q:?}"),
            _ => bail!("{q:?} matches several devices; use the id"),
        }
    }

    async fn request(self: &Arc<Self>, req: Request) -> Result<Value> {
        use Request::*;
        Ok(match req {
            Status => self.status(),
            Pair { device } => json!({"code": self.pair(&self.resolve(&device)?)?}),
            Unpair { device } => self.unpair(&self.resolve(&device)?).map(|_| Value::Null)?,
            Accept { device } => self.accept(&self.resolve(&device)?).map(|_| Value::Null)?,
            Reject { device } => self.reject(&self.resolve(&device)?).map(|_| Value::Null)?,
            Send { device, items } => {
                let id = self.resolve(&device)?;
                self.require_paired(&id)?;
                share::send(self.clone(), id, items)?;
                Value::Null
            }
            Text { device, text } => {
                let id = self.resolve(&device)?;
                self.require_paired(&id)?;
                self.send(&id, Packet::new(protocol::SHARE, json!({"text": text})))?;
                Value::Null
            }
            Mount { device } => {
                let id = self.resolve(&device)?;
                self.require_paired(&id)?;
                sftp::request(self, &id)?;
                Value::Null
            }
            Unmount { device } => {
                sftp::unmount(self, &self.resolve(&device)?);
                Value::Null
            }
            Remote { device } => {
                let id = self.resolve(&device)?;
                self.require_paired(&id)?;
                capture::start(self.clone(), id).await?;
                Value::Null
            }
            Release => {
                capture::stop(self);
                Value::Null
            }
            Ping { device } => {
                let id = self.resolve(&device)?;
                self.require_paired(&id)?;
                self.send(&id, Packet::new(protocol::PING, json!({})))?;
                Value::Null
            }
            Connect { addr } => {
                let addr: SocketAddr = if addr.contains(':') {
                    addr.parse()?
                } else {
                    SocketAddr::new(addr.parse()?, protocol::UDP_PORT)
                };
                let me = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = me.outgoing(addr, None).await {
                        eprintln!("lanlink: connect {addr}: {e:#}");
                    }
                });
                Value::Null
            }
        })
    }

    fn require_paired(&self, id: &str) -> Result<()> {
        let st = self.lock();
        let d = st.devices.get(id).ok_or_else(|| anyhow!("unknown device"))?;
        if !d.paired() {
            bail!("{} is not paired", d.name);
        }
        if !d.online() {
            bail!("{} is offline", d.name);
        }
        Ok(())
    }

    fn status(&self) -> Value {
        let capture = self.capture_target();
        let st = self.lock();
        let mut devices: Vec<Value> = st
            .devices
            .values()
            .map(|d| {
                let (pair, code) = match d.pair {
                    PairState::None => ("none", None),
                    PairState::Requested(ts) => ("requested", Some(ts)),
                    PairState::Incoming(ts) => ("incoming", Some(ts)),
                };
                let code = code.and_then(|ts| tls::verification_key(&self.ident.cert, d.cert.as_ref()?, ts).ok());
                json!({
                    "id": d.id,
                    "name": d.name,
                    "type": d.kind,
                    "paired": d.paired(),
                    "online": d.online(),
                    "ip": d.link.as_ref().map(|l| l.ip.to_string()),
                    "pair": pair,
                    "code": code,
                    "battery": d.battery.map(|b| b.0),
                    "charging": d.battery.map(|b| b.1),
                    "mount": d.mount.as_ref().map(|m| m.display().to_string()),
                    "fingerprint": d.trusted.as_ref().or(d.cert.as_ref()).map(|c| tls::fingerprint(c)),
                    "remote": capture.as_deref() == Some(d.id.as_str()),
                    "can_remote": d.accepts(protocol::MOUSEPAD),
                })
            })
            .collect();
        devices.sort_by_key(|d| (!d["paired"].as_bool().unwrap_or(false), d["name"].as_str().unwrap_or("").to_lowercase()));
        let transfers: Vec<Value> = st
            .transfers
            .iter()
            .rev()
            .take(20)
            .map(|t| {
                json!({"id": t.id, "device": self.dev_name(&st, &t.device), "name": t.name,
                       "incoming": t.incoming, "total": t.total, "done": t.done, "status": t.status})
            })
            .collect();
        json!({
            "self": {"id": self.ident.device_id, "name": self.cfg.name, "type": self.cfg.device_type, "port": self.tcp_port},
            "devices": devices,
            "transfers": transfers,
        })
    }

    fn dev_name(&self, st: &State, id: &str) -> String {
        st.devices.get(id).map(|d| d.name.clone()).unwrap_or_else(|| id.into())
    }
}

/// Notice a vanished peer within about half a minute instead of never; the
/// probes also keep a dozing phone's connection from going stale.
fn keepalive(tcp: &TcpStream) {
    let ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(15))
        .with_interval(Duration::from_secs(5))
        .with_retries(3);
    let _ = socket2::SockRef::from(tcp).set_tcp_keepalive(&ka);
}
