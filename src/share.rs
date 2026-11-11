//! File / text / URL sharing (kdeconnect.share.request).
//! Payloads go over a separate TLS socket: the sender listens, the receiver
//! connects and is the TLS client.

use crate::daemon::{Daemon, Transfer};
use crate::protocol::{self, Packet};
use crate::{notify, tls};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::json;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

const CHUNK: usize = 64 * 1024;

fn start(d: &Daemon, id: &str, name: &str, incoming: bool, total: i64) -> u64 {
    let mut st = d.lock();
    let tid = st.next_id();
    st.transfers.push(Transfer {
        id: tid,
        device: id.into(),
        name: name.into(),
        incoming,
        total,
        done: 0,
        status: "active".into(),
    });
    if st.transfers.len() > 50 {
        st.transfers.remove(0);
    }
    tid
}

fn update(d: &Daemon, tid: u64, done: i64, status: Option<String>) {
    if let Some(t) = d.lock().transfers.iter_mut().find(|t| t.id == tid) {
        t.done = done;
        if let Some(s) = status {
            t.status = s;
        }
    }
}

pub fn receive(d: Arc<Daemon>, id: &str, p: Packet) {
    let from = d.name_of(id);
    if let Some(text) = p.str("text") {
        notify::show(format!("Text from {from}"), text);
        return;
    }
    if let Some(url) = p.str("url") {
        let url = url.to_string();
        notify::ask(format!("Link from {from}"), url.clone(), &[("open", "Open")], 15_000, move |a| {
            if a.as_deref() == Some("open") {
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
            }
        });
        return;
    }
    let Some(name) = p.str("filename").map(safe_name) else { return };
    let Some(port) = p.payload_port() else { return };
    let size = p.payload_size.unwrap_or(-1);
    let id = id.to_string();
    tokio::spawn(async move {
        let tid = start(&d, &id, &name, true, size);
        match download(&d, &id, &name, port, size, tid).await {
            Ok(path) => {
                update(&d, tid, size.max(0), Some("done".into()));
                let shown = path.display().to_string();
                notify::ask(format!("Received from {from}"), shown, &[("open", "Open")], 10_000, move |a| {
                    if a.as_deref() == Some("open") {
                        let _ = std::process::Command::new("xdg-open").arg(&path).spawn();
                    }
                });
            }
            Err(e) => {
                update(&d, tid, 0, Some(format!("failed: {e:#}")));
                notify::show("Transfer failed", format!("{name} from {from}: {e:#}"));
            }
        }
    });
}

fn safe_name(raw: &str) -> String {
    let base = Path::new(raw).file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let s: String = base.chars().filter(|c| *c != '/' && *c != '\0').collect();
    if s.is_empty() || s == "." || s == ".." { "file".into() } else { s }
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (1..)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .expect("some free name")
}

async fn download(d: &Daemon, id: &str, name: &str, port: u16, size: i64, tid: u64) -> Result<PathBuf> {
    let ip = d.peer_ip(id).ok_or_else(|| anyhow!("device went offline"))?;
    let cert = d.cert_of(id).ok_or_else(|| anyhow!("device not paired"))?;
    let tcp = timeout(Duration::from_secs(10), TcpStream::connect((ip, port))).await??;
    let mut tls = timeout(Duration::from_secs(10), d.tls_connector().connect(tls::dummy_name(), tcp)).await??;
    let peer = tls.get_ref().1.peer_certificates().and_then(|c| c.first()).map(|c| c.as_ref().to_vec());
    if peer.as_deref() != Some(cert.as_slice()) {
        bail!("payload socket presented the wrong certificate");
    }

    let dir = &d.cfg.download_dir;
    tokio::fs::create_dir_all(dir).await.with_context(|| format!("creating {}", dir.display()))?;
    let path = unique_path(dir, name);
    let part = path.with_file_name(format!("{}.part", path.file_name().unwrap().to_string_lossy()));
    let mut f = tokio::fs::File::create(&part).await?;
    let mut buf = vec![0u8; CHUNK];
    let mut done: i64 = 0;
    let res: Result<()> = async {
        loop {
            let want = if size >= 0 { ((size - done) as usize).min(CHUNK) } else { CHUNK };
            if want == 0 {
                break;
            }
            let n = match timeout(Duration::from_secs(30), tls.read(&mut buf[..want])).await? {
                Ok(n) => n,
                // Android often closes without a TLS close_notify; fine if we have every byte.
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && size < 0 => 0,
                Err(e) => return Err(e.into()),
            };
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n]).await?;
            done += n as i64;
            update(d, tid, done, None);
        }
        f.flush().await?;
        if size >= 0 && done != size {
            bail!("got {done} of {size} bytes");
        }
        Ok(())
    }
    .await;
    if let Err(e) = res {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }
    tokio::fs::rename(&part, &path).await?;
    Ok(path)
}

