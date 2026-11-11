//! Clipboard access through wl-clipboard (Wayland) or xclip (X11).

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::process::{Command, Stdio};

fn wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

pub fn get() -> Result<String> {
    let out = if wayland() {
        Command::new("wl-paste").args(["--no-newline", "--type", "text"]).output().context("running wl-paste (install wl-clipboard)")?
    } else {
        Command::new("xclip").args(["-o", "-selection", "clipboard"]).output().context("running xclip")?
    };
    if !out.status.success() {
        bail!("couldn't read the clipboard: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Returns false when no clipboard tool is available.
pub fn set(text: &str) -> bool {
    let mut cmd = if wayland() {
        Command::new("wl-copy")
    } else {
        let mut c = Command::new("xclip");
        c.args(["-i", "-selection", "clipboard"]);
        c
    };
    let Ok(mut child) = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
        return false;
    };
    let ok = child.stdin.take().is_some_and(|mut i| i.write_all(text.as_bytes()).is_ok());
    // wl-copy forks to keep serving the selection, so this returns promptly.
    ok && child.wait().is_ok_and(|s| s.success())
}
