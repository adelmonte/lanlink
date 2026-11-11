//! Daemon <-> CLI/TUI: one JSON request line, one JSON response line, over a Unix socket.

use crate::config;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Pair { device: String },
    Unpair { device: String },
    Accept { device: String },
    Reject { device: String },
    Send { device: String, items: Vec<String> },
    Text { device: String, text: String },
    Mount { device: String },
    Unmount { device: String },
    Remote { device: String },
    Release,
    Ping { device: String },
    Connect { addr: String },
}

pub fn call(req: &Request) -> Result<Value> {
    let path = config::socket_path();
    let mut s = UnixStream::connect(&path)
        .with_context(|| format!("can't reach the daemon at {} (is `lanlink daemon` running?)", path.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    s.write_all(&line)?;
    let mut resp = String::new();
    BufReader::new(s).read_line(&mut resp)?;
    let v: Value = serde_json::from_str(&resp).context("bad response from daemon")?;
    if v["ok"].as_bool() == Some(true) {
        Ok(v["data"].clone())
    } else {
        Err(anyhow!("{}", v["error"].as_str().unwrap_or("unknown error")))
    }
}
