mod border;
mod capture;
mod clipboard;
mod config;
mod daemon;
mod input;
mod ipc;
mod mpris;
mod notify;
mod protocol;
mod sftp;
mod share;
mod tls;
mod tui;

use anyhow::{Result, bail};
use ipc::Request;

const HELP: &str = "\
lanlink - KDE Connect-compatible file sharing, phone browsing, media and remote input

usage:
  lanlink                          open the TUI
  lanlink daemon                   run the background service
  lanlink devices                  list known and nearby devices
  lanlink pair <device>            send a pair request
  lanlink unpair <device>
  lanlink accept <device>          accept an incoming pair request
  lanlink reject <device>
  lanlink send <device> <file|url>...
  lanlink clip <device>            send this clipboard to <device>'s clipboard
  lanlink mount <device>           browse a phone's storage
  lanlink unmount <device>
  lanlink remote <device>          control <device> with this keyboard/touchpad
                                   (Ctrl+Alt+Esc releases)
  lanlink release                  stop remote control
  lanlink ping <device>
  lanlink connect <ip[:port]>      connect by address when broadcast doesn't work

<device> is a name or id; any unique prefix works.";

fn main() {
    if let Err(e) = real_main() {
        eprintln!("lanlink: {e:#}");
        std::process::exit(1);
    }
}

fn dev(args: &[String]) -> Result<String> {
    match args.get(1) {
        Some(d) => Ok(d.clone()),
        None => bail!("missing <device>; see `lanlink help`"),
    }
}

fn real_main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let req = match cmd {
        "" => return tui::run(),
        "help" | "-h" | "--help" => {
            println!("{HELP}");
            return Ok(());
        }
        "daemon" => {
            let rt = tokio::runtime::Runtime::new()?;
            return rt.block_on(daemon::run());
        }
        "border" => return border::run(),
        "devices" | "list" | "ls" => return print_devices(),
        "pair" => {
            let code = ipc::call(&Request::Pair { device: dev(&args)? })?;
            println!(
                "Pair request sent. Code: {}. Accept it on the other device.",
                code["code"].as_str().unwrap_or("?")
            );
            return Ok(());
        }
        "unpair" => Request::Unpair { device: dev(&args)? },
        "accept" => Request::Accept { device: dev(&args)? },
        "reject" => Request::Reject { device: dev(&args)? },
        "mount" => Request::Mount { device: dev(&args)? },
        "unmount" => Request::Unmount { device: dev(&args)? },
        "remote" => Request::Remote { device: dev(&args)? },
        "release" => Request::Release,
        "ping" => Request::Ping { device: dev(&args)? },
        "connect" => Request::Connect { addr: dev(&args)? },
        "clip" => {
            let text = clipboard::get()?;
            if text.is_empty() {
                bail!("the clipboard is empty");
            }
            Request::Text { device: dev(&args)?, text }
        }
        "send" => {
            let device = dev(&args)?;
            if args.len() < 3 {
                bail!("usage: lanlink send <device> <file|url>...");
            }
            // The daemon has a different working directory, so send absolute paths.
            let items = args[2..]
                .iter()
                .map(|a| match std::fs::canonicalize(a) {
                    Ok(p) => p.to_string_lossy().into_owned(),
                    Err(_) => a.clone(),
                })
                .collect();
            Request::Send { device, items }
        }
        other => bail!("unknown command {other:?}; see `lanlink help`"),
    };
    ipc::call(&req)?;
    Ok(())
}

fn print_devices() -> Result<()> {
    let s = ipc::call(&Request::Status)?;
    let devs = s["devices"].as_array().cloned().unwrap_or_default();
    if devs.is_empty() {
        println!("No devices yet. Open the lanlink app on your phone on the same network.");
        return Ok(());
    }
    println!("{:<24} {:<8} {:<20} ID", "NAME", "TYPE", "STATE");
    for d in devs {
        println!(
            "{:<24} {:<8} {:<20} {}",
            d["name"].as_str().unwrap_or(""),
            d["type"].as_str().unwrap_or(""),
            tui::state_text(&d),
            d["id"].as_str().unwrap_or("")
        );
    }
    Ok(())
}
