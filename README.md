# lanlink

A small KDE Connect-compatible daemon + TUI. One Rust binary, no KDE or GNOME
libraries. Pairs with the lanlink Android app in `android/` (or the stock KDE
Connect app), and with other machines running lanlink.

What it does:

- **Send/receive files** (and links/text). Received files go to `~/Downloads`.
- **Browse the phone**: mounts its storage with `sshfs` at `$XDG_RUNTIME_DIR/lanlink/<name>`
  (e.g. `/run/user/1000/lanlink/Pixel`); set `mount_dir` to put it elsewhere.
- **Media control**: the phone sees and controls desktop players (MPRIS).
- **Remote input, receiving**: phone or another machine moves the mouse and
  types here, through `/dev/uinput` (any compositor, X11 too).
- **Remote input, sending**: `lanlink remote <device>` grabs this keyboard and
  touchpad and drives the other machine. **Ctrl+Alt+Esc** releases. A
  notification and a coloured screen border show while it's active.

What it doesn't do: SMS, calls, notifications sync, clipboard, screen sharing.

## Install (Arch)

Runtime needs `sshfs` (for phone browsing) and nothing else.

```sh
cargo build --release
install -Dm755 target/release/lanlink ~/.local/bin/lanlink

# remote input: uinput access for the `input` group
sudo install -Dm644 contrib/60-lanlink-uinput.rules /etc/udev/rules.d/60-lanlink-uinput.rules
sudo install -Dm644 contrib/uinput.conf /etc/modules-load.d/uinput.conf
sudo usermod -aG input "$USER"        # log out and back in after this
sudo modprobe uinput && sudo udevadm trigger

# service
install -Dm644 contrib/lanlink.service ~/.config/systemd/user/lanlink.service
systemctl --user daemon-reload
systemctl --user enable --now lanlink
```

Without systemd (dinit, runit...), skip the service and start `lanlink daemon`
from your session instead, e.g. in niri:

```kdl
spawn-at-startup "/home/you/.local/bin/lanlink" "daemon"
```

Firewall, if you run one: allow TCP and UDP 1714-1764 on the LAN.

## Use

```sh
lanlink                      # TUI
lanlink devices
lanlink pair pixel           # any unique prefix of a name or id
lanlink send pixel photo.jpg https://example.com
lanlink mount pixel
lanlink remote tv            # Ctrl+Alt+Esc to stop
lanlink help
```

Pair requests from the phone also pop up as a notification with
Accept/Reject buttons (swaync, mako, dunst...), so the TUI doesn't need to be open.

### niri

```kdl
binds {
    Mod+Shift+T { spawn "lanlink" "remote" "tv"; }
}
```

lanlink waits until you let go of the keybind before grabbing the keyboard,
so Mod doesn't get stuck.

## Config

`~/.config/lanlink/config.toml`, see `contrib/config.toml`. Identity (device id,
certificate, key) lives next to it; paired devices are in `trusted/`.
Deleting a file in `trusted/` unpairs that device.

## Notes

- Protocol version 8, as current KDE Connect. Certificates are ECDSA P-256.
- If broadcast discovery doesn't work on your network, use `lanlink connect <phone-ip>`.
- The daemon releases a stuck grab when it exits: `systemctl --user restart lanlink`
  from another TTY/ssh if you ever need to.