enum Item {
    File(PathBuf, u64),
    Url(String),
}

pub fn send(d: Arc<Daemon>, id: String, items: Vec<String>) -> Result<()> {
    let mut parsed = vec![];
    for it in items {
        let p = PathBuf::from(&it);
        if let Ok(m) = std::fs::metadata(&p) {
            if !m.is_file() {
                bail!("{it} is not a regular file");
            }
            parsed.push(Item::File(p, m.len()));
        } else if it.contains("://") {
            parsed.push(Item::Url(it));
        } else {
            bail!("{it}: no such file");
        }
    }
    let files: Vec<_> = parsed.iter().filter_map(|i| if let Item::File(p, s) = i { Some((p.clone(), *s)) } else { None }).collect();
    for i in &parsed {
        if let Item::Url(u) = i {
            d.send(&id, Packet::new(protocol::SHARE, json!({"url": u})))?;
        }
    }
    let total: u64 = files.iter().map(|f| f.1).sum();
    let count = files.len();
    tokio::spawn(async move {
        for (path, size) in files {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let tid = start(&d, &id, &name, false, size as i64);
            match upload(&d, &id, &path, &name, size, count, total, tid).await {
                Ok(()) => update(&d, tid, size as i64, Some("done".into())),
                Err(e) => update(&d, tid, 0, Some(format!("failed: {e:#}"))),
            }
        }
    });
    Ok(())
}

async fn bind_payload() -> Result<(TcpListener, u16)> {
    for port in protocol::PAYLOAD_PORTS {
        if let Ok(l) = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await {
            return Ok((l, port));
        }
    }
    bail!("no free payload port in 1739-1764")
}

#[allow(clippy::too_many_arguments)]
async fn upload(d: &Daemon, id: &str, path: &Path, name: &str, size: u64, count: usize, total: u64, tid: u64) -> Result<()> {
    let cert = d.cert_of(id).ok_or_else(|| anyhow!("device not paired"))?;
    let mut f = tokio::fs::File::open(path).await?;
    let modified = std::fs::metadata(path)?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|t| t.as_millis() as i64);
    let (listener, port) = bind_payload().await?;
    let mut p = Packet::new(
        protocol::SHARE,
        json!({"filename": name, "lastModified": modified, "open": false,
               "numberOfFiles": count, "totalPayloadSize": total}),
    );
    p.payload_size = Some(size as i64);
    p.payload_info = Some(json!({"port": port}));
    d.send(id, p)?;

    let (tcp, _) = timeout(Duration::from_secs(60), listener.accept()).await.context("device never fetched the file")??;
    drop(listener);
    let mut tls = timeout(Duration::from_secs(10), d.tls_acceptor().accept(tcp)).await??;
    let peer = tls.get_ref().1.peer_certificates().and_then(|c| c.first()).map(|c| c.as_ref().to_vec());
    if peer.as_deref() != Some(cert.as_slice()) {
        bail!("payload socket presented the wrong certificate");
    }
    let mut buf = vec![0u8; CHUNK];
    let mut done: i64 = 0;
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        timeout(Duration::from_secs(30), tls.write_all(&buf[..n])).await??;
        done += n as i64;
        update(d, tid, done, None);
    }
    tls.flush().await?;
    let _ = tls.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("photo.jpg"), "photo.jpg");
        assert_eq!(safe_name(".."), "file");
    }

    #[test]
    fn unique() {
        let dir = std::env::temp_dir().join(format!("lanlink-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::fs::write(dir.join("a (1).txt"), "x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        assert_eq!(unique_path(&dir, "b"), dir.join("b"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
