//! Browse the phone: ask it to start its SFTP server, then mount it with sshfs.

use crate::daemon::Daemon;
use crate::notify;
use crate::protocol::{self, Packet};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub fn request(d: &Daemon, id: &str) -> Result<()> {
    if d.lock().devices.get(id).is_some_and(|dev| dev.mount.is_some()) {
        return Ok(());
    }
    d.send(id, Packet::new(protocol::SFTP_REQUEST, json!({"startBrowsing": true})))
}

pub fn on_info(d: Arc<Daemon>, id: &str, p: Packet) {
    if let Some(err) = p.str("errorMessage") {
        notify::show(format!("Can't browse {}", d.name_of(id)), err);
        return;
    }
    let Some(ip) = d.peer_ip(id).map(|i| i.to_string()).or_else(|| p.str("ip").map(String::from)) else { return };
    let port = p.i64("port").unwrap_or(1739);
    let user = p.str("user").unwrap_or("kdeconnect").to_string();
    let password = p.str("password").unwrap_or_default().to_string();
    let path = p.str("path").unwrap_or("/").to_string();
    let id = id.to_string();
    tokio::spawn(async move {
        match mount(&d, &id, &ip, port, &user, &password, &path).await {
            Ok(mp) => {
                if let Some(dev) = d.lock().devices.get_mut(&id) {
                    dev.mount = Some(mp.clone());
                }
                let shown = mp.display().to_string();
                notify::ask(format!("{} mounted", d.name_of(&id)), shown, &[("open", "Open")], 10_000, move |a| {
                    if a.as_deref() == Some("open") {
                        let _ = std::process::Command::new("xdg-open").arg(&mp).spawn();
                    }
                });
            }
            Err(e) => notify::show(format!("Mounting {} failed", d.name_of(&id)), format!("{e:#}")),
        }
    });
}

fn mountpoint(d: &Daemon, id: &str) -> PathBuf {
    let name: String = d.name_of(id).chars().map(|c| if c == '/' { '_' } else { c }).collect();
    d.cfg.mount_dir.join(name)
}

fn is_mounted(p: &Path) -> bool {
    let target = p.to_string_lossy().replace(' ', "\\040");
    std::fs::read_to_string("/proc/mounts")
        .map(|m| m.lines().any(|l| l.split(' ').nth(1) == Some(target.as_str())))
        .unwrap_or(false)
}

fn fusermount(p: &Path) {
    for cmd in ["fusermount3", "fusermount"] {
        if std::process::Command::new(cmd)
            .args(["-u", "-z"])
            .arg(p)
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn mount(d: &Daemon, id: &str, ip: &str, port: i64, user: &str, password: &str, path: &str) -> Result<PathBuf> {
    let mp = mountpoint(d, id);
    if is_mounted(&mp) {
        fusermount(&mp);
    }
    std::fs::create_dir_all(&mp).with_context(|| format!("creating {}", mp.display()))?;
    let mut child = tokio::process::Command::new("sshfs")
        .arg(format!("{user}@{ip}:{path}"))
        .arg(&mp)
        .args(["-p", &port.to_string()])
        .args([
            "-o", "password_stdin",
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "HostKeyAlgorithms=+ssh-rsa",
            "-o", "LogLevel=ERROR",
            "-o", "reconnect",
            "-o", "ServerAliveInterval=15",
            "-o", "ServerAliveCountMax=3",
            "-o", "idmap=user",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running sshfs (is it installed?)")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(format!("{password}\n").as_bytes()).await?;
    }
    let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .context("sshfs timed out")??;
    if !out.status.success() {
        let _ = std::fs::remove_dir(&mp);
        bail!("sshfs: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(mp)
}

pub fn unmount(d: &Daemon, id: &str) {
    let mp = d.lock().devices.get_mut(id).and_then(|dev| dev.mount.take());
    let Some(mp) = mp else { return };
    if is_mounted(&mp) {
        fusermount(&mp);
    }
    let _ = std::fs::remove_dir(&mp); // only succeeds if empty
}
